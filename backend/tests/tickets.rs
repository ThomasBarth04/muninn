//! Spec 002 end to end: mail in through the Postmark webhook, work in the
//! API, mail out through the send job.

mod common;

use common::{Client, INBOUND_PASSWORD, TestApp};
use muninn::session::{hash_token, new_token};
use serde_json::{Value, json};
use uuid::Uuid;

const DOMAIN: &str = "in.muninn.test";

/// A workspace and a logged-in agent, arranged directly (spec 001's flow has
/// its own tests).
async fn workspace(app: &TestApp, slug: &str, email: &str, role: &str) -> (Uuid, Uuid, Client) {
    let ws: Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (id, name, slug, language, trial_ends_at)
         VALUES (gen_random_uuid(), initcap($1), $1, 'english', now() + interval '14 days') RETURNING id",
    )
    .bind(slug)
    .fetch_one(&app.owner)
    .await
    .unwrap();
    let agent = agent(app, ws, email, role).await;
    let client = login(app, ws, agent).await;
    (ws, agent, client)
}

async fn agent(app: &TestApp, ws: Uuid, email: &str, role: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO agents (workspace_id, email, name, role) VALUES ($1, $2, split_part($2, '@', 1), $3) RETURNING id")
        .bind(ws)
        .bind(email)
        .bind(role)
        .fetch_one(&app.owner)
        .await
        .unwrap()
}

async fn login(app: &TestApp, ws: Uuid, agent: Uuid) -> Client {
    let token = new_token();
    sqlx::query("INSERT INTO sessions (token_hash, workspace_id, agent_id) VALUES ($1, $2, $3)")
        .bind(hash_token(&token))
        .bind(ws)
        .bind(agent)
        .execute(&app.owner)
        .await
        .unwrap();
    let mut client = app.client();
    client.cookie = Some(format!("muninn_session={token}"));
    client
}

/// A Postmark inbound payload from Ola to `to`.
fn mail(to: &str, subject: &str, text: &str, message_id: &str) -> Value {
    json!({
        "OriginalRecipient": to,
        "ToFull": [{ "Email": to, "Name": "", "MailboxHash": "" }],
        "CcFull": [],
        "FromFull": { "Email": "Ola@Kunde.no", "Name": "Ola Nordmann" },
        "Subject": subject,
        "MailboxHash": "",
        "TextBody": text,
        "HtmlBody": "",
        "StrippedTextReply": "",
        "Headers": [{ "Name": "Message-ID", "Value": format!("<{message_id}>") }],
        "Attachments": [],
    })
}

async fn deliver(app: &TestApp, payload: &Value) -> u16 {
    deliver_as(app, payload, Some(INBOUND_PASSWORD)).await
}

async fn deliver_as(app: &TestApp, payload: &Value, password: Option<&str>) -> u16 {
    let mut req = reqwest::Client::new()
        .post(format!("{}/hooks/postmark/inbound", app.url))
        .json(payload);
    if let Some(p) = password {
        req = req.basic_auth("postmark", Some(p));
    }
    req.send().await.unwrap().status().as_u16()
}

async fn count(app: &TestApp, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(&app.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn inbound_is_authenticated_and_routed() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let m = mail(&format!("acme@{DOMAIN}"), "Hi", "Hello", "a1@kunde.no");

    assert_eq!(deliver_as(&app, &m, None).await, 401);
    assert_eq!(deliver_as(&app, &m, Some("wrong")).await, 401);
    assert_eq!(
        deliver(
            &app,
            &mail(&format!("nobody@{DOMAIN}"), "Hi", "Hello", "a2@x")
        )
        .await,
        403
    );
    assert_eq!(
        deliver(&app, &mail("support@elsewhere.com", "Hi", "Hello", "a3@x")).await,
        403
    );

    let empty = c.get("/api/tickets?view=open").await;
    assert_eq!(empty.body["hasTickets"], false);

    // Forwarded from support@acme.com: OriginalRecipient is theirs, To has ours.
    let mut forwarded = m.clone();
    forwarded["OriginalRecipient"] = json!("support@acme.com");
    forwarded["ToFull"] =
        json!([{ "Email": "support@acme.com" }, { "Email": format!("acme@{DOMAIN}") }]);
    assert_eq!(deliver(&app, &forwarded).await, 200);
    // Postmark retrying the same message stores nothing new.
    assert_eq!(deliver(&app, &m).await, 200);
    assert_eq!(count(&app, "SELECT count(*) FROM messages").await, 1);

    // An HTML-only mail still has text; the HTML is kept, never shown.
    let mut html = mail(&format!("acme@{DOMAIN}"), "", "", "a4@kunde.no");
    html["HtmlBody"] = json!("<p>Hei,</p><p>det&nbsp;virker ikke</p><script>x()</script>");
    assert_eq!(deliver(&app, &html).await, 200);

    let list = c.get("/api/tickets?view=unassigned").await;
    assert_eq!(list.status, 200);
    assert_eq!(list.body["hasTickets"], true);
    assert_eq!(
        list.body["counts"],
        json!({ "unassigned": 2, "mine": 0, "open": 2 })
    );
    let newest = &list.body["tickets"][0];
    assert_eq!(newest["subject"], "(no subject)");
    assert_eq!(newest["lastMessage"]["snippet"], "Hei, det virker ikke");
    assert_eq!(newest["status"], "new");
    assert_eq!(
        newest["contact"],
        json!({ "id": newest["contact"]["id"], "email": "ola@kunde.no", "name": "Ola Nordmann" })
    );
    // One contact for both tickets; each ticket enqueued its copilot jobs (spec 003, 004).
    assert_eq!(count(&app, "SELECT count(*) FROM contacts").await, 1);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM jobs WHERE kind IN ('suggest', 'categorize')"
        )
        .await,
        4
    );

    assert_eq!(
        c.get("/api/tickets?view=everything").await.body["error"],
        "invalidView"
    );
    assert_eq!(
        c.get("/api/tickets?view=open&cursor=nope").await.body["error"],
        "invalidCursor"
    );
    assert_eq!(app.client().get("/api/tickets?view=open").await.status, 401);
}

#[tokio::test]
async fn a_conversation_round_trip() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, frank, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    assert_eq!(
        deliver(
            &app,
            &mail(
                &format!("acme@{DOMAIN}"),
                "SSO broken",
                "Nobody can log in",
                "first@kunde.no"
            )
        )
        .await,
        200
    );
    let ticket = c.get("/api/tickets?view=unassigned").await.body["tickets"][0].clone();
    let id = ticket["id"].as_str().unwrap().to_string();

    // Reply: stored, queued, the ticket is Frank's and waits on the contact.
    assert_eq!(
        c.post(
            &format!("/api/tickets/{id}/replies"),
            json!({ "text": "  " })
        )
        .await
        .body["error"],
        "emptyText"
    );
    let reply = c
        .post(
            &format!("/api/tickets/{id}/replies"),
            json!({ "text": "Re-upload the cert" }),
        )
        .await;
    assert_eq!(reply.status, 201);
    assert_eq!(reply.body["kind"], "agent");
    assert_eq!(
        reply.body["delivery"],
        json!({ "status": "queued", "error": null })
    );
    assert_eq!(reply.body["author"]["email"], "frank@acme.com");
    let t = c.get(&format!("/api/tickets/{id}")).await.body;
    assert_eq!(t["status"], "waitingOnContact");
    assert_eq!(t["owner"]["id"], frank.to_string());

    app.run_jobs().await;
    let sent = app.mock.emails().pop().expect("reply sent");
    let token: String = sqlx::query_scalar("SELECT token FROM tickets")
        .fetch_one(&app.owner)
        .await
        .unwrap();
    assert_eq!(
        sent["From"],
        format!("\"Acme Support\" <acme+{token}@{DOMAIN}>")
    );
    assert_eq!(sent["To"], "ola@kunde.no");
    assert_eq!(sent["Subject"], "Re: SSO broken");
    assert_eq!(sent["MessageStream"], "trials");
    assert_eq!(sent["ReplyTo"], Value::Null);
    let headers = sent["Headers"].as_array().unwrap();
    assert!(headers.contains(&json!({ "Name": "In-Reply-To", "Value": "<first@kunde.no>" })));
    let reply_id = headers.iter().find(|h| h["Name"] == "Message-ID").unwrap()["Value"]
        .as_str()
        .unwrap()
        .to_string();
    let detail = c.get(&format!("/api/tickets/{id}")).await.body;
    assert_eq!(detail["messages"][1]["delivery"]["status"], "sent");

    // The customer answers the token address: appended, back on us, quote stripped.
    let mut answer = mail(
        &format!("acme+{token}@{DOMAIN}"),
        "Re: SSO broken",
        "Thanks!\n\n> quoted",
        "second@kunde.no",
    );
    answer["StrippedTextReply"] = json!("Thanks!");
    assert_eq!(deliver(&app, &answer).await, 200);
    let detail = c.get(&format!("/api/tickets/{id}")).await.body;
    assert_eq!(detail["status"], "waitingOnUs");
    assert_eq!(detail["owner"]["id"], frank.to_string());
    assert_eq!(detail["messages"][2]["text"], "Thanks!");
    assert_eq!(detail["lastMessage"]["kind"], "customer");

    // A reply that lost the token (sent to support@acme.com, forwarded back)
    // still threads on References.
    let mut forwarded = mail(
        &format!("acme@{DOMAIN}"),
        "Re: SSO broken",
        "One more thing",
        "third@kunde.no",
    );
    forwarded["Headers"] = json!([
        { "Name": "Message-ID", "Value": "<third@kunde.no>" },
        { "Name": "References", "Value": format!("<first@kunde.no> {reply_id}") },
    ]);
    assert_eq!(deliver(&app, &forwarded).await, 200);
    assert_eq!(count(&app, "SELECT count(*) FROM tickets").await, 1);

    // Internal comment: in the thread, not mailed, status unchanged.
    let emails_before = app.mock.emails().len();
    let note = c
        .post(
            &format!("/api/tickets/{id}/comments"),
            json!({ "text": "IdP cert expired" }),
        )
        .await;
    assert_eq!(note.status, 201);
    assert_eq!(note.body["kind"], "comment");
    assert_eq!(note.body["delivery"], Value::Null);
    app.run_jobs().await;
    assert_eq!(app.mock.emails().len(), emails_before);
    let detail = c.get(&format!("/api/tickets/{id}")).await.body;
    assert_eq!(detail["status"], "waitingOnUs");
    let kinds: Vec<&str> = detail["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["customer", "agent", "customer", "customer", "comment"]
    );

    // The contact's other tickets link from the sidebar.
    deliver(
        &app,
        &mail(
            &format!("acme@{DOMAIN}"),
            "New problem",
            "Other thing",
            "fourth@kunde.no",
        ),
    )
    .await;
    let detail = c.get(&format!("/api/tickets/{id}")).await.body;
    assert_eq!(detail["contactTickets"][0]["subject"], "New problem");
    assert_eq!(
        c.get("/api/tickets?view=mine").await.body["counts"]["mine"],
        1
    );
}

#[tokio::test]
async fn patching_and_the_brain() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let vetle = agent(&app, ws, "vetle@acme.com", "agent").await;
    deliver(
        &app,
        &mail(
            &format!("acme@{DOMAIN}"),
            "SSO certificate",
            "Login fails",
            "p1@kunde.no",
        ),
    )
    .await;
    let id = c.get("/api/tickets?view=open").await.body["tickets"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let path = format!("/api/tickets/{id}");

    assert_eq!(
        c.patch(&path, json!({ "status": "done" })).await.body["error"],
        "invalidStatus"
    );
    assert_eq!(
        c.patch(&path, json!({ "priority": "meh" })).await.body["error"],
        "invalidPriority"
    );
    assert_eq!(
        c.patch(&path, json!({ "ownerId": Uuid::new_v4() }))
            .await
            .body["error"],
        "unknownAgent"
    );
    assert_eq!(
        c.patch(&path, json!({ "categoryId": Uuid::new_v4() }))
            .await
            .body["error"],
        "unknownCategory"
    );
    assert_eq!(
        c.patch(&format!("/api/tickets/{}", Uuid::new_v4()), json!({}))
            .await
            .status,
        404
    );

    let cat: Uuid = sqlx::query_scalar(
        "INSERT INTO categories (workspace_id, name) VALUES ($1, 'Login & access') RETURNING id",
    )
    .bind(ws)
    .fetch_one(&app.owner)
    .await
    .unwrap();
    let t = c
        .patch(
            &path,
            json!({ "ownerId": vetle, "priority": "high", "categoryId": cat }),
        )
        .await;
    assert_eq!(t.status, 200);
    assert_eq!(t.body["owner"]["name"], "vetle");
    assert_eq!(t.body["priority"], "high");
    assert_eq!(
        t.body["category"],
        json!({ "id": cat, "name": "Login & access", "source": "agent", "probability": null })
    );
    let t = c
        .patch(&path, json!({ "priority": null, "categoryId": null }))
        .await;
    assert_eq!(t.body["priority"], Value::Null);
    assert_eq!(t.body["category"], Value::Null);
    assert_eq!(t.body["owner"]["name"], "vetle");

    let search = |app: &TestApp| {
        let owner = app.owner.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>("SELECT search::text FROM tickets")
                .fetch_one(&owner)
                .await
                .unwrap()
        }
    };
    // Closing indexes the ticket, stemmed in the workspace's language.
    assert_eq!(
        c.patch(&path, json!({ "status": "closed" })).await.body["status"],
        "closed"
    );
    let indexed = search(&app).await.expect("indexed on close");
    assert!(
        indexed.contains("'certif'") && indexed.contains("'login'"),
        "{indexed}"
    );
    assert_eq!(
        c.get("/api/tickets?view=closed").await.body["tickets"][0]["id"],
        id
    );
    assert_eq!(
        c.get("/api/tickets?view=open").await.body["counts"]["open"],
        0
    );

    // Reopening by hand takes it out; mail to a closed ticket reopens it too.
    c.patch(&path, json!({ "status": "waitingOnUs" })).await;
    assert_eq!(search(&app).await, None);
    c.patch(&path, json!({ "status": "closed" })).await;
    let token: String = sqlx::query_scalar("SELECT token FROM tickets")
        .fetch_one(&app.owner)
        .await
        .unwrap();
    deliver(
        &app,
        &mail(
            &format!("acme+{token}@{DOMAIN}"),
            "Re",
            "It broke again",
            "p2@kunde.no",
        ),
    )
    .await;
    let t = c.get(&path).await.body;
    assert_eq!(t["status"], "waitingOnUs");
    assert_eq!(search(&app).await, None);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM tickets WHERE closed_at IS NOT NULL"
        )
        .await,
        0
    );

    // Another workspace's agent sees nothing of it.
    let (_, _, mut other) = workspace(&app, "globex", "hank@globex.com", "owner").await;
    assert_eq!(other.get(&path).await.status, 404);
    assert_eq!(
        other
            .patch(&path, json!({ "status": "closed" }))
            .await
            .status,
        404
    );
    assert_eq!(
        other
            .post(&format!("{path}/comments"), json!({ "text": "x" }))
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn trial_cap_holds_the_101st_reply() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, frank, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    deliver(
        &app,
        &mail(&format!("acme@{DOMAIN}"), "Hi", "Hello", "c1@kunde.no"),
    )
    .await;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM tickets")
        .fetch_one(&app.owner)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO messages (workspace_id, ticket_id, kind, agent_id, text, delivery_status, sent_at)
         SELECT $1, $2, 'agent', $3, 'earlier', 'sent', now() - interval '1 hour' FROM generate_series(1, 100)",
    )
    .bind(ws)
    .bind(id)
    .bind(frank)
    .execute(&app.owner)
    .await
    .unwrap();

    let held = c
        .post(
            &format!("/api/tickets/{id}/replies"),
            json!({ "text": "number 101" }),
        )
        .await;
    assert_eq!(held.status, 201);
    assert_eq!(held.body["delivery"]["status"], "held");
    assert!(
        held.body["delivery"]["error"]
            .as_str()
            .unwrap()
            .contains("100 emails a day")
    );
    let mid = held.body["id"].as_str().unwrap().to_string();
    assert_eq!(
        count(&app, "SELECT count(*) FROM jobs WHERE kind = 'send'").await,
        0
    );

    let still = c
        .post(&format!("/api/messages/{mid}/retry"), json!({}))
        .await;
    assert_eq!(still.status, 200);
    assert_eq!(still.body["delivery"]["status"], "held");

    // The window moves on: the cap lifts.
    sqlx::query(
        "UPDATE messages SET sent_at = now() - interval '25 hours' WHERE delivery_status = 'sent'",
    )
    .execute(&app.owner)
    .await
    .unwrap();
    let queued = c
        .post(&format!("/api/messages/{mid}/retry"), json!({}))
        .await;
    assert_eq!(
        queued.body["delivery"],
        json!({ "status": "queued", "error": null })
    );
    assert_eq!(
        c.post(&format!("/api/messages/{mid}/retry"), json!({}))
            .await
            .body["error"],
        "notRetryable"
    );
    app.run_jobs().await;

    // A paying workspace has no cap, and sends on the customer stream.
    sqlx::query("UPDATE messages SET sent_at = now() WHERE delivery_status = 'sent'")
        .execute(&app.owner)
        .await
        .unwrap();
    sqlx::query("UPDATE workspaces SET billing_status = 'active'")
        .execute(&app.owner)
        .await
        .unwrap();
    let paid = c
        .post(
            &format!("/api/tickets/{id}/replies"),
            json!({ "text": "paid" }),
        )
        .await;
    assert_eq!(paid.body["delivery"]["status"], "queued");
    app.run_jobs().await;
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM messages WHERE delivery_status = 'sent'"
        )
        .await,
        102
    );
    let streams: Vec<Value> = app
        .mock
        .emails()
        .iter()
        .map(|e| e["MessageStream"].clone())
        .collect();
    assert_eq!(streams, [json!("trials"), json!("customers")]);
}

#[tokio::test]
async fn postmark_refusals_and_outages() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    deliver(
        &app,
        &mail(&format!("acme@{DOMAIN}"), "Hi", "Hello", "d1@kunde.no"),
    )
    .await;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM tickets")
        .fetch_one(&app.owner)
        .await
        .unwrap();

    app.mock.respond(|_, path, _| {
        (path == "/email").then(|| {
            (
                422,
                json!({ "ErrorCode": 406, "Message": "Inactive recipient" }),
            )
        })
    });
    let mid = c
        .post(
            &format!("/api/tickets/{id}/replies"),
            json!({ "text": "hello" }),
        )
        .await
        .body["id"]
        .clone();
    app.run_jobs().await;
    let m = &c.get(&format!("/api/tickets/{id}")).await.body["messages"][1];
    assert_eq!(
        m["delivery"],
        json!({ "status": "failed", "error": "Inactive recipient" })
    );

    // A temporary failure stays queued and is retried later, not now.
    app.mock
        .respond(|_, path, _| (path == "/email").then(|| (503, json!({ "Message": "down" }))));
    let retried = c
        .post(
            &format!("/api/messages/{}/retry", mid.as_str().unwrap()),
            json!({}),
        )
        .await;
    assert_eq!(retried.body["delivery"]["status"], "queued");
    app.run_jobs().await;
    let m = &c.get(&format!("/api/tickets/{id}")).await.body["messages"][1];
    assert_eq!(m["delivery"]["status"], "queued");
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM jobs WHERE kind = 'send' AND run_at > now() AND attempts = 1"
        )
        .await,
        1
    );

    // Out of attempts: failed, with a reason the agent can read.
    // (The claim counts the fourth attempt; that one is the last.)
    sqlx::query("UPDATE jobs SET attempts = 3, run_at = now() WHERE kind = 'send'")
        .execute(&app.owner)
        .await
        .unwrap();
    app.run_jobs().await;
    let m = &c.get(&format!("/api/tickets/{id}")).await.body["messages"][1];
    assert_eq!(
        m["delivery"],
        json!({ "status": "failed", "error": "Could not reach the mail provider" })
    );
}

#[tokio::test]
async fn attachments_are_always_downloads() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let page = b"<script>alert(document.cookie)</script>";
    let mut m = mail(
        &format!("acme@{DOMAIN}"),
        "See attached",
        "Look",
        "e1@kunde.no",
    );
    m["Attachments"] = json!([
        { "Name": "evil\".html", "Content": base64_encode(page), "ContentType": "text/html", "ContentLength": page.len() },
        { "Name": "broken.bin", "Content": "!!!not base64", "ContentType": "", "ContentLength": 3 },
    ]);
    assert_eq!(deliver(&app, &m).await, 200);
    let id = c.get("/api/tickets?view=open").await.body["tickets"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let detail = c.get(&format!("/api/tickets/{id}")).await.body;
    let atts = detail["messages"][0]["attachments"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        atts.len(),
        1,
        "the undecodable one is skipped, the mail kept"
    );
    assert_eq!(atts[0]["name"], "evil\".html");
    assert_eq!(atts[0]["size"], page.len());

    let aid = atts[0]["id"].as_str().unwrap();
    let res = c.get(&format!("/api/attachments/{aid}")).await;
    assert_eq!(res.status, 200);
    let h = |k: &str| res.headers.get(k).unwrap().to_str().unwrap().to_string();
    assert_eq!(h("content-type"), "text/html");
    assert_eq!(
        h("content-disposition"),
        "attachment; filename=\"evil_.html\"; filename*=UTF-8''evil%22.html"
    );
    assert_eq!(h("x-content-type-options"), "nosniff");
    assert_eq!(h("content-security-policy"), "sandbox");
    assert_eq!(
        res.body,
        Value::String(String::from_utf8(page.to_vec()).unwrap())
    );

    let (_, _, mut other) = workspace(&app, "globex", "hank@globex.com", "owner").await;
    assert_eq!(
        other.get(&format!("/api/attachments/{aid}")).await.status,
        404
    );
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[tokio::test]
async fn views_page_by_cursor() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    for n in 0..55 {
        assert_eq!(
            deliver(
                &app,
                &mail(
                    &format!("acme@{DOMAIN}"),
                    &format!("Ticket {n}"),
                    "x",
                    &format!("{n}@kunde.no")
                )
            )
            .await,
            200
        );
    }
    let first = c.get("/api/tickets?view=open").await.body;
    assert_eq!(first["tickets"].as_array().unwrap().len(), 50);
    assert_eq!(first["tickets"][0]["subject"], "Ticket 54");
    assert_eq!(first["counts"]["open"], 55);
    let cursor = first["nextCursor"].as_str().unwrap();
    let second = c
        .get(&format!("/api/tickets?view=open&cursor={cursor}"))
        .await
        .body;
    let subjects: Vec<&str> = second["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["subject"].as_str().unwrap())
        .collect();
    assert_eq!(
        subjects,
        ["Ticket 4", "Ticket 3", "Ticket 2", "Ticket 1", "Ticket 0"]
    );
    assert_eq!(second["nextCursor"], Value::Null);
}

#[tokio::test]
async fn sending_from_the_workspace_domain() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let vetle = agent(&app, ws, "vetle@acme.com", "agent").await;
    let mut vetle = login(&app, ws, vetle).await;

    let none = c.get("/api/sending-domain").await.body;
    assert_eq!(
        none,
        json!({ "fromAddress": null, "status": null, "dnsRecords": [] })
    );
    assert_eq!(
        c.post("/api/sending-domain/verify", json!({})).await.status,
        404
    );
    assert_eq!(
        vetle
            .put(
                "/api/sending-domain",
                json!({ "fromAddress": "support@acme.com" })
            )
            .await
            .body["error"],
        "ownerOnly"
    );
    assert_eq!(
        c.put("/api/sending-domain", json!({ "fromAddress": "nope" }))
            .await
            .body["error"],
        "invalidAddress"
    );

    let verified = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let v = verified.clone();
    app.mock.respond(move |method, path, _| {
        let ok = v.load(std::sync::atomic::Ordering::SeqCst);
        let domain = json!({
            "ID": 42, "Name": "acme.com",
            "DKIMVerified": ok, "DKIMHost": "", "DKIMTextValue": "",
            "DKIMPendingHost": "2026pm._domainkey.acme.com", "DKIMPendingTextValue": "k=rsa;p=MIGf",
            "ReturnPathDomain": "pm-bounces.acme.com", "ReturnPathDomainVerified": ok,
            "ReturnPathDomainCNAMEValue": "pm.mtasv.net",
        });
        match (method, path) {
            (_, p) if p.starts_with("/domains") && method != "DELETE" => Some((200, domain)),
            _ => None,
        }
    });
    let pending = c
        .put(
            "/api/sending-domain",
            json!({ "fromAddress": "Support@Acme.com" }),
        )
        .await;
    assert_eq!(pending.status, 200);
    assert_eq!(
        pending.body,
        json!({
            "fromAddress": "support@acme.com",
            "status": "pending",
            "dnsRecords": [
                { "type": "TXT", "host": "2026pm._domainkey.acme.com", "value": "k=rsa;p=MIGf" },
                { "type": "CNAME", "host": "pm-bounces.acme.com", "value": "pm.mtasv.net" },
            ],
        })
    );
    let created = app
        .mock
        .calls("/domains")
        .into_iter()
        .find(|c| c.method == "POST")
        .unwrap();
    assert_eq!(
        created.body,
        json!({ "Name": "acme.com", "ReturnPathDomain": "pm-bounces.acme.com" })
    );
    assert_eq!(
        vetle.get("/api/sending-domain").await.body["status"],
        "pending"
    );

    // Still pending: replies keep the default sender.
    deliver(
        &app,
        &mail(&format!("acme@{DOMAIN}"), "Hi", "Hello", "f1@kunde.no"),
    )
    .await;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM tickets")
        .fetch_one(&app.owner)
        .await
        .unwrap();
    c.post(
        &format!("/api/tickets/{id}/replies"),
        json!({ "text": "one" }),
    )
    .await;
    app.run_jobs().await;
    assert!(
        app.mock.emails().last().unwrap()["From"]
            .as_str()
            .unwrap()
            .contains(&format!("@{DOMAIN}"))
    );

    verified.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        c.post("/api/sending-domain/verify", json!({})).await.body["status"],
        "verified"
    );
    c.post(
        &format!("/api/tickets/{id}/replies"),
        json!({ "text": "two" }),
    )
    .await;
    app.run_jobs().await;
    let email = app.mock.emails().pop().unwrap();
    let token: String = sqlx::query_scalar("SELECT token FROM tickets")
        .fetch_one(&app.owner)
        .await
        .unwrap();
    assert_eq!(email["From"], "\"Acme Support\" <support@acme.com>");
    assert_eq!(email["ReplyTo"], format!("acme+{token}@{DOMAIN}"));

    // Another workspace cannot claim the same domain.
    let (_, _, mut globex) = workspace(&app, "globex", "hank@globex.com", "owner").await;
    assert_eq!(
        globex
            .put(
                "/api/sending-domain",
                json!({ "fromAddress": "help@acme.com" })
            )
            .await
            .body["error"],
        "domainTaken"
    );

    // Removing it: back to the default sender at once, Postmark told.
    assert_eq!(c.delete("/api/sending-domain").await.status, 204);
    assert_eq!(c.get("/api/sending-domain").await.body, none);
    assert!(
        app.mock
            .calls("/domains/42")
            .iter()
            .any(|c| c.method == "DELETE")
    );
    // …and the domain is free again.
    assert_eq!(
        globex
            .put(
                "/api/sending-domain",
                json!({ "fromAddress": "help@acme.com" })
            )
            .await
            .status,
        200
    );
}

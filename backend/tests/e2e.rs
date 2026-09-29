//! The product promise end to end, through the public API only: Frank solves
//! a problem on Monday, Vetle gets the same problem on Wednesday and the
//! sidebar shows him Frank's answer.

mod common;

use serde_json::{Value, json};

fn inbound(
    from: &str,
    to: &str,
    subject: &str,
    text: &str,
    message_id: &str,
    mailbox_hash: &str,
) -> Value {
    json!({
        "OriginalRecipient": to,
        "FromFull": { "Email": from, "Name": "Ola Nordmann", "MailboxHash": "" },
        "ToFull": [{ "Email": to, "Name": "", "MailboxHash": mailbox_hash }],
        "CcFull": [],
        "Subject": subject,
        "MailboxHash": mailbox_hash,
        "TextBody": text,
        "HtmlBody": "",
        "StrippedTextReply": "",
        "Headers": [{ "Name": "Message-ID", "Value": format!("<{message_id}>") }],
        "Attachments": [],
    })
}

async fn post_inbound(app: &common::TestApp, mail: Value) -> u16 {
    reqwest::Client::new()
        .post(format!("{}/hooks/postmark/inbound", app.url))
        .basic_auth("postmark", Some(common::INBOUND_PASSWORD))
        .json(&mail)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn a_problem_solved_once_is_solved_for_everyone_after() {
    let Some(app) = common::spawn().await else {
        return;
    };
    // Jev: every past case is the same problem; the category is login.
    app.mock.respond(|_, path, body| {
        if path != "/v1/systemone" {
            return None;
        }
        let mut answers = serde_json::Map::new();
        for (key, q) in body["questions"].as_object().unwrap() {
            let answer = if q["type"] == "choice" {
                json!({ "choice": "Login & access", "probabilities": { "Login & access": 0.87, "Bug": 0.13 } })
            } else {
                json!({ "noul": 0.92 })
            };
            answers.insert(key.clone(), answer);
        }
        Some((200, json!({ "answers": answers })))
    });

    // Frank signs up.
    let mut frank = app.client();
    let r = frank
        .post("/api/signup", json!({ "email": "frank@acme.com", "workspaceName": "Acme", "slug": "acme", "language": "english" }))
        .await;
    assert_eq!(r.status, 202, "{:?}", r.body);
    app.wait_for_email("frank@acme.com").await;
    let token = app.mock.link_token("frank@acme.com");
    let r = frank.get(&format!("/api/auth/link?token={token}")).await;
    assert_eq!(r.body["purpose"], "signup");
    let r = frank
        .post("/api/auth/link", json!({ "token": token }))
        .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["redirect"], "/onboarding");
    let me = frank.get("/api/me").await.body;
    assert_eq!(me["workspace"]["inboundAddress"], "acme@in.muninn.test");

    // Monday: Ola writes in; Frank answers and closes.
    let to = "acme@in.muninn.test";
    let subject = "SSO login fails after certificate rotation";
    assert_eq!(
        post_inbound(
            &app,
            inbound(
                "ola@kunde.no",
                to,
                subject,
                "Since this morning none of us can log in with SSO.",
                "m1@kunde.no",
                ""
            )
        )
        .await,
        200
    );
    app.run_jobs().await;
    let list = frank.get("/api/tickets?view=unassigned").await.body;
    assert_eq!(list["counts"]["unassigned"], 1);
    let first = list["tickets"][0].clone();
    let id = first["id"].as_str().unwrap().to_string();
    assert_eq!(first["category"]["name"], "Login & access");
    assert_eq!(first["category"]["source"], "jev");
    let r = frank
        .post(&format!("/api/tickets/{id}/replies"), json!({ "text": "Your IdP rotated its signing certificate. Re-upload it under Settings → SSO." }))
        .await;
    assert_eq!(r.status, 201, "{:?}", r.body);
    app.run_jobs().await;
    let sent = app
        .mock
        .emails()
        .into_iter()
        .find(|e| e["To"] == "ola@kunde.no")
        .expect("reply sent");
    assert!(sent["From"].as_str().unwrap().contains("acme+"));
    assert_eq!(sent["MessageStream"], "trials");
    let r = frank
        .patch(&format!("/api/tickets/{id}"), json!({ "status": "closed" }))
        .await;
    assert_eq!(r.status, 200, "{:?}", r.body);

    // Frank invites Vetle.
    let r = frank
        .post("/api/invites", json!({ "email": "vetle@acme.com" }))
        .await;
    assert_eq!(r.status, 201, "{:?}", r.body);
    app.wait_for_email("vetle@acme.com").await;
    let mut vetle = app.client();
    let token = app.mock.link_token("vetle@acme.com");
    assert_eq!(
        vetle
            .post("/api/auth/link", json!({ "token": token }))
            .await
            .status,
        200
    );

    // Wednesday: Kari has the same problem.
    let second = inbound(
        "kari@annen.no",
        to,
        "Cannot log in with SSO",
        "SSO login fails for everyone since the certificate rotation.",
        "m2@annen.no",
        "",
    );
    assert_eq!(post_inbound(&app, second).await, 200);
    app.run_jobs().await;
    let list = vetle.get("/api/tickets?view=unassigned").await.body;
    let new_id = list["tickets"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(list["tickets"][0]["seenBefore"], true);
    let s = vetle
        .get(&format!("/api/tickets/{new_id}/suggestions"))
        .await
        .body;
    assert_eq!(s["status"], "ready", "{s:?}");
    assert_eq!(s["brainSize"], 1);
    assert_eq!(s["suggestions"][0]["case"]["ticketId"], id.as_str());
    assert!(
        s["suggestions"][0]["solution"]["text"]
            .as_str()
            .unwrap()
            .contains("Re-upload")
    );
    assert_eq!(s["suggestions"][0]["solution"]["author"]["name"], "frank");
    let sid = s["suggestions"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(
        vetle
            .post(
                &format!("/api/suggestions/{sid}/feedback"),
                json!({ "verdict": "helped" })
            )
            .await
            .status,
        204
    );
    let s = vetle
        .get(&format!("/api/tickets/{new_id}/suggestions"))
        .await
        .body;
    assert_eq!(s["suggestions"][0]["myFeedback"], "helped");

    // Ola replies to Frank's answer: it threads onto the closed ticket and
    // reopens it, which takes it out of the brain.
    let ticket_token = sent["From"]
        .as_str()
        .unwrap()
        .split('+')
        .nth(1)
        .unwrap()
        .split('@')
        .next()
        .unwrap()
        .to_string();
    let reply_to = format!("acme+{ticket_token}@in.muninn.test");
    assert_eq!(
        post_inbound(
            &app,
            inbound(
                "ola@kunde.no",
                &reply_to,
                &format!("Re: {subject}"),
                "Still broken.",
                "m3@kunde.no",
                &ticket_token
            )
        )
        .await,
        200
    );
    let t = frank.get(&format!("/api/tickets/{id}")).await.body;
    assert_eq!(t["status"], "waitingOnUs");
    assert_eq!(t["messages"].as_array().unwrap().len(), 3);
    let s = vetle
        .get(&format!("/api/tickets/{new_id}/suggestions"))
        .await
        .body;
    assert_eq!(s["brainSize"], 0);

    // Another workspace sees none of it.
    let mut globex = app.client();
    globex
        .post("/api/signup", json!({ "email": "boss@globex.com", "workspaceName": "Globex", "slug": "globex", "language": "english" }))
        .await;
    app.wait_for_email("boss@globex.com").await;
    let token = app.mock.link_token("boss@globex.com");
    globex
        .post("/api/auth/link", json!({ "token": token }))
        .await;
    assert_eq!(globex.get(&format!("/api/tickets/{id}")).await.status, 404);
    assert_eq!(
        globex.get("/api/tickets?view=open").await.body["counts"]["open"],
        0
    );
}

//! Spec 006 end to end, one test per Behaviour scenario the backend owns.

mod common;

use chrono::{Duration, Utc};
use common::{Client, DOMAIN, TestApp, agent, count, deliver, login, mail, workspace};
use serde_json::{Value, json};
use uuid::Uuid;

/// A mail from Ola; the new ticket's id.
async fn arrives(app: &TestApp, subject: &str, text: &str) -> String {
    let mid = format!("{}@kunde.no", Uuid::new_v4());
    let to = format!("acme@{DOMAIN}");
    assert_eq!(deliver(app, &mail(&to, subject, text, &mid)).await, 200);
    ticket_of(app, &mid).await
}

async fn ticket_of(app: &TestApp, message_id: &str) -> String {
    let id: Uuid = sqlx::query_scalar("SELECT ticket_id FROM messages WHERE message_id = $1")
        .bind(message_id)
        .fetch_one(&app.owner)
        .await
        .unwrap();
    id.to_string()
}

/// Ola answering on the ticket's own address.
async fn answers(app: &TestApp, ticket: &str, text: &str) {
    let token: String = sqlx::query_scalar("SELECT token FROM tickets WHERE id = $1::uuid")
        .bind(ticket)
        .fetch_one(&app.owner)
        .await
        .unwrap();
    let to = format!("acme+{token}@{DOMAIN}");
    let mid = format!("{}@kunde.no", Uuid::new_v4());
    assert_eq!(deliver(app, &mail(&to, "Re", text, &mid)).await, 200);
}

async fn list(c: &mut Client, query: &str) -> Value {
    let r = c.get(&format!("/api/tickets?{query}")).await;
    assert_eq!(r.status, 200, "{query}: {:?}", r.body);
    r.body
}

fn subjects(list: &Value) -> Vec<String> {
    list["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["subject"].as_str().unwrap().to_string())
        .collect()
}

async fn get_ticket(c: &mut Client, id: &str) -> Value {
    let r = c.get(&format!("/api/tickets/{id}")).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    r.body
}

fn in_hours(h: i64) -> String {
    (Utc::now() + Duration::hours(h)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The owner pool arranging time: the snooze is over and its job is due.
async fn snooze_ends(app: &TestApp, ticket: &str) {
    sqlx::query(
        "UPDATE tickets SET snoozed_until = now() - interval '1 second' WHERE id = $1::uuid AND snoozed_until IS NOT NULL",
    )
    .bind(ticket)
    .execute(&app.owner)
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'wake'")
        .execute(&app.owner)
        .await
        .unwrap();
    app.run_jobs().await;
}

// §1
#[tokio::test]
async fn every_ticket_has_a_number_that_search_finds_with_or_without_the_hash() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let first = arrives(&app, "SSO login fails", "x").await;
    arrives(&app, "Invoice missing", "x").await;
    arrives(&app, "Export broken", "x").await;
    let numbers: Vec<i64> = list(&mut c, "view=all").await["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["number"].as_i64().unwrap())
        .collect();
    assert_eq!(numbers, [3, 2, 1]);

    // Per workspace: another one counts from 1.
    let (_, _, mut globex) = workspace(&app, "globex", "hank@globex.com", "owner").await;
    let to = format!("globex@{DOMAIN}");
    deliver(&app, &mail(&to, "Hello", "x", "g1@kunde.no")).await;
    assert_eq!(
        list(&mut globex, "view=all").await["tickets"][0]["number"],
        1
    );

    for q in ["%232", "2"] {
        assert_eq!(
            subjects(&list(&mut c, &format!("view=all&q={q}")).await),
            ["Invoice missing"]
        );
    }

    // Replies keep their subject (spec 002 §17).
    c.post(
        &format!("/api/tickets/{first}/replies"),
        json!({ "text": "On it" }),
    )
    .await;
    app.run_jobs().await;
    assert_eq!(
        app.mock.emails().pop().unwrap()["Subject"],
        "Re: SSO login fails"
    );
}

// §2
#[tokio::test]
async fn the_left_pane_counts_drafts_and_saved_views_for_whoever_is_looking() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, _, mut frank) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let vetle = agent(&app, ws, "vetle@acme.com", "agent").await;
    let mut vetle = login(&app, ws, vetle).await;
    let a = arrives(&app, "SSO login fails", "x").await;
    arrives(&app, "Invoice missing", "x").await;

    let counts = list(&mut frank, "view=unassigned").await["counts"].clone();
    assert_eq!(
        counts,
        json!({ "unassigned": 2, "mine": 0, "open": 2, "drafts": 0, "views": {} })
    );

    let r = frank
        .put(
            &format!("/api/tickets/{a}/draft"),
            json!({ "mode": "reply", "text": "Hi Ola" }),
        )
        .await;
    assert_eq!(r.status, 204);
    let drafts = list(&mut frank, "view=drafts").await;
    assert_eq!(drafts["counts"]["drafts"], 1);
    assert_eq!(subjects(&drafts), ["SSO login fails"]);
    // A draft is private to whoever wrote it.
    let theirs = list(&mut vetle, "view=drafts").await;
    assert_eq!(theirs["counts"]["drafts"], 0);
    assert_eq!(subjects(&theirs), Vec::<String>::new());

    let view = frank
        .post(
            "/api/views",
            json!({ "name": "Invoices", "shared": false, "filters": {
                "view": "open", "q": "invoice", "status": [], "owner": [], "priority": [],
                "category": [], "created": null, "unread": false, "seenBefore": false, "sort": "recent" } }),
        )
        .await;
    assert_eq!(view.status, 201, "{:?}", view.body);
    let id = view.body["id"].as_str().unwrap();
    let counts = list(&mut frank, "view=open").await["counts"].clone();
    assert_eq!(counts["views"], json!({ id: 1 }));
    assert_eq!(
        list(&mut vetle, "view=open").await["counts"]["views"],
        json!({})
    );
}

// §3
#[tokio::test]
async fn a_snoozed_ticket_shows_only_in_snoozed_drafts_and_all_tickets() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let a = arrives(&app, "SSO login fails", "x").await;
    arrives(&app, "Invoice missing", "x").await;
    c.put(
        &format!("/api/tickets/{a}/draft"),
        json!({ "mode": "comment", "text": "check IdP" }),
    )
    .await;
    let r = c
        .patch(
            &format!("/api/tickets/{a}"),
            json!({ "snoozedUntil": in_hours(3) }),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert!(r.body["snoozedUntil"].is_string());

    for view in ["unassigned", "open"] {
        let l = list(&mut c, &format!("view={view}")).await;
        assert_eq!(subjects(&l), ["Invoice missing"], "{view}");
    }
    let l = list(&mut c, "view=snoozed").await;
    assert_eq!(subjects(&l), ["SSO login fails"]);
    assert_eq!(l["counts"]["unassigned"], 1);
    assert_eq!(l["counts"]["open"], 1);
    assert_eq!(
        subjects(&list(&mut c, "view=drafts").await),
        ["SSO login fails"]
    );
    assert_eq!(
        list(&mut c, "view=all").await["tickets"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Assigned to me hides it too.
    let me: Uuid = sqlx::query_scalar("SELECT id FROM agents")
        .fetch_one(&app.owner)
        .await
        .unwrap();
    c.patch(&format!("/api/tickets/{a}"), json!({ "ownerId": me }))
        .await;
    let mine = list(&mut c, "view=mine").await;
    assert_eq!(subjects(&mine), Vec::<String>::new());
    assert_eq!(mine["counts"]["mine"], 0);
}

// §4
#[tokio::test]
async fn saved_views_are_private_unless_shared_and_only_their_creator_changes_them() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, _, mut frank) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let vetle_id = agent(&app, ws, "vetle@acme.com", "agent").await;
    let mut vetle = login(&app, ws, vetle_id).await;
    let filters = json!({
        "view": "open", "q": null, "status": [], "owner": ["none"], "priority": ["urgent", "high"],
        "category": [], "created": null, "unread": false, "seenBefore": false, "sort": "priority" });

    let bad = frank
        .post(
            "/api/views",
            json!({ "name": "  ", "shared": false, "filters": filters }),
        )
        .await;
    assert_eq!(bad.body["error"], "invalidName");
    let long = "x".repeat(41);
    assert_eq!(
        frank
            .post(
                "/api/views",
                json!({ "name": long, "shared": false, "filters": filters })
            )
            .await
            .body["error"],
        "invalidName"
    );
    let mut wrong = filters.clone();
    wrong["priority"] = json!(["meh"]);
    assert_eq!(
        frank
            .post(
                "/api/views",
                json!({ "name": "Urgent", "shared": false, "filters": wrong })
            )
            .await
            .body["error"],
        "invalidFilter"
    );
    wrong = filters.clone();
    wrong["view"] = json!("inbox");
    assert_eq!(
        frank
            .post(
                "/api/views",
                json!({ "name": "Urgent", "shared": false, "filters": wrong })
            )
            .await
            .body["error"],
        "invalidFilter"
    );

    let created = frank
        .post(
            "/api/views",
            json!({ "name": " Urgent billing ", "shared": false, "filters": filters }),
        )
        .await;
    assert_eq!(created.status, 201);
    assert_eq!(created.body["name"], "Urgent billing");
    assert_eq!(created.body["createdBy"]["name"], "frank");
    assert_eq!(created.body["filters"], filters);
    let id = created.body["id"].as_str().unwrap().to_string();
    let path = format!("/api/views/{id}");

    // Private: to Vetle it does not exist.
    assert_eq!(vetle.get("/api/views").await.body["views"], json!([]));
    assert_eq!(
        vetle.patch(&path, json!({ "name": "Mine" })).await.status,
        404
    );
    assert_eq!(vetle.delete(&path).await.status, 404);

    // Shared: Vetle sees it after his own, but cannot change it.
    let shared = frank.patch(&path, json!({ "shared": true })).await;
    assert_eq!(shared.status, 200);
    assert_eq!(shared.body["shared"], true);
    assert_eq!(shared.body["name"], "Urgent billing");
    let own = vetle
        .post(
            "/api/views",
            json!({ "name": "Zzz mine", "shared": false, "filters": filters }),
        )
        .await;
    assert_eq!(own.status, 201);
    let names: Vec<Value> = vetle.get("/api/views").await.body["views"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].clone())
        .collect();
    assert_eq!(names, [json!("Zzz mine"), json!("Urgent billing")]);
    assert_eq!(
        vetle.patch(&path, json!({ "name": "Mine" })).await.body["error"],
        "notYours"
    );
    assert_eq!(vetle.delete(&path).await.body["error"], "notYours");

    let mut narrower = filters.clone();
    narrower["priority"] = json!(["urgent"]);
    let updated = frank
        .patch(&path, json!({ "filters": narrower, "name": "Urgent" }))
        .await;
    assert_eq!(updated.body["filters"], narrower);
    assert_eq!(updated.body["name"], "Urgent");

    // At most 30 per agent.
    for n in 1..30 {
        let r = frank
            .post(
                "/api/views",
                json!({ "name": format!("View {n}"), "shared": false, "filters": filters }),
            )
            .await;
        assert_eq!(r.status, 201);
    }
    assert_eq!(
        frank
            .post(
                "/api/views",
                json!({ "name": "One too many", "shared": false, "filters": filters })
            )
            .await
            .body["error"],
        "tooManyViews"
    );
    assert_eq!(frank.delete(&path).await.status, 204);
    assert_eq!(frank.delete(&path).await.status, 404);
    assert_eq!(
        frank
            .post(
                "/api/views",
                json!({ "name": "Room again", "shared": false, "filters": filters })
            )
            .await
            .status,
        201
    );

    // Another workspace sees none of it.
    let (_, _, mut globex) = workspace(&app, "globex", "hank@globex.com", "owner").await;
    assert_eq!(globex.get("/api/views").await.body["views"], json!([]));
}

// §6
#[tokio::test]
async fn owner_me_in_a_shared_view_means_whoever_is_looking() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, frank_id, mut frank) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let vetle_id = agent(&app, ws, "vetle@acme.com", "agent").await;
    let mut vetle = login(&app, ws, vetle_id).await;
    let a = arrives(&app, "Frank's", "x").await;
    let b = arrives(&app, "Vetle's", "x").await;
    arrives(&app, "Nobody's", "x").await;
    frank
        .patch(&format!("/api/tickets/{a}"), json!({ "ownerId": frank_id }))
        .await;
    frank
        .patch(&format!("/api/tickets/{b}"), json!({ "ownerId": vetle_id }))
        .await;
    let view = frank
        .post(
            "/api/views",
            json!({ "name": "My tickets", "shared": true, "filters": {
                "view": "open", "q": null, "status": [], "owner": ["me"], "priority": [],
                "category": [], "created": null, "unread": false, "seenBefore": false, "sort": "recent" } }),
        )
        .await;
    let id = view.body["id"].as_str().unwrap().to_string();

    assert_eq!(
        list(&mut frank, "view=open").await["counts"]["views"][&id],
        1
    );
    assert_eq!(
        list(&mut vetle, "view=open").await["counts"]["views"][&id],
        1
    );
    assert_eq!(
        subjects(&list(&mut frank, "view=open&owner=me").await),
        ["Frank's"]
    );
    assert_eq!(
        subjects(&list(&mut vetle, "view=open&owner=me").await),
        ["Vetle's"]
    );
}

// §7
#[tokio::test]
async fn values_in_one_chip_match_any_and_different_chips_must_all_match() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, frank_id, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let billing: Uuid = sqlx::query_scalar(
        "INSERT INTO categories (workspace_id, name) VALUES ($1, 'Billing') RETURNING id",
    )
    .bind(ws)
    .fetch_one(&app.owner)
    .await
    .unwrap();
    let urgent_billing = arrives(&app, "Urgent billing", "x").await;
    let high_billing = arrives(&app, "High billing", "x").await;
    let urgent_none = arrives(&app, "Urgent uncategorised", "x").await;
    let low_mine = arrives(&app, "Low mine", "x").await;
    arrives(&app, "No priority", "x").await;
    for (t, body) in [
        (
            &urgent_billing,
            json!({ "priority": "urgent", "categoryId": billing }),
        ),
        (
            &high_billing,
            json!({ "priority": "high", "categoryId": billing }),
        ),
        (&urgent_none, json!({ "priority": "urgent" })),
        (
            &low_mine,
            json!({ "priority": "low", "ownerId": frank_id, "status": "waitingOnContact" }),
        ),
    ] {
        assert_eq!(
            c.patch(&format!("/api/tickets/{t}"), body).await.status,
            200
        );
    }

    let mut s = subjects(&list(&mut c, "view=all&priority=urgent,high").await);
    s.sort();
    assert_eq!(
        s,
        ["High billing", "Urgent billing", "Urgent uncategorised"]
    );
    assert_eq!(
        subjects(
            &list(
                &mut c,
                &format!("view=all&priority=urgent,high&category={billing}&sort=created")
            )
            .await
        ),
        ["High billing", "Urgent billing"]
    );
    assert_eq!(
        subjects(&list(&mut c, "view=all&priority=urgent&category=none").await),
        ["Urgent uncategorised"]
    );
    assert_eq!(
        subjects(&list(&mut c, "view=all&priority=none").await),
        ["No priority"]
    );
    let mut s = subjects(
        &list(
            &mut c,
            "view=all&owner=me,none&status=waitingOnContact,new&priority=low,none",
        )
        .await,
    );
    s.sort();
    assert_eq!(s, ["Low mine", "No priority"]);
    assert_eq!(
        subjects(&list(&mut c, "view=unassigned&owner=me").await),
        Vec::<String>::new()
    );
}

// §9
#[tokio::test]
async fn created_narrows_to_the_last_day_week_or_month() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    for (subject, age) in [
        ("Today", "1 hour"),
        ("This week", "3 days"),
        ("This month", "20 days"),
        ("Old", "60 days"),
    ] {
        let t = arrives(&app, subject, "x").await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE tickets SET created_at = now() - interval '{age}' WHERE id = $1::uuid"
        )))
        .bind(t)
        .execute(&app.owner)
        .await
        .unwrap();
    }
    let s = |l: Value| subjects(&l).len();
    assert_eq!(s(list(&mut c, "view=all&created=24h").await), 1);
    assert_eq!(s(list(&mut c, "view=all&created=7d").await), 2);
    assert_eq!(s(list(&mut c, "view=all&created=30d").await), 3);
    assert_eq!(s(list(&mut c, "view=all").await), 4);
    assert_eq!(
        c.get("/api/tickets?view=all&created=1y").await.body["error"],
        "invalidFilter"
    );
}

// §10
#[tokio::test]
async fn each_sort_orders_the_whole_list_and_pages_with_its_own_cursor() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    // 60 tickets, two pages, with activity, creation, waiting and priority
    // each in a different order.
    sqlx::query(
        "WITH c AS (INSERT INTO contacts (workspace_id, email) VALUES ($1, 'many@kunde.no') RETURNING id)
         INSERT INTO tickets (workspace_id, token, subject, contact_id, status, priority, created_at, last_activity_at)
         SELECT $1, 'tok' || n, 'T' || n, c.id,
                CASE WHEN n % 3 = 0 THEN 'waitingOnContact' ELSE 'new' END,
                (ARRAY[NULL, 'low', 'medium', 'high', 'urgent'])[1 + n % 5],
                now() - n * interval '1 hour',
                now() - ((n * 7) % 60) * interval '1 minute' - n * interval '1 second'
         FROM c, generate_series(1, 60) n",
    )
    .bind(ws)
    .execute(&app.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages (workspace_id, ticket_id, kind, from_email, text)
         SELECT workspace_id, id, 'customer', 'many@kunde.no', 'x' FROM tickets",
    )
    .execute(&app.owner)
    .await
    .unwrap();

    for (sort, order) in [
        ("recent", "last_activity_at DESC"),
        ("oldest", "last_activity_at"),
        ("created", "created_at DESC"),
        (
            "priority",
            "CASE priority WHEN 'urgent' THEN 4 WHEN 'high' THEN 3 WHEN 'medium' THEN 2 WHEN 'low' THEN 1 ELSE 0 END DESC, last_activity_at DESC",
        ),
        (
            "waiting",
            "waiting_since(t) NULLS LAST, last_activity_at DESC",
        ),
    ] {
        let expected: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT subject FROM tickets t ORDER BY {order}"
        )))
        .fetch_all(&app.owner)
        .await
        .unwrap();
        let first = list(&mut c, &format!("view=all&sort={sort}")).await;
        let cursor = first["nextCursor"]
            .as_str()
            .expect("a second page")
            .to_string();
        let second = list(&mut c, &format!("view=all&sort={sort}&cursor={cursor}")).await;
        assert_eq!(second["nextCursor"], Value::Null);
        let mut got = subjects(&first);
        got.extend(subjects(&second));
        assert_eq!(got, expected, "{sort}");

        // A cursor belongs to its sort.
        let other = if sort == "recent" { "oldest" } else { "recent" };
        assert_eq!(
            c.get(&format!(
                "/api/tickets?view=all&sort={other}&cursor={cursor}"
            ))
            .await
            .body["error"],
            "invalidCursor"
        );
    }
    assert_eq!(
        c.get("/api/tickets?view=all&sort=random").await.body["error"],
        "invalidSort"
    );
}

// §12
#[tokio::test]
async fn a_filter_naming_a_removed_agent_or_category_matches_nothing() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    arrives(&app, "SSO login fails", "x").await;
    let gone = Uuid::new_v4();
    for f in ["owner", "category"] {
        let l = list(&mut c, &format!("view=all&{f}={gone}")).await;
        assert_eq!(l["tickets"], json!([]), "{f}");
        let r = c.get(&format!("/api/tickets?view=all&{f}=vetle")).await;
        assert_eq!(
            (r.status, r.body["error"].clone()),
            (400, json!("invalidFilter"))
        );
    }
    for bad in ["status=done", "unread=yes", "seenBefore=1", "priority=meh"] {
        assert_eq!(
            c.get(&format!("/api/tickets?view=all&{bad}")).await.body["error"],
            "invalidFilter",
            "{bad}"
        );
    }
    let q = "x".repeat(201);
    assert_eq!(
        c.get(&format!("/api/tickets?view=all&q={q}")).await.body["error"],
        "invalidFilter"
    );
    assert_eq!(
        c.get("/api/tickets?view=everything").await.body["error"],
        "invalidView"
    );
    assert_eq!(c.get("/api/tickets").await.body["error"], "invalidView");
}

// §13
#[tokio::test]
async fn search_matches_number_subject_contact_and_every_message_within_the_view() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let sso = arrives(&app, "Cannot log in", "Hi, our SSO broke this morning.").await;
    arrives(&app, "SSO certificate rotation", "Question about certs").await;
    let other = arrives(&app, "Invoice", "Where is my invoice?").await;
    c.post(
        &format!("/api/tickets/{other}/comments"),
        json!({ "text": "Probably 50% off_by_one in billing" }),
    )
    .await;
    let long = format!(
        "{} the SSO is down again {}",
        "a".repeat(80),
        "b".repeat(200)
    );
    answers(&app, &sso, &long).await;

    // Subject, and message text, case-insensitive and inside words.
    let l = list(&mut c, "view=all&q=sso").await;
    let mut s = subjects(&l);
    s.sort();
    assert_eq!(s, ["Cannot log in", "SSO certificate rotation"]);
    let by_id = |id: &str| {
        l["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id)
            .unwrap()
            .clone()
    };
    // The snippet comes from the newest matching message, around the match.
    let m = &by_id(&sso)["searchMatch"];
    assert_eq!(m["kind"], "customer");
    assert_eq!(m["author"], "Ola Nordmann");
    let snippet = m["snippet"].as_str().unwrap();
    assert!(
        snippet.starts_with('…') && snippet.contains("the SSO is down"),
        "{snippet}"
    );
    assert!(snippet.chars().count() <= 141);
    // Only the subject matched: no searchMatch.
    let only_subject = l["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["subject"] == "SSO certificate rotation")
        .unwrap();
    assert_eq!(only_subject["searchMatch"], Value::Null);
    assert_eq!(
        list(&mut c, "view=all").await["tickets"][0]["searchMatch"],
        Value::Null
    );

    // Internal comments count; % and _ are taken literally.
    let l = list(&mut c, "view=all&q=50%25%20off_by").await;
    assert_eq!(subjects(&l), ["Invoice"]);
    assert_eq!(l["tickets"][0]["searchMatch"]["kind"], "comment");
    assert_eq!(l["tickets"][0]["searchMatch"]["author"], "frank");
    assert_eq!(
        subjects(&list(&mut c, "view=all&q=5_%25").await),
        Vec::<String>::new()
    );

    // Contact name and email.
    assert_eq!(
        subjects(&list(&mut c, "view=all&q=NORDMANN").await).len(),
        3
    );
    assert_eq!(
        subjects(&list(&mut c, "view=all&q=kunde.no").await).len(),
        3
    );

    // Within the current view and filters.
    c.patch(
        &format!("/api/tickets/{sso}"),
        json!({ "status": "closed" }),
    )
    .await;
    assert_eq!(
        subjects(&list(&mut c, "view=open&q=sso").await),
        ["SSO certificate rotation"]
    );
    assert_eq!(
        subjects(&list(&mut c, "view=closed&q=sso").await),
        ["Cannot log in"]
    );
}

// §15
#[tokio::test]
async fn a_row_says_who_wrote_the_last_message() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let t = arrives(&app, "SSO login fails", "Hi").await;
    let last = |l: Value| l["tickets"][0]["lastMessage"].clone();
    let m = last(list(&mut c, "view=all").await);
    assert_eq!(
        (m["kind"].clone(), m["author"].clone()),
        (json!("customer"), json!("Ola Nordmann"))
    );
    c.post(
        &format!("/api/tickets/{t}/replies"),
        json!({ "text": "Looking" }),
    )
    .await;
    let m = last(list(&mut c, "view=all").await);
    assert_eq!(
        (m["kind"].clone(), m["author"].clone()),
        (json!("agent"), json!("frank"))
    );
    c.post(
        &format!("/api/tickets/{t}/comments"),
        json!({ "text": "Hmm" }),
    )
    .await;
    let m = last(list(&mut c, "view=all").await);
    assert_eq!(
        (m["kind"].clone(), m["author"].clone()),
        (json!("comment"), json!("frank"))
    );
}

// §16
#[tokio::test]
async fn waiting_counts_from_when_the_customer_started_waiting() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let t = arrives(&app, "SSO login fails", "Hi").await;
    let waiting = |app: &TestApp| {
        let owner = app.owner.clone();
        let t = t.clone();
        async move {
            sqlx::query_scalar::<_, Option<chrono::DateTime<Utc>>>(
                "SELECT waiting_since(t) FROM tickets t WHERE t.id = $1::uuid",
            )
            .bind(t)
            .fetch_one(&owner)
            .await
            .unwrap()
        }
    };
    let since = waiting(&app).await.expect("a new ticket is waiting");
    assert_eq!(
        get_ticket(&mut c, &t).await["waitingSince"]
            .as_str()
            .map(|s| s.parse::<chrono::DateTime<Utc>>().unwrap()),
        Some(since)
    );

    // More customer mail, a comment and New → Waiting on us keep the clock.
    answers(&app, &t, "Still broken").await;
    c.post(
        &format!("/api/tickets/{t}/comments"),
        json!({ "text": "Hmm" }),
    )
    .await;
    c.patch(&format!("/api/tickets/{t}"), json!({ "status": "new" }))
        .await;
    assert_eq!(waiting(&app).await, Some(since));

    // Answered: not waiting. The customer again: waiting from now.
    c.post(
        &format!("/api/tickets/{t}/replies"),
        json!({ "text": "Try this" }),
    )
    .await;
    assert_eq!(waiting(&app).await, None);
    assert_eq!(get_ticket(&mut c, &t).await["waitingSince"], Value::Null);
    answers(&app, &t, "Did not help").await;
    assert!(waiting(&app).await.unwrap() > since);
    c.patch(&format!("/api/tickets/{t}"), json!({ "status": "closed" }))
        .await;
    assert_eq!(waiting(&app).await, None);
}

// §17
#[tokio::test]
async fn a_ticket_is_unread_for_each_agent_until_they_open_it() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, _, mut frank) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let vetle = agent(&app, ws, "vetle@acme.com", "agent").await;
    let mut vetle = login(&app, ws, vetle).await;
    let t = arrives(&app, "SSO login fails", "Hi").await;
    arrives(&app, "Invoice", "Hi").await;
    let unread = |l: &Value| -> Vec<bool> {
        l["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["unread"].as_bool().unwrap())
            .collect()
    };
    assert_eq!(
        unread(&list(&mut frank, "view=open&sort=created").await),
        [true, true]
    );

    // Opening marks it read for Frank only.
    assert_eq!(get_ticket(&mut frank, &t).await["unread"], false);
    assert_eq!(
        unread(&list(&mut frank, "view=open&sort=created").await),
        [true, false]
    );
    assert_eq!(
        unread(&list(&mut vetle, "view=open&sort=created").await),
        [true, true]
    );
    assert_eq!(
        subjects(&list(&mut frank, "view=open&unread=true").await),
        ["Invoice"]
    );

    // A new customer message makes it unread again; agents' own do not.
    frank
        .post(
            &format!("/api/tickets/{t}/comments"),
            json!({ "text": "Hmm" }),
        )
        .await;
    assert_eq!(
        unread(&list(&mut frank, "view=open&sort=created").await),
        [true, false]
    );
    answers(&app, &t, "Still broken").await;
    assert_eq!(
        unread(&list(&mut frank, "view=open&sort=created").await),
        [true, true]
    );

    // Toggled without opening.
    assert_eq!(
        frank
            .put(&format!("/api/tickets/{t}/read"), json!({}))
            .await
            .status,
        204
    );
    assert_eq!(
        unread(&list(&mut frank, "view=open&sort=created").await),
        [true, false]
    );
    assert_eq!(
        frank.delete(&format!("/api/tickets/{t}/read")).await.status,
        204
    );
    assert_eq!(
        unread(&list(&mut frank, "view=open&sort=created").await),
        [true, true]
    );

    // A closed ticket is never unread.
    frank
        .patch(&format!("/api/tickets/{t}"), json!({ "status": "closed" }))
        .await;
    assert_eq!(unread(&list(&mut frank, "view=closed").await), [false]);

    let nope = Uuid::new_v4();
    assert_eq!(
        frank
            .put(&format!("/api/tickets/{nope}/read"), json!({}))
            .await
            .status,
        404
    );
    assert_eq!(
        frank
            .delete(&format!("/api/tickets/{nope}/read"))
            .await
            .status,
        404
    );
    let (_, _, mut globex) = workspace(&app, "globex", "hank@globex.com", "owner").await;
    assert_eq!(
        globex
            .put(&format!("/api/tickets/{t}/read"), json!({}))
            .await
            .status,
        404
    );
}

// §23
#[tokio::test]
async fn bulk_changes_are_all_or_nothing() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let vetle = agent(&app, ws, "vetle@acme.com", "agent").await;
    let a = arrives(&app, "A", "x").await;
    let b = arrives(&app, "B", "x").await;
    let closed = arrives(&app, "Closed", "x").await;
    c.patch(
        &format!("/api/tickets/{closed}"),
        json!({ "status": "closed" }),
    )
    .await;

    let r = c
        .patch(
            "/api/tickets",
            json!({ "tickets": [
                { "id": b, "ownerId": vetle, "priority": "high" },
                { "id": a, "status": "waitingOnContact" },
            ] }),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let ts = r.body["tickets"].as_array().unwrap();
    assert_eq!(
        (ts[0]["id"].clone(), ts[1]["id"].clone()),
        (json!(b), json!(a))
    );
    assert_eq!(ts[0]["owner"]["name"], "vetle");
    assert_eq!(ts[0]["priority"], "high");
    assert_eq!(ts[1]["status"], "waitingOnContact");

    // One ticket that cannot change stops them all.
    let snooze = in_hours(3);
    let r = c
        .patch(
            "/api/tickets",
            json!({ "tickets": [
                { "id": a, "snoozedUntil": snooze, "priority": "urgent" },
                { "id": closed, "snoozedUntil": snooze },
            ] }),
        )
        .await;
    assert_eq!(
        (r.status, r.body["error"].clone()),
        (409, json!("ticketClosed"))
    );
    let t = get_ticket(&mut c, &a).await;
    assert_eq!(
        (t["priority"].clone(), t["snoozedUntil"].clone()),
        (Value::Null, Value::Null)
    );
    assert_eq!(
        count(&app, "SELECT count(*) FROM jobs WHERE kind = 'wake'").await,
        0
    );

    let r = c
        .patch(
            "/api/tickets",
            json!({ "tickets": [{ "id": a, "priority": "low" }, { "id": Uuid::new_v4(), "priority": "low" }] }),
        )
        .await;
    assert_eq!(r.status, 404);
    assert_eq!(get_ticket(&mut c, &a).await["priority"], Value::Null);
    let r = c
        .patch(
            "/api/tickets",
            json!({ "tickets": [{ "id": a, "priority": "low" }, { "id": b, "ownerId": Uuid::new_v4() }] }),
        )
        .await;
    assert_eq!(r.body["error"], "unknownAgent");
    assert_eq!(get_ticket(&mut c, &a).await["priority"], Value::Null);

    let many: Vec<Value> = (0..201).map(|_| json!({ "id": Uuid::new_v4() })).collect();
    for bad in [
        json!({ "tickets": [] }),
        json!({ "tickets": [{ "id": a }, { "id": a, "priority": "low" }] }),
        json!({ "tickets": many }),
    ] {
        assert_eq!(
            c.patch("/api/tickets", bad).await.body["error"],
            "invalidBatch"
        );
    }

    // Another workspace's tickets are not tickets here.
    let (_, _, mut globex) = workspace(&app, "globex", "hank@globex.com", "owner").await;
    assert_eq!(
        globex
            .patch(
                "/api/tickets",
                json!({ "tickets": [{ "id": a, "status": "closed" }] })
            )
            .await
            .status,
        404
    );
}

// §24
#[tokio::test]
async fn undo_puts_back_each_tickets_previous_values() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, frank, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let vetle = agent(&app, ws, "vetle@acme.com", "agent").await;
    let a = arrives(&app, "A", "x").await;
    let b = arrives(&app, "B", "x").await;
    c.patch(
        &format!("/api/tickets/{a}"),
        json!({ "ownerId": frank, "priority": "high" }),
    )
    .await;
    // "Assigned 2 tickets to Vetle", then Undo.
    let done = c
        .patch(
            "/api/tickets",
            json!({ "tickets": [{ "id": a, "ownerId": vetle }, { "id": b, "ownerId": vetle }] }),
        )
        .await;
    assert_eq!(done.status, 200);
    let undo = c
        .patch(
            "/api/tickets",
            json!({ "tickets": [{ "id": a, "ownerId": frank }, { "id": b, "ownerId": null }] }),
        )
        .await;
    assert_eq!(undo.status, 200);
    assert_eq!(undo.body["tickets"][0]["owner"]["id"], json!(frank));
    assert_eq!(undo.body["tickets"][1]["owner"], Value::Null);

    // "Closed #1", then Undo: back to its status, out of the brain.
    c.patch(&format!("/api/tickets/{a}"), json!({ "status": "closed" }))
        .await;
    let undo = c
        .patch(
            "/api/tickets",
            json!({ "tickets": [{ "id": a, "status": "new", "snoozedUntil": null }] }),
        )
        .await;
    assert_eq!(undo.body["tickets"][0]["status"], "new");
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM tickets WHERE search IS NOT NULL"
        )
        .await,
        0
    );
}

// §25
#[tokio::test]
async fn a_snoozed_ticket_wakes_at_the_top_unread_for_everyone() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, _, mut frank) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let vetle = agent(&app, ws, "vetle@acme.com", "agent").await;
    let mut vetle = login(&app, ws, vetle).await;
    let t = arrives(&app, "Snoozed", "x").await;
    arrives(&app, "Newer", "x").await;
    get_ticket(&mut frank, &t).await;
    get_ticket(&mut vetle, &t).await;
    let until = in_hours(20);
    let r = frank
        .patch(
            &format!("/api/tickets/{t}"),
            json!({ "snoozedUntil": until }),
        )
        .await;
    assert_eq!(
        r.body["snoozedUntil"]
            .as_str()
            .unwrap()
            .parse::<chrono::DateTime<Utc>>()
            .unwrap(),
        until.parse::<chrono::DateTime<Utc>>().unwrap()
    );
    // The wake job is due at the snooze's end, not before.
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM jobs j JOIN tickets t ON t.id = j.subject_id
             WHERE j.kind = 'wake' AND j.run_at = t.snoozed_until"
        )
        .await,
        1
    );
    app.run_jobs().await;
    assert_eq!(subjects(&list(&mut frank, "view=open").await), ["Newer"]);

    snooze_ends(&app, &t).await;
    for c in [&mut frank, &mut vetle] {
        let l = list(c, "view=open").await;
        assert_eq!(subjects(&l), ["Snoozed", "Newer"]);
        let woke = &l["tickets"][0];
        assert_eq!(woke["snoozedUntil"], Value::Null);
        assert_eq!(woke["unread"], true);
        assert_eq!(woke["snoozeEnded"], true);
    }
    // Opened: no longer "Snooze ended", for that agent.
    let opened = get_ticket(&mut frank, &t).await;
    assert_eq!(
        (opened["unread"].clone(), opened["snoozeEnded"].clone()),
        (json!(false), json!(false))
    );
    assert_eq!(
        list(&mut vetle, "view=open").await["tickets"][0]["snoozeEnded"],
        true
    );
}

// §26
#[tokio::test]
async fn a_customer_reply_wakes_a_snoozed_ticket_but_the_agents_own_do_not() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let t = arrives(&app, "SSO", "x").await;
    let path = format!("/api/tickets/{t}");
    let snoozed = |t: &Value| !t["snoozedUntil"].is_null();

    // Snooze, then a note and a reply: still snoozed.
    c.patch(&path, json!({ "snoozedUntil": in_hours(3) })).await;
    c.post(&format!("{path}/comments"), json!({ "text": "note" }))
        .await;
    c.post(&format!("{path}/replies"), json!({ "text": "reply" }))
        .await;
    let now = get_ticket(&mut c, &t).await;
    assert!(snoozed(&now));
    assert_eq!(now["status"], "waitingOnContact");

    // The customer answers: awake, back on us.
    answers(&app, &t, "Thanks").await;
    let now = get_ticket(&mut c, &t).await;
    assert!(!snoozed(&now));
    assert_eq!(now["status"], "waitingOnUs");
    assert_eq!(now["snoozeEnded"], false);

    // Closing ends it; so does Unsnooze. A stale wake job does nothing.
    c.patch(&path, json!({ "snoozedUntil": in_hours(3) })).await;
    let closed = c.patch(&path, json!({ "status": "closed" })).await.body;
    assert!(!snoozed(&closed));
    c.patch(&path, json!({ "status": "waitingOnUs" })).await;
    c.patch(&path, json!({ "snoozedUntil": in_hours(3) })).await;
    assert!(!snoozed(
        &c.patch(&path, json!({ "snoozedUntil": null })).await.body
    ));
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'wake'")
        .execute(&app.owner)
        .await
        .unwrap();
    app.run_jobs().await;
    let t = get_ticket(&mut c, &t).await;
    assert_eq!(
        (t["snoozeEnded"].clone(), t["status"].clone()),
        (json!(false), json!("waitingOnUs"))
    );
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM tickets WHERE woke_at IS NOT NULL"
        )
        .await,
        0
    );
    assert_eq!(count(&app, "SELECT count(*) FROM jobs").await, 0);
}

// §27
#[tokio::test]
async fn a_closed_ticket_cannot_be_snoozed_and_the_time_must_be_within_a_year() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let t = arrives(&app, "SSO", "x").await;
    let path = format!("/api/tickets/{t}");
    for when in [in_hours(-1), in_hours(24 * 366)] {
        assert_eq!(
            c.patch(&path, json!({ "snoozedUntil": when })).await.body["error"],
            "invalidSnooze"
        );
    }
    assert_eq!(
        c.patch(&path, json!({ "snoozedUntil": "tomorrow" }))
            .await
            .body["error"],
        "invalidJson"
    );
    assert_eq!(
        c.patch(
            &path,
            json!({ "status": "closed", "snoozedUntil": in_hours(3) })
        )
        .await
        .body["error"],
        "ticketClosed"
    );
    c.patch(&path, json!({ "status": "closed" })).await;
    let r = c.patch(&path, json!({ "snoozedUntil": in_hours(3) })).await;
    assert_eq!(
        (r.status, r.body["error"].clone()),
        (409, json!("ticketClosed"))
    );
    // Reopening and snoozing in one go is fine.
    let r = c
        .patch(
            &path,
            json!({ "status": "waitingOnUs", "snoozedUntil": in_hours(3) }),
        )
        .await;
    assert_eq!(r.status, 200);
    assert!(r.body["snoozedUntil"].is_string());
}

// §29
#[tokio::test]
async fn agents_see_who_else_is_viewing_or_replying() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, _, mut frank) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let vetle_id = agent(&app, ws, "vetle@acme.com", "agent").await;
    let mut vetle = login(&app, ws, vetle_id).await;
    let t = arrives(&app, "SSO", "x").await;

    assert_eq!(get_ticket(&mut frank, &t).await["viewers"], json!([]));
    get_ticket(&mut vetle, &t).await;
    let viewers = |l: Value| l["tickets"][0]["viewers"].clone();
    assert_eq!(
        viewers(list(&mut frank, "view=open").await),
        json!([{ "id": vetle_id, "name": "vetle", "replying": false }])
    );
    // Nobody sees themselves.
    assert_eq!(
        viewers(list(&mut vetle, "view=open").await)
            .as_array()
            .unwrap()
            .len(),
        1
    );

    vetle
        .put(
            &format!("/api/tickets/{t}/draft"),
            json!({ "mode": "reply", "text": "Hi" }),
        )
        .await;
    assert_eq!(
        get_ticket(&mut frank, &t).await["viewers"],
        json!([{ "id": vetle_id, "name": "vetle", "replying": true }])
    );

    // The draft goes quiet after a minute; the viewer after 30 seconds.
    sqlx::query("UPDATE drafts SET updated_at = now() - interval '61 seconds'")
        .execute(&app.owner)
        .await
        .unwrap();
    assert_eq!(
        viewers(list(&mut frank, "view=open").await)[0]["replying"],
        false
    );
    sqlx::query(
        "UPDATE ticket_reads SET viewed_at = now() - interval '31 seconds' WHERE agent_id = $1",
    )
    .bind(vetle_id)
    .execute(&app.owner)
    .await
    .unwrap();
    assert_eq!(viewers(list(&mut frank, "view=open").await), json!([]));
    // Marking read from the list is not viewing it.
    vetle
        .put(&format!("/api/tickets/{t}/read"), json!({}))
        .await;
    assert_eq!(viewers(list(&mut frank, "view=open").await), json!([]));
}

// §32
#[tokio::test]
async fn a_draft_is_kept_per_agent_until_it_is_sent() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, _, mut frank) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let vetle = agent(&app, ws, "vetle@acme.com", "agent").await;
    let mut vetle = login(&app, ws, vetle).await;
    let t = arrives(&app, "SSO", "x").await;
    let draft = format!("/api/tickets/{t}/draft");

    assert_eq!(get_ticket(&mut frank, &t).await["draft"], Value::Null);
    assert_eq!(
        frank
            .put(&draft, json!({ "mode": "email", "text": "x" }))
            .await
            .body["error"],
        "invalidMode"
    );
    assert_eq!(
        frank
            .put(
                &format!("/api/tickets/{}/draft", Uuid::new_v4()),
                json!({ "mode": "reply", "text": "x" })
            )
            .await
            .status,
        404
    );
    frank
        .put(&draft, json!({ "mode": "reply", "text": "Hi Ola," }))
        .await;
    frank
        .put(
            &draft,
            json!({ "mode": "comment", "text": "Hi Ola, re-upload" }),
        )
        .await;
    let d = get_ticket(&mut frank, &t).await["draft"].clone();
    assert_eq!(
        (d["mode"].clone(), d["text"].clone()),
        (json!("comment"), json!("Hi Ola, re-upload"))
    );
    assert!(d["updatedAt"].is_string());
    assert_eq!(get_ticket(&mut vetle, &t).await["draft"], Value::Null);

    // Emptied: gone.
    frank
        .put(&draft, json!({ "mode": "reply", "text": "  \n" }))
        .await;
    assert_eq!(get_ticket(&mut frank, &t).await["draft"], Value::Null);

    // Sending clears the sender's draft, not anyone else's.
    frank
        .put(&draft, json!({ "mode": "reply", "text": "Hi" }))
        .await;
    vetle
        .put(&draft, json!({ "mode": "reply", "text": "Mine" }))
        .await;
    frank
        .post(
            &format!("/api/tickets/{t}/replies"),
            json!({ "text": "Hi" }),
        )
        .await;
    assert_eq!(get_ticket(&mut frank, &t).await["draft"], Value::Null);
    assert_eq!(get_ticket(&mut vetle, &t).await["draft"]["text"], "Mine");
    vetle
        .post(
            &format!("/api/tickets/{t}/comments"),
            json!({ "text": "Mine" }),
        )
        .await;
    assert_eq!(get_ticket(&mut vetle, &t).await["draft"], Value::Null);
}

// §33
#[tokio::test]
async fn send_and_close_sends_the_reply_and_closes_the_ticket() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, frank, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let t = arrives(&app, "SSO", "x").await;
    let path = format!("/api/tickets/{t}/replies");
    assert_eq!(
        c.post(&path, json!({ "text": "x", "status": "waitingOnUs" }))
            .await
            .body["error"],
        "invalidStatus"
    );
    let r = c
        .post(
            &path,
            json!({ "text": "Re-upload the certificate", "status": "closed" }),
        )
        .await;
    assert_eq!(r.status, 201, "{:?}", r.body);
    let ticket = get_ticket(&mut c, &t).await;
    assert_eq!(ticket["status"], "closed");
    assert_eq!(ticket["owner"]["id"], json!(frank));
    // In the brain with the reply in it (ADR 0010).
    let search: String = sqlx::query_scalar("SELECT search::text FROM tickets")
        .fetch_one(&app.owner)
        .await
        .unwrap();
    assert!(search.contains("'certif'"), "{search}");
    app.run_jobs().await;
    assert_eq!(
        app.mock.emails().pop().unwrap()["TextBody"],
        "Re-upload the certificate"
    );
}

// §34
#[tokio::test]
async fn a_reply_waits_ten_seconds_and_can_be_taken_back() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let t = arrives(&app, "SSO", "x").await;
    let reply = c
        .post(
            &format!("/api/tickets/{t}/replies"),
            json!({ "text": "Oops" }),
        )
        .await;
    assert_eq!(reply.body["delivery"]["status"], "queued");
    let mid = reply.body["id"].as_str().unwrap().to_string();
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM jobs WHERE kind = 'send'
               AND run_at BETWEEN now() + interval '9 seconds' AND now() + interval '11 seconds'"
        )
        .await,
        1
    );
    // Not yet due: nothing goes out.
    muninn::jobs::run_due(&app.st).await;
    assert!(app.mock.emails().is_empty());

    // Undo: the reply and its job are gone.
    assert_eq!(c.delete(&format!("/api/messages/{mid}")).await.status, 204);
    assert_eq!(count(&app, "SELECT count(*) FROM jobs").await, 0);
    assert_eq!(
        get_ticket(&mut c, &t).await["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    app.run_jobs().await;
    assert!(app.mock.emails().is_empty());

    // Too late: already on its way, or sent.
    let reply = c
        .post(
            &format!("/api/tickets/{t}/replies"),
            json!({ "text": "Real" }),
        )
        .await;
    let mid = reply.body["id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE jobs SET attempts = 1")
        .execute(&app.owner)
        .await
        .unwrap();
    assert_eq!(
        c.delete(&format!("/api/messages/{mid}")).await.body["error"],
        "notDeletable"
    );
    sqlx::query("UPDATE jobs SET attempts = 0, run_at = now()")
        .execute(&app.owner)
        .await
        .unwrap();
    app.run_jobs().await;
    assert_eq!(app.mock.emails().len(), 1);
    let r = c.delete(&format!("/api/messages/{mid}")).await;
    assert_eq!(
        (r.status, r.body["error"].clone()),
        (409, json!("notDeletable"))
    );

    // Not an agent reply.
    let note = c
        .post(
            &format!("/api/tickets/{t}/comments"),
            json!({ "text": "n" }),
        )
        .await;
    assert_eq!(
        c.delete(&format!(
            "/api/messages/{}",
            note.body["id"].as_str().unwrap()
        ))
        .await
        .body["error"],
        "notDeletable"
    );
    assert_eq!(
        c.delete(&format!("/api/messages/{}", Uuid::new_v4()))
            .await
            .status,
        404
    );
    let (_, _, mut globex) = workspace(&app, "globex", "hank@globex.com", "owner").await;
    assert_eq!(
        globex.delete(&format!("/api/messages/{mid}")).await.status,
        404
    );
}

// §35
#[tokio::test]
async fn a_failed_or_held_reply_can_be_discarded() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, frank, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let t = arrives(&app, "SSO", "x").await;
    app.mock.respond(|_, path, _| {
        (path == "/email").then(|| {
            (
                422,
                json!({ "ErrorCode": 406, "Message": "Inactive recipient" }),
            )
        })
    });
    let failed = c
        .post(
            &format!("/api/tickets/{t}/replies"),
            json!({ "text": "bounced" }),
        )
        .await;
    app.run_jobs().await;
    let fid = failed.body["id"].as_str().unwrap();
    assert_eq!(
        get_ticket(&mut c, &t).await["messages"][1]["delivery"]["status"],
        "failed"
    );
    assert_eq!(c.delete(&format!("/api/messages/{fid}")).await.status, 204);

    // Held at the trial cap.
    sqlx::query(
        "INSERT INTO messages (workspace_id, ticket_id, kind, agent_id, text, delivery_status, sent_at)
         SELECT $1, $2::uuid, 'agent', $3, 'earlier', 'sent', now() - interval '1 hour' FROM generate_series(1, 100)",
    )
    .bind(ws)
    .bind(&t)
    .bind(frank)
    .execute(&app.owner)
    .await
    .unwrap();
    let held = c
        .post(
            &format!("/api/tickets/{t}/replies"),
            json!({ "text": "held" }),
        )
        .await;
    assert_eq!(held.body["delivery"]["status"], "held");
    let hid = held.body["id"].as_str().unwrap();
    assert_eq!(c.delete(&format!("/api/messages/{hid}")).await.status, 204);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM messages WHERE text IN ('bounced', 'held')"
        )
        .await,
        0
    );
}

// §36
#[tokio::test]
async fn send_stops_when_someone_else_wrote_since_the_thread_loaded() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, _, mut frank) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let vetle = agent(&app, ws, "vetle@acme.com", "agent").await;
    let mut vetle = login(&app, ws, vetle).await;
    let t = arrives(&app, "SSO", "x").await;
    let other = arrives(&app, "Other", "x").await;
    let replies = format!("/api/tickets/{t}/replies");
    let newest = |d: &Value| d["messages"].as_array().unwrap().last().unwrap()["id"].clone();
    let loaded = newest(&get_ticket(&mut frank, &t).await);

    // The customer wrote again: stopped, nothing stored.
    answers(&app, &t, "More info").await;
    let r = frank
        .post(&replies, json!({ "text": "Answer", "after": loaded }))
        .await;
    assert_eq!(
        (r.status, r.body["error"].clone()),
        (409, json!("newActivity"))
    );
    let d = get_ticket(&mut frank, &t).await;
    assert_eq!(d["messages"].as_array().unwrap().len(), 2);
    assert_eq!(d["status"], "waitingOnUs");
    assert_eq!(d["owner"], Value::Null);
    assert_eq!(
        count(&app, "SELECT count(*) FROM jobs WHERE kind = 'send'").await,
        0
    );

    // Up to date, and the agent's own comment in between: sent.
    let loaded = newest(&d);
    frank
        .post(
            &format!("/api/tickets/{t}/comments"),
            json!({ "text": "mine" }),
        )
        .await;
    let r = frank
        .post(&replies, json!({ "text": "Answer", "after": loaded }))
        .await;
    assert_eq!(r.status, 201);

    // A colleague's comment stops it; Send anyway omits `after`.
    let loaded = r.body["id"].clone();
    vetle
        .post(
            &format!("/api/tickets/{t}/comments"),
            json!({ "text": "wait" }),
        )
        .await;
    assert_eq!(
        frank
            .post(&replies, json!({ "text": "More", "after": loaded }))
            .await
            .body["error"],
        "newActivity"
    );
    assert_eq!(
        frank.post(&replies, json!({ "text": "More" })).await.status,
        201
    );

    // `after` must be a message on this ticket.
    let elsewhere = newest(&get_ticket(&mut frank, &other).await);
    assert_eq!(
        frank
            .post(&replies, json!({ "text": "x", "after": elsewhere }))
            .await
            .body["error"],
        "invalidAfter"
    );
}

// §38
#[tokio::test]
async fn any_agent_manages_the_workspaces_snippets() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, _, _) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let vetle = agent(&app, ws, "vetle@acme.com", "agent").await;
    let mut c = login(&app, ws, vetle).await;

    for (body, error) in [
        (json!({ "name": " ", "text": "x" }), "invalidName"),
        (
            json!({ "name": "x".repeat(61), "text": "x" }),
            "invalidName",
        ),
        (json!({ "name": "SSO", "text": "  " }), "invalidText"),
        (
            json!({ "name": "SSO", "text": "x".repeat(5001) }),
            "invalidText",
        ),
    ] {
        assert_eq!(c.post("/api/snippets", body).await.body["error"], error);
    }
    let text = "Hi {{contact.firstName}},\n\nRe-upload the certificate.\n\n{{agent.name}}";
    let made = c
        .post(
            "/api/snippets",
            json!({ "name": "SSO cert rotation", "text": text }),
        )
        .await;
    assert_eq!(made.status, 201);
    assert_eq!(made.body["text"], text);
    c.post(
        "/api/snippets",
        json!({ "name": "billing address", "text": "x".repeat(5000) }),
    )
    .await;
    let names: Vec<Value> = c.get("/api/snippets").await.body["snippets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].clone())
        .collect();
    assert_eq!(
        names,
        [json!("billing address"), json!("SSO cert rotation")]
    );

    let path = format!("/api/snippets/{}", made.body["id"].as_str().unwrap());
    let edited = c.patch(&path, json!({ "name": "SSO certificate" })).await;
    assert_eq!(
        (edited.body["name"].clone(), edited.body["text"].clone()),
        (json!("SSO certificate"), json!(text))
    );
    assert_eq!(
        c.patch(&path, json!({ "text": "" })).await.body["error"],
        "invalidText"
    );

    let (_, _, mut globex) = workspace(&app, "globex", "hank@globex.com", "owner").await;
    assert_eq!(
        globex.get("/api/snippets").await.body["snippets"],
        json!([])
    );
    assert_eq!(
        globex.patch(&path, json!({ "name": "x" })).await.status,
        404
    );
    assert_eq!(globex.delete(&path).await.status, 404);

    assert_eq!(c.delete(&path).await.status, 204);
    assert_eq!(c.delete(&path).await.status, 404);
}

// §1, §16: tickets from before this feature, numbered and on the clock.
#[tokio::test]
async fn existing_tickets_are_numbered_by_creation_time() {
    let Some((owner, _)) = common::database().await else {
        return;
    };
    sqlx::migrate!().run_to(4, &owner).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO workspaces (id, name, slug, language, trial_ends_at) VALUES
           ('00000000-0000-0000-0000-00000000000a', 'A', 'a', 'english', now()),
           ('00000000-0000-0000-0000-00000000000b', 'B', 'b', 'english', now());
         INSERT INTO agents (id, workspace_id, email, name, role) VALUES
           ('00000000-0000-0000-0000-0000000000a1', '00000000-0000-0000-0000-00000000000a', 'f@a.no', 'F', 'owner');
         INSERT INTO contacts (id, workspace_id, email) VALUES
           ('00000000-0000-0000-0000-0000000000c1', '00000000-0000-0000-0000-00000000000a', 'o@k.no'),
           ('00000000-0000-0000-0000-0000000000c2', '00000000-0000-0000-0000-00000000000b', 'o@k.no');
         INSERT INTO tickets (id, workspace_id, token, subject, contact_id, status, created_at) VALUES
           ('00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-00000000000a', 't1', 'third', '00000000-0000-0000-0000-0000000000c1', 'new', now() - interval '1 hour'),
           ('00000000-0000-0000-0000-000000000002', '00000000-0000-0000-0000-00000000000a', 't2', 'first', '00000000-0000-0000-0000-0000000000c1', 'waitingOnUs', now() - interval '3 hours'),
           ('00000000-0000-0000-0000-000000000003', '00000000-0000-0000-0000-00000000000a', 't3', 'second', '00000000-0000-0000-0000-0000000000c1', 'closed', now() - interval '2 hours'),
           ('00000000-0000-0000-0000-000000000004', '00000000-0000-0000-0000-00000000000b', 't4', 'other', '00000000-0000-0000-0000-0000000000c2', 'waitingOnContact', now() - interval '5 hours');
         -- Asked, answered, asked again twice: waiting since the first unanswered one.
         INSERT INTO messages (workspace_id, ticket_id, kind, agent_id, from_email, text, created_at) VALUES
           ('00000000-0000-0000-0000-00000000000a', '00000000-0000-0000-0000-000000000001', 'customer', NULL, 'o@k.no', 'hi', now() - interval '60 minutes'),
           ('00000000-0000-0000-0000-00000000000a', '00000000-0000-0000-0000-000000000002', 'customer', NULL, 'o@k.no', 'q1', now() - interval '180 minutes'),
           ('00000000-0000-0000-0000-00000000000a', '00000000-0000-0000-0000-000000000002', 'agent', '00000000-0000-0000-0000-0000000000a1', NULL, 'a1', now() - interval '150 minutes'),
           ('00000000-0000-0000-0000-00000000000a', '00000000-0000-0000-0000-000000000002', 'customer', NULL, 'o@k.no', 'q2', now() - interval '140 minutes'),
           ('00000000-0000-0000-0000-00000000000a', '00000000-0000-0000-0000-000000000002', 'customer', NULL, 'o@k.no', 'q3', now() - interval '130 minutes');",
    )
    .execute(&owner)
    .await
    .unwrap();
    sqlx::migrate!().run(&owner).await.unwrap();

    let rows: Vec<(String, i32, Option<f64>)> = sqlx::query_as(
        "SELECT subject, number, round(extract(epoch FROM now() - waiting_since(t)) / 60)::float8
         FROM tickets t ORDER BY workspace_id, number",
    )
    .fetch_all(&owner)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("first".into(), 1, Some(140.0)),
            ("second".into(), 2, None),
            ("third".into(), 3, Some(60.0)),
            ("other".into(), 1, None),
        ]
    );
    // The next ticket carries on from there.
    let next: i32 = sqlx::query_scalar(
        "INSERT INTO tickets (workspace_id, token, subject, contact_id)
         VALUES ('00000000-0000-0000-0000-00000000000a', 't5', 'new', '00000000-0000-0000-0000-0000000000c1')
         RETURNING number",
    )
    .fetch_one(&owner)
    .await
    .unwrap();
    assert_eq!(next, 4);
}

// §25: the wake job of an earlier snooze does nothing to a later one; a
// snooze that is over is over even before its job has run.
#[tokio::test]
async fn snoozing_again_moves_the_wake_and_a_due_snooze_is_over() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let t = arrives(&app, "SSO", "x").await;
    let path = format!("/api/tickets/{t}");
    c.patch(&path, json!({ "snoozedUntil": in_hours(3) })).await;
    c.patch(&path, json!({ "snoozedUntil": in_hours(6) })).await;
    // The first snooze's job comes due; the ticket stays snoozed.
    sqlx::query(
        "UPDATE jobs SET run_at = now() WHERE id = (SELECT min(id) FROM jobs WHERE kind = 'wake')",
    )
    .execute(&app.owner)
    .await
    .unwrap();
    app.run_jobs().await;
    let now = get_ticket(&mut c, &t).await;
    assert!(now["snoozedUntil"].is_string());
    assert_eq!(now["snoozeEnded"], false);
    assert_eq!(subjects(&list(&mut c, "view=snoozed").await), ["SSO"]);

    // Its time has come, the job has not run (or gave up): back in its views.
    sqlx::query("UPDATE tickets SET snoozed_until = now() - interval '1 minute'")
        .execute(&app.owner)
        .await
        .unwrap();
    let open = list(&mut c, "view=open").await;
    assert_eq!(subjects(&open), ["SSO"]);
    assert_eq!(open["counts"]["open"], 1);
    assert_eq!(
        subjects(&list(&mut c, "view=snoozed").await),
        Vec::<String>::new()
    );
}

// §17, §25: marking a ticket unread does not bring back an old "Snooze ended".
#[tokio::test]
async fn mark_as_unread_keeps_an_old_snooze_ended_away() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let t = arrives(&app, "SSO", "x").await;
    c.patch(
        &format!("/api/tickets/{t}"),
        json!({ "snoozedUntil": in_hours(3) }),
    )
    .await;
    snooze_ends(&app, &t).await;
    get_ticket(&mut c, &t).await;
    answers(&app, &t, "Any news?").await;
    get_ticket(&mut c, &t).await;

    c.delete(&format!("/api/tickets/{t}/read")).await;
    let row = list(&mut c, "view=open").await["tickets"][0].clone();
    assert_eq!(
        (row["unread"].clone(), row["snoozeEnded"].clone()),
        (json!(true), json!(false))
    );
}

// §33, §35 with ADR 0010: the brain holds the closed thread as it is.
#[tokio::test]
async fn the_brain_follows_replies_on_a_closed_ticket() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let t = arrives(&app, "SSO", "Login fails").await;
    c.patch(&format!("/api/tickets/{t}"), json!({ "status": "closed" }))
        .await;
    let search = |app: &TestApp| {
        let owner = app.owner.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT search::text FROM tickets")
                .fetch_one(&owner)
                .await
                .unwrap()
        }
    };
    assert!(!search(&app).await.contains("'certif'"));

    // Send and close on a ticket that was already closed: the reply is in.
    app.mock.respond(|_, path, _| {
        (path == "/email").then(|| {
            (
                422,
                json!({ "ErrorCode": 406, "Message": "Inactive recipient" }),
            )
        })
    });
    let r = c
        .post(
            &format!("/api/tickets/{t}/replies"),
            json!({ "text": "Rotate the certificate", "status": "closed" }),
        )
        .await;
    assert!(search(&app).await.contains("'certif'"));

    // It bounced and is discarded: out again.
    app.run_jobs().await;
    let id = r.body["id"].as_str().unwrap();
    assert_eq!(c.delete(&format!("/api/messages/{id}")).await.status, 204);
    let now = search(&app).await;
    assert!(
        !now.contains("'certif'") && now.contains("'login'"),
        "{now}"
    );
}

/// Ola's ticket, which arrived an hour ago; its waiting clock.
async fn waiting_an_hour(app: &TestApp, c: &mut Client) -> (String, chrono::DateTime<Utc>) {
    let t = arrives(app, "SSO", "Login fails").await;
    sqlx::query(
        "UPDATE messages SET created_at = now() - interval '1 hour' WHERE ticket_id = $1::uuid;",
    )
    .bind(&t)
    .execute(&app.owner)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE tickets SET created_at = now() - interval '1 hour', waiting_set_at = now() - interval '1 hour'
         WHERE id = $1::uuid",
    )
    .bind(&t)
    .execute(&app.owner)
    .await
    .unwrap();
    let since = waiting_since(c, &t).await.expect("waiting");
    assert!(Utc::now() - since > Duration::minutes(59));
    (t, since)
}

async fn waiting_since(c: &mut Client, t: &str) -> Option<chrono::DateTime<Utc>> {
    get_ticket(c, t).await["waitingSince"]
        .as_str()
        .map(|s| s.parse().unwrap())
}

// §16, §24: "Closed #1", Undo — still waiting an hour, not a minute.
#[tokio::test]
async fn undoing_a_close_keeps_the_waiting_clock() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let (t, since) = waiting_an_hour(&app, &mut c).await;
    let close = c
        .patch(
            "/api/tickets",
            json!({ "tickets": [{ "id": t, "status": "closed" }] }),
        )
        .await;
    assert_eq!(close.body["tickets"][0]["waitingSince"], Value::Null);
    let undo = c
        .patch(
            "/api/tickets",
            json!({ "tickets": [{ "id": t, "status": "new", "snoozedUntil": null }] }),
        )
        .await;
    assert_eq!(undo.status, 200);
    assert_eq!(waiting_since(&mut c, &t).await, Some(since));
}

// §16, §34: a reply taken back within its ten seconds leaves the clock as it was.
#[tokio::test]
async fn undoing_a_reply_keeps_the_waiting_clock() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let (t, since) = waiting_an_hour(&app, &mut c).await;
    let reply = c
        .post(
            &format!("/api/tickets/{t}/replies"),
            json!({ "text": "Oops" }),
        )
        .await;
    assert_eq!(waiting_since(&mut c, &t).await, None);
    let id = reply.body["id"].as_str().unwrap();
    assert_eq!(c.delete(&format!("/api/messages/{id}")).await.status, 204);
    c.patch(
        "/api/tickets",
        json!({ "tickets": [{ "id": t, "status": "new", "ownerId": null }] }),
    )
    .await;
    assert_eq!(waiting_since(&mut c, &t).await, Some(since));
}

// §16: the team had the last word; Waiting on us by hand counts from then,
// and the customer writing again does not restart it.
#[tokio::test]
async fn waiting_on_us_set_by_hand_counts_from_when_it_was_set() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (_, _, mut c) = workspace(&app, "acme", "frank@acme.com", "owner").await;
    let (t, asked) = waiting_an_hour(&app, &mut c).await;
    c.post(
        &format!("/api/tickets/{t}/replies"),
        json!({ "text": "Try this" }),
    )
    .await;
    sqlx::query(
        "UPDATE messages SET created_at = now() - interval '30 minutes' WHERE kind = 'agent'",
    )
    .execute(&app.owner)
    .await
    .unwrap();

    let before = Utc::now();
    c.patch(
        &format!("/api/tickets/{t}"),
        json!({ "status": "waitingOnUs" }),
    )
    .await;
    let set = waiting_since(&mut c, &t).await.expect("waiting");
    assert!(set > asked && set >= before - Duration::seconds(1), "{set}");

    answers(&app, &t, "Did not help").await;
    assert_eq!(waiting_since(&mut c, &t).await, Some(set));
}

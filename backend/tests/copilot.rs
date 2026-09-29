//! Specs 003 (similar cases) and 004 (categories), against a mocked Jev.
//! State is arranged with owner-role SQL so these tests do not depend on the
//! signup or inbound code.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Client, TestApp};
use serde_json::{Map, Value, json};
use uuid::Uuid;

struct Ws {
    id: Uuid,
    owner_agent: Uuid,
    client: Client,
}

async fn workspace(app: &TestApp, slug: &str) -> Ws {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, language, trial_ends_at)
         VALUES ($1, $2, $2, 'english', now() + interval '14 days')",
    )
    .bind(id)
    .bind(slug)
    .execute(&app.owner)
    .await
    .unwrap();
    let mut tx = app.owner.begin().await.unwrap();
    muninn::categories::insert_defaults(&mut tx, id)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (owner_agent, client) = agent(app, id, &format!("frank@{slug}.test"), "owner").await;
    Ws {
        id,
        owner_agent,
        client,
    }
}

async fn agent(app: &TestApp, ws: Uuid, email: &str, role: &str) -> (Uuid, Client) {
    let name = email.split('@').next().unwrap();
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO agents (workspace_id, email, name, role) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(ws)
    .bind(email)
    .bind(name)
    .bind(role)
    .fetch_one(&app.owner)
    .await
    .unwrap();
    let token = muninn::session::new_token();
    sqlx::query("INSERT INTO sessions (token_hash, workspace_id, agent_id) VALUES ($1, $2, $3)")
        .bind(muninn::session::hash_token(&token))
        .bind(ws)
        .bind(id)
        .execute(&app.owner)
        .await
        .unwrap();
    let mut client = app.client();
    client.cookie = Some(format!("muninn_session={token}"));
    (id, client)
}

/// A ticket with a thread. `status = 'closed'` puts it in the brain.
async fn ticket(
    app: &TestApp,
    ws: &Ws,
    status: &str,
    subject: &str,
    thread: &[(&str, &str)],
) -> Uuid {
    let contact: Uuid = sqlx::query_scalar(
        "INSERT INTO contacts (workspace_id, email) VALUES ($1, 'ola@kunde.no')
         ON CONFLICT (workspace_id, email) DO UPDATE SET email = excluded.email RETURNING id",
    )
    .bind(ws.id)
    .fetch_one(&app.owner)
    .await
    .unwrap();
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO tickets (workspace_id, token, subject, status, contact_id) VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(ws.id)
    .bind(Uuid::new_v4().simple().to_string())
    .bind(subject)
    .bind(status)
    .bind(contact)
    .fetch_one(&app.owner)
    .await
    .unwrap();
    for (n, (kind, text)) in thread.iter().enumerate() {
        let agent = (*kind != "customer").then_some(ws.owner_agent);
        sqlx::query(
            "INSERT INTO messages (workspace_id, ticket_id, kind, agent_id, from_email, text, created_at)
             VALUES ($1, $2, $3, $4, 'ola@kunde.no', $5, now() - interval '1 hour' + $6 * interval '1 minute')",
        )
        .bind(ws.id)
        .bind(id)
        .bind(kind)
        .bind(agent)
        .bind(text)
        .bind(n as f64)
        .execute(&app.owner)
        .await
        .unwrap();
    }
    if status == "closed" {
        sqlx::query(
            "UPDATE tickets t SET closed_at = now(), search = to_tsvector('english', t.subject || ' ' ||
               (SELECT string_agg(text, ' ') FROM messages m WHERE m.ticket_id = t.id)) WHERE id = $1",
        )
        .bind(id)
        .execute(&app.owner)
        .await
        .unwrap();
    }
    id
}

/// A new ticket with its jobs enqueued, as inbound mail does it (spec 002 §5).
async fn new_ticket(app: &TestApp, ws: &Ws, subject: &str, text: &str) -> Uuid {
    let id = ticket(app, ws, "new", subject, &[("customer", text)]).await;
    for kind in ["suggest", "categorize"] {
        sqlx::query("INSERT INTO jobs (workspace_id, kind, subject_id) VALUES ($1, $2, $3)")
            .bind(ws.id)
            .bind(kind)
            .bind(id)
            .execute(&app.owner)
            .await
            .unwrap();
    }
    id
}

/// Answer a Jev request: nouls scored by `score` on the past case text,
/// the category choice from `probabilities`.
fn jev_answer(body: &Value, score: &dyn Fn(&str) -> f64, probabilities: &Value) -> Value {
    let mut answers = Map::new();
    for (key, q) in body["questions"].as_object().unwrap() {
        let answer = if q["type"] == "noul" {
            json!({ "type": "noul", "noul": score(q["instructions"]["past_case"].as_str().unwrap()) })
        } else {
            let top = probabilities
                .as_object()
                .unwrap()
                .iter()
                .max_by(|a, b| a.1.as_f64().unwrap().total_cmp(&b.1.as_f64().unwrap()))
                .unwrap()
                .0
                .clone();
            json!({ "type": "choice", "choice": top, "probabilities": probabilities, "confidence": 0.8 })
        };
        answers.insert(key.clone(), answer);
    }
    json!({ "model": "jev-1.13.0", "answers": answers, "usage": { "input_tokens": 1, "output_tokens": 1 } })
}

fn jev(app: &TestApp, score: impl Fn(&str) -> f64 + Send + Sync + 'static, probabilities: Value) {
    app.mock.respond(move |_, path, body| {
        (path == "/v1/systemone").then(|| (200, jev_answer(body, &score, &probabilities)))
    });
}

fn case_calls(app: &TestApp) -> Vec<Value> {
    app.mock
        .calls("/v1/systemone")
        .into_iter()
        .map(|c| c.body)
        .filter(|b| !b["questions"]["case_0"].is_null())
        .collect()
}

async fn category_id(app: &TestApp, ws: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM categories WHERE workspace_id = $1 AND name = $2")
        .bind(ws)
        .bind(name)
        .fetch_one(&app.owner)
        .await
        .unwrap()
}

fn login_probs() -> Value {
    json!({ "Login & access": 0.87, "Bug": 0.05, "Billing": 0.02, "How-to": 0.03, "Feature request": 0.01, "Other": 0.02 })
}

#[tokio::test]
async fn an_empty_brain_is_ready_without_asking_jev() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let mut ws = workspace(&app, "acme").await;
    jev(&app, |_| 0.9, login_probs());
    let t = new_ticket(
        &app,
        &ws,
        "SSO login fails",
        "None of us can log in with SSO",
    )
    .await;
    app.run_jobs().await;

    let res = ws
        .client
        .get(&format!("/api/tickets/{t}/suggestions"))
        .await;
    assert_eq!(res.status, 200);
    assert_eq!(
        res.body,
        json!({ "status": "ready", "brainSize": 0, "suggestions": [] })
    );
    assert!(case_calls(&app).is_empty());
}

#[tokio::test]
async fn candidates_are_judged_and_the_best_three_shown() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let mut ws = workspace(&app, "acme").await;
    let long = "certificate ".repeat(500);
    let best = ticket(
        &app,
        &ws,
        "closed",
        "SSO cert rotation",
        &[
            ("customer", &long),
            ("comment", "Their IdP cert expired"),
            ("agent", "Re-upload the new cert under Settings → SSO"),
        ],
    )
    .await;
    let no_reply = ticket(
        &app,
        &ws,
        "closed",
        "SSO metadata",
        &[("customer", "SSO broken for everyone")],
    )
    .await;
    let third = ticket(
        &app,
        &ws,
        "closed",
        "SSO group mapping",
        &[
            ("customer", "login denied"),
            ("agent", "Fixed the group mapping"),
        ],
    )
    .await;
    ticket(
        &app,
        &ws,
        "closed",
        "SSO login loop",
        &[("customer", "login loops"), ("agent", "Clear cookies")],
    )
    .await;
    ticket(
        &app,
        &ws,
        "closed",
        "SSO timeout",
        &[("customer", "login slow"), ("agent", "Raised the timeout")],
    )
    .await;
    ticket(
        &app,
        &ws,
        "closed",
        "Invoice charged twice",
        &[("customer", "Billed two times in September")],
    )
    .await;
    let scores = [
        ("SSO cert rotation", 0.95),
        ("SSO metadata", 0.8),
        ("SSO group mapping", 0.6),
        ("SSO timeout", 0.55),
        ("SSO login loop", 0.4),
    ];
    jev(
        &app,
        move |case| {
            scores
                .iter()
                .find(|(s, _)| case.starts_with(s))
                .map_or(0.0, |(_, v)| *v)
        },
        login_probs(),
    );

    let t = new_ticket(
        &app,
        &ws,
        "SSO login fails",
        "Since this morning none of us can log in with SSO",
    )
    .await;
    // Even a ticket that is itself in the brain is never its own candidate.
    sqlx::query("UPDATE tickets SET status = 'closed', closed_at = now(), search = to_tsvector('english', subject) WHERE id = $1")
        .bind(t)
        .execute(&app.owner)
        .await
        .unwrap();
    app.run_jobs().await;

    // One Jev request, the five SSO cases in it, the invoice and the ticket itself not.
    let calls = case_calls(&app);
    assert_eq!(calls.len(), 1);
    let questions = calls[0]["questions"].as_object().unwrap();
    assert_eq!(questions.len(), 5);
    assert_eq!(
        calls[0]["state"],
        json!({ "subject": "SSO login fails", "message": "Since this morning none of us can log in with SSO" })
    );
    let rotation = questions
        .values()
        .map(|q| q["instructions"]["past_case"].as_str().unwrap())
        .find(|c| c.starts_with("SSO cert rotation"))
        .unwrap();
    assert_eq!(rotation.chars().count(), 2000);
    assert!(rotation.starts_with("SSO cert rotation\n\nCustomer: certificate certificate"));
    let mapping = questions
        .values()
        .map(|q| q["instructions"]["past_case"].as_str().unwrap())
        .find(|c| c.starts_with("SSO group"))
        .unwrap();
    assert_eq!(
        mapping,
        "SSO group mapping\n\nCustomer: login denied\n\nSolution: Fixed the group mapping"
    );

    // Every score stored, shown or not.
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM suggestions WHERE ticket_id = $1")
        .bind(t)
        .fetch_one(&app.owner)
        .await
        .unwrap();
    assert_eq!(stored, 5);

    let res = ws
        .client
        .get(&format!("/api/tickets/{t}/suggestions"))
        .await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body["status"], "ready");
    assert_eq!(res.body["brainSize"], 7);
    let shown = res.body["suggestions"].as_array().unwrap();
    let ids: Vec<&str> = shown
        .iter()
        .map(|s| s["case"]["ticketId"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [best.to_string(), no_reply.to_string(), third.to_string()]
    );
    assert_eq!(
        shown
            .iter()
            .map(|s| s["score"].as_f64().unwrap())
            .collect::<Vec<_>>(),
        [0.95, 0.8, 0.6]
    );
    assert_eq!(
        shown[0]["solution"]["text"],
        "Re-upload the new cert under Settings → SSO"
    );
    assert_eq!(shown[0]["solution"]["author"]["name"], "frank");
    assert_eq!(shown[0]["case"]["subject"], "SSO cert rotation");
    assert!(shown[0]["case"]["closedAt"].is_string());
    assert_eq!(shown[0]["myFeedback"], Value::Null);
    assert_eq!(shown[1]["solution"], Value::Null);

    // A reopened case has left the brain and drops out of the sidebar.
    sqlx::query(
        "UPDATE tickets SET status = 'waitingOnUs', closed_at = NULL, search = NULL WHERE id = $1",
    )
    .bind(best)
    .execute(&app.owner)
    .await
    .unwrap();
    let res = ws
        .client
        .get(&format!("/api/tickets/{t}/suggestions"))
        .await;
    assert_eq!(res.body["suggestions"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_rate_limit_is_retried_and_a_bad_key_fails_at_once() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let mut ws = workspace(&app, "acme").await;
    ticket(
        &app,
        &ws,
        "closed",
        "SSO cert rotation",
        &[("customer", "SSO broken"), ("agent", "Re-upload the cert")],
    )
    .await;
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    app.mock.respond(move |_, path, body| {
        if path != "/v1/systemone" || body["questions"]["case_0"].is_null() {
            return None;
        }
        if h.fetch_add(1, Ordering::SeqCst) == 0 {
            Some((429, json!({ "error": "rate limited" })))
        } else {
            Some((200, jev_answer(body, &|_| 0.9, &json!({}))))
        }
    });
    let t = new_ticket(&app, &ws, "SSO login fails", "SSO is down").await;
    app.run_jobs().await;
    let res = ws
        .client
        .get(&format!("/api/tickets/{t}/suggestions"))
        .await;
    assert_eq!(res.body["status"], "pending");
    sqlx::query("UPDATE jobs SET run_at = now() WHERE subject_id = $1")
        .bind(t)
        .execute(&app.owner)
        .await
        .unwrap();
    app.run_jobs().await;
    let res = ws
        .client
        .get(&format!("/api/tickets/{t}/suggestions"))
        .await;
    assert_eq!(res.body["status"], "ready");
    assert_eq!(res.body["suggestions"].as_array().unwrap().len(), 1);
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    app.mock.respond(|_, path, _| {
        (path == "/v1/systemone").then(|| (401, json!({ "error": "bad key" })))
    });
    let t = new_ticket(&app, &ws, "SSO again", "SSO is down again").await;
    app.run_jobs().await;
    let res = ws
        .client
        .get(&format!("/api/tickets/{t}/suggestions"))
        .await;
    assert_eq!(
        res.body,
        json!({ "status": "failed", "brainSize": 1, "suggestions": [] })
    );
    let failed: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE subject_id = $1 AND kind = 'suggest' AND failed_at IS NOT NULL AND attempts = 1")
        .bind(t)
        .fetch_one(&app.owner)
        .await
        .unwrap();
    assert_eq!(failed, 1);
    // A failed job never touches the ticket itself.
    let status: String = sqlx::query_scalar("SELECT status FROM tickets WHERE id = $1")
        .bind(t)
        .fetch_one(&app.owner)
        .await
        .unwrap();
    assert_eq!(status, "new");
}

#[tokio::test]
async fn feedback_and_opens_are_one_per_agent() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let mut ws = workspace(&app, "acme").await;
    ticket(
        &app,
        &ws,
        "closed",
        "SSO cert rotation",
        &[("customer", "SSO broken"), ("agent", "Re-upload the cert")],
    )
    .await;
    jev(&app, |_| 0.9, login_probs());
    let t = new_ticket(&app, &ws, "SSO login fails", "SSO is down").await;
    app.run_jobs().await;
    let res = ws
        .client
        .get(&format!("/api/tickets/{t}/suggestions"))
        .await;
    let s = res.body["suggestions"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let fb = format!("/api/suggestions/{s}/feedback");
    assert_eq!(
        ws.client
            .post(&fb, json!({ "verdict": "helped" }))
            .await
            .status,
        204
    );
    let res = ws
        .client
        .get(&format!("/api/tickets/{t}/suggestions"))
        .await;
    assert_eq!(res.body["suggestions"][0]["myFeedback"], "helped");
    assert_eq!(
        ws.client
            .post(&fb, json!({ "verdict": "notRelevant" }))
            .await
            .status,
        204
    );
    let res = ws
        .client
        .get(&format!("/api/tickets/{t}/suggestions"))
        .await;
    assert_eq!(res.body["suggestions"][0]["myFeedback"], "notRelevant");
    let res = ws.client.post(&fb, json!({ "verdict": "meh" })).await;
    assert_eq!(
        (res.status, res.body),
        (400, json!({ "error": "invalidVerdict" }))
    );

    let open = format!("/api/suggestions/{s}/opened");
    assert_eq!(ws.client.post(&open, json!({})).await.status, 204);
    let first: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT opened_at FROM suggestion_feedback WHERE suggestion_id = $1::uuid",
    )
    .bind(&s)
    .fetch_one(&app.owner)
    .await
    .unwrap();
    assert_eq!(ws.client.post(&open, json!({})).await.status, 204);
    let (again, verdict, rows): (chrono::DateTime<chrono::Utc>, String, i64) = sqlx::query_as(
        "SELECT opened_at, verdict, count(*) OVER () FROM suggestion_feedback WHERE suggestion_id = $1::uuid",
    )
    .bind(&s)
    .fetch_one(&app.owner)
    .await
    .unwrap();
    assert_eq!((again, verdict.as_str(), rows), (first, "notRelevant", 1));

    // Another agent's verdict is theirs alone.
    let (_, mut vetle) = agent(&app, ws.id, "vetle@acme.test", "agent").await;
    let res = vetle.get(&format!("/api/tickets/{t}/suggestions")).await;
    assert_eq!(res.body["suggestions"][0]["myFeedback"], Value::Null);

    // Another workspace sees none of it.
    let mut other = workspace(&app, "globex").await;
    assert_eq!(
        other
            .client
            .get(&format!("/api/tickets/{t}/suggestions"))
            .await
            .status,
        404
    );
    assert_eq!(
        other
            .client
            .post(&fb, json!({ "verdict": "helped" }))
            .await
            .status,
        404
    );
    assert_eq!(other.client.post(&open, json!({})).await.status, 404);
    assert_eq!(
        ws.client
            .post(
                &format!("/api/suggestions/{}/opened", Uuid::new_v4()),
                json!({})
            )
            .await
            .status,
        404
    );
}

/// category_id, category_source, jev_category_id, jev_category_probability, category_suggestions
type CategoryColumns = (
    Option<Uuid>,
    Option<String>,
    Option<Uuid>,
    Option<f64>,
    Value,
);

async fn category_of(app: &TestApp, t: Uuid) -> CategoryColumns {
    type Raw = (
        Option<Uuid>,
        Option<String>,
        Option<Uuid>,
        Option<f64>,
        sqlx::types::Json<Value>,
    );
    let (a, b, c, d, sqlx::types::Json(e)): Raw =
        sqlx::query_as(
            "SELECT category_id, category_source, jev_category_id, jev_category_probability, category_suggestions
             FROM tickets WHERE id = $1",
        )
        .bind(t)
        .fetch_one(&app.owner)
        .await
        .unwrap();
    (a, b, c, d, e)
}

#[tokio::test]
async fn categorize_applies_confident_picks_and_offers_chips_otherwise() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let ws = workspace(&app, "acme").await;
    let login = category_id(&app, ws.id, "Login & access").await;
    let bug = category_id(&app, ws.id, "Bug").await;

    // Confident: set, marked as Jev's.
    jev(&app, |_| 0.0, login_probs());
    let t = new_ticket(&app, &ws, "SSO login fails", "SSO is down").await;
    app.run_jobs().await;
    assert_eq!(
        category_of(&app, t).await,
        (
            Some(login),
            Some("jev".into()),
            Some(login),
            Some(0.87),
            json!([])
        )
    );
    let call = app
        .mock
        .calls("/v1/systemone")
        .into_iter()
        .map(|c| c.body)
        .find(|b| !b["questions"]["category"].is_null())
        .unwrap();
    assert_eq!(call["questions"]["category"]["type"], "choice");
    assert_eq!(
        call["questions"]["category"]["criteria"]
            .as_object()
            .unwrap()
            .len(),
        6
    );
    assert_eq!(
        call["questions"]["category"]["criteria"]["Billing"],
        "Invoices, payments, plans, refunds."
    );

    // Unsure: uncategorised, top two offered, Jev's pick still stored.
    jev(
        &app,
        |_| 0.0,
        json!({ "Login & access": 0.41, "Bug": 0.33, "Other": 0.26 }),
    );
    let t = new_ticket(&app, &ws, "Weird error", "Something odd").await;
    app.run_jobs().await;
    let chips = json!([
        { "id": login, "name": "Login & access", "probability": 0.41 },
        { "id": bug, "name": "Bug", "probability": 0.33 },
    ]);
    assert_eq!(
        category_of(&app, t).await,
        (None, None, Some(login), Some(0.41), chips)
    );

    // An agent's choice made before Jev answered is never overwritten.
    jev(&app, |_| 0.0, login_probs());
    let t = new_ticket(&app, &ws, "SSO login fails", "SSO is down").await;
    sqlx::query("UPDATE tickets SET category_id = $2, category_source = 'agent' WHERE id = $1")
        .bind(t)
        .bind(bug)
        .execute(&app.owner)
        .await
        .unwrap();
    app.run_jobs().await;
    assert_eq!(
        category_of(&app, t).await,
        (
            Some(bug),
            Some("agent".into()),
            Some(login),
            Some(0.87),
            json!([])
        )
    );

    // No active categories: nothing asked, nothing set.
    sqlx::query("UPDATE categories SET archived = true WHERE workspace_id = $1")
        .bind(ws.id)
        .execute(&app.owner)
        .await
        .unwrap();
    let before = app.mock.calls("/v1/systemone").len();
    let t = new_ticket(&app, &ws, "Hello", "Anyone there").await;
    app.run_jobs().await;
    assert_eq!(
        category_of(&app, t).await,
        (None, None, None, None, json!([]))
    );
    assert_eq!(app.mock.calls("/v1/systemone").len(), before);
}

#[tokio::test]
async fn categories_are_managed_by_the_owner() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let mut ws = workspace(&app, "acme").await;
    let res = ws.client.get("/api/categories").await;
    let names: Vec<&str> = res.body["categories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "Billing",
            "Bug",
            "Feature request",
            "How-to",
            "Login & access",
            "Other"
        ]
    );

    let res = ws
        .client
        .post(
            "/api/categories",
            json!({ "name": " Integrations ", "description": "Slack, Teams, API." }),
        )
        .await;
    assert_eq!(res.status, 201);
    assert_eq!(res.body["name"], "Integrations");
    assert_eq!(res.body["archived"], false);
    let id = res.body["id"].as_str().unwrap().to_string();

    let err = |status, code: &str| (status, json!({ "error": code }));
    let res = ws
        .client
        .post("/api/categories", json!({ "name": "integrations" }))
        .await;
    assert_eq!((res.status, res.body), err(409, "nameTaken"));
    let res = ws
        .client
        .post("/api/categories", json!({ "name": "  " }))
        .await;
    assert_eq!((res.status, res.body), err(400, "invalidName"));
    let res = ws
        .client
        .post("/api/categories", json!({ "name": "x".repeat(41) }))
        .await;
    assert_eq!((res.status, res.body), err(400, "invalidName"));
    let res = ws
        .client
        .post(
            "/api/categories",
            json!({ "name": "Ok", "description": "x".repeat(201) }),
        )
        .await;
    assert_eq!((res.status, res.body), err(400, "invalidDescription"));

    // Archived: listed last, name free again, unarchiving then collides.
    let res = ws
        .client
        .patch(
            &format!("/api/categories/{id}"),
            json!({ "archived": true }),
        )
        .await;
    assert_eq!((res.status, &res.body["archived"]), (200, &json!(true)));
    let res = ws.client.get("/api/categories").await;
    assert_eq!(res.body["categories"][6]["name"], "Integrations");
    assert_eq!(
        ws.client
            .post("/api/categories", json!({ "name": "Integrations" }))
            .await
            .status,
        201
    );
    let res = ws
        .client
        .patch(
            &format!("/api/categories/{id}"),
            json!({ "archived": false }),
        )
        .await;
    assert_eq!((res.status, res.body), err(409, "nameTaken"));
    let res = ws
        .client
        .patch(
            &format!("/api/categories/{id}"),
            json!({ "name": "Old integrations", "description": "" }),
        )
        .await;
    assert_eq!(
        (res.status, &res.body["name"], &res.body["description"]),
        (200, &json!("Old integrations"), &json!(""))
    );
    assert_eq!(
        ws.client
            .patch(
                &format!("/api/categories/{}", Uuid::new_v4()),
                json!({ "name": "X" })
            )
            .await
            .status,
        404
    );

    // At most 255 active.
    sqlx::query("INSERT INTO categories (workspace_id, name) SELECT $1, 'c' || n FROM generate_series(1, 255 - 7) n")
        .bind(ws.id)
        .execute(&app.owner)
        .await
        .unwrap();
    let res = ws
        .client
        .post("/api/categories", json!({ "name": "One too many" }))
        .await;
    assert_eq!((res.status, res.body), err(409, "tooManyCategories"));
    let res = ws
        .client
        .patch(
            &format!("/api/categories/{id}"),
            json!({ "archived": false }),
        )
        .await;
    assert_eq!((res.status, res.body), err(409, "tooManyCategories"));

    // Agents read the list; only the owner writes it.
    let (_, mut vetle) = agent(&app, ws.id, "vetle@acme.test", "agent").await;
    assert_eq!(vetle.get("/api/categories").await.status, 200);
    let res = vetle
        .post("/api/categories", json!({ "name": "Mine" }))
        .await;
    assert_eq!((res.status, res.body), err(403, "ownerOnly"));
    let res = vetle
        .patch(&format!("/api/categories/{id}"), json!({ "name": "Mine" }))
        .await;
    assert_eq!((res.status, res.body), err(403, "ownerOnly"));
}

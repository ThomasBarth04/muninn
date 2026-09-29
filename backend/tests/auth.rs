//! Spec 001 §1–17 through the HTTP API.

mod common;

use common::{Client, TestApp};
use serde_json::json;
use uuid::Uuid;

/// Wait until `to` has received at least `n` emails.
async fn wait_emails(app: &TestApp, to: &str, n: usize) {
    for _ in 0..100 {
        if emails_to(app, to) >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("expected {n} emails to {to}, got {}", emails_to(app, to));
}

fn emails_to(app: &TestApp, to: &str) -> usize {
    app.mock
        .emails()
        .iter()
        .filter(|e| e["To"].as_str() == Some(to))
        .count()
}

/// Sign up and press the button: a logged-in owner.
async fn signup(app: &TestApp, email: &str, slug: &str) -> Client {
    let mut c = app.client();
    let n = emails_to(app, email);
    let body =
        json!({ "email": email, "workspaceName": "Acme", "slug": slug, "language": "english" });
    assert_eq!(c.post("/api/signup", body).await.status, 202);
    wait_emails(app, email, n + 1).await;
    let r = c
        .post(
            "/api/auth/link",
            json!({ "token": app.mock.link_token(email) }),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    c
}

/// Follow the most recent link sent to `email` in a fresh browser.
async fn follow_link(app: &TestApp, email: &str) -> (Client, common::Res) {
    let mut c = app.client();
    let r = c
        .post(
            "/api/auth/link",
            json!({ "token": app.mock.link_token(email) }),
        )
        .await;
    (c, r)
}

#[tokio::test]
async fn signup_and_login() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let mut anon = app.client();

    let signup_body = json!({ "email": " Frank@Acme.com ", "workspaceName": "Acme", "slug": "acme", "language": "english" });
    assert_eq!(anon.post("/api/signup", signup_body).await.status, 202);
    wait_emails(&app, "frank@acme.com", 1).await;
    let token = app.mock.link_token("frank@acme.com");

    // Reading the link does not consume it (link scanners).
    for _ in 0..2 {
        let info = anon.get(&format!("/api/auth/link?token={token}")).await;
        assert_eq!(info.status, 200);
        assert_eq!(info.body["purpose"], "signup");
        assert_eq!(info.body["workspaceName"], "Acme");
        assert_eq!(info.body["email"], "frank@acme.com");
        assert_eq!(info.body["slug"], "acme");
        assert_eq!(info.body["language"], "english");
    }

    let mut frank = app.client();
    let r = frank
        .post("/api/auth/link", json!({ "token": token }))
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body["redirect"], "/onboarding");
    let cookie = r.headers["set-cookie"].to_str().unwrap();
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("Secure") && cookie.contains("SameSite=Lax")
    );

    let me = frank.get("/api/me").await;
    assert_eq!(me.status, 200);
    assert_eq!(me.body["agent"]["email"], "frank@acme.com");
    assert_eq!(me.body["agent"]["name"], "frank");
    assert_eq!(me.body["agent"]["role"], "owner");
    assert_eq!(me.body["workspace"]["slug"], "acme");
    assert_eq!(
        me.body["workspace"]["inboundAddress"],
        "acme@in.muninn.test"
    );
    assert_eq!(me.body["workspace"]["language"], "english");
    assert_eq!(me.body["workspace"]["billing"]["status"], "trialing");
    assert_eq!(me.body["workspace"]["billing"]["locked"], false);

    // A used link is gone, read or pressed.
    assert_eq!(
        anon.get(&format!("/api/auth/link?token={token}"))
            .await
            .status,
        410
    );
    assert_eq!(
        anon.post("/api/auth/link", json!({ "token": token }))
            .await
            .body["error"],
        "linkExpired"
    );
    assert_eq!(
        anon.post("/api/auth/link", json!({ "token": "nope" }))
            .await
            .status,
        410
    );

    // Taken slug, bad input.
    let taken = json!({ "email": "kari@globex.com", "workspaceName": "Globex", "slug": "acme", "language": "english" });
    assert_eq!(
        anon.post("/api/signup", taken).await.body["error"],
        "slugTaken"
    );
    for (field, value, code) in [
        ("email", "frank", "invalidEmail"),
        ("workspaceName", " ", "invalidWorkspaceName"),
        ("slug", "ab", "invalidSlug"),
        ("slug", "Globex", "invalidSlug"),
        ("slug", "postmaster", "invalidSlug"),
        ("language", "klingon", "unsupportedLanguage"),
    ] {
        let mut body = json!({ "email": "kari@globex.com", "workspaceName": "Globex", "slug": "globex", "language": "simple" });
        body[field] = json!(value);
        let r = anon.post("/api/signup", body).await;
        assert_eq!(
            (r.status, r.body["error"].as_str()),
            (400, Some(code)),
            "{field}={value}"
        );
    }

    // Signing up again with an agent's address sends a login link, same 202.
    let again = json!({ "email": "frank@acme.com", "workspaceName": "Other", "slug": "other", "language": "english" });
    assert_eq!(anon.post("/api/signup", again).await.status, 202);
    wait_emails(&app, "frank@acme.com", 2).await;
    let info = anon
        .get(&format!(
            "/api/auth/link?token={}",
            app.mock.link_token("frank@acme.com")
        ))
        .await;
    assert_eq!(info.body["purpose"], "login");
    assert_eq!(info.body["workspaceName"], "Acme");
    assert_eq!(info.body["slug"], serde_json::Value::Null);

    // Login: unknown addresses get the same 202 and no mail.
    assert_eq!(
        anon.post("/api/login", json!({ "email": "nobody@acme.com" }))
            .await
            .status,
        202
    );
    assert_eq!(
        anon.post("/api/login", json!({ "email": "nope" }))
            .await
            .body["error"],
        "invalidEmail"
    );
    assert_eq!(
        anon.post("/api/login", json!({ "email": "frank@acme.com" }))
            .await
            .status,
        202
    );
    wait_emails(&app, "frank@acme.com", 3).await;
    let (mut second, r) = follow_link(&app, "frank@acme.com").await;
    assert_eq!(r.body["redirect"], "/");
    assert_eq!(second.get("/api/me").await.status, 200);

    // Five links per address per hour; the sixth request still answers 202.
    for _ in 0..3 {
        assert_eq!(
            anon.post("/api/login", json!({ "email": "frank@acme.com" }))
                .await
                .status,
            202
        );
    }
    wait_emails(&app, "frank@acme.com", 5).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(emails_to(&app, "nobody@acme.com"), 0);
    assert_eq!(emails_to(&app, "frank@acme.com"), 5);

    // Logging out ends this session only.
    let out = second.post("/api/logout", json!({})).await;
    assert_eq!(out.status, 204);
    assert!(
        out.headers["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    assert_eq!(second.get("/api/me").await.body["error"], "unauthenticated");
    assert_eq!(frank.get("/api/me").await.status, 200);

    // CSRF: a mutating request must be JSON.
    let raw = reqwest::Client::new()
        .post(format!("{}/api/logout", app.url))
        .header("cookie", frank.cookie.clone().unwrap())
        .body("x=1")
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status(), 415);
    assert_eq!(frank.get("/api/me").await.status, 200);
}

#[tokio::test]
async fn the_team() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let mut frank = signup(&app, "frank@acme.com", "acme").await;

    let r = frank.patch("/api/me", json!({ "name": "  Frank " })).await;
    assert_eq!(r.body["agent"]["name"], "Frank");
    assert_eq!(
        frank.patch("/api/me", json!({ "name": "" })).await.body["error"],
        "invalidName"
    );

    // Invite, re-invite replaces, then join.
    let r = frank
        .post("/api/invites", json!({ "email": "Vetle@Acme.com" }))
        .await;
    assert_eq!(r.status, 201);
    assert_eq!(r.body["email"], "vetle@acme.com");
    assert_eq!(
        frank
            .post("/api/invites", json!({ "email": "vetle@acme.com" }))
            .await
            .status,
        201
    );
    assert_eq!(
        frank
            .post("/api/invites", json!({ "email": "x" }))
            .await
            .body["error"],
        "invalidEmail"
    );
    let team = frank.get("/api/agents").await;
    assert_eq!(team.body["agents"].as_array().unwrap().len(), 1);
    assert_eq!(team.body["invites"].as_array().unwrap().len(), 1);

    wait_emails(&app, "vetle@acme.com", 2).await;
    let mail = app
        .mock
        .emails()
        .into_iter()
        .rev()
        .find(|e| e["To"] == "vetle@acme.com")
        .unwrap();
    assert!(
        mail["Subject"]
            .as_str()
            .unwrap()
            .contains("Frank invited you to join Acme")
    );
    assert_eq!(mail["MessageStream"], "outbound");
    let mut info_client = app.client();
    let info = info_client
        .get(&format!(
            "/api/auth/link?token={}",
            app.mock.link_token("vetle@acme.com")
        ))
        .await;
    assert_eq!(info.body["purpose"], "invite");
    let (mut vetle, r) = follow_link(&app, "vetle@acme.com").await;
    assert_eq!(r.body["redirect"], "/");
    let me = vetle.get("/api/me").await;
    assert_eq!(me.body["agent"]["role"], "agent");
    assert_eq!(me.body["workspace"]["slug"], "acme");
    let vetle_id: Uuid = serde_json::from_value(me.body["agent"]["id"].clone()).unwrap();

    let team = frank.get("/api/agents").await;
    assert_eq!(team.body["agents"].as_array().unwrap().len(), 2);
    assert_eq!(team.body["invites"].as_array().unwrap().len(), 0);

    // One email is one agent anywhere.
    assert_eq!(
        frank
            .post("/api/invites", json!({ "email": "vetle@acme.com" }))
            .await
            .body["error"],
        "emailTaken"
    );
    let mut other = signup(&app, "kari@globex.com", "globex").await;
    assert_eq!(
        other
            .post("/api/invites", json!({ "email": "frank@acme.com" }))
            .await
            .body["error"],
        "emailTaken"
    );

    // Owner only.
    assert_eq!(
        vetle
            .post("/api/invites", json!({ "email": "ola@acme.com" }))
            .await
            .body["error"],
        "ownerOnly"
    );
    assert_eq!(
        vetle
            .delete(&format!("/api/agents/{vetle_id}"))
            .await
            .status,
        403
    );
    assert_eq!(vetle.get("/api/agents").await.status, 200);

    // Removing an agent: sessions end, open tickets unassigned, closed ones kept.
    let ws: Uuid = sqlx::query_scalar("SELECT workspace_id FROM agents WHERE id = $1")
        .bind(vetle_id)
        .fetch_one(&app.owner)
        .await
        .unwrap();
    for (status, token) in [("waitingOnUs", "t-open"), ("closed", "t-closed")] {
        sqlx::query(
            "WITH c AS (INSERT INTO contacts (workspace_id, email) VALUES ($1, $3) RETURNING id)
             INSERT INTO tickets (workspace_id, token, subject, contact_id, owner_id, status)
             SELECT $1, $3, 'SSO', id, $2, $4 FROM c",
        )
        .bind(ws)
        .bind(vetle_id)
        .bind(token)
        .bind(status)
        .execute(&app.owner)
        .await
        .unwrap();
    }
    // Another workspace's agent id is not found here.
    let foreign: Uuid = sqlx::query_scalar("SELECT id FROM agents WHERE email = 'kari@globex.com'")
        .fetch_one(&app.owner)
        .await
        .unwrap();
    assert_eq!(
        frank.delete(&format!("/api/agents/{foreign}")).await.status,
        404
    );
    let frank_id = frank.get("/api/me").await.body["agent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        frank.delete(&format!("/api/agents/{frank_id}")).await.body["error"],
        "cannotRemoveOwner"
    );

    assert_eq!(
        frank
            .delete(&format!("/api/agents/{vetle_id}"))
            .await
            .status,
        204
    );
    assert_eq!(vetle.get("/api/me").await.status, 401);
    assert_eq!(
        frank
            .delete(&format!("/api/agents/{vetle_id}"))
            .await
            .status,
        404
    );
    let owners: Vec<(String, Option<Uuid>)> = sqlx::query_as(
        "SELECT status, owner_id FROM tickets WHERE workspace_id = $1 ORDER BY status",
    )
    .bind(ws)
    .fetch_all(&app.owner)
    .await
    .unwrap();
    assert_eq!(
        owners,
        vec![
            ("closed".into(), Some(vetle_id)),
            ("waitingOnUs".into(), None)
        ]
    );
    assert_eq!(
        frank.get("/api/agents").await.body["agents"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // A removed agent's address can be invited again.
    assert_eq!(
        frank
            .post("/api/invites", json!({ "email": "vetle@acme.com" }))
            .await
            .status,
        201
    );
}

#[tokio::test]
async fn a_locked_workspace() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let mut frank = signup(&app, "frank@acme.com", "acme").await;
    assert_eq!(app.client().get("/api/agents").await.status, 401);

    sqlx::query("UPDATE workspaces SET trial_ends_at = now() - interval '1 minute'")
        .execute(&app.owner)
        .await
        .unwrap();
    let r = frank.get("/api/agents").await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (402, Some("paymentRequired"))
    );
    let me = frank.get("/api/me").await;
    assert_eq!(me.status, 200);
    assert_eq!(me.body["workspace"]["billing"]["status"], "trialExpired");
    assert_eq!(me.body["workspace"]["billing"]["locked"], true);
    // Billing stays reachable so the owner can pay; 502 here is the mock's
    // missing Checkout URL, not the paywall.
    assert_ne!(
        frank.post("/api/billing/checkout", json!({})).await.status,
        402
    );
    assert_eq!(frank.post("/api/logout", json!({})).await.status, 204);
    assert_eq!(frank.get("/api/me").await.status, 401);
}

//! Spec 001 through the HTTP API and the operator's commands: workspaces,
//! login with a password and an authenticator, the team, the beta's billing.

mod common;

use common::{Client, PASSWORD, TestApp, code};
use serde_json::{Value, json};
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

fn last_email(app: &TestApp, to: &str) -> Value {
    app.mock
        .emails()
        .into_iter()
        .rev()
        .find(|e| e["To"] == to)
        .unwrap_or_else(|| panic!("no email to {to}"))
}

async fn login(app: &TestApp, email: &str, password: &str, code: &str) -> (Client, common::Res) {
    let mut c = app.client();
    let r = c
        .post(
            "/api/login",
            json!({ "email": email, "password": password, "code": code }),
        )
        .await;
    (c, r)
}

fn error(r: &common::Res) -> (u16, Option<&str>) {
    (r.status, r.body["error"].as_str())
}

#[tokio::test]
async fn the_operator_creates_a_workspace_and_the_owner_sets_up_their_login() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let create =
        |name: &'static str, slug: &'static str, language: &'static str, owner: &'static str| {
            let st = app.st.clone();
            async move { muninn::admin::create_workspace(&st, name, slug, language, owner).await }
        };

    let done = create("Acme", "acme", "english", " Frank@Acme.com ")
        .await
        .unwrap();
    assert_eq!(
        done,
        "created acme — owner frank@acme.com (setup link emailed), inbound acme@in.muninn.test"
    );
    for (name, slug, language, owner, refusal) in [
        ("Globex", "acme", "english", "kari@globex.com", "slug taken"),
        (
            "Globex",
            "globex",
            "english",
            "frank@acme.com",
            "frank@acme.com is already an agent",
        ),
        ("Globex", "ab", "english", "kari@globex.com", "invalid slug"),
        (
            "Globex",
            "Globex",
            "english",
            "kari@globex.com",
            "invalid slug",
        ),
        (
            "Globex",
            "postmaster",
            "english",
            "kari@globex.com",
            "invalid slug",
        ),
        (
            " ",
            "globex",
            "english",
            "kari@globex.com",
            "invalid workspace name",
        ),
        (
            "Globex",
            "globex",
            "klingon",
            "kari@globex.com",
            "unsupported language",
        ),
        ("Globex", "globex", "english", "kari", "invalid email"),
    ] {
        assert_eq!(
            create(name, slug, language, owner).await,
            Err(refusal.to_string()),
            "{refusal}"
        );
    }
    let workspaces: i64 = sqlx::query_scalar("SELECT count(*) FROM workspaces")
        .fetch_one(&app.owner)
        .await
        .unwrap();
    assert_eq!(workspaces, 1, "a refusal creates nothing");

    // The setup link, on the system stream.
    wait_emails(&app, "frank@acme.com", 1).await;
    let mail = last_email(&app, "frank@acme.com");
    assert_eq!(mail["Subject"], "Set up your login to Acme on Muninn");
    assert_eq!(mail["MessageStream"], "outbound");
    let token = app.mock.link_token("frank@acme.com");

    // Reading the link does not use it (link scanners).
    let mut frank = app.client();
    let mut secret = String::new();
    for _ in 0..2 {
        let info = frank.get(&format!("/api/auth/link?token={token}")).await;
        assert_eq!(info.status, 200);
        assert_eq!(info.body["purpose"], "setup");
        assert_eq!(info.body["workspaceName"], "Acme");
        assert_eq!(info.body["email"], "frank@acme.com");
        secret = info.body["totp"]["secret"].as_str().unwrap().to_string();
        assert_eq!(secret.len(), 32);
        assert!(
            info.body["totp"]["uri"]
                .as_str()
                .unwrap()
                .starts_with("otpauth://totp/Muninn:frank%40acme.com?secret=")
        );
        assert!(
            info.body["totp"]["qrSvg"]
                .as_str()
                .unwrap()
                .contains("<svg")
        );
    }

    // A short password or a wrong code keeps the link.
    let submit = |password: &str, code: String| json!({ "token": token, "password": password, "code": code });
    let r = frank
        .post("/api/auth/link", submit("short", code(&secret, 0)))
        .await;
    assert_eq!(error(&r), (400, Some("invalidPassword")));
    let r = frank
        .post("/api/auth/link", submit(PASSWORD, "000000".into()))
        .await;
    assert_eq!(error(&r), (400, Some("invalidCode")));
    let r = frank
        .post("/api/auth/link", submit(PASSWORD, code(&secret, 0)))
        .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["redirect"], "/onboarding");
    let cookie = r.headers["set-cookie"].to_str().unwrap();
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("Secure") && cookie.contains("SameSite=Lax")
    );

    let me = frank.get("/api/me").await;
    assert_eq!(me.body["agent"]["role"], "owner");
    assert_eq!(me.body["agent"]["name"], "frank");
    assert_eq!(
        me.body["workspace"]["inboundAddress"],
        "acme@in.muninn.test"
    );
    assert_eq!(me.body["workspace"]["billing"]["status"], "active");
    assert_eq!(me.body["workspace"]["billing"]["locked"], false);
    let categories = frank.get("/api/categories").await;
    assert_eq!(categories.body["categories"].as_array().unwrap().len(), 6);

    // A used link is gone.
    assert_eq!(
        frank
            .get(&format!("/api/auth/link?token={token}"))
            .await
            .status,
        410
    );
    let r = app
        .client()
        .post("/api/auth/link", submit(PASSWORD, code(&secret, 1)))
        .await;
    assert_eq!(error(&r), (410, Some("linkExpired")));

    // Once mail has arrived, the owner lands in the inbox.
    let ws: Uuid = serde_json::from_value(me.body["workspace"]["id"].clone()).unwrap();
    sqlx::query(
        "WITH c AS (INSERT INTO contacts (workspace_id, email) VALUES ($1, 'ola@kunde.no') RETURNING id)
         INSERT INTO tickets (workspace_id, token, subject, contact_id) SELECT $1, 't1', 'SSO', id FROM c",
    )
    .bind(ws)
    .execute(&app.owner)
    .await
    .unwrap();
    let (_, r) = login(&app, "frank@acme.com", PASSWORD, &code(&secret, 1)).await;
    assert_eq!(r.body["redirect"], "/");
}

#[tokio::test]
async fn login_takes_password_and_code_and_says_nothing_else() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (mut first, secret) = app.owner("acme", "frank@acme.com").await;

    // Every wrong answer is the same answer.
    for (email, password, code) in [
        ("frank@acme.com", "wrong password!", code(&secret, 1)),
        ("frank@acme.com", PASSWORD, "000000".to_string()),
        ("frank@acme.com", PASSWORD, "".to_string()),
        ("nobody@acme.com", PASSWORD, code(&secret, 1)),
        ("not an email", PASSWORD, code(&secret, 1)),
    ] {
        let (_, r) = login(&app, email, password, &code).await;
        assert_eq!(
            error(&r),
            (401, Some("invalidCredentials")),
            "{email} {password} {code}"
        );
    }

    // The right three log in; the same code does not work twice.
    let (mut second, r) = login(&app, " Frank@Acme.com ", PASSWORD, &code(&secret, 1)).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(second.get("/api/me").await.status, 200);
    let (_, r) = login(&app, "frank@acme.com", PASSWORD, &code(&secret, 1)).await;
    assert_eq!(error(&r), (401, Some("invalidCredentials")));

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
    assert_eq!(first.get("/api/me").await.status, 200);

    // CSRF: a mutating request must be JSON.
    let raw = reqwest::Client::new()
        .post(format!("{}/api/logout", app.url))
        .header("cookie", first.cookie.clone().unwrap())
        .body("x=1")
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status(), 415);
    assert_eq!(first.get("/api/me").await.status, 200);

    // Ten failures in 15 minutes lock the account, right password or not.
    // The login above reset the count; the replayed code was one failure.
    for _ in 0..9 {
        let (_, r) = login(&app, "frank@acme.com", "wrong password!", "000000").await;
        assert_eq!(r.status, 401);
    }
    let (_, r) = login(&app, "frank@acme.com", PASSWORD, &code(&secret, 1)).await;
    assert_eq!(error(&r), (429, Some("tooManyAttempts")));
    // Fifteen minutes after the last failure it opens again.
    // (And step the authenticator back, so the next code is fresh again.)
    sqlx::query(
        "UPDATE agents SET last_failed_login_at = now() - interval '16 minutes',
                           totp_last_step = totp_last_step - 1",
    )
    .execute(&app.owner)
    .await
    .unwrap();
    let (_, r) = login(&app, "frank@acme.com", PASSWORD, &code(&secret, 1)).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
}

#[tokio::test]
async fn a_forgotten_password_needs_the_authenticator_too() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (mut old_session, secret) = app.owner("acme", "frank@acme.com").await;
    let mut anon = app.client();

    assert_eq!(
        anon.post("/api/password-reset", json!({ "email": "nope" }))
            .await
            .body["error"],
        "invalidEmail"
    );
    assert_eq!(
        anon.post("/api/password-reset", json!({ "email": "nobody@acme.com" }))
            .await
            .status,
        202
    );
    assert_eq!(
        anon.post("/api/password-reset", json!({ "email": "Frank@Acme.com" }))
            .await
            .status,
        202
    );
    wait_emails(&app, "frank@acme.com", 2).await;
    assert_eq!(
        last_email(&app, "frank@acme.com")["Subject"],
        "Reset your Muninn password"
    );
    let token = app.mock.link_token("frank@acme.com");
    let info = anon.get(&format!("/api/auth/link?token={token}")).await;
    assert_eq!(info.body["purpose"], "reset");
    assert_eq!(info.body["totp"], Value::Null);

    let new_password = "a brand new passphrase";
    let submit = |code: String| json!({ "token": token, "password": new_password, "code": code });
    // The mailbox alone is not enough.
    let r = anon.post("/api/auth/link", submit("000000".into())).await;
    assert_eq!(error(&r), (400, Some("invalidCode")));
    let r = anon.post("/api/auth/link", submit(code(&secret, 1))).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(anon.get("/api/me").await.status, 200);
    // Other sessions end; the old password is gone; the authenticator stays.
    assert_eq!(old_session.get("/api/me").await.status, 401);
    sqlx::query("UPDATE agents SET totp_last_step = totp_last_step - 1")
        .execute(&app.owner)
        .await
        .unwrap();
    let (_, r) = login(&app, "frank@acme.com", PASSWORD, &code(&secret, 1)).await;
    assert_eq!(r.status, 401);
    let (_, r) = login(&app, "frank@acme.com", new_password, &code(&secret, 1)).await;
    assert_eq!(r.status, 200, "{:?}", r.body);

    // Five reset links per address per hour; the sixth request still answers 202.
    for _ in 0..5 {
        assert_eq!(
            anon.post("/api/password-reset", json!({ "email": "frank@acme.com" }))
                .await
                .status,
            202
        );
    }
    wait_emails(&app, "frank@acme.com", 6).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(emails_to(&app, "frank@acme.com"), 6);
    assert_eq!(emails_to(&app, "nobody@acme.com"), 0);
}

#[tokio::test]
async fn resetting_a_login_and_changing_a_password() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (mut frank, frank_secret) = app.owner("acme", "frank@acme.com").await;
    assert_eq!(
        frank
            .post("/api/invites", json!({ "email": "vetle@acme.com" }))
            .await
            .status,
        201
    );
    let (mut vetle, old_secret) = app.set_up_account("vetle@acme.com").await;
    let team = frank.get("/api/agents").await.body;
    let id = |email: &str| {
        team["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["email"] == email)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let (vetle_id, frank_id) = (id("vetle@acme.com"), id("frank@acme.com"));

    // Vetle lost his phone: Frank resets his login.
    assert_eq!(
        vetle
            .post(&format!("/api/agents/{frank_id}/reset-login"), json!({}))
            .await
            .body["error"],
        "ownerOnly"
    );
    let r = frank
        .post(&format!("/api/agents/{frank_id}/reset-login"), json!({}))
        .await;
    assert_eq!(error(&r), (409, Some("cannotResetOwner")));
    let r = frank
        .post(
            &format!("/api/agents/{}/reset-login", Uuid::new_v4()),
            json!({}),
        )
        .await;
    assert_eq!(r.status, 404);
    let r = frank
        .post(&format!("/api/agents/{vetle_id}/reset-login"), json!({}))
        .await;
    assert_eq!(r.status, 204);
    assert_eq!(vetle.get("/api/me").await.status, 401);
    let (_, r) = login(&app, "vetle@acme.com", PASSWORD, &code(&old_secret, 1)).await;
    assert_eq!(r.status, 401);
    wait_emails(&app, "vetle@acme.com", 2).await;
    assert_eq!(
        last_email(&app, "vetle@acme.com")["Subject"],
        "Set up your login to Acme on Muninn"
    );
    let (mut vetle, new_secret) = app.set_up_account("vetle@acme.com").await;
    assert_ne!(new_secret, old_secret);
    assert_eq!(vetle.get("/api/me").await.status, 200);

    // The owner's own login is the operator's.
    let st = app.st.clone();
    assert_eq!(
        muninn::admin::run(&st, None, &["reset-login".into(), "frank@acme.com".into()]).await,
        Ok("reset frank@acme.com in acme (setup link emailed)".into())
    );
    assert_eq!(
        muninn::admin::run(&st, None, &["reset-login".into(), "nobody@acme.com".into()]).await,
        Err("no agent nobody@acme.com".into())
    );
    assert_eq!(frank.get("/api/me").await.status, 401);
    let (_, r) = login(&app, "frank@acme.com", PASSWORD, &code(&frank_secret, 1)).await;
    assert_eq!(r.status, 401);
    let (mut frank, _) = app.set_up_account("frank@acme.com").await;

    // Changing a password keeps this session and ends the others.
    let (mut vetle_elsewhere, r) =
        login(&app, "vetle@acme.com", PASSWORD, &code(&new_secret, 1)).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let change =
        |current: &str, new: &str| json!({ "currentPassword": current, "newPassword": new });
    let r = vetle
        .post(
            "/api/me/password",
            change("not it at all", "a longer one this time"),
        )
        .await;
    assert_eq!(error(&r), (400, Some("wrongPassword")));
    let r = vetle
        .post("/api/me/password", change(PASSWORD, "short"))
        .await;
    assert_eq!(error(&r), (400, Some("invalidPassword")));
    let r = vetle
        .post(
            "/api/me/password",
            change(PASSWORD, "a longer one this time"),
        )
        .await;
    assert_eq!(r.status, 204);
    assert_eq!(vetle.get("/api/me").await.status, 200);
    assert_eq!(vetle_elsewhere.get("/api/me").await.status, 401);
    assert_eq!(frank.get("/api/me").await.status, 200);
}

#[tokio::test]
async fn the_team() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (mut frank, _) = app.owner("acme", "frank@acme.com").await;

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
    let mail = last_email(&app, "vetle@acme.com");
    assert!(
        mail["Subject"]
            .as_str()
            .unwrap()
            .contains("Frank invited you to join Acme")
    );
    assert_eq!(mail["MessageStream"], "outbound");
    let info = app
        .client()
        .get(&format!(
            "/api/auth/link?token={}",
            app.mock.link_token("vetle@acme.com")
        ))
        .await;
    assert_eq!(info.body["purpose"], "invite");
    let (mut vetle, _) = app.set_up_account("vetle@acme.com").await;
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
    let (mut other, _) = app.owner("globex", "kari@globex.com").await;
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
async fn a_paused_workspace_is_locked_and_resumes() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (mut frank, _) = app.owner("acme", "frank@acme.com").await;
    assert_eq!(app.client().get("/api/agents").await.status, 401);
    let admin = |args: &[&str]| {
        let st = app.st.clone();
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        async move { muninn::admin::run(&st, None, &args).await }
    };

    assert_eq!(admin(&["pause", "acme"]).await, Ok("paused acme".into()));
    assert_eq!(
        admin(&["pause", "nope"]).await,
        Err("no workspace nope".into())
    );
    let r = frank.get("/api/agents").await;
    assert_eq!(error(&r), (402, Some("paymentRequired")));
    let me = frank.get("/api/me").await;
    assert_eq!(me.status, 200);
    assert_eq!(me.body["workspace"]["billing"]["status"], "canceled");
    assert_eq!(me.body["workspace"]["billing"]["locked"], true);

    assert_eq!(admin(&["resume", "acme"]).await, Ok("resumed acme".into()));
    assert_eq!(frank.get("/api/agents").await.status, 200);
    assert!(
        admin(&["frobnicate"])
            .await
            .unwrap_err()
            .starts_with("usage:")
    );
    assert!(admin(&["seats"]).await.unwrap_err().contains("owner role"));
}

#[tokio::test]
async fn seats_count_everyone_who_was_an_agent_that_month() {
    let Some(app) = common::spawn().await else {
        return;
    };
    app.owner("acme", "frank@acme.com").await;
    app.owner("globex", "kari@globex.com").await;
    let ws: Uuid = sqlx::query_scalar("SELECT id FROM workspaces WHERE slug = 'acme'")
        .fetch_one(&app.owner)
        .await
        .unwrap();
    sqlx::query("UPDATE agents SET created_at = '2026-09-01'")
        .execute(&app.owner)
        .await
        .unwrap();
    sqlx::query("UPDATE workspaces SET created_at = '2026-09-01'")
        .execute(&app.owner)
        .await
        .unwrap();
    for (email, created, removed) in [
        ("joined-20th@acme.com", "2026-10-20", None),
        ("left-3rd@acme.com", "2026-09-10", Some("2026-10-03")),
        (
            "left-in-september@acme.com",
            "2026-09-02",
            Some("2026-09-15"),
        ),
        ("joins-in-november@acme.com", "2026-11-02", None),
    ] {
        sqlx::query(
            "INSERT INTO agents (workspace_id, email, name, role, created_at, removed_at)
             VALUES ($1, $2, 'x', 'agent', $3::timestamptz, $4::timestamptz)",
        )
        .bind(ws)
        .bind(email)
        .bind(created)
        .bind(removed)
        .execute(&app.owner)
        .await
        .unwrap();
    }
    muninn::admin::run(&app.st, None, &["pause".into(), "globex".into()])
        .await
        .unwrap();

    let october = muninn::admin::seats(&app.owner, Some("2026-10"))
        .await
        .unwrap();
    assert_eq!(
        october,
        "slug\tworkspace\towner\tstatus\tseats\n\
         acme\tAcme\tfrank@acme.com\tactive\t3\n\
         globex\tGlobex\tkari@globex.com\tpaused\t1"
    );
    let august = muninn::admin::seats(&app.owner, Some("2026-08"))
        .await
        .unwrap();
    assert_eq!(august, "slug\tworkspace\towner\tstatus\tseats");
    assert_eq!(
        muninn::admin::seats(&app.owner, Some("october")).await,
        Err("invalid month".into())
    );
}

//! Test harness: a fresh database per test, migrated as the owner role and
//! used as the app role exactly as in production (ADR 0003), the real router
//! on a random port, and one mock server standing in for Postmark, Jev and
//! Stripe.
//!
//! Needs `TEST_DATABASE_URL`, a superuser URL, e.g.
//! `postgres://postgres:postgres@localhost:55432/postgres`
//! (`docker run -d -p 55432:5432 -e POSTGRES_PASSWORD=postgres postgres:17-alpine`).
//! Without it every integration test prints a notice and passes.
#![allow(dead_code)]

use std::str::FromStr;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use muninn::{AppState, Config};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

pub const INBOUND_PASSWORD: &str = "inbound-secret";
pub const STRIPE_WEBHOOK_SECRET: &str = "whsec_test";

/// One request the app made to an external service.
#[derive(Clone, Debug)]
pub struct Call {
    pub method: String,
    pub path: String,
    /// JSON bodies parsed; anything else (Stripe's form encoding) as a string.
    pub body: Value,
}

type Responder = dyn Fn(&str, &str, &Value) -> Option<(u16, Value)> + Send + Sync;

#[derive(Clone, Default)]
pub struct Mock {
    pub calls: Arc<Mutex<Vec<Call>>>,
    responder: Arc<Mutex<Option<Box<Responder>>>>,
}

impl Mock {
    /// Answer requests with `f(method, path, body)`; `None` falls through to
    /// the default (Postmark accepts every email, everything else is `200 {}`).
    pub fn respond(
        &self,
        f: impl Fn(&str, &str, &Value) -> Option<(u16, Value)> + Send + Sync + 'static,
    ) {
        *self.responder.lock().unwrap() = Some(Box::new(f));
    }

    pub fn calls(&self, path_prefix: &str) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.path.starts_with(path_prefix))
            .cloned()
            .collect()
    }

    /// Emails Postmark was asked to send, oldest first.
    pub fn emails(&self) -> Vec<Value> {
        self.calls("/email").into_iter().map(|c| c.body).collect()
    }

    /// The magic-link token in the most recent email to `to`.
    pub fn link_token(&self, to: &str) -> String {
        let email = self
            .emails()
            .into_iter()
            .rev()
            .find(|e| e["To"].as_str() == Some(to))
            .unwrap_or_else(|| panic!("no email to {to}"));
        let text = email["TextBody"].as_str().unwrap();
        let at = text.find("token=").expect("link in email") + "token=".len();
        text[at..].split_whitespace().next().unwrap().to_string()
    }
}

async fn mock_handler(
    axum::extract::State(mock): axum::extract::State<Mock>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    body: String,
) -> (StatusCode, axum::Json<Value>) {
    let body = serde_json::from_str(&body).unwrap_or(Value::String(body));
    let path = uri.path().to_string();
    mock.calls.lock().unwrap().push(Call {
        method: method.to_string(),
        path: path.clone(),
        body: body.clone(),
    });
    let answer = mock
        .responder
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|f| f(method.as_str(), &path, &body));
    let (status, value) = answer.unwrap_or_else(|| match path.as_str() {
        "/email" => (
            200,
            json!({ "ErrorCode": 0, "Message": "OK", "MessageID": uuid::Uuid::new_v4() }),
        ),
        _ => (200, json!({})),
    });
    (StatusCode::from_u16(status).unwrap(), axum::Json(value))
}

pub struct TestApp {
    pub url: String,
    pub st: AppState,
    /// The owner role: bypasses RLS, for arranging and inspecting state.
    pub owner: PgPool,
    pub mock: Mock,
}

pub async fn spawn() -> Option<TestApp> {
    let Ok(admin_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL unset — skipping integration test");
        return None;
    };
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("admin db");
    // Roles are cluster-wide; every test database shares them.
    sqlx::query(
        "DO $$ BEGIN
           IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'muninn_owner') THEN
             CREATE ROLE muninn_owner LOGIN BYPASSRLS PASSWORD 'muninn';
           END IF;
           IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'muninn_app') THEN
             CREATE ROLE muninn_app LOGIN PASSWORD 'muninn';
           END IF;
         END $$",
    )
    .execute(&admin)
    .await
    .ok(); // two tests racing to create the roles: one wins, both proceed
    let name = format!("muninn_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE DATABASE {name} OWNER muninn_owner"
    )))
    .execute(&admin)
    .await
    .expect("create database");

    let base = PgConnectOptions::from_str(&admin_url)
        .unwrap()
        .database(&name);
    let owner = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(base.clone().username("muninn_owner").password("muninn"))
        .await
        .expect("owner db");
    sqlx::migrate!().run(&owner).await.expect("migrations");
    let db = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(base.username("muninn_app").password("muninn"))
        .await
        .expect("app db");

    let mock = Mock::default();
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_url = format!("http://{}", mock_listener.local_addr().unwrap());
    let mock_app = axum::Router::new()
        .fallback(mock_handler)
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(mock_listener, mock_app).await.unwrap() });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let cfg = Config {
        app_url: url.clone(),
        inbound_domain: "in.muninn.test".into(),
        mail_from: "Muninn <login@muninn.test>".into(),
        static_dir: "/nonexistent".into(),
        postmark_api_url: mock_url.clone(),
        postmark_server_token: Some("pm-server".into()),
        postmark_account_token: Some("pm-account".into()),
        postmark_system_stream: "outbound".into(),
        postmark_customer_stream: "customers".into(),
        postmark_trial_stream: "trials".into(),
        postmark_inbound_user: "postmark".into(),
        postmark_inbound_password: INBOUND_PASSWORD.into(),
        jev_api_url: mock_url.clone(),
        jev_api_key: Some("jev-key".into()),
        stripe_api_url: mock_url,
        stripe_secret_key: Some("sk_test".into()),
        stripe_webhook_secret: Some(STRIPE_WEBHOOK_SECRET.into()),
        stripe_price_id: Some("price_seat".into()),
    };
    let st = AppState::new(db, cfg);
    let app = muninn::router(st.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    Some(TestApp {
        url,
        st,
        owner,
        mock,
    })
}

impl TestApp {
    pub fn client(&self) -> Client {
        Client {
            base: self.url.clone(),
            cookie: None,
            http: reqwest::Client::new(),
        }
    }

    pub async fn run_jobs(&self) -> usize {
        muninn::jobs::run_due(&self.st).await
    }

    /// Wait for fire-and-forget system mail (magic links) to reach the mock.
    pub async fn wait_for_email(&self, to: &str) {
        for _ in 0..100 {
            if self
                .mock
                .emails()
                .iter()
                .any(|e| e["To"].as_str() == Some(to))
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("no email to {to}");
    }
}

/// A browser: JSON in and out, with the session cookie carried by hand
/// (the cookie is `Secure`, which a cookie jar would refuse over http).
pub struct Client {
    pub base: String,
    pub cookie: Option<String>,
    http: reqwest::Client,
}

pub struct Res {
    pub status: u16,
    pub body: Value,
    pub headers: reqwest::header::HeaderMap,
}

impl Client {
    pub async fn req(&mut self, method: &str, path: &str, body: Option<Value>) -> Res {
        let method = reqwest::Method::from_str(method).unwrap();
        let mut req = self
            .http
            .request(method.clone(), format!("{}{}", self.base, path));
        if method != reqwest::Method::GET {
            req = req
                .header("content-type", "application/json")
                .body(body.unwrap_or(json!({})).to_string());
        }
        if let Some(c) = &self.cookie {
            req = req.header("cookie", c);
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        if let Some(set) = headers.get("set-cookie").and_then(|v| v.to_str().ok()) {
            let pair = set.split(';').next().unwrap().to_string();
            self.cookie = if pair.ends_with('=') {
                None
            } else {
                Some(pair)
            };
        }
        let text = res.text().await.unwrap();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Res {
            status,
            body,
            headers,
        }
    }

    pub async fn get(&mut self, path: &str) -> Res {
        self.req("GET", path, None).await
    }
    pub async fn post(&mut self, path: &str, body: Value) -> Res {
        self.req("POST", path, Some(body)).await
    }
    pub async fn patch(&mut self, path: &str, body: Value) -> Res {
        self.req("PATCH", path, Some(body)).await
    }
    pub async fn put(&mut self, path: &str, body: Value) -> Res {
        self.req("PUT", path, Some(body)).await
    }
    pub async fn delete(&mut self, path: &str) -> Res {
        self.req("DELETE", path, None).await
    }
}

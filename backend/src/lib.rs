pub mod api;
pub mod auth;
pub mod billing;
pub mod categories;
pub mod copilot;
pub mod db;
pub mod error;
pub mod inbound;
pub mod jev;
pub mod jobs;
pub mod mail;
pub mod session;
pub mod tickets;

use std::sync::Arc;

use axum::{Router, middleware};
use sqlx::PgPool;
use tower_http::services::{ServeDir, ServeFile};

pub use error::ApiError;

/// Everything comes from the environment (ADR 0008: `deploy/.env`).
/// A missing external credential turns that integration off instead of
/// failing startup, so the app runs locally with nothing but a database.
#[derive(Clone, Debug)]
pub struct Config {
    /// Public origin, used in links in emails and Stripe return URLs.
    pub app_url: String,
    /// `in.muninn.io` — workspaces receive at `<slug>@<inbound_domain>`.
    pub inbound_domain: String,
    /// From header for magic links: `Muninn <login@muninn.io>`.
    pub mail_from: String,
    pub static_dir: String,

    pub postmark_api_url: String,
    /// None: emails are logged instead of sent (local development).
    pub postmark_server_token: Option<String>,
    /// For the domains API (spec 002 §23).
    pub postmark_account_token: Option<String>,
    /// Login and invite mail.
    pub postmark_system_stream: String,
    /// Replies from workspaces with an active subscription.
    pub postmark_customer_stream: String,
    /// Replies from trial workspaces (ADR 0009).
    pub postmark_trial_stream: String,
    /// Basic-auth credentials in the inbound webhook URL (spec 002 §1).
    pub postmark_inbound_user: String,
    pub postmark_inbound_password: String,

    pub jev_api_url: String,
    pub jev_api_key: Option<String>,

    pub stripe_api_url: String,
    pub stripe_secret_key: Option<String>,
    pub stripe_webhook_secret: Option<String>,
    /// The per-seat monthly price (spec 001 open question 1: config, not code).
    pub stripe_price_id: Option<String>,
}

impl Config {
    pub fn from_env() -> Config {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let or = |k: &str, d: &str| var(k).unwrap_or_else(|| d.to_string());
        Config {
            app_url: or("APP_URL", "http://localhost:3000")
                .trim_end_matches('/')
                .to_string(),
            inbound_domain: or("INBOUND_DOMAIN", "in.muninn.io"),
            mail_from: or("MAIL_FROM", "Muninn <login@muninn.io>"),
            static_dir: or("STATIC_DIR", "../frontend/dist"),
            postmark_api_url: or("POSTMARK_API_URL", "https://api.postmarkapp.com"),
            postmark_server_token: var("POSTMARK_SERVER_TOKEN"),
            postmark_account_token: var("POSTMARK_ACCOUNT_TOKEN"),
            postmark_system_stream: or("POSTMARK_SYSTEM_STREAM", "outbound"),
            postmark_customer_stream: or("POSTMARK_CUSTOMER_STREAM", "customers"),
            postmark_trial_stream: or("POSTMARK_TRIAL_STREAM", "trials"),
            postmark_inbound_user: or("POSTMARK_INBOUND_USER", "postmark"),
            postmark_inbound_password: var("POSTMARK_INBOUND_PASSWORD")
                .expect("POSTMARK_INBOUND_PASSWORD is required"),
            jev_api_url: or("JEV_API_URL", "https://api.typesafe.ai"),
            jev_api_key: var("JEV_API_KEY"),
            stripe_api_url: or("STRIPE_API_URL", "https://api.stripe.com"),
            stripe_secret_key: var("STRIPE_SECRET_KEY"),
            stripe_webhook_secret: var("STRIPE_WEBHOOK_SECRET"),
            stripe_price_id: var("STRIPE_PRICE_ID"),
        }
    }

    pub fn inbound_address(&self, slug: &str) -> String {
        format!("{slug}@{}", self.inbound_domain)
    }
}

#[derive(Clone)]
pub struct AppState {
    /// Connected as muninn_app. Tenant data only through `db::tenant_tx`.
    pub db: PgPool,
    pub http: reqwest::Client,
    pub cfg: Arc<Config>,
}

impl AppState {
    pub fn new(db: PgPool, cfg: Config) -> AppState {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("http client");
        AppState {
            db,
            http,
            cfg: Arc::new(cfg),
        }
    }
}

/// `/api/*` (session, JSON), `/hooks/*` (Postmark, Stripe), and the SPA for
/// everything else (ADR 0007).
pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .merge(auth::routes())
        .merge(billing::routes())
        .merge(tickets::routes())
        .merge(mail::routes())
        .merge(copilot::routes())
        .merge(categories::routes())
        .fallback(|| async { ApiError::not_found() })
        .layer(middleware::from_fn(session::require_json));

    let hooks = Router::new()
        .merge(inbound::routes())
        .merge(billing::hooks());

    let spa = ServeDir::new(&state.cfg.static_dir).fallback(ServeFile::new(format!(
        "{}/index.html",
        state.cfg.static_dir
    )));

    Router::new()
        .nest("/api", api)
        .nest("/hooks", hooks)
        .fallback_service(spa)
        .with_state(state)
}

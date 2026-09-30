pub mod admin;
pub mod api;
pub mod auth;
pub mod billing;
pub mod categories;
pub mod copilot;
pub mod credentials;
pub mod db;
pub mod error;
pub mod hubspot;
pub mod import;
pub mod inbound;
pub mod jev;
pub mod jobs;
pub mod mail;
pub mod session;
pub mod snippets;
pub mod tickets;
pub mod views;

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use axum::{Router, middleware};
use sqlx::PgPool;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

pub use error::ApiError;

/// Every byte but RFC 3986's unreserved characters as `%XX`: for header
/// parameters (RFC 5987) and URI components.
pub fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Everything comes from the environment (ADR 0008: `deploy/.env`).
/// A missing external credential turns that integration off instead of
/// failing startup, so the app runs locally with nothing but a database.
#[derive(Clone, Debug)]
pub struct Config {
    /// Public origin, used in links in emails and Stripe return URLs.
    pub app_url: String,
    /// `in.muninn.io` — workspaces receive at `<slug>@<inbound_domain>`.
    pub inbound_domain: String,
    /// From header of setup, reset and invite links: `Muninn <login@muninn.io>`.
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
    /// The per-seat monthly price (spec 001 §23: configuration, not code).
    pub stripe_price_id: Option<String>,

    pub hubspot_api_url: String,
    /// HubSpot's own app: the consent page and every ticket's record.
    pub hubspot_app_url: String,
    /// Without both, the HubSpot sync is off (spec 007 §1).
    pub hubspot_client_id: Option<String>,
    pub hubspot_client_secret: Option<String>,
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
            hubspot_api_url: or("HUBSPOT_API_URL", "https://api.hubapi.com"),
            hubspot_app_url: or("HUBSPOT_APP_URL", "https://app.hubspot.com"),
            hubspot_client_id: var("HUBSPOT_CLIENT_ID"),
            hubspot_client_secret: var("HUBSPOT_CLIENT_SECRET"),
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

/// `/api/*` (session, JSON), `/hooks/*` (Postmark, Stripe, HubSpot), `/healthz`, and
/// the SPA for everything else (ADR 0007).
pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .merge(auth::routes())
        .merge(billing::routes())
        .merge(tickets::routes())
        .merge(mail::routes())
        .merge(copilot::routes())
        .merge(categories::routes())
        .merge(views::routes())
        .merge(snippets::routes())
        .merge(hubspot::routes())
        .fallback(|| async { ApiError::not_found() })
        .layer(middleware::from_fn(session::require_json));

    let hooks = Router::new()
        .merge(inbound::routes())
        .merge(billing::hooks())
        .merge(hubspot::hooks());

    let spa = spa(&state.cfg.static_dir);

    Router::new()
        .nest("/api", api)
        .nest("/hooks", hooks)
        .route("/healthz", get(healthz))
        .fallback_service(spa)
        // 5xx are logged with method and path. Never the query: `/api/auth/link?token=` is a login.
        // ponytail: RUST_LOG=tower_http=debug logs every request when you need an access log.
        .layer(
            TraceLayer::new_for_http().make_span_with(|req: &axum::extract::Request| {
                tracing::info_span!("request", method = %req.method(), path = %req.uri().path())
            }),
        )
        .with_state(state)
}

/// The built SPA. Vite names assets by content hash, so they are cached for
/// good and a missing one is a 404 — not index.html, which the browser would
/// then keep under that name. index.html is revalidated on every load, so a
/// deploy is picked up without a hard reload.
pub fn spa(dir: &str) -> Router {
    Router::new()
        .nest_service("/assets", ServeDir::new(format!("{dir}/assets")))
        .fallback_service(ServeDir::new(dir).fallback(ServeFile::new(format!("{dir}/index.html"))))
        .layer(middleware::map_response(
            |uri: axum::http::Uri, mut res: Response| async move {
                let value = if uri.path().starts_with("/assets/") && res.status().is_success() {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                };
                res.headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
                res
            },
        ))
}

/// For the uptime monitor (ADR 0008): `200 ok`, or `503` naming what is wrong.
/// Checks the database answers, no job ran out of attempts in the last hour,
/// WAL archiving is not failing, and — where archiving is on, i.e. production —
/// a base backup finished in the last 26 hours.
async fn healthz(State(st): State<AppState>) -> (StatusCode, String) {
    let checks: Result<(bool, bool, bool), sqlx::Error> = sqlx::query_as(
        "SELECT
            EXISTS (SELECT 1 FROM jobs WHERE failed_at > now() - interval '1 hour'),
            (SELECT coalesce(last_failed_time > last_archived_time, last_failed_time IS NOT NULL)
             FROM pg_stat_archiver),
            current_setting('archive_mode') <> 'off'
              AND coalesce((SELECT max(finished_at) FROM base_backups), '-infinity')
                  < now() - interval '26 hours'",
    )
    .fetch_one(&st.db)
    .await;
    let problems: Vec<&str> = match checks {
        Err(e) => {
            tracing::error!("healthz: {e}");
            vec!["database unreachable"]
        }
        Ok((jobs, archiving, backup)) => [
            (jobs, "a job failed in the last hour (see jobs.last_error)"),
            (archiving, "WAL archiving is failing"),
            (backup, "no base backup in the last 26 hours"),
        ]
        .into_iter()
        .filter_map(|(bad, what)| bad.then_some(what))
        .collect(),
    };
    if problems.is_empty() {
        (StatusCode::OK, "ok\n".into())
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, problems.join("\n") + "\n")
    }
}

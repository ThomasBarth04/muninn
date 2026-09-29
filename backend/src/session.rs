//! Who is calling: the session cookie, the 401/402 gate, and the CSRF rule
//! (ADR 0006).

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::api::Billing;
use crate::db::tenant_tx;
use crate::{ApiError, AppState};

pub const COOKIE: &str = "muninn_session";

/// 32 random bytes, base64url: magic link tokens and session tokens alike.
pub fn new_token() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

/// Only the hash is stored, so a copy of the database logs nobody in.
pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// The session lives 30 days from last use on the server; the cookie is only
/// the carrier, so it can outlive that (browsers cap Max-Age at 400 days).
pub fn session_cookie(token: &str) -> String {
    format!("{COOKIE}={token}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=34560000")
}

pub fn clear_cookie() -> String {
    format!("{COOKIE}=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0")
}

pub fn cookie_token(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(COOKIE)?.strip_prefix('='))
        .filter(|t| !t.is_empty())
}

/// The logged-in agent. Taking this as an argument is what makes a route
/// answer `401 unauthenticated`, and on a locked workspace
/// `402 paymentRequired` — except the routes spec 001 §24 leaves open.
#[derive(Clone, Debug)]
pub struct Auth {
    pub agent_id: Uuid,
    pub workspace_id: Uuid,
    pub is_owner: bool,
    pub billing_status: String,
    pub trial_ends_at: DateTime<Utc>,
    pub token_hash: Vec<u8>,
}

impl Auth {
    pub fn require_owner(&self) -> Result<(), ApiError> {
        if self.is_owner {
            Ok(())
        } else {
            Err(ApiError::owner_only())
        }
    }

    pub fn billing(&self) -> Billing {
        billing(&self.billing_status, self.trial_ends_at)
    }
}

/// `billing_status` as stored plus the clock → what spec 001 exposes.
pub fn billing(status: &str, trial_ends_at: DateTime<Utc>) -> Billing {
    let status = match status {
        "trialing" if trial_ends_at <= Utc::now() => "trialExpired",
        "trialing" => "trialing",
        "active" => "active",
        "past_due" => "pastDue",
        _ => "canceled",
    };
    Billing {
        status: status.to_string(),
        trial_ends_at,
        locked: matches!(status, "trialExpired" | "canceled"),
    }
}

/// Paid, for the purposes of the send cap and the Postmark stream (ADR 0009).
/// `past_due` is still a subscription Stripe is collecting on.
pub fn is_paid(billing_status: &str) -> bool {
    matches!(billing_status, "active" | "past_due")
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Auth, ApiError> {
        let token = cookie_token(parts).ok_or_else(ApiError::unauthenticated)?;
        let token_hash = hash_token(token);
        let (workspace_id, agent_id): (Uuid, Uuid) = sqlx::query_as(
            "UPDATE sessions SET last_used_at = now()
             WHERE token_hash = $1 AND last_used_at > now() - interval '30 days'
             RETURNING workspace_id, agent_id",
        )
        .bind(&token_hash)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(ApiError::unauthenticated)?;

        let mut tx = tenant_tx(&st.db, workspace_id).await?;
        let (role, billing_status, trial_ends_at): (String, String, DateTime<Utc>) =
            sqlx::query_as(
                "SELECT a.role, w.billing_status, w.trial_ends_at
             FROM agents a JOIN workspaces w ON w.id = a.workspace_id
             WHERE a.id = $1 AND a.workspace_id = $2 AND a.removed_at IS NULL",
            )
            .bind(agent_id)
            .bind(workspace_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(ApiError::unauthenticated)?;
        tx.commit().await?;

        let auth = Auth {
            agent_id,
            workspace_id,
            is_owner: role == "owner",
            billing_status,
            trial_ends_at,
            token_hash,
        };
        // Nested routers see the path without `/api`; strip it either way.
        let path = parts.uri.path();
        let path = path.strip_prefix("/api").unwrap_or(path);
        let open = path == "/me" || path == "/logout" || path.starts_with("/billing/");
        if auth.billing().locked && !open {
            return Err(ApiError::new(
                StatusCode::PAYMENT_REQUIRED,
                "paymentRequired",
            ));
        }
        Ok(auth)
    }
}

/// CSRF defence (ADR 0006): with `SameSite=Lax`, a cross-site form can still
/// send a simple POST without the cookie — but it cannot set this header.
pub async fn require_json(req: Request, next: Next) -> Response {
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let json = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if safe || json {
        next.run(req).await
    } else {
        ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "jsonRequired").into_response()
    }
}

/// `Json<T>` whose rejection is a contract error (`400 invalidJson`) instead
/// of axum's plain-text one.
pub struct Body<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, st: &S) -> Result<Body<T>, ApiError> {
        axum::Json::<T>::from_request(req, st)
            .await
            .map(|axum::Json(v)| Body(v))
            .map_err(|_| ApiError::bad_request("invalidJson"))
    }
}

/// For PATCH bodies: absent → `None`, `null` → `Some(None)`, value → `Some(Some(v))`.
pub fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(d).map(Some)
}

/// Lowercased and minimally checked; the real check is whether mail arrives.
pub fn normalize_email(raw: &str) -> Option<String> {
    let email = raw.trim().to_lowercase();
    let (local, domain) = email.split_once('@')?;
    let ok = !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && email.len() <= 254
        && !email
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | ',' | '"'));
    ok.then_some(email)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails() {
        assert_eq!(
            normalize_email(" Frank@Acme.com ").as_deref(),
            Some("frank@acme.com")
        );
        for bad in [
            "",
            "frank",
            "@acme.com",
            "frank@acme",
            "frank@acme.",
            "a b@acme.com",
            "a@b@c.com",
            "<a@b.com>",
        ] {
            assert_eq!(normalize_email(bad), None, "{bad}");
        }
    }

    #[test]
    fn billing_states() {
        let future = Utc::now() + chrono::Duration::days(1);
        let past = Utc::now() - chrono::Duration::days(1);
        assert!(!billing("trialing", future).locked);
        assert_eq!(billing("trialing", past).status, "trialExpired");
        assert!(billing("trialing", past).locked);
        assert!(!billing("past_due", past).locked);
        assert!(billing("canceled", future).locked);
    }
}

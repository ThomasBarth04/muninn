//! Spec 001 §23–27: Stripe Checkout, portal, webhook, seat sync — switched
//! off during the beta (no Stripe keys), kept and tested.
//! Stripe over plain form posts — four endpoints do not need an SDK.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;
use uuid::Uuid;

use crate::api::UrlResponse;
use crate::db::{Tx, tenant_tx};
use crate::error::ApiResult;
use crate::jobs::{self, JobError, JobResult};
use crate::session::{Auth, is_paid};
use crate::{ApiError, AppState};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/billing/checkout", post(checkout))
        .route("/billing/portal", post(portal))
}

pub fn hooks() -> Router<AppState> {
    Router::new().route("/stripe", post(webhook))
}

#[derive(Debug)]
enum StripeError {
    Unconfigured,
    /// Network, 429 or 5xx — worth retrying.
    Temporary(String),
    /// Stripe said no to this request.
    Rejected(String),
}

async fn stripe(
    st: &AppState,
    method: Method,
    path: &str,
    form: &[(&str, String)],
) -> Result<Value, StripeError> {
    let key = st
        .cfg
        .stripe_secret_key
        .as_ref()
        .ok_or(StripeError::Unconfigured)?;
    let mut req = st
        .http
        .request(method, format!("{}{path}", st.cfg.stripe_api_url))
        .bearer_auth(key);
    if !form.is_empty() {
        req = req.form(form);
    }
    let res = req
        .send()
        .await
        .map_err(|e| StripeError::Temporary(e.to_string()))?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        Ok(body)
    } else if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        Err(StripeError::Temporary(format!("Stripe {status}")))
    } else {
        Err(StripeError::Rejected(format!(
            "Stripe {status}: {}",
            body["error"]["message"]
        )))
    }
}

fn upstream(e: StripeError) -> ApiError {
    tracing::error!("stripe: {e:?}");
    ApiError::upstream()
}

fn url_of(v: &Value) -> ApiResult<Json<UrlResponse>> {
    let url = v["url"].as_str().ok_or_else(ApiError::upstream)?;
    Ok(Json(UrlResponse { url: url.into() }))
}

/// A subscription on the per-seat price, quantity = agents (§23). The
/// workspace id rides along twice so every later event can find it.
async fn checkout(State(st): State<AppState>, auth: Auth) -> ApiResult<Json<UrlResponse>> {
    auth.require_owner()?;
    if is_paid(&auth.billing_status) {
        return Err(ApiError::conflict("alreadySubscribed"));
    }
    let price = st
        .cfg
        .stripe_price_id
        .clone()
        .ok_or_else(ApiError::upstream)?;
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    let (customer, email, seats): (Option<String>, String, i64) = sqlx::query_as(
        "SELECT w.stripe_customer_id, a.email,
                (SELECT count(*) FROM agents x WHERE x.workspace_id = w.id AND x.removed_at IS NULL)
         FROM workspaces w JOIN agents a ON a.workspace_id = w.id
         WHERE w.id = $1 AND a.id = $2",
    )
    .bind(auth.workspace_id)
    .bind(auth.agent_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    let ws = auth.workspace_id.to_string();
    let back = format!("{}/settings/billing", st.cfg.app_url);
    let mut form = vec![
        ("mode", "subscription".to_string()),
        ("line_items[0][price]", price),
        ("line_items[0][quantity]", seats.to_string()),
        ("client_reference_id", ws.clone()),
        ("subscription_data[metadata][workspace_id]", ws),
        ("success_url", back.clone()),
        ("cancel_url", back),
    ];
    match customer {
        Some(c) => form.push(("customer", c)),
        None => form.push(("customer_email", email)),
    }
    let session = stripe(&st, Method::POST, "/v1/checkout/sessions", &form)
        .await
        .map_err(upstream)?;
    url_of(&session)
}

/// Card, invoices, cancellation — all Stripe's page (§24).
async fn portal(State(st): State<AppState>, auth: Auth) -> ApiResult<Json<UrlResponse>> {
    auth.require_owner()?;
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    let customer: Option<String> =
        sqlx::query_scalar("SELECT stripe_customer_id FROM workspaces WHERE id = $1")
            .bind(auth.workspace_id)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    let customer = customer.ok_or(ApiError::conflict("noSubscription"))?;
    let form = [
        ("customer", customer),
        ("return_url", format!("{}/settings/billing", st.cfg.app_url)),
    ];
    let session = stripe(&st, Method::POST, "/v1/billing_portal/sessions", &form)
        .await
        .map_err(upstream)?;
    url_of(&session)
}

/// `Stripe-Signature: t=<unix>,v1=<hex hmac>[,v1=…]` over `"{t}.{body}"`,
/// within five minutes of now.
fn signature_ok(secret: &str, header: &str, body: &[u8], now: i64) -> bool {
    let mut t = None;
    let mut sigs = vec![];
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => t = Some(v),
            Some(("v1", v)) => sigs.push(v),
            _ => {}
        }
    }
    let Some(t) = t else { return false };
    if t.parse::<i64>().map_or(true, |t| (now - t).abs() > 300) {
        return false;
    }
    sigs.iter().filter_map(|s| hex::decode(s).ok()).any(|sig| {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
        mac.update(t.as_bytes());
        mac.update(b".");
        mac.update(body);
        mac.verify_slice(&sig).is_ok() // constant time
    })
}

/// Stripe → Muninn (§20, §26). One of ADR 0003's cross-tenant routes: the
/// caller has no session, so the workspace comes from the ids Checkout sent
/// to Stripe and Stripe echoes back — `client_reference_id` on the session,
/// `metadata.workspace_id` on the subscription — and the change is then
/// applied through the ordinary tenant helper.
async fn webhook(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<StatusCode> {
    let invalid = || ApiError::bad_request("invalidSignature");
    let secret = st
        .cfg
        .stripe_webhook_secret
        .as_deref()
        .ok_or_else(invalid)?;
    let header = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !signature_ok(secret, header, &body, chrono::Utc::now().timestamp()) {
        return Err(invalid());
    }
    let event: Value =
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("invalidJson"))?;
    let obj = &event["data"]["object"];
    let s = |v: &Value| v.as_str().map(str::to_string);

    // (workspace, new status, customer, subscription); None leaves a field as is.
    let (ws, status, customer, subscription) = match event["type"].as_str() {
        Some("checkout.session.completed") => (
            &obj["client_reference_id"],
            (obj["payment_status"] == "paid").then_some("active"),
            s(&obj["customer"]),
            s(&obj["subscription"]),
        ),
        Some(kind @ ("customer.subscription.updated" | "customer.subscription.deleted")) => {
            let status = match obj["status"].as_str() {
                _ if kind.ends_with("deleted") => Some("canceled"),
                Some("active" | "trialing") => Some("active"),
                Some("past_due") => Some("past_due"),
                Some("canceled" | "unpaid" | "incomplete_expired") => Some("canceled"),
                _ => None,
            };
            (
                &obj["metadata"]["workspace_id"],
                status,
                s(&obj["customer"]),
                s(&obj["id"]),
            )
        }
        _ => return Ok(StatusCode::OK),
    };
    let (Some(ws), Some(event_id)) = (
        ws.as_str().and_then(|w| Uuid::parse_str(w).ok()),
        event["id"].as_str(),
    ) else {
        return Ok(StatusCode::OK);
    };

    let mut tx = tenant_tx(&st.db, ws).await?;
    let fresh = sqlx::query("INSERT INTO stripe_events (id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(event_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    if fresh {
        let found = sqlx::query(
            "UPDATE workspaces SET billing_status = coalesce($2, billing_status),
                    stripe_customer_id = coalesce($3, stripe_customer_id),
                    stripe_subscription_id = coalesce($4, stripe_subscription_id)
             WHERE id = $1",
        )
        .bind(ws)
        .bind(status)
        .bind(customer)
        .bind(subscription)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        // Someone may have joined between Checkout and payment.
        if found && event["type"] == "checkout.session.completed" {
            seats_changed(&mut tx, ws).await?;
        }
    }
    tx.commit().await?;
    Ok(StatusCode::OK)
}

/// Agents were added or removed: bring the subscription quantity along,
/// prorated (§25). Pending invites are not seats. Call inside the
/// transaction that changed the agents.
pub async fn seats_changed(tx: &mut Tx, ws: Uuid) -> Result<(), sqlx::Error> {
    let status: String = sqlx::query_scalar("SELECT billing_status FROM workspaces WHERE id = $1")
        .bind(ws)
        .fetch_one(&mut **tx)
        .await?;
    if is_paid(&status) {
        jobs::enqueue(tx, ws, "seats", None).await?;
    }
    Ok(())
}

fn job_error(e: StripeError) -> JobError {
    match e {
        StripeError::Temporary(m) => JobError::Retry(std::time::Duration::from_secs(60), m),
        StripeError::Rejected(m) => JobError::Fail(m),
        StripeError::Unconfigured => JobError::Fail("STRIPE_SECRET_KEY unset".into()),
    }
}

/// Sets the absolute quantity, so running twice is harmless.
pub async fn seats_job(st: &AppState, ws: Uuid) -> JobResult {
    let mut tx = tenant_tx(&st.db, ws).await?;
    let row: Option<(String, Option<String>, i64)> = sqlx::query_as(
        "SELECT w.billing_status, w.stripe_subscription_id,
                (SELECT count(*) FROM agents a WHERE a.workspace_id = w.id AND a.removed_at IS NULL)
         FROM workspaces w WHERE w.id = $1",
    )
    .bind(ws)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let Some((status, Some(sub), seats)) = row else {
        return Ok(());
    };
    if !is_paid(&status) {
        return Ok(());
    }
    let subscription = stripe(st, Method::GET, &format!("/v1/subscriptions/{sub}"), &[])
        .await
        .map_err(job_error)?;
    // ponytail: one price, so one item; a second product would need to pick by price id.
    let item = subscription["items"]["data"][0]["id"]
        .as_str()
        .ok_or_else(|| JobError::Fail(format!("subscription {sub} has no items")))?;
    let form = [
        ("quantity", seats.to_string()),
        ("proration_behavior", "create_prorations".to_string()),
    ];
    stripe(
        st,
        Method::POST,
        &format!("/v1/subscription_items/{item}"),
        &form,
    )
    .await
    .map_err(job_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, t: i64, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{t}.").as_bytes());
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn signatures() {
        let body = br#"{"id":"evt_1"}"#;
        let good = sign("whsec", 1000, body);
        assert!(signature_ok(
            "whsec",
            &format!("t=1000,v1={good}"),
            body,
            1100
        ));
        // Stripe sends several v1 during secret rotation; one match is enough.
        assert!(signature_ok(
            "whsec",
            &format!("t=1000,v1=00ff,v1={good}"),
            body,
            1000
        ));
        assert!(!signature_ok(
            "other",
            &format!("t=1000,v1={good}"),
            body,
            1000
        ));
        assert!(!signature_ok(
            "whsec",
            &format!("t=1000,v1={good}"),
            b"{}",
            1000
        ));
        assert!(!signature_ok(
            "whsec",
            &format!("t=1000,v1={good}"),
            body,
            1301
        )); // too old
        assert!(!signature_ok("whsec", &format!("v1={good}"), body, 1000));
        assert!(!signature_ok("whsec", "", body, 1000));
    }
}

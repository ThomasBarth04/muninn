//! Outbound mail through Postmark (ADR 0005). This is the only code that
//! knows Postmark's send API.

use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::types::Json as SqlJson;
use uuid::Uuid;

use crate::api::{DnsRecord, SendingDomain, SetSendingDomain};
use crate::db::{Tx, tenant_tx, unique_violation};
use crate::error::ApiResult;
use crate::jobs::{JobError, JobResult};
use crate::session::{Auth, Body, is_paid, normalize_email};
use crate::{ApiError, AppState};

pub struct Email<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub reply_to: Option<&'a str>,
    pub subject: &'a str,
    pub text: &'a str,
    pub stream: &'a str,
    /// Extra headers, e.g. `Message-ID`, `In-Reply-To`, `References`.
    pub headers: Vec<(&'static str, String)>,
}

#[derive(Debug)]
pub enum SendError {
    /// Postmark refused this message for good (inactive recipient, bad address).
    Permanent(String),
    /// Postmark unreachable, rate limited, or erroring — worth retrying.
    Temporary(String),
}

/// Send one email. Without `POSTMARK_SERVER_TOKEN` the email is logged
/// instead, so login links work in local development.
pub async fn send(st: &AppState, email: &Email<'_>) -> Result<(), SendError> {
    let Some(token) = &st.cfg.postmark_server_token else {
        tracing::info!(
            "\n--- email (POSTMARK_SERVER_TOKEN unset, not sent) ---\nFrom: {}\nTo: {}\nSubject: {}\n\n{}\n---",
            email.from,
            email.to,
            email.subject,
            email.text
        );
        return Ok(());
    };
    let headers: Vec<Value> = email
        .headers
        .iter()
        .map(|(k, v)| json!({ "Name": k, "Value": v }))
        .collect();
    let body = json!({
        "From": email.from,
        "To": email.to,
        "ReplyTo": email.reply_to,
        "Subject": email.subject,
        "TextBody": email.text,
        "MessageStream": email.stream,
        "Headers": headers,
    });
    let res = st
        .http
        .post(format!("{}/email", st.cfg.postmark_api_url))
        .header("X-Postmark-Server-Token", token)
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| SendError::Temporary(format!("Postmark unreachable: {e}")))?;
    let status = res.status();
    let reply: Value = res.json().await.unwrap_or(Value::Null);
    let reason = reply["Message"]
        .as_str()
        .unwrap_or("no reason given")
        .to_string();
    match status.as_u16() {
        200 => Ok(()),
        // 422 is Postmark saying no to this message: inactive recipient,
        // invalid address, unconfirmed sender. Retrying will not change it.
        422 => Err(SendError::Permanent(reason)),
        _ => Err(SendError::Temporary(format!("Postmark {status}: {reason}"))),
    }
}

/// System mail — setup, reset and invite links — from Muninn itself, on its own stream.
/// Fire and forget: the caller answers at once and the timing of the answer
/// does not reveal whether a mail was sent (spec 001 §9).
pub fn send_system(st: &AppState, to: String, subject: String, text: String) {
    let st = st.clone();
    tokio::spawn(async move {
        let email = Email {
            from: &st.cfg.mail_from,
            to: &to,
            reply_to: None,
            subject: &subject,
            text: &text,
            stream: &st.cfg.postmark_system_stream,
            headers: vec![],
        };
        if let Err(e) = send(&st, &email).await {
            tracing::error!("system mail to {to}: {e:?}");
        }
    });
}

/// `"Acme Support" <addr>` — quotes and backslashes in the name escaped.
fn display_from(workspace: &str, address: &str) -> String {
    let name = workspace.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{name} Support\" <{address}>")
}

fn reply_subject(subject: &str) -> String {
    if subject
        .get(..3)
        .is_some_and(|p| p.eq_ignore_ascii_case("re:"))
    {
        subject.to_string()
    } else {
        format!("Re: {subject}")
    }
}

#[derive(sqlx::FromRow)]
struct Outgoing {
    text: String,
    message_id: String,
    delivery_status: Option<String>,
    subject: String,
    token: String,
    to_email: String,
    workspace_name: String,
    slug: String,
    billing_status: String,
    from_address: Option<String>,
    sending_domain_verified: bool,
    in_reply_to: Option<String>,
}

/// The `send` job (spec 002 §17–18). Reads in a tenant transaction, then
/// calls Postmark with no transaction open.
pub async fn send_job(st: &AppState, ws: Uuid, message_id: Uuid) -> JobResult {
    let mut tx = tenant_tx(&st.db, ws).await?;
    let row: Option<Outgoing> = sqlx::query_as(
        "SELECT m.text, m.message_id, m.delivery_status, t.subject, t.token, c.email AS to_email,
                w.name AS workspace_name, w.slug, w.billing_status, w.from_address, w.sending_domain_verified,
                (SELECT p.message_id FROM messages p
                 WHERE p.workspace_id = m.workspace_id AND p.ticket_id = m.ticket_id
                   AND p.kind = 'customer' AND p.message_id IS NOT NULL
                 ORDER BY p.created_at DESC LIMIT 1) AS in_reply_to
         FROM messages m
         JOIN tickets t ON t.id = m.ticket_id
         JOIN contacts c ON c.id = t.contact_id
         JOIN workspaces w ON w.id = m.workspace_id
         WHERE m.workspace_id = $1 AND m.id = $2 AND m.kind = 'agent'",
    )
    .bind(ws)
    .bind(message_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    // Already sent, failed, or gone: a re-run after a lease expiry is a no-op.
    let Some(m) = row.filter(|m| m.delivery_status.as_deref() == Some("queued")) else {
        return Ok(());
    };

    // ADR 0005: the token address threads the reply; a verified domain
    // sends as the workspace itself and keeps the token in Reply-To.
    let token_address = format!("{}+{}@{}", m.slug, m.token, st.cfg.inbound_domain);
    let verified = m
        .from_address
        .as_deref()
        .filter(|_| m.sending_domain_verified);
    let from = display_from(&m.workspace_name, verified.unwrap_or(&token_address));
    let subject = reply_subject(&m.subject);
    let mut headers = vec![("Message-ID", format!("<{}>", m.message_id))];
    if let Some(parent) = &m.in_reply_to {
        headers.push(("In-Reply-To", format!("<{parent}>")));
        headers.push(("References", format!("<{parent}>")));
    }
    // ADR 0009: trials send on their own stream, away from paying customers.
    let stream = if is_paid(&m.billing_status) {
        &st.cfg.postmark_customer_stream
    } else {
        &st.cfg.postmark_trial_stream
    };
    let email = Email {
        from: &from,
        to: &m.to_email,
        reply_to: verified.map(|_| token_address.as_str()),
        subject: &subject,
        text: &m.text,
        stream,
        headers,
    };
    let (status, error) = match send(st, &email).await {
        Ok(()) => ("sent", None),
        Err(SendError::Permanent(reason)) => ("failed", Some(reason)),
        Err(SendError::Temporary(msg)) => {
            return Err(JobError::Retry(Duration::from_secs(60), msg));
        }
    };
    let mut tx = tenant_tx(&st.db, ws).await?;
    sqlx::query(
        "UPDATE messages SET delivery_status = $3, delivery_error = $4,
                sent_at = CASE WHEN $3 = 'sent' THEN now() END
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(message_id)
    .bind(status)
    .bind(error)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Out of retries: "Not sent — Retry" (spec 002 §18).
pub async fn send_gave_up(
    st: &AppState,
    ws: Uuid,
    message_id: Uuid,
    error: &str,
) -> Result<(), sqlx::Error> {
    tracing::warn!(%message_id, "reply not sent: {error}");
    let mut tx = tenant_tx(&st.db, ws).await?;
    sqlx::query(
        "UPDATE messages SET delivery_status = 'failed',
                delivery_error = 'Could not reach the mail provider'
         WHERE workspace_id = $1 AND id = $2 AND delivery_status = 'queued'",
    )
    .bind(ws)
    .bind(message_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

// ---------------------------------------------------------------------------
// Sending from the workspace's own domain (spec 002 §23–26). The only code
// that knows Postmark's domains API.
// ---------------------------------------------------------------------------

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/sending-domain",
            get(get_domain).put(put_domain).delete(delete_domain),
        )
        .route("/sending-domain/verify", post(verify_domain))
}

/// Postmark's domain details; `null` where a record is not issued yet.
#[derive(Deserialize, Default)]
#[serde(default)]
struct PmDomain {
    #[serde(rename = "ID")]
    id: i64,
    #[serde(rename = "DKIMVerified")]
    dkim_verified: bool,
    #[serde(rename = "DKIMHost")]
    dkim_host: Option<String>,
    #[serde(rename = "DKIMTextValue")]
    dkim_text_value: Option<String>,
    #[serde(rename = "DKIMPendingHost")]
    dkim_pending_host: Option<String>,
    #[serde(rename = "DKIMPendingTextValue")]
    dkim_pending_text_value: Option<String>,
    #[serde(rename = "ReturnPathDomain")]
    return_path_domain: Option<String>,
    #[serde(rename = "ReturnPathDomainVerified")]
    return_path_verified: bool,
    #[serde(rename = "ReturnPathDomainCNAMEValue")]
    return_path_cname: Option<String>,
}

impl PmDomain {
    fn verified(&self) -> bool {
        self.dkim_verified && self.return_path_verified
    }

    /// The two records the owner adds: a DKIM `TXT` (the pending one while a
    /// key is being issued or rotated) and the Return-Path `CNAME`.
    fn records(&self) -> Vec<DnsRecord> {
        let pick = |pending: &Option<String>, current: &Option<String>| {
            pending
                .clone()
                .filter(|v| !v.is_empty())
                .or_else(|| current.clone())
                .unwrap_or_default()
        };
        vec![
            DnsRecord {
                kind: "TXT".into(),
                host: pick(&self.dkim_pending_host, &self.dkim_host),
                value: pick(&self.dkim_pending_text_value, &self.dkim_text_value),
            },
            DnsRecord {
                kind: "CNAME".into(),
                host: self.return_path_domain.clone().unwrap_or_default(),
                value: self.return_path_cname.clone().unwrap_or_default(),
            },
        ]
    }
}

async fn postmark_domains(
    st: &AppState,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> ApiResult<PmDomain> {
    let token = st.cfg.postmark_account_token.as_deref().ok_or_else(|| {
        tracing::error!("POSTMARK_ACCOUNT_TOKEN unset: sending domains are unavailable");
        ApiError::upstream()
    })?;
    let mut req = st
        .http
        .request(method, format!("{}{path}", st.cfg.postmark_api_url))
        .header("X-Postmark-Account-Token", token)
        .header("Accept", "application/json");
    if let Some(body) = body {
        req = req.json(&body);
    }
    let res = req.send().await.map_err(|e| {
        tracing::error!("postmark domains: {e}");
        ApiError::upstream()
    })?;
    let status = res.status();
    let reply: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        tracing::error!("postmark domains {path}: {status} {reply}");
        return Err(ApiError::upstream());
    }
    Ok(serde_json::from_value(reply).unwrap_or_default())
}

async fn current(tx: &mut Tx, ws: Uuid) -> Result<SendingDomain, sqlx::Error> {
    let (from_address, verified, SqlJson(dns_records)): (
        Option<String>,
        bool,
        SqlJson<Vec<DnsRecord>>,
    ) = sqlx::query_as(
        "SELECT from_address, sending_domain_verified, dns_records FROM workspaces WHERE id = $1",
    )
    .bind(ws)
    .fetch_one(&mut **tx)
    .await?;
    Ok(SendingDomain {
        status: from_address
            .as_ref()
            .map(|_| if verified { "verified" } else { "pending" }.to_string()),
        from_address,
        dns_records,
    })
}

async fn get_domain(auth: Auth, State(st): State<AppState>) -> ApiResult<Json<SendingDomain>> {
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    let domain = current(&mut tx, auth.workspace_id).await?;
    tx.commit().await?;
    Ok(Json(domain))
}

/// §23, §26. The workspace row is written first, so a domain another
/// workspace uses is refused before Postmark hears of it.
async fn put_domain(
    auth: Auth,
    State(st): State<AppState>,
    Body(req): Body<SetSendingDomain>,
) -> ApiResult<Json<SendingDomain>> {
    auth.require_owner()?;
    let address = normalize_email(&req.from_address)
        .ok_or_else(|| ApiError::bad_request("invalidAddress"))?;
    let domain = address
        .split_once('@')
        .map(|(_, d)| d.to_string())
        .unwrap_or_default();
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let (old_domain, old_id): (Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT sending_domain, postmark_domain_id FROM workspaces WHERE id = $1 FOR UPDATE",
    )
    .bind(ws)
    .fetch_one(&mut *tx)
    .await?;
    if old_domain.as_deref() == Some(domain.as_str()) {
        // Same domain, another address at it: DKIM covers the whole domain.
        sqlx::query("UPDATE workspaces SET from_address = $2 WHERE id = $1")
            .bind(ws)
            .bind(&address)
            .execute(&mut *tx)
            .await?;
    } else {
        let taken = sqlx::query(
            "UPDATE workspaces SET from_address = $2, sending_domain = $3, sending_domain_verified = false,
                    postmark_domain_id = NULL, dns_records = '[]'
             WHERE id = $1",
        )
        .bind(ws)
        .bind(&address)
        .bind(&domain)
        .execute(&mut *tx)
        .await;
        match taken {
            Err(e) if unique_violation(&e).is_some() => {
                return Err(ApiError::conflict("domainTaken"));
            }
            other => other?,
        };
        let created = postmark_domains(
            &st,
            Method::POST,
            "/domains",
            Some(json!({ "Name": domain, "ReturnPathDomain": format!("pm-bounces.{domain}") })),
        )
        .await?;
        sqlx::query(
            "UPDATE workspaces SET postmark_domain_id = $2, dns_records = $3, sending_domain_verified = $4
             WHERE id = $1",
        )
        .bind(ws)
        .bind(created.id)
        .bind(SqlJson(created.records()))
        .bind(created.verified())
        .execute(&mut *tx)
        .await?;
    }
    let domain = current(&mut tx, ws).await?;
    tx.commit().await?;
    if let Some(old) = old_id.filter(|_| old_domain.as_deref() != domain_of(&domain)) {
        forget_postmark_domain(&st, old).await;
    }
    Ok(Json(domain))
}

fn domain_of(d: &SendingDomain) -> Option<&str> {
    d.from_address
        .as_deref()
        .and_then(|a| a.split_once('@'))
        .map(|(_, d)| d)
}

/// Best effort: the workspace already stopped using it.
async fn forget_postmark_domain(st: &AppState, id: i64) {
    if postmark_domains(st, Method::DELETE, &format!("/domains/{id}"), None)
        .await
        .is_err()
    {
        tracing::warn!(
            postmark_domain = id,
            "could not delete Postmark domain; remove it by hand"
        );
    }
}

/// §24: "Check now".
async fn verify_domain(auth: Auth, State(st): State<AppState>) -> ApiResult<Json<SendingDomain>> {
    auth.require_owner()?;
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let id: Option<i64> =
        sqlx::query_scalar("SELECT postmark_domain_id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(ws)
            .fetch_one(&mut *tx)
            .await?;
    let id = id.ok_or_else(ApiError::not_found)?;
    postmark_domains(&st, Method::PUT, &format!("/domains/{id}/verifyDkim"), None).await?;
    let now = postmark_domains(
        &st,
        Method::PUT,
        &format!("/domains/{id}/verifyReturnPath"),
        None,
    )
    .await?;
    sqlx::query(
        "UPDATE workspaces SET sending_domain_verified = $2, dns_records = $3 WHERE id = $1",
    )
    .bind(ws)
    .bind(now.verified())
    .bind(SqlJson(now.records()))
    .execute(&mut *tx)
    .await?;
    let domain = current(&mut tx, ws).await?;
    tx.commit().await?;
    Ok(Json(domain))
}

/// §25: back to the default sender at once.
async fn delete_domain(auth: Auth, State(st): State<AppState>) -> ApiResult<StatusCode> {
    auth.require_owner()?;
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let id: Option<i64> =
        sqlx::query_scalar("SELECT postmark_domain_id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(ws)
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query(
        "UPDATE workspaces SET from_address = NULL, sending_domain = NULL, sending_domain_verified = false,
                postmark_domain_id = NULL, dns_records = '[]'
         WHERE id = $1",
    )
    .bind(ws)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if let Some(id) = id {
        forget_postmark_domain(&st, id).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_from_workspace_data() {
        assert_eq!(
            display_from(r#"Acme "Best" \ Co"#, "acme+t@in.muninn.io"),
            r#""Acme \"Best\" \\ Co Support" <acme+t@in.muninn.io>"#
        );
        assert_eq!(reply_subject("Login broken"), "Re: Login broken");
        assert_eq!(reply_subject("RE: Login broken"), "RE: Login broken");
        assert_eq!(reply_subject("Ré"), "Re: Ré");
    }
}

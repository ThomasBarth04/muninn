//! Spec 007: a read-only mirror of one HubSpot account. OAuth connects it;
//! after that there is one operation — re-read a ticket from HubSpot and make
//! Muninn's copy match — driven by the import, the webhook and the hourly
//! check. Muninn never writes to HubSpot, and nothing outside this module
//! knows HubSpot's API.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::types::Json as SqlJson;
use uuid::Uuid;

use crate::api::{
    HubspotConnection, HubspotPipeline, HubspotStatus, ImportProgress, SetPipelines, UrlResponse,
};
use crate::db::{Tx, tenant_tx, unique_violation};
use crate::error::ApiResult;
use crate::import::strip_quotes;
use crate::inbound::html_to_text;
use crate::jobs::{self, JobError, JobResult};
use crate::session::{Auth, Body, hash_token, new_token, normalize_email};
use crate::{ApiError, AppState, percent_encode};

/// Read-only, all of them (Contract). `crm.objects.contacts.read` also reads
/// a ticket's notes (assumed, spec 007).
const SCOPES: &str = "tickets crm.objects.tickets.read conversations.read \
                      crm.objects.contacts.read crm.objects.owners.read";
const TICKET_PROPERTIES: &str = "subject,content,hs_pipeline,hs_pipeline_stage,\
                                 hs_ticket_priority,hubspot_owner_id,createdate,closed_date,hs_lastmodifieddate";
/// §7: a year of seasonal problems, not fixes for a product that has moved on.
const IMPORT_DAYS: i64 = 365;
/// §27: HubSpot allows each app 110 requests per 10 seconds per account.
const CALLS_PER_10S: usize = 100;
/// An import job hands over to its successor well inside the job lease.
const IMPORT_SLICE: Duration = Duration::from_secs(120);
/// More changes than this since the last check are caught up by an import,
/// which works in slices, rather than inline within one job's lease (§19).
const CHECK_INLINE: i64 = 300;
/// HubSpot's default ticket pipeline, the one ticked to start with (§6).
const DEFAULT_PIPELINE: &str = "0";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/integrations/hubspot", get(status).delete(disconnect))
        .route("/integrations/hubspot/connect", post(connect))
        .route("/integrations/hubspot/callback", get(callback))
        .route("/integrations/hubspot/pipelines", put(set_pipelines))
}

pub fn hooks() -> Router<AppState> {
    Router::new().route("/hubspot", post(webhook))
}

/// The app's client id and secret; without both the sync is off (§1).
fn app(st: &AppState) -> Option<(&str, &str)> {
    Some((
        st.cfg.hubspot_client_id.as_deref()?,
        st.cfg.hubspot_client_secret.as_deref()?,
    ))
}

fn available(st: &AppState) -> ApiResult<(&str, &str)> {
    app(st).ok_or_else(ApiError::not_found)
}

fn redirect_uri(st: &AppState) -> String {
    format!("{}/api/integrations/hubspot/callback", st.cfg.app_url)
}

// ---------------------------------------------------------------------------
// Talking to HubSpot
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum HsError {
    /// HubSpot refused the refresh token: uninstalled or revoked (§29).
    Revoked,
    /// A 401 on an access token, worth one refresh.
    Unauthorized,
    /// Network, 429 or 5xx: try again after this long.
    Temporary(Duration, String),
    /// HubSpot said no to this request — a bug on our side.
    Rejected(String),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for HsError {
    fn from(e: sqlx::Error) -> HsError {
        HsError::Db(e)
    }
}

impl From<HsError> for JobError {
    fn from(e: HsError) -> JobError {
        match e {
            HsError::Temporary(after, msg) => JobError::Retry(after, msg),
            HsError::Db(e) => e.into(),
            HsError::Revoked => JobError::Fail("HubSpot connection revoked".into()),
            HsError::Unauthorized => JobError::Fail("HubSpot refused a fresh access token".into()),
            HsError::Rejected(msg) => JobError::Fail(msg),
        }
    }
}

/// Access tokens, in memory only (§5), until a minute before they expire.
static TOKENS: LazyLock<Mutex<HashMap<Uuid, (String, Instant)>>> = LazyLock::new(Default::default);
/// The calls of the last 10 seconds, per workspace (§27).
/// ponytail: per process, which is right for one instance (ADR 0008).
static CALLS: LazyLock<Mutex<HashMap<Uuid, VecDeque<Instant>>>> = LazyLock::new(Default::default);

fn remember(ws: Uuid, access: &str, expires_in: u64) {
    let until = Instant::now() + Duration::from_secs(expires_in.saturating_sub(60));
    TOKENS
        .lock()
        .expect("tokens")
        .insert(ws, (access.to_string(), until));
}

fn forget(ws: Uuid) {
    TOKENS.lock().expect("tokens").remove(&ws);
}

/// Waits until this workspace has fewer than `CALLS_PER_10S` calls in the
/// last 10 seconds, then counts one.
async fn pace(ws: Uuid) {
    loop {
        let wait = {
            let mut calls = CALLS.lock().expect("calls");
            let q = calls.entry(ws).or_default();
            let now = Instant::now();
            while q
                .front()
                .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(10))
            {
                q.pop_front();
            }
            if q.len() < CALLS_PER_10S {
                q.push_back(now);
                return;
            }
            Duration::from_secs(10) - now.duration_since(q[0])
        };
        tokio::time::sleep(wait).await;
    }
}

/// One API call. `Ok(None)` is a 404.
async fn call(
    st: &AppState,
    ws: Uuid,
    token: &str,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Option<Value>, HsError> {
    pace(ws).await;
    let mut req = st
        .http
        .request(method, format!("{}{path}", st.cfg.hubspot_api_url))
        .bearer_auth(token);
    if let Some(body) = body {
        req = req.json(body);
    }
    let res = req.send().await.map_err(|e| {
        HsError::Temporary(Duration::from_secs(30), format!("HubSpot unreachable: {e}"))
    })?;
    let status = res.status();
    let interval = res
        .headers()
        .get("x-hubspot-ratelimit-interval-milliseconds")
        .and_then(|v| v.to_str().ok()?.parse().ok())
        .map(Duration::from_millis);
    let value: Value = res.json().await.unwrap_or(Value::Null);
    match status {
        s if s.is_success() => Ok(Some(value)),
        StatusCode::NOT_FOUND => Ok(None),
        StatusCode::UNAUTHORIZED => Err(HsError::Unauthorized),
        // §27: wait the interval out.
        StatusCode::TOO_MANY_REQUESTS => Err(HsError::Temporary(
            interval.unwrap_or(Duration::from_secs(10)),
            "HubSpot rate limit".into(),
        )),
        s if s.is_server_error() => Err(HsError::Temporary(
            Duration::from_secs(30),
            format!("HubSpot {s}"),
        )),
        s => Err(HsError::Rejected(format!(
            "HubSpot {s}: {}",
            value["message"]
        ))),
    }
}

struct Tokens {
    access: String,
    refresh: String,
    expires_in: u64,
}

/// `POST /oauth/2026-03/token`: exchanging the code, or refreshing.
async fn token_request(st: &AppState, form: &[(&str, &str)]) -> Result<Tokens, HsError> {
    let res = st
        .http
        .post(format!("{}/oauth/2026-03/token", st.cfg.hubspot_api_url))
        .form(form)
        .send()
        .await
        .map_err(|e| {
            HsError::Temporary(Duration::from_secs(30), format!("HubSpot unreachable: {e}"))
        })?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        return Err(HsError::Temporary(
            Duration::from_secs(30),
            format!("HubSpot {status}"),
        ));
    }
    match (status.is_success(), v["access_token"].as_str()) {
        (true, Some(access)) => Ok(Tokens {
            access: access.into(),
            refresh: v["refresh_token"].as_str().unwrap_or_default().into(),
            expires_in: v["expires_in"].as_u64().unwrap_or(1800),
        }),
        _ => Err(HsError::Rejected(format!(
            "HubSpot {status}: {}",
            v["message"]
        ))),
    }
}

/// A HubSpot id, which the API writes as a string or a number.
fn id_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A HubSpot time: ISO-8601, or milliseconds since the epoch.
fn time_of(v: &Value) -> Option<DateTime<Utc>> {
    let ms = |ms: i64| DateTime::from_timestamp_millis(ms);
    match v {
        Value::String(s) => DateTime::parse_from_rfc3339(s)
            .map(|d| d.to_utc())
            .ok()
            .or_else(|| s.parse().ok().and_then(ms)),
        Value::Number(n) => n.as_i64().and_then(ms),
        _ => None,
    }
}

fn ms(t: DateTime<Utc>) -> String {
    t.timestamp_millis().to_string()
}

#[derive(Clone)]
struct Person {
    email: Option<String>,
    name: Option<String>,
}

#[derive(sqlx::FromRow)]
struct Conn {
    id: Uuid,
    portal_id: i64,
    status: String,
    pipelines: SqlJson<Vec<HubspotPipeline>>,
    closed_stages: Vec<String>,
    import_run: Option<Uuid>,
    import_pipelines: Vec<String>,
    import_since: Option<DateTime<Utc>>,
    import_before: Option<DateTime<Utc>>,
    import_after: Option<String>,
    checked_at: Option<DateTime<Utc>>,
    connected_at: DateTime<Utc>,
}

impl Conn {
    fn selected(&self) -> Vec<String> {
        self.pipelines
            .iter()
            .filter(|p| p.selected)
            .map(|p| p.id.clone())
            .collect()
    }
}

/// A message as Muninn will store it.
struct Msg {
    id: String,
    kind: &'static str,
    name: Option<String>,
    email: Option<String>,
    text: String,
    at: DateTime<Utc>,
}

/// One ticket in a search result.
struct Found {
    id: String,
    modified: DateTime<Utc>,
    pipeline: String,
}

/// One page of a ticket search.
struct Page {
    total: i64,
    tickets: Vec<Found>,
    after: Option<String>,
}

/// Where the next page starts: a fresh query from this page's last ticket
/// back. HubSpot's `after` is an offset, which shifts when an earlier ticket
/// is deleted or moved; a time does not, and it never meets search's 10,000
/// cap. The last ticket comes again, which a re-read makes harmless. Only a
/// page that is all one millisecond pages on by offset within it.
/// `None`: that was the last page.
fn next_page(
    page: &Page,
    before: Option<DateTime<Utc>>,
) -> Option<(Option<DateTime<Utc>>, Option<String>)> {
    let after = page.after.as_ref()?;
    let last = page.tickets.last()?.modified;
    if Some(last) == before {
        Some((before, Some(after.clone())))
    } else {
        Some((Some(last), None))
    }
}

/// One job's view of the connection while it talks to HubSpot.
struct Hs<'a> {
    st: &'a AppState,
    ws: Uuid,
    conn: Conn,
    owners: Option<HashMap<String, Person>>,
}

async fn load(st: &AppState, ws: Uuid) -> Result<Option<Conn>, sqlx::Error> {
    let mut tx = tenant_tx(&st.db, ws).await?;
    let conn = sqlx::query_as(
        "SELECT id, portal_id, status, pipelines, closed_stages, import_run, import_pipelines,
                import_since, import_before, import_after, checked_at, connected_at
         FROM hubspot_connections WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(conn)
}

impl<'a> Hs<'a> {
    async fn open(st: &'a AppState, ws: Uuid) -> Result<Option<Hs<'a>>, sqlx::Error> {
        Ok(load(st, ws).await?.map(|conn| Hs {
            st,
            ws,
            conn,
            owners: None,
        }))
    }

    /// The access token, refreshed when it has expired. A refused refresh
    /// marks the connection revoked (§29).
    async fn token(&self) -> Result<String, HsError> {
        if let Some((token, until)) = TOKENS.lock().expect("tokens").get(&self.ws)
            && *until > Instant::now()
        {
            return Ok(token.clone());
        }
        let (client_id, secret) =
            app(self.st).ok_or_else(|| HsError::Rejected("HubSpot app not configured".into()))?;
        let mut tx = tenant_tx(&self.st.db, self.ws).await?;
        let refresh: Option<String> = sqlx::query_scalar(
            "SELECT refresh_token FROM hubspot_connections WHERE workspace_id = $1 AND id = $2",
        )
        .bind(self.ws)
        .bind(self.conn.id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let refresh = refresh.ok_or(HsError::Revoked)?;
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("client_secret", secret),
            ("refresh_token", refresh.as_str()),
        ];
        match token_request(self.st, &form).await {
            Ok(t) => {
                remember(self.ws, &t.access, t.expires_in);
                // A refresh may hand out a new refresh token.
                if !t.refresh.is_empty() && t.refresh != refresh {
                    let mut tx = tenant_tx(&self.st.db, self.ws).await?;
                    sqlx::query(
                        "UPDATE hubspot_connections SET refresh_token = $3 WHERE workspace_id = $1 AND id = $2",
                    )
                    .bind(self.ws)
                    .bind(self.conn.id)
                    .bind(&t.refresh)
                    .execute(&mut *tx)
                    .await?;
                    tx.commit().await?;
                }
                Ok(t.access)
            }
            Err(HsError::Rejected(msg)) => {
                tracing::warn!(workspace = %self.ws, "HubSpot refused the refresh token: {msg}");
                let mut tx = tenant_tx(&self.st.db, self.ws).await?;
                sqlx::query(
                    "UPDATE hubspot_connections SET status = 'revoked', last_error = 'HubSpot disconnected Muninn'
                     WHERE workspace_id = $1 AND id = $2",
                )
                .bind(self.ws)
                .bind(self.conn.id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Err(HsError::Revoked)
            }
            Err(e) => Err(e),
        }
    }

    /// A call with the connection's token; a 401 gets one fresh token.
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Option<Value>, HsError> {
        let token = self.token().await?;
        match call(self.st, self.ws, &token, method.clone(), path, body).await {
            Err(HsError::Unauthorized) => {
                forget(self.ws);
                let token = self.token().await?;
                call(self.st, self.ws, &token, method, path, body).await
            }
            other => other,
        }
    }

    async fn get(&self, path: &str) -> Result<Option<Value>, HsError> {
        self.request(Method::GET, path, None).await
    }

    /// A HubSpot owner, by `id:<ownerId>` or `user:<userId>` (the number in
    /// an agent's `A-` actor id). Every owner is read once per job.
    async fn owner(&mut self, key: &str) -> Result<Option<Person>, HsError> {
        if self.owners.is_none() {
            let mut owners = HashMap::new();
            let mut after = String::new();
            loop {
                let page = self
                    .get(&format!("/crm/owners/2026-09?limit=500{after}"))
                    .await?
                    .unwrap_or(Value::Null);
                for o in page["results"].as_array().into_iter().flatten() {
                    let full = format!(
                        "{} {}",
                        o["firstName"].as_str().unwrap_or(""),
                        o["lastName"].as_str().unwrap_or("")
                    );
                    let person = Person {
                        email: o["email"].as_str().and_then(normalize_email),
                        name: Some(full.trim().to_string()).filter(|n| !n.is_empty()),
                    };
                    if let Some(id) = id_of(&o["id"]) {
                        owners.insert(format!("id:{id}"), person.clone());
                    }
                    if let Some(user) = id_of(&o["userId"]) {
                        owners.insert(format!("user:{user}"), person);
                    }
                }
                match page["paging"]["next"]["after"].as_str() {
                    Some(a) => after = format!("&after={}", percent_encode(a)),
                    None => break,
                }
            }
            self.owners = Some(owners);
        }
        Ok(self.owners.as_ref().and_then(|o| o.get(key).cloned()))
    }

    /// The Muninn key of a HubSpot id: ids are only unique within an account.
    fn key(&self, id: &str) -> String {
        format!("{}/{id}", self.conn.portal_id)
    }

    fn url(&self, id: &str) -> String {
        format!(
            "{}/contacts/{}/record/0-5/{id}",
            self.st.cfg.hubspot_app_url, self.conn.portal_id
        )
    }

    /// §21: gone from HubSpot, or from the ticked pipelines — gone here,
    /// with its thread and every suggestion that points to it.
    async fn delete(&self, id: &str) -> Result<(), HsError> {
        let mut tx = tenant_tx(&self.st.db, self.ws).await?;
        sqlx::query("DELETE FROM tickets WHERE workspace_id = $1 AND hubspot_id = $2")
            .bind(self.ws)
            .bind(self.key(id))
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE hubspot_connections SET skipped = array_remove(skipped, $2) WHERE workspace_id = $1",
        )
        .bind(self.ws)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The thread's messages and the ticket's notes, oldest first (§13–15).
    async fn messages(&mut self, t: &Value, thread: Option<&str>) -> Result<Vec<Msg>, HsError> {
        let created = time_of(&t["properties"]["createdate"]).unwrap_or_else(Utc::now);
        let mut out = vec![];
        if let Some(thread) = thread {
            let base = format!("/conversations/conversations/2026-09/threads/{thread}/messages");
            let mut after = String::new();
            loop {
                let page = self
                    .get(&format!("{base}?limit=500{after}"))
                    .await?
                    .unwrap_or(Value::Null);
                for m in page["results"].as_array().into_iter().flatten() {
                    let kind = match (m["type"].as_str(), m["direction"].as_str()) {
                        (Some("COMMENT"), _) => "comment",
                        (Some("MESSAGE"), Some("INCOMING")) => "customer",
                        (Some("MESSAGE"), _) => "agent",
                        _ => continue, // assignments, status changes, welcome messages
                    };
                    let Some(id) = id_of(&m["id"]) else { continue };
                    let mut text = m["text"].as_str().unwrap_or("").to_string();
                    if m["truncationStatus"]
                        .as_str()
                        .is_some_and(|s| s != "NOT_TRUNCATED")
                        && let Some(full) =
                            self.get(&format!("{base}/{id}/original-content")).await?
                        && let Some(full) = full["text"].as_str()
                    {
                        text = full.to_string();
                    }
                    if kind != "comment" {
                        text = strip_quotes(&text);
                    }
                    let sender = &m["senders"][0];
                    let mut name = sender["name"].as_str().map(str::to_string);
                    let mut email = (sender["deliveryIdentifier"]["type"] == "HS_EMAIL_ADDRESS")
                        .then(|| sender["deliveryIdentifier"]["value"].as_str())
                        .flatten()
                        .and_then(normalize_email);
                    // A HubSpot user: their owner email, which is how an agent
                    // is matched — not the shared inbox the mail went out from.
                    if let Some(user) = sender["actorId"]
                        .as_str()
                        .and_then(|a| a.strip_prefix("A-"))
                        && let Some(p) = self.owner(&format!("user:{user}")).await?
                    {
                        email = p.email.or(email);
                        name = name.or(p.name);
                    }
                    out.push(Msg {
                        id: format!("hubspot:{}", self.key(&id)),
                        kind,
                        name,
                        email,
                        text,
                        at: time_of(&m["createdAt"]).unwrap_or(created),
                    });
                }
                match page["paging"]["next"]["after"].as_str() {
                    Some(a) => after = format!("&after={}", percent_encode(a)),
                    None => break,
                }
            }
        }

        let notes: Vec<String> = t["associations"]["notes"]["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| id_of(&n["id"]))
            .collect();
        for chunk in notes.chunks(100) {
            let body = json!({
                "properties": ["hs_note_body", "hs_timestamp", "hubspot_owner_id"],
                "inputs": chunk.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
            });
            let read = self
                .request(
                    Method::POST,
                    "/crm/objects/2026-09/notes/batch/read",
                    Some(&body),
                )
                .await?
                .unwrap_or(Value::Null);
            for n in read["results"].as_array().into_iter().flatten() {
                let Some(id) = id_of(&n["id"]) else { continue };
                let p = &n["properties"];
                let author = match id_of(&p["hubspot_owner_id"]) {
                    Some(owner) => self.owner(&format!("id:{owner}")).await?,
                    None => None,
                };
                out.push(Msg {
                    id: format!("hubspot:note:{}", self.key(&id)),
                    kind: "comment",
                    name: author.as_ref().and_then(|a| a.name.clone()),
                    email: author.and_then(|a| a.email),
                    // A note's body is HTML; only its text is kept (§14).
                    text: html_to_text(p["hs_note_body"].as_str().unwrap_or("")),
                    at: time_of(&p["hs_timestamp"]).unwrap_or(created),
                });
            }
        }
        out.sort_by(|a, b| (a.at, &a.id).cmp(&(b.at, &b.id)));
        Ok(out)
    }

    /// The one operation (§18, §20): read ticket `id` from HubSpot and make
    /// Muninn's copy match — create it, update it, or delete it.
    async fn sync_ticket(&mut self, id: &str) -> Result<(), HsError> {
        let mut id = id.to_string();
        let t = loop {
            let path = format!(
                "/crm/objects/2026-09/tickets/{id}?properties={TICKET_PROPERTIES}&associations=contacts,notes"
            );
            let Some(t) = self.get(&path).await? else {
                return self.delete(&id).await;
            };
            // A merged-away id answers with the ticket it went into (§21).
            match id_of(&t["id"]) {
                Some(actual) if actual != id => {
                    self.delete(&id).await?;
                    id = actual;
                }
                _ => break t,
            }
        };
        let p = &t["properties"];
        let pipeline = p["hs_pipeline"].as_str().unwrap_or("").to_string();
        if !self.conn.selected().contains(&pipeline) {
            return self.drop_unticked(&id, &pipeline).await;
        }

        let threads = self
            .get(&format!(
                "/conversations/conversations/2026-09/threads?associatedTicketId={id}"
            ))
            .await?
            .unwrap_or(Value::Null);
        let thread = id_of(&threads["results"][0]["id"]);
        let mut messages = self.messages(&t, thread.as_deref()).await?;

        // The customer: the first incoming message's sender, else the
        // ticket's contact (§12).
        let first = messages
            .iter()
            .find(|m| m.kind == "customer" && m.email.is_some());
        let mut customer = Person {
            email: first.and_then(|m| m.email.clone()),
            name: first.and_then(|m| m.name.clone()),
        };
        if customer.email.is_none()
            && let Some(contact) = id_of(&t["associations"]["contacts"]["results"][0]["id"])
            && let Some(c) = self
                .get(&format!(
                    "/crm/objects/2026-09/contacts/{contact}?properties=email,firstname,lastname"
                ))
                .await?
        {
            let c = &c["properties"];
            let full = format!(
                "{} {}",
                c["firstname"].as_str().unwrap_or(""),
                c["lastname"].as_str().unwrap_or("")
            );
            customer = Person {
                email: c["email"].as_str().and_then(normalize_email),
                name: Some(full.trim().to_string()).filter(|n| !n.is_empty()),
            };
        }
        let Some(email) = customer.email.clone() else {
            // §16: an anonymous chat has no one to be the contact.
            self.delete(&id).await?;
            let mut tx = tenant_tx(&self.st.db, self.ws).await?;
            sqlx::query(
                "UPDATE hubspot_connections SET skipped = array_append(array_remove(skipped, $2), $2)
                 WHERE workspace_id = $1",
            )
            .bind(self.ws)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(());
        };

        let created = time_of(&p["createdate"]).unwrap_or_else(Utc::now);
        // A ticket created by hand has no thread: its description is the message.
        let content = p["content"].as_str().unwrap_or("").trim();
        if thread.is_none() && !content.is_empty() {
            messages.insert(
                0,
                Msg {
                    id: format!("hubspot:content:{}", self.key(&id)),
                    kind: "customer",
                    name: customer.name.clone(),
                    email: Some(email.clone()),
                    text: content.to_string(),
                    at: created,
                },
            );
        }

        let stage = p["hs_pipeline_stage"].as_str().unwrap_or("");
        let status = if self.conn.closed_stages.iter().any(|s| s == stage) {
            "closed"
        } else {
            match (pipeline.as_str(), stage) {
                (DEFAULT_PIPELINE, "1") => "new",
                (DEFAULT_PIPELINE, "2") => "waitingOnContact",
                _ => "waitingOnUs",
            }
        };
        let priority = p["hs_ticket_priority"]
            .as_str()
            .map(str::to_lowercase)
            .filter(|p| matches!(p.as_str(), "low" | "medium" | "high" | "urgent"));
        let owner_email = match id_of(&p["hubspot_owner_id"]) {
            Some(owner) => self
                .owner(&format!("id:{owner}"))
                .await?
                .and_then(|o| o.email),
            None => None,
        };
        let closed_at = (status == "closed").then(|| {
            time_of(&p["closed_date"])
                .or_else(|| time_of(&p["hs_lastmodifieddate"]))
                .unwrap_or(created)
        });
        let last_activity = messages.iter().map(|m| m.at).max().unwrap_or(created);
        let subject = p["subject"].as_str().unwrap_or("").trim();
        let subject = if subject.is_empty() {
            "(no subject)"
        } else {
            subject
        };

        let mut tx = tenant_tx(&self.st.db, self.ws).await?;
        // The connection as it is now, not as the job found it: a disconnect,
        // an unticked pipeline or the import ending may have happened since.
        let now: Option<(Uuid, String, SqlJson<Vec<HubspotPipeline>>)> = sqlx::query_as(
            "SELECT id, status, pipelines FROM hubspot_connections WHERE workspace_id = $1 FOR SHARE",
        )
        .bind(self.ws)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((conn_id, conn_status, pipelines)) = now else {
            return Ok(());
        };
        if conn_id != self.conn.id || !pipelines.iter().any(|p| p.selected && p.id == pipeline) {
            return Ok(());
        }
        let contact: Uuid = sqlx::query_scalar(
            "INSERT INTO contacts (workspace_id, email, name) VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, email) DO UPDATE SET email = EXCLUDED.email
             RETURNING id",
        )
        .bind(self.ws)
        .bind(&email)
        .bind(&customer.name)
        .fetch_one(&mut *tx)
        .await?;
        let (ticket, inserted): (Uuid, bool) = sqlx::query_as(
            "INSERT INTO tickets (workspace_id, token, subject, contact_id, status, priority, owner_id,
                                  created_at, last_activity_at, closed_at, suggest_status,
                                  hubspot_id, hubspot_url, hubspot_pipeline, hubspot_thread, hubspot_owner_email)
             VALUES ($1, $2, $3, $4, $5, $6,
                     (SELECT id FROM agents WHERE workspace_id = $1 AND email = $14 AND removed_at IS NULL),
                     $7, $8, $9, CASE WHEN $5 = 'closed' THEN 'ready' ELSE 'pending' END,
                     $10, $11, $12, $13, $14)
             ON CONFLICT (workspace_id, hubspot_id) DO UPDATE SET
                 subject = EXCLUDED.subject, contact_id = EXCLUDED.contact_id, status = EXCLUDED.status,
                 priority = EXCLUDED.priority, owner_id = EXCLUDED.owner_id,
                 created_at = EXCLUDED.created_at, last_activity_at = EXCLUDED.last_activity_at,
                 closed_at = EXCLUDED.closed_at, hubspot_url = EXCLUDED.hubspot_url,
                 hubspot_pipeline = EXCLUDED.hubspot_pipeline, hubspot_thread = EXCLUDED.hubspot_thread,
                 hubspot_owner_email = EXCLUDED.hubspot_owner_email
             RETURNING id, xmax = 0",
        )
        .bind(self.ws)
        .bind(hex::encode(rand::random::<[u8; 16]>()))
        .bind(subject)
        .bind(contact)
        .bind(status)
        .bind(&priority)
        .bind(created)
        .bind(last_activity)
        .bind(closed_at)
        .bind(self.key(&id))
        .bind(self.url(&id))
        .bind(&pipeline)
        .bind(thread.as_deref().map(|t| self.key(t)))
        .bind(&owner_email)
        .fetch_one(&mut *tx)
        .await?;
        write_messages(&mut tx, self.ws, ticket, &messages).await?;
        // ADR 0010: closed is in the brain with its whole thread, open is not.
        sqlx::query(
            "UPDATE tickets SET search = CASE WHEN status = 'closed' THEN brain_tsvector(workspace_id, id) END
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(self.ws)
        .bind(ticket)
        .execute(&mut *tx)
        .await?;
        // §11: an open arrival gets Jev once — now, or when the import is done.
        if inserted && status != "closed" && conn_status != "importing" {
            jobs::enqueue(&mut tx, self.ws, "suggest", Some(ticket)).await?;
            jobs::enqueue(&mut tx, self.ws, "categorize", Some(ticket)).await?;
        }
        tx.commit().await?;
        // Its own transaction: updating the row we hold FOR SHARE would
        // deadlock with every other re-read doing the same.
        let mut tx = tenant_tx(&self.st.db, self.ws).await?;
        sqlx::query(
            "UPDATE hubspot_connections SET skipped = array_remove(skipped, $2), last_synced_at = now()
             WHERE workspace_id = $1",
        )
        .bind(self.ws)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// §21: not in a ticked pipeline — checked again under the connection's
    /// lock, since the owner may have ticked it after this job began.
    async fn drop_unticked(&self, id: &str, pipeline: &str) -> Result<(), HsError> {
        let mut tx = tenant_tx(&self.st.db, self.ws).await?;
        let pipelines: Option<SqlJson<Vec<HubspotPipeline>>> = sqlx::query_scalar(
            "SELECT pipelines FROM hubspot_connections WHERE workspace_id = $1 AND id = $2 FOR SHARE",
        )
        .bind(self.ws)
        .bind(self.conn.id)
        .fetch_optional(&mut *tx)
        .await?;
        if pipelines.is_some_and(|p| !p.iter().any(|p| p.selected && p.id == pipeline)) {
            sqlx::query("DELETE FROM tickets WHERE workspace_id = $1 AND hubspot_id = $2")
                .bind(self.ws)
                .bind(self.key(id))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Tickets modified since `since`, newest first — in `pipelines`, or in
    /// any pipeline.
    async fn search(
        &self,
        pipelines: Option<&[String]>,
        since: DateTime<Utc>,
        before: Option<DateTime<Utc>>,
        after: Option<&str>,
    ) -> Result<Page, HsError> {
        let mut filters = vec![
            json!({ "propertyName": "hs_lastmodifieddate", "operator": "GTE", "value": ms(since) }),
        ];
        if let Some(pipelines) = pipelines {
            filters.insert(
                0,
                json!({ "propertyName": "hs_pipeline", "operator": "IN", "values": pipelines }),
            );
        }
        if let Some(before) = before {
            filters.push(
                json!({ "propertyName": "hs_lastmodifieddate", "operator": "LTE", "value": ms(before) }),
            );
        }
        let mut body = json!({
            "filterGroups": [{ "filters": filters }],
            "sorts": [{ "propertyName": "hs_lastmodifieddate", "direction": "DESCENDING" }],
            "properties": ["hs_lastmodifieddate", "hs_pipeline"],
            "limit": 100,
        });
        if let Some(after) = after {
            body["after"] = json!(after);
        }
        let v = self
            .request(
                Method::POST,
                "/crm/objects/2026-09/tickets/search",
                Some(&body),
            )
            .await?
            .unwrap_or(Value::Null);
        Ok(Page {
            total: v["total"].as_i64().unwrap_or(0),
            tickets: v["results"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| {
                    Some(Found {
                        id: id_of(&t["id"])?,
                        modified: time_of(&t["properties"]["hs_lastmodifieddate"]).unwrap_or(since),
                        pipeline: t["properties"]["hs_pipeline"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect(),
            after: v["paging"]["next"]["after"].as_str().map(str::to_string),
        })
    }
}

/// Mirror `messages` onto the ticket: new ones added, changed ones updated
/// (or moved here from a merged-away ticket), the rest removed (§20).
async fn write_messages(
    tx: &mut Tx,
    ws: Uuid,
    ticket: Uuid,
    messages: &[Msg],
) -> Result<(), sqlx::Error> {
    let ids: Vec<&str> = messages.iter().map(|m| m.id.as_str()).collect();
    let kinds: Vec<&str> = messages.iter().map(|m| m.kind).collect();
    let names: Vec<Option<&str>> = messages.iter().map(|m| m.name.as_deref()).collect();
    let emails: Vec<Option<&str>> = messages.iter().map(|m| m.email.as_deref()).collect();
    let texts: Vec<&str> = messages.iter().map(|m| m.text.as_str()).collect();
    let dates: Vec<DateTime<Utc>> = messages.iter().map(|m| m.at).collect();
    // Replies were sent from HubSpot and are never sent again (§15). No
    // sent_at: that is what the trial send cap counts, and these are not ours.
    sqlx::query(
        "INSERT INTO messages (workspace_id, ticket_id, kind, agent_id, from_name, from_email, text,
                               hubspot_key, created_at, delivery_status)
         SELECT $1, $2, x.kind,
                (SELECT a.id FROM agents a WHERE a.workspace_id = $1 AND a.email = x.email
                   AND a.removed_at IS NULL AND x.kind <> 'customer'),
                x.name, x.email, x.text, x.id, x.at, CASE WHEN x.kind = 'agent' THEN 'sent' END
         FROM unnest($3::text[], $4::text[], $5::text[], $6::text[], $7::text[], $8::timestamptz[])
              AS x(kind, name, email, text, id, at)
         ON CONFLICT (workspace_id, hubspot_key) DO UPDATE SET
             ticket_id = EXCLUDED.ticket_id, kind = EXCLUDED.kind, agent_id = EXCLUDED.agent_id,
             from_name = EXCLUDED.from_name, from_email = EXCLUDED.from_email, text = EXCLUDED.text,
             created_at = EXCLUDED.created_at, delivery_status = EXCLUDED.delivery_status",
    )
    .bind(ws)
    .bind(ticket)
    .bind(&kinds)
    .bind(&names)
    .bind(&emails)
    .bind(&texts)
    .bind(&ids)
    .bind(&dates)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "DELETE FROM messages WHERE workspace_id = $1 AND ticket_id = $2 AND hubspot_key <> ALL($3)",
    )
    .bind(ws)
    .bind(ticket)
    .bind(&ids)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The pipelines as HubSpot has them now, keeping the owner's ticks; a new
/// connection ticks the default pipeline (§6). Also the closed stages (§12).
fn read_pipelines(
    v: &Value,
    old: &[HubspotPipeline],
    new: bool,
) -> (Vec<HubspotPipeline>, Vec<String>) {
    let mut pipelines = vec![];
    let mut closed = vec![];
    for p in v["results"].as_array().into_iter().flatten() {
        let Some(id) = id_of(&p["id"]) else { continue };
        let selected = match old.iter().find(|o| o.id == id) {
            Some(o) => o.selected,
            None => new && id == DEFAULT_PIPELINE,
        };
        for s in p["stages"].as_array().into_iter().flatten() {
            if s["metadata"]["ticketState"] == "CLOSED"
                && let Some(stage) = id_of(&s["id"])
            {
                closed.push(stage);
            }
        }
        pipelines.push(HubspotPipeline {
            label: p["label"].as_str().unwrap_or(&id).to_string(),
            id,
            selected,
        });
    }
    (pipelines, closed)
}

/// A job's result. Revoked is not a failure: the status says so (§29).
/// Anything else is also shown in Settings (§28).
async fn job_outcome(st: &AppState, ws: Uuid, r: Result<(), HsError>) -> JobResult {
    let e = match r {
        Err(HsError::Revoked) => return Ok(()),
        // It worked: whatever failed before is over (Contract: lastError).
        Ok(()) => {
            let mut tx = tenant_tx(&st.db, ws).await?;
            sqlx::query(
                "UPDATE hubspot_connections SET last_error = NULL
                 WHERE workspace_id = $1 AND status <> 'revoked' AND last_error IS NOT NULL",
            )
            .bind(ws)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(());
        }
        Err(e) => e,
    };
    let shown = match &e {
        HsError::Temporary(..) => Some("HubSpot is not answering"),
        HsError::Rejected(_) | HsError::Unauthorized => Some("HubSpot refused a request"),
        _ => None,
    };
    if let Some(shown) = shown
        && let Ok(mut tx) = tenant_tx(&st.db, ws).await
    {
        let _ =
            sqlx::query("UPDATE hubspot_connections SET last_error = $2 WHERE workspace_id = $1")
                .bind(ws)
                .bind(shown)
                .execute(&mut *tx)
                .await;
        let _ = tx.commit().await;
    }
    Err(e.into())
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// A webhook's re-read of one ticket (§18).
pub async fn ticket_job(st: &AppState, ws: Uuid, id: &str) -> JobResult {
    let Some(mut hs) = Hs::open(st, ws).await? else {
        return Ok(());
    };
    if !matches!(hs.conn.status.as_str(), "importing" | "synced") {
        return Ok(());
    }
    let r = hs.sync_ticket(id).await;
    job_outcome(st, ws, r).await
}

/// A thread changed: re-read its ticket. A deleted thread is found through
/// the ticket that had it.
pub async fn thread_job(st: &AppState, ws: Uuid, thread: &str) -> JobResult {
    let Some(mut hs) = Hs::open(st, ws).await? else {
        return Ok(());
    };
    if !matches!(hs.conn.status.as_str(), "importing" | "synced") {
        return Ok(());
    }
    let r = async {
        let found = hs
            .get(&format!(
                "/conversations/conversations/2026-09/threads/{thread}?association=TICKET"
            ))
            .await?;
        let ticket = match found {
            Some(t) => id_of(&t["threadAssociations"]["associatedTicketId"]),
            None => {
                let mut tx = tenant_tx(&st.db, ws).await?;
                let key: Option<String> = sqlx::query_scalar(
                    "SELECT hubspot_id FROM tickets WHERE workspace_id = $1 AND hubspot_thread = $2",
                )
                .bind(ws)
                .bind(hs.key(thread))
                .fetch_optional(&mut *tx)
                .await?;
                tx.commit().await?;
                key.and_then(|k| k.split_once('/').map(|(_, id)| id.to_string()))
            }
        };
        match ticket {
            Some(id) => hs.sync_ticket(&id).await,
            None => Ok(()),
        }
    }
    .await;
    job_outcome(st, ws, r).await
}

/// Re-read a page's tickets. A ticket HubSpot refuses to give us is logged
/// and skipped, so one bad ticket cannot hold up the rest (§10); anything
/// worth retrying stops here. Each one counts towards `import`'s progress.
async fn sync_page(
    hs: &mut Hs<'_>,
    page: &Page,
    skip: Option<&str>,
    import: Option<Uuid>,
) -> Result<(), HsError> {
    let selected = hs.conn.selected();
    for t in &page.tickets {
        // The previous page's last ticket, which this page starts with again.
        if skip == Some(t.id.as_str()) {
            continue;
        }
        let r = if selected.contains(&t.pipeline) {
            hs.sync_ticket(&t.id).await
        } else {
            // Moved out of the ticked pipelines: no call to HubSpot needed.
            hs.drop_unticked(&t.id, &t.pipeline).await
        };
        match r {
            Ok(()) => {}
            Err(HsError::Rejected(msg)) => {
                tracing::warn!(workspace = %hs.ws, ticket = %t.id, "HubSpot ticket skipped: {msg}");
            }
            Err(e) => return Err(e),
        }
        if let Some(run) = import {
            let mut tx = tenant_tx(&hs.st.db, hs.ws).await?;
            sqlx::query(
                "UPDATE hubspot_connections SET import_done = import_done + 1
                 WHERE workspace_id = $1 AND import_run = $2",
            )
            .bind(hs.ws)
            .bind(run)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        }
    }
    Ok(())
}

/// §7–11: one slice of the import, then the next slice as a new job — so
/// it never outlives a job lease. A run replaced by a newer one stops; one
/// that failed is started again by the hourly check (§19).
pub async fn import_job(st: &AppState, ws: Uuid, conn_id: Uuid, run: &str) -> JobResult {
    let Some(mut hs) = Hs::open(st, ws).await? else {
        return Ok(());
    };
    let current = hs.conn.import_run.map(|r| r.to_string());
    if hs.conn.id != conn_id || current.as_deref() != Some(run) || hs.conn.status != "importing" {
        return Ok(());
    }
    let run_id: Uuid = run
        .parse()
        .map_err(|_| JobError::Fail("bad import run".into()))?;
    let started = Instant::now();
    let r = async {
        let pipelines = hs.conn.import_pipelines.clone();
        let since = hs
            .conn
            .import_since
            .unwrap_or_else(|| Utc::now() - chrono::Duration::days(IMPORT_DAYS));
        let (mut before, mut after) = (hs.conn.import_before, hs.conn.import_after.clone());
        let mut skip: Option<String> = None;
        loop {
            let page = hs
                .search(Some(&pipelines), since, before, after.as_deref())
                .await?;
            let mut tx = tenant_tx(&st.db, ws).await?;
            sqlx::query(
                "UPDATE hubspot_connections SET import_total = $3
                 WHERE workspace_id = $1 AND import_run = $2 AND import_total = 0",
            )
            .bind(ws)
            .bind(run_id)
            .bind(page.total as i32)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            sync_page(&mut hs, &page, skip.as_deref(), Some(run_id)).await?;
            let Some((b, a)) = next_page(&page, before) else {
                return finish_import(st, ws, run_id).await;
            };
            (before, after) = (b, a);
            skip = page.tickets.last().map(|t| t.id.clone());
            let mut tx = tenant_tx(&st.db, ws).await?;
            let still = sqlx::query(
                "UPDATE hubspot_connections SET import_before = $3, import_after = $4
                 WHERE workspace_id = $1 AND import_run = $2",
            )
            .bind(ws)
            .bind(run_id)
            .bind(before)
            .bind(&after)
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1;
            if still && started.elapsed() > IMPORT_SLICE {
                jobs::enqueue_object(&mut tx, ws, "hubspotImport", Some(conn_id), run, 0.0).await?;
            }
            tx.commit().await?;
            if !still || started.elapsed() > IMPORT_SLICE {
                return Ok(());
            }
        }
    }
    .await;
    job_outcome(st, ws, r).await
}

/// The import is done: synced, and the open tickets it brought get Jev.
async fn finish_import(st: &AppState, ws: Uuid, run: Uuid) -> Result<(), HsError> {
    let mut tx = tenant_tx(&st.db, ws).await?;
    let done = sqlx::query(
        "UPDATE hubspot_connections SET status = 'synced', import_run = NULL, import_pipelines = '{}',
                import_before = NULL, import_after = NULL
         WHERE workspace_id = $1 AND import_run = $2 AND status = 'importing'",
    )
    .bind(ws)
    .bind(run)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if done {
        owe_jev(&mut tx, ws).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// §11: the open tickets that arrived while an import ran get Jev now that
/// the brain is whole — however the import ended.
async fn owe_jev(tx: &mut Tx, ws: Uuid) -> Result<(), sqlx::Error> {
    let owed: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM tickets WHERE workspace_id = $1 AND hubspot_id IS NOT NULL AND suggest_status = 'pending'",
    )
    .bind(ws)
    .fetch_all(&mut **tx)
    .await?;
    for id in owed {
        jobs::enqueue(tx, ws, "suggest", Some(id)).await?;
        jobs::enqueue(tx, ws, "categorize", Some(id)).await?;
    }
    Ok(())
}

/// §19: hourly, re-read every ticket modified since an hour before the last
/// check, in any pipeline, so one moved out of a ticked pipeline leaves too.
/// More than `CHECK_INLINE` of them are caught up by an import instead. An
/// import that stopped on errors is started again. The next check is queued
/// first, so a failing one never ends the chain.
pub async fn check_job(st: &AppState, ws: Uuid, conn_id: Uuid) -> JobResult {
    let mut tx = tenant_tx(&st.db, ws).await?;
    let current: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM hubspot_connections WHERE workspace_id = $1")
            .bind(ws)
            .fetch_optional(&mut *tx)
            .await?;
    if current != Some(conn_id) {
        return Ok(()); // disconnected: the chain ends
    }
    jobs::enqueue_object(
        &mut tx,
        ws,
        "hubspotCheck",
        Some(conn_id),
        &conn_id.to_string(),
        3600.0,
    )
    .await?;
    tx.commit().await?;

    let Some(mut hs) = Hs::open(st, ws).await? else {
        return Ok(());
    };
    match (hs.conn.status.as_str(), hs.conn.import_run) {
        ("importing", Some(run)) => {
            let run = run.to_string();
            let mut tx = tenant_tx(&st.db, ws).await?;
            let working: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM jobs WHERE workspace_id = $1 AND kind = 'hubspotImport'
                                AND object_ref = $2 AND failed_at IS NULL)",
            )
            .bind(ws)
            .bind(&run)
            .fetch_one(&mut *tx)
            .await?;
            if !working {
                jobs::enqueue_object(&mut tx, ws, "hubspotImport", Some(conn_id), &run, 0.0)
                    .await?;
            }
            tx.commit().await?;
            return Ok(());
        }
        ("synced", _) => {}
        _ => return Ok(()),
    }
    let started = Utc::now();
    let r = async {
        // The pipelines may have been renamed, added or changed stages. The
        // owner's ticks are merged under the lock, so a save meanwhile stands.
        if let Some(v) = hs.get("/crm/pipelines/2026-09/tickets").await? {
            let mut tx = tenant_tx(&st.db, ws).await?;
            let SqlJson(ticks): SqlJson<Vec<HubspotPipeline>> = sqlx::query_scalar(
                "SELECT pipelines FROM hubspot_connections WHERE workspace_id = $1 FOR UPDATE",
            )
            .bind(ws)
            .fetch_one(&mut *tx)
            .await?;
            let (pipelines, closed) = read_pipelines(&v, &ticks, false);
            sqlx::query(
                "UPDATE hubspot_connections SET pipelines = $2, closed_stages = $3 WHERE workspace_id = $1",
            )
            .bind(ws)
            .bind(SqlJson(&pipelines))
            .bind(&closed)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            hs.conn.pipelines = SqlJson(pipelines);
            hs.conn.closed_stages = closed;
        }
        let since = hs.conn.checked_at.unwrap_or(hs.conn.connected_at) - chrono::Duration::hours(1);
        let (mut before, mut after) = (None, None);
        let mut skip: Option<String> = None;
        loop {
            let page = hs.search(None, since, before, after.as_deref()).await?;
            if before.is_none() && after.is_none() && page.total > CHECK_INLINE {
                // A long outage or a revoked week: an import catches it up.
                let mut tx = tenant_tx(&st.db, ws).await?;
                let run = Uuid::new_v4();
                let started_import = sqlx::query(
                    "UPDATE hubspot_connections SET status = 'importing', import_run = $3,
                            import_pipelines = ARRAY(SELECT p->>'id' FROM jsonb_array_elements(pipelines) p
                                                     WHERE (p->>'selected')::boolean),
                            import_since = $4, import_before = NULL, import_after = NULL,
                            import_done = 0, import_total = 0, checked_at = $5
                     WHERE workspace_id = $1 AND id = $2 AND status = 'synced'",
                )
                .bind(ws)
                .bind(conn_id)
                .bind(run)
                .bind(since)
                .bind(started)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                    == 1;
                if started_import {
                    jobs::enqueue_object(
                        &mut tx,
                        ws,
                        "hubspotImport",
                        Some(conn_id),
                        &run.to_string(),
                        0.0,
                    )
                    .await?;
                }
                tx.commit().await?;
                return Ok(());
            }
            sync_page(&mut hs, &page, skip.as_deref(), None).await?;
            match next_page(&page, before) {
                Some((b, a)) => (before, after) = (b, a),
                None => break,
            }
            skip = page.tickets.last().map(|t| t.id.clone());
        }
        let mut tx = tenant_tx(&st.db, ws).await?;
        sqlx::query(
            "UPDATE hubspot_connections SET checked_at = $2 WHERE workspace_id = $1 AND id = $3",
        )
        .bind(ws)
        .bind(started)
        .bind(conn_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    job_outcome(st, ws, r).await
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

async fn connection(tx: &mut Tx, ws: Uuid) -> Result<Option<HubspotConnection>, sqlx::Error> {
    type Row = (
        i64,
        String,
        String,
        SqlJson<Vec<HubspotPipeline>>,
        i32,
        i32,
        i32,
        i32,
        Option<DateTime<Utc>>,
        Option<String>,
        DateTime<Utc>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT portal_id, account_name, status, pipelines, import_done, import_total,
                (SELECT count(*) FROM tickets t WHERE t.workspace_id = c.workspace_id AND t.hubspot_id IS NOT NULL)::int,
                cardinality(skipped), last_synced_at, last_error, connected_at
         FROM hubspot_connections c WHERE c.workspace_id = $1",
    )
    .bind(ws)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(
        |(
            account_id,
            account_name,
            status,
            pipelines,
            done,
            total,
            tickets,
            skipped,
            last_synced_at,
            last_error,
            connected_at,
        )| {
            HubspotConnection {
                account_id,
                account_name,
                import: (status == "importing").then_some(ImportProgress { done, total }),
                status,
                pipelines: pipelines.0,
                tickets,
                skipped,
                last_synced_at,
                last_error,
                connected_at,
            }
        },
    ))
}

/// `GET /api/integrations/hubspot` — any agent.
async fn status(auth: Auth, State(st): State<AppState>) -> ApiResult<Json<HubspotStatus>> {
    available(&st)?;
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    let connection = connection(&mut tx, auth.workspace_id).await?;
    tx.commit().await?;
    Ok(Json(HubspotStatus { connection }))
}

/// `POST /api/integrations/hubspot/connect` — §2–3: HubSpot's consent page,
/// with a `state` for this agent, 10 minutes, once. A revoked connection
/// may reconnect (§29).
async fn connect(auth: Auth, State(st): State<AppState>) -> ApiResult<Json<UrlResponse>> {
    let (client_id, _) = available(&st)?;
    auth.require_owner()?;
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM hubspot_connections WHERE workspace_id = $1")
            .bind(ws)
            .fetch_optional(&mut *tx)
            .await?;
    if status.is_some_and(|s| s != "revoked") {
        return Err(ApiError::conflict("alreadyConnected"));
    }
    let state = new_token();
    sqlx::query("DELETE FROM hubspot_states WHERE workspace_id = $1 AND expires_at < now()")
        .bind(ws)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO hubspot_states (workspace_id, token_hash, agent_id, expires_at)
         VALUES ($1, $2, $3, now() + interval '10 minutes')",
    )
    .bind(ws)
    .bind(hash_token(&state))
    .bind(auth.agent_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let url = format!(
        "{}/oauth/authorize?client_id={}&redirect_uri={}&scope={}&state={}",
        st.cfg.hubspot_app_url,
        percent_encode(client_id),
        percent_encode(&redirect_uri(&st)),
        percent_encode(SCOPES),
        percent_encode(&state),
    );
    Ok(Json(UrlResponse { url }))
}

/// `GET /api/integrations/hubspot/callback` — HubSpot sends the browser back
/// here, so every answer is a redirect to Settings (§3–4).
async fn callback(
    auth: Result<Auth, ApiError>,
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if app(&st).is_none() {
        return ApiError::not_found().into_response();
    }
    let Ok(auth) = auth else {
        return Redirect::to("/login").into_response();
    };
    let to = match finish_connect(&st, &auth, &q).await {
        Ok(()) => "/settings/hubspot".to_string(),
        Err(code) => format!("/settings/hubspot?error={code}"),
    };
    Redirect::to(&to).into_response()
}

async fn finish_connect(
    st: &AppState,
    auth: &Auth,
    q: &HashMap<String, String>,
) -> Result<(), &'static str> {
    let (client_id, secret) = app(st).ok_or("upstream")?;
    let ws = auth.workspace_id;
    let db = |e: sqlx::Error| {
        tracing::error!("database: {e}");
        "upstream"
    };

    // The state is used up whatever HubSpot answered.
    let state = q.get("state").map(String::as_str).unwrap_or("");
    let mut tx = tenant_tx(&st.db, ws).await.map_err(db)?;
    let valid = sqlx::query(
        "DELETE FROM hubspot_states
         WHERE workspace_id = $1 AND token_hash = $2 AND agent_id = $3 AND expires_at > now()",
    )
    .bind(ws)
    .bind(hash_token(state))
    .bind(auth.agent_id)
    .execute(&mut *tx)
    .await
    .map_err(db)?
    .rows_affected()
        == 1;
    tx.commit().await.map_err(db)?;
    if !valid {
        return Err("expired");
    }
    let Some(code) = q.get("code").filter(|_| !q.contains_key("error")) else {
        return Err("denied");
    };

    let hs_error = |e: HsError| {
        tracing::error!("hubspot connect: {e:?}");
        "upstream"
    };
    let redirect = redirect_uri(st);
    let tokens = token_request(
        st,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("client_secret", secret),
            ("redirect_uri", redirect.as_str()),
            ("code", code.as_str()),
        ],
    )
    .await
    .map_err(hs_error)?;
    // Which account the user picked (assumed field names, spec 007).
    let info: Value = st
        .http
        .post(format!(
            "{}/oauth/2026-03/token/introspect",
            st.cfg.hubspot_api_url
        ))
        .form(&[
            ("token", tokens.access.as_str()),
            ("client_id", client_id),
            ("client_secret", secret),
        ])
        .send()
        .await
        .map_err(|_| "upstream")?
        .json()
        .await
        .map_err(|_| "upstream")?;
    let portal = info["hub_id"].as_i64().ok_or("upstream")?;
    let account_name = info["hub_domain"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| portal.to_string());
    let pipelines = call(
        st,
        ws,
        &tokens.access,
        Method::GET,
        "/crm/pipelines/2026-09/tickets",
        None,
    )
    .await
    .map_err(hs_error)?
    .unwrap_or(Value::Null);

    let mut tx = tenant_tx(&st.db, ws).await.map_err(db)?;
    let existing: Option<(i64, String, Option<Uuid>, Uuid)> = sqlx::query_as(
        "SELECT portal_id, status, import_run, id FROM hubspot_connections WHERE workspace_id = $1 FOR UPDATE",
    )
    .bind(ws)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?;
    match existing {
        // §29: the same account, reconnected — carry on where it stopped.
        Some((p, status, run, conn_id)) if p == portal && status == "revoked" => {
            sqlx::query(
                "UPDATE hubspot_connections SET refresh_token = $2, last_error = NULL,
                        status = CASE WHEN import_run IS NOT NULL THEN 'importing'
                                      WHEN checked_at IS NOT NULL THEN 'synced'
                                      ELSE 'pickPipelines' END
                 WHERE workspace_id = $1",
            )
            .bind(ws)
            .bind(&tokens.refresh)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
            if let Some(run) = run {
                jobs::enqueue_object(
                    &mut tx,
                    ws,
                    "hubspotImport",
                    Some(conn_id),
                    &run.to_string(),
                    0.0,
                )
                .await
                .map_err(db)?;
            }
        }
        Some(_) => return Err("alreadyConnected"),
        None => {
            let (pipelines, closed) = read_pipelines(&pipelines, &[], true);
            let inserted = sqlx::query(
                "INSERT INTO hubspot_connections (workspace_id, portal_id, account_name, refresh_token,
                                                  pipelines, closed_stages)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(ws)
            .bind(portal)
            .bind(&account_name)
            .bind(&tokens.refresh)
            .bind(SqlJson(&pipelines))
            .bind(&closed)
            .execute(&mut *tx)
            .await;
            match inserted {
                Ok(_) => {}
                // §4: the tokens are dropped, not revoked — revoking could
                // break the other workspace's install.
                Err(e) if unique_violation(&e) == Some("hubspot_connections_portal") => {
                    return Err("portalTaken");
                }
                Err(e) if unique_violation(&e).is_some() => return Err("alreadyConnected"),
                Err(e) => return Err(db(e)),
            }
        }
    }
    tx.commit().await.map_err(db)?;
    remember(ws, &tokens.access, tokens.expires_in);
    Ok(())
}

/// `PUT /api/integrations/hubspot/pipelines` — §6, §22. The first save starts
/// the import and the hourly check; a new tick imports its pipeline; an
/// untick deletes its tickets.
async fn set_pipelines(
    auth: Auth,
    State(st): State<AppState>,
    Body(req): Body<SetPipelines>,
) -> ApiResult<Json<HubspotConnection>> {
    available(&st)?;
    auth.require_owner()?;
    if req.pipeline_ids.is_empty() {
        return Err(ApiError::bad_request("noPipelines"));
    }
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let (conn_id, portal, status, pipelines, import_pipelines): (
        Uuid,
        i64,
        String,
        SqlJson<Vec<HubspotPipeline>>,
        Vec<String>,
    ) = sqlx::query_as(
        "SELECT id, portal_id, status, pipelines, import_pipelines FROM hubspot_connections
             WHERE workspace_id = $1 FOR UPDATE",
    )
    .bind(ws)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(ApiError::not_found)?;
    let wanted: HashSet<&str> = req.pipeline_ids.iter().map(String::as_str).collect();
    if wanted
        .iter()
        .any(|id| !pipelines.iter().any(|p| p.id == *id))
    {
        return Err(ApiError::bad_request("unknownPipeline"));
    }
    // Before the first save a tick is only a default: everything ticked is new.
    let first = status == "pickPipelines";
    let added: Vec<String> = pipelines
        .iter()
        .filter(|p| (first || !p.selected) && wanted.contains(p.id.as_str()))
        .map(|p| p.id.clone())
        .collect();
    let removed: Vec<String> = pipelines
        .iter()
        .filter(|p| p.selected && !wanted.contains(p.id.as_str()))
        .map(|p| p.id.clone())
        .collect();
    let pipelines: Vec<HubspotPipeline> = pipelines
        .0
        .into_iter()
        .map(|p| HubspotPipeline {
            selected: wanted.contains(p.id.as_str()),
            ..p
        })
        .collect();

    // §22: unticking says the pipeline's tickets are not support. This
    // account's only: every account has a pipeline "0", and tickets kept from
    // an earlier connection (§30) are not this one's to delete.
    sqlx::query(
        "DELETE FROM tickets WHERE workspace_id = $1 AND hubspot_pipeline = ANY($2)
                                AND hubspot_id LIKE $3 || '/%'",
    )
    .bind(ws)
    .bind(&removed)
    .bind(portal.to_string())
    .execute(&mut *tx)
    .await?;
    let mut importing: Vec<String> = import_pipelines
        .into_iter()
        .filter(|p| !removed.contains(p))
        .collect();
    if added.is_empty() {
        // Nothing new to import; an import left without pipelines is done,
        // and owes Jev as one that finished would (§11).
        let ended = status == "importing" && importing.is_empty();
        sqlx::query(
            "UPDATE hubspot_connections SET pipelines = $2, import_pipelines = $3,
                    status = CASE WHEN $4 THEN 'synced' ELSE status END,
                    import_run = CASE WHEN $4 THEN NULL ELSE import_run END
             WHERE workspace_id = $1",
        )
        .bind(ws)
        .bind(SqlJson(&pipelines))
        .bind(&importing)
        .bind(ended)
        .execute(&mut *tx)
        .await?;
        if ended {
            owe_jev(&mut tx, ws).await?;
        }
    } else {
        // A new run: an import already under way restarts with the new
        // pipelines too, and what it already brought in is re-read, not doubled.
        importing.extend(added);
        let run = Uuid::new_v4();
        sqlx::query(
            "UPDATE hubspot_connections SET pipelines = $2, status = 'importing', import_run = $3,
                    import_pipelines = $4, import_since = now() - $5 * interval '1 day',
                    import_before = NULL, import_after = NULL, import_done = 0, import_total = 0,
                    checked_at = coalesce(checked_at, now())
             WHERE workspace_id = $1",
        )
        .bind(ws)
        .bind(SqlJson(&pipelines))
        .bind(run)
        .bind(&importing)
        .bind(IMPORT_DAYS as f64)
        .execute(&mut *tx)
        .await?;
        jobs::enqueue_object(
            &mut tx,
            ws,
            "hubspotImport",
            Some(conn_id),
            &run.to_string(),
            0.0,
        )
        .await?;
        if status == "pickPipelines" {
            jobs::enqueue_object(
                &mut tx,
                ws,
                "hubspotCheck",
                Some(conn_id),
                &conn_id.to_string(),
                3600.0,
            )
            .await?;
        }
    }
    let connection = connection(&mut tx, ws)
        .await?
        .ok_or_else(ApiError::not_found)?;
    tx.commit().await?;
    Ok(Json(connection))
}

/// `DELETE /api/integrations/hubspot` — §30: the app is uninstalled from
/// HubSpot if HubSpot lets us; closed tickets stay in the brain, open ones go.
async fn disconnect(auth: Auth, State(st): State<AppState>) -> ApiResult<StatusCode> {
    available(&st)?;
    auth.require_owner()?;
    let ws = auth.workspace_id;
    let hs = Hs::open(&st, ws).await?.ok_or_else(ApiError::not_found)?;
    if hs.conn.status != "revoked"
        && let Err(e) = hs
            .request(Method::DELETE, "/appinstalls/v3/external-install", None)
            .await
    {
        tracing::warn!(workspace = %ws, "HubSpot uninstall failed: {e:?}");
    }
    let mut tx = tenant_tx(&st.db, ws).await?;
    // The connection first: that waits for any re-read holding it, so the
    // open tickets deleted next include whatever those just wrote.
    sqlx::query("DELETE FROM hubspot_connections WHERE workspace_id = $1")
        .bind(ws)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM tickets WHERE workspace_id = $1 AND hubspot_id LIKE $2 || '/%' AND status <> 'closed'",
    )
    .bind(ws)
    .bind(hs.conn.portal_id.to_string())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    forget(ws);
    Ok(StatusCode::NO_CONTENT)
}

/// `X-HubSpot-Signature-v3`: base64 HMAC-SHA256, keyed with the client
/// secret, over method, URI, body and timestamp; the timestamp within five
/// minutes of now (in milliseconds).
fn signature_ok(
    secret: &str,
    uri: &str,
    body: &[u8],
    timestamp: &str,
    signature: &str,
    now_ms: i64,
) -> bool {
    if timestamp
        .parse::<i64>()
        .map_or(true, |t| (now_ms - t).abs() > 300_000)
    {
        return false;
    }
    let Ok(sig) = STANDARD.decode(signature) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(b"POST");
    mac.update(uri.as_bytes());
    mac.update(body);
    mac.update(timestamp.as_bytes());
    mac.verify_slice(&sig).is_ok() // constant time
}

/// `POST /hooks/hubspot` — §18. One of ADR 0003's cross-tenant routes: the
/// workspace comes from `portalId`, then each event queues one re-read in the
/// tenant's own transaction. HubSpot retries any 4xx too, so an account no
/// workspace has is ignored rather than refused.
async fn webhook(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<StatusCode> {
    let (_, secret) = available(&st)?;
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    };
    let uri = format!("{}/hooks/hubspot", st.cfg.app_url);
    if !signature_ok(
        secret,
        &uri,
        &body,
        header("x-hubspot-request-timestamp"),
        header("x-hubspot-signature-v3"),
        Utc::now().timestamp_millis(),
    ) {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "invalidSignature"));
    }
    let events: Vec<Value> = serde_json::from_slice(&body).map_err(|e| {
        tracing::error!("hubspot webhook: not the expected JSON: {e}");
        ApiError::internal()
    })?;

    let mut by_portal: HashMap<i64, Vec<(&'static str, String)>> = HashMap::new();
    for e in &events {
        let Some(portal) = e["portalId"].as_i64() else {
            continue;
        };
        let kind = e["subscriptionType"].as_str().unwrap_or("");
        let refs: Vec<(&'static str, Option<String>)> = match kind {
            "ticket.merge" => std::iter::once(&e["primaryObjectId"])
                .chain(e["mergedObjectIds"].as_array().into_iter().flatten())
                .map(|id| ("hubspotTicket", id_of(id)))
                .collect(),
            "ticket.associationChange" => vec![("hubspotTicket", id_of(&e["fromObjectId"]))],
            k if k.starts_with("ticket.") => vec![("hubspotTicket", id_of(&e["objectId"]))],
            k if k.starts_with("conversation.") => vec![("hubspotThread", id_of(&e["objectId"]))],
            _ => vec![],
        };
        by_portal
            .entry(portal)
            .or_default()
            .extend(refs.into_iter().filter_map(|(k, id)| Some((k, id?))));
    }
    for (portal, refs) in by_portal {
        let ws: Option<Uuid> = sqlx::query_scalar("SELECT workspace_id_by_hubspot_portal($1)")
            .bind(portal)
            .fetch_one(&st.db)
            .await?;
        let Some(ws) = ws else { continue };
        let mut tx = tenant_tx(&st.db, ws).await?;
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM hubspot_connections WHERE workspace_id = $1")
                .bind(ws)
                .fetch_optional(&mut *tx)
                .await?;
        if status.is_some_and(|s| s == "importing" || s == "synced") {
            for (kind, id) in refs {
                jobs::enqueue_object(&mut tx, ws, kind, None, &id, 0.0).await?;
            }
        }
        tx.commit().await?;
    }
    Ok(StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        let now = 1_759_000_000_000_i64;
        let sign = |ts: &str, body: &[u8]| {
            let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
            mac.update(b"POSThttps://muninn.io/hooks/hubspot");
            mac.update(body);
            mac.update(ts.as_bytes());
            STANDARD.encode(mac.finalize().into_bytes())
        };
        let uri = "https://muninn.io/hooks/hubspot";
        let ts = now.to_string();
        let good = sign(&ts, b"[]");
        assert!(signature_ok("secret", uri, b"[]", &ts, &good, now));
        assert!(
            !signature_ok("secret", uri, b"[{}]", &ts, &good, now),
            "another body"
        );
        assert!(
            !signature_ok("other", uri, b"[]", &ts, &good, now),
            "another secret"
        );
        let old = (now - 301_000).to_string();
        assert!(
            !signature_ok("secret", uri, b"[]", &old, &sign(&old, b"[]"), now),
            "too old"
        );
        assert!(!signature_ok("secret", uri, b"[]", "", &good, now));
    }

    #[test]
    fn times_and_ids() {
        assert_eq!(
            time_of(&json!("2026-09-29T09:12:40.123Z")).map(|t| t.timestamp_millis()),
            Some(1_790_673_160_123)
        );
        assert_eq!(
            time_of(&json!("1790673160123")).map(|t| t.timestamp_millis()),
            Some(1_790_673_160_123)
        );
        assert_eq!(time_of(&json!(null)), None);
        assert_eq!(
            id_of(&json!(35512339183_i64)).as_deref(),
            Some("35512339183")
        );
        assert_eq!(id_of(&json!("0")).as_deref(), Some("0"));
        assert_eq!(id_of(&json!("")), None);
    }

    #[test]
    fn paging_goes_by_time_not_offset() {
        let t = |ms: i64| DateTime::from_timestamp_millis(ms).unwrap();
        let found = |id: &str, ms: i64| Found {
            id: id.into(),
            modified: t(ms),
            pipeline: "0".into(),
        };
        let page = |after: Option<&str>, last: i64| Page {
            total: 0,
            tickets: vec![found("1", 2_000), found("2", last)],
            after: after.map(str::to_string),
        };
        assert!(next_page(&page(None, 1_000), None).is_none());
        // A fresh query from the last ticket back, whatever the offset says.
        assert_eq!(
            next_page(&page(Some("100"), 1_000), None),
            Some((Some(t(1_000)), None))
        );
        assert_eq!(
            next_page(&page(Some("100"), 1_000), Some(t(5_000))),
            Some((Some(t(1_000)), None))
        );
        // A page that did not move time on pages on within that millisecond.
        assert_eq!(
            next_page(&page(Some("200"), 1_000), Some(t(1_000))),
            Some((Some(t(1_000)), Some("200".into())))
        );
    }

    #[test]
    fn pipelines_keep_their_ticks() {
        let v = json!({ "results": [
            { "id": "0", "label": "Support Pipeline", "stages": [
                { "id": "1", "metadata": { "ticketState": "OPEN" } },
                { "id": "4", "metadata": { "ticketState": "CLOSED" } }] },
            { "id": "7", "label": "Onboarding", "stages": [
                { "id": "70", "metadata": { "ticketState": "CLOSED" } }] }
        ]});
        let (fresh, closed) = read_pipelines(&v, &[], true);
        assert_eq!(closed, vec!["4", "70"]);
        assert_eq!(
            fresh.iter().map(|p| p.selected).collect::<Vec<_>>(),
            vec![true, false]
        );
        let mine = vec![HubspotPipeline {
            id: "0".into(),
            label: "Old".into(),
            selected: false,
        }];
        let (later, _) = read_pipelines(&v, &mine, false);
        assert_eq!(later[0].label, "Support Pipeline");
        assert!(
            !later[0].selected && !later[1].selected,
            "a new pipeline starts unticked"
        );
    }
}

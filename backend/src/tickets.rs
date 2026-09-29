//! Spec 002: views, the ticket, replies, comments, attachments.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::types::Json as SqlJson;
use uuid::Uuid;

use crate::api::{
    ContactTicket, Counts, Message, PatchTicket, TextRequest, Ticket, TicketDetail, TicketList,
};
use crate::db::{Tx, tenant_tx};
use crate::error::ApiResult;
use crate::session::{Auth, Body, is_paid};
use crate::{ApiError, AppState, jobs};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/tickets", get(list))
        .route("/tickets/{id}", get(detail).patch(patch))
        .route("/tickets/{id}/replies", post(reply))
        .route("/tickets/{id}/comments", post(comment))
        .route("/messages/{id}/retry", post(retry))
        .route("/attachments/{id}", get(attachment))
}

const PAGE: i64 = 50;

/// The Ticket (spec 002 Contract), built by Postgres in the contract's shape.
/// `$2` narrows to one ticket; `$3`–`$6` are the list's view and cursor.
const TICKETS: &str = r#"
SELECT json_build_object(
    'id', t.id,
    'subject', t.subject,
    'status', t.status,
    'priority', t.priority,
    'owner', CASE WHEN o.id IS NOT NULL THEN json_build_object('id', o.id, 'name', o.name) END,
    'contact', json_build_object('id', c.id, 'email', c.email, 'name', c.name),
    'category', CASE WHEN cat.id IS NOT NULL THEN json_build_object(
        'id', cat.id, 'name', cat.name, 'source', t.category_source,
        'probability', CASE WHEN t.category_source = 'jev' THEN t.jev_category_probability END) END,
    'categorySuggestions', CASE WHEN t.category_id IS NULL THEN t.category_suggestions ELSE '[]'::jsonb END,
    'seenBefore', EXISTS (SELECT 1 FROM suggestions s
                          WHERE s.workspace_id = t.workspace_id AND s.ticket_id = t.id AND s.rank IS NOT NULL),
    'lastMessage', (SELECT json_build_object(
                        'kind', m.kind,
                        'snippet', btrim(left(regexp_replace(m.text, '\s+', ' ', 'g'), 140)),
                        'at', m.created_at)
                    FROM messages m WHERE m.workspace_id = t.workspace_id AND m.ticket_id = t.id
                    ORDER BY m.created_at DESC, m.id DESC LIMIT 1),
    'createdAt', t.created_at,
    'lastActivityAt', t.last_activity_at)
FROM tickets t
JOIN contacts c ON c.id = t.contact_id
LEFT JOIN agents o ON o.id = t.owner_id
LEFT JOIN categories cat ON cat.id = t.category_id
WHERE t.workspace_id = $1
  AND ($2::uuid IS NULL OR t.id = $2)
  AND CASE $3::text
        WHEN 'unassigned' THEN t.status <> 'closed' AND t.owner_id IS NULL
        WHEN 'mine' THEN t.status <> 'closed' AND t.owner_id = $4
        WHEN 'open' THEN t.status <> 'closed'
        WHEN 'closed' THEN t.status = 'closed'
        ELSE true END
  AND ($5::timestamptz IS NULL OR (t.last_activity_at, t.id) < ($5, $6))
ORDER BY t.last_activity_at DESC, t.id DESC
LIMIT $7
"#;

/// The thread's messages (spec 002 Contract), oldest first. `$2` narrows to a
/// ticket, `$3` to one message.
const MESSAGES: &str = r#"
SELECT json_build_object(
    'id', m.id,
    'kind', m.kind,
    -- An imported team reply has no agent: its author is the mail's (spec 005 §9).
    'author', CASE WHEN m.kind = 'customer' OR m.agent_id IS NULL
        THEN json_build_object('name', coalesce(nullif(m.from_name, ''), m.from_email, ''), 'email', coalesce(m.from_email, ''))
        ELSE json_build_object('name', a.name, 'email', a.email) END,
    'text', m.text,
    'at', m.created_at,
    'delivery', CASE WHEN m.kind = 'agent'
        THEN json_build_object('status', m.delivery_status, 'error', m.delivery_error) END,
    'attachments', coalesce((SELECT json_agg(json_build_object(
                                'id', x.id, 'name', x.name, 'contentType', x.content_type, 'size', x.size)
                                ORDER BY x.name, x.id)
                             FROM attachments x WHERE x.workspace_id = m.workspace_id AND x.message_id = m.id),
                            '[]'::json))
FROM messages m
LEFT JOIN agents a ON a.id = m.agent_id
WHERE m.workspace_id = $1
  AND ($2::uuid IS NULL OR m.ticket_id = $2)
  AND ($3::uuid IS NULL OR m.id = $3)
ORDER BY m.created_at, m.id
"#;

async fn tickets(
    tx: &mut Tx,
    ws: Uuid,
    id: Option<Uuid>,
    view: &str,
    me: Uuid,
    after: Option<(DateTime<Utc>, Uuid)>,
    limit: i64,
) -> Result<Vec<Ticket>, sqlx::Error> {
    let rows: Vec<SqlJson<Ticket>> = sqlx::query_scalar(TICKETS)
        .bind(ws)
        .bind(id)
        .bind(view)
        .bind(me)
        .bind(after.map(|a| a.0))
        .bind(after.map(|a| a.1))
        .bind(limit)
        .fetch_all(&mut **tx)
        .await?;
    Ok(rows.into_iter().map(|SqlJson(t)| t).collect())
}

pub async fn ticket(tx: &mut Tx, ws: Uuid, id: Uuid, me: Uuid) -> ApiResult<Ticket> {
    tickets(tx, ws, Some(id), "any", me, None, 1)
        .await?
        .pop()
        .ok_or_else(ApiError::not_found)
}

async fn messages(
    tx: &mut Tx,
    ws: Uuid,
    ticket: Option<Uuid>,
    message: Option<Uuid>,
) -> Result<Vec<Message>, sqlx::Error> {
    let rows: Vec<SqlJson<Message>> = sqlx::query_scalar(MESSAGES)
        .bind(ws)
        .bind(ticket)
        .bind(message)
        .fetch_all(&mut **tx)
        .await?;
    Ok(rows.into_iter().map(|SqlJson(m)| m).collect())
}

async fn message(tx: &mut Tx, ws: Uuid, id: Uuid) -> ApiResult<Message> {
    messages(tx, ws, None, Some(id))
        .await?
        .pop()
        .ok_or_else(ApiError::not_found)
}

/// Opaque to the client: base64url of `<last_activity_at>|<id>`.
fn encode_cursor(t: &Ticket) -> String {
    let at = t
        .last_activity_at
        .to_rfc3339_opts(SecondsFormat::Micros, true);
    URL_SAFE_NO_PAD.encode(format!("{at}|{}", t.id))
}

fn decode_cursor(cursor: &str) -> Option<(DateTime<Utc>, Uuid)> {
    let raw = String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).ok()?).ok()?;
    let (at, id) = raw.split_once('|')?;
    Some((
        DateTime::parse_from_rfc3339(at).ok()?.to_utc(),
        id.parse().ok()?,
    ))
}

/// `GET /api/tickets?view=&cursor=` — spec 002 §10–13. `closed` resolves
/// spec 002's open question 1.
async fn list(
    auth: Auth,
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<TicketList>> {
    let view = q.get("view").map(String::as_str).unwrap_or("");
    if !matches!(view, "unassigned" | "mine" | "open" | "closed") {
        return Err(ApiError::bad_request("invalidView"));
    }
    let after = match q.get("cursor") {
        Some(c) => Some(decode_cursor(c).ok_or_else(|| ApiError::bad_request("invalidCursor"))?),
        None => None,
    };
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let mut page = tickets(&mut tx, ws, None, view, auth.agent_id, after, PAGE + 1).await?;
    let next_cursor = if page.len() as i64 > PAGE {
        page.truncate(PAGE as usize);
        page.last().map(encode_cursor)
    } else {
        None
    };
    let (unassigned, mine, open, has_tickets): (i32, i32, i32, bool) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status <> 'closed' AND owner_id IS NULL)::int,
                count(*) FILTER (WHERE status <> 'closed' AND owner_id = $2)::int,
                count(*) FILTER (WHERE status <> 'closed')::int,
                count(*) > 0
         FROM tickets WHERE workspace_id = $1",
    )
    .bind(ws)
    .bind(auth.agent_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(TicketList {
        tickets: page,
        next_cursor,
        counts: Counts {
            unassigned,
            mine,
            open,
        },
        has_tickets,
    }))
}

/// `GET /api/tickets/{id}` — spec 002 §14–15.
async fn detail(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<TicketDetail>> {
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let ticket = ticket(&mut tx, ws, id, auth.agent_id).await?;
    let messages = messages(&mut tx, ws, Some(id), None).await?;
    let contact_tickets: Vec<SqlJson<ContactTicket>> = sqlx::query_scalar(
        "SELECT json_build_object('id', id, 'subject', subject, 'status', status, 'createdAt', created_at)
         FROM tickets WHERE workspace_id = $1 AND contact_id = $2 AND id <> $3
         ORDER BY created_at DESC LIMIT 10",
    )
    .bind(ws)
    .bind(ticket.contact.id)
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(TicketDetail {
        ticket,
        messages,
        contact_tickets: contact_tickets.into_iter().map(|SqlJson(c)| c).collect(),
    }))
}

/// `PATCH /api/tickets/{id}` — spec 002 §21–22. Last write wins.
async fn patch(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Body(req): Body<PatchTicket>,
) -> ApiResult<Json<Ticket>> {
    let ws = auth.workspace_id;
    if let Some(s) = &req.status
        && !matches!(
            s.as_str(),
            "new" | "waitingOnContact" | "waitingOnUs" | "closed"
        )
    {
        return Err(ApiError::bad_request("invalidStatus"));
    }
    if let Some(Some(p)) = &req.priority
        && !matches!(p.as_str(), "low" | "medium" | "high" | "urgent")
    {
        return Err(ApiError::bad_request("invalidPriority"));
    }
    let mut tx = tenant_tx(&st.db, ws).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM tickets WHERE workspace_id = $1 AND id = $2)",
    )
    .bind(ws)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if !exists {
        return Err(ApiError::not_found());
    }
    if let Some(Some(owner)) = req.owner_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM agents WHERE workspace_id = $1 AND id = $2 AND removed_at IS NULL)",
        )
        .bind(ws)
        .bind(owner)
        .fetch_one(&mut *tx)
        .await?;
        if !ok {
            return Err(ApiError::bad_request("unknownAgent"));
        }
    }
    if let Some(Some(category)) = req.category_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM categories WHERE workspace_id = $1 AND id = $2 AND NOT archived)",
        )
        .bind(ws)
        .bind(category)
        .fetch_one(&mut *tx)
        .await?;
        if !ok {
            return Err(ApiError::bad_request("unknownCategory"));
        }
    }

    if let Some(owner) = req.owner_id {
        sqlx::query("UPDATE tickets SET owner_id = $3 WHERE workspace_id = $1 AND id = $2")
            .bind(ws)
            .bind(id)
            .bind(owner)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(priority) = req.priority {
        sqlx::query("UPDATE tickets SET priority = $3 WHERE workspace_id = $1 AND id = $2")
            .bind(ws)
            .bind(id)
            .bind(priority)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(category) = req.category_id {
        // Spec 004 §8: Jev's own pick stays in jev_category_id, so this is an override.
        sqlx::query(
            "UPDATE tickets SET category_id = $3::uuid,
                    category_source = CASE WHEN $3::uuid IS NULL THEN NULL ELSE 'agent' END
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(ws)
        .bind(id)
        .bind(category)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(status) = req.status {
        // ADR 0010: closing indexes the whole thread into the brain,
        // leaving closed takes it out. The old status is the row's own.
        sqlx::query(
            "UPDATE tickets t SET
                status = $3,
                closed_at = CASE WHEN $3 = 'closed' THEN coalesce(t.closed_at, now()) END,
                search = CASE
                    WHEN $3 <> 'closed' THEN NULL
                    WHEN t.status = 'closed' THEN t.search
                    ELSE brain_tsvector(t.workspace_id, t.id)
                END
             WHERE t.workspace_id = $1 AND t.id = $2",
        )
        .bind(ws)
        .bind(id)
        .bind(status)
        .execute(&mut *tx)
        .await?;
    }
    let ticket = ticket(&mut tx, ws, id, auth.agent_id).await?;
    tx.commit().await?;
    Ok(Json(ticket))
}

/// ADR 0009: a workspace without a subscription sends at most 100 mails a
/// rolling day; the next one is held. Locks the workspace row, so two
/// replies at 99 cannot both go out.
async fn delivery_for_send(
    tx: &mut Tx,
    ws: Uuid,
) -> Result<(&'static str, Option<&'static str>), sqlx::Error> {
    let billing: String =
        sqlx::query_scalar("SELECT billing_status FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(ws)
            .fetch_one(&mut **tx)
            .await?;
    if is_paid(&billing) {
        return Ok(("queued", None));
    }
    let sent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM messages
         WHERE workspace_id = $1 AND kind = 'agent'
           AND (delivery_status = 'queued'
                OR (delivery_status = 'sent' AND sent_at > now() - interval '24 hours'))",
    )
    .bind(ws)
    .fetch_one(&mut **tx)
    .await?;
    Ok(if sent >= 100 {
        (
            "held",
            Some("Held — trial limit of 100 emails a day. Retry later"),
        )
    } else {
        ("queued", None)
    })
}

fn nonempty(text: &str) -> ApiResult<&str> {
    let text = text.trim();
    if text.is_empty() {
        Err(ApiError::bad_request("emptyText"))
    } else {
        Ok(text)
    }
}

/// `POST /api/tickets/{id}/replies` — spec 002 §16–19. Replying to an
/// unassigned ticket makes it the replier's (spec 002 open question 2).
async fn reply(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Body(req): Body<TextRequest>,
) -> ApiResult<(StatusCode, Json<Message>)> {
    let text = nonempty(&req.text)?;
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let found = sqlx::query(
        "UPDATE tickets SET status = 'waitingOnContact', last_activity_at = now(),
                owner_id = coalesce(owner_id, $3)
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(id)
    .bind(auth.agent_id)
    .execute(&mut *tx)
    .await?;
    if found.rows_affected() == 0 {
        return Err(ApiError::not_found());
    }
    let (delivery, error) = delivery_for_send(&mut tx, ws).await?;
    let message_id: Uuid = sqlx::query_scalar(
        "INSERT INTO messages (workspace_id, ticket_id, kind, agent_id, text, message_id, delivery_status, delivery_error)
         VALUES ($1, $2, 'agent', $3, $4, gen_random_uuid() || '@' || $5, $6, $7) RETURNING id",
    )
    .bind(ws)
    .bind(id)
    .bind(auth.agent_id)
    .bind(text)
    .bind(&st.cfg.inbound_domain)
    .bind(delivery)
    .bind(error)
    .fetch_one(&mut *tx)
    .await?;
    if delivery == "queued" {
        jobs::enqueue(&mut tx, ws, "send", Some(message_id)).await?;
    }
    let message = message(&mut tx, ws, message_id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(message)))
}

/// `POST /api/messages/{id}/retry` — a failed or held reply, queued again
/// if the cap allows (spec 002 §18–19).
async fn retry(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Message>> {
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let (delivery, error) = delivery_for_send(&mut tx, ws).await?;
    let current: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT kind, delivery_status FROM messages WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(ws)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    match current {
        None => return Err(ApiError::not_found()),
        Some((kind, Some(status)))
            if kind == "agent" && matches!(status.as_str(), "failed" | "held") => {}
        Some(_) => return Err(ApiError::conflict("notRetryable")),
    }
    sqlx::query("UPDATE messages SET delivery_status = $3, delivery_error = $4 WHERE workspace_id = $1 AND id = $2")
        .bind(ws)
        .bind(id)
        .bind(delivery)
        .bind(error)
        .execute(&mut *tx)
        .await?;
    if delivery == "queued" {
        jobs::enqueue(&mut tx, ws, "send", Some(id)).await?;
    }
    let message = message(&mut tx, ws, id).await?;
    tx.commit().await?;
    Ok(Json(message))
}

/// `POST /api/tickets/{id}/comments` — never emailed, status unchanged (§20).
async fn comment(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Body(req): Body<TextRequest>,
) -> ApiResult<(StatusCode, Json<Message>)> {
    let text = nonempty(&req.text)?;
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let found = sqlx::query(
        "UPDATE tickets SET last_activity_at = now() WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if found.rows_affected() == 0 {
        return Err(ApiError::not_found());
    }
    let message_id: Uuid = sqlx::query_scalar(
        "INSERT INTO messages (workspace_id, ticket_id, kind, agent_id, text)
         VALUES ($1, $2, 'comment', $3, $4) RETURNING id",
    )
    .bind(ws)
    .bind(id)
    .bind(auth.agent_id)
    .bind(text)
    .fetch_one(&mut *tx)
    .await?;
    let message = message(&mut tx, ws, message_id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(message)))
}

/// `GET /api/attachments/{id}` — always a download (spec 002 §27). An HTML
/// or SVG attachment is attacker-controlled content on our origin otherwise.
async fn attachment(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let (name, content_type, content): (String, String, Vec<u8>) = sqlx::query_as(
        "SELECT name, content_type, content FROM attachments WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(ApiError::not_found)?;
    tx.commit().await?;
    let content_type = HeaderValue::from_str(&content_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let disposition = HeaderValue::from_str(&content_disposition(&name)).expect("sanitized header");
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_DISPOSITION, disposition),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static("sandbox"),
            ),
        ],
        content,
    )
        .into_response())
}

/// `attachment; filename="<ascii>"; filename*=UTF-8''<percent-encoded>` —
/// nothing from the mail can reach the header unescaped.
fn content_disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c == ' ' || (c.is_ascii_graphic() && c != '"' && c != '\\') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded = crate::percent_encode(name);
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition_cannot_break_the_header() {
        assert_eq!(
            content_disposition("rapport \"final\".pdf"),
            "attachment; filename=\"rapport _final_.pdf\"; filename*=UTF-8''rapport%20%22final%22.pdf"
        );
        let evil = content_disposition("a\r\nSet-Cookie: x=1.html");
        assert!(HeaderValue::from_str(&evil).is_ok());
        assert!(!evil.contains('\r') && !evil.contains('\n'));
        assert_eq!(
            content_disposition("blåbær.png"),
            "attachment; filename=\"bl_b_r.png\"; filename*=UTF-8''bl%C3%A5b%C3%A6r.png"
        );
    }

    #[test]
    fn cursor_round_trips() {
        assert_eq!(decode_cursor("not a cursor"), None);
        let raw = URL_SAFE_NO_PAD.encode(format!("2026-09-29T13:50:55.123456Z|{}", Uuid::nil()));
        assert_eq!(
            decode_cursor(&raw),
            Some(("2026-09-29T13:50:55.123456Z".parse().unwrap(), Uuid::nil()))
        );
    }
}

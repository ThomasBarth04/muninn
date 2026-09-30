//! Spec 002 and 006: views, filters and search, the ticket, read marks and
//! presence, drafts, replies, comments, snooze, attachments.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use sqlx::types::Json as SqlJson;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::api::{
    ContactTicket, Counts, Draft, DraftRequest, Message, PatchTicket, PatchTickets, ReplyRequest,
    TextRequest, Ticket, TicketDetail, TicketList, TicketsResponse, ViewFilters,
};
use crate::db::{Tx, tenant_tx};
use crate::error::ApiResult;
use crate::jobs::JobResult;
use crate::session::{Auth, Body, is_paid};
use crate::{ApiError, AppState, jobs, views};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/tickets", get(list).patch(patch_many))
        .route("/tickets/{id}", get(detail).patch(patch))
        .route("/tickets/{id}/read", put(mark_read).delete(mark_unread))
        .route("/tickets/{id}/draft", put(save_draft))
        .route("/tickets/{id}/replies", post(reply))
        .route("/tickets/{id}/comments", post(comment))
        .route("/messages/{id}", delete(delete_message))
        .route("/messages/{id}/retry", post(retry))
        .route("/attachments/{id}", get(attachment))
}

const PAGE: i64 = 50;
/// Spec 006 §23.
const MAX_BATCH: usize = 200;
/// Spec 006 §34: a reply waits this long before its send job is due.
const UNDO_SEND: Duration = Duration::seconds(10);

/// A message's author as a display name: an imported team reply has no agent,
/// so its author is the mail's (spec 005 §9).
const AUTHOR: &str = "CASE WHEN m.kind = 'customer' OR m.agent_id IS NULL
    THEN coalesce(nullif(m.from_name, ''), m.from_email, '') ELSE a.name END";

/// Unread for the agent asking (spec 006 §17): open, and a customer message
/// or the end of a snooze since they last read it. A snooze is over at its
/// time even if the wake job has not run (or gave up): views never depend on it.
const UNREAD: &str = "(t.status <> 'closed' AND (
    ((t.snoozed_until IS NULL OR t.snoozed_until <= now())
     AND t.woke_at IS NOT NULL AND t.woke_at > coalesce(r.seen_at, '-infinity'))
    OR EXISTS (SELECT 1 FROM messages um
               WHERE um.workspace_id = t.workspace_id AND um.ticket_id = t.id AND um.kind = 'customer'
                 AND um.created_at > coalesce(r.seen_at, '-infinity'))))";

/// Spec 003 §14.
const SEEN_BEFORE: &str = "EXISTS (SELECT 1 FROM suggestions s
    WHERE s.workspace_id = t.workspace_id AND s.ticket_id = t.id AND s.rank IS NOT NULL)";

/// The Ticket (spec 002 and 006 Contract), built by Postgres in the contract's
/// shape. Relative to `p.me`; `p.q` and `p.pattern` are the search.
fn ticket_json() -> String {
    format!(
        r#"json_build_object(
    'id', t.id,
    'number', t.number,
    'subject', t.subject,
    'status', t.status,
    'priority', t.priority,
    'owner', CASE WHEN o.id IS NOT NULL THEN json_build_object('id', o.id, 'name', o.name) END,
    'contact', json_build_object('id', c.id, 'email', c.email, 'name', c.name),
    'category', CASE WHEN cat.id IS NOT NULL THEN json_build_object(
        'id', cat.id, 'name', cat.name, 'source', t.category_source,
        'probability', CASE WHEN t.category_source = 'jev' THEN t.jev_category_probability END) END,
    'categorySuggestions', CASE WHEN t.category_id IS NULL THEN t.category_suggestions ELSE '[]'::jsonb END,
    'seenBefore', {SEEN_BEFORE},
    'lastMessage', (SELECT json_build_object(
                        'kind', m.kind,
                        'author', {AUTHOR},
                        'snippet', btrim(left(regexp_replace(m.text, '\s+', ' ', 'g'), 140)),
                        'at', m.created_at)
                    FROM messages m LEFT JOIN agents a ON a.workspace_id = m.workspace_id AND a.id = m.agent_id
                    WHERE m.workspace_id = t.workspace_id AND m.ticket_id = t.id
                    ORDER BY m.created_at DESC, m.id DESC LIMIT 1),
    'searchMatch', CASE WHEN p.pattern IS NOT NULL THEN (
                    SELECT json_build_object(
                        'kind', m.kind,
                        'author', {AUTHOR},
                        'snippet', CASE WHEN strpos(lower(x.s), lower(p.q)) > 41 THEN '…' ELSE '' END
                                   || btrim(left(substr(x.s, greatest(strpos(lower(x.s), lower(p.q)) - 40, 1)), 140)),
                        'at', m.created_at)
                    FROM messages m LEFT JOIN agents a ON a.workspace_id = m.workspace_id AND a.id = m.agent_id,
                         LATERAL (SELECT regexp_replace(m.text, '\s+', ' ', 'g') AS s) x
                    WHERE m.workspace_id = t.workspace_id AND m.ticket_id = t.id AND m.text ILIKE p.pattern
                    ORDER BY m.created_at DESC, m.id DESC LIMIT 1) END,
    'createdAt', t.created_at,
    'lastActivityAt', t.last_activity_at,
    'unread', {UNREAD},
    'waitingSince', waiting_since(t),
    'snoozedUntil', t.snoozed_until,
    'snoozeEnded', t.status <> 'closed' AND (t.snoozed_until IS NULL OR t.snoozed_until <= now())
                   AND t.woke_at IS NOT NULL AND t.woke_at > coalesce(r.seen_at, '-infinity'),
    'viewers', coalesce((
        SELECT json_agg(json_build_object(
                   'id', va.id, 'name', va.name,
                   'replying', EXISTS (SELECT 1 FROM drafts d
                                       WHERE d.workspace_id = v.workspace_id AND d.ticket_id = v.ticket_id
                                         AND d.agent_id = v.agent_id AND d.updated_at > now() - interval '60 seconds'))
                   ORDER BY va.name)
        FROM ticket_reads v JOIN agents va ON va.workspace_id = v.workspace_id AND va.id = v.agent_id
        WHERE v.workspace_id = t.workspace_id AND v.ticket_id = t.id AND v.agent_id <> p.me
          AND v.viewed_at > now() - interval '30 seconds'), '[]'::json),
    'hubspot', CASE WHEN t.hubspot_id IS NOT NULL THEN json_build_object('url', t.hubspot_url) END)"#
    )
}

/// `FROM`, joins and the tenant filter every list query shares. `p` binds the
/// agent asking and the search once, for every expression that needs them.
fn push_from(qb: &mut QueryBuilder<Postgres>, ws: Uuid, me: Uuid, q: Option<&str>) {
    let q = q.map(str::trim).filter(|q| !q.is_empty());
    qb.push(" FROM (SELECT ")
        .push_bind(me)
        .push("::uuid AS me, ")
        .push_bind(q.map(str::to_string))
        .push("::text AS q, ")
        .push_bind(q.map(like_pattern))
        .push("::text AS pattern) p");
    qb.push(
        " CROSS JOIN tickets t
          JOIN contacts c ON c.workspace_id = t.workspace_id AND c.id = t.contact_id
          LEFT JOIN agents o ON o.workspace_id = t.workspace_id AND o.id = t.owner_id
          LEFT JOIN categories cat ON cat.workspace_id = t.workspace_id AND cat.id = t.category_id
          LEFT JOIN ticket_reads r ON r.workspace_id = t.workspace_id AND r.ticket_id = t.id AND r.agent_id = p.me
          WHERE t.workspace_id = ",
    )
    .push_bind(ws);
}

/// `%q%` for ILIKE, with its wildcards and escape character taken literally.
fn like_pattern(q: &str) -> String {
    let mut p = String::from("%");
    for ch in q.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            p.push('\\');
        }
        p.push(ch);
    }
    p.push('%');
    p
}

/// `#1042` or `1042` (spec 006 §1).
fn ticket_number(q: &str) -> Option<i32> {
    let digits = q.trim().strip_prefix('#').unwrap_or(q.trim());
    digits
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| digits.parse().ok())
        .flatten()
}

/// `none` apart from the rest, parsed. None if any value does not parse.
fn split_none<T: std::str::FromStr>(values: &[String]) -> Option<(bool, Vec<T>)> {
    let mut none = false;
    let mut rest = vec![];
    for v in values {
        if v == "none" {
            none = true;
        } else {
            rest.push(v.parse().ok()?);
        }
    }
    Some((none, rest))
}

const VIEWS: [&str; 7] = [
    "unassigned",
    "mine",
    "open",
    "snoozed",
    "drafts",
    "closed",
    "all",
];
const SORTS: [&str; 5] = ["recent", "oldest", "waiting", "priority", "created"];
const STATUSES: [&str; 4] = ["new", "waitingOnContact", "waitingOnUs", "closed"];
const PRIORITIES: [&str; 4] = ["low", "medium", "high", "urgent"];

/// Validates what `GET /api/tickets` and a saved view take: the error code, if
/// any. An id that does not exist is not an error (§12); one that is not an id is.
pub fn check_filters(f: &ViewFilters) -> Result<(), &'static str> {
    if !VIEWS.contains(&f.view.as_str()) {
        return Err("invalidView");
    }
    if !SORTS.contains(&f.sort.as_str()) {
        return Err("invalidSort");
    }
    let owners = f
        .owner
        .iter()
        .filter(|o| *o != "me")
        .cloned()
        .collect::<Vec<_>>();
    let ok = f.q.as_ref().is_none_or(|q| q.chars().count() <= 200)
        && f.status.iter().all(|s| STATUSES.contains(&s.as_str()))
        && split_none::<Uuid>(&owners).is_some()
        && split_none::<String>(&f.priority)
            .is_some_and(|(_, p)| p.iter().all(|p| PRIORITIES.contains(&p.as_str())))
        && split_none::<Uuid>(&f.category).is_some()
        && f.created
            .as_deref()
            .is_none_or(|c| matches!(c, "24h" | "7d" | "30d"));
    if ok { Ok(()) } else { Err("invalidFilter") }
}

/// The view's and the filters' conditions (spec 006 §2–3, §7–13). A snoozed
/// ticket is only in Snoozed, Drafts and All tickets. Assumes `check_filters`.
fn push_filters(qb: &mut QueryBuilder<Postgres>, f: &ViewFilters, me: Uuid) {
    qb.push(match f.view.as_str() {
        "unassigned" => " AND t.status <> 'closed' AND NOT coalesce(t.snoozed_until > now(), false) AND t.owner_id IS NULL",
        "mine" => " AND t.status <> 'closed' AND NOT coalesce(t.snoozed_until > now(), false) AND t.owner_id = p.me",
        "open" => " AND t.status <> 'closed' AND NOT coalesce(t.snoozed_until > now(), false)",
        "snoozed" => " AND t.snoozed_until > now()",
        "drafts" => {
            " AND EXISTS (SELECT 1 FROM drafts d
                          WHERE d.workspace_id = t.workspace_id AND d.ticket_id = t.id AND d.agent_id = p.me)"
        }
        "closed" => " AND t.status = 'closed'",
        _ => "",
    });
    if !f.status.is_empty() {
        qb.push(" AND t.status = ANY(")
            .push_bind(f.status.clone())
            .push(")");
    }
    if !f.owner.is_empty() {
        let others: Vec<String> = f.owner.iter().filter(|o| *o != "me").cloned().collect();
        let (none, mut ids) = split_none::<Uuid>(&others).unwrap_or_default();
        if f.owner.iter().any(|o| o == "me") {
            ids.push(me);
        }
        qb.push(" AND (t.owner_id = ANY(")
            .push_bind(ids)
            .push(") OR (")
            .push_bind(none)
            .push(" AND t.owner_id IS NULL))");
    }
    if !f.priority.is_empty() {
        let (none, values) = split_none::<String>(&f.priority).unwrap_or_default();
        qb.push(" AND (t.priority = ANY(")
            .push_bind(values)
            .push(") OR (")
            .push_bind(none)
            .push(" AND t.priority IS NULL))");
    }
    if !f.category.is_empty() {
        let (none, ids) = split_none::<Uuid>(&f.category).unwrap_or_default();
        qb.push(" AND (t.category_id = ANY(")
            .push_bind(ids)
            .push(") OR (")
            .push_bind(none)
            .push(" AND t.category_id IS NULL))");
    }
    qb.push(match f.created.as_deref() {
        Some("24h") => " AND t.created_at > now() - interval '24 hours'",
        Some("7d") => " AND t.created_at > now() - interval '7 days'",
        Some("30d") => " AND t.created_at > now() - interval '30 days'",
        _ => "",
    });
    if f.unread {
        qb.push(" AND ").push(UNREAD);
    }
    if f.seen_before {
        qb.push(" AND ").push(SEEN_BEFORE);
    }
    // ponytail: substring search is a sequential scan of the workspace's
    // tickets and messages. Fine to tens of thousands of messages; past that,
    // pg_trgm GIN indexes on messages.text and tickets.subject serve the same ILIKE.
    if let Some(q) = f.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        qb.push(
            " AND (t.subject ILIKE p.pattern OR c.name ILIKE p.pattern OR c.email ILIKE p.pattern
                   OR EXISTS (SELECT 1 FROM messages sm
                              WHERE sm.workspace_id = t.workspace_id AND sm.ticket_id = t.id
                                AND sm.text ILIKE p.pattern)
                   OR t.number = ",
        )
        .push_bind(ticket_number(q))
        .push(")");
    }
}

const ACTIVITY: &str = "(extract(epoch FROM t.last_activity_at) * 1000000)::bigint";

/// Each sort as two bigint keys, both descending, then the id: one keyset
/// cursor shape for all five (spec 006 §10). Microseconds, so exact.
fn sort_keys(sort: &str) -> (String, String) {
    match sort {
        "oldest" => ("0".into(), format!("-{ACTIVITY}")),
        "created" => (
            "0".into(),
            "(extract(epoch FROM t.created_at) * 1000000)::bigint".into(),
        ),
        "priority" => (
            "CASE t.priority WHEN 'urgent' THEN 4 WHEN 'high' THEN 3 WHEN 'medium' THEN 2 WHEN 'low' THEN 1 ELSE 0 END".into(),
            ACTIVITY.into(),
        ),
        // Waiting longest first, then the rest by last activity. The clock is
        // migration 0005's waiting_since().
        "waiting" => (
            "(waiting_since(t) IS NOT NULL)::int".into(),
            format!(
                "coalesce(-(extract(epoch FROM waiting_since(t)) * 1000000)::bigint, {ACTIVITY})"
            ),
        ),
        _ => ("0".into(), ACTIVITY.into()),
    }
}

/// Where a page ended: the sort it belongs to and the last row's keys.
#[derive(Debug, PartialEq)]
struct Cursor {
    sort: String,
    k1: i64,
    k2: i64,
    id: Uuid,
}

/// Opaque to the client: base64url of `<sort>|<k1>|<k2>|<id>`.
fn encode_cursor(c: &Cursor) -> String {
    URL_SAFE_NO_PAD.encode(format!("{}|{}|{}|{}", c.sort, c.k1, c.k2, c.id))
}

/// None for anything but a cursor of this sort.
fn decode_cursor(cursor: &str, sort: &str) -> Option<Cursor> {
    let raw = String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).ok()?).ok()?;
    let mut parts = raw.split('|');
    let c = Cursor {
        sort: parts.next()?.to_string(),
        k1: parts.next()?.parse().ok()?,
        k2: parts.next()?.parse().ok()?,
        id: parts.next()?.parse().ok()?,
    };
    (parts.next().is_none() && c.sort == sort).then_some(c)
}

/// The list's tickets, or one ticket by `id`, each with its sort keys.
async fn query(
    tx: &mut Tx,
    ws: Uuid,
    me: Uuid,
    id: Option<Uuid>,
    f: &ViewFilters,
    after: Option<&Cursor>,
    limit: i64,
) -> Result<Vec<(Ticket, Cursor)>, sqlx::Error> {
    let (k1, k2) = sort_keys(&f.sort);
    let mut qb = QueryBuilder::new(format!(
        "SELECT {}, ({k1})::bigint, ({k2})::bigint",
        ticket_json()
    ));
    push_from(&mut qb, ws, me, f.q.as_deref());
    if let Some(id) = id {
        qb.push(" AND t.id = ").push_bind(id);
    }
    push_filters(&mut qb, f, me);
    if let Some(c) = after {
        qb.push(format!(" AND (({k1})::bigint, ({k2})::bigint, t.id) < ("))
            .push_bind(c.k1)
            .push(", ")
            .push_bind(c.k2)
            .push(", ")
            .push_bind(c.id)
            .push(")");
    }
    qb.push(" ORDER BY 2 DESC, 3 DESC, t.id DESC LIMIT ")
        .push_bind(limit);
    let rows: Vec<(SqlJson<Ticket>, i64, i64)> = qb.build_query_as().fetch_all(&mut **tx).await?;
    Ok(rows
        .into_iter()
        .map(|(SqlJson(t), k1, k2)| {
            let id = t.id;
            let sort = f.sort.clone();
            (t, Cursor { sort, k1, k2, id })
        })
        .collect())
}

/// No filters: every ticket, newest activity first.
fn everything() -> ViewFilters {
    ViewFilters {
        view: "all".into(),
        q: None,
        status: vec![],
        owner: vec![],
        priority: vec![],
        category: vec![],
        created: None,
        unread: false,
        seen_before: false,
        sort: "recent".into(),
    }
}

pub async fn ticket(tx: &mut Tx, ws: Uuid, id: Uuid, me: Uuid) -> ApiResult<Ticket> {
    query(tx, ws, me, Some(id), &everything(), None, 1)
        .await?
        .pop()
        .map(|(t, _)| t)
        .ok_or_else(ApiError::not_found)
}

/// How many tickets a saved view holds for the agent asking (spec 006 §2, §6).
pub async fn count(tx: &mut Tx, ws: Uuid, me: Uuid, f: &ViewFilters) -> Result<i32, sqlx::Error> {
    let mut qb = QueryBuilder::new("SELECT count(*)::int");
    push_from(&mut qb, ws, me, f.q.as_deref());
    push_filters(&mut qb, f, me);
    qb.build_query_scalar().fetch_one(&mut **tx).await
}

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

/// `?view=&q=&status=…` as `ViewFilters`; comma-separated lists, empty = any.
fn from_query(q: &HashMap<String, String>) -> ApiResult<ViewFilters> {
    let get = |k: &str| q.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let list = |k: &str| {
        get(k)
            .map(|v| {
                v.split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let flag = |k: &str| match get(k) {
        None => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err(ApiError::bad_request("invalidFilter")),
    };
    Ok(ViewFilters {
        view: get("view").unwrap_or_default().to_string(),
        q: get("q").map(str::to_string),
        status: list("status"),
        owner: list("owner"),
        priority: list("priority"),
        category: list("category"),
        created: get("created").map(str::to_string),
        unread: flag("unread")?,
        seen_before: flag("seenBefore")?,
        sort: get("sort").unwrap_or("recent").to_string(),
    })
}

/// `GET /api/tickets` — spec 002 §10–13, spec 006 §2–14.
async fn list(
    auth: Auth,
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<TicketList>> {
    let f = from_query(&q)?;
    check_filters(&f).map_err(ApiError::bad_request)?;
    let after = match q.get("cursor") {
        Some(c) => {
            Some(decode_cursor(c, &f.sort).ok_or_else(|| ApiError::bad_request("invalidCursor"))?)
        }
        None => None,
    };
    let (ws, me) = (auth.workspace_id, auth.agent_id);
    let mut tx = tenant_tx(&st.db, ws).await?;
    let mut page = query(&mut tx, ws, me, None, &f, after.as_ref(), PAGE + 1).await?;
    let next_cursor = if page.len() as i64 > PAGE {
        page.truncate(PAGE as usize);
        page.last().map(|(_, c)| encode_cursor(c))
    } else {
        None
    };
    let (unassigned, mine, open, drafts, has_tickets): (i32, i32, i32, i32, bool) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status <> 'closed' AND NOT coalesce(snoozed_until > now(), false) AND owner_id IS NULL)::int,
                count(*) FILTER (WHERE status <> 'closed' AND NOT coalesce(snoozed_until > now(), false) AND owner_id = $2)::int,
                count(*) FILTER (WHERE status <> 'closed' AND NOT coalesce(snoozed_until > now(), false))::int,
                (SELECT count(*) FROM drafts d WHERE d.workspace_id = $1 AND d.agent_id = $2)::int,
                count(*) > 0
         FROM tickets WHERE workspace_id = $1",
    )
    .bind(ws)
    .bind(me)
    .fetch_one(&mut *tx)
    .await?;
    // ponytail: one count query per saved view the agent can see. Fold them
    // into one UNION ALL if the 10-second poll shows up in pg_stat_statements.
    let mut views = HashMap::new();
    for v in views::visible(&mut tx, ws, me).await? {
        views.insert(v.id, count(&mut tx, ws, me, &v.filters).await?);
    }
    tx.commit().await?;
    Ok(Json(TicketList {
        tickets: page.into_iter().map(|(t, _)| t).collect(),
        next_cursor,
        counts: Counts {
            unassigned,
            mine,
            open,
            drafts,
            views,
        },
        has_tickets,
    }))
}

/// `GET /api/tickets/{id}` — spec 002 §14–15. Also marks it read for the
/// agent and records them as viewing (spec 006 §17, §29).
async fn detail(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<TicketDetail>> {
    let (ws, me) = (auth.workspace_id, auth.agent_id);
    let mut tx = tenant_tx(&st.db, ws).await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO ticket_reads (workspace_id, ticket_id, agent_id, seen_at, viewed_at)
         SELECT t.workspace_id, t.id, $3, {SEEN}, now() FROM tickets t WHERE t.workspace_id = $1 AND t.id = $2
         ON CONFLICT (workspace_id, ticket_id, agent_id)
         DO UPDATE SET seen_at = greatest(ticket_reads.seen_at, EXCLUDED.seen_at), viewed_at = now()"
    )))
    .bind(ws)
    .bind(id)
    .bind(me)
    .execute(&mut *tx)
    .await?;
    let ticket = ticket(&mut tx, ws, id, me).await?;
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
    let draft: Option<SqlJson<Draft>> = sqlx::query_scalar(
        "SELECT json_build_object('mode', mode, 'text', text, 'updatedAt', updated_at)
         FROM drafts WHERE workspace_id = $1 AND ticket_id = $2 AND agent_id = $3",
    )
    .bind(ws)
    .bind(id)
    .bind(me)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(TicketDetail {
        ticket,
        messages,
        contact_tickets: contact_tickets.into_iter().map(|SqlJson(c)| c).collect(),
        draft: draft.map(|SqlJson(d)| d),
    }))
}

async fn exists(tx: &mut Tx, ws: Uuid, id: Uuid) -> ApiResult<()> {
    let found: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM tickets WHERE workspace_id = $1 AND id = $2)",
    )
    .bind(ws)
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    if found {
        Ok(())
    } else {
        Err(ApiError::not_found())
    }
}

/// What reading a ticket has seen: its newest message or the end of its
/// snooze, as this transaction sees them. Not `now()`: a mail whose
/// transaction started earlier but commits later is newer than the read.
const SEEN: &str = "coalesce(greatest(t.woke_at, (SELECT max(m.created_at) FROM messages m
                                                  WHERE m.workspace_id = t.workspace_id AND m.ticket_id = t.id)), now())";

/// `PUT /api/tickets/{id}/read` — read without opening it (spec 006 §17).
async fn mark_read(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let done = sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO ticket_reads (workspace_id, ticket_id, agent_id, seen_at)
         SELECT t.workspace_id, t.id, $3, {SEEN} FROM tickets t WHERE t.workspace_id = $1 AND t.id = $2
         ON CONFLICT (workspace_id, ticket_id, agent_id)
         DO UPDATE SET seen_at = greatest(ticket_reads.seen_at, EXCLUDED.seen_at)"
    )))
    .bind(ws)
    .bind(id)
    .bind(auth.agent_id)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::not_found());
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/tickets/{id}/read` — unread again for this agent: read up to
/// just before the newest customer message. A snooze that ended before that
/// message does not come back as "Snooze ended".
async fn mark_unread(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let done = sqlx::query(
        "INSERT INTO ticket_reads (workspace_id, ticket_id, agent_id, seen_at)
         SELECT t.workspace_id, t.id, $3,
                coalesce((SELECT max(m.created_at) FROM messages m
                          WHERE m.workspace_id = t.workspace_id AND m.ticket_id = t.id AND m.kind = 'customer'),
                         t.created_at) - interval '1 microsecond'
         FROM tickets t WHERE t.workspace_id = $1 AND t.id = $2
         ON CONFLICT (workspace_id, ticket_id, agent_id) DO UPDATE SET seen_at = EXCLUDED.seen_at",
    )
    .bind(ws)
    .bind(id)
    .bind(auth.agent_id)
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::not_found());
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/tickets/{id}/draft` — per agent and ticket, private (spec 006 §32).
async fn save_draft(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Body(req): Body<DraftRequest>,
) -> ApiResult<StatusCode> {
    if !matches!(req.mode.as_str(), "reply" | "comment") {
        return Err(ApiError::bad_request("invalidMode"));
    }
    let (ws, me) = (auth.workspace_id, auth.agent_id);
    let mut tx = tenant_tx(&st.db, ws).await?;
    exists(&mut tx, ws, id).await?;
    if req.text.trim().is_empty() {
        delete_draft(&mut tx, ws, id, me).await?;
    } else {
        sqlx::query(
            "INSERT INTO drafts (workspace_id, ticket_id, agent_id, mode, text) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (workspace_id, ticket_id, agent_id)
             DO UPDATE SET mode = EXCLUDED.mode, text = EXCLUDED.text, updated_at = now()",
        )
        .bind(ws)
        .bind(id)
        .bind(me)
        .bind(&req.mode)
        .bind(&req.text)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_draft(tx: &mut Tx, ws: Uuid, ticket: Uuid, me: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM drafts WHERE workspace_id = $1 AND ticket_id = $2 AND agent_id = $3")
        .bind(ws)
        .bind(ticket)
        .bind(me)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// ADR 0010: closing indexes the whole thread into the brain — again when it
/// was closed already, so a Send and close reply is in it — and leaving closed
/// takes it out; closing ends a snooze (spec 006 §26). When the status was
/// set, for the waiting clock, is the table's trigger's (migration 0005).
async fn set_status(tx: &mut Tx, ws: Uuid, id: Uuid, status: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE tickets t SET
            status = $3,
            closed_at = CASE WHEN $3 = 'closed' THEN coalesce(t.closed_at, now()) END,
            search = CASE WHEN $3 = 'closed' THEN brain_tsvector(t.workspace_id, t.id) END,
            snoozed_until = CASE WHEN $3 = 'closed' THEN NULL ELSE t.snoozed_until END
         WHERE t.workspace_id = $1 AND t.id = $2",
    )
    .bind(ws)
    .bind(id)
    .bind(status)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// One ticket's PATCH, inside the caller's transaction; an error leaves the
/// transaction to be dropped, so a batch is all or nothing.
async fn apply(tx: &mut Tx, ws: Uuid, id: Uuid, req: &PatchTicket) -> ApiResult<()> {
    if let Some(s) = &req.status
        && !STATUSES.contains(&s.as_str())
    {
        return Err(ApiError::bad_request("invalidStatus"));
    }
    if let Some(Some(p)) = &req.priority
        && !PRIORITIES.contains(&p.as_str())
    {
        return Err(ApiError::bad_request("invalidPriority"));
    }
    let now = Utc::now();
    if let Some(Some(until)) = req.snoozed_until
        && (until <= now || until > now + Duration::days(365))
    {
        return Err(ApiError::bad_request("invalidSnooze"));
    }
    let (current, hubspot): (String, bool) = sqlx::query_as(
        "SELECT status, hubspot_id IS NOT NULL FROM tickets WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(ws)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(ApiError::not_found)?;
    // Spec 007 §25–26: HubSpot owns these; category and snooze are Muninn's own.
    if hubspot && (req.status.is_some() || req.owner_id.is_some() || req.priority.is_some()) {
        return Err(synced_from_hubspot());
    }
    let closed = req.status.as_deref().unwrap_or(&current) == "closed";
    if closed && matches!(req.snoozed_until, Some(Some(_))) {
        return Err(ApiError::conflict("ticketClosed"));
    }
    if let Some(Some(owner)) = req.owner_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM agents WHERE workspace_id = $1 AND id = $2 AND removed_at IS NULL)",
        )
        .bind(ws)
        .bind(owner)
        .fetch_one(&mut **tx)
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
        .fetch_one(&mut **tx)
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
            .execute(&mut **tx)
            .await?;
    }
    if let Some(priority) = &req.priority {
        sqlx::query("UPDATE tickets SET priority = $3 WHERE workspace_id = $1 AND id = $2")
            .bind(ws)
            .bind(id)
            .bind(priority)
            .execute(&mut **tx)
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
        .execute(&mut **tx)
        .await?;
    }
    if let Some(snoozed_until) = req.snoozed_until {
        sqlx::query("UPDATE tickets SET snoozed_until = $3 WHERE workspace_id = $1 AND id = $2")
            .bind(ws)
            .bind(id)
            .bind(snoozed_until)
            .execute(&mut **tx)
            .await?;
        if let Some(at) = snoozed_until {
            jobs::enqueue_at(tx, ws, "wake", Some(id), Some(at)).await?;
        }
    }
    if let Some(status) = &req.status {
        set_status(tx, ws, id, status).await?;
    }
    Ok(())
}

/// `PATCH /api/tickets/{id}` — spec 002 §21–22, spec 006 §25–27. Last write wins.
async fn patch(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Body(req): Body<PatchTicket>,
) -> ApiResult<Json<Ticket>> {
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    apply(&mut tx, ws, id, &req).await?;
    let ticket = ticket(&mut tx, ws, id, auth.agent_id).await?;
    tx.commit().await?;
    Ok(Json(ticket))
}

/// `PATCH /api/tickets` — up to 200 tickets in one transaction (spec 006 §23–24).
async fn patch_many(
    auth: Auth,
    State(st): State<AppState>,
    Body(req): Body<PatchTickets>,
) -> ApiResult<Json<TicketsResponse>> {
    let mut ids: Vec<Uuid> = req.tickets.iter().map(|t| t.id).collect();
    ids.sort();
    ids.dedup();
    if req.tickets.is_empty() || req.tickets.len() > MAX_BATCH || ids.len() != req.tickets.len() {
        return Err(ApiError::bad_request("invalidBatch"));
    }
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    // Locked in id order, so two overlapping batches cannot deadlock.
    sqlx::query(
        "SELECT 1 FROM tickets WHERE workspace_id = $1 AND id = ANY($2) ORDER BY id FOR UPDATE",
    )
    .bind(ws)
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    let mut tickets = vec![];
    for item in &req.tickets {
        apply(&mut tx, ws, item.id, &item.patch).await?;
    }
    for item in &req.tickets {
        tickets.push(ticket(&mut tx, ws, item.id, auth.agent_id).await?);
    }
    tx.commit().await?;
    Ok(Json(TicketsResponse { tickets }))
}

/// The `wake` job (spec 006 §25): back in its views at the top, unread for
/// everyone. Does nothing if it was unsnoozed, snoozed again or closed since.
pub async fn wake_job(st: &AppState, ws: Uuid, id: Uuid) -> JobResult {
    let mut tx = tenant_tx(&st.db, ws).await?;
    sqlx::query(
        "UPDATE tickets SET snoozed_until = NULL, woke_at = now(), last_activity_at = now()
         WHERE workspace_id = $1 AND id = $2 AND snoozed_until <= now() AND status <> 'closed'",
    )
    .bind(ws)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
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

/// Whether the ticket came from HubSpot (spec 007); `404` if there is none.
async fn from_hubspot(tx: &mut Tx, ws: Uuid, id: Uuid) -> ApiResult<bool> {
    sqlx::query_scalar(
        "SELECT hubspot_id IS NOT NULL FROM tickets WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(ApiError::not_found)
}

/// Spec 007 §25: a HubSpot ticket is answered in HubSpot.
fn synced_from_hubspot() -> ApiError {
    ApiError::conflict("syncedFromHubSpot")
}

fn nonempty(text: &str) -> ApiResult<&str> {
    let text = text.trim();
    if text.is_empty() {
        Err(ApiError::bad_request("emptyText"))
    } else {
        Ok(text)
    }
}

/// `POST /api/tickets/{id}/replies` — spec 002 §16–19, spec 006 §33–36.
/// Replying to an unassigned ticket makes it the replier's (spec 002 open
/// question 2). A snooze is left alone unless the reply closes the ticket.
async fn reply(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Body(req): Body<ReplyRequest>,
) -> ApiResult<(StatusCode, Json<Message>)> {
    let text = nonempty(&req.text)?;
    let status = req.status.as_deref().unwrap_or("waitingOnContact");
    if !matches!(status, "waitingOnContact" | "closed") {
        return Err(ApiError::bad_request("invalidStatus"));
    }
    let (ws, me) = (auth.workspace_id, auth.agent_id);
    let mut tx = tenant_tx(&st.db, ws).await?;
    if from_hubspot(&mut tx, ws, id).await? {
        return Err(synced_from_hubspot());
    }
    let found = sqlx::query(
        "UPDATE tickets SET last_activity_at = now(), owner_id = coalesce(owner_id, $3)
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(id)
    .bind(me)
    .execute(&mut *tx)
    .await?;
    if found.rows_affected() == 0 {
        return Err(ApiError::not_found());
    }
    // §36: someone else wrote since the agent last loaded the thread. The
    // transaction is dropped, so nothing is stored.
    if let Some(after) = req.after {
        let newer: Option<bool> = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM messages m
                            WHERE m.workspace_id = s.workspace_id AND m.ticket_id = s.ticket_id
                              AND (m.created_at, m.id) > (s.created_at, s.id)
                              AND (m.kind = 'customer' OR m.agent_id IS DISTINCT FROM $4))
             FROM messages s WHERE s.workspace_id = $1 AND s.ticket_id = $2 AND s.id = $3",
        )
        .bind(ws)
        .bind(id)
        .bind(after)
        .bind(me)
        .fetch_optional(&mut *tx)
        .await?;
        match newer {
            None => return Err(ApiError::bad_request("invalidAfter")),
            Some(true) => return Err(ApiError::conflict("newActivity")),
            Some(false) => {}
        }
    }
    let (delivery, error) = delivery_for_send(&mut tx, ws).await?;
    let message_id: Uuid = sqlx::query_scalar(
        "INSERT INTO messages (workspace_id, ticket_id, kind, agent_id, text, message_id, delivery_status, delivery_error)
         VALUES ($1, $2, 'agent', $3, $4, gen_random_uuid() || '@' || $5, $6, $7) RETURNING id",
    )
    .bind(ws)
    .bind(id)
    .bind(me)
    .bind(text)
    .bind(&st.cfg.inbound_domain)
    .bind(delivery)
    .bind(error)
    .fetch_one(&mut *tx)
    .await?;
    // After the insert, so closing indexes the reply into the brain too.
    set_status(&mut tx, ws, id, status).await?;
    if delivery == "queued" {
        let due = Utc::now() + UNDO_SEND;
        jobs::enqueue_at(&mut tx, ws, "send", Some(message_id), Some(due)).await?;
    }
    delete_draft(&mut tx, ws, id, me).await?;
    let message = message(&mut tx, ws, message_id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(message)))
}

/// `DELETE /api/messages/{id}` — undo send and Discard (spec 006 §34–35). A
/// claim bumps the job's attempts, so an untouched job is one not yet picked up.
async fn delete_message(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let (kind, status, ticket): (String, Option<String>, Uuid) = sqlx::query_as(
        "SELECT kind, delivery_status, ticket_id FROM messages WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(ws)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(ApiError::not_found)?;
    let deletable =
        match (kind, status) {
            (kind, Some(status)) if kind == "agent" => match status.as_str() {
                "held" | "failed" => true,
                "queued" => sqlx::query(
                    "DELETE FROM jobs WHERE workspace_id = $1 AND kind = 'send' AND subject_id = $2
                       AND attempts = 0 AND failed_at IS NULL",
                )
                .bind(ws)
                .bind(id)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                    > 0,
                _ => false,
            },
            _ => false,
        };
    if !deletable {
        return Err(ApiError::conflict("notDeletable"));
    }
    sqlx::query("DELETE FROM messages WHERE workspace_id = $1 AND id = $2")
        .bind(ws)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    // A reply discarded from a closed ticket leaves its brain entry too (ADR 0010).
    sqlx::query(
        "UPDATE tickets SET search = brain_tsvector(workspace_id, id)
         WHERE workspace_id = $1 AND id = $2 AND status = 'closed'",
    )
    .bind(ws)
    .bind(ticket)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
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

/// `POST /api/tickets/{id}/comments` — never emailed, status and snooze
/// unchanged (spec 002 §20, spec 006 §26).
async fn comment(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Body(req): Body<TextRequest>,
) -> ApiResult<(StatusCode, Json<Message>)> {
    let text = nonempty(&req.text)?;
    let (ws, me) = (auth.workspace_id, auth.agent_id);
    let mut tx = tenant_tx(&st.db, ws).await?;
    if from_hubspot(&mut tx, ws, id).await? {
        return Err(synced_from_hubspot());
    }
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
    .bind(me)
    .bind(text)
    .fetch_one(&mut *tx)
    .await?;
    delete_draft(&mut tx, ws, id, me).await?;
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
    fn cursor_round_trips_and_keeps_its_sort() {
        assert_eq!(decode_cursor("not a cursor", "recent"), None);
        let c = Cursor {
            sort: "waiting".into(),
            k1: 1,
            k2: -1_790_000_000_000_000,
            id: Uuid::nil(),
        };
        let raw = encode_cursor(&c);
        assert_eq!(decode_cursor(&raw, "waiting"), Some(c));
        assert_eq!(decode_cursor(&raw, "recent"), None);
    }

    #[test]
    fn search_is_literal() {
        assert_eq!(like_pattern(r"50%_off\"), r"%50\%\_off\\%");
        assert_eq!(ticket_number("#1042"), Some(1042));
        assert_eq!(ticket_number(" 1042 "), Some(1042));
        assert_eq!(ticket_number("+5"), None);
        assert_eq!(ticket_number("#"), None);
        assert_eq!(ticket_number("99999999999"), None);
        assert_eq!(ticket_number("sso"), None);
    }

    #[test]
    fn filters_are_checked() {
        let mut f = everything();
        assert_eq!(check_filters(&f), Ok(()));
        f.owner = vec!["me".into(), "none".into(), Uuid::nil().to_string()];
        f.priority = vec!["none".into(), "urgent".into()];
        f.category = vec!["none".into()];
        assert_eq!(check_filters(&f), Ok(()));
        for bad in [
            ViewFilters {
                owner: vec!["vetle".into()],
                ..everything()
            },
            ViewFilters {
                priority: vec!["meh".into()],
                ..everything()
            },
            ViewFilters {
                created: Some("1y".into()),
                ..everything()
            },
            ViewFilters {
                q: Some("x".repeat(201)),
                ..everything()
            },
        ] {
            assert_eq!(check_filters(&bad), Err("invalidFilter"), "{bad:?}");
        }
        f.view = "inbox".into();
        assert_eq!(check_filters(&f), Err("invalidView"));
        f.view = "all".into();
        f.sort = "random".into();
        assert_eq!(check_filters(&f), Err("invalidSort"));
    }
}

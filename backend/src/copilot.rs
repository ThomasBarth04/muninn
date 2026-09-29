//! Spec 003: similar cases. Postgres full-text search finds candidates in the
//! brain (ADR 0010), Jev judges them (ADR 0002), agents rate the result.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use sqlx::types::Json as SqlJson;
use uuid::Uuid;

use crate::api::{FeedbackRequest, Suggestion, Suggestions};
use crate::db::{Tx, tenant_tx};
use crate::error::ApiResult;
use crate::jobs::{JobError, JobResult};
use crate::session::{Auth, Body};
use crate::{ApiError, AppState, jev};

/// A candidate is shown when Jev scores it at least this (spec 003 §5).
/// Tuned against `docs/queries/suggestion-quality.sql`.
pub const SIMILARITY_THRESHOLD: f64 = 0.5;
const MAX_SHOWN: usize = 3;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/tickets/{id}/suggestions", get(suggestions))
        .route("/suggestions/{id}/feedback", post(feedback))
        .route("/suggestions/{id}/opened", post(opened))
}

/// Jev's `state` for a ticket: its subject and first message (spec 003 §4,
/// spec 004 §5). None if the ticket is gone.
pub async fn ticket_state(
    tx: &mut Tx,
    ws: Uuid,
    ticket_id: Uuid,
) -> Result<Option<Value>, sqlx::Error> {
    // ponytail: the message is cut to 8,000 characters to stay well inside
    // Jev's 32k-token state limit; a longer first mail loses its tail.
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT t.subject,
                (SELECT left(m.text, 8000) FROM messages m
                 WHERE m.workspace_id = $1 AND m.ticket_id = t.id
                 ORDER BY m.created_at, m.id LIMIT 1)
         FROM tickets t WHERE t.workspace_id = $1 AND t.id = $2",
    )
    .bind(ws)
    .bind(ticket_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(
        |(subject, message)| json!({ "subject": subject, "message": message.unwrap_or_default() }),
    ))
}

/// Spec 003 §1–5. Runs once per ticket: a ticket that is no longer pending is left alone.
pub async fn suggest_job(st: &AppState, ws: Uuid, ticket_id: Uuid) -> JobResult {
    let mut tx = tenant_tx(&st.db, ws).await?;
    let pending: Option<bool> = sqlx::query_scalar(
        "SELECT suggest_status = 'pending' FROM tickets WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(ticket_id)
    .fetch_optional(&mut *tx)
    .await?;
    if pending != Some(true) {
        return Ok(());
    }
    let Some(state) = ticket_state(&mut tx, ws, ticket_id).await? else {
        return Ok(());
    };
    let query_text = format!(
        "{} {}",
        state["subject"].as_str().unwrap_or(""),
        state["message"].as_str().unwrap_or("")
    );

    // The OR of the new ticket's stemmed words, built from its tsvector's
    // lexemes (quoted, so no input can inject tsquery operators), ranked by
    // ts_rank_cd. Each candidate comes back as the text Jev reads: subject,
    // first customer message, internal comments, last agent reply — cut to
    // 2,000 characters in that order of priority (spec 003 §4).
    let candidates: Vec<(Uuid, String)> = sqlx::query_as(
        r"WITH q AS (
             SELECT string_agg('''' || replace(replace(l, '\', '\\'), '''', '''''') || '''', ' | ')::tsquery AS q
             FROM workspaces w, unnest(tsvector_to_array(to_tsvector(w.language, left($2, 4000)))) l
             WHERE w.id = $1
         )
         SELECT t.id, left(concat_ws(E'\n\n', t.subject,
             'Customer: ' || (SELECT m.text FROM messages m
                              WHERE m.workspace_id = $1 AND m.ticket_id = t.id AND m.kind = 'customer'
                              ORDER BY m.created_at, m.id LIMIT 1),
             'Internal: ' || (SELECT string_agg(m.text, E'\n' ORDER BY m.created_at, m.id) FROM messages m
                              WHERE m.workspace_id = $1 AND m.ticket_id = t.id AND m.kind = 'comment'),
             'Solution: ' || (SELECT m.text FROM messages m
                              WHERE m.workspace_id = $1 AND m.ticket_id = t.id AND m.kind = 'agent'
                              ORDER BY m.created_at DESC, m.id DESC LIMIT 1)
         ), 2000)
         FROM tickets t, q
         WHERE t.workspace_id = $1 AND t.status = 'closed' AND t.search IS NOT NULL
           AND t.id <> $3 AND t.search @@ q.q
         ORDER BY ts_rank_cd(t.search, q.q) DESC, t.id
         LIMIT 50",
    )
    .bind(ws)
    .bind(&query_text)
    .bind(ticket_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    // Empty brain, or nothing shares a word: done, Jev is not called (§3).
    let scores = if candidates.is_empty() {
        vec![]
    } else {
        let questions: serde_json::Map<String, Value> = candidates
            .iter()
            .enumerate()
            .map(|(n, (_, past_case))| {
                let question = json!({
                    "type": "noul",
                    "instructions": {
                        "past_case": past_case,
                        "question": "Is past_case about the same underlying problem as the ticket in state?",
                    },
                    "criteria": {
                        "true": "Same root cause, or the same fix would solve it",
                        "false": "A different problem, even if it uses similar words",
                    },
                });
                (format!("case_{n}"), question)
            })
            .collect();
        let answers = jev::ask(st, state, Value::Object(questions)).await?;
        (0..candidates.len())
            .map(|n| {
                answers[format!("case_{n}")]["noul"]
                    .as_f64()
                    .ok_or_else(|| {
                        JobError::Fail(format!("Jev answer without case_{n}.noul: {answers}"))
                    })
            })
            .collect::<Result<Vec<f64>, JobError>>()?
    };

    // Best first; the top three at or over the threshold get a rank and are shown.
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]));
    let mut ranks: Vec<Option<i32>> = vec![None; scores.len()];
    for (rank, &i) in order
        .iter()
        .filter(|&&i| scores[i] >= SIMILARITY_THRESHOLD)
        .take(MAX_SHOWN)
        .enumerate()
    {
        ranks[i] = Some(rank as i32 + 1);
    }
    let case_ids: Vec<Uuid> = candidates.iter().map(|(id, _)| *id).collect();

    let mut tx = tenant_tx(&st.db, ws).await?;
    // Every score is stored, shown or not: the evals (§5, §15).
    sqlx::query(
        "INSERT INTO suggestions (workspace_id, ticket_id, case_ticket_id, score, rank)
         SELECT $1, $2, c, s, r FROM unnest($3::uuid[], $4::float8[], $5::int4[]) AS x(c, s, r)
         ON CONFLICT (ticket_id, case_ticket_id) DO NOTHING",
    )
    .bind(ws)
    .bind(ticket_id)
    .bind(&case_ids)
    .bind(&scores)
    .bind(&ranks)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE tickets SET suggest_status = 'ready' WHERE workspace_id = $1 AND id = $2")
        .bind(ws)
        .bind(ticket_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Out of attempts: "Suggestions unavailable right now." (§11).
pub async fn suggest_gave_up(st: &AppState, ws: Uuid, ticket_id: Uuid) -> Result<(), sqlx::Error> {
    let mut tx = tenant_tx(&st.db, ws).await?;
    sqlx::query(
        "UPDATE tickets SET suggest_status = 'failed'
         WHERE workspace_id = $1 AND id = $2 AND suggest_status = 'pending'",
    )
    .bind(ws)
    .bind(ticket_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// `GET /api/tickets/{id}/suggestions`
async fn suggestions(
    State(st): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Suggestions>> {
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let (status, brain_size): (String, i32) = sqlx::query_as(
        "SELECT t.suggest_status,
                (SELECT count(*)::int FROM tickets b WHERE b.workspace_id = $1 AND b.status = 'closed')
         FROM tickets t WHERE t.workspace_id = $1 AND t.id = $2",
    )
    .bind(ws)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(ApiError::not_found)?;

    // Only shown suggestions whose case is still in the brain — a reopened
    // case has left it (ADR 0010).
    let suggestions: Vec<SqlJson<Suggestion>> = if status == "ready" {
        sqlx::query_scalar(
            "SELECT json_build_object(
                 'id', s.id,
                 'case', json_build_object('ticketId', c.id, 'subject', c.subject, 'closedAt', c.closed_at),
                 'score', s.score,
                 'solution', (SELECT json_build_object('text', m.text, 'author', json_build_object('name', a.name),
                                                       'at', m.created_at)
                              FROM messages m JOIN agents a ON a.id = m.agent_id
                              WHERE m.workspace_id = $1 AND m.ticket_id = c.id AND m.kind = 'agent'
                              ORDER BY m.created_at DESC, m.id DESC LIMIT 1),
                 'myFeedback', (SELECT f.verdict FROM suggestion_feedback f
                                WHERE f.workspace_id = $1 AND f.suggestion_id = s.id AND f.agent_id = $3))
             FROM suggestions s JOIN tickets c ON c.id = s.case_ticket_id
             WHERE s.workspace_id = $1 AND c.workspace_id = $1 AND s.ticket_id = $2
               AND s.rank IS NOT NULL AND c.status = 'closed' AND c.closed_at IS NOT NULL
             ORDER BY s.rank",
        )
        .bind(ws)
        .bind(id)
        .bind(auth.agent_id)
        .fetch_all(&mut *tx)
        .await?
    } else {
        vec![]
    };
    tx.commit().await?;
    Ok(Json(Suggestions {
        status,
        brain_size,
        suggestions: suggestions.into_iter().map(|SqlJson(s)| s).collect(),
    }))
}

/// `POST /api/suggestions/{id}/feedback` — one verdict per agent, the latest wins (§13).
async fn feedback(
    State(st): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Body(req): Body<FeedbackRequest>,
) -> ApiResult<StatusCode> {
    if !matches!(req.verdict.as_str(), "helped" | "notRelevant") {
        return Err(ApiError::bad_request("invalidVerdict"));
    }
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let n = sqlx::query(
        "INSERT INTO suggestion_feedback (suggestion_id, agent_id, workspace_id, verdict, verdict_at)
         SELECT id, $3, $1, $4, now() FROM suggestions WHERE workspace_id = $1 AND id = $2 AND rank IS NOT NULL
         ON CONFLICT (suggestion_id, agent_id)
         DO UPDATE SET verdict = excluded.verdict, verdict_at = excluded.verdict_at",
    )
    .bind(ws)
    .bind(id)
    .bind(auth.agent_id)
    .bind(&req.verdict)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    if n == 0 {
        Err(ApiError::not_found())
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

/// `POST /api/suggestions/{id}/opened` — the first open per agent is kept (§12).
async fn opened(
    State(st): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let n = sqlx::query(
        "INSERT INTO suggestion_feedback (suggestion_id, agent_id, workspace_id, opened_at)
         SELECT id, $3, $1, now() FROM suggestions WHERE workspace_id = $1 AND id = $2 AND rank IS NOT NULL
         ON CONFLICT (suggestion_id, agent_id)
         DO UPDATE SET opened_at = coalesce(suggestion_feedback.opened_at, excluded.opened_at)",
    )
    .bind(ws)
    .bind(id)
    .bind(auth.agent_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    if n == 0 {
        Err(ApiError::not_found())
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

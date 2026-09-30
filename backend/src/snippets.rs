//! Spec 006 §37–38: snippets. Human-written (ADR 0002), workspace-wide, and
//! any agent manages them. Placeholders are filled in by the SPA on insert.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::api::{NewSnippet, PatchSnippet, Snippet, SnippetsResponse};
use crate::db::tenant_tx;
use crate::error::ApiResult;
use crate::session::{Auth, Body};
use crate::{ApiError, AppState};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/snippets", get(list).post(create))
        .route("/snippets/{id}", patch(update).delete(remove))
}

type Row = (Uuid, String, String, DateTime<Utc>);

fn snippet((id, name, text, updated_at): Row) -> Snippet {
    Snippet {
        id,
        name,
        text,
        updated_at,
    }
}

fn valid_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 60 {
        return Err(ApiError::bad_request("invalidName"));
    }
    Ok(name.to_string())
}

fn valid_text(text: &str) -> ApiResult<&str> {
    if text.trim().is_empty() || text.chars().count() > 5000 {
        return Err(ApiError::bad_request("invalidText"));
    }
    Ok(text)
}

/// `GET /api/snippets` — by name.
async fn list(auth: Auth, State(st): State<AppState>) -> ApiResult<Json<SnippetsResponse>> {
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, name, text, updated_at FROM snippets WHERE workspace_id = $1
         ORDER BY lower(name), id",
    )
    .bind(auth.workspace_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(SnippetsResponse {
        snippets: rows.into_iter().map(snippet).collect(),
    }))
}

/// `POST /api/snippets`
async fn create(
    auth: Auth,
    State(st): State<AppState>,
    Body(req): Body<NewSnippet>,
) -> ApiResult<(StatusCode, Json<Snippet>)> {
    let name = valid_name(&req.name)?;
    let text = valid_text(&req.text)?;
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    let row: Row = sqlx::query_as(
        "INSERT INTO snippets (workspace_id, name, text) VALUES ($1, $2, $3)
         RETURNING id, name, text, updated_at",
    )
    .bind(auth.workspace_id)
    .bind(name)
    .bind(text)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(snippet(row))))
}

/// `PATCH /api/snippets/{id}` — any of name, text.
async fn update(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Body(req): Body<PatchSnippet>,
) -> ApiResult<Json<Snippet>> {
    let name = req.name.as_deref().map(valid_name).transpose()?;
    let text = req.text.as_deref().map(valid_text).transpose()?;
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    let row: Row = sqlx::query_as(
        "UPDATE snippets SET name = coalesce($3, name), text = coalesce($4, text), updated_at = now()
         WHERE workspace_id = $1 AND id = $2
         RETURNING id, name, text, updated_at",
    )
    .bind(auth.workspace_id)
    .bind(id)
    .bind(name)
    .bind(text)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(ApiError::not_found)?;
    tx.commit().await?;
    Ok(Json(snippet(row)))
}

/// `DELETE /api/snippets/{id}`
async fn remove(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    let done = sqlx::query("DELETE FROM snippets WHERE workspace_id = $1 AND id = $2")
        .bind(auth.workspace_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::not_found());
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

//! Spec 006 §4–6: saved views. A view is private to its creator unless
//! shared; only the creator changes it. Its filters are what
//! `GET /api/tickets` takes, with `owner: me` meaning whoever is looking.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch};
use axum::{Json, Router};
use sqlx::types::Json as SqlJson;
use uuid::Uuid;

use crate::api::{NewView, PatchView, View, ViewFilters, ViewsResponse};
use crate::db::{Tx, tenant_tx};
use crate::error::ApiResult;
use crate::session::{Auth, Body};
use crate::tickets::check_filters;
use crate::{ApiError, AppState};

/// Per agent (§4).
const MAX_VIEWS: i64 = 30;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/views", get(list).post(create))
        .route("/views/{id}", patch(update).delete(remove))
}

/// Views the agent can see: their own, then those others shared, each by
/// name. `$3` narrows to one.
const VIEWS: &str = "
SELECT json_build_object('id', v.id, 'name', v.name, 'shared', v.shared,
                         'createdBy', json_build_object('id', a.id, 'name', a.name),
                         'filters', v.filters)
FROM saved_views v JOIN agents a ON a.workspace_id = v.workspace_id AND a.id = v.agent_id
WHERE v.workspace_id = $1 AND (v.agent_id = $2 OR v.shared) AND ($3::uuid IS NULL OR v.id = $3)
ORDER BY v.agent_id <> $2, lower(v.name), v.id";

async fn views(
    tx: &mut Tx,
    ws: Uuid,
    me: Uuid,
    id: Option<Uuid>,
) -> Result<Vec<View>, sqlx::Error> {
    let rows: Vec<SqlJson<View>> = sqlx::query_scalar(VIEWS)
        .bind(ws)
        .bind(me)
        .bind(id)
        .fetch_all(&mut **tx)
        .await?;
    Ok(rows.into_iter().map(|SqlJson(v)| v).collect())
}

pub async fn visible(tx: &mut Tx, ws: Uuid, me: Uuid) -> Result<Vec<View>, sqlx::Error> {
    views(tx, ws, me, None).await
}

async fn view(tx: &mut Tx, ws: Uuid, me: Uuid, id: Uuid) -> ApiResult<View> {
    views(tx, ws, me, Some(id))
        .await?
        .pop()
        .ok_or_else(ApiError::not_found)
}

fn valid_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 40 {
        return Err(ApiError::bad_request("invalidName"));
    }
    Ok(name.to_string())
}

fn valid_filters(f: &ViewFilters) -> ApiResult<()> {
    check_filters(f).map_err(|_| ApiError::bad_request("invalidFilter"))
}

/// `GET /api/views`
async fn list(auth: Auth, State(st): State<AppState>) -> ApiResult<Json<ViewsResponse>> {
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    let views = visible(&mut tx, auth.workspace_id, auth.agent_id).await?;
    tx.commit().await?;
    Ok(Json(ViewsResponse { views }))
}

/// `POST /api/views`
async fn create(
    auth: Auth,
    State(st): State<AppState>,
    Body(req): Body<NewView>,
) -> ApiResult<(StatusCode, Json<View>)> {
    let name = valid_name(&req.name)?;
    valid_filters(&req.filters)?;
    let (ws, me) = (auth.workspace_id, auth.agent_id);
    let mut tx = tenant_tx(&st.db, ws).await?;
    // Locks the agent's row so two saves cannot both slip under the limit.
    sqlx::query("SELECT 1 FROM agents WHERE workspace_id = $1 AND id = $2 FOR UPDATE")
        .bind(ws)
        .bind(me)
        .execute(&mut *tx)
        .await?;
    let mine: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM saved_views WHERE workspace_id = $1 AND agent_id = $2",
    )
    .bind(ws)
    .bind(me)
    .fetch_one(&mut *tx)
    .await?;
    if mine >= MAX_VIEWS {
        return Err(ApiError::conflict("tooManyViews"));
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO saved_views (workspace_id, agent_id, name, shared, filters)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(ws)
    .bind(me)
    .bind(name)
    .bind(req.shared)
    .bind(SqlJson(&req.filters))
    .fetch_one(&mut *tx)
    .await?;
    let view = view(&mut tx, ws, me, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(view)))
}

/// Another agent's private view does not exist; their shared one is not ours
/// to change.
async fn own(tx: &mut Tx, ws: Uuid, me: Uuid, id: Uuid) -> ApiResult<()> {
    if view(tx, ws, me, id).await?.created_by.id == me {
        Ok(())
    } else {
        Err(ApiError::new(StatusCode::FORBIDDEN, "notYours"))
    }
}

/// `PATCH /api/views/{id}` — any of name, shared, filters.
async fn update(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Body(req): Body<PatchView>,
) -> ApiResult<Json<View>> {
    let name = req.name.as_deref().map(valid_name).transpose()?;
    if let Some(f) = &req.filters {
        valid_filters(f)?;
    }
    let (ws, me) = (auth.workspace_id, auth.agent_id);
    let mut tx = tenant_tx(&st.db, ws).await?;
    own(&mut tx, ws, me, id).await?;
    sqlx::query(
        "UPDATE saved_views SET name = coalesce($3, name), shared = coalesce($4, shared),
                filters = coalesce($5, filters)
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(id)
    .bind(name)
    .bind(req.shared)
    .bind(req.filters.as_ref().map(SqlJson))
    .execute(&mut *tx)
    .await?;
    let view = view(&mut tx, ws, me, id).await?;
    tx.commit().await?;
    Ok(Json(view))
}

/// `DELETE /api/views/{id}`
async fn remove(
    auth: Auth,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let (ws, me) = (auth.workspace_id, auth.agent_id);
    let mut tx = tenant_tx(&st.db, ws).await?;
    own(&mut tx, ws, me, id).await?;
    sqlx::query("DELETE FROM saved_views WHERE workspace_id = $1 AND id = $2")
        .bind(ws)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

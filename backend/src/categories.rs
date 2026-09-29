//! Spec 004: categories and the categorize job.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch};
use axum::{Json, Router};
use serde_json::{Value, json};
use sqlx::types::Json as SqlJson;
use uuid::Uuid;

use crate::api::{CategoriesResponse, Category, CategorySuggestion, NewCategory, PatchCategory};
use crate::copilot::ticket_state;
use crate::db::{Tx, tenant_tx, unique_violation};
use crate::error::ApiResult;
use crate::jobs::JobResult;
use crate::session::{Auth, Body};
use crate::{ApiError, AppState, jev};

/// Jev's pick is applied at or over this probability (spec 004 §6).
/// Tuned against the override rate in `docs/queries/suggestion-quality.sql`.
pub const CATEGORY_THRESHOLD: f64 = 0.6;
/// Jev's limit for one choice question (spec 004 §4).
const MAX_ACTIVE: i64 = 255;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/categories", get(list).post(create))
        .route("/categories/{id}", patch(update))
}

/// The six starting categories (spec 004 §1), inside the signup transaction.
pub async fn insert_defaults(tx: &mut Tx, ws: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO categories (workspace_id, name, description) VALUES
           ($1, 'Bug', 'Something in the product is broken or behaves wrongly.'),
           ($1, 'Login & access', 'Cannot log in, SSO, passwords, locked or deactivated accounts, permissions.'),
           ($1, 'Billing', 'Invoices, payments, plans, refunds.'),
           ($1, 'How-to', 'How to do something the product already supports.'),
           ($1, 'Feature request', 'Asks for something the product does not do.'),
           ($1, 'Other', 'None of the above.')",
    )
    .bind(ws)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

type Row = (Uuid, String, String, bool);

fn category((id, name, description, archived): Row) -> Category {
    Category {
        id,
        name,
        description,
        archived,
    }
}

/// `GET /api/categories` — active first, then archived, each alphabetical.
async fn list(State(st): State<AppState>, auth: Auth) -> ApiResult<Json<CategoriesResponse>> {
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, name, description, archived FROM categories
         WHERE workspace_id = $1 ORDER BY archived, lower(name)",
    )
    .bind(auth.workspace_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(CategoriesResponse {
        categories: rows.into_iter().map(category).collect(),
    }))
}

fn valid_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 40 {
        return Err(ApiError::bad_request("invalidName"));
    }
    Ok(name.to_string())
}

fn valid_description(description: &str) -> ApiResult<String> {
    let description = description.trim();
    if description.chars().count() > 200 {
        return Err(ApiError::bad_request("invalidDescription"));
    }
    Ok(description.to_string())
}

/// Locks the workspace row so two writers cannot both slip under the limit.
async fn check_room(tx: &mut Tx, ws: Uuid) -> ApiResult<()> {
    sqlx::query("SELECT 1 FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(ws)
        .execute(&mut **tx)
        .await?;
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM categories WHERE workspace_id = $1 AND NOT archived",
    )
    .bind(ws)
    .fetch_one(&mut **tx)
    .await?;
    if active >= MAX_ACTIVE {
        Err(ApiError::conflict("tooManyCategories"))
    } else {
        Ok(())
    }
}

/// Names are unique among active categories, ignoring case (§2).
fn name_taken(e: sqlx::Error) -> ApiError {
    if unique_violation(&e) == Some("categories_active_name") {
        ApiError::conflict("nameTaken")
    } else {
        e.into()
    }
}

/// `POST /api/categories`
async fn create(
    State(st): State<AppState>,
    auth: Auth,
    Body(req): Body<NewCategory>,
) -> ApiResult<(StatusCode, Json<Category>)> {
    auth.require_owner()?;
    let name = valid_name(&req.name)?;
    let description = valid_description(req.description.as_deref().unwrap_or(""))?;
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    check_room(&mut tx, ws).await?;
    let row: Row = sqlx::query_as(
        "INSERT INTO categories (workspace_id, name, description) VALUES ($1, $2, $3)
         RETURNING id, name, description, archived",
    )
    .bind(ws)
    .bind(&name)
    .bind(&description)
    .fetch_one(&mut *tx)
    .await
    .map_err(name_taken)?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(category(row))))
}

/// `PATCH /api/categories/{id}` — any subset of name, description, archived.
/// Existing tickets keep their category either way (§3, §11).
async fn update(
    State(st): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Body(req): Body<PatchCategory>,
) -> ApiResult<Json<Category>> {
    auth.require_owner()?;
    let name = req.name.as_deref().map(valid_name).transpose()?;
    let description = req
        .description
        .as_deref()
        .map(valid_description)
        .transpose()?;
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let archived: bool =
        sqlx::query_scalar("SELECT archived FROM categories WHERE workspace_id = $1 AND id = $2")
            .bind(ws)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(ApiError::not_found)?;
    if archived && req.archived == Some(false) {
        check_room(&mut tx, ws).await?;
    }
    let row: Row = sqlx::query_as(
        "UPDATE categories SET name = coalesce($3, name), description = coalesce($4, description),
                               archived = coalesce($5, archived)
         WHERE workspace_id = $1 AND id = $2
         RETURNING id, name, description, archived",
    )
    .bind(ws)
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(req.archived)
    .fetch_one(&mut *tx)
    .await
    .map_err(name_taken)?;
    tx.commit().await?;
    Ok(Json(category(row)))
}

/// Spec 004 §5–10: one choice question over the active categories.
pub async fn categorize_job(st: &AppState, ws: Uuid, ticket_id: Uuid) -> JobResult {
    let mut tx = tenant_tx(&st.db, ws).await?;
    // Already judged (a job re-run after a crash): nothing to do.
    let judged: Option<bool> = sqlx::query_scalar(
        "SELECT jev_category_probability IS NOT NULL FROM tickets WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(ticket_id)
    .fetch_optional(&mut *tx)
    .await?;
    if judged != Some(false) {
        return Ok(());
    }
    let Some(state) = ticket_state(&mut tx, ws, ticket_id).await? else {
        return Ok(());
    };
    let categories: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, name, description FROM categories
         WHERE workspace_id = $1 AND NOT archived ORDER BY lower(name) LIMIT 255",
    )
    .bind(ws)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    if categories.is_empty() {
        return Ok(()); // §10
    }

    // Criteria are keyed by name — why names are unique among active categories.
    let criteria: serde_json::Map<String, Value> = categories
        .iter()
        .map(|(_, name, description)| {
            let description = if description.is_empty() {
                name
            } else {
                description
            };
            (name.clone(), Value::String(description.clone()))
        })
        .collect();
    let questions = json!({
        "category": {
            "type": "choice",
            "instructions": "Which category does this support ticket belong to?",
            "criteria": criteria,
        }
    });
    let answers = jev::ask(st, state, questions).await?;

    // Jev's probabilities, best first, for the categories that still exist.
    let probabilities = &answers["category"]["probabilities"];
    let mut ranked: Vec<CategorySuggestion> = categories
        .into_iter()
        .filter_map(|(id, name, _)| {
            let probability = probabilities[name.as_str()].as_f64()?;
            Some(CategorySuggestion {
                id,
                name,
                probability,
            })
        })
        .collect();
    ranked.sort_by(|a, b| b.probability.total_cmp(&a.probability));
    let Some(top) = ranked.first() else {
        return Err(crate::jobs::JobError::Fail(format!(
            "Jev answer without probabilities: {answers}"
        )));
    };
    let (top_id, top_p) = (top.id, top.probability);
    let confident = top_p >= CATEGORY_THRESHOLD;
    ranked.truncate(2);
    let chips = if confident { vec![] } else { ranked };

    // Jev's pick is kept whatever happens next (§8); it becomes the category
    // only if confident and no agent has chosen one meanwhile.
    let mut tx = tenant_tx(&st.db, ws).await?;
    sqlx::query(
        "UPDATE tickets SET jev_category_id = $3, jev_category_probability = $4,
             category_source = CASE WHEN $5 AND category_id IS NULL THEN 'jev' ELSE category_source END,
             category_id = CASE WHEN $5 AND category_id IS NULL THEN $3 ELSE category_id END,
             category_suggestions = $6
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(ticket_id)
    .bind(top_id)
    .bind(top_p)
    .bind(confident)
    .bind(SqlJson(&chips))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

//! Spec 001: signup, magic links, sessions, the team (ADR 0006).

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::api::{
    Agent, AgentsResponse, AuthLinkInfo, ConsumeLinkRequest, ConsumeLinkResponse, Invite,
    InviteRequest, LoginRequest, Me, SignupRequest, UpdateMeRequest, Workspace,
};
use crate::db::{Tx, set_tenant, tenant_tx, unique_violation};
use crate::error::ApiResult;
use crate::mail::send_system;
use crate::session::{
    Auth, Body, billing, clear_cookie, hash_token, new_token, normalize_email, session_cookie,
};
use crate::{ApiError, AppState};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/signup", post(signup))
        .route("/login", post(login))
        .route("/auth/link", get(link_info).post(consume_link))
        .route("/logout", post(logout))
        .route("/me", get(me).patch(update_me))
        .route("/agents", get(agents))
        .route("/agents/{id}", delete(remove_agent))
        .route("/invites", post(invite))
}

/// Addresses every mail system reserves (RFC 2142) — not a workspace's inbox.
const RESERVED_SLUGS: &[&str] = &[
    "postmaster",
    "abuse",
    "hostmaster",
    "webmaster",
    "mailer-daemon",
    "noreply",
    "no-reply",
    "root",
    "admin",
];

fn valid_slug(slug: &str) -> bool {
    (3..=32).contains(&slug.len())
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !RESERVED_SLUGS.contains(&slug)
}

/// Trimmed, 1–64 characters (workspace and agent names).
fn valid_name(name: &str) -> Option<&str> {
    let name = name.trim();
    (!name.is_empty() && name.chars().count() <= 64).then_some(name)
}

fn accepted() -> (StatusCode, Json<Value>) {
    (StatusCode::ACCEPTED, Json(json!({})))
}

/// The one agent this address is, in whichever workspace (spec 001 §15).
async fn agent_by_email(
    st: &AppState,
    email: &str,
) -> Result<Option<(Uuid, Uuid, String)>, sqlx::Error> {
    sqlx::query_as("SELECT agent_id, workspace_id, workspace_name FROM agent_by_email($1)")
        .bind(email)
        .fetch_optional(&st.db)
        .await
}

/// Stores a link and returns its token. Signup and login links are capped at
/// 5 per address per hour, counted over every link that address was sent
/// (spec 001 §9); over the cap nothing is stored and `None` comes back, and
/// the caller still answers 202. Invites are not capped: only an owner sends
/// them.
async fn create_link(
    ex: impl PgExecutor<'_>,
    purpose: &str,
    email: &str,
    workspace_id: Option<Uuid>,
    workspace_name: &str,
    slug: Option<&str>,
    language: Option<&str>,
) -> Result<Option<String>, sqlx::Error> {
    let token = new_token();
    let stored = sqlx::query(
        "INSERT INTO auth_links (token_hash, purpose, email, workspace_id, workspace_name, slug, language, expires_at)
         SELECT $1, $2, $3, $4, $5, $6, $7,
                now() + CASE WHEN $2 = 'invite' THEN interval '7 days' ELSE interval '15 minutes' END
         WHERE $2 = 'invite'
            OR (SELECT count(*) FROM auth_links WHERE email = $3 AND created_at > now() - interval '1 hour') < 5",
    )
    .bind(hash_token(&token))
    .bind(purpose)
    .bind(email)
    .bind(workspace_id)
    .bind(workspace_name)
    .bind(slug)
    .bind(language)
    .execute(ex)
    .await?
    .rows_affected();
    Ok((stored == 1).then_some(token))
}

fn link_url(st: &AppState, token: &str) -> String {
    format!("{}/auth?token={token}", st.cfg.app_url)
}

async fn send_login_link(
    st: &AppState,
    email: String,
    (_, ws, ws_name): (Uuid, Uuid, String),
) -> Result<(), sqlx::Error> {
    if let Some(token) =
        create_link(&st.db, "login", &email, Some(ws), &ws_name, None, None).await?
    {
        let text = format!(
            "Open this link and press the button to log in to {ws_name}:\n\n{}\n\n\
             The link works once, for 15 minutes. If you did not ask to log in, ignore this email.\n",
            link_url(st, &token)
        );
        send_system(st, email, format!("Log in to {ws_name} on Muninn"), text);
    }
    Ok(())
}

async fn signup(
    State(st): State<AppState>,
    Body(req): Body<SignupRequest>,
) -> ApiResult<impl IntoResponse> {
    let email = normalize_email(&req.email).ok_or(ApiError::bad_request("invalidEmail"))?;
    let name =
        valid_name(&req.workspace_name).ok_or(ApiError::bad_request("invalidWorkspaceName"))?;
    if !valid_slug(&req.slug) {
        return Err(ApiError::bad_request("invalidSlug"));
    }
    let language_ok: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_ts_config WHERE cfgname = $1)")
            .bind(&req.language)
            .fetch_one(&st.db)
            .await?;
    if !language_ok {
        return Err(ApiError::bad_request("unsupportedLanguage"));
    }
    // Slugs are public addresses, so saying one is taken reveals nothing (§3).
    let taken: Option<Uuid> = sqlx::query_scalar("SELECT workspace_id_by_slug($1)")
        .bind(&req.slug)
        .fetch_one(&st.db)
        .await?;
    if taken.is_some() {
        return Err(ApiError::conflict("slugTaken"));
    }
    // Nothing is created until the link is used (§2). An existing agent gets a
    // login link and the same answer (§4).
    if let Some(agent) = agent_by_email(&st, &email).await? {
        send_login_link(&st, email, agent).await?;
    } else if let Some(token) = create_link(
        &st.db,
        "signup",
        &email,
        None,
        name,
        Some(&req.slug),
        Some(&req.language),
    )
    .await?
    {
        let text = format!(
            "Open this link and press the button to create {name} on Muninn:\n\n{}\n\n\
             The link works once, for 15 minutes. If you did not sign up, ignore this email.\n",
            link_url(&st, &token)
        );
        send_system(&st, email, format!("Create {name} on Muninn"), text);
    }
    Ok(accepted())
}

async fn login(
    State(st): State<AppState>,
    Body(req): Body<LoginRequest>,
) -> ApiResult<impl IntoResponse> {
    let email = normalize_email(&req.email).ok_or(ApiError::bad_request("invalidEmail"))?;
    if let Some(agent) = agent_by_email(&st, &email).await? {
        send_login_link(&st, email, agent).await?;
    }
    Ok(accepted())
}

fn link_expired() -> ApiError {
    ApiError::new(StatusCode::GONE, "linkExpired")
}

/// What the `/auth` page shows. Reading does not consume: link scanners fetch
/// every URL in an email (ADR 0006).
async fn link_info(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<AuthLinkInfo>> {
    let token = q.get("token").map(String::as_str).unwrap_or("");
    let (purpose, workspace_name, email, slug, language) = sqlx::query_as(
        "SELECT purpose, workspace_name, email, slug, language FROM auth_links
         WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()",
    )
    .bind(hash_token(token))
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(link_expired)?;
    Ok(Json(AuthLinkInfo {
        purpose,
        workspace_name,
        email,
        slug,
        language,
    }))
}

#[derive(sqlx::FromRow)]
struct Link {
    purpose: String,
    email: String,
    workspace_id: Option<Uuid>,
    workspace_name: String,
    slug: Option<String>,
    language: Option<String>,
}

/// A lost race is settled by the unique indexes; the transaction rolls back
/// and the link stays unused.
fn taken(e: sqlx::Error) -> ApiError {
    match unique_violation(&e) {
        Some("workspaces_slug_key") => ApiError::conflict("slugTaken"),
        Some("agents_email") => ApiError::conflict("emailTaken"),
        _ => e.into(),
    }
}

/// An agent's display name until they change it (§13).
fn default_name(email: &str) -> &str {
    email.split('@').next().unwrap_or(email)
}

/// The button. Consuming the link, creating what it carries and opening the
/// session is one transaction.
async fn consume_link(
    State(st): State<AppState>,
    Body(req): Body<ConsumeLinkRequest>,
) -> ApiResult<impl IntoResponse> {
    let mut tx: Tx = st.db.begin().await?;
    let link: Link = sqlx::query_as(
        "UPDATE auth_links SET used_at = now()
         WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()
         RETURNING purpose, email, workspace_id, workspace_name, slug, language",
    )
    .bind(hash_token(&req.token))
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(link_expired)?;

    let (ws, agent_id, redirect) = match (link.purpose.as_str(), link.workspace_id) {
        ("signup", _) => {
            let ws = Uuid::new_v4();
            set_tenant(&mut tx, ws).await?;
            sqlx::query(
                "INSERT INTO workspaces (id, name, slug, language, trial_ends_at)
                 VALUES ($1, $2, $3, $4::regconfig, now() + interval '14 days')",
            )
            .bind(ws)
            .bind(&link.workspace_name)
            .bind(&link.slug)
            .bind(&link.language)
            .execute(&mut *tx)
            .await
            .map_err(taken)?;
            let agent_id = insert_agent(&mut tx, ws, &link.email, "owner").await?;
            crate::categories::insert_defaults(&mut tx, ws).await?;
            (ws, agent_id, "/onboarding")
        }
        ("login", Some(ws)) => {
            set_tenant(&mut tx, ws).await?;
            let agent_id: Uuid = sqlx::query_scalar(
                "SELECT id FROM agents WHERE workspace_id = $1 AND email = $2 AND removed_at IS NULL",
            )
            .bind(ws)
            .bind(&link.email)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(link_expired)?;
            (ws, agent_id, "/")
        }
        ("invite", Some(ws)) => {
            set_tenant(&mut tx, ws).await?;
            let agent_id = insert_agent(&mut tx, ws, &link.email, "agent").await?;
            crate::billing::seats_changed(&mut tx, ws).await?;
            (ws, agent_id, "/")
        }
        _ => return Err(link_expired()),
    };

    let token = new_token();
    sqlx::query("INSERT INTO sessions (token_hash, workspace_id, agent_id) VALUES ($1, $2, $3)")
        .bind(hash_token(&token))
        .bind(ws)
        .bind(agent_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((
        [(header::SET_COOKIE, session_cookie(&token))],
        Json(ConsumeLinkResponse {
            redirect: redirect.into(),
        }),
    ))
}

async fn insert_agent(tx: &mut Tx, ws: Uuid, email: &str, role: &str) -> ApiResult<Uuid> {
    sqlx::query_scalar(
        "INSERT INTO agents (workspace_id, email, name, role) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(ws)
    .bind(email)
    .bind(default_name(email))
    .bind(role)
    .fetch_one(&mut **tx)
    .await
    .map_err(taken)
}

/// Ends this session only (§12).
async fn logout(State(st): State<AppState>, auth: Auth) -> ApiResult<impl IntoResponse> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(&auth.token_hash)
        .execute(&st.db)
        .await?;
    Ok((
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, clear_cookie())],
    ))
}

type MeRow = (
    Uuid,
    String,
    String,
    String,
    Uuid,
    String,
    String,
    String,
    String,
    DateTime<Utc>,
);

async fn load_me(st: &AppState, auth: &Auth) -> ApiResult<Me> {
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    let (id, email, name, role, ws, ws_name, slug, language, status, trial_ends_at): MeRow =
        sqlx::query_as(
            "SELECT a.id, a.email, a.name, a.role, w.id, w.name, w.slug, w.language::text,
                    w.billing_status, w.trial_ends_at
             FROM agents a JOIN workspaces w ON w.id = a.workspace_id
             WHERE a.id = $1 AND a.workspace_id = $2",
        )
        .bind(auth.agent_id)
        .bind(auth.workspace_id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Me {
        agent: Agent {
            id,
            email,
            name,
            role,
        },
        workspace: Workspace {
            id: ws,
            name: ws_name,
            inbound_address: st.cfg.inbound_address(&slug),
            slug,
            language,
            billing: billing(&status, trial_ends_at),
        },
    })
}

async fn me(State(st): State<AppState>, auth: Auth) -> ApiResult<Json<Me>> {
    Ok(Json(load_me(&st, &auth).await?))
}

async fn update_me(
    State(st): State<AppState>,
    auth: Auth,
    Body(req): Body<UpdateMeRequest>,
) -> ApiResult<Json<Me>> {
    let name = valid_name(&req.name).ok_or(ApiError::bad_request("invalidName"))?;
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    sqlx::query("UPDATE agents SET name = $3 WHERE id = $1 AND workspace_id = $2")
        .bind(auth.agent_id)
        .bind(auth.workspace_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(load_me(&st, &auth).await?))
}

async fn agents(State(st): State<AppState>, auth: Auth) -> ApiResult<Json<AgentsResponse>> {
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    let agents: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "SELECT id, email, name, role FROM agents
         WHERE workspace_id = $1 AND removed_at IS NULL ORDER BY created_at",
    )
    .bind(auth.workspace_id)
    .fetch_all(&mut *tx)
    .await?;
    // auth_links has no RLS (ADR 0003 exception): the filter is the only guard.
    let invites: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT email, expires_at FROM auth_links
         WHERE workspace_id = $1 AND purpose = 'invite' AND used_at IS NULL AND expires_at > now()
         ORDER BY created_at",
    )
    .bind(auth.workspace_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(AgentsResponse {
        agents: agents
            .into_iter()
            .map(|(id, email, name, role)| Agent {
                id,
                email,
                name,
                role,
            })
            .collect(),
        invites: invites
            .into_iter()
            .map(|(email, expires_at)| Invite { email, expires_at })
            .collect(),
    }))
}

async fn invite(
    State(st): State<AppState>,
    auth: Auth,
    Body(req): Body<InviteRequest>,
) -> ApiResult<impl IntoResponse> {
    auth.require_owner()?;
    let email = normalize_email(&req.email).ok_or(ApiError::bad_request("invalidEmail"))?;
    if agent_by_email(&st, &email).await?.is_some() {
        return Err(ApiError::conflict("emailTaken"));
    }
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    let (ws_name, inviter): (String, String) = sqlx::query_as(
        "SELECT w.name, a.name FROM workspaces w JOIN agents a ON a.workspace_id = w.id
         WHERE w.id = $1 AND a.id = $2",
    )
    .bind(auth.workspace_id)
    .bind(auth.agent_id)
    .fetch_one(&mut *tx)
    .await?;
    // A new invite to the same address replaces the pending one.
    sqlx::query(
        "DELETE FROM auth_links
         WHERE workspace_id = $1 AND purpose = 'invite' AND email = $2 AND used_at IS NULL",
    )
    .bind(auth.workspace_id)
    .bind(&email)
    .execute(&mut *tx)
    .await?;
    let token = create_link(
        &mut *tx,
        "invite",
        &email,
        Some(auth.workspace_id),
        &ws_name,
        None,
        None,
    )
    .await?
    .ok_or_else(ApiError::internal)?;
    let expires_at: DateTime<Utc> =
        sqlx::query_scalar("SELECT expires_at FROM auth_links WHERE token_hash = $1")
            .bind(hash_token(&token))
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;

    let text = format!(
        "{inviter} invited you to join {ws_name} on Muninn, the help desk.\n\n\
         Open this link and press the button to join:\n\n{}\n\nThe link works once, for 7 days.\n",
        link_url(&st, &token)
    );
    send_system(
        &st,
        email.clone(),
        format!("{inviter} invited you to join {ws_name} on Muninn"),
        text,
    );
    Ok((StatusCode::CREATED, Json(Invite { email, expires_at })))
}

/// Their sessions end now and their open tickets become unassigned (§16).
async fn remove_agent(
    State(st): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    auth.require_owner()?;
    let mut tx = tenant_tx(&st.db, auth.workspace_id).await?;
    let role: String = sqlx::query_scalar(
        "SELECT role FROM agents WHERE id = $1 AND workspace_id = $2 AND removed_at IS NULL",
    )
    .bind(id)
    .bind(auth.workspace_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(ApiError::not_found)?;
    if role == "owner" {
        return Err(ApiError::conflict("cannotRemoveOwner"));
    }
    sqlx::query("UPDATE agents SET removed_at = now() WHERE id = $1 AND workspace_id = $2")
        .bind(id)
        .bind(auth.workspace_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE agent_id = $1 AND workspace_id = $2")
        .bind(id)
        .bind(auth.workspace_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE tickets SET owner_id = NULL
         WHERE workspace_id = $1 AND owner_id = $2 AND status <> 'closed'",
    )
    .bind(auth.workspace_id)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    crate::billing::seats_changed(&mut tx, auth.workspace_id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        for ok in ["acme", "acme-support", "a1b", "x".repeat(32).as_str()] {
            assert!(valid_slug(ok), "{ok}");
        }
        for bad in [
            "ab",
            "Acme",
            "acme_1",
            "acme.no",
            "æøå",
            "postmaster",
            "abuse",
            &"x".repeat(33),
        ] {
            assert!(!valid_slug(bad), "{bad}");
        }
    }
}

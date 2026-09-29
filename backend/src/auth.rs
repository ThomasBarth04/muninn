//! Spec 001: login with a password and an authenticator (ADR 0011), the links
//! that set them, sessions, the team.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::api::{
    Agent, AgentsResponse, AuthLinkInfo, ChangePasswordRequest, Invite, InviteRequest,
    LoginRequest, LoginResponse, Me, PasswordResetRequest, Totp, UpdateMeRequest, UseLinkRequest,
    Workspace,
};
use crate::credentials::{
    base32, check_password, hash_password, new_secret, otpauth_uri, qr_svg, valid_password,
    verify_code,
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
        .route("/login", post(login))
        .route("/password-reset", post(password_reset))
        .route("/auth/link", get(link_info).post(use_link))
        .route("/logout", post(logout))
        .route("/me", get(me).patch(update_me))
        .route("/me/password", post(change_password))
        .route("/agents", get(agents))
        .route("/agents/{id}", delete(remove_agent))
        .route("/agents/{id}/reset-login", post(reset_agent_login))
        .route("/invites", post(invite))
}

/// Trimmed, 1–64 characters (workspace and agent names).
pub fn valid_name(name: &str) -> Option<&str> {
    let name = name.trim();
    (!name.is_empty() && name.chars().count() <= 64).then_some(name)
}

fn accepted() -> (StatusCode, Json<Value>) {
    (StatusCode::ACCEPTED, Json(json!({})))
}

/// The one agent this address is, in whichever workspace (spec 001 §15).
pub async fn agent_by_email(
    st: &AppState,
    email: &str,
) -> Result<Option<(Uuid, Uuid, String)>, sqlx::Error> {
    sqlx::query_as("SELECT agent_id, workspace_id, workspace_name FROM agent_by_email($1)")
        .bind(email)
        .fetch_optional(&st.db)
        .await
}

/// Stores a link and returns its token (spec 001 §5, §9). `invite` and
/// `setup` offer a new authenticator and replace any pending one to the same
/// address; `reset` keeps the agent's own and is capped at 5 per address per
/// hour — over the cap nothing is stored and `None` comes back.
pub async fn create_link(
    tx: &mut Tx,
    purpose: &str,
    email: &str,
    ws: Uuid,
    workspace_name: &str,
) -> Result<Option<String>, sqlx::Error> {
    if purpose != "reset" {
        // auth_links has no RLS (ADR 0003 exception): the filter is the only guard.
        sqlx::query(
            "DELETE FROM auth_links
             WHERE workspace_id = $1 AND email = $2 AND purpose IN ('invite', 'setup') AND used_at IS NULL",
        )
        .bind(ws)
        .bind(email)
        .execute(&mut **tx)
        .await?;
    }
    let token = new_token();
    let stored = sqlx::query(
        "INSERT INTO auth_links (token_hash, purpose, email, workspace_id, workspace_name, totp_secret, expires_at)
         SELECT $1, $2, $3, $4, $5, $6,
                now() + CASE WHEN $2 = 'reset' THEN interval '1 hour' ELSE interval '7 days' END
         WHERE $2 <> 'reset'
            OR (SELECT count(*) FROM auth_links
                WHERE email = $3 AND purpose = 'reset' AND created_at > now() - interval '1 hour') < 5",
    )
    .bind(hash_token(&token))
    .bind(purpose)
    .bind(email)
    .bind(ws)
    .bind(workspace_name)
    .bind((purpose != "reset").then(new_secret))
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok((stored == 1).then_some(token))
}

/// Subject and text of the email that carries a link.
pub fn link_email(
    st: &AppState,
    purpose: &str,
    token: &str,
    workspace_name: &str,
    inviter: Option<&str>,
) -> (String, String) {
    let url = format!("{}/auth?token={token}", st.cfg.app_url);
    match purpose {
        "invite" => {
            let inviter = inviter.unwrap_or("Your colleague");
            (
                format!("{inviter} invited you to join {workspace_name} on Muninn"),
                format!(
                    "{inviter} invited you to join {workspace_name} on Muninn, the help desk.\n\n\
                     Open this link to choose a password and set up an authenticator app:\n\n{url}\n\n\
                     The link works once, for 7 days.\n"
                ),
            )
        }
        "setup" => (
            format!("Set up your login to {workspace_name} on Muninn"),
            format!(
                "Open this link to choose a password and set up an authenticator app for \
                 {workspace_name} on Muninn:\n\n{url}\n\nThe link works once, for 7 days.\n"
            ),
        ),
        _ => (
            "Reset your Muninn password".to_string(),
            format!(
                "Open this link to choose a new password for {workspace_name} on Muninn. \
                 You will need your authenticator app.\n\n{url}\n\n\
                 The link works once, for one hour. If you did not ask for this, ignore this email.\n"
            ),
        ),
    }
}

fn invalid_credentials() -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, "invalidCredentials")
}

fn link_expired() -> ApiError {
    ApiError::new(StatusCode::GONE, "linkExpired")
}

/// Opens a session: the cookie, and where to land (spec 001 §3) — an owner
/// whose workspace has no ticket yet goes to onboarding.
async fn start_session(
    tx: &mut Tx,
    ws: Uuid,
    agent_id: Uuid,
) -> ApiResult<(String, LoginResponse)> {
    let token = new_token();
    sqlx::query("INSERT INTO sessions (token_hash, workspace_id, agent_id) VALUES ($1, $2, $3)")
        .bind(hash_token(&token))
        .bind(ws)
        .bind(agent_id)
        .execute(&mut **tx)
        .await?;
    let onboarding: bool = sqlx::query_scalar(
        "SELECT role = 'owner' AND NOT EXISTS (SELECT 1 FROM tickets WHERE workspace_id = $1)
         FROM agents WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(agent_id)
    .fetch_one(&mut **tx)
    .await?;
    let redirect = if onboarding { "/onboarding" } else { "/" };
    Ok((
        session_cookie(&token),
        LoginResponse {
            redirect: redirect.into(),
        },
    ))
}

/// Counts a failure towards §8's limit. Its own transaction, so it sticks
/// whatever the request does next.
async fn record_failure(st: &AppState, ws: Uuid, agent_id: Uuid) -> Result<(), sqlx::Error> {
    let mut tx = tenant_tx(&st.db, ws).await?;
    sqlx::query(
        "UPDATE agents SET
             failed_logins = CASE WHEN last_failed_login_at > now() - interval '15 minutes'
                                  THEN failed_logins + 1 ELSE 1 END,
             last_failed_login_at = now()
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(agent_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

const LOCKED: &str = "failed_logins >= 10 AND last_failed_login_at > now() - interval '15 minutes'";

/// `POST /api/login` — §4, §7, §8.
async fn login(
    State(st): State<AppState>,
    Body(req): Body<LoginRequest>,
) -> ApiResult<impl IntoResponse> {
    let found = match normalize_email(&req.email) {
        Some(email) => agent_by_email(&st, &email).await?,
        None => None,
    };
    let Some((agent_id, ws, _)) = found else {
        check_password(req.password, None).await;
        return Err(invalid_credentials());
    };
    let mut tx = tenant_tx(&st.db, ws).await?;
    // Locked for the whole check: two guesses cannot both slip under the
    // limit, and one code cannot be used twice.
    let (hash, secret, last_step, locked): (Option<String>, Option<Vec<u8>>, Option<i64>, bool) =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT password_hash, totp_secret, totp_last_step, {LOCKED}
             FROM agents WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
        )))
        .bind(ws)
        .bind(agent_id)
        .fetch_one(&mut *tx)
        .await?;
    if locked {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "tooManyAttempts",
        ));
    }
    let password_ok = check_password(req.password, hash).await;
    let step = secret
        .as_deref()
        .and_then(|s| verify_code(s, &req.code, Utc::now().timestamp(), last_step));
    let Some(step) = step.filter(|_| password_ok) else {
        tx.rollback().await?;
        record_failure(&st, ws, agent_id).await?;
        return Err(invalid_credentials());
    };
    sqlx::query(
        "UPDATE agents SET failed_logins = 0, totp_last_step = $3 WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(agent_id)
    .bind(step)
    .execute(&mut *tx)
    .await?;
    let (cookie, body) = start_session(&mut tx, ws, agent_id).await?;
    tx.commit().await?;
    Ok(([(header::SET_COOKIE, cookie)], Json(body)))
}

/// `POST /api/password-reset` — §9. Always 202; the mail goes after.
async fn password_reset(
    State(st): State<AppState>,
    Body(req): Body<PasswordResetRequest>,
) -> ApiResult<impl IntoResponse> {
    let email = normalize_email(&req.email).ok_or(ApiError::bad_request("invalidEmail"))?;
    if let Some((agent_id, ws, ws_name)) = agent_by_email(&st, &email).await? {
        let mut tx = tenant_tx(&st.db, ws).await?;
        let has_password: bool = sqlx::query_scalar(
            "SELECT password_hash IS NOT NULL FROM agents WHERE workspace_id = $1 AND id = $2",
        )
        .bind(ws)
        .bind(agent_id)
        .fetch_one(&mut *tx)
        .await?;
        let token = if has_password {
            create_link(&mut tx, "reset", &email, ws, &ws_name).await?
        } else {
            None
        };
        tx.commit().await?;
        if let Some(token) = token {
            let (subject, text) = link_email(&st, "reset", &token, &ws_name, None);
            send_system(&st, email, subject, text);
        }
    }
    Ok(accepted())
}

/// `GET /api/auth/link` — what the `/auth` page shows. Reading does not use
/// the link: link scanners fetch every URL in an email.
async fn link_info(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<AuthLinkInfo>> {
    let token = q.get("token").map(String::as_str).unwrap_or("");
    let (purpose, workspace_name, email, secret): (String, String, String, Option<Vec<u8>>) =
        sqlx::query_as(
            "SELECT purpose, workspace_name, email, totp_secret FROM auth_links
             WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()",
        )
        .bind(hash_token(token))
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(link_expired)?;
    let totp = secret.map(|secret| {
        let uri = otpauth_uri(&email, &secret);
        Totp {
            secret: base32(&secret),
            qr_svg: qr_svg(&uri),
            uri,
        }
    });
    Ok(Json(AuthLinkInfo {
        purpose,
        workspace_name,
        email,
        totp,
    }))
}

#[derive(sqlx::FromRow)]
struct Link {
    purpose: String,
    email: String,
    workspace_id: Uuid,
    totp_secret: Option<Vec<u8>>,
}

fn email_taken(e: sqlx::Error) -> ApiError {
    if unique_violation(&e) == Some("agents_email") {
        ApiError::conflict("emailTaken")
    } else {
        e.into()
    }
}

/// `POST /api/auth/link` — §5, §6, §9, §14. A wrong password or code leaves
/// the link as it was.
async fn use_link(
    State(st): State<AppState>,
    Body(req): Body<UseLinkRequest>,
) -> ApiResult<impl IntoResponse> {
    let mut tx: Tx = st.db.begin().await?;
    let link: Link = sqlx::query_as(
        "SELECT purpose, email, workspace_id, totp_secret FROM auth_links
         WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now() FOR UPDATE",
    )
    .bind(hash_token(&req.token))
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(link_expired)?;
    if !valid_password(&req.password) {
        return Err(ApiError::bad_request("invalidPassword"));
    }
    let ws = link.workspace_id;
    set_tenant(&mut tx, ws).await?;
    let now = Utc::now().timestamp();
    let invalid_code = || ApiError::bad_request("invalidCode");

    let (agent_id, step) = match (link.purpose.as_str(), &link.totp_secret) {
        // A new authenticator: the code proves the QR code was scanned.
        ("invite" | "setup", Some(secret)) => {
            let step = verify_code(secret, &req.code, now, None).ok_or_else(invalid_code)?;
            let agent_id = if link.purpose == "invite" {
                let id = insert_agent(&mut tx, ws, &link.email, "agent")
                    .await
                    .map_err(email_taken)?;
                crate::billing::seats_changed(&mut tx, ws).await?;
                id
            } else {
                sqlx::query_scalar(
                    "SELECT id FROM agents WHERE workspace_id = $1 AND email = $2 AND removed_at IS NULL",
                )
                .bind(ws)
                .bind(&link.email)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(link_expired)?
            };
            sqlx::query("UPDATE agents SET totp_secret = $3 WHERE workspace_id = $1 AND id = $2")
                .bind(ws)
                .bind(agent_id)
                .bind(secret)
                .execute(&mut *tx)
                .await?;
            (agent_id, step)
        }
        // Forgot password: the mailbox alone is not enough, the current
        // authenticator must agree — and guessing it counts as a failed login.
        ("reset", None) => {
            let (agent_id, secret, last_step, locked): (Uuid, Option<Vec<u8>>, Option<i64>, bool) =
                sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "SELECT id, totp_secret, totp_last_step, {LOCKED} FROM agents
                     WHERE workspace_id = $1 AND email = $2 AND removed_at IS NULL FOR UPDATE"
                )))
                .bind(ws)
                .bind(&link.email)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(link_expired)?;
            if locked {
                return Err(ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "tooManyAttempts",
                ));
            }
            // The login was reset meanwhile: a setup link is on its way instead.
            let secret = secret.ok_or_else(link_expired)?;
            let Some(step) = verify_code(&secret, &req.code, now, last_step) else {
                tx.rollback().await?;
                record_failure(&st, ws, agent_id).await?;
                return Err(invalid_code());
            };
            sqlx::query("DELETE FROM sessions WHERE workspace_id = $1 AND agent_id = $2")
                .bind(ws)
                .bind(agent_id)
                .execute(&mut *tx)
                .await?;
            (agent_id, step)
        }
        _ => return Err(link_expired()),
    };

    let hash = hash_password(req.password).await;
    sqlx::query(
        "UPDATE agents SET password_hash = $3, totp_last_step = $4, failed_logins = 0
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(agent_id)
    .bind(hash)
    .bind(step)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE auth_links SET used_at = now() WHERE token_hash = $1")
        .bind(hash_token(&req.token))
        .execute(&mut *tx)
        .await?;
    let (cookie, body) = start_session(&mut tx, ws, agent_id).await?;
    tx.commit().await?;
    Ok(([(header::SET_COOKIE, cookie)], Json(body)))
}

pub async fn insert_agent(
    tx: &mut Tx,
    ws: Uuid,
    email: &str,
    role: &str,
) -> Result<Uuid, sqlx::Error> {
    // The display name until they change it (§13).
    let name = email.split('@').next().unwrap_or(email);
    sqlx::query_scalar(
        "INSERT INTO agents (workspace_id, email, name, role) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(ws)
    .bind(email)
    .bind(name)
    .bind(role)
    .fetch_one(&mut **tx)
    .await
}

/// §10: both factors cleared, sessions and pending links gone, a new setup
/// link. Returns the address and the link's token; the caller sends it.
pub async fn reset_login(
    tx: &mut Tx,
    ws: Uuid,
    agent_id: Uuid,
) -> Result<Option<(String, String, String)>, sqlx::Error> {
    let Some((email, ws_name)): Option<(String, String)> = sqlx::query_as(
        "SELECT a.email, w.name FROM agents a JOIN workspaces w ON w.id = a.workspace_id
         WHERE a.workspace_id = $1 AND a.id = $2 AND a.removed_at IS NULL",
    )
    .bind(ws)
    .bind(agent_id)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(None);
    };
    sqlx::query(
        "UPDATE agents SET password_hash = NULL, totp_secret = NULL, totp_last_step = NULL,
                failed_logins = 0, last_failed_login_at = NULL
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(agent_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM sessions WHERE workspace_id = $1 AND agent_id = $2")
        .bind(ws)
        .bind(agent_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "DELETE FROM auth_links WHERE workspace_id = $1 AND email = $2 AND used_at IS NULL",
    )
    .bind(ws)
    .bind(&email)
    .execute(&mut **tx)
    .await?;
    let token = create_link(tx, "setup", &email, ws, &ws_name)
        .await?
        .expect("setup links are not capped");
    Ok(Some((email, ws_name, token)))
}

/// `POST /api/agents/{id}/reset-login` — §10. The owner's own is the operator's.
async fn reset_agent_login(
    State(st): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    auth.require_owner()?;
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let role: String = sqlx::query_scalar(
        "SELECT role FROM agents WHERE id = $1 AND workspace_id = $2 AND removed_at IS NULL",
    )
    .bind(id)
    .bind(ws)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(ApiError::not_found)?;
    if role == "owner" {
        return Err(ApiError::conflict("cannotResetOwner"));
    }
    let (email, ws_name, token) = reset_login(&mut tx, ws, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    tx.commit().await?;
    let (subject, text) = link_email(&st, "setup", &token, &ws_name, None);
    send_system(&st, email, subject, text);
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/me/password` — §11. This session stays; the others end.
async fn change_password(
    State(st): State<AppState>,
    auth: Auth,
    Body(req): Body<ChangePasswordRequest>,
) -> ApiResult<StatusCode> {
    let ws = auth.workspace_id;
    let mut tx = tenant_tx(&st.db, ws).await?;
    let hash: Option<String> = sqlx::query_scalar(
        "SELECT password_hash FROM agents WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(ws)
    .bind(auth.agent_id)
    .fetch_one(&mut *tx)
    .await?;
    if !check_password(req.current_password, hash).await {
        return Err(ApiError::bad_request("wrongPassword"));
    }
    if !valid_password(&req.new_password) {
        return Err(ApiError::bad_request("invalidPassword"));
    }
    sqlx::query("UPDATE agents SET password_hash = $3 WHERE workspace_id = $1 AND id = $2")
        .bind(ws)
        .bind(auth.agent_id)
        .bind(hash_password(req.new_password).await)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM sessions WHERE workspace_id = $1 AND agent_id = $2 AND token_hash <> $3",
    )
    .bind(ws)
    .bind(auth.agent_id)
    .bind(&auth.token_hash)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
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

/// `POST /api/invites` — §14, §15.
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
    let token = create_link(&mut tx, "invite", &email, auth.workspace_id, &ws_name)
        .await?
        .ok_or_else(ApiError::internal)?;
    let expires_at: DateTime<Utc> =
        sqlx::query_scalar("SELECT expires_at FROM auth_links WHERE token_hash = $1")
            .bind(hash_token(&token))
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;

    let (subject, text) = link_email(&st, "invite", &token, &ws_name, Some(&inviter));
    send_system(&st, email.clone(), subject, text);
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

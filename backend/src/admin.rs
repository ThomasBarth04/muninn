//! `muninn admin …` — the operator's commands on the box (spec 001 §28,
//! spec 005).
//! Every command returns the line to print, or the refusal; nothing is half
//! done on a refusal.

use chrono::{Datelike, Months, NaiveDate, TimeZone, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::{agent_by_email, create_link, insert_agent, link_email, reset_login, valid_name};
use crate::db::{set_tenant, tenant_tx, unique_violation};
use crate::mail::{Email, send};
use crate::session::normalize_email;
use crate::{AppState, categories};

pub const USAGE: &str = "usage:
  muninn admin create-workspace --name <name> --slug <slug> --language <language> --owner <email>
  muninn admin reset-login <email>
  muninn admin seats [--month YYYY-MM]
  muninn admin pause <slug>
  muninn admin resume <slug>
  muninn admin import <slug> <export.mbox>... --team <address or @domain>...";

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

type Outcome = Result<String, String>;

fn db(e: sqlx::Error) -> String {
    format!("database: {e}")
}

/// `seats` reads across workspaces and needs the owner role; the rest run as
/// the app role through the tenant helper.
pub async fn run(st: &AppState, owner_db: Option<&PgPool>, args: &[String]) -> Outcome {
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    };
    let arg = |i: usize| args.get(i).map(String::as_str).ok_or(USAGE.to_string());
    match args.first().map(String::as_str) {
        Some("create-workspace") => {
            let need = |name| flag(name).ok_or(USAGE.to_string());
            create_workspace(
                st,
                need("--name")?,
                need("--slug")?,
                need("--language")?,
                need("--owner")?,
            )
            .await
        }
        Some("reset-login") => reset(st, arg(1)?).await,
        Some("seats") => {
            let owner_db = owner_db.ok_or("seats needs MIGRATE_DATABASE_URL (the owner role)")?;
            seats(owner_db, flag("--month")).await
        }
        Some(cmd @ ("pause" | "resume")) => set_paused(st, arg(1)?, cmd == "pause").await,
        Some("import") => {
            // Spec 005: positional slug and files, `--team` as often as needed.
            let (mut team, mut positional) = (vec![], vec![]);
            let mut rest = args[1..].iter();
            while let Some(a) = rest.next() {
                if a == "--team" {
                    team.push(rest.next().ok_or(USAGE.to_string())?.clone());
                } else {
                    positional.push(a.clone());
                }
            }
            let (slug, files) = positional.split_first().ok_or(USAGE.to_string())?;
            if files.is_empty() {
                return Err(USAGE.into());
            }
            crate::import::run(st, slug, files, &team).await
        }
        _ => Err(USAGE.into()),
    }
}

/// Emailed now and awaited: the command's process ends right after.
async fn email_link(st: &AppState, to: &str, subject: &str, text: &str) -> Result<(), String> {
    let email = Email {
        from: &st.cfg.mail_from,
        to,
        reply_to: None,
        subject,
        text,
        stream: &st.cfg.postmark_system_stream,
        headers: vec![],
    };
    send(st, &email).await.map_err(|e| {
        format!("could not email the setup link ({e:?}); run `muninn admin reset-login {to}` to send a new one")
    })
}

/// §1–2: workspace, owner and default categories in one transaction, active
/// from the start; then the owner's setup link.
pub async fn create_workspace(
    st: &AppState,
    name: &str,
    slug: &str,
    language: &str,
    owner: &str,
) -> Outcome {
    let name = valid_name(name).ok_or("invalid workspace name")?;
    if !valid_slug(slug) {
        return Err("invalid slug".into());
    }
    let email = normalize_email(owner).ok_or("invalid email")?;
    let language_ok: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_ts_config WHERE cfgname = $1)")
            .bind(language)
            .fetch_one(&st.db)
            .await
            .map_err(db)?;
    if !language_ok {
        return Err("unsupported language".into());
    }
    let already = |email: &str| format!("{email} is already an agent");
    if agent_by_email(st, &email).await.map_err(db)?.is_some() {
        return Err(already(&email));
    }

    let ws = Uuid::new_v4();
    let mut tx = st.db.begin().await.map_err(db)?;
    set_tenant(&mut tx, ws).await.map_err(db)?;
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, language, trial_ends_at, billing_status)
         VALUES ($1, $2, $3, $4::regconfig, now(), 'active')",
    )
    .bind(ws)
    .bind(name)
    .bind(slug)
    .bind(language)
    .execute(&mut *tx)
    .await
    .map_err(|e| match unique_violation(&e) {
        Some("workspaces_slug_key") => "slug taken".to_string(),
        _ => db(e),
    })?;
    insert_agent(&mut tx, ws, &email, "owner")
        .await
        .map_err(|e| match unique_violation(&e) {
            Some("agents_email") => already(&email),
            _ => db(e),
        })?;
    categories::insert_defaults(&mut tx, ws).await.map_err(db)?;
    let token = create_link(&mut tx, "setup", &email, ws, name)
        .await
        .map_err(db)?
        .expect("setup links are not capped");
    tx.commit().await.map_err(db)?;

    let (subject, text) = link_email(st, "setup", &token, name, None);
    email_link(st, &email, &subject, &text).await?;
    Ok(format!(
        "created {slug} — owner {email} (setup link emailed), inbound {}",
        st.cfg.inbound_address(slug)
    ))
}

/// §10, for any agent — the only way to reset an owner's login.
async fn reset(st: &AppState, email: &str) -> Outcome {
    let email = normalize_email(email).ok_or("invalid email")?;
    let (agent_id, ws, _) = agent_by_email(st, &email)
        .await
        .map_err(db)?
        .ok_or(format!("no agent {email}"))?;
    let mut tx = tenant_tx(&st.db, ws).await.map_err(db)?;
    let slug: String = sqlx::query_scalar("SELECT slug FROM workspaces WHERE id = $1")
        .bind(ws)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    let (email, ws_name, token) = reset_login(&mut tx, ws, agent_id)
        .await
        .map_err(db)?
        .ok_or(format!("no agent {email}"))?;
    tx.commit().await.map_err(db)?;
    let (subject, text) = link_email(st, "setup", &token, &ws_name, None);
    email_link(st, &email, &subject, &text).await?;
    Ok(format!("reset {email} in {slug} (setup link emailed)"))
}

/// §20: paused is `canceled`, which locks the workspace (§21).
async fn set_paused(st: &AppState, slug: &str, pause: bool) -> Outcome {
    let ws: Option<Uuid> = sqlx::query_scalar("SELECT workspace_id_by_slug($1)")
        .bind(slug)
        .fetch_one(&st.db)
        .await
        .map_err(db)?;
    let ws = ws.ok_or(format!("no workspace {slug}"))?;
    let mut tx = tenant_tx(&st.db, ws).await.map_err(db)?;
    sqlx::query("UPDATE workspaces SET billing_status = $2 WHERE id = $1")
        .bind(ws)
        .bind(if pause { "canceled" } else { "active" })
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(format!(
        "{} {slug}",
        if pause { "paused" } else { "resumed" }
    ))
}

/// `2026-10` → the first moment of October and of November, UTC.
fn month_bounds(
    month: Option<&str>,
) -> Result<(chrono::DateTime<Utc>, chrono::DateTime<Utc>), String> {
    let start = match month {
        None => {
            let today = Utc::now().date_naive();
            NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
        }
        Some(m) if m.len() == 7 => NaiveDate::parse_from_str(&format!("{m}-01"), "%Y-%m-%d").ok(),
        Some(_) => None,
    }
    .ok_or("invalid month")?;
    let end = start
        .checked_add_months(Months::new(1))
        .ok_or("invalid month")?;
    let utc = |d: NaiveDate| Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight"));
    Ok((utc(start), utc(end)))
}

/// §18: an agent at any moment of the month is one seat. Tab-separated, one
/// workspace per line, for a spreadsheet.
pub async fn seats(owner_db: &PgPool, month: Option<&str>) -> Outcome {
    let (start, end) = month_bounds(month)?;
    let rows: Vec<(String, String, String, String, i64)> = sqlx::query_as(
        "SELECT w.slug, w.name,
                coalesce((SELECT a.email FROM agents a
                          WHERE a.workspace_id = w.id AND a.role = 'owner' AND a.removed_at IS NULL
                          ORDER BY a.created_at LIMIT 1), ''),
                CASE w.billing_status WHEN 'canceled' THEN 'paused' ELSE w.billing_status END,
                (SELECT count(*) FROM agents a
                 WHERE a.workspace_id = w.id AND a.created_at < $2
                   AND (a.removed_at IS NULL OR a.removed_at >= $1))
         FROM workspaces w WHERE w.created_at < $2 ORDER BY w.slug",
    )
    .bind(start)
    .bind(end)
    .fetch_all(owner_db)
    .await
    .map_err(db)?;
    let mut out = String::from("slug\tworkspace\towner\tstatus\tseats");
    for (slug, name, owner, status, seats) in rows {
        out.push_str(&format!("\n{slug}\t{name}\t{owner}\t{status}\t{seats}"));
    }
    Ok(out)
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

    #[test]
    fn months() {
        let (start, end) = month_bounds(Some("2026-12")).unwrap();
        assert_eq!(start.to_rfc3339(), "2026-12-01T00:00:00+00:00");
        assert_eq!(end.to_rfc3339(), "2027-01-01T00:00:00+00:00");
        for bad in ["2026-13", "2026-1", "26-10", "october", "2026-10-01"] {
            assert!(month_bounds(Some(bad)).is_err(), "{bad}");
        }
        assert!(month_bounds(None).is_ok());
    }
}

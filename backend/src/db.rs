//! The tenant helper (ADR 0003). Every read or write of tenant data goes
//! through `tenant_tx`, and every query in it *also* filters on
//! `workspace_id` — RLS is the backstop, not the filter.

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

pub type Tx = Transaction<'static, Postgres>;

pub async fn tenant_tx(db: &PgPool, workspace_id: Uuid) -> Result<Tx, sqlx::Error> {
    let mut tx = db.begin().await?;
    set_tenant(&mut tx, workspace_id).await?;
    Ok(tx)
}

/// For a transaction that learns its tenant partway through — consuming a
/// signup link creates the workspace it then writes into.
/// `SET LOCAL` semantics: gone at commit, so a pooled connection cannot
/// carry one request's workspace into the next.
pub async fn set_tenant(tx: &mut Tx, workspace_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('app.workspace_id', $1, true)")
        .bind(workspace_id.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// The name of the unique index or constraint a statement violated, if that
/// is why it failed. Races (slug taken, email taken, duplicate Message-ID) are
/// settled by the database, not by a check-then-insert.
pub fn unique_violation(e: &sqlx::Error) -> Option<&str> {
    match e {
        sqlx::Error::Database(d) if d.is_unique_violation() => Some(d.constraint().unwrap_or("")),
        _ => None,
    }
}

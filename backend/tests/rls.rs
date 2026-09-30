//! ADR 0003's proof: two workspaces, a query that forgets its filter, zero
//! foreign rows.

mod common;

use muninn::db::tenant_tx;
use uuid::Uuid;

async fn workspace_with_ticket(owner: &sqlx::PgPool, slug: &str) -> Uuid {
    let ws = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces (id, name, slug, language, trial_ends_at) VALUES ($1, $2, $2, 'english', now())")
    .bind(ws)
    .bind(slug)
    .execute(owner)
    .await
    .unwrap();
    sqlx::query(
        "WITH c AS (INSERT INTO contacts (workspace_id, email) VALUES ($1, 'ola@kunde.no') RETURNING id)
         INSERT INTO tickets (workspace_id, token, subject, contact_id) SELECT $1, $2, 'secret', id FROM c",
    )
    .bind(ws)
    .bind(Uuid::new_v4().simple().to_string())
    .execute(owner)
    .await
    .unwrap();
    ws
}

#[tokio::test]
async fn a_forgotten_filter_returns_nothing_foreign() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let a = workspace_with_ticket(&app.owner, "acme").await;
    let b = workspace_with_ticket(&app.owner, "globex").await;

    let mut tx = tenant_tx(&app.st.db, b).await.unwrap();
    // No WHERE workspace_id — the bug RLS exists for.
    let seen: Vec<Uuid> = sqlx::query_scalar("SELECT workspace_id FROM tickets")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(seen, vec![b]);
    let workspaces: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM workspaces")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(workspaces, vec![b]);
    // Writing into another tenant is refused, not silently allowed.
    let foreign = sqlx::query("INSERT INTO contacts (workspace_id, email) VALUES ($1, 'x@y.no')")
        .bind(a)
        .execute(&mut *tx)
        .await;
    assert!(foreign.is_err());
    tx.rollback().await.unwrap();

    // Outside the tenant helper the app sees nothing at all.
    let none: i64 = sqlx::query_scalar("SELECT count(*) FROM tickets")
        .fetch_one(&app.st.db)
        .await
        .unwrap();
    assert_eq!(none, 0);

    // The setting is transaction-local: a pooled connection forgets it.
    let mut tx = tenant_tx(&app.st.db, a).await.unwrap();
    sqlx::query("SELECT 1").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM tickets")
        .fetch_one(&app.st.db)
        .await
        .unwrap();
    assert_eq!(after, 0);

    // The cross-tenant lookups are the named exceptions and nothing more.
    let by_slug: Option<Uuid> = sqlx::query_scalar("SELECT workspace_id_by_slug('acme')")
        .fetch_one(&app.st.db)
        .await
        .unwrap();
    assert_eq!(by_slug, Some(a));
}

#[tokio::test]
async fn the_app_role_cannot_bypass_rls() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (superuser, bypass): (bool, bool) =
        sqlx::query_as("SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user")
            .fetch_one(&app.st.db)
            .await
            .unwrap();
    assert!(!superuser && !bypass);
    let owns: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_tables WHERE tableowner = current_user")
            .fetch_one(&app.st.db)
            .await
            .unwrap();
    assert_eq!(owns, 0);
}

/// Every table that carries a tenant is one of these, with RLS forced and a
/// policy. A new tenant table goes on this list in the change that adds it.
const TENANT_TABLES: &[&str] = &[
    "agents",
    "attachments",
    "categories",
    "contacts",
    "drafts",
    "messages",
    "saved_views",
    "snippets",
    "suggestion_feedback",
    "suggestions",
    "ticket_reads",
    "tickets",
];
/// ADR 0003's named exceptions: reached before the tenant is known.
const CROSS_TENANT: &[&str] = &["auth_links", "jobs", "sessions"];

#[tokio::test]
async fn every_tenant_table_is_behind_rls() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let tables: Vec<(String, bool, bool, bool)> = sqlx::query_as(
        "SELECT c.relname::text, c.relrowsecurity, c.relforcerowsecurity,
                EXISTS (SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid)
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = 'public' AND c.relkind = 'r'
           AND EXISTS (SELECT 1 FROM information_schema.columns i
                       WHERE i.table_schema = 'public' AND i.table_name = c.relname
                         AND i.column_name = 'workspace_id')
         ORDER BY 1",
    )
    .fetch_all(&app.owner)
    .await
    .unwrap();
    let tenant: Vec<&str> = tables
        .iter()
        .map(|(t, ..)| t.as_str())
        .filter(|t| !CROSS_TENANT.contains(t))
        .collect();
    assert_eq!(tenant, TENANT_TABLES);
    for (table, enabled, forced, policy) in tables {
        if TENANT_TABLES.contains(&table.as_str()) {
            assert!(enabled && forced && policy, "{table} is not behind RLS");
        }
    }
}

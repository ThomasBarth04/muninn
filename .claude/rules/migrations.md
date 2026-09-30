---
paths:
  - "backend/migrations/**"
  - "deploy/delete-workspace.sql"
---

# Migrations

- A committed migration is frozen: sqlx checksums it, and the app runs
  `sqlx::migrate!()` at startup as `muninn_owner`, so an edit breaks every
  database that ran it. New change, new file: `NNNN_snake_name.sql`, next number.
- The app connects as `muninn_app`, which gets DML through default privileges.
  Never make it an owner, superuser or `BYPASSRLS`: those skip RLS.
- A tenant table has `workspace_id uuid NOT NULL REFERENCES workspaces ON
  DELETE CASCADE`, indexes that lead with `workspace_id`, and in the same file:

  ```sql
  ALTER TABLE t ENABLE ROW LEVEL SECURITY;
  ALTER TABLE t FORCE ROW LEVEL SECURITY;
  CREATE POLICY tenant ON t USING (workspace_id = app_workspace_id());
  ```

  Then add it to `backend/tests/rls.rs` and to the counts in
  `deploy/delete-workspace.sql`. The hook blocks a tenant table without RLS.
- A table that is deliberately outside the tenant gets `-- cross-tenant: <why>`
  directly above its `CREATE TABLE`, and holds no customer content.
- Enum-like text columns get a `CHECK`, and the same values go in the
  `#[ts(type = ...)]` in `api.rs`.

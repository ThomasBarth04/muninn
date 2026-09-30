---
paths:
  - "backend/src/**"
  - "backend/tests/**"
---

# Backend

- Tenant data: `db::tenant_tx(&st.db, ws)` and a `workspace_id` filter in the
  SQL itself (ADR 0003). The cross-tenant surface is short on purpose:
  `workspace_id_by_slug()`, `workspace_id_by_hubspot_portal()`, `auth_links`, `sessions`, `jobs`, `stripe_events`,
  `base_backups`, operator code in `admin.rs`. Keep customer content out of it.
- Errors are `ApiError` with a code the spec's Contract names. A new code is a
  spec change. `?` on a `sqlx::Error` logs it and returns `500 internal`.
- Races are settled by a unique index and `db::unique_violation`, never by
  check-then-insert.
- Contract structs live in `api.rs`: `#[derive(TS)] #[ts(export)]`,
  `#[serde(rename_all = "camelCase")]`, enum-like fields as `String` with
  `#[ts(type = "'a' | 'b'")]` matching the DB `CHECK`. `cargo test` rewrites
  `frontend/src/api/types/`; commit them with the Rust change or CI fails.
- `inbound.rs`: store first, decide later. A locked, over-limit or broken
  workspace still gets its mail stored; a duplicate Message-ID is a `200`.
  Non-2xx only when Postmark should retry, or when there is no workspace to
  store into (bad auth, unknown slug).
- Jev calls (`jev.rs`, `copilot.rs`, `categories.rs`): load the `typesafe`
  skill. Jev failing must show in the UI, never be papered over with a guess.
- Integration tests (`backend/tests/`) go through `common::spawn()`, which
  **returns `None` and passes silently without `TEST_DATABASE_URL`**. A green
  `cargo test` without it proves only the unit tests; use `/ci`.
- External services (Postmark, Jev, Stripe, HubSpot) are one `Mock` in tests; assert on
  `app.mock.calls(prefix)`, never hit the network.

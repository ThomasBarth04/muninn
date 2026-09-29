# muninn

An AI-native help desk, sold as multi-tenant SaaS. The help desk itself (inbox,
tickets, replies) mirrors HubSpot Help Desk; the difference is the copilot in the
ticket sidebar, which shows how the team solved the same problem before.

## Layout

- `backend/` — Rust service (`axum`, `sqlx`, `tokio`). Serves the API, the built
  SPA and the job worker from one binary.
- `frontend/` — React + Vite + TypeScript SPA. Built into `frontend/dist`, served
  by the backend from the same origin.
- `deploy/` — Docker Compose for the Hetzner box: app, Postgres, `wal-g`, Caddy.
- `specs/` — one spec per feature, source of truth for behaviour
- `docs/adr/` — architecture decisions, numbered, append-only

## Rules that are not negotiable

- **Every tenant query runs inside the tenant transaction** that sets
  `app.workspace_id`, *and* filters on `workspace_id` itself. Row-level security
  is the backstop, not the filter (ADR 0003). The app's database role is never a
  superuser or table owner — those bypass RLS.
- **Jev is the only model** (ADR 0002). Nothing generates text. The copilot shows
  what humans wrote.
- **Customer email is never rendered as HTML** and attachments are always served
  as downloads (spec 002). Mail bodies are attacker-controlled input.
- **Inbound mail is never dropped.** A locked, over-limit or broken workspace
  still stores what arrives; the Postmark webhook only returns non-2xx when it
  wants Postmark to retry.

## Spec-driven development

Order is: spec → review → code. No feature code lands without a spec file.

1. `/spec <name>` writes `specs/NNN-name.md` from the template.
2. Fill in behaviour and the API contract. Get it agreed before writing code.
3. Implement backend and frontend against that contract.
4. Spec changes with the code, in the same commit. A stale spec is worse than none.

The spec owns the HTTP contract shared by `frontend/` and `backend/`. If they
disagree, the spec is right and the code is a bug. Changing the contract's
shape (not just adding a field) needs an ADR. TypeScript types are generated
from the Rust structs with `ts-rs` — never hand-write a response type in
`frontend/`.

## ADRs

`/adr <title>` for a decision that is expensive to reverse: datastore, auth
model, tenancy, deploy target, anything a future reader would ask "why the hell"
about. Not for library picks you could swap in an afternoon.

ADRs are append-only. Superseding one means writing a new ADR that says so and
marking the old one `Superseded by NNNN`. Never edit the decision of an
accepted ADR.

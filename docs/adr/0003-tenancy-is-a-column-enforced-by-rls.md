# 3. Tenancy is a column, enforced by row-level security

Date: 2026-09-29

## Status

Proposed

## Context

Muninn is public SaaS from the first release: anyone signs up and gets a
workspace. Every workspace's tickets are other people's email — names,
addresses, account details, attachments. One query that forgets its tenant
filter shows company A's customers to company B, and that is the bug a help desk
does not survive.

Rejected:

- **Application filtering alone** (`WHERE workspace_id = $1` everywhere). Correct
  until the first query that forgets, and nothing notices when it does.
- **A schema or database per tenant.** The strongest isolation, but every
  migration runs once per tenant, connection pools multiply, and cross-tenant
  operations (billing sweeps, job queue) get awkward. Heavy for a product with
  zero customers.
- **Single-tenant first, tenancy later.** Adding a tenant column to a live
  database full of data is the expensive version of this decision.

## Decision

Every tenant-owned table has a non-null `workspace_id` and a row-level security
policy `workspace_id = current_setting('app.workspace_id')::uuid`. The backend
touches tenant data only through one helper that opens a transaction and runs
`SET LOCAL app.workspace_id`. Queries *also* filter on `workspace_id`
explicitly — RLS is the backstop that turns a forgotten filter into an empty
result instead of a leak.

The application connects as a role that is neither superuser nor owner of the
tables, and the tables have `FORCE ROW LEVEL SECURITY`. Migrations run as a
separate owner role.

## Consequences

A forgotten filter returns nothing rather than someone else's data. An
integration test proves it: two workspaces, one query without its filter, zero
foreign rows.

Code that must cross tenants — the job worker picking the next job, the Stripe
webhook finding a workspace by customer id, the Postmark webhook finding a
workspace by inbound address — cannot use the tenant helper. Those lookups go
through a small set of `SECURITY DEFINER` functions or tables without RLS
(`jobs`, `workspaces` lookup by slug), each named in the spec that needs it.
That list is the audit surface and stays short. The operator's commands on the
box (`muninn admin`, spec 001 §35) are the one other crossing: they connect as
the owner role, and only from a shell on the server.

`SET LOCAL` only lives inside a transaction, so every tenant read is a
transaction. With a pooled connection that is also what keeps one request's
workspace from leaking into the next.

Per-tenant backups, deletes and data residency are harder than with a database
per tenant: deleting a workspace is a `DELETE` cascading through every table,
not a `DROP DATABASE`.

# 8. Hetzner with Docker Compose, backed up by wal-g

Date: 2026-09-29

## Status

Proposed

## Context

Muninn stores other companies' customer email, so data stays in the EU and
losing it is the worst failure the product has. It is also a pre-revenue SaaS
that should cost little to run and be cheap to leave.

The backend is one binary (ADR 0007) that serves the API and runs the job loop
(ADR 0004), so it wants one always-on process, not functions.

Rejected:

- **Render in Frankfurt** — the battlebus choice. Managed Postgres with backups
  included, but pricier as data grows, and less control over the box.
- **Fly.io.** Similar to Render, more knobs.
- **Vercel.** Runs Rust functions, but an in-process job loop and a long-lived
  server are not what functions are for.
- **Managed Postgres elsewhere in the EU** (Neon, Aiven, Scaleway) next to a
  Hetzner app box. Backups become someone else's problem, at the price of a
  second vendor, a second bill and a network hop per query.
- **Nightly `pg_dump` only.** Simple, but a disk failure loses up to a day of
  tickets. A help desk that loses a day of customer email has lost the
  customers.

## Decision

Production is one Hetzner Cloud server in an EU location running Docker Compose
from `deploy/`: the app, Postgres, a `wal-g` sidecar and Caddy for TLS. Postgres
archives WAL continuously and takes a daily base backup with `wal-g` to Hetzner
Object Storage in the EU, giving point-in-time recovery with at most about a
minute of loss. `deploy/restore.sh` restores to a given timestamp, and it is run
against a scratch server once a month; a backup that has not been restored is a
hope.

## Consequences

Everything — compute, database, backups — sits with one EU provider on one bill.

We own Postgres: upgrades, disk space, tuning, and the 3am restore. The monthly
drill is what makes that survivable, and skipping it silently makes this ADR
wrong.

One server is one failure domain. A dead box means restoring to a new one from
object storage, which is minutes to an hour of downtime, not data loss. High
availability (a replica) is a later ADR when a customer's contract needs it.

Point-in-time recovery also undoes a bad migration or an accidental `DELETE`,
which nightly dumps would only partly cover.

Leaving is cheap: the app is a Docker image and configuration is environment
variables in `deploy/.env`.

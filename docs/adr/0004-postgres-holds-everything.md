# 4. Postgres holds everything

Date: 2026-09-29

## Status

Proposed

## Context

The MVP needs: relational ticket data, full-text search for the brain, login
sessions, a background job queue (inbound processing, Jev calls, outbound mail),
and attachment storage. Each of these has a specialist product. Each specialist
is also another thing to run on one Hetzner box (ADR 0008), back up, and keep
tenant-isolated (ADR 0003).

Rejected:

- **Redis for sessions and jobs.** Sessions are one indexed lookup per request
  and jobs are a few per ticket; Postgres does both at this scale. Redis would
  be a second datastore whose contents are not in the backup.
- **A job library with its own broker** (RabbitMQ, SQS). Same objection, plus a
  job enqueued in a different system from the row that caused it can be lost
  between the two commits.
- **Object storage for attachments** (S3, Cloudflare R2, Hetzner Object
  Storage). The usual answer, and the upgrade path. Rejected for now because a
  bucket is a second place tenant data lives, with its own access rules to get
  wrong; in Postgres, attachments get RLS and point-in-time recovery for free.
- **A search engine** (Meilisearch, Elasticsearch). Postgres `tsvector` with the
  workspace's language stemmer is enough to fetch fifty candidates for Jev to
  rerank (ADR 0010).

## Decision

Postgres is the only datastore. Tickets, messages, contacts, sessions and magic
link tokens are tables. The job queue is a `jobs` table worked with
`FOR UPDATE SKIP LOCKED` by a loop inside the backend binary, and a job is
inserted in the same transaction as the row that caused it. Attachments are
`bytea` in their own table, never selected alongside messages. The brain is a
`tsvector` on closed tickets.

## Consequences

One thing to back up, restore and reason about. A job cannot exist without the
row that caused it, and vice versa.

Attachments make the database large. Backups and restores grow with them, and
`wal-g` ships every attachment through the WAL. The ceiling is roughly 20 GB of
attachments — past that, move them to object storage in the EU with a new ADR.
The attachments table exists alone so that move is one table and one handler.

The job loop runs in the web process, so a crash takes both down together and
heavy jobs compete with requests for CPU. At one instance (ADR 0008) that is the
same failure domain anyway. Split the worker into its own process when jobs
visibly slow the UI.

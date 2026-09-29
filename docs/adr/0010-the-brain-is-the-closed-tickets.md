# 10. The brain is the closed tickets

Date: 2026-09-29

## Status

Proposed

## Context

The product's promise is that a problem solved once is solved for everyone
after: when Frank closes a ticket on Monday, Vetle sees it on Wednesday. That
needs a store of past cases, a way to find candidates in it, and a decision
about what counts as "the solution".

Forces: agents will not do extra work at close time for a tool they have not
learned to trust; there is no text model to clean up or summarise (ADR 0002);
Postgres full-text search stems per language, and Jev can only rerank what
search finds.

Rejected:

- **A required resolution note on close.** The cleanest signal — the internal
  "what I actually did" — at about 15 seconds per ticket. Rejected to keep
  closing a ticket free; internal comments in the thread capture the same
  knowledge when agents choose to write it.
- **A separate `cases` table** written at close. A second copy of data that
  already exists, and a sync problem when a ticket is reopened.
- **Importing history** (from Zendesk or a mailbox export) so a new workspace
  starts full. The best first impression, and one importer per source. Deferred
  until trial drop-off shows the empty brain is why.
- **Stemming per message, or no stemming.** Detecting language per message needs
  a detector and still misses cross-language matches; the `simple` config works
  for every language by matching exact words only, and loses recall everywhere.

## Decision

The brain is `tickets WHERE status = 'closed'`. When a ticket closes, its subject
and every message in its thread — customer mail, agent replies and internal
comments — are indexed into a `tsvector` with the workspace's language
configuration, chosen once at signup from Postgres' built-in stemmers. The
solution shown for a case is its last agent reply. A reopened ticket leaves the
brain until it closes again, and is re-indexed then.

New workspaces start empty. The sidebar says how many cases the brain holds, so
the learning is visible from the first closed ticket.

## Consequences

Zero extra work for agents; the brain grows as a side effect of doing the job.

"Last agent reply" is sometimes "Glad that worked, closing this!" rather than the
fix. The whole thread is one click away, and the feedback buttons (spec 003)
will show how often it happens. If it is often, the resolution note is the
answer, in a new ADR.

A workspace's language is fixed: changing it means re-indexing every closed
ticket, which the MVP does not offer. A mixed-language inbox gets weaker matching
in the language it did not choose.

A trial workspace sees no copilot value until its first repeat problem, which
may be days. That is the risk accepted in exchange for building no importer.

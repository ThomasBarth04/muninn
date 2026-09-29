# 7. A React SPA and a JSON API, served from one origin

Date: 2026-09-29

## Status

Proposed

## Context

The agent workspace mirrors HubSpot Help Desk: a view list, a ticket list, a
thread, and a sidebar that fills in as the copilot finishes. That is an
interactive client, and the team knows React.

Rejected:

- **Server-rendered HTML from Rust with htmx.** One language and no JSON
  contract, and it would cover the MVP. Rejected in favour of a client that can
  grow into keyboard triage, richer editors and an embeddable widget without a
  rewrite.
- **Next.js.** A second server runtime and deploy target next to the Rust one,
  for server rendering an authenticated app does not need.
- **Separate origins** (SPA on a CDN, API on `api.`). CORS, cross-site cookies
  and two deploys, for no user-visible gain.
- **WebSockets or SSE for live updates.** A ticket list that is ten seconds late
  is acceptable; HubSpot's own inbox is not instant either. A long-lived
  connection per agent is backend code we do not need yet.

## Decision

`frontend/` is React + Vite + TypeScript with TanStack Router and TanStack Query.
Its build output is embedded in or served by the backend binary, so the SPA,
`/api/*` and `/hooks/*` share one origin and one deploy. Response and request
types are generated from the Rust structs with `ts-rs`. The client polls: open
views and the open ticket refetch every 10 seconds.

## Consequences

One origin means cookie sessions with no CORS configuration. One binary means a
deploy is one image.

The JSON contract is a real interface now, and drifting from it is the classic
SPA bug. `ts-rs` makes the Rust structs the single definition; the spec's
Contract section describes them for humans.

Polling costs a query per open tab per 10 seconds. At the MVP's scale that is
nothing; if it stops being nothing, or "the ticket appeared late" becomes a real
complaint, move to SSE with a new ADR.

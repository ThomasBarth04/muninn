# 003. Similar cases

Status: Draft

## Problem

Frank solved an SSO certificate problem in ten minutes on Monday because he had
seen it before. On Wednesday Vetle gets the same problem, and nothing in front
of him says it has been solved — so he spends two hours rediscovering it and
asking people on Slack. The team's own answers exist, in closed tickets, and
nobody can find them at the moment they are needed.

## Behaviour

**Finding candidates**

1. When a ticket is created (spec 002 §5), a `suggest` job runs. It runs once per
   ticket. Later messages and a growing brain do not re-run it.
2. The brain is the workspace's closed tickets (ADR 0010). Candidates are found by
   full-text search in the workspace's language: the new ticket's subject and
   first message, turned into an OR of their stemmed words, ranked by
   `ts_rank_cd`, top 50. The ticket itself is never a candidate.
3. No candidates — an empty brain, or nothing shares a word — ends the job as
   ready with no suggestions. Jev is not called.

**Judging them**

4. One Jev request judges all candidates (contract below). `state` is the new
   ticket's subject and first message. Each candidate is one yes/no question
   whose instructions carry the past case: its subject, first customer message,
   internal comments and last agent reply, cut to 2,000 characters in that
   order of priority. Jev scores each independently. The first message in
   `state` is cut to 8,000 characters, inside Jev's 32k-token state limit.
5. Suggestions are the candidates scoring at least `SIMILARITY_THRESHOLD` (0.5),
   best first, at most 3. Every candidate's score is stored, shown or not — that
   is what the threshold gets tuned against.
6. Jev answering `429` or `529`: retried up to 3 times, after `retry-after` if
   Jev sends one (it does not document it) and 5 seconds otherwise, then the job
   fails. `401` or `422` fail at once and are logged as a
   configuration bug. A failed job never affects the ticket itself.

**In the sidebar**

7. The copilot section is headed with the brain's size: "Brain: 128 cases".
8. While the job runs: "Looking through 128 past cases…". Jev answers in well
   under a second, so the sidebar re-asks every 2 seconds while pending.
9. Ready with suggestions: up to 3 cards, each with the past case's subject, when
   it was closed, the match as a percentage, and its solution — the last agent
   reply, its author and date, the first 600 characters with "Show all". A case
   closed without any agent reply says "Closed without a reply" instead.
10. Ready with none: "Nothing similar yet — every closed ticket teaches the
    brain."
11. Failed: "Suggestions unavailable right now." No retry button.
12. Clicking a card's subject opens that case in the middle pane; back returns to
    the ticket. The first open per agent per suggestion is recorded.
13. Each card has **Helped** and **Not relevant**. One verdict per agent per
    suggestion; pressing the other one changes it.
14. A ticket with at least one suggestion shows a "seen before" mark in the
    ticket list (spec 002 §11).

**Measuring it** — no UI

15. The stored scores, verdicts and opens are the evals: how often suggestions
    help, and whether 0.5 is the right threshold. Time-to-close for tickets with
    and without suggestions is a SQL query over the same tables, kept in
    `docs/queries/`.

## Contract

Conventions as spec 001. Every route answers `401` and, when locked, `402`.

### `GET /api/tickets/{id}/suggestions`

`200`

```json
{
  "status": "ready",
  "brainSize": 128,
  "suggestions": [
    {
      "id": "4e81…",
      "case": { "ticketId": "19c3…", "subject": "SSO broken after IdP cert rotation", "closedAt": "2026-09-28T10:41:02Z" },
      "score": 0.92,
      "solution": {
        "text": "Their IdP rotated its signing certificate. Had them re-upload the new cert under Settings → SSO…",
        "author": { "name": "Frank" },
        "at": "2026-09-28T10:39:55Z"
      },
      "myFeedback": null
    }
  ]
}
```

`status`: `pending` · `ready` · `failed`. `suggestions` is `[]` unless `ready`.
`solution` is `null` for a case with no agent reply. `myFeedback`: `null` ·
`helped` · `notRelevant`.

`404 {"error":"notFound"}`

### `POST /api/suggestions/{id}/feedback`

```json
{ "verdict": "helped" }
```

`204` · `400 {"error":"invalidVerdict"}` · `404`

### `POST /api/suggestions/{id}/opened`

`204` — idempotent per agent. `404`

### Ticket

`seenBefore` (spec 002) is `true` when the ticket has at least one suggestion.

### Jev request (backend → TypeSafe, not a client contract)

`POST https://api.typesafe.ai/v1/systemone`, `Authorization: Bearer <JEV_API_KEY>`.

```json
{
  "model": "jev-latest",
  "state": {
    "subject": "Can't log in with SSO since this morning",
    "message": "Hi, since this morning none of us can log in with SSO…"
  },
  "questions": {
    "case_0": {
      "type": "noul",
      "instructions": {
        "past_case": "SSO broken after IdP cert rotation\n\nCustomer: …\n\nInternal: …\n\nSolution: …",
        "question": "Is past_case about the same underlying problem as the ticket in state?"
      },
      "criteria": {
        "true": "Same root cause, or the same fix would solve it",
        "false": "A different problem, even if it uses similar words"
      }
    }
  }
}
```

One `case_<n>` per candidate, up to 50. The response is
`{"model": "jev-1.13.0", "answers": {"case_0": {"type": "noul", "noul": 0.92}}, "usage": {…}}`;
`answers.case_<n>.noul` is the score (checked against docs.typesafe.ai,
2026-09-29). At about 2,000 characters per case, 50 cases stay well inside Jev's
64k-token request limit and cost about $0.0007 per ticket.

## Out of scope

- Re-running suggestions, or updating them as the thread grows.
- Suggestions from other workspaces — every brain is private to its workspace.
- Importing history to fill a new brain — spec 005.
- Editing a case's solution, or picking a better one than the last agent reply.
- Explaining why Jev matched (it cannot, ADR 0002); reply drafts; links to Jira
  or docs.
- A dashboard for the evals.

## Open questions

None.

# 004. Categories

Status: Draft

## Problem

Every ticket arrives as an undifferentiated email. An agent scanning the list
cannot tell a login problem from a bug report without opening each one, and
nothing records what kinds of problems a team gets — which is also what later
reporting and automatic pre-checks ("is this a login problem? check the user
first") will need to route on.

## Behaviour

**The list**

1. A new workspace starts with six categories, each with a description Jev reads:
   - **Bug** — Something in the product is broken or behaves wrongly.
   - **Login & access** — Cannot log in, SSO, passwords, locked or deactivated
     accounts, permissions.
   - **Billing** — Invoices, payments, plans, refunds.
   - **How-to** — How to do something the product already supports.
   - **Feature request** — Asks for something the product does not do.
   - **Other** — None of the above.
2. The owner manages the list in settings: add (name up to 40 characters,
   description up to 200, optional), edit name and description, archive. Names
   are unique among active categories, ignoring case. Other agents see the list
   and get `403 ownerOnly` on changes, like other workspace settings (spec 001
   §17); any agent can still set a ticket's category.
3. An archived category is no longer offered — not to Jev, not in the picker —
   but tickets that have it keep showing it.
4. At most 255 active categories, Jev's limit for one choice.

**Categorising a ticket**

5. When a ticket is created (spec 002 §5), a `categorize` job asks Jev one choice
   question over the active categories, with the ticket's subject and first
   message as `state`.
6. If Jev's top choice has probability at least `CATEGORY_THRESHOLD` (0.6), it
   becomes the ticket's category, marked as set by Jev.
7. Below that, the ticket stays uncategorised and the sidebar offers Jev's top
   two as one-click chips with their percentages: "Login & access 41% · Bug
   33%". Clicking one sets it, as the agent's choice.
8. Jev's pick and its probability are stored on the ticket whatever happens next.
   An agent changing a category that Jev set is an override; comparing the
   ticket's category with Jev's pick finds every override, with no event log.
9. The agent can change or clear the category at any time from the sidebar
   (spec 002 §21).
10. No active categories: the job does nothing. Jev failing: retried as in spec
    003 §6, then the ticket stays uncategorised with no chips.
11. Editing or archiving categories does not re-categorise existing tickets.

## Contract

Conventions as spec 001. Every route answers `401` and, when locked, `402`.

### Ticket

`category` and `categorySuggestions` in spec 002's Ticket:

```json
{
  "category": { "id": "0b6f…", "name": "Login & access", "source": "jev", "probability": 0.87 },
  "categorySuggestions": []
}
```

```json
{
  "category": null,
  "categorySuggestions": [
    { "id": "0b6f…", "name": "Login & access", "probability": 0.41 },
    { "id": "7d20…", "name": "Bug", "probability": 0.33 }
  ]
}
```

`category.source`: `jev` · `agent`. `category.probability` is Jev's when `source`
is `jev`, otherwise `null`. `categorySuggestions` is non-empty only while the
ticket is uncategorised and Jev fell below the threshold.

### `GET /api/categories`

`200`

```json
{
  "categories": [
    { "id": "0b6f…", "name": "Login & access", "description": "Cannot log in, SSO, passwords…", "archived": false }
  ]
}
```

Active first, then archived; each group alphabetical.

### `POST /api/categories`

```json
{ "name": "Integrations", "description": "Problems with the Slack, Teams or API integrations." }
```

`201` — the category.

`400 {"error":"invalidName"}` · `400 {"error":"invalidDescription"}` · `403
{"error":"ownerOnly"}` · `409 {"error":"nameTaken"}` · `409
{"error":"tooManyCategories"}`

### `PATCH /api/categories/{id}`

Any subset of `{ "name", "description", "archived" }`.

`200` — the category. Same errors as `POST`, plus `404`. Unarchiving can hit
`nameTaken` and `tooManyCategories`.

### Jev request (backend → TypeSafe, not a client contract)

```json
{
  "model": "jev-latest",
  "state": { "subject": "Can't log in with SSO since this morning", "message": "Hi, since this morning…" },
  "questions": {
    "category": {
      "type": "choice",
      "instructions": "Which category does this support ticket belong to?",
      "criteria": {
        "Bug": "Something in the product is broken or behaves wrongly.",
        "Login & access": "Cannot log in, SSO, passwords, locked or deactivated accounts, permissions."
      }
    }
  }
}
```

Criteria keys are the category names, which is why names are unique among active
categories; a category without a description uses its name as the criterion.
The answer is `answers.category` =
`{"type": "choice", "choice": "Login & access", "probabilities": {"Login & access": 0.87, "Bug": 0.09, …}, "confidence": …}`;
`choice` and `probabilities` give the pick and the top two. This question can share one request with spec 003's, since
both use the same `state`.

## Out of scope

- Reporting. (Filtering and saved views by category are spec 006.)
- Re-categorising when the list changes or when new messages arrive.
- More than one category per ticket; nested categories.
- Setting priority automatically.

## Open questions

None.

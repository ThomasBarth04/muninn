# 001. Workspaces, login and billing

Status: Draft

## Problem

A support team that wants to try Muninn has no way in. There is no workspace to
receive their mail, no address to forward their inbox to, no way to bring the
other agents on the team, and no way to pay once the trial has convinced them.

## Behaviour

**Signing up**

1. On `/signup` a visitor enters their email, a workspace name, a slug and a
   language. The slug is pre-filled from the name, is `[a-z0-9-]`, 3–32
   characters, and becomes the inbound address `<slug>@in.muninn.io`. Mailbox
   names mail systems reserve (`postmaster`, `abuse`, `hostmaster`, `webmaster`,
   `mailer-daemon`, `noreply`, `no-reply`, `root`, `admin`) are refused. The
   language is one of Postgres' text search configurations, English by default,
   `simple` for a language Postgres has no stemmer for (ADR 0010). The page says
   the language cannot be changed later.
2. `POST /api/signup` validates and sends a magic link to the email. Nothing is
   created yet — a slug is not reserved by someone who never clicks.
3. A slug that is already taken is reported at once (`409 slugTaken`). Slugs are
   public, so this reveals nothing.
4. An email that already belongs to an agent gets a login link instead of a
   signup link, with the same `202`. The response never reveals whether an
   address has an account.
   Signup links count towards the same limit as login links (§9): signup is
   otherwise a way to make us mail any address.
5. Following the link opens `/auth?token=…`, a page with one button: "Create
   Acme". The button, not the page load, consumes the token — corporate link
   scanners fetch every URL in an email and would otherwise burn it.
6. Pressing it creates the workspace, its owner, and the default categories
   (spec 004), starts a 14-day trial, logs the owner in, and lands on
   `/onboarding`: the inbound address, a copy button, and forwarding instructions
   for Google Workspace and Microsoft 365.
7. If someone else took the slug between the request and the click, the page
   says so and links back to `/signup` with the fields kept (the link's email,
   slug and language come from `GET /api/auth/link`).
8. A link that is expired (15 minutes) or already used says "This link has
   expired" and offers to send a new one.

**Logging in**

9. On `/login` an agent enters their email. `POST /api/login` always answers
   `202`; a link is sent only if the address is an agent. At most 5 links per
   address per hour are sent, signup and login links together — further requests
   still answer `202` and send nothing. The mail is sent after the response, so
   its timing does not reveal whether the address has an account.
10. The link opens the same `/auth` page ("Log in to Acme"). Pressing the button
    sets the session cookie and lands on `/`.
11. A session lasts 30 days from its last use. Any API call without a valid
    session answers `401 unauthenticated` and the SPA goes to `/login`.
12. "Log out" ends the current session only.

**The team**

13. An agent's display name defaults to the part of their email before `@` and
    can be changed in their profile.
14. The owner invites a teammate by email. The invite link is valid 7 days and
    opens `/auth` ("Join Acme"). Pressing the button creates the agent and logs
    them in.
15. An address that is already an agent — in this or any other workspace — cannot
    be invited (`409 emailTaken`). One email is one agent in one workspace. A
    removed agent's address can be invited again.
16. The owner can remove an agent. Their sessions end immediately and their open
    tickets become unassigned. The owner cannot remove themselves.
17. Only the owner invites, removes, manages billing and edits workspace settings
    (sending domain, spec 002; categories, spec 004). Anyone else gets
    `403 ownerOnly`, and the SPA does not show those controls to them.

**Trial and billing**

18. The trial runs 14 days from workspace creation. A banner shows the days left.
    No card is asked for.
19. The owner presses "Subscribe". `POST /api/billing/checkout` returns a Stripe
    Checkout URL for a subscription on the per-seat price, quantity = number of
    agents. The owner pays on Stripe and returns to `/settings/billing`. The
    price is the Stripe price `STRIPE_PRICE_ID` — configuration, not code.
20. The workspace becomes `active` when Stripe's webhook says so, not when the
    browser returns — a closed tab after payment still activates it.
21. "Manage billing" opens Stripe's customer portal: card, invoices,
    cancellation.
22. While active, adding or removing an agent updates the subscription quantity
    on Stripe, prorated, and so does completing Checkout (someone may have
    joined while the owner was paying). Pending invites are not seats. A
    subscription status Stripe reports that is none of active, trialing,
    past_due, canceled, unpaid or incomplete_expired leaves the status as it
    was.
23. `past_due` (Stripe is retrying a failed payment) keeps the workspace
    unlocked. `canceled`, or a trial that ends without a subscription, locks it.
24. A locked workspace: every API route except `/api/me`, `/api/billing/*` and
    `/api/logout` answers `402 paymentRequired`. The owner sees the paywall with
    "Subscribe"; other agents see "Ask your workspace owner to subscribe".
25. A locked workspace still receives mail. Everything that arrives is stored and
    is there when it unlocks (CLAUDE.md: inbound mail is never dropped).
26. A Stripe event is applied at most once, however often Stripe delivers it.

**Deleting a workspace**

27. On request (GDPR), an operator runs `deploy/delete-workspace.sql` by hand. It
    deletes the workspace and everything that cascades from it in one
    transaction; the Postmark domain and the Stripe customer are removed by hand.
    A button comes when requests stop being rare.

**Retention**

28. Magic links, used or not, are deleted a day after they expire; sessions 30
    days after their last use, when they can no longer log anyone in. Both hold
    email addresses and nothing reads them after that. A removed agent keeps
    their row: their replies keep an author (§16).

## Contract

Base path `/api`. Bodies JSON, times ISO-8601 UTC, ids UUIDs. Errors are
`{"error":"<code>"}`. A mutating request without `Content-Type: application/json`
answers `415 {"error":"jsonRequired"}` (ADR 0006: CSRF defence), bodiless ones
included. A body that is not the expected JSON answers `400
{"error":"invalidJson"}`. Stripe or Postmark failing while the user waits
answers `502 {"error":"upstream"}`; anything unexpected `500
{"error":"internal"}`.

The session is the cookie `muninn_session` (`HttpOnly; Secure; SameSite=Lax;
Path=/`). Every route except signup, login and the auth link answers
`401 {"error":"unauthenticated"}` without it.

### `POST /api/signup`

```json
{ "email": "frank@acme.com", "workspaceName": "Acme", "slug": "acme", "language": "english" }
```

`202 {}`

`400 {"error":"invalidEmail"}` · `400 {"error":"invalidWorkspaceName"}` (empty or
over 64 characters) · `400 {"error":"invalidSlug"}` · `400
{"error":"unsupportedLanguage"}` (not in `pg_ts_config`) · `409
{"error":"slugTaken"}`

### `POST /api/login`

```json
{ "email": "frank@acme.com" }
```

`202 {}` · `400 {"error":"invalidEmail"}`

### `GET /api/auth/link?token=<token>`

What the `/auth` page shows before the button is pressed. Does not consume the
token.

`200`

```json
{ "purpose": "signup", "workspaceName": "Acme", "email": "frank@acme.com", "slug": "acme", "language": "english" }
```

`purpose`: `signup` · `login` · `invite`. `slug` and `language` are `null` except
on `signup`.

`410 {"error":"linkExpired"}` — expired, used, or unknown.

### `POST /api/auth/link`

```json
{ "token": "q8Hk…" }
```

`200`, with `Set-Cookie: muninn_session=…`

```json
{ "redirect": "/onboarding" }
```

`/onboarding` after signup, `/` otherwise.

`410 {"error":"linkExpired"}` · `409 {"error":"slugTaken"}` (signup lost the
race) · `409 {"error":"emailTaken"}` (invite or signup lost the race)

### `POST /api/logout`

`204`, with the cookie cleared.

### `GET /api/me`

Allowed on a locked workspace.

`200`

```json
{
  "agent": { "id": "8f0c…", "email": "frank@acme.com", "name": "frank", "role": "owner" },
  "workspace": {
    "id": "2b7e…",
    "name": "Acme",
    "slug": "acme",
    "inboundAddress": "acme@in.muninn.io",
    "language": "english",
    "billing": {
      "status": "trialing",
      "trialEndsAt": "2026-10-13T18:02:11Z",
      "locked": false
    }
  }
}
```

`role`: `owner` · `agent`
`billing.status`: `trialing` · `trialExpired` · `active` · `pastDue` · `canceled`

### `PATCH /api/me`

```json
{ "name": "Frank" }
```

`200` — the `/api/me` body. `400 {"error":"invalidName"}` (empty or over 64
characters).

### `GET /api/agents`

`200`

```json
{
  "agents": [
    { "id": "8f0c…", "email": "frank@acme.com", "name": "Frank", "role": "owner" },
    { "id": "c41a…", "email": "vetle@acme.com", "name": "Vetle", "role": "agent" }
  ],
  "invites": [
    { "email": "kari@acme.com", "expiresAt": "2026-10-06T09:12:00Z" }
  ]
}
```

### `POST /api/invites`

```json
{ "email": "kari@acme.com" }
```

`201 { "email": "kari@acme.com", "expiresAt": "2026-10-06T09:12:00Z" }`

`400 {"error":"invalidEmail"}` · `403 {"error":"ownerOnly"}` · `409
{"error":"emailTaken"}`

Inviting an address with a pending invite replaces the old invite.

### `DELETE /api/agents/{id}`

`204` · `403 {"error":"ownerOnly"}` · `404 {"error":"notFound"}` · `409
{"error":"cannotRemoveOwner"}`

### `POST /api/billing/checkout`

`200 { "url": "https://checkout.stripe.com/c/pay/cs_…" }`

`403 {"error":"ownerOnly"}` · `409 {"error":"alreadySubscribed"}`

The Checkout session carries `client_reference_id` = workspace id, and the
subscription it creates carries `metadata.workspace_id`; that is how the webhook
finds the workspace.

### `POST /api/billing/portal`

`200 { "url": "https://billing.stripe.com/p/session/…" }`

`403 {"error":"ownerOnly"}` · `409 {"error":"noSubscription"}`

### `POST /hooks/stripe`

Stripe → Muninn. Raw body, verified against the `Stripe-Signature` header. Handled
events: `checkout.session.completed`, `customer.subscription.updated`,
`customer.subscription.deleted`. Everything else is acknowledged and ignored.
Event ids are stored; a repeat is acknowledged and not applied again.

`200` · `400 {"error":"invalidSignature"}`

This route crosses tenants (it finds the workspace by the id Stripe echoes back)
and is one of ADR 0003's named exceptions.

### Cross-tenant surface (ADR 0003)

Reached before the tenant is known, so outside the tenant helper:

- `auth_links` — no RLS. A signup link exists before its workspace.
- `sessions` — no RLS. The cookie is looked up before the workspace is known.
- `stripe_events` — no RLS. Event ids only.
- `agent_by_email(email)` — `SECURITY DEFINER`. Login, signup and invites find
  an agent in any workspace.
- `workspace_id_by_slug(slug)` — `SECURITY DEFINER`. `slugTaken` at signup.

None of them holds customer content.

## Out of scope

- Passwords, Google/Microsoft login, SAML SSO (ADR 0006).
- One email in several workspaces; switching workspaces.
- More than one owner, transferring ownership, roles beyond owner and agent.
- Revoking a pending invite — it expires after 7 days.
- Changing the slug or language after signup.
- Deleting a workspace from the product (§27 is by hand); exporting its data.
- Annual plans, coupons, invoices inside Muninn — Stripe's portal and dashboard
  cover them.

## Open questions

None. The price per seat is a pricing decision, not a behaviour: the product
reads it from Stripe (§19).

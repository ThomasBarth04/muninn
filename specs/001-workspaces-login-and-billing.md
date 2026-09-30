# 001. Workspaces, login and billing

Status: Draft

## Problem

A support team that wants to use Muninn has no way in: no workspace to receive
their mail, no address to forward their inbox to, no accounts for the agents on
the team, and no way to pay for it.

## Behaviour

Muninn runs as a private beta. An operator creates every workspace and invoices
it by hand; there is no public signup and no Stripe checkout — `/signup` says
Muninn is in private beta and how to ask for a workspace. Agents log in with a
password and an authenticator app (ADR 0011).

**Workspaces**

1. An operator creates a workspace from the production box with
   `muninn admin create-workspace` (Contract): a name, a slug, a language and
   the owner's email. The slug is `[a-z0-9-]`, 3–32 characters, and becomes the
   inbound address `<slug>@in.muninn.io`; mailbox names mail systems reserve
   (`postmaster`, `abuse`, `hostmaster`, `webmaster`, `mailer-daemon`,
   `noreply`, `no-reply`, `root`, `admin`) are refused. The language is one of
   Postgres' text search configurations, `simple` for a language Postgres has no
   stemmer for (ADR 0010), and cannot be changed later.
2. One transaction creates the workspace, its owner and the default categories
   (spec 004), with billing status `active`: no trial, no trial send cap,
   replies on the customer stream (ADR 0009). The owner is emailed a setup link
   (§5) and the command prints the inbound address. A taken slug, an email that
   is already an agent (§15) or an invalid field creates nothing and says why.
3. While the workspace has received no ticket, the owner lands on
   `/onboarding` after logging in instead of `/`: the inbound address, a copy
   button, and forwarding instructions for Google Workspace and Microsoft 365.

**Logging in** (ADR 0011)

4. An agent logs in on `/login` with their email, their password and the
   six-digit code their authenticator app shows — all three, every time.
   Success sets the session cookie and lands on `/` (or `/onboarding`, §3).
5. A setup link is how an agent gets a password and an authenticator. It is
   emailed when an operator creates an owner (§2), when the owner invites an
   agent (§14) and when a login is reset (§10). It is valid 7 days and works
   once. It opens `/auth?token=…`: the workspace name, the email, a password
   field, the authenticator's QR code with its key written out for typing in,
   and a field for the first code. Reading the page does not use the link —
   corporate link scanners fetch every URL in an email; submitting the form
   does. Success sets the password and the authenticator and logs the agent in.
6. The password is 10 to 256 characters, anything else goes. A password out of
   range or a code that does not match the QR code keeps the link valid and
   says which is wrong.
7. A wrong password, a wrong code, an unknown email and an agent who has not
   used their setup link yet all get the same `401 invalidCredentials`, in
   about the same time. A code works once; the codes of the 30 seconds before
   and after the current one also count, for clocks that drift.
8. Ten failed logins to one account within 15 minutes refuse its logins with
   `429 tooManyAttempts` until 15 minutes after the last failure, right
   password or not.
9. "Forgot password?" on `/login` asks for the email. `POST
   /api/password-reset` always answers `202`; an agent who has a password gets
   a reset link, valid one hour, at most 5 per address per hour, sent after the
   response so its timing reveals nothing. The link's page asks for a new
   password and a current authenticator code — the mailbox alone cannot take
   the account — and leaves the authenticator as it was. Success logs the agent
   in and ends their other sessions.
10. A lost authenticator is a reset login. The owner presses "Reset login" next
    to an agent in Settings → Team: the agent's password and authenticator are
    cleared, their sessions end, and they are emailed a setup link (§5). The
    owner's own login is reset by the operator with `muninn admin reset-login
    <email>`, which does the same for any agent.
11. In Settings → Profile an agent changes their password with the current one
    and a new one. Their other sessions end.
12. A session lasts 30 days from its last use. Any API call without a valid
    session answers `401 unauthenticated` and the SPA goes to `/login`. "Log
    out" ends the current session only.

**The team**

13. An agent's display name defaults to the part of their email before `@` and
    can be changed in their profile.
14. The owner invites a teammate by email in Settings → Team. The invite is a
    setup link (§5) whose page reads "Join Acme"; submitting it creates the
    agent. Inviting an address with a pending invite replaces the old one.
15. An address that is already an agent — in this or any other workspace —
    cannot be invited (`409 emailTaken`). One email is one agent in one
    workspace. A removed agent's address can be invited again.
16. The owner can remove an agent. Their sessions end immediately and their open
    tickets become unassigned. The owner cannot remove themselves.
17. Only the owner invites, removes and resets agents, sees billing and edits
    workspace settings (sending domain, spec 002; categories, spec 004). Anyone
    else gets `403 ownerOnly`, and the SPA does not show those controls to them.

**Billing during the beta**

18. Seats are invoiced monthly, outside Muninn. `muninn admin seats` lists every
    workspace with its seats for a month (UTC): each agent who was an agent at
    any moment of that month counts once — added on the 20th or removed on the
    3rd alike. Pending invites are not seats; paused workspaces are listed with
    theirs. Without `--month` it is the current month so far.
19. Settings → Billing shows the owner "Invoiced monthly per seat" and the
    current number of seats. There is nothing to subscribe to or manage.
20. An operator pauses a workspace that has stopped paying with `muninn admin
    pause <slug>` and resumes it with `muninn admin resume <slug>`. Paused is
    billing status `canceled`, and locks the workspace (§21).
21. A locked workspace: every API route except `/api/me`, `/api/billing/*` and
    `/api/logout` answers `402 paymentRequired`. Owner and agents alike see
    "This workspace is paused. Contact us to resume it." with the beta contact
    address.
22. A locked workspace still receives mail. Everything that arrives is stored and
    is there when it unlocks (CLAUDE.md: inbound mail is never dropped).

**Stripe — switched off during the beta**

The code below stays and stays tested; production has no Stripe keys and the
SPA shows no Stripe button, so none of it runs.

23. `POST /api/billing/checkout` returns a Stripe Checkout URL for a
    subscription on the per-seat price `STRIPE_PRICE_ID`, quantity = number of
    agents. The workspace becomes `active` when Stripe's webhook says so, not
    when the browser returns.
24. `POST /api/billing/portal` opens Stripe's customer portal: card, invoices,
    cancellation.
25. While a workspace has a subscription, adding or removing an agent updates
    its quantity on Stripe, prorated, and so does completing Checkout. A
    subscription status that is none of active, trialing, past_due, canceled,
    unpaid or incomplete_expired leaves the status as it was.
26. `past_due` (Stripe is retrying a failed payment) keeps the workspace
    unlocked; `canceled` locks it (§21).
27. A Stripe event is applied at most once, however often Stripe delivers it.

**Operator commands**

28. The `muninn admin` commands (Contract) run inside the app container on the
    box and are not reachable over HTTP. `create-workspace`, `reset-login`,
    `pause` and `resume` write through the tenant helper as `muninn_app`, like
    any request; `seats` reads across workspaces and so connects as
    `muninn_owner` through `MIGRATE_DATABASE_URL` (ADR 0003's operator
    exception).

**Deleting a workspace**

29. On request (GDPR), an operator runs `deploy/delete-workspace.sql` by hand. It
    deletes the workspace and everything that cascades from it in one
    transaction; the Postmark domain and the Stripe customer are removed by hand.
    A button comes when requests stop being rare.

**Retention**

30. Setup and reset links, used or not, are deleted a day after they expire;
    sessions 30 days after their last use, when they can no longer log anyone
    in. Both hold email addresses and nothing reads them after that. A removed
    agent keeps their row: their replies keep an author (§16).

## Contract

Base path `/api`. Bodies JSON, times ISO-8601 UTC, ids UUIDs. Errors are
`{"error":"<code>"}`. A mutating request without `Content-Type: application/json`
answers `415 {"error":"jsonRequired"}` (ADR 0006's CSRF defence, kept by ADR
0011), bodiless ones included. A body that is not the expected JSON answers
`400 {"error":"invalidJson"}`. Stripe or Postmark failing while the user waits
answers `502 {"error":"upstream"}`; anything unexpected `500
{"error":"internal"}`.

The session is the cookie `muninn_session` (`HttpOnly; Secure; SameSite=Lax;
Path=/`). Every route except login, password reset and the auth link answers
`401 {"error":"unauthenticated"}` without it.

### `POST /api/login`

```json
{ "email": "frank@acme.com", "password": "correct horse battery", "code": "492039" }
```

`200`, with `Set-Cookie: muninn_session=…`

```json
{ "redirect": "/" }
```

`/onboarding` for an owner whose workspace has no tickets (§3), `/` otherwise.

`401 {"error":"invalidCredentials"}` · `429 {"error":"tooManyAttempts"}`

### `POST /api/password-reset`

```json
{ "email": "frank@acme.com" }
```

`202 {}` · `400 {"error":"invalidEmail"}`

### `GET /api/auth/link?token=<token>`

What the `/auth` page shows. Does not use the token.

`200`

```json
{
  "purpose": "invite",
  "workspaceName": "Acme",
  "email": "kari@acme.com",
  "totp": {
    "secret": "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP",
    "uri": "otpauth://totp/Muninn:kari%40acme.com?secret=JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP&issuer=Muninn",
    "qrSvg": "<svg …>"
  }
}
```

`purpose`: `invite` (joins, §14) · `setup` (a new owner or a reset login, §2,
§10) · `reset` (forgot password, §9). `totp` is the authenticator to set up, on
`invite` and `setup`; `null` on `reset`, which asks for a code from the existing
one. `qrSvg` is an SVG image of `uri`.

`410 {"error":"linkExpired"}` — expired, used, or unknown.

### `POST /api/auth/link`

```json
{ "token": "q8Hk…", "password": "correct horse battery", "code": "492039" }
```

`200`, with `Set-Cookie: muninn_session=…`, and the `/api/login` body.

`410 {"error":"linkExpired"}` · `400 {"error":"invalidPassword"}` (under 10 or
over 256 characters) · `400 {"error":"invalidCode"}` (does not match the QR
code, or on `reset` the current authenticator) · `409 {"error":"emailTaken"}`
(an invite whose address became an agent meanwhile)

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
      "status": "active",
      "trialEndsAt": "2026-10-13T18:02:11Z",
      "locked": false
    }
  }
}
```

`role`: `owner` · `agent`
`billing.status`: `trialing` · `trialExpired` · `active` · `pastDue` ·
`canceled`. During the beta a workspace is `active`, or `canceled` while paused.

### `PATCH /api/me`

```json
{ "name": "Frank" }
```

`200` — the `/api/me` body. `400 {"error":"invalidName"}` (empty or over 64
characters).

### `POST /api/me/password`

```json
{ "currentPassword": "correct horse battery", "newPassword": "a longer one this time" }
```

`204` · `400 {"error":"wrongPassword"}` · `400 {"error":"invalidPassword"}`

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

### `DELETE /api/agents/{id}`

`204` · `403 {"error":"ownerOnly"}` · `404 {"error":"notFound"}` · `409
{"error":"cannotRemoveOwner"}`

### `POST /api/agents/{id}/reset-login`

`204` · `403 {"error":"ownerOnly"}` · `404 {"error":"notFound"}` · `409
{"error":"cannotResetOwner"}` (the operator resets the owner, §10)

### `POST /api/billing/checkout` — switched off (§23)

`200 { "url": "https://checkout.stripe.com/c/pay/cs_…" }`

`403 {"error":"ownerOnly"}` · `409 {"error":"alreadySubscribed"}`

The Checkout session carries `client_reference_id` = workspace id, and the
subscription it creates carries `metadata.workspace_id`; that is how the webhook
finds the workspace.

### `POST /api/billing/portal` — switched off (§24)

`200 { "url": "https://billing.stripe.com/p/session/…" }`

`403 {"error":"ownerOnly"}` · `409 {"error":"noSubscription"}`

### `POST /hooks/stripe` — switched off (§23–27)

Stripe → Muninn. Raw body, verified against the `Stripe-Signature` header. Handled
events: `checkout.session.completed`, `customer.subscription.updated`,
`customer.subscription.deleted`. Everything else is acknowledged and ignored.
Event ids are stored; a repeat is acknowledged and not applied again.

`200` · `400 {"error":"invalidSignature"}`

This route crosses tenants (it finds the workspace by the id Stripe echoes back)
and is one of ADR 0003's named exceptions.

### `muninn admin` — operator commands (§28)

Run on the box: `docker compose -f deploy/compose.yaml exec app muninn admin …`.
Success prints to stdout and exits `0`; a refusal prints one line to stderr and
exits `1`, having changed nothing.

```sh
muninn admin create-workspace --name "Acme" --slug acme --language english --owner frank@acme.com
# created acme — owner frank@acme.com (setup link emailed), inbound acme@in.muninn.io

muninn admin reset-login frank@acme.com
# reset frank@acme.com in acme (setup link emailed)

muninn admin seats --month 2026-10
# slug    workspace  owner           status  seats
# acme    Acme       frank@acme.com  active  4
# globex  Globex     ola@globex.no   paused  2

muninn admin pause acme      # paused acme
muninn admin resume acme     # resumed acme
```

`seats` output is tab-separated, one workspace per line, sorted by slug, so it
pastes into a spreadsheet.

Refusals: `invalid slug` · `invalid workspace name` · `unsupported language` ·
`invalid email` · `slug taken` · `frank@acme.com is already an agent` · `no
workspace acme` · `no agent frank@acme.com` · `invalid month` (not `YYYY-MM`) ·
`could not email the setup link` (the workspace or reset is kept; run
`reset-login` to send a new one).

### Cross-tenant surface (ADR 0003)

Reached before the tenant is known, so outside the tenant helper:

- `auth_links` — no RLS. Looked up by token before the workspace is known.
- `sessions` — no RLS. The cookie is looked up before the workspace is known.
- `stripe_events` — no RLS. Event ids only.
- `agent_by_email(email)` — `SECURITY DEFINER`. Login, password reset, invites
  and the operator find an agent in any workspace.
- `workspace_id_by_slug(slug)` — `SECURITY DEFINER`. Inbound mail and the
  operator find a workspace by slug.
- `muninn admin seats` — as `muninn_owner`, on the box only (§28). Workspace
  names, owner emails and agent counts.

None of them holds customer content.

## Out of scope

- Self-serve signup and trials. They return as their own spec, on this login and
  with ADR 0009's mail caps.
- Passkeys, SMS codes, recovery codes, "remember this device", Google/Microsoft
  login, SAML SSO (ADR 0011).
- One email in several workspaces; switching workspaces.
- More than one owner, transferring ownership, roles beyond owner and agent.
- Revoking a pending invite — it expires after 7 days.
- Changing the slug or language.
- Deleting a workspace from the product (§29 is by hand); exporting its data.
- A web admin. The operator is one person with a shell on the box; a page needs
  operator login and a cross-tenant HTTP route, and waits until that changes.
- Invoices inside Muninn: the seat list feeds invoices written in the
  accounting system.

## Open questions

1. The beta contact address the paused screen (§21) and `/signup` show.

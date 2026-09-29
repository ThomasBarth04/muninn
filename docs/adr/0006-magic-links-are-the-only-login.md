# 6. Magic links are the only login

Date: 2026-09-29

## Status

Proposed

## Context

Agents sign up themselves (public SaaS, ADR 0003) and invite teammates. We
already send email reliably (ADR 0005). Every password system also needs a
reset flow, and a reset flow is a magic link with extra steps.

Rejected:

- **Email + password.** Hash storage (argon2), a reset flow that is a magic link
  anyway, credential-stuffing exposure, and a breach of our table becomes a
  breach of every agent who reused a password.
- **Google / Microsoft OAuth.** Fastest for users, but two provider apps to
  register and verify, and an agent at a company on neither is locked out
  without a fallback — which would be a magic link.
- **A hosted provider** (Clerk, Auth0, WorkOS). The least auth code, but a
  per-active-user bill and a vendor in the login path for a flow that is four
  endpoints.

## Decision

The only way in is a single-use link sent to the agent's email. The token is 32
random bytes, stored as a SHA-256 hash, valid 15 minutes (7 days for an invite).
The link opens a page with one button, and pressing the button — a `POST` — is
what consumes the token: corporate link scanners fetch every URL in an email and
would otherwise burn it before the agent clicks. That creates a session: another
random token, stored hashed, sent as an `HttpOnly`, `Secure`, `SameSite=Lax`
cookie, valid 30 days and extended on use. Signup and invites use the same link,
carrying what to create when it is consumed.

## Consequences

No passwords exist, so none can leak. Logging in costs a trip to the inbox, which
is where support agents live anyway.

Login depends on our own outbound mail. If Postmark is down, nobody can log in
who is not already logged in; sessions are long for that reason.

`SameSite=Lax` stops cross-site `POST`s from carrying the cookie; together with
requiring `Content-Type: application/json` on every mutating route, that is the
CSRF defence. A `GET` must never change state.

Google/Microsoft sign-in can be added later as another way to reach the same
session. SAML SSO for enterprise goes through WorkOS when a customer asks, in its
own ADR.

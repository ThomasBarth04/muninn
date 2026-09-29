# 11. Passwords with an authenticator app

Date: 2026-09-29

## Status

Proposed. Supersedes 0006.

## Context

ADR 0006 made a magic link the only way in, so Muninn stored no password.
That makes the agent's mailbox the one factor: whoever reads an agent's email
reads every customer's tickets in their workspace. Buyers of a help desk ask
for multi-factor login, agents log in several times a day on locked-down
desktops, and a login that waits for an email is slow when mail is.

Rejected:

- **Magic links (ADR 0006).** One factor, and it is the mailbox.
- **Passkeys (WebAuthn).** Phishing-resistant and the strongest option, but
  more code and a harder story on shared and locked-down machines. The upgrade
  path when a customer asks.
- **SMS codes.** SIM swaps, cost per message, and a phone number per agent.
- **Recovery codes.** Another secret for agents to lose. At beta scale the
  owner (or the operator, for an owner) resetting a login does the same job.
- **MFA as an option.** The agents who skip it are the accounts that get
  taken over.

## Decision

Every agent logs in with email, password and a six-digit code from an
authenticator app, every time — no "remember this device".

- Passwords are hashed with Argon2id (the `argon2` crate's defaults), at least
  10 and at most 256 characters, no composition rules.
- The second factor is TOTP (RFC 6238): HMAC-SHA1, 6 digits, 30-second steps —
  what every authenticator app supports. The step before and after also
  count, and a code works once.
- Ten failed logins to one account within 15 minutes refuse further attempts
  for 15 minutes. Every failure — unknown email, wrong password, wrong code —
  gets the same answer.
- An email link never logs anyone in on its own. Links remain only to set a
  password: joining a workspace, a new owner, a reset login (these also set up
  the authenticator), and forgot-password, which asks for a current
  authenticator code as well, so the mailbox alone cannot take an account.
- A lost authenticator is a reset login: the owner resets an agent's, the
  operator an owner's. Both factors are cleared and a fresh setup link is sent.

Sessions, the cookie and the CSRF rule stay as ADR 0006 set them.

## Consequences

`POST /api/login` changes shape (spec 001): email, password and code in,
session out. The magic-link login and self-serve signup are gone; signup comes
back as its own spec, on this login.

The database now holds password hashes and TOTP secrets. The secrets are stored
as they are, because the server needs them to check a code: a copy of the
database gives an attacker the second factor but not the first. Encrypting them
with a key kept outside the database is the upgrade if that matters.

Argon2id costs about 20 ms of CPU and 19 MB of memory per attempt, run off the
async threads. An unknown email is checked against a dummy hash, so how long
the answer takes does not say whether an account exists.

The lockout lets someone who knows an agent's email keep that agent locked out,
15 minutes at a time. Accepted: it is visible, and it does not get them in.

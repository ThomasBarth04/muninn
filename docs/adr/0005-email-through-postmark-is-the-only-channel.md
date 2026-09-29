# 5. Email through Postmark is the only channel

Date: 2026-09-29

## Status

Proposed

## Context

Every company that could sign up already has a support inbox. "Forward
`support@yourcompany.com` to this address" is a two-minute onboarding step that
works with Google Workspace, Microsoft 365 and anything else, and it is the one
channel every help desk has to support anyway.

Doing email ourselves means SMTP servers, MIME parsing, bounce handling and IP
reputation. Buying it means choosing a provider that does inbound as well as
outbound, because the product is a conversation.

Rejected:

- **A web form, widget or live chat in the MVP.** A form is a thin layer over
  the same ticket and can come later. Chat is a different product (realtime,
  presence, availability) and is out.
- **AWS SES.** Cheapest at volume, but inbound arrives as raw MIME in S3 via SNS
  and we would parse it; new accounts start in a sandbox and need approval to
  send. Revisit when the Postmark bill is worth an engineer-week.
- **Mailgun / Resend.** Mailgun's deliverability is generally rated a step below
  Postmark's; Resend's inbound is newer than we want to bet the product on.
- **Always sending from our domain.** Zero setup, but a reply from
  `acme@in.muninn.io` looks less trustworthy to Acme's customers than one from
  `support@acme.com`.
- **Only sending from the customer's domain.** Nobody can reply to a ticket
  until they have edited DNS, which kills "forward and you are live". Sending as
  `support@acme.com` without their DKIM would fail DMARC and land in spam.
- **A separate `mail.muninn.io` sending subdomain.** Considered during design.
  It isolates sending reputation from inbound, but a customer replying to it
  (clients that ignore `Reply-To`) would hit a domain that does not receive.
  One subdomain that both sends and receives cannot lose a reply that way;
  `muninn.io` itself stays clean either way.

## Decision

Email is the only channel, and Postmark carries it both ways.

**Inbound.** `in.muninn.io` has its MX at Postmark. Every workspace has the
address `<slug>@in.muninn.io`; Postmark posts every message for the domain to
one webhook, and the local part picks the workspace. Each ticket has a random
token, and outbound mail carries it as `<slug>+<token>@in.muninn.io`, which
Postmark hands back as `MailboxHash` — that is the primary threading key.
`In-Reply-To` / `References` against stored `Message-ID`s is the fallback, for
replies that went to the customer's own address and were forwarded back.

**Outbound.** Until a workspace verifies its own domain, replies go out as
`"<Workspace> Support" <slug>+<token>@in.muninn.io>`. Once Postmark confirms the
workspace's DKIM and Return-Path records, replies go out from the workspace's
chosen address (`support@acme.com`) with `Reply-To: <slug>+<token>@in.muninn.io`.
The switch is one branch at send time.

## Consequences

Onboarding is: sign up, forward the inbox, reply. Custom-domain sending is an
optional settings step that never blocks anyone.

Postmark sees every message. It is a US subprocessor, alongside Jev (ADR 0002),
and goes in the privacy policy and DPA.

Postmark is a single point of failure for the product and its reputation is
shared across all workspaces — one spamming workspace can get the account
suspended. ADR 0009 is the mitigation and must ship with this.

Threading is only as good as the token surviving the round trip. A customer who
starts a fresh email about an old problem creates a new ticket; that is correct
behaviour for a help desk, and exactly the case the brain exists for.

Leaving Postmark means reimplementing inbound parsing; the webhook handler is
the only code that knows Postmark's inbound JSON, and the sender is the only
code that knows its send API.

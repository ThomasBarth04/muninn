# 002. Tickets and email

Status: Draft

## Problem

A team that has forwarded its support inbox has nowhere to see those emails as
work: nothing says who owns a conversation, what is waiting on whom, or what was
said on the last shift. Whoever comes on duty next starts from a raw inbox.

## Behaviour

The layout and the non-AI behaviour follow HubSpot Help Desk: views on the left,
the ticket list, the thread in the middle, properties and context on the right.

**Mail arriving**

1. Postmark posts every message sent to `*@in.muninn.io` to
   `POST /hooks/postmark/inbound` (ADR 0005). The webhook URL carries basic-auth
   credentials; a request without them answers `401`.
2. The workspace is the local part of `OriginalRecipient` before any `+`. If that
   is not at `in.muninn.io`, the first `To`/`Cc` address that is. No matching
   workspace answers `403`, which tells Postmark to stop retrying — there is no
   one to store it for.
3. A message whose `Message-ID` header this workspace has already stored answers
   `200` and stores nothing. Postmark retries on failure, so duplicates are
   normal.
4. Threading, first match wins:
   1. `MailboxHash` is the token of a ticket in this workspace → append to it.
   2. `In-Reply-To` or `References` names a `Message-ID` stored in this
      workspace → append to that ticket.
   3. Otherwise → a new ticket.
5. A new ticket: status **New**, no owner, no priority, subject from the mail
   (`(no subject)` if empty). The contact is found by the sender's email in this
   workspace, or created with the sender's display name. Categorising (spec 004)
   and similar-case jobs (spec 003) are enqueued in the same transaction.
6. A message appended to a ticket moves it to **Waiting on us**, including a
   closed ticket, which reopens and leaves the brain (ADR 0010). The owner is
   kept.
7. The text shown for a message is Postmark's `StrippedTextReply` when appending
   (the reply without the quoted history), else `TextBody`, else the text of
   `HtmlBody` with tags removed. `HtmlBody` is stored and never rendered.
8. Attachments, inline images included, are stored and listed on their message
   with name and size.
9. A locked workspace (spec 001), a workspace over its send cap, or a failing job
   does not stop any of this. Only a failure to store answers non-`2xx`, so
   Postmark retries — and a payload that is not Postmark's JSON (`400`), which
   gives us Postmark's retry window to fix the parser. An attachment that is not
   valid base64 is logged and skipped; the mail is kept.

**The workspace**

10. Three views, each with a count: **Unassigned** (open, no owner), **Assigned
    to me** (open, owner is me), **All open**. Open means any status but Closed.
    A fourth view, **Closed**, has no count; with no search it is the way back
    to a closed ticket.
11. The list shows, per ticket: contact name (or email), subject, a snippet of
    the last message (140 characters, whitespace collapsed), status, priority, owner, category (spec 004), the "seen
    before" mark (spec 003), and time of last activity. Newest activity first,
    50 at a time, "Load more" for the next 50.
12. The list, the counts and the open ticket refresh every 10 seconds (ADR 0007).
13. An empty view says so. A workspace that has never received mail shows its
    inbound address and the forwarding instructions instead.
14. The thread shows every message oldest first: customer mail, agent replies,
    and internal comments on a yellow background, each with author, time, text
    and attachments. An agent reply shows its delivery state when it is not
    simply sent.
15. The sidebar shows the ticket's status, owner, priority and category, all
    editable in place; the contact's name and email and their 10 most recent
    other tickets as links; and the copilot (spec 003).

**Working a ticket**

16. **Reply.** The agent writes plain text and sends. The reply is stored, a send
    job is enqueued in the same transaction, and the ticket moves to **Waiting
    on contact**. An unassigned ticket becomes the replying agent's, as in
    HubSpot — an answered ticket with no owner looks like nobody's job.
17. The mail goes to the ticket's contact only. From: the workspace's verified
    address if it has one, otherwise `"Acme Support" <acme+<token>@in.muninn.io>`;
    with a verified address, `Reply-To` is the token address (ADR 0005). Subject
    is `Re: <subject>` unless it already starts with `Re:`. `In-Reply-To` and
    `References` point at the last customer message; the reply gets its own
    `Message-ID` at `in.muninn.io`, stored for threading.
18. Postmark accepting the message marks it **sent**. A temporary failure is
    retried three times, a minute apart, then marked **failed** with "Could not
    reach the mail provider". A permanent rejection (Postmark's `422`, for
    example an inactive recipient) is **failed** at once, with Postmark's
    reason. A failed reply shows "Not sent — Retry".
19. A workspace without an active subscription (`past_due` still counts as one)
    that has 100 replies queued or sent in the last 24 hours holds the 101st: it is stored with delivery **held** and shows
    "Held — trial limit of 100 emails a day. Retry later" (ADR 0009). Retry
    sends it if the cap allows by then.
20. **Internal comment.** Stored in the thread, never emailed, status unchanged.
21. **Status, owner, priority, category** are changed from the sidebar. Any
    status can be set by hand. Closing indexes the ticket into the brain;
    reopening by hand takes it out (ADR 0010).
22. Two agents changing the same ticket: the last write wins. The other sees the
    change on the next refresh.

**Sending from your own domain** (owner, settings)

23. The owner enters the address replies should come from, e.g.
    `support@acme.com`. Muninn registers the domain with Postmark and shows the
    two DNS records to add: a DKIM `TXT` and a Return-Path `CNAME`. Status
    **pending**.
    Changing to another address on the same domain keeps the domain and its
    records.
24. "Check now" asks Postmark. When both records verify, status is **verified**
    and every reply from then on is sent from that address.
25. Removing it returns to the default sender immediately.
26. A domain another workspace already uses is refused (`409 domainTaken`).

**Attachments**

27. Downloading an attachment always downloads it — never displayed inline, with
    `Content-Disposition: attachment`, `X-Content-Type-Options: nosniff` and
    `Content-Security-Policy: sandbox`. An HTML or SVG attachment is
    attacker-controlled content on our origin otherwise.

## Contract

Conventions as spec 001: base path `/api`, JSON, ISO-8601 UTC, UUIDs,
`{"error":"<code>"}`. Every route here answers `401 unauthenticated` and, on a
locked workspace, `402 paymentRequired`. A ticket or message in another
workspace does not exist: `404 {"error":"notFound"}`.

### Ticket

The shape shared by the list and the detail view.

```json
{
  "id": "5d2c…",
  "subject": "SSO login fails after cert rotation",
  "status": "waitingOnUs",
  "priority": "high",
  "owner": { "id": "c41a…", "name": "Vetle" },
  "contact": { "id": "a9e1…", "email": "ola@kunde.no", "name": "Ola Nordmann" },
  "category": { "id": "0b6f…", "name": "Login & access", "source": "jev", "probability": 0.87 },
  "categorySuggestions": [],
  "seenBefore": true,
  "lastMessage": { "kind": "customer", "snippet": "Still getting the same error after…", "at": "2026-09-29T13:50:55Z" },
  "createdAt": "2026-09-29T09:12:40Z",
  "lastActivityAt": "2026-09-29T13:50:55Z"
}
```

`status`: `new` · `waitingOnContact` · `waitingOnUs` · `closed`
`priority`: `null` · `low` · `medium` · `high` · `urgent`
`owner`: `null` when unassigned.
`category`, `categorySuggestions`: spec 004. `seenBefore`: spec 003.
`lastMessage.kind`: `customer` · `agent` · `comment`

### `GET /api/tickets?view=<view>&cursor=<cursor>`

`view`: `unassigned` · `mine` · `open` · `closed`. `cursor` is omitted for the
first page.

`200`

```json
{
  "tickets": [ { "…": "Ticket" } ],
  "nextCursor": "eyJ0IjoiMjAyNi0wOS0yOVQxMzo1MDo1NVoiLCJpZCI6IjVkMmMifQ",
  "counts": { "unassigned": 3, "mine": 5, "open": 12 },
  "hasTickets": true
}
```

At most 50 tickets. `nextCursor` is `null` on the last page and is opaque to the
client. `counts` are in every response, so the left pane needs no request of its
own. `hasTickets` is `false` until the workspace's first mail, which is when the
SPA shows the forwarding instructions instead of an empty view (§13).

`400 {"error":"invalidView"}` · `400 {"error":"invalidCursor"}`

### `GET /api/tickets/{id}`

`200` — the Ticket, plus:

```json
{
  "messages": [
    {
      "id": "e3f0…",
      "kind": "customer",
      "author": { "name": "Ola Nordmann", "email": "ola@kunde.no" },
      "text": "Hi, since this morning none of us can log in with SSO…",
      "at": "2026-09-29T09:12:40Z",
      "delivery": null,
      "attachments": [
        { "id": "77aa…", "name": "error.png", "contentType": "image/png", "size": 48213 }
      ]
    },
    {
      "id": "f19b…",
      "kind": "agent",
      "author": { "name": "Vetle", "email": "vetle@acme.com" },
      "text": "Hi Ola, could you check whether your IdP certificate…",
      "at": "2026-09-29T10:02:13Z",
      "delivery": { "status": "sent", "error": null },
      "attachments": []
    }
  ],
  "contactTickets": [
    { "id": "19c3…", "subject": "Can't add new users", "status": "closed", "createdAt": "2026-08-14T11:20:00Z" }
  ]
}
```

`kind`: `customer` · `agent` · `comment`. `delivery` is `null` except on `agent`.
`delivery.status`: `queued` · `sent` · `failed` · `held`. `delivery.error` is a
human-readable reason on `failed` and `held`, otherwise `null`.
`contactTickets`: the contact's other tickets, newest first, at most 10.

### `PATCH /api/tickets/{id}`

Any subset:

```json
{ "status": "closed", "ownerId": "c41a…", "priority": "high", "categoryId": "0b6f…" }
```

`ownerId: null` unassigns; `priority: null` and `categoryId: null` clear.

`200` — the Ticket.

`400 {"error":"invalidStatus"}` · `400 {"error":"invalidPriority"}` · `400
{"error":"unknownAgent"}` · `400 {"error":"unknownCategory"}` (spec 004) · `404`

### `POST /api/tickets/{id}/replies`

```json
{ "text": "Hi Ola, re-upload the certificate under Settings → SSO…" }
```

`201` — the message, with `delivery.status` `queued`, or `held` when the trial
cap is reached (behaviour 19). A held reply is still a created message, so it is
still `201`.

`400 {"error":"emptyText"}` · `404`

### `POST /api/messages/{id}/retry`

For an agent reply that is `failed` or `held`.

`200` — the message, `queued` again or still `held`.

`409 {"error":"notRetryable"}` · `404`

### `POST /api/tickets/{id}/comments`

```json
{ "text": "Their IdP cert expired — the fix is on our side of the docs, not theirs." }
```

`201` — the message, `kind: "comment"`. `400 {"error":"emptyText"}` · `404`

### `GET /api/attachments/{id}`

`200` — the bytes, with the stored `Content-Type`, `Content-Disposition:
attachment; filename="error.png"`, `X-Content-Type-Options: nosniff` and
`Content-Security-Policy: sandbox`. `404`

### `GET /api/sending-domain`

`200`

```json
{
  "fromAddress": "support@acme.com",
  "status": "pending",
  "dnsRecords": [
    { "type": "TXT",   "host": "20260929201313pm._domainkey.acme.com", "value": "k=rsa;p=MIGfMA0GCS…" },
    { "type": "CNAME", "host": "pm-bounces.acme.com", "value": "pm.mtasv.net" }
  ]
}
```

`status`: `pending` · `verified`. With no custom address: `{"fromAddress": null,
"status": null, "dnsRecords": []}`.

### `PUT /api/sending-domain`

```json
{ "fromAddress": "support@acme.com" }
```

`200` — as `GET`, status `pending` (or unchanged, for a new address on the same
domain).

`400 {"error":"invalidAddress"}` · `403 {"error":"ownerOnly"}` · `409
{"error":"domainTaken"}` · `502 {"error":"upstream"}` (Postmark failed)

### `POST /api/sending-domain/verify`

`200` — as `GET`, with the status Postmark reports now. `403` · `404` (no custom
address set) · `502`

### `DELETE /api/sending-domain`

`204` · `403`

### `POST /hooks/postmark/inbound`

Postmark → Muninn, basic auth in the URL. Postmark's inbound JSON; the fields read
are `OriginalRecipient`, `ToFull`, `CcFull`, `FromFull`, `Subject`,
`MailboxHash`, `TextBody`, `HtmlBody`, `StrippedTextReply`, `Headers`
(`Message-ID`, `In-Reply-To`, `References`) and `Attachments` (`Name`,
`Content`, `ContentType`, `ContentLength`, `ContentID`).

`200` — stored, or a duplicate · `400` — not Postmark's JSON · `401` — bad
credentials · `403` — no such workspace (stops retries) · `5xx` — could not
store; Postmark retries.

This route crosses tenants (it finds the workspace by slug) and is one of ADR
0003's named exceptions.

### Cross-tenant surface (ADR 0003)

- `workspace_id_by_slug(slug)` — `SECURITY DEFINER`. The inbound webhook picks
  the workspace by the address's local part.
- `jobs` — no RLS. The worker claims the next job of any workspace, then does
  the work inside that job's tenant transaction.

## Out of scope

- Rendering HTML email; rich-text replies; attachments on replies.
- Cc/Bcc on replies, composing a new outbound email (ADR 0009), forwarding a
  ticket.
- Tags, SLAs, routing and auto-assignment, companies, @mentions and
  notifications, custom pipelines or statuses, merging and splitting tickets.
  (Search, snippets, keyboard shortcuts, ticket numbers and collision warnings
  are spec 006.)
- Automatic acknowledgement emails ("we got your message").
- Bounce and spam-complaint webhooks from Postmark; spam filtering of inbound.

## Open questions

None.

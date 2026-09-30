# 007. HubSpot sync

Status: Agreed

## Problem

A team that answers its tickets in HubSpot Help Desk cannot use the copilot
without moving its inbox to Muninn first, and the problems it has already
solved sit in HubSpot, where Muninn cannot see them. So its brain would start
empty (ADR 0010), and the tickets it is working today never reach Muninn.

## Behaviour

HubSpot stays the help desk: the team replies there. Muninn reads HubSpot and
never writes to it (ADR 0005). The HubSpot tickets it holds feed the brain and get the
copilot, and each one opens in HubSpot for anything else.

**Connecting**

1. Settings → HubSpot exists when the server has the HubSpot app's credentials
   (`HUBSPOT_CLIENT_ID`, `HUBSPOT_CLIENT_SECRET`). Without them, the section is
   not shown and the routes answer `404`.
2. The owner presses **Connect HubSpot**. The browser goes to HubSpot's consent
   page for Muninn's HubSpot app. There, a HubSpot user allowed to install apps
   picks the account and approves read-only access (Contract: scopes). Other
   agents see the connection's status but not the controls (`403 ownerOnly`,
   spec 001 §17).
3. HubSpot sends the browser back with a code, which Muninn exchanges for
   tokens. The owner then lands on Settings → HubSpot. The code only works if
   the round trip started from this workspace's owner, in this browser, within
   10 minutes (`state`). Otherwise nothing is connected and the page says "That
   link expired — connect again".
4. A workspace connects one HubSpot account, and a HubSpot account belongs to
   one workspace. An account that another workspace already uses is refused
   ("This HubSpot account is connected to another Muninn workspace"), and its
   tokens are thrown away without revoking them. Revoking could break the other
   workspace's install.
5. The refresh token is stored. It is useless without the app's client secret,
   which lives in the environment, not the database. Access tokens last 30
   minutes, are kept in memory only, and are refreshed when they expire.
6. The owner ticks the pipelines to sync. HubSpot's default ticket pipeline
   starts ticked, and nothing is imported until the owner saves. Onboarding
   (spec 001 §3) offers **Connect HubSpot** next to the forwarding
   instructions.

**The import**

7. Saving starts the import. It takes every ticket in the ticked pipelines,
   open or closed, that was modified (`hs_lastmodifieddate`) in the 12 months
   before. Twelve months covers a full year of seasonal problems: renewals,
   year-end, holidays. Older fixes are left out, because they describe a
   product that has since changed. A year of 10,000 tickets imports in about
   an hour inside HubSpot's rate limit (§27).
8. Newest tickets come first, so today's open tickets appear within a minute
   and the brain fills backwards.
9. While the import runs, Settings shows "Importing from HubSpot — 2,140 of
   6,012 tickets", and the ticket list and brain size grow as it goes.
   Afterwards it shows "Synced — 6,012 tickets, last change 2 minutes ago".
10. The import runs in the background, so closing the browser does not stop
    it. After a Muninn restart or a HubSpot outage it picks up where it
    stopped; one that gave up after its retries is started again by the
    hourly check (§19). A ticket HubSpot refuses to hand over is skipped and
    logged rather than holding up the rest. Tickets already imported are
    updated, never duplicated (§20).
11. Jev is not asked about tickets that arrive closed (as spec 005 §6). A
    ticket that arrives open is categorised (spec 004) and gets suggestions
    (spec 003), once, like a ticket from inbound mail. For tickets imported
    while the import runs, those jobs wait until it finishes, so they search
    the whole brain.

**What a HubSpot ticket becomes**

12. Each HubSpot ticket becomes one Muninn ticket, marked as coming from
    HubSpot:
    - **Subject:** `subject`, or `(no subject)` if empty.
    - **Status:** a stage whose `ticketState` is `CLOSED` becomes Closed. The
      default pipeline's stages `1` (New), `2` (Waiting on contact) and `3`
      (Waiting on us) map to the same statuses. Any other open stage becomes Waiting on us.
    - **Priority:** `hs_ticket_priority` maps to low, medium, high or urgent.
      Anything else means no priority.
    - **Owner:** the Muninn agent whose email matches the HubSpot owner's.
      Without a match the ticket has no owner. An agent who joins later owns
      their tickets from the moment they join (spec 001 §14), so the team
      should be invited with the emails they use in HubSpot.
    - **Contact:** the customer's email address: the sender of the first
      incoming message, or else the ticket's primary contact. It is found or
      created as for inbound mail (spec 002 §5), with the HubSpot contact's
      name.
    - **Dates:** created is `createdate`, and closed is HubSpot's close date.
      Last activity is the newest message's time, or the creation date if
      there are no messages.
13. The thread is the ticket's HubSpot conversation thread, oldest message
    first, whatever the channel (email, chat, form). Incoming messages become
    customer messages and outgoing ones agent replies. Internal comments on
    the thread and notes on the ticket become internal comments. Assignment,
    status-change and welcome messages are left out. A ticket with no thread,
    such as one created by hand, gets its `content` as its only customer
    message, if it has any.
14. Only text is kept. Muninn stores HubSpot's plain `text`, never its
    `richText` HTML, and cuts quoted history as in spec 005 §8. A message that
    HubSpot truncated is fetched in full. Attachments are not imported;
    **Open in HubSpot** shows them.
15. An agent reply's author is the Muninn agent with that HubSpot user's
    email. Without a match, the author is the HubSpot user's name and email,
    as for imported replies (spec 005 §9). Replies count as sent and are never
    sent again.
16. A ticket whose customer has no email address, such as an anonymous chat,
    is skipped and counted: "18 skipped — no customer email".
17. A closed ticket is in the brain (ADR 0010) as soon as it lands.

**Keeping in sync**

18. After the import, HubSpot reports changes through webhooks (Contract). A
    ticket was created, changed, deleted, merged, restored or had a note
    added. A thread got a message or was deleted. Each event makes Muninn
    re-read that ticket from HubSpot and make its copy match. HubSpot does not
    promise order and sends duplicates, so re-reading the whole ticket is safe
    where applying events one by one would not be. A change shows up in
    Muninn within seconds, and the list refreshes every 10 seconds.
19. Every hour, Muninn also re-reads each ticket modified since one hour
    before its last check, in any pipeline. This catches a change, or a move
    out of the ticked pipelines, whose webhook was missed, including during a
    Muninn outage longer than HubSpot's 24 hours of retries. A deletion shows
    only through its webhook, because HubSpot's search does not return
    deleted tickets. More than 300 changed tickets, after a long outage or a
    revoked week, are caught up as an import, with its progress in Settings.
20. Making the copy match means the fields are set as in §12. Messages are
    added, changed and removed until they mirror the thread. A ticket closed
    in HubSpot enters the brain, and a reopened one leaves it. A closed ticket
    whose thread changes is re-indexed (ADR 0010).
21. A ticket deleted in HubSpot, or merged into another, is deleted in Muninn,
    along with its thread and every suggestion that points to it. A ticket
    moved to an unticked pipeline is deleted, and one moved into a ticked
    pipeline is imported.
22. Ticking another pipeline later imports its last 12 months (§7). Unticking
    one deletes its tickets, closed ones too. Unticking a pipeline says its
    tickets are not support.
23. Sync keeps running while a workspace is locked, as inbound mail does
    (spec 002 §9).

**In Muninn**

24. HubSpot tickets appear everywhere Muninn tickets do: views, counts, the
    list, the brain and the copilot. Each has a HubSpot mark and an **Open in
    HubSpot** link in the thread header.
25. They are read-only in Muninn. There is no reply or comment box, and
    status, owner and priority are shown but cannot be edited. Those change in
    HubSpot and arrive through the sync. The API refuses such edits with
    `409 syncedFromHubSpot`.
26. Category can still be edited and suggestion feedback still works. Both
    belong to Muninn and never go to HubSpot.

**When it breaks**

27. HubSpot allows each app 110 requests per 10 seconds per account. Muninn
    stays under 100, and on a `429` it waits the interval out and tries again.
28. When HubSpot errors or does not answer, the job retries with backoff, and
    the hourly check (§19) catches up on what was missed. Settings shows the
    last successful sync and the last error.
29. If the app is uninstalled in HubSpot or its token revoked, HubSpot refuses
    the next refresh. The status becomes **revoked** and Settings says
    "HubSpot disconnected Muninn — Reconnect or Disconnect". Tickets stay as
    they were. Reconnecting the same account catches up from the last check
    (§19). A different account is refused until the owner disconnects.

**Disconnecting**

30. When the owner presses **Disconnect**, Muninn uninstalls its app from the
    HubSpot account and stops syncing. Settings then says to remove the app
    under Connected apps in HubSpot if it still shows there, since the
    uninstall call can fail.
    - **Closed tickets** stay in the brain, still marked and still read-only.
    - **Open tickets** are deleted. They are still in HubSpot, and frozen
      copies would clutter the open views forever.
31. Connecting again imports again (§7). Tickets kept from before are
    updated, not duplicated.
32. Deleting a workspace (spec 001 §29) deletes its connection and its
    HubSpot tickets. As with the Postmark domain, the operator uninstalls the
    app from HubSpot by hand.

## Contract

Conventions as spec 001. Every route answers `401` and, on a locked workspace,
`402`. The routes below answer `404 {"error":"notFound"}` when the server has
no HubSpot credentials (§1).

### Ticket

Spec 002's Ticket, list and detail alike, gains `hubspot`:

```json
{
  "id": "5d2c…",
  "subject": "SSO login fails after cert rotation",
  "hubspot": { "url": "https://app.hubspot.com/contacts/139574231/record/0-5/35512339183" }
}
```

`hubspot` is `null` for a ticket that did not come from HubSpot. Its messages
use spec 002's shape. An agent reply has `kind: "agent"`, `author` set to the
agent (or the HubSpot user's `name` and `email`), `delivery: { "status":
"sent", "error": null }` and `attachments: []`.

### Refused on a HubSpot ticket

- `POST /api/tickets/{id}/replies` and `POST /api/tickets/{id}/comments`:
  `409 {"error":"syncedFromHubSpot"}`.
- `PATCH /api/tickets/{id}` with `status`, `ownerId` or `priority`:
  `409 {"error":"syncedFromHubSpot"}`, and nothing in the request is applied.
  `categoryId` alone is accepted.

### `GET /api/integrations/hubspot`

Any agent.

`200`

```json
{
  "connection": {
    "accountId": 139574231,
    "accountName": "acme.com",
    "status": "importing",
    "pipelines": [
      { "id": "0", "label": "Support Pipeline", "selected": true },
      { "id": "72104633", "label": "Onboarding", "selected": false }
    ],
    "import": { "done": 2140, "total": 6012 },
    "tickets": 2140,
    "skipped": 18,
    "lastSyncedAt": "2026-09-29T14:02:11Z",
    "lastError": null,
    "connectedAt": "2026-09-29T12:40:00Z"
  }
}
```

With no connection: `{"connection": null}`.

- `status`: `pickPipelines` (connected, nothing saved yet) · `importing` ·
  `synced` · `revoked` (§29).
- `import`: set while `importing`, otherwise `null`. `total` is HubSpot's own
  count and can move while the import runs.
- `tickets`: the Muninn tickets that came from HubSpot.
- `skipped`: tickets skipped under §16.
- `lastError`: a human-readable reason when the last attempt failed, otherwise
  `null`.
- `pipelines`: read from HubSpot on connect, and again at every hourly check.

### `POST /api/integrations/hubspot/connect`

Bodiless, `Content-Type: application/json` (spec 001).

`200`

```json
{ "url": "https://app.hubspot.com/oauth/authorize?client_id=…&redirect_uri=…&scope=…&state=…" }
```

The SPA navigates to `url`. The `state` is random, is tied to this agent, is
valid for 10 minutes and works once.

`403 {"error":"ownerOnly"}` · `409 {"error":"alreadyConnected"}`

### `GET /api/integrations/hubspot/callback?code=…&state=…`

HubSpot redirects the browser here, so the answer is a redirect, not JSON.
Without a session it is `303 /login`. Otherwise it is `303
/settings/hubspot`, carrying `?error=<code>` when the connection failed:

- `expired`: the `state` is unknown, used, older than 10 minutes, or belongs
  to another agent.
- `denied`: the HubSpot user did not approve.
- `portalTaken`: the account belongs to another workspace (§4).
- `alreadyConnected`: this workspace connected meanwhile.
- `upstream`: exchanging the code failed.

### `PUT /api/integrations/hubspot/pipelines`

```json
{ "pipelineIds": ["0"] }
```

`200`: the `connection` object as in `GET`. The first save starts the import
(§7). Adding a pipeline imports it, and removing one deletes its tickets (§22).

`400 {"error":"noPipelines"}` · `400 {"error":"unknownPipeline"}` · `403
{"error":"ownerOnly"}` · `404` (not connected)

### `DELETE /api/integrations/hubspot`

`204`: disconnected under §30, whether or not the uninstall at HubSpot
succeeded. `403 {"error":"ownerOnly"}` · `404` (not connected)

### `POST /hooks/hubspot`

HubSpot → Muninn, configured once for the app, for every installed account.
The request is verified with `X-HubSpot-Signature-v3`: HMAC-SHA256, keyed with
the client secret, over method, URI, body and `X-HubSpot-Request-Timestamp`.
A timestamp older than 5 minutes is refused. The body is an array of up to 100
events. The fields read are `portalId`, `subscriptionType`, `objectId`,
`primaryObjectId`, `mergedObjectIds` and, for `ticket.associationChange`,
`fromObjectId` (the ticket).

Subscriptions:

- `ticket.creation`, `ticket.deletion`, `ticket.merge`, `ticket.restore`,
  `ticket.associationChange` (notes)
- `ticket.propertyChange` on `subject`, `content`, `hs_pipeline`,
  `hs_pipeline_stage`, `hs_ticket_priority` and `hubspot_owner_id`
- `conversation.newMessage`, `conversation.deletion`,
  `conversation.privacyDeletion`. For these, `objectId` is the thread, which
  leads to its ticket.

Each event queues one re-read of its ticket. An event for a ticket that already
has a re-read queued adds nothing.

Responses:

- `200`: queued, or ignored (an account no workspace has connected).
- `401 {"error":"invalidSignature"}`
- `5xx`: could not queue; HubSpot retries.

HubSpot retries any `4xx` as well, so refusing an unknown account would only
cost retries. That is why it is ignored instead.

This route crosses tenants, since it finds the workspace by `portalId`. It is
one of ADR 0003's named exceptions.

### Cross-tenant surface (ADR 0003)

- `workspace_id_by_hubspot_portal(portal_id)`: `SECURITY DEFINER`. Used by the
  webhook.
- The connection's HubSpot account id is unique across workspaces. That is
  what §4 checks, the same way `domainTaken` works (spec 002 §26).
- The hourly check (§19) is a job that queues its own successor inside its
  tenant transaction. It adds nothing new across tenants.

### HubSpot calls (backend → HubSpot, not a client contract)

Scopes, all read-only: `tickets`, `crm.objects.tickets.read`,
`conversations.read`, `crm.objects.contacts.read` (which also covers notes),
`crm.objects.owners.read`.

Calls use HubSpot's dated `2026-09` paths. The v3 paths lose support in
September 2027.

- `POST /oauth/2026-03/token`: exchange the code, refresh the token.
- `POST /oauth/2026-03/token/introspect`: which account the user picked
  (`hub_id`) and its name (`hub_domain`).
- `GET /crm/pipelines/2026-09/tickets`: pipelines, and stages with
  `ticketState`.
- `GET /crm/owners/2026-09`: owner `id`, `userId` and `email`. This maps
  `hubspot_owner_id` and `A-<userId>` senders to emails.
- `POST /crm/objects/2026-09/tickets/search`: the import, with
  `hs_pipeline IN` the ticked pipelines, and the hourly check, across every
  pipeline. Both filter `hs_lastmodifieddate GTE` and read newest first,
  100 per page, with `hs_pipeline` in each result. Each page is a new query
  from the previous page's last ticket back (`LTE`). HubSpot's `after` is an
  offset, which shifts when a ticket is deleted, so it is only used within a
  page that is all one millisecond. This also keeps clear of search's
  10,000-result cap. Search allows 5 requests a second.
- `GET /crm/objects/2026-09/tickets/{id}`, with `associations=contacts,notes`:
  a re-read. A `404` means deleted, and an answer for another id means merged
  into it (§21).
- `GET /conversations/conversations/2026-09/threads?associatedTicketId={id}`
  finds the ticket's thread, and `…/threads/{thread}?association=TICKET` a
  thread's ticket. The messages call returns `MESSAGE` and `COMMENT` only; a
  truncated one is fetched through `…/messages/{id}/original-content`.
- `POST /crm/objects/2026-09/notes/batch/read`: the ticket's notes. A note's
  `hs_note_body` is HTML, so only its text is kept.
- `GET /crm/objects/2026-09/contacts/{id}`: `email`, `firstname`, `lastname`,
  only for a ticket whose thread names no customer email.
- `DELETE /appinstalls/v3/external-install`: Disconnect (§30). This call is
  in public beta.

These are assumed, not yet checked against a HubSpot test account. Check them
first, before connecting a customer:

- `crm.objects.contacts.read` reads a ticket's notes.
- Token introspection names the account as `hub_id` and `hub_domain`.
- Help Desk threads still return their old `COMMENT` messages. Since
  2026-09-23, new Help Desk comments are CRM notes, which §13 reads as well.
- The default pipeline's stage ids are `1`–`4`.
- `app.hubspot.com` redirects an EU account's record URL to its own data
  centre.
- Any HubSpot account can install an unlisted Projects-platform app through
  its install URL.

## Out of scope

- Writing anything to HubSpot: replies, comments, status, owner, priority,
  category.
- The copilot inside HubSpot, as a card in HubSpot's ticket sidebar. Agents
  open the ticket in Muninn to see it.
- Attachments; emails logged on a ticket outside a conversation thread (older
  Service Hub setups); calls, meetings, tasks; custom ticket properties;
  companies.
- Tickets modified more than 12 months ago, and letting the owner choose the
  window.
- More than one HubSpot account per workspace. Other help desks: Zendesk,
  Freshdesk, Intercom.
- Deduplicating against forwarded mail or an mbox import (spec 005). A team
  that does both gets each ticket twice.
- Showing HubSpot's own stage names for custom pipelines.
- Listing on the HubSpot App Marketplace. The app is installed through
  **Connect HubSpot**.

## Open questions

None.

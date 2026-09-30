# 006. Triage: views, filters, keyboard and the small things

Status: Shipped

Extends spec 002 (the workspace) and spec 004 (categories). Search, snippets,
keyboard shortcuts, ticket numbers and collision warnings leave spec 002's Out
of scope; filtering by category leaves spec 004's. Both specs' Out of scope
lines are updated in the commit that ships this one.

## Problem

An agent working the queue can pick one of four fixed views and scroll. They
cannot narrow it to "urgent billing tickets nobody owns", find last week's
ticket from ola@kunde.no, act on twenty tickets at once, or move through the
queue without the mouse. Nothing tells them which tickets changed since they
last looked, that a colleague is already answering, or how long a customer has
been waiting. A reply leaves the instant Send is pressed, a close cannot be
taken back, and a half-written reply is gone when they click another ticket.
The help desk works; it is not yet a tool someone would choose to live in all
day.

## Behaviour

Layout, wording and flow follow HubSpot Help Desk (spec 002) wherever HubSpot
has the behaviour. HubSpot has no keyboard shortcuts and no collision warnings
on tickets; those follow Help Scout, Intercom and Plain, which agree on most
keys.

**Ticket numbers**

1. Every ticket has a number, per workspace, counting from 1 in order of
   arrival: `#1042`. Existing tickets are numbered by creation time. The number
   shows in the list row, the thread header and the tab title, and search finds
   it with or without the `#`. Reply subjects are unchanged (spec 002 §17).

**Views**

2. The left pane lists **Unassigned**, **Assigned to me**, **All open**,
   **Snoozed**, **Drafts**, **Closed** and **All tickets**, then **My views**,
   then **Team views**. Unassigned, Assigned to me, All open, Drafts and every
   saved view show a count; Snoozed, Closed and All tickets do not. Drafts —
   tickets where the agent has an unsent draft (§32) — shows only while it has
   any.
3. Open still means any status but Closed (spec 002 §10), but a snoozed ticket
   shows only in Snoozed, Drafts and All tickets until it wakes (§25) — not in
   the other views or their counts.
4. **Saved views.** Any filtered, searched or sorted list can be saved: "Save
   view" asks for a name (1–40 characters) and stores the base view, filters,
   search and sort. A saved view is private unless "Share with team" is ticked.
   My views lists the agent's own, shared ones marked as such; Team views lists
   those other agents shared, each with its creator's name. Hovering a saved
   view shows its filters. Only the creator renames, shares, updates or deletes
   a view. At most 30 per agent. A removed agent's shared views stay in Team
   views, and nobody can change them.
5. Changing the filters of an open saved view shows "Update view · Save as new ·
   Reset" above the list; nothing is saved until one is chosen.
6. **Owner: Me** in a saved view means whoever is looking, so one shared "My
   urgent tickets" works for the whole team (HubSpot's "dynamic view filtered
   by ticket owner").

**Filtering and sorting**

7. Above the list: the search box, then filter chips — **Status**, **Owner**,
   **Priority**, **Category**, **Created** — and **More** (Unread only, Seen
   before). Each chip opens a checklist. Several values in one chip match any
   of them; different chips must all match. Owner offers Me, Unassigned, then
   the agents; Priority and Category offer None; Category lists archived
   categories too, under a divider (spec 004 §3).
8. An active chip names its values ("Priority: Urgent, High") and has a × that
   clears it. "Clear all" clears every chip and the search.
9. Created offers Last 24 hours, Last 7 days and Last 30 days.
10. **Sort** menu: **Last activity** (newest first, the default), **Oldest
    activity**, **Waiting longest** (§16), **Priority** (Urgent first, then
    last activity), **Created** (newest first). The time on each row follows
    the sort: last activity, waiting since, or created.
11. Filters, search and sort live in the URL: back and forward work, reloading
    keeps them, and a copied link opens the same list for a colleague.
12. A filter naming an agent or category that no longer exists matches nothing
    rather than failing, so a shared view outlives a removed agent.

**Search**

13. The search box (`/` focuses it) matches the ticket number, subject,
    contact name and email, and the text of every message, internal comments
    included — case-insensitive, anywhere in the word. It searches within the
    current view and filters, and updates 250 ms after the agent stops typing.
    When a message matched, the row's snippet comes from the newest matching
    message, around the match, instead of the last message (as HubSpot's
    search does). Matches are highlighted in the subject, contact and snippet.
14. No results: "No tickets match "sso" in Assigned to me", with **Search all
    tickets**, which keeps the query and switches to All tickets.

**The list**

15. A row shows: checkbox, unread dot, the contact's avatar and name, the
    ticket number, the time (§10), the subject with the "seen before" mark
    (spec 003), a snippet of the last message — prefixed "Vetle:" when an agent
    wrote it and "Note · Vetle:" for an internal comment — then status,
    priority, category, the owner's avatar or "Unassigned", and who else is on
    it (§29).
16. **Waiting.** A ticket that is New or Waiting on us shows how long the
    customer has been waiting, "Waiting 3 h": since the first customer message
    after the team's last reply (an agent reply, not an internal comment). If
    there is no such message — an agent set Waiting on us by hand after the team
    had the last word — it counts from when the status was set. Further
    customer messages and internal comments do not restart it. So undoing a
    close or an unsent reply (§24, §34) keeps the original clock. Past 24 hours
    the badge turns red. It is a clock, not an SLA.
17. **Unread.** An open ticket with a customer message the agent has not seen
    since they last opened it is unread for that agent: bold, with a dot. So is
    one whose snooze ended since (§25). Opening a ticket marks it read for that
    agent only. Clicking the dot toggles read and unread; **Mark as unread**
    (`Shift+U`, or the ticket's ⋯ menu) returns to the list with it unread,
    as if its newest customer message had not been seen. A snooze that ended
    before that message does not come back as "Snooze ended".
18. Hovering a row shows **Assign to me**, **Close** and **Snooze**. A row is
    still a link: Cmd/Ctrl-click and middle-click open it in a new tab.
19. **The list does not move under the agent.** While they are scrolled away
    from the top, have rows selected, or are moving with `j`/`k`, tickets that
    arrive wait behind a "3 new tickets" pill at the top; clicking it (or
    scrolling to the top with no selection) shows them. Rows already shown
    update in place.
20. Loading shows skeleton rows, never a blank pane. Changing filters keeps the
    old rows, dimmed, until the new ones arrive.
21. Empty states: each built-in view keeps its sentence (spec 002 §13); a
    filtered or searched list with nothing in it says "No tickets match these
    filters" with **Clear all**; Snoozed says "Nothing snoozed. Snooze a ticket
    to hide it until it needs you."

**Selecting and bulk actions**

22. A row's checkbox shows on hover, and on every row once one is selected.
    Shift-click selects a range; the header checkbox selects every loaded row.
23. With a selection, the list header becomes an action bar: "12 selected",
    **Assign**, **Status**, **Priority**, **Category**, **Snooze**, and ×
    to clear. At most 200 tickets at a time. All or nothing: if one ticket
    cannot change, none do, and the bar says why. Snooze is disabled while the
    selection includes a closed ticket.

**Undo and feedback**

24. A change of status, owner, priority, category or snooze — from a row, the
    action bar, the sidebar or a shortcut — shows at once, then saves. A toast
    confirms it ("Closed #1042", "Assigned 12 tickets to Vetle") with **Undo**
    for 8 seconds; `z` undoes the most recent. Undo puts back each ticket's
    previous values. If saving fails, the screen reverts and the toast says
    "Could not save." and why ("A closed ticket cannot be snoozed.", "That
    category is archived or gone."), or "Try again." when there is no reason
    to give. An archived category or a removed owner cannot be put back by
    Undo, which says so. Toasts sit bottom-left, at most three stacked, stay
    while hovered, and are announced to screen readers.

**Snooze**

25. Snooze (HubSpot's, for everyone) offers **Later today** (3 hours from
    now), **Tomorrow** (09:00), **Next week** (Monday 09:00) and **Pick a date
    and time**, in the agent's local time. A snoozed ticket leaves its views
    for Snoozed, showing "Until Mon 09:00", and its header reads "Snoozed until
    Mon 09:00 · Unsnooze". When the time comes it wakes: back in its views, at
    the top (its last activity is the wake time), unread for everyone, marked
    "Snooze ended" until opened. A snooze whose time has passed is over even if
    the wake has not run yet: the ticket is back in its views, just not at the
    top.
26. A customer reply wakes it at once, and spec 002 §6 applies. Closing it or
    **Unsnooze** ends the snooze. An agent's own reply or comment leaves it
    alone, so reply-then-snooze and snooze-then-note both work.
27. A closed ticket cannot be snoozed. The time must be in the future and at
    most a year away.

**The ticket**

28. The header shows `#1042`, the subject and status, **Assign to me** when
    the ticket is not the agent's, up/down arrows with "3 of 42" for moving
    through the list it was opened from (the rows loaded so far, "3 of 50+"
    while there are more), and a ⋯ menu: Copy link, Mark as unread, Snooze or
    Unsnooze.
29. **Presence.** Other agents who have had the ticket open in the last 30
    seconds show as avatars in the header ("Vetle is viewing"), and as an eye
    on its list row; marking it read or unread from the list is not viewing it. One whose draft on it changed in the last minute is
    replying: a pencil on the row, and "Vetle is replying" above the composer.
    This rides the 10-second poll (ADR 0007), so it can lag by that.
30. The thread opens scrolled to the newest message. With more than six
    messages, the first and the last three show and the rest collapse into
    "Show 4 earlier messages". Day separators — Today, Yesterday, Mon 28 Sep —
    sit between messages from different days. A message that arrives while the
    agent is scrolled up shows a "1 new message ↓" pill instead of moving the
    thread.
31. Closing or snoozing the open ticket — by shortcut, header or Send and close
    — keeps it open, as Help Scout does by default. If it has left the list,
    `j` opens the ticket that followed it and `k` the one before.

**Composing**

32. **Drafts.** What the agent types is saved a second after they stop, per
    agent and ticket, with the Reply/Internal comment choice. It is private to
    them (as in HubSpot) and survives moving to another ticket, reloading and
    another device; a quiet "Draft saved" shows under the box. Sending clears
    it. A draft that could not be saved shows "Draft not saved", is retried on
    the next keystroke, and reloading or closing the tab with it unsaved asks
    the browser's "Leave page?".
33. Cmd/Ctrl+Enter sends. The Send button has a menu with **Send and close**
    (Cmd/Ctrl+Shift+Enter): the reply goes out and the ticket closes. The
    Internal comment composer stays yellow, like comments in the thread.
34. **Undo send.** A reply waits 10 seconds before it goes out. The toast
    reads "Sending… Undo" and the message shows "Sending". Undo removes the
    reply, puts its text back in the composer, and puts the ticket's status and
    owner back to what they were. Too late — already on its way — says so.
    Retry (spec 002 §18–19) sends at once; only a new reply waits.
35. A failed or held reply offers **Discard** next to Retry (spec 002 §18–19).
    Discarding puts its text back in the composer when the composer is empty.
    On a closed ticket the discarded reply leaves the brain too (ADR 0010).
36. **Someone got there first.** If a customer message, or another agent's
    reply or comment, arrived after the thread was last loaded, Send stops:
    "New activity on this ticket" shows the new messages, and the button
    becomes **Send anyway**.
37. **Snippets.** Typing `#` at the start of a word (HubSpot's key), or the
    Snippets button, opens the workspace's snippets, filtered as the agent
    types by name and text; Enter inserts one at the cursor. `{{contact.firstName}}`,
    `{{contact.name}}`, `{{agent.firstName}}` and `{{agent.name}}` are filled
    in on insert; a value that is missing becomes empty. The first name is the
    name's first word. Snippets are human-written (ADR 0002).
38. Settings → Snippets: any agent adds, edits and deletes them. Name 1–60
    characters, text 1–5000.

**Keyboard**

39. Single-key shortcuts work when focus is not in a text field; Cmd/Ctrl
    combinations work everywhere. With rows selected, the ticket actions apply
    to the selection; otherwise to the open ticket, or in the list to the
    focused row. A menu opened by a shortcut filters as the agent types and
    Enter picks, so `a` `v` `e` `t` `Enter` assigns to Vetle.

    | Keys | Does |
    |---|---|
    | `?` | Show all shortcuts |
    | `/` | Search |
    | `Cmd/Ctrl+K` | Command palette (§40) |
    | `g` then `u` `m` `o` `s` `d` `c` `a` | Go to Unassigned, Assigned to me, All open, Snoozed, Drafts, Closed, All tickets |
    | `j` / `k`, `↓` / `↑` | Next / previous ticket; with a ticket open, opens it |
    | `Enter`, `o` | Open the focused ticket |
    | `Esc` | Close the open menu, else leave the composer, else back to the list |
    | `x` | Select or deselect the focused ticket |
    | `r` | Reply |
    | `n` | Internal comment |
    | `a` | Assign… (Me first) |
    | `e` | Close |
    | `s` | Status… |
    | `p` | Priority… |
    | `c` | Category… |
    | `b` | Snooze… |
    | `Shift+U` | Mark as unread |
    | `z` | Undo |
    | `Cmd/Ctrl+Enter` | Send |
    | `Cmd/Ctrl+Shift+Enter` | Send and close |

40. **Command palette.** `Cmd/Ctrl+K` opens one box that finds tickets (by
    number, subject or contact, over All tickets, the first eight), views
    (built-in and saved) and actions on the open ticket, each action showing
    its shortcut beside it. Arrows and Enter pick.
41. Settings → Profile has **Keyboard shortcuts: On / Off**, stored on this
    device. Off disables the single-key ones, for screen reader users (WCAG
    2.1.4); Cmd/Ctrl combinations stay.

**Small things**

42. The tab title is "(3) Unassigned · Muninn", the count of the current
    view, or "#1042 SSO login fails · Muninn" with a ticket open.
43. Relative times tick every minute without a refetch; hovering any of them
    shows the exact time.
44. Avatars are initials on a colour derived from the email, the same colour
    everywhere the person appears.
45. Copy buttons on the ticket link and the contact's email, confirming
    "Copied".
46. Losing the connection shows a thin "Reconnecting…" bar above the list and
    keeps everything on screen; it goes when a poll succeeds.
47. Focus is always visible, every shortcut has a mouse path, and colour is
    never the only signal: unread is bold as well as dotted, priority has a
    label as well as a colour, presence has an icon and a tooltip.

## Contract

Conventions as spec 001 and 002. Every change is additive — new query
parameters, fields and routes — so no ADR.

### Ticket, new fields

```json
{
  "number": 1042,
  "unread": true,
  "waitingSince": "2026-09-29T09:12:40Z",
  "snoozedUntil": null,
  "snoozeEnded": false,
  "viewers": [ { "id": "c41a…", "name": "Vetle", "replying": true } ],
  "lastMessage": { "kind": "agent", "author": "Vetle", "snippet": "Hi Ola, could you check…", "at": "2026-09-29T10:02:13Z" },
  "searchMatch": null
}
```

`unread`, `snoozeEnded` and `viewers` are relative to the agent making the
request (§17, §25, §29). `waitingSince`, in `new` and `waitingOnUs`: the
first customer message after the last agent reply, or when the status was set
if that is earlier (§16); worked out from the thread, so removing a reply
(`DELETE /api/messages/{id}`) moves it back. `null` in any other status. `viewers`: other agents who fetched this ticket's detail in the last 30
seconds; `replying` when their draft on it changed in the last 60.
`lastMessage.author` is the display name of whoever wrote it. `searchMatch` has
`lastMessage`'s shape — the newest message matching `q`, its snippet around the
match — and is `null` without `q` or when only the subject, number or contact
matched (§13).

### `GET /api/tickets`

| Parameter | Values | Default |
|---|---|---|
| `view` | `unassigned` · `mine` · `open` · `snoozed` · `drafts` · `closed` · `all` | required |
| `q` | 1–200 characters | none |
| `status` | comma-separated statuses | any |
| `owner` | comma-separated: `me`, `none`, agent ids | any |
| `priority` | comma-separated: `none`, `low`, `medium`, `high`, `urgent` | any |
| `category` | comma-separated: `none`, category ids | any |
| `created` | `24h` · `7d` · `30d` | any |
| `unread` | `true` | any |
| `seenBefore` | `true` | any |
| `sort` | `recent` · `oldest` · `waiting` · `priority` · `created` | `recent` |
| `cursor` | a `nextCursor` from the same query | first page |

`waiting` puts tickets that are not waiting after the waiting ones, by `recent`.
`q` of `#1042` or `1042` also matches ticket number 1042; any `q` is the
substring search of §13. An empty parameter is the same as leaving it out;
`unread` and `seenBefore` take only `true`, anything else is `invalidFilter`.

`200` — as spec 002, with more counts:

```json
{
  "tickets": [ { "…": "Ticket" } ],
  "nextCursor": null,
  "counts": { "unassigned": 3, "mine": 5, "open": 12, "drafts": 1, "views": { "8c1e…": 4 } },
  "hasTickets": true
}
```

`counts` ignore the request's own filters. `views` has an entry for every saved
view the agent can see.

`400 {"error":"invalidView"}` · `400 {"error":"invalidFilter"}` (a malformed
value) · `400 {"error":"invalidSort"}` · `400 {"error":"invalidCursor"}` (also
a cursor from a different sort). An id that does not exist is not an error: it
matches nothing (§12).

### `GET /api/tickets/{id}`, new field

Also marks the ticket read for the agent and records them as viewing (§17, §29).

```json
{ "draft": { "mode": "reply", "text": "Hi Ola, re-upload the certificate…", "updatedAt": "2026-09-29T14:02:11Z" } }
```

`draft`: the requesting agent's, or `null`. `mode`: `reply` · `comment`.

### `PATCH /api/tickets/{id}`, new field

```json
{ "snoozedUntil": "2026-09-30T07:00:00Z" }
```

`null` unsnoozes. `"status": "closed"` also clears it.

`400 {"error":"invalidSnooze"}` (not in the future, or more than 365 days
ahead) · `409 {"error":"ticketClosed"}` (snoozing a closed ticket, or closing
and snoozing in one request)

### `PATCH /api/tickets` — several at once

```json
{
  "tickets": [
    { "id": "5d2c…", "status": "closed" },
    { "id": "19c3…", "ownerId": null, "priority": "high" }
  ]
}
```

Each item is an `id` and any fields of the single `PATCH`. 1–200 items, no id
twice. One transaction: all change or none. Undo sends each ticket's previous
values through this route (§24).

`200 {"tickets": [Ticket, …]}` in request order.

`400 {"error":"invalidBatch"}` (empty, over 200, or an id twice) · any error
of the single `PATCH` · `404` when any id is not a ticket in this workspace.

### `PUT /api/tickets/{id}/read` · `DELETE /api/tickets/{id}/read`

Mark read or unread for the requesting agent, without opening it (§17).
Neither records the agent as viewing (§29). Read covers the ticket's newest
message and snooze end; unread goes back to just before its newest customer
message. `GET /api/tickets/{id}` reads it the same way.

`204` · `404`

### `PUT /api/tickets/{id}/draft`

```json
{ "mode": "reply", "text": "Hi Ola, re-upload the certificate…" }
```

`204`. Text that is empty after trimming deletes the draft.

`400 {"error":"invalidMode"}` · `404`

### `POST /api/tickets/{id}/replies`, new fields

```json
{ "text": "Hi Ola, re-upload the certificate…", "status": "closed", "after": "f19b…" }
```

`status`: `waitingOnContact` (the default, spec 002 §16) or `closed` (§33).
`after`: the newest message the agent has seen; omitted, nothing is checked.
The send job is due 10 seconds after the reply is stored (§34). A reply or a
comment deletes the agent's draft on the ticket, and neither touches a snooze
(§26).

`status: "closed"` indexes the thread, the new reply included, into the
brain, also when the ticket was closed already (ADR 0010).

`409 {"error":"newActivity"}` — a message newer than `after`, written by
someone else, exists; nothing is stored (§36) · `400 {"error":"invalidStatus"}`
· `400 {"error":"invalidAfter"}` (not a message on this ticket)

### `DELETE /api/messages/{id}`

Undo send and Discard (§34–35): an agent reply that is `queued` and not yet
picked up for sending, `held`, or `failed`. Status and owner are put back by
the client, through `PATCH /api/tickets`.

`204` · `409 {"error":"notDeletable"}` (being sent, sent, or not an agent
reply) · `404`

### Saved views

```json
{
  "id": "8c1e…",
  "name": "Urgent billing",
  "shared": true,
  "createdBy": { "id": "c41a…", "name": "Vetle" },
  "filters": {
    "view": "open",
    "q": null,
    "status": [],
    "owner": ["none"],
    "priority": ["urgent", "high"],
    "category": ["0b6f…"],
    "created": null,
    "unread": false,
    "seenBefore": false,
    "sort": "priority"
  }
}
```

`filters` carries the same values as `GET /api/tickets`'s parameters, as JSON.

`GET /api/views` → `200 {"views": [View, …]}`: the agent's own, then those
others shared, each group by name. Another agent's private view does not exist
(`404`).

`POST /api/views` with `{ "name", "shared", "filters" }` → `201` View.

`PATCH /api/views/{id}` with any of `{ "name", "shared", "filters" }` → `200`
View.

`DELETE /api/views/{id}` → `204`.

`400 {"error":"invalidName"}` (1–40 characters after trimming) · `400
{"error":"invalidFilter"}` · `403 {"error":"notYours"}` (changing another
agent's shared view) · `409 {"error":"tooManyViews"}` (30 per agent) · `404`

### Snippets

```json
{ "id": "3fa0…", "name": "SSO cert rotation", "text": "Hi {{contact.firstName}},\n\nRe-upload the certificate…", "updatedAt": "2026-09-29T12:00:00Z" }
```

`GET /api/snippets` → `200 {"snippets": [Snippet, …]}` by name.

`POST /api/snippets` with `{ "name", "text" }` → `201` Snippet.

`PATCH /api/snippets/{id}` with any of `{ "name", "text" }` → `200` Snippet.

`DELETE /api/snippets/{id}` → `204`.

`400 {"error":"invalidName"}` (1–60 characters after trimming) · `400
{"error":"invalidText"}` (1–5000) · `404`

### Cross-tenant surface (ADR 0003)

Nothing new. Waking a snoozed ticket is a `wake` job on the existing `jobs`
table, due at `snoozedUntil`; it does its work in the ticket's tenant
transaction and does nothing if the ticket was unsnoozed, re-snoozed or closed
meanwhile.

## Out of scope

- Tags. Categories classify (spec 004); tags come when one category per ticket
  stops being enough.
- SLAs, business hours, breach alerts. "Waiting" is a clock, not a target.
- Notifications of any kind — email, desktop, @mentions.
- An activity log in the thread (who changed status or owner, when). It needs
  an event table.
- Merging, splitting, deleting, blocking senders and marking tickets as spam.
  Spam needs its own decision: a closed ticket is in the brain (ADR 0010).
- Reopening into a new linked ticket after a time window (HubSpot's reopen
  setting); a customer reply always reopens (spec 002 §6).
- Editing or deleting comments and sent replies.
- Selecting every ticket in a view beyond the loaded ones; bulk actions over a
  filter.
- Search operators (`from:`, `status:`), exact-phrase quotes, relevance
  ranking, searching attachments.
- Board and table layouts, column pickers, density settings, reordering views,
  view folders, per-team views, workspace default views.
- Remapping shortcuts (on or off only); status chords.
- Send and snooze; per-agent after-send preferences; moving to the next ticket
  on close.
- Live presence without polling (ADR 0007).
- Narrow screens, dark mode, resizable panes.
- Jev suggesting priority, snooze or snippets.

## Open questions

None.

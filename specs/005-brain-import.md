# 005. Filling the brain from a mailbox export

Status: Draft

## Problem

A new workspace's brain is empty, so the copilot shows nothing until the team
has closed enough tickets for problems to repeat — weeks (ADR 0010). The years
of problems the team already solved sit in their old support mailbox, where
Muninn cannot see them.

## Behaviour

**The export**

1. The customer exports their support mailbox, received and sent mail both, as
   mbox: Google Takeout for Gmail and Google Workspace, Export in Thunderbird or
   Apple Mail, `readpst` for an Outlook `.pst`. The file is their customers'
   email; the operator agrees with them how it is handed over (open question 1),
   copies it onto the production box, and deletes it from the box once the
   import is done. Muninn keeps only what it imported.
2. During the beta the operator runs the import on the box:
   `muninn admin import <slug> <export.mbox>… --team <address or @domain>…`
   (Contract). `--team` says who the support team is: exact addresses
   (`support@acme.com`) or whole domains (`@acme.com`). Mail from a team address
   is a team reply; mail from anyone else is the customer.

**What becomes a case**

3. Automatic mail is dropped before anything else: `Auto-Submitted` other than
   `no`; `Precedence` `bulk`, `junk`, `list` or `auto_reply`; `X-Autoreply` or
   `X-Autorespond`; delivery reports (`Content-Type: multipart/report`, or from
   `mailer-daemon@` or `postmaster@`).
4. Messages are grouped into threads by `Message-ID`, `In-Reply-To` and
   `References`. A message whose parent is not in the export starts a thread of
   its own. Subjects do not thread: "Re: Invoice" would join strangers.
5. A thread becomes a ticket only if its first message is from a customer and a
   team reply follows — a solved case. Unanswered threads and threads the team
   started (campaigns, notifications, mail between colleagues) are skipped.
6. The ticket is closed, with no owner, priority or category. Its subject is the
   first message's; its contact is the first message's sender, created or
   reused as for inbound mail (spec 002 §5). It was created at the first
   message's `Date` and closed, and last active, at the last message's. Jev is
   not asked about it: no category (spec 004) and no suggestions of its own
   (spec 003) — an import of five thousand tickets is not ten thousand Jev
   calls. It is marked as imported, so a mistaken import can be deleted by hand
   with one statement.
7. Each message keeps its `Date`, sender name and address, `Message-ID` and
   text: the `text/plain` part, or the HTML part turned into text as for inbound
   mail (spec 002 §7). HTML is not stored and attachments are not imported: the
   brain reads text, and exports run to gigabytes of PDFs.
8. Quoted history is cut from each message's text, from the first of: a line
   ending in `wrote:` (`On Mon, 3 Mar 2025 … wrote:`); a line containing `skrev`
   and ending in `:` (Norwegian, Danish and Swedish clients); `-----Original
   Message-----` or `-----Opprinnelig melding-----`; an Outlook header block
   starting `From:`/`Fra:` followed by `Sent:`/`Sendt:`. Lines starting with `>`
   go wherever they are. Without this every case repeats its thread several
   times, and the "Solution" shows the customer's own question quoted.
9. Team replies are agent messages without an agent: the author shown in the
   thread and as the case's solution (spec 003) is the mail's From name, or its
   address when it has none. They count as sent at their `Date` and are never
   sent again.
10. The thread is indexed into the brain when its ticket is created, exactly as
    closing a ticket indexes it (ADR 0010), and the brain size in the sidebar
    counts it at once.
11. Importing the same export again adds nothing twice: a thread whose first
    message's `Message-ID` the workspace already has is skipped. A message
    without a `Message-ID` gets one made from a hash of its sender, date and
    text, so it dedupes too.
12. A customer who later answers an imported thread — their mail's
    `In-Reply-To` names an imported message — reopens that ticket like any other
    (spec 002 §4, §6), and it leaves the brain until it closes again.
13. Each thread is one transaction: a thread that cannot be read is reported and
    skipped, and the rest import. The command prints progress, then a summary:
    messages read, threads, tickets and messages imported, and threads skipped
    by reason.
14. An export of several gigabytes works: the file is streamed, not loaded.
15. The import writes through the tenant helper as `muninn_app`, like inbound
    mail (ADR 0003). It calls no external service.

## Contract

### `muninn admin import`

```sh
docker compose -f deploy/compose.yaml exec app \
  muninn admin import acme /imports/acme-support.mbox --team support@acme.com --team @acme.com
```

Progress goes to stderr; the summary to stdout:

```
acme: read 48210 messages in 17904 threads
imported 6112 tickets (21877 messages)
skipped 11792 threads: 9810 no team reply, 1544 started by the team, 31 already imported, 5 unreadable
dropped 402 automatic messages
brain: 6140 cases
```

Exit `0` when the import ran, however many threads were skipped. Exit `1`, with
one line on stderr and nothing imported, for `no workspace acme`, `cannot read
/imports/acme-support.mbox`, or `--team is required`.

The file must be inside the app container: mount a directory or
`docker compose cp` it there first.

### HTTP

No new routes. In spec 002's `Message`, an imported team reply is `kind:
"agent"` with `author` the mail's From name and address and `delivery: {
"status": "sent", "error": null }`. Spec 003's `solution.author.name` is that
same From name.

## Out of scope

- Upload from the product (Settings → Import). The operator imports during the
  beta.
- Other sources: Zendesk, HubSpot or Freshdesk exports, IMAP, `.eml` folders,
  `.pst` without `readpst`.
- Attachments, HTML bodies, internal notes from another help desk.
- Categorising imported tickets or judging them with Jev; choosing a better
  solution than the last team reply (ADR 0010).
- Undoing an import from the command line; showing in the UI that a ticket was
  imported.
- Matching team senders to Muninn agents: the author is the name on the mail.

## Open questions

1. How the customer hands the export over, and where it waits on the box — it
   holds their customers' personal data (their DPA with Muninn covers the
   processing).

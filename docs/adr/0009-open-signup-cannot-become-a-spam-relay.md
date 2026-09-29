# 9. Open signup cannot become a spam relay

Date: 2026-09-29

## Status

Proposed

## Context

Anyone can sign up (ADR 0003) and every workspace sends mail through our Postmark
account from our domain (ADR 0005). A spammer can sign up, mail their own
inbound address with a forged `From: victim@example.com`, and "reply" spam to the
victim through us. Postmark suspends accounts that generate complaints, and a
suspension takes every workspace's mail down, paying customers included.

Rejected:

- **Manual approval of every new workspace.** Safe, but it contradicts
  self-serve signup and puts a human on the critical path of every trial.
- **A card at signup.** Stops most abuse and most conversions with it.
- **Content filtering of outbound mail.** An arms race we would lose, and a
  source of false positives on legitimate replies.

## Decision

Three limits, all shipping with the first outbound email:

1. **Reply-only.** Agents can only reply to an existing ticket. There is no
   "compose to any address" in the product.
2. **A trial send cap.** A workspace without an active subscription can send at
   most 100 emails per rolling 24 hours; the 101st reply is stored but not sent,
   with delivery `held`, and goes out when the agent retries after the window
   has room. The cap lifts when the subscription is active.
3. **A separate Postmark message stream for trial workspaces.** Complaints and
   bounces from trials accumulate on that stream, not on the one paying
   customers send through.

## Consequences

A spammer gets at most 100 messages a day per verified email address before
paying, on a stream isolated from customers. A real trial team of three agents
does not come near the cap.

Reply-only still lets someone reply to a forged `From`. The cap bounds that; it
does not prevent it. If abuse appears inside the cap, the next step is checking
SPF/DKIM on inbound mail before a ticket can be replied to — a new ADR, because
it can refuse legitimate mail from badly configured senders.

The cap is a counter over outbound messages in the last 24 hours, read on every
send. Cheap, and correct across restarts because it is in Postgres.

Found before launch, and dormant while signup is closed for the beta (spec 001
§29): two more ways to make us mail anyone, both on the *system* stream that
carries every workspace's login links. Invites are uncapped and put the
workspace's and inviter's own words in the subject, so one signup sends
unlimited mail. Signup links reach any address — capped per address, not per
IP — with the chosen workspace name in the subject. Before signup opens: cap
invites for workspaces without a subscription, cap signup per client IP, and
send both on the trial stream.

---
description: Record an architecture decision in docs/adr/. Use for a decision that is expensive to reverse (datastore, auth, tenancy, deploy target, a new model vendor, a change to a contract's shape), not for library picks.
argument-hint: <decision title>
---

Record an ADR for: $ARGUMENTS

1. `ls docs/adr/` — next number, `NNNN-kebab-title.md`.
2. Same structure as `docs/adr/0001-use-adrs.md`. Status starts `Proposed`.
3. Context states the forces and constraints, not the answer. Decision is one
   choice in the active voice. Consequences includes what this makes harder.
4. Name the options you rejected and why, inside Context. An ADR without a
   rejected alternative is a description, not a decision.
5. If this supersedes an existing ADR, set the old one's status to
   `Superseded by NNNN` — that is the only edit ever allowed to an accepted ADR.

Ask the user before flipping Proposed to Accepted.

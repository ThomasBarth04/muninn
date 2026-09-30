---
description: Implement an Agreed spec end to end — migration, contract structs, handlers, tests, frontend. Use when the user asks to build or implement a feature that has a spec in specs/.
argument-hint: <spec number or name>
---

Implement: $ARGUMENTS

1. Read the spec. If its Status is not `Agreed`, stop and ask; if no spec
   covers the work, use the `spec` skill instead. Read the ADRs it names.
2. Data first. A migration if the schema changes (`.claude/rules/migrations.md`).
3. Contract. Every request and response in the spec's Contract becomes a
   struct in `backend/src/api.rs`, field for field, then `cargo test` to
   regenerate `frontend/src/api/types/`. If the code needs a shape the spec
   does not have, stop: the spec changes first (a shape change needs an ADR).
4. Handlers, in the module that owns the area, through `tenant_tx`, with the
   error codes the spec names.
5. One integration test in `backend/tests/` per numbered Behaviour scenario,
   failures included, named after what the user sees.
6. Frontend against the generated types (`.claude/rules/frontend.md`).
7. `/ci`, then hand the diff to the `rules-reviewer` agent and fix what it finds.
8. Anything learned while building that changes behaviour or contract goes
   into the spec now, in the same commit. Ask the user before setting
   `Status: Shipped`.

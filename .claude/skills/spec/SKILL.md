---
description: Write specs/NNN-name.md before any feature code. Use when the user asks to build a new feature, or to change behaviour or an HTTP contract that no Agreed spec covers.
argument-hint: <feature name>
---

Write a spec for: $ARGUMENTS

1. `ls specs/` — next free number, `NNN-kebab-name.md`.
2. Copy `specs/TEMPLATE.md`, fill every section.
3. Interview the user for anything you had to guess. Do not invent the contract
   and do not leave a section as a placeholder — an unanswered question goes in
   Open questions, not into a confident-sounding paragraph.
4. Stop after the spec. No implementation until the user marks it Agreed.

The Contract section is the interface between `frontend/` and `backend/`, so it
has to be concrete: real JSON, real status codes, real error shapes. If the
contract implies a decision that is expensive to reverse, say so and suggest an
ADR — do not write one unprompted.

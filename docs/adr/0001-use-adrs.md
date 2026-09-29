# 1. Record architecture decisions

Date: 2026-09-29

## Status

Accepted

## Context

Two codebases (React SPA, Rust service) share one contract, and a multi-tenant
SaaS accumulates decisions — tenancy, mail, model choice — whose reasoning
evaporates within weeks. Commit messages do not survive as an explanation.

## Decision

Record every hard-to-reverse decision as a numbered markdown file in
`docs/adr/`, using the structure of this file: Status, Context, Decision,
Consequences. This file is the template — copy it.

Status is one of `Proposed`, `Accepted`, `Rejected`, `Superseded by NNNN`.
Accepted ADRs are never edited; they are superseded.

## Consequences

Decisions get a paragraph of writing before they get code, which is the point.
Cheap, reversible choices do not get an ADR — the ceremony would cost more than
the decision.

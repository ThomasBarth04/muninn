# 2. Jev is the only model

Date: 2026-09-29

## Status

Proposed

## Context

The copilot has two jobs in the MVP: find past tickets that were the same
problem, and put the new ticket in a category. Neither needs text to be written.
Both are judgments — "same problem, yes or no", "which of these categories" —
over text that already exists.

Jev (TypeSafe AI, generally available 2026-09-20) is a decision model, not a
language model: it takes a `state` and typed questions (yes/no, choice up to 255
options, score) and returns values with calibrated probabilities. It cannot
generate text, so it cannot invent a solution. It answers in 70–500 ms at
$0.042 per million input tokens with output free — cheap enough to judge fifty
candidates per ticket. It has no embeddings, so it cannot find candidates by
itself; something else has to retrieve and Jev reranks. It is weak on numbers,
dates and adversarial input, and never explains a judgment.

Rejected:

- **An LLM (Claude, GPT) for drafting replies and summarising cases.** The brain
  is made of what the team actually wrote; showing Frank's real answer beats a
  paraphrase of it, and a generated draft is the one place the product could
  confidently say something false to a customer. Two model vendors is also two
  sets of prompts, evals and subprocessor terms before the first customer.
- **Embeddings + vector search for retrieval.** Needs an embedding model (a
  second vendor) and `pgvector`. Full-text search for candidates with Jev as the
  reranker is the pattern Jev's own launch material recommends, and keeps the
  whole AI surface to one HTTP call.
- **The third-party `jev` crate.** Two weeks old at the time of writing. The API
  is one POST; `reqwest` + `serde` is fewer moving parts than a dependency we
  would have to audit.
- **Jev through OpenRouter.** A middleman on every request and another company
  processing ticket content, for no feature we need.

## Decision

Jev, called directly at `api.typesafe.ai` with `reqwest`, is the only model in
Muninn. Nothing in the product generates text. Retrieval is Postgres full-text
search (ADR 0010); Jev judges what retrieval found.

## Consequences

The copilot can never hallucinate a solution: every word in the sidebar was
written by an agent on a real ticket. It also cannot help on a problem the team
has never solved — an empty brain shows nothing (ADR 0010).

Jev does not explain itself, so "why was this suggested" has no answer beyond
the score. Evals carry the weight explanations would: suggestion feedback and
category overrides are stored from day one (specs 003 and 004), and the
thresholds are constants tuned against that data.

Ticket content goes to TypeSafe, a US company. It is a subprocessor and has to
be named in the privacy policy and DPA before a paying customer's mail flows
through it. As of 2026-09-29 TypeSafe states it does not train on API inputs,
offers a DPA with EU standard contractual clauses, and retains data "as long as
necessary"; zero data retention is an enterprise agreement through their sales
team, worth asking for before the first EU customer with a DPA questionnaire.

Summarising Datadog logs, drafting bug reports and drafting replies all need a
text model. Adding one is a new ADR, not a quiet dependency.

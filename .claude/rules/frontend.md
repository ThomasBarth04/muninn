---
paths:
  - "frontend/src/**"
---

# Frontend

- Response and request types come from `src/api/types/` (ts-rs). A missing
  type means the Rust struct in `backend/src/api.rs` is missing: add it there
  and run `cargo test`. Never hand-write one.
- Every request goes through `api` in `client.ts`. Its `Content-Type:
  application/json` on mutations is the CSRF defence (ADR 0006); a raw `fetch`
  loses it.
- Mail bodies render as text (`white-space: pre-wrap`), never as HTML;
  attachments are plain download links. The hook denies `dangerouslySetInnerHTML`.
- Map `errorCode(e)` to the user-facing text the spec describes; every failing
  scenario in the spec's Behaviour has a visible state.
- Non-copilot UI mirrors HubSpot Help Desk: layout, wording, flow.
  Reuse the classes in `app.css`; no component library.
- TypeScript is v7 (native `tsc`); check with `npm run build`.

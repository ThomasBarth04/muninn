# Muninn

An AI-native help desk, sold as multi-tenant SaaS. Teams forward their support
inbox; tickets, replies and the inbox work like HubSpot Help Desk; the ticket
sidebar shows how the team solved the same problem before, judged by Jev from
the workspace's own closed tickets. Nothing generates text. Rules live in
[CLAUDE.md](CLAUDE.md), behaviour in [`specs/`](specs/), decisions in
[`docs/adr/`](docs/adr/).

## Local development

```sh
docker compose -f deploy/compose.dev.yaml up -d      # Postgres 17 on localhost:5432

cd backend
export MIGRATE_DATABASE_URL=postgres://muninn_owner:muninn@localhost:5432/muninn
export DATABASE_URL=postgres://muninn_app:muninn@localhost:5432/muninn
export POSTMARK_INBOUND_PASSWORD=dev
cargo run                                            # API on :3000

cd frontend && npm install && npm run dev            # SPA on :5173, proxies /api and /hooks
```

Without `POSTMARK_SERVER_TOKEN` no mail is sent: magic links are printed in the
backend log. Sign up at <http://localhost:5173/signup> and open the link from
the log. Without `JEV_API_KEY` suggestions and categories fail visibly; without
Stripe keys billing does.

Fake an inbound email to a workspace with slug `acme`:

```sh
curl -u postmark:dev -H 'content-type: application/json' \
  http://localhost:3000/hooks/postmark/inbound -d '{
    "OriginalRecipient": "acme@in.muninn.io",
    "FromFull": {"Email": "ola@kunde.no", "Name": "Ola Nordmann"},
    "ToFull": [{"Email": "acme@in.muninn.io", "Name": ""}],
    "CcFull": [], "Subject": "SSO login fails", "MailboxHash": "",
    "TextBody": "Hi, none of us can log in with SSO since this morning.",
    "HtmlBody": "", "StrippedTextReply": "",
    "Headers": [{"Name": "Message-ID", "Value": "<test-1@kunde.no>"}],
    "Attachments": []
  }'
```

Tests: `cargo test` runs unit tests and regenerates the TypeScript contract
types in `frontend/src/api/types/`. The integration tests need a Postgres
superuser URL; each test creates its own database:

```sh
docker run -d --name muninn-test-pg -p 55432:5432 -e POSTGRES_PASSWORD=postgres postgres:17-alpine
TEST_DATABASE_URL=postgres://postgres:postgres@localhost:55432/postgres cargo test
```

## Production

One Hetzner Cloud server in the EU running `deploy/compose.yaml`: the app,
Postgres with `wal-g`, a `wal-g` backup sidecar and Caddy (ADR 0008). CI builds
the app image from `main` and publishes it to
`ghcr.io/thomasbarth04/muninn` (`:latest` and `:<git sha>`).

1. The server: a Hetzner Cloud Firewall that allows only 22/tcp, 80/tcp,
   443/tcp and 443/udp; SSH with keys only (`PasswordAuthentication no`);
   `unattended-upgrades` on; Docker from Docker's apt repository.
2. `cp deploy/.env.example deploy/.env` and fill it in (URL-safe passwords).
3. DNS: an A record for `DOMAIN` → the server; an MX record for the inbound
   domain (`in.muninn.io`) → `inbound.postmarkapp.com`; a DMARC record
   (`_dmarc`, starting at `v=DMARC1; p=none; rua=mailto:…`) for `muninn.io` and
   `in.muninn.io`.
4. Postmark: inbound webhook
   `https://postmark:<POSTMARK_INBOUND_PASSWORD>@<DOMAIN>/hooks/postmark/inbound`
   on the inbound domain; three outbound message streams — system mail
   (`POSTMARK_SYSTEM_STREAM`), paying customers and trials (ADR 0009). Verify
   **two** sending domains (DKIM and Return-Path): the one in `MAIL_FROM`, and
   the inbound domain itself — replies go out as
   `<slug>+<token>@in.muninn.io` (ADR 0005), and Postmark refuses them until
   `in.muninn.io` is verified.
5. Stripe: a per-seat monthly price (`STRIPE_PRICE_ID`) and a webhook to
   `https://<DOMAIN>/hooks/stripe` for `checkout.session.completed`,
   `customer.subscription.updated` and `customer.subscription.deleted`.
6. Deploy: `docker login ghcr.io` (a token with `read:packages`, unless the
   package is public), then
   `docker compose -f deploy/compose.yaml pull app && docker compose -f deploy/compose.yaml up -d`.
   The app runs migrations as `muninn_owner` on start and serves as
   `muninn_app`. On `SIGTERM` it finishes running requests and jobs (up to a
   minute) before it exits. To roll back, set `MUNINN_TAG` in `deploy/.env` to
   an earlier commit's sha and run the same two commands.
7. Monitoring: point an uptime monitor (Better Stack, UptimeRobot, …) at
   `https://<DOMAIN>/healthz` and alert on anything but `200`. It answers
   `503` with the reason when the database is unreachable, a job ran out of
   attempts in the last hour (`SELECT * FROM jobs WHERE failed_at IS NOT NULL`),
   WAL archiving is failing, or no base backup finished in the last 26 hours.
   Right after the very first start it is `503` until the first base backup
   is done, a few minutes. Container logs rotate at 5 × 10 MB per service.

Backups: WAL is archived continuously and a base backup is taken daily, 14
kept. Restore with [`deploy/restore.sh`](deploy/restore.sh) and run the
[monthly drill](deploy/RESTORE_DRILL.md) — once before the first customer, too.
A GDPR deletion request is
[`deploy/delete-workspace.sql`](deploy/delete-workspace.sql), run by hand.

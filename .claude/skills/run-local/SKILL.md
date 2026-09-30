---
description: Start muninn locally — dev Postgres, backend, Vite — with a workspace and a test ticket. Use when asked to run, start or try the app locally, or to see a change working in the browser.
allowed-tools: Bash(docker *), Bash(cargo *), Bash(npm *), Bash(curl *)
---

Local dev never reads `deploy/.env`; that file is the production box's.

1. Postgres 17 on `localhost:5432`, same roles as production:

   ```sh
   docker compose -f deploy/compose.dev.yaml up -d
   until docker compose -f deploy/compose.dev.yaml exec db pg_isready -q; do sleep 1; done
   ```

2. Backend environment. Shell state doesn't persist between Bash calls, so
   prefix every backend command with it:

   ```sh
   export MIGRATE_DATABASE_URL=postgres://muninn_owner:muninn@localhost:5432/muninn \
     DATABASE_URL=postgres://muninn_app:muninn@localhost:5432/muninn \
     POSTMARK_INBOUND_PASSWORD=dev APP_URL=http://localhost:5173
   ```

   `APP_URL` makes login links open the Vite server. `JEV_API_KEY` and
   `JEV_API_URL` come from `.claude/settings.local.json` `env`, which Claude
   Code already puts in the shell. Check with `echo ${JEV_API_KEY:+set}`. If
   they're unset the app still runs, but suggestions and categories fail.
   Say so, don't block.

3. Backend on `:3000`, in `backend/`, `run_in_background`: `cargo run`. It
   migrates on start. It's up when
   `curl -s localhost:3000/healthz` answers `ok`.

4. Workspace, in `backend/` with the same environment:

   ```sh
   cargo run -q -- admin create-workspace --name Acme --slug acme --language english --owner owner@acme.test
   ```

   `slug taken` or `already an agent` means it exists from an earlier run.
   Get a fresh link with `cargo run -q -- admin reset-login owner@acme.test`.
   No mail is sent without `POSTMARK_SERVER_TOKEN`; the setup link
   (`http://localhost:5173/auth?token=…`) is in the command's output.

5. Frontend on `:5173`, in `frontend/`: `npm install` if `node_modules/` is
   missing, then `npm run dev` with `run_in_background`. It proxies `/api` and
   `/hooks` to `:3000`.

6. A test ticket (change `Message-ID` for each new one; a repeat is deduplicated):

   ```sh
   curl -s -u postmark:dev -H 'content-type: application/json' \
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

   The copilot only suggests from closed tickets. For suggestions, close a few
   similar ones first, or fill the brain from an mbox with
   `cargo run -q -- admin import acme <file.mbox> --team @acme.com`.

7. Report: `http://localhost:5173`, the setup link, whether Jev is configured,
   and anything that failed, with the log lines. Login needs an authenticator
   app to scan the QR code. If every code is rejected, check the clock first:
   TOTP allows ±30 s, so an unsynced laptop clock (`timedatectl`) breaks it.

Stop: kill the background `cargo run` and `npm run dev`, then
`docker compose -f deploy/compose.dev.yaml down`. Data survives in the
`pgdata` volume. `down -v` wipes it; only do that if asked.

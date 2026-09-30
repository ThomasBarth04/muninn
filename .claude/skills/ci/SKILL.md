---
description: Run what CI runs, locally, against a real Postgres — before saying a change works or committing it. Use after any backend, migration or frontend change.
allowed-tools: Bash(docker *), Bash(cargo *), Bash(npm *), Bash(git *), Bash(python3 .claude/hooks/rules.py --selftest)
---

Mirror `.github/workflows/ci.yml`, plus fmt and clippy. Stop at the first
failure, fix it, rerun from that step.

1. Throwaway Postgres (each integration test creates its own database in it):

   ```sh
   docker start muninn-test-pg 2>/dev/null || docker run -d --name muninn-test-pg \
     -p 55432:5432 -e POSTGRES_PASSWORD=postgres postgres:17-alpine
   until docker exec muninn-test-pg pg_isready -q; do sleep 1; done
   ```

2. In `backend/`:

   ```sh
   cargo fmt --check
   cargo clippy --all-targets --locked -- -D warnings
   TEST_DATABASE_URL=postgres://postgres:postgres@localhost:55432/postgres cargo test --locked
   ```

   Without `TEST_DATABASE_URL` the integration tests pass without running.
   Never report a run without it as green.

3. Contract types: `git status --short frontend/src/api/types`. Changes there
   are the regenerated ts-rs types; they belong in the same commit as the Rust
   change (CI fails on a diff).

4. In `frontend/`: `npm run build`.

5. If `.claude/hooks/` changed: `python3 .claude/hooks/rules.py --selftest`.

Report what ran and what passed in one line per step. A skipped step is
reported as skipped, with why.

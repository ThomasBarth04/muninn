# Monthly restore drill

ADR 0008: a backup that has not been restored is a hope. Once a month, restore
production's backups onto a scratch server and look at the result. Skipping it
silently makes the ADR wrong.

1. Create a throwaway Hetzner Cloud server (any small type) with Docker, and
   clone the repo onto it.
2. Copy `deploy/.env` from production, but use an Object Storage key that can
   **only read** the backup bucket. The scratch Postgres archives WAL after it
   promotes; with a read-only key those pushes fail harmlessly instead of
   landing next to production's backups.
3. Build the database image: `docker compose -f deploy/compose.yaml build db`.
4. Restore to a recent moment, e.g. ten minutes ago:
   `deploy/restore.sh 2026-10-01T14:05:00Z`
5. Check what the script prints: the newest message sits just before the
   target, and the workspace and ticket counts match production (within ten
   minutes of traffic).
6. Write the date, the target, the time the restore took and the counts in
   the ops log. Anything surprising is an incident.
7. Delete the server.

A real restore on production is the same script on the production box:
`deploy/restore.sh latest` after a lost disk, or a timestamp just before a bad
migration or an accidental `DELETE`. The data it replaces is moved to a
`muninn_pgdata_before_restore_*` volume, not deleted.

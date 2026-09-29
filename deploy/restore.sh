#!/usr/bin/env bash
# Point-in-time restore from wal-g (ADR 0008).
#
#   deploy/restore.sh latest                  # everything that reached object storage
#   deploy/restore.sh 2026-10-01T14:05:00Z    # the database as it was at that moment (UTC)
#
# Run on the box (or a scratch server for the monthly drill) with deploy/.env
# in place. The current data is moved aside to a new volume, never deleted.
# The app stays stopped at the end: check the data, then start it.
set -euo pipefail
cd "$(dirname "$0")"

target="${1:?usage: restore.sh <latest|YYYY-MM-DDTHH:MM:SSZ>}"
if [[ "$target" != latest && ! "$target" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]]; then
  echo "target must be 'latest' or a UTC timestamp like 2026-10-01T14:05:00Z" >&2
  exit 1
fi

compose=(docker compose -f compose.yaml)
volume=muninn_pgdata
aside="muninn_pgdata_before_restore_$(date -u +%Y%m%dT%H%M%SZ)"
# Runs a shell in the db image with the db service's wal-g environment.
in_db() { "${compose[@]}" run --rm --no-deps --user postgres --entrypoint bash db -c "$1"; }

echo "==> stopping app, walg and db"
"${compose[@]}" stop app walg db

echo "==> moving current data aside to volume $aside"
docker volume create "$aside" >/dev/null
docker run --rm -v "$volume:/from" -v "$aside:/to" muninn-postgres \
  bash -c 'cp -a /from/. /to/ && find /from -mindepth 1 -delete'

# The base backup has to be older than the target; WAL replays forward from it.
if [[ "$target" == latest ]]; then
  backup=LATEST
else
  backup=$(in_db "wal-g backup-list 2>/dev/null" | awk -v t="$target" 'NR > 1 && $2 < t { b = $1 } END { print b }')
  if [[ -z "$backup" ]]; then
    echo "no base backup older than $target (see: wal-g backup-list)" >&2
    exit 1
  fi
fi

echo "==> fetching base backup $backup"
recovery="restore_command = 'wal-g wal-fetch %f %p'
recovery_target_action = 'promote'"
if [[ "$target" != latest ]]; then
  # Postgres wants "2026-10-01 14:05:00+00", not the ISO form wal-g lists.
  pg_time="${target/T/ }"
  recovery+=$'\n'"recovery_target_time = '${pg_time%Z}+00'"
fi
in_db "wal-g backup-fetch \"\$PGDATA\" $backup \
  && touch \"\$PGDATA/recovery.signal\" \
  && printf '%s\n' \"$recovery\" >> \"\$PGDATA/postgresql.auto.conf\""

echo "==> starting db, replaying WAL"
"${compose[@]}" up -d db
for ((i = 0; ; i++)); do
  "${compose[@]}" exec -T db psql -U postgres -d muninn -tAc 'SELECT NOT pg_is_in_recovery()' 2>/dev/null | grep -q t && break
  # A target past the end of the archived WAL makes Postgres stop instead of promote.
  if (( i > 720 )) || [[ -z "$("${compose[@]}" ps -q --status running db)" ]]; then
    echo "recovery did not finish; see: docker compose -f deploy/compose.yaml logs db" >&2
    exit 1
  fi
  sleep 5
done
# The recovery settings only mattered while recovery.signal existed; drop them
# so the next restart is an ordinary start.
# (Separate -c: ALTER SYSTEM refuses to run inside a multi-statement transaction.)
"${compose[@]}" exec -T db psql -U postgres -d muninn -q \
  -c 'ALTER SYSTEM RESET restore_command' \
  -c 'ALTER SYSTEM RESET recovery_target_action' \
  -c 'ALTER SYSTEM RESET recovery_target_time' \
  -c 'SELECT pg_reload_conf()'

echo "==> recovered. Before starting the app, check:"
"${compose[@]}" exec -T db psql -U postgres -d muninn -c \
  "SELECT (SELECT count(*) FROM workspaces) AS workspaces,
          (SELECT count(*) FROM tickets) AS tickets,
          (SELECT max(created_at) FROM messages) AS newest_message"
cat <<EOF

  - newest_message is close to $target (or to the failure, for 'latest')
  - the numbers look like production, not an empty database
  - the old data is in volume $aside until you remove it

Then:  docker compose -f deploy/compose.yaml up -d app walg
       (walg takes a fresh base backup on start, on the new timeline)
EOF

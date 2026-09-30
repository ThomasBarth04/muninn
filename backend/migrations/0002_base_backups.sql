-- The wal-g sidecar inserts a row after every successful base backup, as the
-- postgres superuser; /healthz reads the newest (ADR 0008). No customer
-- content, no tenant, no RLS.
CREATE TABLE base_backups (
    finished_at timestamptz NOT NULL DEFAULT now()
);

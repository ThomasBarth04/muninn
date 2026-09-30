-- Erase one workspace and everything in it: a GDPR deletion request
-- (spec 001 §29 — a documented, tested script run by hand).
--
-- Run as muninn_owner (bypasses RLS, owns the tables):
--   docker compose -f deploy/compose.yaml exec -T db \
--     psql -U muninn_owner -d muninn -v slug=acme -f - < deploy/delete-workspace.sql
--
-- One transaction. Every tenant table, sessions, setup links and jobs cascade
-- from workspaces; the other foreign keys (ticket owner, contact, category,
-- message author) point inside the same workspace and go in the same statement.
--
-- By hand afterwards, with the ids this prints:
--   1. Postmark: delete the sending domain (postmark_domain_id), if any.
--   2. Stripe: delete the customer (stripe_customer_id), which also cancels
--      the subscription and removes their card.
--   3. Tell the requester that backups age out after the wal-g retention
--      (14 daily base backups, see compose.yaml) and are not edited.

\set ON_ERROR_STOP on

BEGIN;

-- Stops here if the slug does not exist.
SELECT id AS ws FROM workspaces WHERE slug = :'slug' \gset

\echo 'Deleting workspace:'
SELECT id, name, slug, billing_status, stripe_customer_id, postmark_domain_id, sending_domain
FROM workspaces WHERE id = :'ws';

SELECT (SELECT count(*) FROM agents      WHERE workspace_id = :'ws') AS agents,
       (SELECT count(*) FROM contacts    WHERE workspace_id = :'ws') AS contacts,
       (SELECT count(*) FROM tickets     WHERE workspace_id = :'ws') AS tickets,
       (SELECT count(*) FROM messages    WHERE workspace_id = :'ws') AS messages,
       (SELECT count(*) FROM attachments WHERE workspace_id = :'ws') AS attachments,
       (SELECT count(*) FROM ticket_reads WHERE workspace_id = :'ws') AS ticket_reads,
       (SELECT count(*) FROM drafts      WHERE workspace_id = :'ws') AS drafts,
       (SELECT count(*) FROM saved_views WHERE workspace_id = :'ws') AS saved_views,
       (SELECT count(*) FROM snippets    WHERE workspace_id = :'ws') AS snippets;

DELETE FROM workspaces WHERE id = :'ws';

-- Nothing may be left behind in any table that carries workspace_id — this
-- catches a future table that forgot its ON DELETE CASCADE foreign key.
SELECT set_config('muninn.ws', :'ws', true) \g /dev/null
DO $$
DECLARE t text; n bigint;
BEGIN
    FOR t IN SELECT table_name FROM information_schema.columns
             WHERE table_schema = 'public' AND column_name = 'workspace_id' LOOP
        EXECUTE format('SELECT count(*) FROM %I WHERE workspace_id = %L', t, current_setting('muninn.ws'))
            INTO n;
        IF n > 0 THEN
            RAISE EXCEPTION '% rows left in %', n, t;
        END IF;
    END LOOP;
END $$;

COMMIT;
\echo 'Deleted. Now do the manual follow-ups listed at the top of this file.'

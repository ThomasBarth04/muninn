-- Spec 005: tickets filled in from a mailbox export. Marked, so a mistaken
-- import can be deleted by hand with one statement, and so the evals can
-- leave them out (docs/queries).
ALTER TABLE tickets ADD COLUMN imported_at timestamptz;

-- A closed ticket's entry in the brain (ADR 0010): its subject and every
-- message, stemmed in the workspace's language. Closing a ticket and
-- importing one both write it. Runs as the caller, so RLS applies inside.
-- ponytail: text capped at 200k chars — tsvector's limit is 1 MB; a longer
-- thread is matched on its first 200k.
CREATE FUNCTION brain_tsvector(p_workspace uuid, p_ticket uuid) RETURNS tsvector
LANGUAGE sql STABLE AS
$$ SELECT to_tsvector(w.language, left(t.subject || ' ' || coalesce((
       SELECT string_agg(m.text, ' ' ORDER BY m.created_at)
       FROM messages m WHERE m.workspace_id = p_workspace AND m.ticket_id = t.id), ''), 200000))
   FROM tickets t JOIN workspaces w ON w.id = t.workspace_id
   WHERE t.workspace_id = p_workspace AND t.id = p_ticket $$;

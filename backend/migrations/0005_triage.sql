-- Spec 006: ticket numbers, the waiting clock, snooze, read marks and
-- presence, drafts, saved views and snippets.

-- The last ticket number handed out in this workspace (§1).
ALTER TABLE workspaces ADD COLUMN ticket_count integer NOT NULL DEFAULT 0;

ALTER TABLE tickets
    ADD COLUMN number        integer,
    -- When New or Waiting on us was set; NULL in any other status. One input
    -- of the waiting clock, waiting_since() below (§16).
    ADD COLUMN waiting_set_at timestamptz,
    -- Snoozed while set; the wake job clears it (§25).
    ADD COLUMN snoozed_until timestamptz,
    -- The last time a snooze ended by itself: "Snooze ended" until opened.
    ADD COLUMN woke_at       timestamptz;

-- Existing tickets are numbered by creation time.
UPDATE tickets t SET number = n.number
FROM (SELECT id, row_number() OVER (PARTITION BY workspace_id ORDER BY created_at, id) AS number
      FROM tickets) n
WHERE n.id = t.id;
UPDATE workspaces w SET ticket_count = coalesce((SELECT max(number) FROM tickets t WHERE t.workspace_id = w.id), 0);
ALTER TABLE tickets ALTER COLUMN number SET NOT NULL;
CREATE UNIQUE INDEX tickets_number ON tickets (workspace_id, number);

-- When an open ticket's status was set is not known: its last activity is the
-- latest it can have been, and an unanswered customer message before that wins.
UPDATE tickets SET waiting_set_at = last_activity_at WHERE status IN ('new', 'waitingOnUs');

-- §16: while New or Waiting on us, a ticket waits since the first customer
-- message after the team's last reply (an agent reply, not a comment), or since
-- the status was set if that is earlier — set by hand after the team had the
-- last word. Worked out from the thread, so taking a reply back (undo send,
-- Discard) moves it back, and undoing a close keeps it. Runs as the caller.
-- ponytail: two subqueries per ticket, on messages_ticket; sorting a big view
-- by Waiting longest scans them all. Store it, kept by a messages trigger, if
-- that shows.
CREATE FUNCTION waiting_since(t tickets) RETURNS timestamptz LANGUAGE sql STABLE AS $$
    SELECT CASE WHEN t.status IN ('new', 'waitingOnUs') THEN least(t.waiting_set_at, (
        SELECT min(m.created_at) FROM messages m
        WHERE m.workspace_id = t.workspace_id AND m.ticket_id = t.id AND m.kind = 'customer'
          AND m.created_at > coalesce((SELECT max(a.created_at) FROM messages a
                                       WHERE a.workspace_id = t.workspace_id AND a.ticket_id = t.id
                                         AND a.kind = 'agent'), '-infinity')))
    END
$$;

-- Every way a ticket is created or changes status goes through here, so none
-- of them can forget the number or when the status was set. Runs as the
-- caller: the counter row is the tenant's own workspace, and locking it
-- serializes new tickets per workspace.
CREATE FUNCTION tickets_number_and_waiting() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        UPDATE workspaces SET ticket_count = ticket_count + 1 WHERE id = NEW.workspace_id
        RETURNING ticket_count INTO NEW.number;
    END IF;
    IF NEW.status NOT IN ('new', 'waitingOnUs') THEN
        NEW.waiting_set_at := NULL;
    ELSIF TG_OP = 'INSERT' THEN
        NEW.waiting_set_at := coalesce(NEW.waiting_set_at, NEW.created_at);
    ELSIF OLD.status NOT IN ('new', 'waitingOnUs') THEN
        NEW.waiting_set_at := now();
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER tickets_number_and_waiting BEFORE INSERT OR UPDATE OF status ON tickets
    FOR EACH ROW EXECUTE FUNCTION tickets_number_and_waiting();

-- The wake job (§25), due at snoozed_until.
ALTER TABLE jobs
    DROP CONSTRAINT jobs_kind_check,
    ADD CONSTRAINT jobs_kind_check CHECK (kind IN ('suggest', 'categorize', 'send', 'seats', 'wake'));

-- Per agent and ticket: when they last read it (§17) and last had it open (§29).
CREATE TABLE ticket_reads (
    workspace_id uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    ticket_id    uuid NOT NULL REFERENCES tickets ON DELETE CASCADE,
    agent_id     uuid NOT NULL REFERENCES agents ON DELETE CASCADE,
    seen_at      timestamptz NOT NULL,
    viewed_at    timestamptz,
    PRIMARY KEY (workspace_id, ticket_id, agent_id)
);

-- One unsent reply or comment per agent and ticket (§32).
CREATE TABLE drafts (
    workspace_id uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    ticket_id    uuid NOT NULL REFERENCES tickets ON DELETE CASCADE,
    agent_id     uuid NOT NULL REFERENCES agents ON DELETE CASCADE,
    mode         text NOT NULL CHECK (mode IN ('reply', 'comment')),
    text         text NOT NULL,
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, ticket_id, agent_id)
);
CREATE INDEX drafts_agent ON drafts (workspace_id, agent_id);

-- §4: filters as GET /api/tickets takes them, as JSON.
CREATE TABLE saved_views (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    agent_id     uuid NOT NULL REFERENCES agents ON DELETE CASCADE,
    name         text NOT NULL,
    shared       boolean NOT NULL DEFAULT false,
    filters      jsonb NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX saved_views_agent ON saved_views (workspace_id, agent_id);

-- §37–38: human-written, workspace-wide.
CREATE TABLE snippets (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    name         text NOT NULL,
    text         text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX snippets_workspace ON snippets (workspace_id);

ALTER TABLE ticket_reads ENABLE ROW LEVEL SECURITY;
ALTER TABLE ticket_reads FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON ticket_reads USING (workspace_id = app_workspace_id());
ALTER TABLE drafts ENABLE ROW LEVEL SECURITY;
ALTER TABLE drafts FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON drafts USING (workspace_id = app_workspace_id());
ALTER TABLE saved_views ENABLE ROW LEVEL SECURITY;
ALTER TABLE saved_views FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON saved_views USING (workspace_id = app_workspace_id());
ALTER TABLE snippets ENABLE ROW LEVEL SECURITY;
ALTER TABLE snippets FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON snippets USING (workspace_id = app_workspace_id());

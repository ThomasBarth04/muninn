-- Spec 007: a read-only mirror of one HubSpot account per workspace.

-- One per workspace (§4). The refresh token is useless without the app's
-- client secret, which is in the environment, not here (§5).
CREATE TABLE hubspot_connections (
    workspace_id     uuid PRIMARY KEY REFERENCES workspaces ON DELETE CASCADE,
    -- New for every connection: jobs of an earlier one see it changed and stop.
    id               uuid NOT NULL DEFAULT gen_random_uuid(),
    portal_id        bigint NOT NULL,
    account_name     text NOT NULL,
    refresh_token    text NOT NULL,
    status           text NOT NULL DEFAULT 'pickPipelines'
                     CHECK (status IN ('pickPipelines', 'importing', 'synced', 'revoked')),
    pipelines        jsonb NOT NULL DEFAULT '[]',   -- [{id, label, selected}], as the contract shows them
    closed_stages    text[] NOT NULL DEFAULT '{}',  -- stages whose ticketState is CLOSED (§12)
    -- The import (§7, §22): its run, which pipelines, since when, and how far it got.
    import_run       uuid,
    import_pipelines text[] NOT NULL DEFAULT '{}',
    import_since     timestamptz,
    import_before    timestamptz,
    import_after     text,
    import_done      integer NOT NULL DEFAULT 0,
    import_total     integer NOT NULL DEFAULT 0,
    skipped          text[] NOT NULL DEFAULT '{}',  -- HubSpot ticket ids skipped under §16
    checked_at       timestamptz,                   -- the hourly check's watermark (§19)
    last_synced_at   timestamptz,
    last_error       text,
    connected_at     timestamptz NOT NULL DEFAULT now()
);
-- One workspace per HubSpot account (§4): unique across tenants, like a
-- sending domain, so a second workspace's insert fails.
CREATE UNIQUE INDEX hubspot_connections_portal ON hubspot_connections (portal_id);
ALTER TABLE hubspot_connections ENABLE ROW LEVEL SECURITY;
ALTER TABLE hubspot_connections FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON hubspot_connections USING (workspace_id = app_workspace_id());

-- The OAuth round trip's `state` (§3): this agent, 10 minutes, once.
CREATE TABLE hubspot_states (
    workspace_id uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    token_hash   bytea NOT NULL,
    agent_id     uuid NOT NULL REFERENCES agents ON DELETE CASCADE,
    expires_at   timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, token_hash)
);
ALTER TABLE hubspot_states ENABLE ROW LEVEL SECURITY;
ALTER TABLE hubspot_states FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON hubspot_states USING (workspace_id = app_workspace_id());

-- A ticket that came from HubSpot (§12). The owner's email is kept so an agent
-- who joins later owns their tickets at once; the URL outlives a disconnect.
ALTER TABLE tickets
    ADD COLUMN hubspot_id          text,
    ADD COLUMN hubspot_url         text,
    ADD COLUMN hubspot_pipeline    text,
    ADD COLUMN hubspot_thread      text,
    ADD COLUMN hubspot_owner_email text;
CREATE UNIQUE INDEX tickets_hubspot ON tickets (workspace_id, hubspot_id);
CREATE INDEX tickets_hubspot_thread ON tickets (workspace_id, hubspot_thread) WHERE hubspot_thread IS NOT NULL;

-- A message that came from HubSpot, by HubSpot's id. Apart from message_id on
-- purpose: that is mail's threading key, and a mail must never thread into a
-- read-only HubSpot ticket, nor collide with one of its messages.
ALTER TABLE messages ADD COLUMN hubspot_key text;
CREATE UNIQUE INDEX messages_hubspot ON messages (workspace_id, hubspot_key);

-- A job about a HubSpot object names it. One not yet started absorbs another
-- for the same object (§18); a started one does not, so a change that lands
-- while a re-read runs gets a re-read of its own.
ALTER TABLE jobs ADD COLUMN object_ref text;
CREATE UNIQUE INDEX jobs_waiting_object ON jobs (workspace_id, kind, object_ref)
    WHERE object_ref IS NOT NULL AND attempts = 0 AND failed_at IS NULL;
ALTER TABLE jobs
    DROP CONSTRAINT jobs_kind_check,
    ADD CONSTRAINT jobs_kind_check CHECK (kind IN ('suggest', 'categorize', 'send', 'seats', 'wake',
        'hubspotImport', 'hubspotCheck', 'hubspotTicket', 'hubspotThread'));

-- The webhook finds the workspace by HubSpot account (ADR 0003's exception).
CREATE FUNCTION workspace_id_by_hubspot_portal(p_portal bigint) RETURNS uuid
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS
$$ SELECT workspace_id FROM hubspot_connections WHERE portal_id = p_portal $$;

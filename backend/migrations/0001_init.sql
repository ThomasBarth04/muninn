-- Runs as the owner role (ADR 0003). The app connects as muninn_app, which owns
-- nothing and is not a superuser, so every policy below applies to it.
-- The owner role has BYPASSRLS; that is what lets the SECURITY DEFINER lookups
-- at the bottom cross tenants.

GRANT USAGE ON SCHEMA public TO muninn_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO muninn_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT USAGE, SELECT ON SEQUENCES TO muninn_app;

-- The tenant of the current transaction; NULL outside the tenant helper, so a
-- query that forgets it matches nothing.
CREATE FUNCTION app_workspace_id() RETURNS uuid LANGUAGE sql STABLE AS
$$ SELECT nullif(current_setting('app.workspace_id', true), '')::uuid $$;

CREATE TABLE workspaces (
    id                      uuid PRIMARY KEY,
    name                    text NOT NULL,
    slug                    text NOT NULL UNIQUE,
    language                regconfig NOT NULL,
    created_at              timestamptz NOT NULL DEFAULT now(),
    trial_ends_at           timestamptz NOT NULL,
    billing_status          text NOT NULL DEFAULT 'trialing'
                            CHECK (billing_status IN ('trialing', 'active', 'past_due', 'canceled')),
    stripe_customer_id      text,
    stripe_subscription_id  text,
    -- Sending from the workspace's own domain (spec 002 §23–26).
    from_address            text,
    sending_domain          text UNIQUE,
    sending_domain_verified boolean NOT NULL DEFAULT false,
    postmark_domain_id      bigint,
    dns_records             jsonb NOT NULL DEFAULT '[]'
);

CREATE TABLE agents (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    email        text NOT NULL,  -- lowercased at the API boundary
    name         text NOT NULL,
    role         text NOT NULL CHECK (role IN ('owner', 'agent')),
    created_at   timestamptz NOT NULL DEFAULT now(),
    removed_at   timestamptz     -- soft delete: their replies keep an author
);
-- One email is one agent in one workspace (spec 001 §15).
CREATE UNIQUE INDEX agents_email ON agents (email) WHERE removed_at IS NULL;
CREATE INDEX agents_workspace ON agents (workspace_id);

CREATE TABLE contacts (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    email        text NOT NULL,  -- lowercased
    name         text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, email)
);

CREATE TABLE categories (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    name         text NOT NULL,
    description  text NOT NULL DEFAULT '',
    archived     boolean NOT NULL DEFAULT false,
    created_at   timestamptz NOT NULL DEFAULT now()
);
-- Jev's criteria are keyed by name (spec 004 §2).
CREATE UNIQUE INDEX categories_active_name ON categories (workspace_id, lower(name)) WHERE NOT archived;

CREATE TABLE tickets (
    id                       uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id             uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    token                    text NOT NULL UNIQUE,  -- <slug>+<token>@in.muninn.io
    subject                  text NOT NULL,
    status                   text NOT NULL DEFAULT 'new'
                             CHECK (status IN ('new', 'waitingOnContact', 'waitingOnUs', 'closed')),
    priority                 text CHECK (priority IN ('low', 'medium', 'high', 'urgent')),
    owner_id                 uuid REFERENCES agents,
    contact_id               uuid NOT NULL REFERENCES contacts,
    -- Spec 004: the category shown, who set it, and Jev's own pick kept apart
    -- so every override is category_id <> jev_category_id.
    category_id              uuid REFERENCES categories,
    category_source          text CHECK (category_source IN ('jev', 'agent')),
    jev_category_id          uuid REFERENCES categories,
    jev_category_probability double precision,
    category_suggestions     jsonb NOT NULL DEFAULT '[]',  -- [{id, name, probability}]
    suggest_status           text NOT NULL DEFAULT 'pending'
                             CHECK (suggest_status IN ('pending', 'ready', 'failed')),
    created_at               timestamptz NOT NULL DEFAULT now(),
    last_activity_at         timestamptz NOT NULL DEFAULT now(),
    closed_at                timestamptz,
    -- The brain (ADR 0010): set when the ticket closes, NULL otherwise.
    search                   tsvector
);
CREATE INDEX tickets_list ON tickets (workspace_id, last_activity_at DESC, id DESC);
CREATE INDEX tickets_contact ON tickets (contact_id, created_at DESC);
CREATE INDEX tickets_brain ON tickets USING gin (search) WHERE status = 'closed';

CREATE TABLE messages (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id    uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    ticket_id       uuid NOT NULL REFERENCES tickets ON DELETE CASCADE,
    kind            text NOT NULL CHECK (kind IN ('customer', 'agent', 'comment')),
    agent_id        uuid REFERENCES agents,   -- agent and comment
    from_name       text,                     -- customer
    from_email      text,                     -- customer
    text            text NOT NULL,
    html_body       text,                     -- stored, never rendered (spec 002 §7)
    message_id      text,                     -- RFC 5322 Message-ID without <>
    created_at      timestamptz NOT NULL DEFAULT now(),
    delivery_status text CHECK (delivery_status IN ('queued', 'sent', 'failed', 'held')),
    delivery_error  text,
    sent_at         timestamptz
);
CREATE UNIQUE INDEX messages_message_id ON messages (workspace_id, message_id);
CREATE INDEX messages_ticket ON messages (ticket_id, created_at);
-- The trial send cap counts these (ADR 0009).
CREATE INDEX messages_outbound ON messages (workspace_id, created_at) WHERE kind = 'agent';

-- Alone so moving attachments to object storage is one table (ADR 0004).
CREATE TABLE attachments (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    message_id   uuid NOT NULL REFERENCES messages ON DELETE CASCADE,
    name         text NOT NULL,
    content_type text NOT NULL,
    size         integer NOT NULL,
    content_id   text,
    content      bytea NOT NULL
);
CREATE INDEX attachments_message ON attachments (message_id);

-- Every candidate Jev scored, shown or not (spec 003 §5). rank is 1..3 when shown.
CREATE TABLE suggestions (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id   uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    ticket_id      uuid NOT NULL REFERENCES tickets ON DELETE CASCADE,
    case_ticket_id uuid NOT NULL REFERENCES tickets ON DELETE CASCADE,
    score          double precision NOT NULL,
    rank           integer,
    created_at     timestamptz NOT NULL DEFAULT now(),
    UNIQUE (ticket_id, case_ticket_id)
);

-- Verdicts and first opens, one row per agent per suggestion (spec 003 §12–13).
CREATE TABLE suggestion_feedback (
    suggestion_id uuid NOT NULL REFERENCES suggestions ON DELETE CASCADE,
    agent_id      uuid NOT NULL REFERENCES agents ON DELETE CASCADE,
    workspace_id  uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    verdict       text CHECK (verdict IN ('helped', 'notRelevant')),
    verdict_at    timestamptz,
    opened_at     timestamptz,
    PRIMARY KEY (suggestion_id, agent_id)
);

DO $$
DECLARE t text;
BEGIN
    ALTER TABLE workspaces ENABLE ROW LEVEL SECURITY;
    ALTER TABLE workspaces FORCE ROW LEVEL SECURITY;
    CREATE POLICY tenant ON workspaces USING (id = app_workspace_id());
    FOREACH t IN ARRAY ARRAY['agents', 'contacts', 'categories', 'tickets', 'messages',
                             'attachments', 'suggestions', 'suggestion_feedback'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant ON %I USING (workspace_id = app_workspace_id())', t);
    END LOOP;
END $$;

-- ---------------------------------------------------------------------------
-- Cross-tenant surface (ADR 0003). Everything below is reached without a
-- tenant; keep it short and keep customer content out of it.
-- ---------------------------------------------------------------------------

-- Magic links (ADR 0006). A signup link exists before its workspace does.
CREATE TABLE auth_links (
    token_hash     bytea PRIMARY KEY,
    purpose        text NOT NULL CHECK (purpose IN ('signup', 'login', 'invite')),
    email          text NOT NULL,
    workspace_id   uuid REFERENCES workspaces ON DELETE CASCADE,  -- login, invite
    workspace_name text NOT NULL,
    slug           text,                                        -- signup
    language       text,                                        -- signup
    created_at     timestamptz NOT NULL DEFAULT now(),
    expires_at     timestamptz NOT NULL,
    used_at        timestamptz
);
CREATE INDEX auth_links_email ON auth_links (email, created_at);
CREATE INDEX auth_links_invites ON auth_links (workspace_id) WHERE purpose = 'invite';

-- Looked up by cookie before the tenant is known.
CREATE TABLE sessions (
    token_hash   bytea PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    agent_id     uuid NOT NULL REFERENCES agents ON DELETE CASCADE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX sessions_agent ON sessions (agent_id);

-- ADR 0004: inserted in the same transaction as the row that caused it.
-- subject_id is the ticket (suggest, categorize), the message (send), or NULL (seats).
CREATE TABLE jobs (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces ON DELETE CASCADE,
    kind         text NOT NULL CHECK (kind IN ('suggest', 'categorize', 'send', 'seats')),
    subject_id   uuid,
    run_at       timestamptz NOT NULL DEFAULT now(),
    attempts     integer NOT NULL DEFAULT 0,
    last_error   text,
    failed_at    timestamptz
);
CREATE INDEX jobs_due ON jobs (run_at) WHERE failed_at IS NULL;

-- A Stripe event is applied at most once (spec 001 §26).
CREATE TABLE stripe_events (
    id          text PRIMARY KEY,
    received_at timestamptz NOT NULL DEFAULT now()
);

-- Inbound mail and signup find a workspace by slug.
CREATE FUNCTION workspace_id_by_slug(p_slug text) RETURNS uuid
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS
$$ SELECT id FROM workspaces WHERE slug = p_slug $$;

-- Login, signup and invites find an agent by email in any workspace.
CREATE FUNCTION agent_by_email(p_email text)
RETURNS TABLE (agent_id uuid, workspace_id uuid, workspace_name text)
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS
$$ SELECT a.id, w.id, w.name FROM agents a JOIN workspaces w ON w.id = a.workspace_id
   WHERE a.email = p_email AND a.removed_at IS NULL $$;

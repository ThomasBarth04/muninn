-- ADR 0011: a password and an authenticator app replace magic-link login.

ALTER TABLE agents
    -- Argon2id PHC string. NULL until the agent uses their setup link.
    ADD COLUMN password_hash        text,
    -- 20 bytes. NULL until set up, and again after a reset login.
    ADD COLUMN totp_secret          bytea,
    -- The 30-second step of the last code accepted: a code works once.
    ADD COLUMN totp_last_step       bigint,
    -- Ten failures within 15 minutes refuse logins (spec 001 §8).
    ADD COLUMN failed_logins        integer NOT NULL DEFAULT 0,
    ADD COLUMN last_failed_login_at timestamptz;

-- Links now only set a password (spec 001 §5, §9); login and signup links are
-- gone, and every link belongs to a workspace.
DELETE FROM auth_links WHERE purpose IN ('signup', 'login');
ALTER TABLE auth_links
    DROP CONSTRAINT auth_links_purpose_check,
    ADD CONSTRAINT auth_links_purpose_check CHECK (purpose IN ('invite', 'setup', 'reset')),
    ALTER COLUMN workspace_id SET NOT NULL,
    DROP COLUMN slug,
    DROP COLUMN language,
    -- The authenticator an invite or setup link offers: shown by the page,
    -- kept when the link is used. NULL on reset, which keeps the agent's own.
    ADD COLUMN totp_secret bytea;

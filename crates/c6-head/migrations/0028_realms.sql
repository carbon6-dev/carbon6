-- Realms (docs/os.md §3): every object belongs to exactly one. Root holds
-- carbon6's administration (and the assistant's conversations, which do
-- it); home holds everything else that exists today.
CREATE TABLE realms (
    id           TEXT PRIMARY KEY NOT NULL,
    name         TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    created_at   INTEGER NOT NULL,
    created_by   TEXT NOT NULL,          -- system | you | app:<name>
    app          TEXT                    -- the app it was made for, if any
);
INSERT INTO realms (id, name, description, created_at, created_by) VALUES
    ('root', 'Root', 'carbon6 itself: workers, connections, sign-in, and the defaults every realm inherits.', CAST(strftime('%s', 'now') AS INTEGER) * 1000, 'system'),
    ('home', 'Home', 'Your sessions, knowledge and data.', CAST(strftime('%s', 'now') AS INTEGER) * 1000, 'system');

ALTER TABLE sessions ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
UPDATE sessions SET realm = 'root' WHERE worker_id IN (SELECT id FROM workers WHERE role = 'head');
CREATE INDEX sessions_realm ON sessions (realm, updated_at);

ALTER TABLE memories ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE todos ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE automations ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE workflow_runs ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE secrets ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
ALTER TABLE usage ADD COLUMN realm TEXT NOT NULL DEFAULT 'home';
UPDATE usage SET realm = 'root' WHERE session_id IN (SELECT id FROM sessions WHERE realm = 'root');

-- An API token reaches one realm, or every realm (NULL: a root token, as
-- every token was before).
ALTER TABLE api_tokens ADD COLUMN realm TEXT;

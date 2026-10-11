-- Automations: start a template on a schedule (cron, the head's local
-- time) or when a webhook is called (its token stored hashed).
CREATE TABLE automations (
    id           TEXT PRIMARY KEY NOT NULL,
    name         TEXT NOT NULL,
    template     TEXT NOT NULL,
    trigger      TEXT NOT NULL,                -- "schedule" or "webhook"
    cron         TEXT NOT NULL DEFAULT '',
    input        TEXT NOT NULL DEFAULT '',     -- {{input}} for scheduled runs
    token_hash   TEXT NOT NULL DEFAULT '',
    enabled      INTEGER NOT NULL DEFAULT 1,
    created_at   INTEGER NOT NULL,
    last_run_at  INTEGER,
    last_run_id  TEXT,
    last_error   TEXT
);

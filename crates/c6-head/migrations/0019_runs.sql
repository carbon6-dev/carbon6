-- Automation runs and how they ended; a failure streak can switch one off.
ALTER TABLE automations ADD COLUMN disable_after INTEGER NOT NULL DEFAULT 0;

CREATE TABLE automation_runs (
    run_id         TEXT PRIMARY KEY NOT NULL,
    automation_id  TEXT NOT NULL,
    started_at     INTEGER NOT NULL,
    finished_at    INTEGER,
    trigger        TEXT NOT NULL,              -- "schedule", "webhook" or "manual"
    outcome        TEXT NOT NULL,              -- "running", "ok", "failed" or "stopped"
    error          TEXT
);
CREATE INDEX automation_runs_by_automation ON automation_runs (automation_id, started_at);

-- A run's sessions and how each one's first turn ended (NULL: not yet).
CREATE TABLE run_sessions (
    session_id  TEXT PRIMARY KEY NOT NULL,
    run_id      TEXT NOT NULL,
    outcome     TEXT
);
CREATE INDEX run_sessions_by_run ON run_sessions (run_id);

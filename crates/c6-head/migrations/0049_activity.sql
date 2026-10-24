-- Timing marks from session logs, for analytics (docs/analytics.md): one
-- row per event that says who was working when.
CREATE TABLE activity (
    session_id TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    ts         INTEGER NOT NULL,
    -- user, turn_start, turn_end, model, tool, approval, question, rewind
    kind       TEXT NOT NULL,
    -- the model, tool, turn end reason, or approval/question state
    name       TEXT NOT NULL DEFAULT '',
    -- a tool call's own duration
    ms         INTEGER NOT NULL DEFAULT 0,
    failed     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, seq)
) WITHOUT ROWID;
CREATE INDEX activity_ts ON activity (ts);

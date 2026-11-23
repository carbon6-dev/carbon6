-- One informational plan forecast per session, even across head restarts.
CREATE TABLE plan_awareness (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE
) WITHOUT ROWID;

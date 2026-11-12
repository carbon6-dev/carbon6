-- Your follow-ups for a session, held until you send them (docs/capture.md §4).
CREATE TABLE followups (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    text TEXT NOT NULL,
    quote TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);
CREATE INDEX followups_session ON followups (session_id, created_at);

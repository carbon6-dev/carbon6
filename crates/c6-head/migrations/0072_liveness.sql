-- Observation state is separate from the worker-authored event sequence.
CREATE TABLE liveness_incidents (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    kind TEXT NOT NULL,
    cause TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    last_seq INTEGER NOT NULL,
    worker_id TEXT,
    ownership_epoch INTEGER,
    resolved_at INTEGER,
    delivered INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX liveness_open ON liveness_incidents(session_id, resolved_at);

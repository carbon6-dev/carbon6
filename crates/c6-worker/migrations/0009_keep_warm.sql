-- Provider-ready request prefix. Never stores credentials or request headers.
CREATE TABLE keep_warm_snapshots (
    session_id TEXT PRIMARY KEY NOT NULL,
    call_ms INTEGER NOT NULL,
    connection_id TEXT NOT NULL,
    generation TEXT NOT NULL,
    ready INTEGER NOT NULL DEFAULT 0,
    -- An accepted stream reserves only metadata; large JSON is written once,
    -- after a complete response with enough measured context to warrant it.
    data TEXT
);

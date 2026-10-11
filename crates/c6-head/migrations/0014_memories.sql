-- Memory: durable notes for sessions, scoped by a label (empty: every
-- session). Written by you (Settings) or by agents (remember/forget).
CREATE TABLE memories (
    id          TEXT PRIMARY KEY NOT NULL,
    scope       TEXT NOT NULL DEFAULT '',
    text        TEXT NOT NULL,
    session_id  TEXT,                    -- the session that wrote it, if any
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX memories_scope ON memories (scope);

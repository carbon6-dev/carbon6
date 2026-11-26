-- Head-owned archive state is not a session event. Retain the last local
-- placement here independently of journal trimming, never infer archive from
-- inactivity or a missing actor.
CREATE TABLE build_folder_archives (
    session_id TEXT PRIMARY KEY NOT NULL,
    archived_at_ms INTEGER,
    cwd TEXT NOT NULL DEFAULT '',
    repository TEXT NOT NULL DEFAULT '',
    last_used_ms INTEGER
);

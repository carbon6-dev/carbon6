-- Placement is a durable projection, not an inference from recent events.
-- Keep the checkpoint across restarts so legacy backfill can make bounded
-- progress without loading whole conversations during disk investigation.
CREATE TABLE build_folder_places (
    session_id TEXT PRIMARY KEY NOT NULL,
    checkpoint_seq INTEGER NOT NULL DEFAULT 0,
    known INTEGER NOT NULL DEFAULT 0,
    backfill_blocked INTEGER NOT NULL DEFAULT 0,
    cwd TEXT NOT NULL DEFAULT '',
    repository TEXT NOT NULL DEFAULT '',
    worker_id TEXT NOT NULL DEFAULT '',
    local INTEGER NOT NULL DEFAULT 1
);

ALTER TABLE sessions ADD COLUMN ownership_epoch INTEGER NOT NULL DEFAULT 0 CHECK (ownership_epoch >= 0);
ALTER TABLE commands ADD COLUMN ownership_epoch INTEGER NOT NULL DEFAULT 0 CHECK (ownership_epoch >= 0);

CREATE TABLE session_moves (
    id               TEXT PRIMARY KEY NOT NULL,
    session_id       TEXT NOT NULL,
    source_worker_id TEXT NOT NULL,
    target_worker_id TEXT NOT NULL,
    source_epoch     INTEGER NOT NULL CHECK (source_epoch >= 0),
    target_epoch     INTEGER NOT NULL CHECK (target_epoch > source_epoch),
    state            TEXT NOT NULL,
    request_json     TEXT NOT NULL,
    result_json      TEXT NOT NULL DEFAULT '{}',
    error            TEXT,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    CHECK (source_worker_id <> target_worker_id)
);

CREATE UNIQUE INDEX session_moves_active ON session_moves (session_id)
WHERE state NOT IN ('done', 'failed');

CREATE TRIGGER commands_require_stable_session_owner
BEFORE INSERT ON commands
BEGIN
    SELECT RAISE(ABORT, 'session is moving') WHERE EXISTS (
        SELECT 1 FROM session_moves WHERE session_id = NEW.session_id AND state NOT IN ('done', 'failed')
    );
    SELECT RAISE(ABORT, 'session command has stale owner') WHERE EXISTS (
        SELECT 1 FROM sessions WHERE id = NEW.session_id
          AND (worker_id <> NEW.worker_id OR ownership_epoch <> NEW.ownership_epoch)
    );
END;

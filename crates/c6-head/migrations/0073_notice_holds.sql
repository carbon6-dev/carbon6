-- Notice queues outlive turns; keep their attention separate from tool approvals.
ALTER TABLE sessions ADD COLUMN notice_waiting_at INTEGER;
ALTER TABLE sessions ADD COLUMN notices_proceeding INTEGER NOT NULL DEFAULT 0;

CREATE TABLE notice_holds (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    command_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    needs_you INTEGER NOT NULL,
    consumed INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, command_id)
) WITHOUT ROWID;
CREATE INDEX notice_holds_by_reason ON notice_holds (session_id, needs_you, at DESC) WHERE consumed = 0;

CREATE INDEX sessions_notice_waiting ON sessions (realm, notice_waiting_at DESC)
    WHERE archived = 0 AND notice_waiting_at IS NOT NULL;

-- Only existing journals need a Rust backfill (protobuf plus effective history).
CREATE TABLE notice_holds_backfill (session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE) WITHOUT ROWID;
INSERT INTO notice_holds_backfill SELECT id FROM sessions WHERE last_seq > 0;

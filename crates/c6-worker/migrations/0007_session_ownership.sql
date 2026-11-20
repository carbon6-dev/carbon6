-- No row denotes a legacy, unblocked session at epoch zero. Never trim a
-- fence along with cached events: that could revive a superseded epoch.
CREATE TABLE session_ownership (
    session_id TEXT PRIMARY KEY NOT NULL,
    epoch INTEGER NOT NULL,
    blocked INTEGER NOT NULL DEFAULT 0,
    move_id TEXT NOT NULL DEFAULT ''
);

-- An abort can arrive before a delayed freeze or prepare RPC. Retain every
-- receipt independently of the session row and any later move.
CREATE TABLE aborted_session_moves (
    move_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    epoch INTEGER NOT NULL,
    PRIMARY KEY (move_id, session_id, epoch)
) WITHOUT ROWID;

-- Worker journal: a cache of session state, never authoritative. Events are
-- written here before they are sent to the head.

CREATE TABLE sessions (
    id          TEXT PRIMARY KEY NOT NULL,
    spec        BLOB NOT NULL,      -- CreateSession protobuf
    created_at  INTEGER NOT NULL,
    last_seq    INTEGER NOT NULL DEFAULT 0,
    acked_seq   INTEGER NOT NULL DEFAULT 0,
    open_turn   INTEGER             -- set between TurnStarted and TurnEnded
);

CREATE TABLE events (
    session_id  TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    data        BLOB NOT NULL,      -- SessionEvent protobuf
    PRIMARY KEY (session_id, seq)
) WITHOUT ROWID;

CREATE TABLE applied_commands (
    command_id  TEXT PRIMARY KEY NOT NULL,
    session_id  TEXT NOT NULL
) WITHOUT ROWID;

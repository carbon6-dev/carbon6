CREATE TABLE task_shifts (
    session_id   TEXT PRIMARY KEY NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    command_seq  INTEGER NOT NULL,
    command_id   TEXT NOT NULL,
    offered      INTEGER NOT NULL,
    feedback     INTEGER NOT NULL DEFAULT 0,
    base_seq     INTEGER NOT NULL
);

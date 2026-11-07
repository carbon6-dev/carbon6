-- Claude's chain UUIDs belong to the worker's CLI, not the public transcript.
-- Keep them across process/worker restarts without exposing them to the UI.
CREATE TABLE claude_turns (
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (session_id, seq)
) WITHOUT ROWID;

-- Written before stopping/resetting Claude. Until Rewound is committed, a
-- restarted actor must retry this reset before accepting any more input.
CREATE TABLE claude_rewinds (
    session_id TEXT PRIMARY KEY NOT NULL,
    command_id TEXT NOT NULL,
    data TEXT NOT NULL,
    applied_seq INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

-- Record ownership before any model/initialize input. A missing process
-- handle or leader after restart is not proof that its descendants stopped.
CREATE TABLE claude_processes (
    session_id TEXT NOT NULL,
    pgid INTEGER NOT NULL CHECK (pgid > 0),
    PRIMARY KEY (session_id, pgid)
) WITHOUT ROWID;

-- Reserve before spawning any carbon6 shell. NULL PGID means a launch not
-- yet identified, or an SSH writer whose identity cannot be proved locally.
-- Keep source IDs even after ESRCH, for late-notice suppression and ID reuse.
CREATE TABLE shell_writers (
    writer_id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL,
    root_session_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    pgid INTEGER CHECK (pgid > 0),
    absent INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE INDEX shell_writers_session ON shell_writers(session_id);
CREATE INDEX shell_writers_root ON shell_writers(root_session_id);

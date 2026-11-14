-- A session's goal: checked after each turn, it keeps the session going
-- until reached, needing you, or out of turns or time (docs/goals.md).
CREATE TABLE goals (
    session_id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    state TEXT NOT NULL,
    turns INTEGER NOT NULL DEFAULT 0,
    max_turns INTEGER NOT NULL,
    deadline INTEGER NOT NULL,
    why TEXT NOT NULL DEFAULT '',
    left_to_do TEXT NOT NULL DEFAULT '',
    stalls INTEGER NOT NULL DEFAULT 0,
    checked_seq INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

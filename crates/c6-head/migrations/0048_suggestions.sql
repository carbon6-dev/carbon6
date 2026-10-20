-- Suggested next messages (docs/suggestions.md): at most one per session,
-- for the position its log had when it was made.
CREATE TABLE suggestions (
    session_id TEXT PRIMARY KEY NOT NULL,
    for_seq    INTEGER NOT NULL,
    text       TEXT NOT NULL,           -- empty: no clear next step
    made_at    INTEGER NOT NULL,
    shown      INTEGER NOT NULL DEFAULT 0,
    accepted   INTEGER NOT NULL DEFAULT 0
);

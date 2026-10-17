-- Sessions imported from Claude Code and Codex (docs/imports.md): where
-- each came from, and how many of its events came from there (more means
-- it was continued in carbon6, and imports leave it alone).
CREATE TABLE session_imports (
    session_id   TEXT PRIMARY KEY NOT NULL,
    source       TEXT NOT NULL,             -- claude_code | codex
    file         TEXT NOT NULL DEFAULT '',
    imported_seq INTEGER NOT NULL DEFAULT 0,
    imported_at  INTEGER NOT NULL
);

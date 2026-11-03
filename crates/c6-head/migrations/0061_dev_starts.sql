-- How dev work starts (docs/dev-starts.md): your labels for workers, which
-- a template's pool picks by, and what a session's worktree does when its
-- work ends (with the template it was started from).
ALTER TABLE workers ADD COLUMN labels TEXT NOT NULL DEFAULT '[]';
CREATE TABLE worktree_starts (
    session_id TEXT PRIMARY KEY,
    cleanup TEXT NOT NULL DEFAULT '',
    template TEXT NOT NULL DEFAULT ''
);

-- Claude Code's own subagents, mirrored from its stream: shown, never run
-- (docs/subagents.md §8).
ALTER TABLE sessions ADD COLUMN observed INTEGER NOT NULL DEFAULT 0;

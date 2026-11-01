-- A queued input the worker has handed to the agent (in the session's
-- inbox, or written to its Claude Code process): it reads it at its next
-- step (docs/background.md §4).
ALTER TABLE commands ADD COLUMN with_agent INTEGER NOT NULL DEFAULT 0;

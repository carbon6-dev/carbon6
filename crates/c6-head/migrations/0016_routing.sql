-- The model for a session's subagents, from a routing rule.
ALTER TABLE sessions ADD COLUMN subagent_model TEXT NOT NULL DEFAULT '';

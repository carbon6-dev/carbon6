-- The default for a session's subagents: with subagent_model, a connection
-- (empty: the session's) and an effort, from a routing rule or picked in
-- the session (docs/subagents.md §14).
ALTER TABLE sessions ADD COLUMN subagent_connection_id TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN subagent_effort TEXT NOT NULL DEFAULT '';

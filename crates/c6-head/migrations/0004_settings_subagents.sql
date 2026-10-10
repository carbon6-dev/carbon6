-- Head-wide settings as JSON documents (e.g. "mcp_servers", "skills").
CREATE TABLE settings (
    key    TEXT PRIMARY KEY NOT NULL,
    value  TEXT NOT NULL
);

-- Subagent sessions point at the parent session and the Task call.
ALTER TABLE sessions ADD COLUMN parent_id TEXT;
ALTER TABLE sessions ADD COLUMN parent_call_id TEXT;
CREATE INDEX sessions_by_parent ON sessions (parent_id);

-- The tools from the routing rule, fixed for the session (side chats
-- inherit them). A JSON array; empty: all tools (docs/subagents.md).
ALTER TABLE sessions ADD COLUMN tools TEXT NOT NULL DEFAULT '[]';

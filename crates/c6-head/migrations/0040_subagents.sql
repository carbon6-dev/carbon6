-- Subagents as sessions (docs/subagents.md): the named agent, how the last
-- turn ended, and the top-level session of the tree (NULL for one).
ALTER TABLE sessions ADD COLUMN agent TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN end_reason TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN root_id TEXT;
UPDATE sessions SET root_id = (
    WITH RECURSIVE up(id, parent_id) AS (
        SELECT s.id, s.parent_id FROM sessions s WHERE s.id = sessions.parent_id
        UNION ALL
        SELECT p.id, p.parent_id FROM sessions p JOIN up ON p.id = up.parent_id
    )
    SELECT id FROM up WHERE parent_id IS NULL
) WHERE parent_id IS NOT NULL;
CREATE INDEX sessions_root ON sessions (root_id);

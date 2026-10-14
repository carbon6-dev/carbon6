-- carbon6's configuration lists become system collections (docs/os.md
-- §12.2): each entry of each list a record, in order. Restrictions
-- (policies, budgets) and model routing and prices live in root; the rest
-- in home. The original values stay in legacy_settings for one release.
ALTER TABLE records ADD COLUMN position INTEGER NOT NULL DEFAULT 0;

CREATE TABLE legacy_settings AS
    SELECT key, value FROM settings
    WHERE key IN ('skills', 'agents', 'commands', 'hooks', 'mcp_servers', 'templates', 'workflows', 'policies', 'routes',
                  'prices', 'budgets', 'channels', 'hosts', 'memex_folders', 'memex_scopes');

INSERT INTO records (id, realm, collection, fields, version, created_by, updated_by, created_at, updated_at, position)
SELECT lower(hex(randomblob(16))),
       CASE s.key WHEN 'policies' THEN 'root' WHEN 'budgets' THEN 'root' WHEN 'routes' THEN 'root' WHEN 'prices' THEN 'root' ELSE 'home' END,
       s.key, j.value, 1, 'upgrade', 'upgrade',
       CAST(strftime('%s', 'now') AS INTEGER) * 1000, CAST(strftime('%s', 'now') AS INTEGER) * 1000,
       CAST(j.key AS INTEGER)
FROM legacy_settings s, json_each(s.value) j
WHERE json_valid(s.value) AND json_type(s.value) = 'array' AND j.type = 'object';

INSERT INTO record_history (record_id, version, op, fields, changed_by, changed_at)
SELECT id, 1, 'create', fields, 'upgrade', created_at FROM records WHERE created_by = 'upgrade';

DELETE FROM settings
WHERE key IN ('skills', 'agents', 'commands', 'hooks', 'mcp_servers', 'templates', 'workflows', 'policies', 'routes',
              'prices', 'budgets', 'channels', 'hosts', 'memex_folders', 'memex_scopes');

-- Memory notes become the "memory" collection: each note a record with
-- the same id. The table stays as legacy_memories for one release.
INSERT INTO records (id, realm, collection, fields, version, created_by, updated_by, created_at, updated_at, position)
SELECT id, realm, 'memory', json_object('scope', scope, 'text', text, 'session_id', session_id), 1,
       CASE WHEN session_id IS NULL THEN 'you' ELSE 'session:' || session_id END,
       CASE WHEN session_id IS NULL THEN 'you' ELSE 'session:' || session_id END,
       created_at, updated_at, 0
FROM memories;
INSERT INTO record_history (record_id, version, op, fields, changed_by, changed_at)
SELECT id, 1, 'create', fields, created_by, created_at FROM records WHERE collection = 'memory';
ALTER TABLE memories RENAME TO legacy_memories;

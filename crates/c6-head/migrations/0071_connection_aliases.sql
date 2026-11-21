CREATE TABLE connection_aliases (
    alias_id TEXT PRIMARY KEY REFERENCES connections(id) ON DELETE CASCADE,
    target_id TEXT NOT NULL REFERENCES connections(id),
    target_kind TEXT NOT NULL CHECK (target_kind IN ('openai', 'openai_compat', 'anthropic', 'chatgpt')),
    switched_at INTEGER NOT NULL,
    CHECK (alias_id <> target_id)
);

CREATE INDEX connection_aliases_by_target ON connection_aliases(target_id);

-- Historical target IDs are snapshots: only the active target blocks deletion.
CREATE TABLE connection_alias_history (
    seq INTEGER PRIMARY KEY,
    alias_id TEXT NOT NULL REFERENCES connection_aliases(alias_id) ON DELETE CASCADE,
    previous_target_id TEXT,
    target_id TEXT NOT NULL,
    switched_at INTEGER NOT NULL,
    changed_by TEXT NOT NULL
);

CREATE INDEX connection_alias_history_by_alias ON connection_alias_history(alias_id, seq);

-- A worker's usage may arrive after a switch or deletion. Keep the
-- authorized account and names as snapshots, without pinning connections.
CREATE TABLE connection_alias_grants (
    root_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    worker_id TEXT NOT NULL,
    alias_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    alias_name TEXT NOT NULL,
    target_name TEXT NOT NULL,
    PRIMARY KEY (root_session_id, worker_id, alias_id, target_id)
) WITHOUT ROWID;

-- Keep the selected ID and its defaults, but expose the active provider to
-- runtime callers. Two constrained, disjoint realm lists allow no realm;
-- [""] is that empty intersection (where [] means unrestricted).
CREATE VIEW effective_connections AS
SELECT c.id, c.name, COALESCE(t.kind, c.kind) AS kind,
       COALESCE(t.base_url, c.base_url) AS base_url,
       CASE WHEN t.id IS NULL THEN c.api_key ELSE t.api_key END AS api_key,
       COALESCE(c.default_model, t.default_model) AS default_model,
       COALESCE(c.context_window, t.context_window) AS context_window,
       c.created_at, c.members,
       COALESCE(c.subagent_model, t.subagent_model) AS subagent_model,
       COALESCE(c.compact_at, t.compact_at) AS compact_at,
       CASE WHEN t.id IS NULL OR json_array_length(t.realms) = 0 THEN c.realms
            WHEN json_array_length(c.realms) = 0 THEN t.realms
            ELSE COALESCE(NULLIF((SELECT json_group_array(value) FROM json_each(c.realms)
                 WHERE value IN (SELECT value FROM json_each(t.realms))), '[]'), '[""]') END AS realms,
       t.id AS resolved_connection_id, t.name AS resolved_connection_name
  FROM connections c
  LEFT JOIN connection_aliases a ON a.alias_id = c.id
  LEFT JOIN connections t ON t.id = a.target_id;

-- Full-text search over session events. Derived data: rebuilt from
-- `events` when missing or when its format changes (settings key
-- search.version). Trigram tokens give substring matches, which suit code,
-- paths and identifiers.
CREATE VIRTUAL TABLE search USING fts5(
    text,
    session_id UNINDEXED,
    seq UNINDEXED,
    part UNINDEXED,   -- block index within an assistant message, else 0
    kind UNINDEXED,   -- user, assistant, thinking, tool_call, tool_result, error, summary
    tokenize = 'trigram'
);

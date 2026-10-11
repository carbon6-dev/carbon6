-- API tokens for scripts and the `c6` CLI (Authorization: Bearer).
-- Stored as SHA-256 hashes; shown once when created.
CREATE TABLE api_tokens (
    id            TEXT PRIMARY KEY NOT NULL,
    name          TEXT NOT NULL,
    token_hash    TEXT NOT NULL UNIQUE,
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER
);

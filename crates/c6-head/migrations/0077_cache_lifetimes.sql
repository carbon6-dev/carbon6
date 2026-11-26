-- Materialized evidence, not a configured TTL. Keys are actual usage targets.
CREATE TABLE cache_lifetime_estimates (
    realm TEXT NOT NULL,
    connection_id TEXT NOT NULL,
    model TEXT NOT NULL,
    evidence TEXT NOT NULL,
    PRIMARY KEY (realm, connection_id, model)
) WITHOUT ROWID;
CREATE TABLE cache_lifetime_previous (
    session_id TEXT PRIMARY KEY NOT NULL,
    call TEXT NOT NULL
) WITHOUT ROWID;
-- Bounded replay checkpoints, including partially reconstructed call timing.
CREATE TABLE cache_lifetime_replay (
    session_id TEXT PRIMARY KEY NOT NULL,
    after_seq INTEGER NOT NULL,
    timing TEXT NOT NULL,
    live INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE INDEX cache_lifetime_replay_pending ON cache_lifetime_replay (session_id) WHERE live = 0;

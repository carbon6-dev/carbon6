-- Tokens per model call (from Usage events), for costs and budgets.
CREATE TABLE usage (
    session_id     TEXT NOT NULL,
    seq            INTEGER NOT NULL,
    ts             INTEGER NOT NULL,
    connection_id  TEXT NOT NULL,
    model          TEXT NOT NULL,
    input          INTEGER NOT NULL,
    output         INTEGER NOT NULL,
    cache_read     INTEGER NOT NULL,
    cache_write    INTEGER NOT NULL,
    PRIMARY KEY (session_id, seq)
) WITHOUT ROWID;
CREATE INDEX usage_ts ON usage (ts);

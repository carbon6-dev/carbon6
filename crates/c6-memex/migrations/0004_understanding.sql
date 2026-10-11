-- What a model has made of items (tier 2): a summary per item, and which
-- items (or email threads) have been read by which version.
ALTER TABLE items ADD COLUMN summary TEXT NOT NULL DEFAULT '';
CREATE TABLE enrich (
    target     TEXT PRIMARY KEY NOT NULL,   -- item:<id> or thread:<account>:<thread>
    version    TEXT NOT NULL,               -- understand-v1@<model>
    done_at    INTEGER NOT NULL,
    error      TEXT
);
-- Model calls per day, for the budget.
CREATE TABLE model_calls (
    day    TEXT PRIMARY KEY NOT NULL,
    calls  INTEGER NOT NULL
);
-- Who owes what, by when (from mail and sessions).
CREATE TABLE commitments (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id     TEXT NOT NULL,
    who         TEXT NOT NULL,      -- "me", or a person's address or name
    what        TEXT NOT NULL,
    due         INTEGER,
    status      TEXT NOT NULL DEFAULT 'open',
    made_by     TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE INDEX commitments_status ON commitments (status, due);
-- Suggestions for you: things that may be the same (merge), links a model
-- is unsure about.
CREATE TABLE review (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,      -- merge
    a           TEXT NOT NULL,
    b           TEXT NOT NULL,
    reason      TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'open',
    created_at  INTEGER NOT NULL,
    UNIQUE (kind, a, b)
);
-- Same-name lookups (merge suggestions).
CREATE INDEX things_kind_name ON things (kind, name COLLATE NOCASE);

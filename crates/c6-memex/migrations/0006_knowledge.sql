-- Environment knowledge (docs/follow-through.md §6): facts agents learn
-- about the systems around the code, with how each is known (status), how
-- long it holds (volatility), where it applies (scope), how to re-check it,
-- and the evidence. Each fact is also an item (kind fact), so search,
-- briefs and the graph see it.
CREATE TABLE facts (
    id             TEXT PRIMARY KEY NOT NULL,
    item_id        TEXT NOT NULL,
    claim          TEXT NOT NULL,
    key            TEXT NOT NULL,              -- the claim, normalized (duplicates)
    about          TEXT NOT NULL,              -- thing keys (JSON array)
    status         TEXT NOT NULL,              -- reviewed verified observed inferred disputed retired
    volatility     TEXT NOT NULL,              -- stable slow
    labels         TEXT NOT NULL DEFAULT '[]', -- applies only with these labels
    conditions     TEXT NOT NULL DEFAULT '',   -- and these conditions (text)
    negative       INTEGER NOT NULL DEFAULT 0, -- "X does not apply to Y"
    check_cmd      TEXT NOT NULL DEFAULT '',   -- re-checks it (exit 0: still true)
    check_on       TEXT NOT NULL DEFAULT '',
    check_worker   TEXT NOT NULL DEFAULT '',
    evidence       TEXT NOT NULL DEFAULT '[]', -- [{at, by, where, command, excerpt}]
    supports       TEXT NOT NULL DEFAULT '[]', -- for inferred: the facts it rests on
    replaced_by    TEXT,
    valid_from     INTEGER NOT NULL,
    valid_to       INTEGER,
    last_verified  INTEGER,
    made_by        TEXT NOT NULL,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
CREATE INDEX facts_key ON facts (key);
CREATE INDEX facts_status ON facts (status, valid_to);
-- A system's page: a current summary written from its facts.
ALTER TABLE things ADD COLUMN page TEXT NOT NULL DEFAULT '';
ALTER TABLE things ADD COLUMN page_at INTEGER;

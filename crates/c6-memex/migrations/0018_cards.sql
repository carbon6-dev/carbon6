-- Who each person and organization is to you (docs/relationships.md §2.3):
-- a card the model writes from the history and their threads' summaries,
-- grown as new threads are read.
CREATE TABLE cards (
    key           TEXT PRIMARY KEY NOT NULL,   -- person:<address> or org:<domain>
    name          TEXT NOT NULL,
    card          TEXT NOT NULL,               -- JSON: relationship, role, story, open, how
    covered_from  INTEGER,                     -- the oldest and newest thread it was written from
    covered_to    INTEGER,
    threads       INTEGER NOT NULL,            -- how many threads it has read
    model         TEXT NOT NULL,
    updated_at    INTEGER NOT NULL,
    dirty         INTEGER NOT NULL DEFAULT 0   -- a thread with them was read since
);
CREATE INDEX cards_dirty ON cards (dirty, updated_at);

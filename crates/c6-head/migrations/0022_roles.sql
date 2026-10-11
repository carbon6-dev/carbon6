-- What a node is: a worker (runs sessions) or the memex (the knowledge
-- service). Set when it first attaches, by what it says it is, then fixed.
ALTER TABLE workers ADD COLUMN role TEXT NOT NULL DEFAULT '';
-- What the head has sent the memex, waiting to be delivered (in order).
CREATE TABLE memex_outbox (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    method      TEXT NOT NULL,
    params      TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    last_error  TEXT
);

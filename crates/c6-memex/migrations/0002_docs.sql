-- Files seen in watched folders: what was ingested from each.
CREATE TABLE files (
    path      TEXT PRIMARY KEY NOT NULL,
    mtime     INTEGER NOT NULL,
    size      INTEGER NOT NULL,
    item_id   TEXT,
    error     TEXT,
    seen_at   INTEGER NOT NULL
);

-- Associative trails: named paths through items and things (Bush's memex).
CREATE TABLE trails (
    id           TEXT PRIMARY KEY NOT NULL,
    name         TEXT NOT NULL UNIQUE,
    description  TEXT NOT NULL DEFAULT '',
    made_by      TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
CREATE TABLE trail_steps (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    trail_id    TEXT NOT NULL,
    ordinal     INTEGER NOT NULL,
    target      TEXT NOT NULL,      -- an item id or a thing (id or label)
    comment     TEXT NOT NULL DEFAULT '',
    made_by     TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE INDEX trail_steps_trail ON trail_steps (trail_id, ordinal);

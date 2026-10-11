-- What went in, as it came (docs/memex.md 4.1).
CREATE TABLE items (
    id           TEXT PRIMARY KEY NOT NULL,
    kind         TEXT NOT NULL,
    source       TEXT NOT NULL,
    external_id  TEXT NOT NULL,
    title        TEXT NOT NULL,
    body         TEXT NOT NULL,
    url          TEXT,
    created_at   INTEGER NOT NULL,   -- when it happened
    added_at     INTEGER NOT NULL,   -- when it came in
    updated_at   INTEGER NOT NULL,
    meta         TEXT NOT NULL DEFAULT '{}',
    UNIQUE (source, external_id)
);
CREATE INDEX items_kind ON items (kind, created_at);

-- Passages of an item's text, for search (4.2).
CREATE TABLE parts (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id  TEXT NOT NULL,
    ordinal  INTEGER NOT NULL,
    text     TEXT NOT NULL
);
CREATE INDEX parts_item ON parts (item_id);
CREATE VIRTUAL TABLE parts_fts USING fts5(text, content='parts', content_rowid='id', tokenize='porter unicode61 remove_diacritics 2');
CREATE TRIGGER parts_ai AFTER INSERT ON parts BEGIN
    INSERT INTO parts_fts (rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER parts_ad AFTER DELETE ON parts BEGIN
    INSERT INTO parts_fts (parts_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;

-- A passage's vector in one embedding space (a model), unit length.
CREATE TABLE vectors (
    part_id  INTEGER NOT NULL,
    space    TEXT NOT NULL,
    data     BLOB NOT NULL,
    PRIMARY KEY (part_id, space)
);

-- Things (4.3): people, hosts, repos, clients, topics, … `key` is the
-- carbon6 label a thing stands for, if any.
CREATE TABLE things (
    id          TEXT PRIMARY KEY NOT NULL,
    kind        TEXT NOT NULL,
    name        TEXT NOT NULL,
    key         TEXT UNIQUE,
    aliases     TEXT NOT NULL DEFAULT '[]',
    created_at  INTEGER NOT NULL
);

-- Typed links with where they come from.
CREATE TABLE links (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    src_thing   TEXT,
    src_item    TEXT,
    rel         TEXT NOT NULL,
    dst_thing   TEXT,
    dst_item    TEXT,
    basis       TEXT NOT NULL,      -- observed | inferred
    confidence  REAL NOT NULL DEFAULT 1.0,
    made_by     TEXT NOT NULL,
    valid_from  INTEGER,
    valid_to    INTEGER,
    created_at  INTEGER NOT NULL
);
CREATE INDEX links_src_item ON links (src_item);
CREATE INDEX links_dst_thing ON links (dst_thing);
CREATE INDEX links_src_thing ON links (src_thing);

CREATE TABLE evidence (
    link_id  INTEGER NOT NULL,
    item_id  TEXT NOT NULL,
    part_id  INTEGER,
    quote    TEXT
);
CREATE INDEX evidence_link ON evidence (link_id);

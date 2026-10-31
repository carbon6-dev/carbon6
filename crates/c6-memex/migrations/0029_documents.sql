-- Documents (docs/docs.md §9): docs and decks, first-class objects of the
-- realm whose knowledge this is. A document is tabs of blocks with stable
-- ids; every batch of operations is a version in doc_ops, with what it
-- changed, and every 50 versions a snapshot of the whole document.
CREATE TABLE docs (
    id          TEXT PRIMARY KEY NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN ('doc', 'deck')),
    title       TEXT NOT NULL,
    -- A deck's theme (a record of deck_themes, by name) and size.
    theme       TEXT NOT NULL DEFAULT '',
    size        TEXT NOT NULL DEFAULT '',
    labels      TEXT NOT NULL DEFAULT '[]',
    -- The latest version, and the clock new block ids are issued from.
    rev         INTEGER NOT NULL,
    clock       INTEGER NOT NULL,
    created_by  TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_by  TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    deleted_at  INTEGER
);
CREATE INDEX docs_updated ON docs (updated_at);

CREATE TABLE doc_tabs (
    doc_id      TEXT NOT NULL,
    id          TEXT NOT NULL,
    title       TEXT NOT NULL,
    pos         REAL NOT NULL,
    PRIMARY KEY (doc_id, id)
);

-- A block: a paragraph, heading, list item, table, code, quote, a pending
-- section, an embed, or a slide (whose regions' blocks name it as parent).
-- Order is by pos within (tab, parent, region).
CREATE TABLE doc_blocks (
    doc_id      TEXT NOT NULL,
    id          TEXT NOT NULL,
    tab         TEXT NOT NULL,
    parent      TEXT NOT NULL DEFAULT '',
    region      TEXT NOT NULL DEFAULT '',
    pos         REAL NOT NULL,
    kind        TEXT NOT NULL,
    text        TEXT NOT NULL DEFAULT '',
    attrs       TEXT NOT NULL DEFAULT '{}',
    -- Who last wrote it, and in which version.
    by          TEXT NOT NULL,
    rev         INTEGER NOT NULL,
    PRIMARY KEY (doc_id, id)
);
CREATE INDEX doc_blocks_tab ON doc_blocks (doc_id, tab);

-- The history: each batch, who sent it (and for a session, the turn), the
-- operations as sent, and what they changed (blocks, tabs and the
-- document's own fields, as they are after; null for gone).
CREATE TABLE doc_ops (
    doc_id      TEXT NOT NULL,
    rev         INTEGER NOT NULL,
    by          TEXT NOT NULL,
    turn        INTEGER,
    at          INTEGER NOT NULL,
    ops         TEXT NOT NULL,
    changes     TEXT NOT NULL,
    answering   TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (doc_id, rev)
);

CREATE TABLE doc_snapshots (
    doc_id      TEXT NOT NULL,
    rev         INTEGER NOT NULL,
    state       TEXT NOT NULL,
    PRIMARY KEY (doc_id, rev)
);

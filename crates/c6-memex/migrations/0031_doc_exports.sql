-- A document's exports (docs/docs.md §10): each a file kept as a blob with
-- the version it was made from, so the PDF someone saw last week can be
-- sent again.
CREATE TABLE doc_exports (
    id          TEXT PRIMARY KEY NOT NULL,
    doc_id      TEXT NOT NULL,
    format      TEXT NOT NULL,
    rev         INTEGER NOT NULL,
    name        TEXT NOT NULL,
    size        INTEGER NOT NULL,
    blob        TEXT NOT NULL,
    by          TEXT NOT NULL,
    at          INTEGER NOT NULL
);
CREATE INDEX doc_exports_doc ON doc_exports (doc_id, at);

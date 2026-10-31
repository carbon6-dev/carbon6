-- Pictures and files in documents (docs/docs.md §3): each a blob in the
-- realm's knowledge folder, named by an image or file embed.
CREATE TABLE doc_files (
    doc_id      TEXT NOT NULL,
    blob        TEXT NOT NULL,
    name        TEXT NOT NULL,
    mime        TEXT NOT NULL,
    size        INTEGER NOT NULL,
    by          TEXT NOT NULL,
    at          INTEGER NOT NULL,
    PRIMARY KEY (doc_id, blob)
);

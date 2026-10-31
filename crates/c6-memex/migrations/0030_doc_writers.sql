-- Sessions you let write a document that isn't their work's (docs/docs.md §9).
CREATE TABLE doc_writers (
    doc_id      TEXT NOT NULL,
    writer      TEXT NOT NULL,
    granted_at  INTEGER NOT NULL,
    PRIMARY KEY (doc_id, writer)
);

-- A document's Google file (docs/docs.md §10): where its last export to
-- Google Docs or Slides went, so the next one updates it.
CREATE TABLE doc_google (
    doc_id      TEXT PRIMARY KEY NOT NULL,
    file_id     TEXT NOT NULL,
    url         TEXT NOT NULL,
    account     TEXT NOT NULL,
    rev         INTEGER NOT NULL,
    modified    TEXT NOT NULL,
    by          TEXT NOT NULL,
    at          INTEGER NOT NULL
);

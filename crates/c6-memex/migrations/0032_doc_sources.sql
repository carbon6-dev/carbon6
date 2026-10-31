-- Where a document came from outside carbon6's own writing (docs/docs.md
-- §13): a decks app record it was brought in from, so it comes in once.
CREATE TABLE doc_sources (
    source      TEXT PRIMARY KEY NOT NULL,
    doc_id      TEXT NOT NULL,
    at          INTEGER NOT NULL
);

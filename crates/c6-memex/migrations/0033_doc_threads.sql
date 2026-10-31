-- Comments on documents (docs/docs.md §8): threads anchored to quoted
-- words, a block or slide, or a tab, and the comments in them.
CREATE TABLE doc_threads (
    id          TEXT PRIMARY KEY NOT NULL,
    doc_id      TEXT NOT NULL,
    anchor      TEXT NOT NULL,
    -- What it was about when it began (shown once it's detached).
    about       TEXT NOT NULL DEFAULT '',
    state       TEXT NOT NULL DEFAULT 'open',
    to_agent    INTEGER NOT NULL DEFAULT 0,
    agent       TEXT NOT NULL DEFAULT '',
    created_by  TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    resolved_by TEXT NOT NULL DEFAULT '',
    resolved_at INTEGER
);
CREATE INDEX doc_threads_doc ON doc_threads (doc_id, state);

CREATE TABLE doc_comments (
    id          TEXT PRIMARY KEY NOT NULL,
    thread_id   TEXT NOT NULL,
    doc_id      TEXT NOT NULL,
    by          TEXT NOT NULL,
    text        TEXT NOT NULL,
    at          INTEGER NOT NULL
);
CREATE INDEX doc_comments_thread ON doc_comments (thread_id, at);

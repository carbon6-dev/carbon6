-- Reply boxes' drafts (docs/conversations.md §3): what a session wrote for
-- a box (its key: thread:…, chat:…, compose:…; a request's draft is the
-- request's), and the session working on it.
CREATE TABLE drafts (
    key      TEXT PRIMARY KEY,
    text     TEXT NOT NULL DEFAULT '',
    session  TEXT NOT NULL DEFAULT '',
    by       TEXT NOT NULL DEFAULT '',
    at       INTEGER NOT NULL
);

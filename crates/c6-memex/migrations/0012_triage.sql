-- What needs you (docs/brief.md): each recent mail thread's and chat
-- conversation's sorting, the senders you marked, and your corrections
-- (the model is shown them as examples).
CREATE TABLE triage (
    target     TEXT PRIMARY KEY NOT NULL,  -- thread:<account>:<thread>, or chat:<workspace>:<channel>[:<thread ts>]
    kind       TEXT NOT NULL,              -- mail or chat
    category   TEXT NOT NULL,              -- needs_you, first_contact, fyi, newsletter, notification, pitch, noise, yours
    reason     TEXT NOT NULL DEFAULT '',
    by         TEXT NOT NULL,              -- rule, signals, sequence, mark, model, you
    sender     TEXT NOT NULL DEFAULT '',   -- who wrote last (not you): an address, or a chat user's id
    sender_name TEXT NOT NULL DEFAULT '',
    title      TEXT NOT NULL DEFAULT '',
    at         INTEGER NOT NULL,           -- its latest message's time
    done_at    INTEGER NOT NULL,
    version    TEXT NOT NULL DEFAULT ''
);
CREATE INDEX triage_category ON triage (category, at);
CREATE INDEX triage_sender ON triage (sender);
CREATE TABLE sender_marks (
    pattern     TEXT PRIMARY KEY NOT NULL, -- an address, or @domain
    mark        TEXT NOT NULL,             -- pitch or known
    created_at  INTEGER NOT NULL
);
CREATE TABLE triage_examples (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,
    sender      TEXT NOT NULL,
    title       TEXT NOT NULL,
    category    TEXT NOT NULL,
    note        TEXT NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL
);

-- What people need from you (docs/brief.md §5): one request per ask, from
-- whichever conversations it came in (mail threads, chat), joined across
-- them; open until you answer, settle or drop it.
CREATE TABLE requests (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    person       TEXT NOT NULL,            -- who asks: an address, or a chat user's id when there's none
    person_name  TEXT NOT NULL DEFAULT '',
    title        TEXT NOT NULL,            -- what they need, one line
    due          INTEGER,
    category     TEXT NOT NULL,            -- needs_you or first_contact
    status       TEXT NOT NULL DEFAULT 'open',  -- open, snoozed, done, dropped
    until        INTEGER,                  -- snoozed: when it comes back
    reason       TEXT NOT NULL DEFAULT '',
    closed_by    TEXT,                     -- you, replied, settled
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
CREATE INDEX requests_status ON requests (status, updated_at);
CREATE INDEX requests_person ON requests (person, status);
-- The conversations a request came from.
CREATE TABLE request_sources (
    request_id  INTEGER NOT NULL,
    target      TEXT NOT NULL,             -- a triage target
    kind        TEXT NOT NULL,             -- mail or chat
    at          INTEGER NOT NULL,            -- its latest message's time
    noted_at    INTEGER NOT NULL DEFAULT 0,  -- when that news reached the request
    PRIMARY KEY (request_id, target)
);
CREATE INDEX request_sources_target ON request_sources (target);
-- Two requests that may be the same ask (the model wasn't sure): shown
-- together, merged with one key.
CREATE TABLE request_maybe (
    a  INTEGER NOT NULL,
    b  INTEGER NOT NULL,
    PRIMARY KEY (a, b)
);

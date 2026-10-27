-- Who you correspond with (docs/relationships.md §2.1): each address's
-- messages each way, threads, how many you answered, how many in bulk.
-- Rebuilt from the mail table (relations.rs).
CREATE TABLE correspondents (
    addr      TEXT PRIMARY KEY NOT NULL,
    domain    TEXT NOT NULL,
    name      TEXT,
    received  INTEGER NOT NULL,   -- messages they sent you
    sent_to   INTEGER NOT NULL,   -- messages you sent them (to or cc)
    threads   INTEGER NOT NULL,   -- threads they wrote in
    answered  INTEGER NOT NULL,   -- of those, threads you wrote in too
    bulk      INTEGER NOT NULL,   -- of theirs, with mailing-list headers
    first_at  INTEGER,
    last_at   INTEGER
);
CREATE INDEX correspondents_domain ON correspondents (domain);
CREATE TABLE correspondents_built (
    id    INTEGER PRIMARY KEY CHECK (id = 1),
    mark  TEXT NOT NULL,          -- the mail it was counted from: <count>:<latest date>
    at    INTEGER NOT NULL
);
-- Sorting now reads each conversation with its history, and the model
-- sorts what rules used to: everything recent is sorted again, except what
-- you sorted or marked yourself.
UPDATE triage SET done_at = 0 WHERE by NOT IN ('you', 'mark');

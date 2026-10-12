-- Follow-through (docs/follow-through.md): next steps offered in sessions,
-- and trackers following what sessions start outside carbon6, with every
-- change they saw (kept, like sessions).
CREATE TABLE suggestions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL,
    anchor_seq  INTEGER NOT NULL,
    kind        TEXT NOT NULL,          -- next | note
    text        TEXT NOT NULL,
    prompt      TEXT NOT NULL,
    source      TEXT NOT NULL,
    state       TEXT NOT NULL DEFAULT 'open',
    created_at  INTEGER NOT NULL
);
CREATE INDEX suggestions_session ON suggestions (session_id, state);

CREATE TABLE trackers (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id     TEXT NOT NULL,
    anchor_seq     INTEGER NOT NULL,
    kit            TEXT NOT NULL,
    name           TEXT NOT NULL,
    title          TEXT NOT NULL,
    def            TEXT NOT NULL,       -- the TrackerDef (JSON), as started
    vars           TEXT NOT NULL,       -- template context (JSON)
    phase          TEXT NOT NULL,
    line           TEXT NOT NULL DEFAULT '',
    url            TEXT NOT NULL DEFAULT '',
    error          TEXT NOT NULL DEFAULT '',
    started_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    next_at        INTEGER NOT NULL,
    ended_at       INTEGER,
    investigation  TEXT
);
CREATE INDEX trackers_session ON trackers (session_id);
CREATE INDEX trackers_due ON trackers (ended_at, next_at);

CREATE TABLE tracker_observations (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    tracker_id  INTEGER NOT NULL,
    ts          INTEGER NOT NULL,
    phase       TEXT NOT NULL,
    line        TEXT NOT NULL,
    url         TEXT NOT NULL,
    output      TEXT NOT NULL
);
CREATE INDEX tracker_observations_tracker ON tracker_observations (tracker_id, ts);

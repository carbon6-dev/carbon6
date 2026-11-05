-- Watchers (docs/watchers.md): standing agents that follow sessions.
-- A watcher's state per scope: key is the watched session's id, or '*'
-- for a watcher over everything. Its position and memory live here, not
-- in its conversation, so they outlast compaction and restarts.
CREATE TABLE watcher_state (
    watcher      TEXT NOT NULL,
    key          TEXT NOT NULL,
    realm        TEXT NOT NULL,
    session_id   TEXT NOT NULL,          -- the watcher's own session
    position     INTEGER NOT NULL DEFAULT 0,
    memory       TEXT NOT NULL DEFAULT '',
    wakes        INTEGER NOT NULL DEFAULT 0,
    skips        INTEGER NOT NULL DEFAULT 0,
    turn         INTEGER NOT NULL DEFAULT 0,
    last_wake_at INTEGER NOT NULL DEFAULT 0,
    ended_at     INTEGER,
    PRIMARY KEY (watcher, key)
);
CREATE INDEX watcher_state_session ON watcher_state (session_id);

-- Tokens a watcher spent, per UTC day (its budget).
CREATE TABLE watcher_spend (
    watcher TEXT NOT NULL,
    day     TEXT NOT NULL,
    tokens  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (watcher, day)
);

-- What watchers found, and what was done with it.
CREATE TABLE watcher_findings (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    watcher     TEXT NOT NULL,
    realm       TEXT NOT NULL,
    watched     TEXT NOT NULL DEFAULT '',  -- the watched session, if one
    session_id  TEXT NOT NULL,             -- the watcher's session
    seq         INTEGER NOT NULL DEFAULT 0, -- its turn's event there
    severity    TEXT NOT NULL,             -- info, important, urgent
    kind        TEXT NOT NULL,             -- note, issue, lesson, directive, change, ledger, card
    text        TEXT NOT NULL,
    done        TEXT NOT NULL DEFAULT '',  -- what happened: sent, marked, applied, asked, carded…
    ref         TEXT NOT NULL DEFAULT '',  -- memory:<id>, decision:<id>, …
    seen        INTEGER NOT NULL DEFAULT 0,
    at          INTEGER NOT NULL
);
CREATE INDEX watcher_findings_watched ON watcher_findings (watched, at);
CREATE INDEX watcher_findings_watcher ON watcher_findings (watcher, at);

-- carbon6's own failures, grouped without a model (the improvement watcher).
CREATE TABLE watch_signals (
    key        TEXT PRIMARY KEY,           -- source and the message with ids and numbers taken out
    source     TEXT NOT NULL,              -- log, ui, tool
    sample     TEXT NOT NULL,
    count      INTEGER NOT NULL DEFAULT 0,
    first_at   INTEGER NOT NULL,
    last_at    INTEGER NOT NULL,
    reported   INTEGER NOT NULL DEFAULT 0  -- count when last given to the watcher
);

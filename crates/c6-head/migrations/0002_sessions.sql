CREATE TABLE connections (
    id             TEXT PRIMARY KEY NOT NULL,
    name           TEXT NOT NULL,
    kind           TEXT NOT NULL,
    base_url       TEXT NOT NULL,
    api_key        BLOB,              -- nonce || ciphertext under master.key
    default_model  TEXT,
    created_at     INTEGER NOT NULL
);

CREATE TABLE sessions (
    id             TEXT PRIMARY KEY NOT NULL,
    worker_id      TEXT NOT NULL,
    title          TEXT NOT NULL,
    cwd            TEXT NOT NULL,
    connection_id  TEXT NOT NULL,
    model          TEXT NOT NULL,
    status         TEXT NOT NULL,     -- starting | idle | running | failed
    error          TEXT,              -- why it failed to start
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    last_seq       INTEGER NOT NULL DEFAULT 0,
    archived       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX sessions_by_worker ON sessions (worker_id);
CREATE INDEX sessions_by_updated ON sessions (updated_at);

-- The authoritative session logs.
CREATE TABLE events (
    session_id     TEXT NOT NULL,
    seq            INTEGER NOT NULL,
    ts             INTEGER NOT NULL,
    data           BLOB NOT NULL,     -- SessionEvent protobuf
    PRIMARY KEY (session_id, seq)
) WITHOUT ROWID;

-- Durable commands awaiting the worker. `seq` orders delivery.
CREATE TABLE commands (
    seq            INTEGER PRIMARY KEY AUTOINCREMENT,
    id             TEXT NOT NULL UNIQUE,
    session_id     TEXT NOT NULL,
    worker_id      TEXT NOT NULL,
    data           BLOB NOT NULL,     -- SessionCommand protobuf
    text           TEXT,              -- user input, for showing the queue
    created_at     INTEGER NOT NULL,
    state          TEXT NOT NULL DEFAULT 'pending',  -- pending | done | rejected
    error          TEXT
);
CREATE INDEX commands_by_worker ON commands (worker_id, state, seq);
CREATE INDEX commands_by_session ON commands (session_id, state);

CREATE TABLE workers (
    id           TEXT PRIMARY KEY NOT NULL,
    name         TEXT NOT NULL,
    hostname     TEXT NOT NULL,
    os           TEXT NOT NULL,
    arch         TEXT NOT NULL,
    version      TEXT,
    cert_serial  TEXT NOT NULL,
    enrolled_at  INTEGER NOT NULL,
    last_seen    INTEGER,
    revoked_at   INTEGER
);

CREATE TABLE enrollments (
    secret_hash  TEXT PRIMARY KEY NOT NULL,
    name         TEXT,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    used_at      INTEGER,
    worker_id    TEXT
);

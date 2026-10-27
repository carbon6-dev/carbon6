-- One-time jobs the memex has done (by name), so they run once.
CREATE TABLE memex_once (
    name  TEXT PRIMARY KEY NOT NULL,
    at    INTEGER NOT NULL
);

-- The last reachability check of each host in the inventory.
CREATE TABLE host_checks (
    name        TEXT PRIMARY KEY NOT NULL,
    at          INTEGER NOT NULL,
    ok          INTEGER NOT NULL,
    worker      TEXT NOT NULL,
    message     TEXT NOT NULL,
    latency_ms  INTEGER NOT NULL
);

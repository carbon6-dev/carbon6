-- Grants (docs/os.md §3.7): the only way data crosses realms. "A's data
-- may flow into B": B reads, appends to or snapshots A's collection,
-- reads A's knowledge of some kinds, or calls some of A's operations.
-- Every use is logged; only you create one (an agent may request it,
-- as a decision).
CREATE TABLE grants (
    id          TEXT PRIMARY KEY NOT NULL,
    from_realm  TEXT NOT NULL,
    to_realm    TEXT NOT NULL,
    kind        TEXT NOT NULL,                 -- collection | memex | operations
    scope       TEXT NOT NULL DEFAULT '{}',    -- {"collection", "where"} | {"kinds"} | {"methods"}
    mode        TEXT NOT NULL,                 -- read | append | snapshot | call
    state       TEXT NOT NULL DEFAULT 'active',-- active | requested | revoked | denied
    expires_at  INTEGER,
    note        TEXT NOT NULL DEFAULT '',
    created_by  TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    revoked_at  INTEGER
);
CREATE INDEX grants_to ON grants (to_realm, state);

CREATE TABLE grant_uses (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    grant_id  TEXT NOT NULL,
    at        INTEGER NOT NULL,
    by        TEXT NOT NULL,                   -- session:<id>, token:<realm>
    what      TEXT NOT NULL
);
CREATE INDEX grant_uses_grant ON grant_uses (grant_id, at);

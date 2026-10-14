-- Collections (docs/os.md §4.1): typed tables agents and code use for
-- anything. A record's fields always match its collection's schema;
-- every change is kept, with who made it.
CREATE TABLE collections (
    realm           TEXT NOT NULL,
    name            TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    schema          TEXT NOT NULL,                 -- JSON Schema (strict)
    schema_version  INTEGER NOT NULL DEFAULT 1,
    keys            TEXT NOT NULL DEFAULT '[]',    -- fields whose values are unique
    system          INTEGER NOT NULL DEFAULT 0,    -- carbon6's own (configuration)
    created_by      TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (realm, name)
);

CREATE TABLE records (
    id          TEXT PRIMARY KEY NOT NULL,
    realm       TEXT NOT NULL,
    collection  TEXT NOT NULL,
    fields      TEXT NOT NULL,                     -- JSON object
    links       TEXT NOT NULL DEFAULT '[]',        -- memex things and items
    labels      TEXT NOT NULL DEFAULT '[]',
    version     INTEGER NOT NULL DEFAULT 1,
    created_by  TEXT NOT NULL,
    updated_by  TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX records_collection ON records (realm, collection, updated_at);

CREATE TABLE record_history (
    record_id   TEXT NOT NULL,
    version     INTEGER NOT NULL,
    op          TEXT NOT NULL,                     -- create | update | delete
    fields      TEXT NOT NULL,
    changed_by  TEXT NOT NULL,
    changed_at  INTEGER NOT NULL,
    PRIMARY KEY (record_id, version)
);

-- Apps (docs/os.md §5): packages of configuration, each installed into a
-- realm of its own. `installed` is the package as installed (the base an
-- upgrade compares your edits and the new version with); `pending` is a
-- package waiting for your decision.
CREATE TABLE apps (
    realm        TEXT PRIMARY KEY NOT NULL,
    app          TEXT NOT NULL,
    version      TEXT NOT NULL DEFAULT '',
    description  TEXT NOT NULL DEFAULT '',
    state        TEXT NOT NULL,              -- installing | installed | upgrading | removing
    installed    TEXT NOT NULL DEFAULT '{}',
    pending      TEXT,
    decision     INTEGER,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);

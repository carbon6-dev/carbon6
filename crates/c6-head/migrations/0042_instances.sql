-- App instances (docs/apps.md §5): a package version installed in a realm
-- under a name, possibly many times. An instance's code is its package's,
-- read from the registry; what it owns (collections, series, triggers) says
-- so, and its sessions act as it.

-- Installs are named, and unique by realm and name rather than by package
-- and realm (one package, many instances).
CREATE TABLE installs_new (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    package      TEXT NOT NULL,
    version      TEXT NOT NULL,
    digest       TEXT NOT NULL,
    realm        TEXT NOT NULL DEFAULT '',
    name         TEXT NOT NULL,              -- the instance's name in its realm
    state        TEXT NOT NULL,
    approved     TEXT NOT NULL DEFAULT '{}',
    locks        TEXT NOT NULL DEFAULT '{}',
    required_by  TEXT NOT NULL DEFAULT '[]',
    installed    TEXT NOT NULL DEFAULT '{}',
    pending      TEXT,
    decision     INTEGER,
    settings     TEXT NOT NULL DEFAULT '{}', -- values of the settings its package declares
    adopted      INTEGER NOT NULL DEFAULT 1, -- 0: installed before instances; its realm's copies are still to sort out
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    UNIQUE (realm, name)
);
INSERT INTO installs_new (id, package, version, digest, realm, name, state, approved, locks, required_by, installed, pending, decision,
                          settings, adopted, created_at, updated_at)
    SELECT id, package, version, digest, realm, package, state, approved, locks, required_by, installed, pending, decision,
           '{}', 0, created_at, updated_at FROM installs;
DROP TABLE installs;
ALTER TABLE installs_new RENAME TO installs;

-- What an instance owns.
ALTER TABLE collections ADD COLUMN owner TEXT NOT NULL DEFAULT '';
ALTER TABLE series ADD COLUMN owner TEXT NOT NULL DEFAULT '';
ALTER TABLE triggers ADD COLUMN owner TEXT NOT NULL DEFAULT '';
-- The instance a session acts as.
ALTER TABLE sessions ADD COLUMN instance TEXT NOT NULL DEFAULT '';

-- Apps installed before: their collections, series and triggers (the ones
-- their package brought, by name) are theirs.
UPDATE collections SET owner = (
    SELECT i.name FROM installs i WHERE i.realm = collections.realm AND i.state = 'installed'
       AND EXISTS (SELECT 1 FROM json_each(json_extract(i.installed, '$.contents.collections')) c WHERE json_extract(c.value, '$.name') = collections.name)
) WHERE system = 0 AND EXISTS (
    SELECT 1 FROM installs i WHERE i.realm = collections.realm AND i.state = 'installed'
       AND EXISTS (SELECT 1 FROM json_each(json_extract(i.installed, '$.contents.collections')) c WHERE json_extract(c.value, '$.name') = collections.name)
);
UPDATE series SET owner = (
    SELECT i.name FROM installs i WHERE i.realm = series.realm AND i.state = 'installed'
       AND EXISTS (SELECT 1 FROM json_each(json_extract(i.installed, '$.contents.series')) c WHERE json_extract(c.value, '$.name') = series.name)
) WHERE system = 0 AND EXISTS (
    SELECT 1 FROM installs i WHERE i.realm = series.realm AND i.state = 'installed'
       AND EXISTS (SELECT 1 FROM json_each(json_extract(i.installed, '$.contents.series')) c WHERE json_extract(c.value, '$.name') = series.name)
);
UPDATE triggers SET owner = (
    SELECT i.name FROM installs i WHERE i.realm = triggers.realm AND i.state = 'installed'
       AND EXISTS (SELECT 1 FROM json_each(json_extract(i.installed, '$.contents.triggers')) c WHERE json_extract(c.value, '$.name') = triggers.name)
) WHERE EXISTS (
    SELECT 1 FROM installs i WHERE i.realm = triggers.realm AND i.state = 'installed'
       AND EXISTS (SELECT 1 FROM json_each(json_extract(i.installed, '$.contents.triggers')) c WHERE json_extract(c.value, '$.name') = triggers.name)
);

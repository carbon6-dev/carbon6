-- Packages (docs/packages.md): the local registry, and what is installed.
-- Apps installed before become packages in the registry and installs of
-- them; their digests are computed when the head starts (they start as
-- legacy:<realm>).
CREATE TABLE packages (
    digest    TEXT PRIMARY KEY NOT NULL,
    name      TEXT NOT NULL,
    version   TEXT NOT NULL,
    manifest  TEXT NOT NULL,
    roles     TEXT NOT NULL,              -- JSON: app, kit, library
    source    TEXT NOT NULL DEFAULT 'local',
    added_at  INTEGER NOT NULL,
    UNIQUE (name, version)
);
CREATE TABLE package_files (
    digest  TEXT NOT NULL,
    path    TEXT NOT NULL,
    data    BLOB NOT NULL,
    PRIMARY KEY (digest, path)
);
CREATE TABLE installs (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    package      TEXT NOT NULL,
    version      TEXT NOT NULL,
    digest       TEXT NOT NULL,
    realm        TEXT NOT NULL DEFAULT '',   -- its realm (app), the realm it is in (library), '' (kit)
    state        TEXT NOT NULL,              -- installing | installed | upgrading | removing
    approved     TEXT NOT NULL DEFAULT '{}', -- the permissions approved
    locks        TEXT NOT NULL DEFAULT '{}', -- what it requires: name → digest
    required_by  TEXT NOT NULL DEFAULT '[]', -- installs that require it
    installed    TEXT NOT NULL DEFAULT '{}', -- the manifest as installed (the base of upgrades)
    pending      TEXT,                       -- what waits for your decision
    decision     INTEGER,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    UNIQUE (package, realm)
);

INSERT INTO packages (digest, name, version, manifest, roles, source, added_at)
    SELECT 'legacy:' || realm, app, version, CASE WHEN installed = '{}' THEN coalesce(pending, '{}') ELSE installed END, '["app"]', 'local', created_at
    FROM apps;
INSERT INTO installs (package, version, digest, realm, state, approved, installed, pending, decision, created_at, updated_at)
    SELECT app, version, 'legacy:' || realm, realm, state, '{}', installed, pending, decision, created_at, updated_at FROM apps;
DROP TABLE apps;

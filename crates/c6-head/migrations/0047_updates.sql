-- Updating carbon6 from signed GitHub releases (docs/updates.md).
-- A release the head found and verified, and where it is.
CREATE TABLE updates (
    version TEXT PRIMARY KEY,
    manifest TEXT NOT NULL,          -- release.json, exactly as signed
    signature TEXT NOT NULL,         -- release.json.sig, hex
    base_url TEXT NOT NULL,          -- where its files are
    found_at INTEGER NOT NULL,
    -- available · approved · skipped · applying · done · failed
    state TEXT NOT NULL DEFAULT 'available',
    detail TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL
);

-- Each machine's part in an update: the head's ("head") and each worker's.
CREATE TABLE update_machines (
    machine TEXT NOT NULL,           -- "head", or a worker id
    version TEXT NOT NULL,
    -- waiting · downloading · installing · restarting · done · failed · rolled_back
    state TEXT NOT NULL,
    method TEXT NOT NULL DEFAULT '',
    detail TEXT NOT NULL DEFAULT '',
    started_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (machine, version)
);

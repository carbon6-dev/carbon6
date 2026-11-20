-- Derived index bookkeeping. FTS5's UNINDEXED session/seq columns cannot
-- support range deletes without scanning the entire index for every batch.
CREATE TABLE search_entries (
    rowid INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL
);
CREATE INDEX search_entries_by_event ON search_entries(session_id, seq);
CREATE TABLE search_backfill (
    version TEXT NOT NULL,
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL DEFAULT 0,
    through INTEGER NOT NULL,
    done INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(version, session_id)
);
CREATE TABLE search_rebuild (
    version TEXT PRIMARY KEY,
    old_rowid INTEGER NOT NULL,
    cursor INTEGER NOT NULL DEFAULT 0,
    cleared INTEGER NOT NULL DEFAULT 0
);

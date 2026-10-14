-- Commons (docs/os.md §3.3): knowledge meant for every realm. Realms
-- publish into it on purpose, and you review each item first; every realm
-- reads it unless you turn that off for the realm.
INSERT OR IGNORE INTO realms (id, name, description, created_at, created_by) VALUES
    ('commons', 'Commons', 'Knowledge meant for every realm: published into on purpose, each item reviewed by you.', CAST(strftime('%s', 'now') AS INTEGER) * 1000, 'system');
ALTER TABLE realms ADD COLUMN reads_commons INTEGER NOT NULL DEFAULT 1;

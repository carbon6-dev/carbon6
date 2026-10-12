-- Action items (docs/follow-through.md §7): things for you to settle when
-- you want to, raised by sessions, the memex (disputed facts) or kits, kept
-- with their context. Items with the same group are one item.
CREATE TABLE todos (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    kind          TEXT NOT NULL,
    title         TEXT NOT NULL,
    detail        TEXT NOT NULL DEFAULT '',
    context       TEXT NOT NULL DEFAULT '{}',
    source        TEXT NOT NULL,
    priority      TEXT NOT NULL DEFAULT 'normal',
    grp           TEXT NOT NULL DEFAULT '',
    state         TEXT NOT NULL DEFAULT 'open',
    snooze_until  INTEGER,
    resolution    TEXT NOT NULL DEFAULT '',
    sessions      TEXT NOT NULL DEFAULT '[]',
    count         INTEGER NOT NULL DEFAULT 1,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);
CREATE INDEX todos_state ON todos (state, updated_at);
CREATE INDEX todos_group ON todos (grp, state);

-- Files an app keeps for showing (docs/conversations.md §2.2): a chat's
-- pictures. Not items: not searched, not photos.
CREATE TABLE kept_files (
    source      TEXT NOT NULL,
    external_id TEXT NOT NULL,
    blob        TEXT NOT NULL,
    mime        TEXT NOT NULL,
    name        TEXT NOT NULL DEFAULT '',
    size        INTEGER NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (source, external_id)
);
CREATE INDEX kept_files_external ON kept_files (external_id);

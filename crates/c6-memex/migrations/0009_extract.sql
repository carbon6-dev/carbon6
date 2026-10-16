-- Extraction (docs/knowledge.md §4): what waits to be read by the
-- c6-extract kit on a worker. One job per key (what it is for).
CREATE TABLE extractions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    key         TEXT NOT NULL UNIQUE,
    tool        TEXT NOT NULL,             -- document | mail | media | faces | video
    input       TEXT NOT NULL,             -- head:<blob> | worker:<worker>:<path> | a path on the head's machine
    name        TEXT NOT NULL DEFAULT '',
    params      TEXT NOT NULL DEFAULT '{}',
    attempts    INTEGER NOT NULL DEFAULT 0,
    next_at     INTEGER NOT NULL DEFAULT 0,
    error       TEXT,
    created_at  INTEGER NOT NULL
);
CREATE INDEX extractions_due ON extractions (next_at, id);

-- A message's text and HTML as read once (blobs), and its attachments'
-- names: shown without reading the message again. Mail kept before this
-- is read again in the background (parsed = 0).
ALTER TABLE mail ADD COLUMN text_blob TEXT;
ALTER TABLE mail ADD COLUMN html_blob TEXT;
ALTER TABLE mail ADD COLUMN parts TEXT NOT NULL DEFAULT '[]';
ALTER TABLE mail ADD COLUMN parsed INTEGER NOT NULL DEFAULT 0;

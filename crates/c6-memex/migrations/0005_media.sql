-- Photos and videos: what the file says (EXIF, the container), a
-- thumbnail, and how far each has been looked at.
CREATE TABLE media (
    item_id      TEXT PRIMARY KEY NOT NULL,
    path         TEXT NOT NULL,
    kind         TEXT NOT NULL,              -- photo | video
    taken_at     INTEGER,
    lat          REAL,
    lon          REAL,
    place        TEXT,                       -- the place thing, if known
    camera       TEXT NOT NULL DEFAULT '',
    width        INTEGER,
    height       INTEGER,
    duration     REAL,                       -- seconds (videos)
    thumb        TEXT,                       -- blob hash of a JPEG
    frames       TEXT NOT NULL DEFAULT '[]', -- keyframes' blob hashes (videos)
    seen         TEXT,                       -- vision version that captioned it
    faces_done   TEXT,                       -- face models that looked at it
    heard        TEXT,                       -- transcription version
    error        TEXT
);
CREATE INDEX media_taken ON media (taken_at);
-- Faces found in photos: where (0..1 of the picture), what they look like
-- (an embedding), and which person (a thing: a cluster, until named).
CREATE TABLE faces (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id   TEXT NOT NULL,
    x         REAL NOT NULL,
    y         REAL NOT NULL,
    w         REAL NOT NULL,
    h         REAL NOT NULL,
    score     REAL NOT NULL,
    vec       BLOB NOT NULL,
    person    TEXT,
    crop      TEXT NOT NULL               -- blob hash of a JPEG
);
CREATE INDEX faces_item ON faces (item_id);
CREATE INDEX faces_person ON faces (person);

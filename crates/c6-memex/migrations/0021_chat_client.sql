-- The Chat page as a full client (docs/chat.md §2.2): conversations and
-- their read state, people, your organization of them, threads you've
-- looked at, and messages saved for later.
CREATE TABLE chat_conversations (
    workspace       TEXT NOT NULL,
    channel         TEXT NOT NULL,
    workspace_name  TEXT NOT NULL DEFAULT '',
    name            TEXT NOT NULL DEFAULT '',
    kind            TEXT NOT NULL DEFAULT 'public',   -- public, private, dm or group
    topic           TEXT NOT NULL DEFAULT '',
    purpose         TEXT NOT NULL DEFAULT '',
    members         INTEGER,
    user            TEXT,                             -- a DM's other person
    archived        INTEGER NOT NULL DEFAULT 0,
    last_at         INTEGER NOT NULL DEFAULT 0,       -- its newest message
    slack_read      TEXT,                             -- what the service last said was read
    read_ts         TEXT,                             -- read up to here
    PRIMARY KEY (workspace, channel)
);

CREATE TABLE chat_people (
    workspace  TEXT NOT NULL,
    id         TEXT NOT NULL,
    name       TEXT NOT NULL,
    email      TEXT NOT NULL DEFAULT '',
    avatar     TEXT NOT NULL DEFAULT '',
    title      TEXT NOT NULL DEFAULT '',
    status     TEXT NOT NULL DEFAULT '',
    bot        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (workspace, id)
);

-- Per conversation; the sections themselves (in order) under channel ''.
CREATE TABLE chat_prefs (
    workspace  TEXT NOT NULL,
    channel    TEXT NOT NULL,
    starred    INTEGER NOT NULL DEFAULT 0,
    muted      INTEGER NOT NULL DEFAULT 0,
    hidden     INTEGER NOT NULL DEFAULT 0,
    section    TEXT NOT NULL DEFAULT '',
    sections   TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (workspace, channel)
);

CREATE TABLE chat_thread_reads (
    workspace  TEXT NOT NULL,
    channel    TEXT NOT NULL,
    thread_ts  TEXT NOT NULL,
    read_ts    TEXT NOT NULL,
    PRIMARY KEY (workspace, channel, thread_ts)
);

CREATE TABLE chat_saved (
    item_id   TEXT PRIMARY KEY NOT NULL,
    saved_at  INTEGER NOT NULL,
    done_at   INTEGER
);

CREATE INDEX items_chat_thread ON items (json_extract(meta, '$.workspace'), json_extract(meta, '$.channel'), json_extract(meta, '$.thread_ts')) WHERE kind = 'chat';
CREATE INDEX items_chat_mine ON items (json_extract(meta, '$.from_me'), created_at) WHERE kind = 'chat';
CREATE INDEX items_chat_mentions ON items (json_extract(meta, '$.mentions_me'), created_at) WHERE kind = 'chat';

-- The conversations there are messages of so far.
INSERT INTO chat_conversations (workspace, channel, workspace_name, name, kind, last_at)
SELECT json_extract(meta, '$.workspace'), json_extract(meta, '$.channel'), MAX(json_extract(meta, '$.workspace_name')),
       MAX(json_extract(meta, '$.channel_name')), COALESCE(MAX(json_extract(meta, '$.channel_kind')), 'public'), MAX(created_at)
FROM items WHERE kind = 'chat' AND json_extract(meta, '$.workspace') IS NOT NULL AND json_extract(meta, '$.channel') IS NOT NULL
GROUP BY json_extract(meta, '$.workspace'), json_extract(meta, '$.channel');

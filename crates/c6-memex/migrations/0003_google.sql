-- More keys for a thing (a person's other addresses).
CREATE TABLE thing_keys (
    key       TEXT PRIMARY KEY NOT NULL,
    thing_id  TEXT NOT NULL
);

-- Where each sync is: per account and kind ("mail", "calendars",
-- "cal:<calendar id>", "contacts").
CREATE TABLE sync_state (
    account     TEXT NOT NULL,
    kind        TEXT NOT NULL,
    cursor      TEXT,            -- Gmail history id, or a sync token
    page_token  TEXT,            -- where the history import is
    phase       TEXT NOT NULL DEFAULT 'import',
    done        INTEGER NOT NULL DEFAULT 0,
    error       TEXT,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (account, kind)
);

-- Mail, indexed for the mail client's views (the message is an item).
CREATE TABLE mail (
    item_id      TEXT PRIMARY KEY NOT NULL,
    account      TEXT NOT NULL,
    gmail_id     TEXT NOT NULL,
    thread_id    TEXT NOT NULL,
    rfc_id       TEXT NOT NULL DEFAULT '',
    date         INTEGER NOT NULL,
    from_addr    TEXT NOT NULL DEFAULT '',
    from_name    TEXT NOT NULL DEFAULT '',
    to_json      TEXT NOT NULL DEFAULT '[]',
    cc_json      TEXT NOT NULL DEFAULT '[]',
    subject      TEXT NOT NULL DEFAULT '',
    snippet      TEXT NOT NULL DEFAULT '',
    labels       TEXT NOT NULL DEFAULT '[]',
    unread       INTEGER NOT NULL DEFAULT 0,
    starred      INTEGER NOT NULL DEFAULT 0,
    inbox        INTEGER NOT NULL DEFAULT 0,
    spam         INTEGER NOT NULL DEFAULT 0,
    trash        INTEGER NOT NULL DEFAULT 0,
    sent         INTEGER NOT NULL DEFAULT 0,
    draft        INTEGER NOT NULL DEFAULT 0,
    list         INTEGER NOT NULL DEFAULT 0,   -- a mailing list or bulk sender
    attachments  INTEGER NOT NULL DEFAULT 0,
    blob         TEXT NOT NULL,                -- the raw message
    UNIQUE (account, gmail_id)
);
CREATE INDEX mail_thread ON mail (account, thread_id);
CREATE INDEX mail_date ON mail (date);
CREATE INDEX mail_inbox ON mail (inbox, date);
CREATE INDEX mail_from ON mail (from_addr);
CREATE TABLE mail_labels (
    item_id  TEXT NOT NULL,
    label    TEXT NOT NULL,
    PRIMARY KEY (item_id, label)
);
CREATE INDEX mail_labels_label ON mail_labels (label);
CREATE TABLE label_names (
    account  TEXT NOT NULL,
    id       TEXT NOT NULL,
    name     TEXT NOT NULL,
    type     TEXT NOT NULL,
    PRIMARY KEY (account, id)
);

-- Mail being sent: held a few seconds (undo), or until a time (later).
CREATE TABLE outgoing (
    id          TEXT PRIMARY KEY NOT NULL,
    account     TEXT NOT NULL,
    raw         TEXT NOT NULL,          -- base64url RFC 822
    thread_id   TEXT,
    send_at     INTEGER NOT NULL,
    status      TEXT NOT NULL,          -- pending, sent, cancelled, failed
    error       TEXT,
    subject     TEXT NOT NULL DEFAULT '',
    to_json     TEXT NOT NULL DEFAULT '[]',
    made_by     TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);

CREATE TABLE calendars (
    account      TEXT NOT NULL,
    id           TEXT NOT NULL,
    summary      TEXT NOT NULL,
    color        TEXT NOT NULL DEFAULT '',
    is_primary   INTEGER NOT NULL DEFAULT 0,
    access       TEXT NOT NULL DEFAULT '',
    selected     INTEGER NOT NULL DEFAULT 1,
    time_zone    TEXT NOT NULL DEFAULT 'UTC',
    PRIMARY KEY (account, id)
);
CREATE TABLE events (
    account             TEXT NOT NULL,
    calendar_id         TEXT NOT NULL,
    event_id            TEXT NOT NULL,
    status              TEXT NOT NULL,
    summary             TEXT NOT NULL DEFAULT '',
    description         TEXT NOT NULL DEFAULT '',
    location            TEXT NOT NULL DEFAULT '',
    start               INTEGER NOT NULL,      -- ms; all-day: local midnight UTC
    end                 INTEGER NOT NULL,
    all_day             INTEGER NOT NULL DEFAULT 0,
    time_zone           TEXT NOT NULL DEFAULT '',
    recurrence          TEXT NOT NULL DEFAULT '[]',
    recurring_event_id  TEXT,
    original_start      INTEGER,
    attendees           TEXT NOT NULL DEFAULT '[]',
    organizer           TEXT NOT NULL DEFAULT '',
    my_status           TEXT NOT NULL DEFAULT '',
    transparent         INTEGER NOT NULL DEFAULT 0,
    html_link           TEXT NOT NULL DEFAULT '',
    conference          TEXT NOT NULL DEFAULT '',
    updated             INTEGER NOT NULL DEFAULT 0,
    item_id             TEXT,
    PRIMARY KEY (account, calendar_id, event_id)
);
CREATE INDEX events_start ON events (start);
CREATE INDEX events_master ON events (account, calendar_id, recurring_event_id);

CREATE TABLE contacts (
    account   TEXT NOT NULL,
    resource  TEXT NOT NULL,
    etag      TEXT NOT NULL DEFAULT '',
    name      TEXT NOT NULL DEFAULT '',
    emails    TEXT NOT NULL DEFAULT '[]',
    phones    TEXT NOT NULL DEFAULT '[]',
    orgs      TEXT NOT NULL DEFAULT '[]',
    photo     TEXT NOT NULL DEFAULT '',
    groups    TEXT NOT NULL DEFAULT '[]',
    thing_id  TEXT,
    updated   INTEGER NOT NULL,
    PRIMARY KEY (account, resource)
);

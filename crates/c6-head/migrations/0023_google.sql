-- Google accounts (Gmail, Calendar, Contacts) signed in through your own
-- OAuth client. Tokens are encrypted; the memex gets short-lived access
-- tokens over the fabric and never the refresh token.
CREATE TABLE google_accounts (
    id              TEXT PRIMARY KEY NOT NULL,
    email           TEXT NOT NULL UNIQUE,
    name            TEXT NOT NULL DEFAULT '',
    refresh_token   BLOB NOT NULL,
    access_token    BLOB,
    access_expires  INTEGER NOT NULL DEFAULT 0,
    scopes          TEXT NOT NULL DEFAULT '',
    added_at        INTEGER NOT NULL
);

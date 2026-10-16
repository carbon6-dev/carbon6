-- OAuth providers declared by packages (docs/knowledge.md §6.2, #2): the
-- accounts you connected for an app instance, each with its refresh token
-- (encrypted). The app's kit jobs get short-lived access tokens only.
CREATE TABLE oauth_accounts (
    id              TEXT PRIMARY KEY NOT NULL,
    realm           TEXT NOT NULL,
    instance        TEXT NOT NULL,
    provider        TEXT NOT NULL,
    account         TEXT NOT NULL,              -- e.g. the email address
    name            TEXT NOT NULL DEFAULT '',
    refresh_token   BLOB NOT NULL,
    access_token    BLOB,
    access_expires  INTEGER NOT NULL DEFAULT 0,
    scopes          TEXT NOT NULL DEFAULT '',
    added_at        INTEGER NOT NULL,
    UNIQUE (realm, instance, provider, account)
);

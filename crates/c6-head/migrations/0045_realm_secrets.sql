-- Secrets are a realm's (docs/os.md §3): the same name can hold a value in
-- each realm (work's GITHUB_TOKEN and home's), and a realm without one
-- uses root's.
CREATE TABLE secrets_by_realm (
    realm        TEXT NOT NULL DEFAULT 'home',
    name         TEXT NOT NULL,
    value        BLOB NOT NULL,              -- nonce || ciphertext
    description  TEXT NOT NULL DEFAULT '',
    applies_to   TEXT NOT NULL DEFAULT '[]', -- JSON array of labels
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (realm, name)
);
INSERT INTO secrets_by_realm (realm, name, value, description, applies_to, created_at, updated_at)
    SELECT realm, name, value, description, applies_to, created_at, updated_at FROM secrets;
DROP TABLE secrets;
ALTER TABLE secrets_by_realm RENAME TO secrets;

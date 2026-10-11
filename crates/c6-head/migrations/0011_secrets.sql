-- Secrets for sessions: environment variables for their commands. Values
-- are encrypted under master.key and never sent to the browser.
CREATE TABLE secrets (
    name         TEXT PRIMARY KEY NOT NULL,
    value        BLOB NOT NULL,              -- nonce || ciphertext
    description  TEXT NOT NULL DEFAULT '',
    applies_to   TEXT NOT NULL DEFAULT '[]', -- JSON array of labels
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);

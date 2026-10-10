-- Web UI sign-in: passkeys, browser sessions and one-time login links.
-- Secrets (session tokens, link tokens) are stored as SHA-256 hashes.
CREATE TABLE passkeys (
    id            TEXT PRIMARY KEY NOT NULL,   -- credential id, base64url
    rp_id         TEXT NOT NULL,               -- the host name it was made for
    public_key    BLOB NOT NULL,               -- SubjectPublicKeyInfo DER
    alg           INTEGER NOT NULL,            -- COSE algorithm (-7, -8, -257)
    name          TEXT NOT NULL,
    sign_count    INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER
);

CREATE TABLE auth_sessions (
    token_hash    TEXT PRIMARY KEY NOT NULL,
    method        TEXT NOT NULL,               -- "passkey" or "link"
    user_agent    TEXT,
    created_at    INTEGER NOT NULL,
    last_seen_at  INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL
);

CREATE TABLE login_links (
    token_hash    TEXT PRIMARY KEY NOT NULL,
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL,
    used_at       INTEGER
);

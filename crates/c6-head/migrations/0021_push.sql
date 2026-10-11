-- Browsers and phones subscribed to web push notifications.
CREATE TABLE push_subscriptions (
    id            TEXT PRIMARY KEY NOT NULL,   -- a hash of the endpoint
    endpoint      TEXT NOT NULL,
    p256dh        TEXT NOT NULL,
    auth          TEXT NOT NULL,
    name          TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    last_sent_at  INTEGER,
    last_error    TEXT
);

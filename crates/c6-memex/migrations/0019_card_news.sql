-- Threads read that a person's or organization's card hasn't taken in yet
-- (docs/relationships.md §4): the walk notes them as it reads, back through
-- time; the card step folds them in, the most recently active first.
CREATE TABLE card_news (
    key        TEXT NOT NULL,      -- person:<address> or org:<domain>
    name       TEXT NOT NULL,
    account    TEXT NOT NULL,
    thread_id  TEXT NOT NULL,
    at         INTEGER NOT NULL,   -- the thread's latest message
    PRIMARY KEY (key, account, thread_id)
);
CREATE INDEX card_news_at ON card_news (at);

-- For trimming idle, fully synced sessions out of the journal.
ALTER TABLE sessions ADD COLUMN last_activity INTEGER NOT NULL DEFAULT 0;

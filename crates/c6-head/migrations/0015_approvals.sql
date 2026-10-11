-- Tool calls waiting for your approval (policy "ask"), per session.
ALTER TABLE sessions ADD COLUMN awaiting INTEGER NOT NULL DEFAULT 0;

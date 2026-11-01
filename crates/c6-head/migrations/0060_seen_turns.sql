-- When a session's last turn ended (or it failed), and when you last saw
-- it after that (docs/work.md §3): a turn end you've seen no longer makes
-- its work need you.
ALTER TABLE sessions ADD COLUMN turn_ended_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN seen_at INTEGER NOT NULL DEFAULT 0;
-- Turns that ended before: when the session last moved, not yet seen.
UPDATE sessions SET turn_ended_at = updated_at WHERE status IN ('idle', 'failed');

-- Background tasks started and not yet ended, so a restarted worker can
-- tell those sessions' agents that their tasks were lost.
ALTER TABLE sessions ADD COLUMN open_tasks INTEGER NOT NULL DEFAULT 0;

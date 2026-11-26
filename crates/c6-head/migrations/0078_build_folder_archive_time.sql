-- Archive is not in the worker event log. Keep the transition time durable so
-- worker build-folder grace periods do not restart on every reconnect. Legacy
-- archived sessions receive a full grace period starting at migration.
ALTER TABLE sessions ADD COLUMN archived_at_ms INTEGER;
UPDATE sessions SET archived_at_ms = CAST(strftime('%s', 'now') AS INTEGER) * 1000 WHERE archived != 0;

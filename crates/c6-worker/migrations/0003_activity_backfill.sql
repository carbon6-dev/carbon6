-- last_activity was never written before this migration, so every synced
-- session looked idle since 1970 and was trimmed at the next start.
UPDATE sessions SET last_activity = MAX(created_at, last_activity);

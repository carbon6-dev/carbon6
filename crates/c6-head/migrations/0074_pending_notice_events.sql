-- Link each pending notice to its latest effective durable snapshot.
ALTER TABLE notice_holds ADD COLUMN event_seq INTEGER;
ALTER TABLE notice_holds ADD COLUMN first_at INTEGER;
ALTER TABLE notice_holds ADD COLUMN first_seq INTEGER;
INSERT OR IGNORE INTO notice_holds_backfill SELECT DISTINCT session_id FROM notice_holds WHERE consumed = 0;

CREATE VIEW active_liveness_losses AS
SELECT session_id, cause, observed_at, id FROM liveness_incidents WHERE kind = 'lost' AND resolved_at IS NULL;

-- One actionable row per session, whether it is held or unavailable (or both).
CREATE VIEW session_notice_attention AS
SELECT s.id, s.title, s.realm, s.notice_waiting_at AS at, 0 AS lost
FROM sessions s WHERE s.archived = 0 AND s.notice_waiting_at IS NOT NULL
AND NOT EXISTS (SELECT 1 FROM active_liveness_losses l WHERE l.session_id = s.id)
UNION ALL
SELECT s.id, s.title, s.realm, l.observed_at, 1
FROM active_liveness_losses l JOIN sessions s ON l.session_id = s.id WHERE s.archived = 0;

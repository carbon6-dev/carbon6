-- Pools (docs/pools.md): a connection of kind `pool` spreads what runs in
-- the head over other connections, each taking some calls at once:
-- [{"connection_id": "...", "slots": 2}, ...].
ALTER TABLE connections ADD COLUMN members TEXT NOT NULL DEFAULT '[]';

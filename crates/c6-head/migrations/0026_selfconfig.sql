-- Self-repair: how many probes in a row failed, and the repair started.
ALTER TABLE trackers ADD COLUMN errors INTEGER NOT NULL DEFAULT 0;
ALTER TABLE trackers ADD COLUMN repair TEXT;

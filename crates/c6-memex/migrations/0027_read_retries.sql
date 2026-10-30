ALTER TABLE enrich ADD COLUMN tries INTEGER NOT NULL DEFAULT 0;
UPDATE enrich SET tries = 1 WHERE error IS NOT NULL;
CREATE INDEX enrich_retry ON enrich (version) WHERE error IS NOT NULL AND tries < 3;

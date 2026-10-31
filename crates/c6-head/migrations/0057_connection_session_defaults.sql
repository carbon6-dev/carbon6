-- What sessions on a connection do by default (docs/architecture.md):
-- the model their subagents run on when no routing rule names one, and
-- the share of the context window at which a conversation is compacted.
ALTER TABLE connections ADD COLUMN subagent_model TEXT;
ALTER TABLE connections ADD COLUMN compact_at INTEGER;

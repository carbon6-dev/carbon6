-- Data that explains itself (docs/apps.md §8): a collection's data card,
-- and each record's provenance and what it was derived from.
ALTER TABLE collections ADD COLUMN card TEXT NOT NULL DEFAULT '{}';
ALTER TABLE records ADD COLUMN derived_from TEXT NOT NULL DEFAULT '[]';
ALTER TABLE records ADD COLUMN provenance TEXT NOT NULL DEFAULT '{}';

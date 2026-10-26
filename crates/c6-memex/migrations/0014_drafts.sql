-- A request's draft reply (docs/brief.md §6): where to answer, the text,
-- what to check; or why it couldn't be drafted. Drafted again when one of
-- its conversations has news since.
ALTER TABLE requests ADD COLUMN draft TEXT;
ALTER TABLE requests ADD COLUMN drafted_at INTEGER;

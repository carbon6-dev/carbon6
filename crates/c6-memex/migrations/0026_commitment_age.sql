-- Age promises by their source, and keep the decisions you make by hand.
ALTER TABLE commitments ADD COLUMN said_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE commitments ADD COLUMN decided_at INTEGER;
UPDATE commitments SET said_at = COALESCE((SELECT created_at FROM items WHERE id = commitments.item_id), created_at);

-- The periodic expiry pass only visits old, undecided pipeline promises.
CREATE INDEX commitments_expiry ON commitments (max(said_at, coalesce(due, said_at)))
WHERE status = 'open' AND made_by LIKE 'understand-%' AND decided_at IS NULL;

-- Failed preflight and explicit retries have terminal rejection receipts,
-- not rewind confirmations. Redelivery must not retry these commands.
ALTER TABLE applied_commands ADD COLUMN rejection TEXT;

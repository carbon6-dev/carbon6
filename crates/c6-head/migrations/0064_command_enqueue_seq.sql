-- Capture the durable event boundary at enqueue, not a wall-clock estimate.
-- Legacy commands conservatively scan from the start, outside the write lock.
ALTER TABLE commands ADD COLUMN enqueued_after_seq INTEGER NOT NULL DEFAULT 0;

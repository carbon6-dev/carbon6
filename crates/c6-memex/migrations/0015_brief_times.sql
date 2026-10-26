-- The brief's timing (docs/brief.md §2.3): which scheduled briefs and
-- close-of-day notes were sent, and when an urgent request told you.
CREATE TABLE brief_sent (
    slot  TEXT PRIMARY KEY NOT NULL,   -- brief:<local date> <HH:MM>, or close:<local date>
    at    INTEGER NOT NULL
);
ALTER TABLE requests ADD COLUMN notified_at INTEGER;

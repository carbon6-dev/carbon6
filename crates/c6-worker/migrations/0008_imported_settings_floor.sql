-- An imported spec already reflects current settings through this event.
-- Zero preserves snapshot replay for locally created and pre-migration rows.
ALTER TABLE sessions ADD COLUMN imported_through INTEGER NOT NULL DEFAULT 0;

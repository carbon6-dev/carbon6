-- A connection can set its context window in tokens; NULL means automatic.
ALTER TABLE connections ADD COLUMN context_window INTEGER;

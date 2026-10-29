-- Only requests triage recognized as just an invitation can close from Calendar.
ALTER TABLE requests ADD COLUMN invitation INTEGER NOT NULL DEFAULT 0;

-- The invitation mark covers only the incoming mail triage read in the source.
ALTER TABLE request_sources ADD COLUMN seen_mail TEXT NOT NULL DEFAULT '[]';

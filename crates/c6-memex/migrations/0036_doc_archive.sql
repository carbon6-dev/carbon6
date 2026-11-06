-- Archiving preserves the document, history, comments, attachments and exports.
ALTER TABLE docs ADD COLUMN archived_at INTEGER;
CREATE INDEX docs_active_updated ON docs (updated_at DESC)
    WHERE deleted_at IS NULL AND archived_at IS NULL;

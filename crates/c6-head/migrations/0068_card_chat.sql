-- Discussions belong to cards, never to the top-level session list.
CREATE TABLE decision_discussions (
    decision_id INTEGER PRIMARY KEY REFERENCES decisions(id) ON DELETE CASCADE,
    running_until INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE decision_discussion_messages (
    id INTEGER PRIMARY KEY,
    decision_id INTEGER NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX decision_discussion_messages_card ON decision_discussion_messages(decision_id, id);
CREATE TABLE decision_revisions (
    id INTEGER PRIMARY KEY,
    decision_id INTEGER NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,
    before_text TEXT NOT NULL,
    after_text TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX decision_revisions_card ON decision_revisions(decision_id, id);

-- Side chats are child sessions (parent_id, no parent_call_id) that branch
-- from an event of the parent.
ALTER TABLE sessions ADD COLUMN fork_seq INTEGER;

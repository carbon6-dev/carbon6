-- Chat messages (kind chat: Slack and the like, docs/slack.md) by their
-- conversation, for a channel's messages and a thread's replies.
CREATE INDEX items_chat ON items (json_extract(meta, '$.workspace'), json_extract(meta, '$.channel'), created_at) WHERE kind = 'chat';

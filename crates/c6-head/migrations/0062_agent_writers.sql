-- Things you let a session (with its side chats and subagents) change,
-- though they aren't its own (docs/agent-actions.md). thing: work:<id>,
-- project:<id>, doc:<id>, record:<collection>/<id>, session:<id>, …;
-- writer: session:<root>.
CREATE TABLE agent_writers (
    realm      TEXT NOT NULL,
    thing      TEXT NOT NULL,
    writer     TEXT NOT NULL,
    granted_at INTEGER NOT NULL,
    PRIMARY KEY (realm, thing, writer)
);

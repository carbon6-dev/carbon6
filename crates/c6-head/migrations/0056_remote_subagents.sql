-- Remote subagents (docs/remote-subagents.md): sessions the head started
-- from a template for another session's Task call, perhaps on another
-- worker. `remote_template` names the template ('' for every other
-- session); the parent and its call are `parent_id` and `parent_call_id`.
ALTER TABLE sessions ADD COLUMN remote_template TEXT NOT NULL DEFAULT '';

-- Inputs sent to a remote subagent for a call of its parent's that waits
-- on the turn taking them (Task or SendMessage): when that turn ends, the
-- head sends the parent how it ended (DelegationEnded).
CREATE TABLE remote_delegations (
    command_id TEXT PRIMARY KEY,
    child_id TEXT NOT NULL,
    parent_id TEXT NOT NULL,
    call_id TEXT NOT NULL,
    -- waiting | reported
    state TEXT NOT NULL DEFAULT 'waiting',
    created_at INTEGER NOT NULL
);
CREATE INDEX remote_delegations_child ON remote_delegations (child_id, state);

-- Triggers (docs/os.md §4.3): automations, generalized. An event (a
-- schedule, a webhook, a memex event, a record, a decision or a session),
-- a filter on it, and an action (start a template or workflow, write a
-- record, ask, notify). Ids, webhook token hashes and run history stay.
ALTER TABLE automations RENAME TO triggers;
ALTER TABLE triggers RENAME COLUMN trigger TO event;
ALTER TABLE triggers ADD COLUMN event_params TEXT NOT NULL DEFAULT '{}';
ALTER TABLE triggers ADD COLUMN filter TEXT NOT NULL DEFAULT '{}';
ALTER TABLE triggers ADD COLUMN action TEXT NOT NULL DEFAULT 'start';
ALTER TABLE triggers ADD COLUMN action_params TEXT NOT NULL DEFAULT '{}';
-- Runs in an hour beyond this are skipped (loop protection).
ALTER TABLE triggers ADD COLUMN per_hour INTEGER NOT NULL DEFAULT 60;

ALTER TABLE automation_runs RENAME TO trigger_runs;
ALTER TABLE trigger_runs RENAME COLUMN automation_id TO trigger_id;
-- The chain of triggers whose actions caused this run (JSON list of ids).
ALTER TABLE trigger_runs ADD COLUMN cause TEXT NOT NULL DEFAULT '[]';

-- A drain belongs to a machine, even while its target build changes.
-- started_at is the first wait; persist the deadline too so changing policy
-- does not misrepresent a deadline already sent to the worker.
ALTER TABLE update_machines ADD COLUMN deadline_ms INTEGER;

-- Old heads left a waiting row for every superseded build. Recover the first
-- wait in the unfinished chain, never one from before a successful update.
WITH waits AS (
    SELECT m.machine, MIN(m.started_at) AS first_wait
    FROM update_machines m
    WHERE m.machine != 'head' AND m.state != 'done' AND m.updated_at > COALESCE(
        (SELECT MAX(done.updated_at) FROM update_machines done
         WHERE done.machine = m.machine AND done.state = 'done'), 0)
    GROUP BY m.machine
)
UPDATE update_machines SET started_at = (SELECT first_wait FROM waits WHERE waits.machine = update_machines.machine)
WHERE state != 'done' AND machine IN (SELECT machine FROM waits)
  AND updated_at > COALESCE(
      (SELECT MAX(done.updated_at) FROM update_machines done
       WHERE done.machine = update_machines.machine AND done.state = 'done'), 0);

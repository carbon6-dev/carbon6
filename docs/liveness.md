# Deterministic session liveness

The head checks work without calling a model. Worker health reports describe
the actual session actor, not just the last durable turn status. A periodic
sweep (30 seconds by default) also checks disconnected workers and silence.
Every wait must either have live work behind it or produce a visible reason.

## Evidence and bounds

Workers report actor state (running, idle, recovering, stopped), its reason,
activity and held-process health over additive worker-link fields. Reports
are connection-scoped: an old connection cannot certify a replacement's work.
Disconnected workers get a short configurable grace; reconnecting within it
does not lose work. Model silence and recovery have separate bounds. A live
tool is not model silence, however long the tool takes. Missing reports from
older workers are unknown, never evidence that their actors died.

The worker sends evidence on its 10-second heartbeat, independently of the
actor mailbox and journal writes. The head fences it by worker connection and
ownership epoch. Tool activity tracks outstanding tool-call ids, so the first
result of parallel calls cannot turn a still-live tool into model silence.
Loss commits are ordered against replacement connections, newer health
reports and progress. The incident and any orphan/goal projection change
commit together, conditional on the observed owner and journal sequence.
Queuing a parent report rechecks that generation in the same transaction
that retires its waiters.
One nonblocking mailbox probe per actor distinguishes an open mailbox from a
responsive actor: an unanswered probe after 30 seconds reports unresponsive.
The probe never waits in the heartbeat path; recovery has its own state and
bound rather than inheriting a stale probe deadline.
Heartbeats themselves are not progress. Durable events and streamed activity
are progress; repeated recovery diagnostics do not restart the recovery bound.

The realm's subagent settings expose these bounds (in seconds):

| Setting | Default |
| --- | ---: |
| `liveness_sweep_seconds` | 30 |
| `liveness_worker_grace_seconds` | 30 |
| `liveness_model_silence_seconds` | 600 |
| `session_recovery_seconds` (existing worker recovery bound) | 180 |
| `liveness_tree_silence_seconds` | 1800 |

Reports and worker-link changes also trigger sweeps. Thus disconnection and
affirmative actor death normally become visible within a minute; model and
recovery waits use their separate configured bounds. Older workers without
actor evidence still get worker-disconnection monitoring, but cannot prove
actor death while connected. Upgrade those workers for actor-level coverage.

The head records a loss with its actual cause and the observation time. A
running turn with an explicitly stopped/idle actor is ended visibly rather
than left running forever. It never fabricates an event sequence on the
worker's journal. Recovery and process cancellation remain the worker's
responsibility; declaring a loss is not authorization to launch a duplicate.
An orphan's head projection becomes idle with an error, not startup-failed, so
SendMessage can resume it. Active goals move to `needs_you` under their normal
error policy. Silence alone never cancels an in-flight model request.

## Parents and notices

A lost child yields a failure notice through the existing durable notice path.
The notice carries its session id, last message and progress, and the editable
prompt tells the parent to continue it with SendMessage or restart, not wait.
Failures bypass merge/group delays. Existing spending and no-progress guards
remain authoritative: a guarded failure must notify the user, not disappear.
Repeated losses must escalate to the user instead of authorizing infinite
relaunch. Continuation starts a new observation generation; stale reports must
not immediately lose a newly resumed child.

Incidents and report command ids are durable and idempotent across sweeps and
head restarts. A loss also sends a user notification, even when the parent's
notice wake is guarded or offline. The second consecutive loss of the same
normalized task description pauses unattended notice wakes; multiple waiters
on one incident count only once. Success or direct user input clears that
streak. Existing notice token and no-progress guards remain in force.

Before enqueueing a local delegated input, the parent journals its exact
wait obligation. The head can therefore identify the waiting call even if
the child's actor wedges before journaling that input. Normal foreground
results and terminal background callbacks retire their respective obligations.
A marker that syncs after an earlier loss starts a new observation requiring
fresh evidence, rather than leaving its waiter behind an already-delivered
incident.

A child whose turn ended with background work still running is waiting on
that work, not finished. Its parent must retain the outstanding dependency.
Before a delegated foreground turn journals its end, an actor fence journals
the background callbacks already submitted by its tools. Thus the head cannot
mistake a delayed task-start callback for quiescence. Local settlement also
checks live counted work, and both local and remote reports wait for an idle
foreground and the final terminal background event. Uncounted servers and
watchers do not hold a delegation open.
The sweep covers background work as well as foreground actors. A parent with
pending descendants and no activity anywhere in its tree for 30 minutes gets
a pending-work summary, with ids and since times, even if everything is alive.

## Visibility and tests

The read-only liveness operation lists evidence and current losses. Activity
includes recent incidents; session/subagent views retain the loss cause until
work resumes. Liveness settings are durable realm settings, with defaults for
older saved settings. Agent-facing prose lives in editable prompts.
`GET /api/v1/liveness` (`system.liveness` in the operation catalog) returns
current evidence and up to 200 recent incidents, restricted to the caller's
realm. Real journal progress or a newly registered local continuation resolves
an earlier loss; metadata edits and heartbeats cannot erase it. The editable
prompts are `session.liveness_lost`, `session.liveness_silence`, and
`session.liveness_notification`.

Tests invoke a sweep with an explicit observation time, or advance a paused
clock; no assertion waits for real grace/model/silence windows. Integration
tests use the Env harness and inject actor death, disconnected workers and
gated model responses. Live tools and grace-period reconnects are negative
controls. Assertions wait for observable conditions rather than sleeping.

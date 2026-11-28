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
does not lose work. Model silence and non-resource recovery have separate bounds. A live
tool is not model silence, however long the tool takes. Missing reports from
older workers are unknown, never evidence that their actors died.

### Head and update reconnects

A head restart is not a worker restart. Active worker actors, model streams,
tools and the journal keep running; no turn or completed tool is replayed.
Before this head has seen a worker attach, liveness gives its old sessions a
two-minute startup reconnect window in addition to the ordinary disconnect
test. This covers an unannounced head restart and a roughly 30-second outage.
After a successful attachment the normal configurable worker grace applies.

A head update persists a five-minute restart expectation **before installing**
(a package installer may restart the service itself), and sends an additive
`RestartExpected` advisory to attached workers. The replacement head reads the
persisted expectation. A worker update's acknowledged `installing`/`restarting`
status is likewise a planned disconnect; its latest persisted update status
gives it up to five minutes to reattach. `done`, `failed` and rollback supersede
the expectation. Draining/downloading alone do not postpone loss detection.
The windows are bounded: an expected reconnect is not proof that a disconnected
process is still alive. A worker that never returns is still reported lost;
affirmative stopped-actor evidence on a live connection is never hidden by an
update marker. No parent failure report is manufactured during these windows.

Model credential transport has its own reconnect-aware wait: two minutes
normally, up to five when a head update was announced. Backoff grows from one
to thirty seconds and a new attachment wakes it immediately. A 15-second
unanswered request is retried, including on the same healthy stream, rather
than failing the model turn. Each attempt has a fresh request id; cancellation,
timeout and disconnect remove its old waiter. Explicit authorization refusals
still fail promptly. Valid cached grants bridge an outage; reattachment still
invalidates session model grants so current authorization is checked again.

Credentials are control traffic, independent of ordered journal ingestion,
initial spec refresh and command replay. A session committed just before
shutdown can authorize from its durable row immediately after reconnect;
there is no new-stream session-registration prerequisite. Local subagents ask
as their root session, so credential logs under the root id do **not** mean the
child's request was unanswered. Grant logs include the requested session,
authorization root and request id for correlation.

### Actor evidence

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
Held Claude Code adoption uses the same recovery budget: queued input stays
unsent while ownership is uncertain, the first failure is recorded visibly,
and bounded retries do not need another mailbox event. Exhaustion exposes the
existing Retry/Restart recovery controls without bypassing ownership checks.

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
recovery waits and the startup/planned-restart exceptions above use their
separate bounds. Older workers without
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
Failures bypass merge/group delays, not the adaptive repetition guard
(docs/runaway-guards.md). A lost-child notification remains visible even when
its parent's wake is held. Repeated equivalent failures first warn the agent,
then back off, and only persistent repetition pauses for the user.
Continuation starts a new observation generation; stale reports must
not immediately lose a newly resumed child.

Incidents and report command ids are durable and idempotent across sweeps and
head restarts. A loss also sends a user notification, even when the parent's
notice wake is guarded or offline. Repeated losses of the same normalized task
description and error contribute evidence to the adaptive loop guard; multiple
waiters on one incident count only once. Success, genuinely new reports or direct
user input clear the streak. There is no default spending pause or fixed
second-loss stop; an explicitly enabled hourly spending cap still applies.

Before enqueueing a local delegated input, the parent journals its exact
wait obligation. The head can therefore identify the waiting call even if
the child's actor wedges before journaling that input. Normal foreground
results and terminal background callbacks retire their respective obligations.
The child's durable local-report receipt also retires the exact consumed input
obligations covered by that report. This includes SendMessage continuations
and reports after the child's own background work wakes it again, even if the
parent's original detached callback is no longer available. Report-window
membership comes from the worker's shared replay logic, not idle/completed
status, reused tool-call ids or the latest report's sequence alone. An unrelated
earlier report and a queued later input remain pending. Reconciliation is
transactional in either journal replication order and repairs saved outstanding
obligations when the head opens its database.
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
pending descendants gets a pending-work summary, with ids and since times,
only after 30 minutes with no recorded progress and no live tool or counted
background work anywhere in its tree (including the parent and idle
intermediates). Worker evidence for this exemption must match the live worker
connection and ownership epoch and follow the latest progress. A tool call
or notifying background process can run longer than the silence window without
producing a false alarm; stale evidence, exited processes and stopped actors
cannot exempt the tree. Heartbeats alone still do not reset the progress clock.
The worker separately reports counted background processes and tools waiting
only on subagents: a parent's Task/SendMessage dependency is not itself live
process work and cannot mask a silent child. The existing total background-task
count remains available for actor-loss checks. These distinctions use additive
worker-link fields; upgrade workers for background-process tree coverage.

## Visibility and tests

Disk exhaustion is an automatic resource wait, not a stuck or lost session.
The worker reports `waiting_for_disk` with the state filesystem's mount and
available bytes, independently of the failing journal. It retries beyond the
normal 180-second recovery budget, with a 5-second initial delay doubling to
5 minutes, and samples free space once a second to wake sooner when 16 MiB
returns. Corruption (`SQLITE_CORRUPT`, `SQLITE_NOTADB`, failed integrity check)
still escalates through the bounded Retry/Restart lane. User notifications and
editable parent notices distinguish the automatic wait from a terminal stop.

Early warnings sample the state directory, worker worktrees and temporary
directory filesystems on the heartbeat tick. Each filesystem warns once below
the larger of 10% free and 20 GiB; a healthy sample clears and rearms it.
`disk_warning_percent` and `disk_warning_bytes` configure the threshold;
`disk_recovery_initial_seconds` and `disk_recovery_max_seconds` configure the
retry cap. New sessions carry these settings and local children inherit them.
The warning uses the existing `failed` notification event until the general
machine notification event is available. Neither sampling nor warnings writes
to the worker journal.

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
Tree regressions advance the observation past the silence window with a live
tool or counted background process, then remove that evidence and require a
single silence notice. Local receipt tests cover background continuations,
self-woken completion, independent out-of-order reports, reused call ids,
either journal replication order, same-batch ingestion, and restart repair.

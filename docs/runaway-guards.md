# Runaway guards — design

Carbon6 should let useful unattended work run. A burst of report-driven turns,
a long-running task, or a fast model is not by itself a runaway. Use separate
signals for repeated behavior, plan consumption, and worker resource pressure;
none is a substitute for the others. See [notice scheduling](notices.md),
[subagents](subagents.md), and [plan limits](analytics.md#4-plan-limits).

## Repetition: warn, slow, then pause

Use durable, effective journal events to evaluate completed unattended wakes.
Collect deterministic evidence: no new observable result over successive turns;
near-identical reports or repeated peer replies; or relaunching equivalent work
and getting the same failure. Normalize incidental wrapper text, timestamps and
IDs; do not treat distinct waiter deliveries of a structured loss as separate
failures. Do not use number of tokens, elapsed time, or a model verdict as proof
of a loop. A new report, task description, or observed file change is evidence
of progress; a checkpoint commit ID alone is not proof of a changed file.
Checkpoints retain their already-computed tree identity. Before imposing a
repetition warning, backoff or pause on a local workspace, the worker also probes its
current tree and journals any changed fingerprint. This catches shell writes
on the deciding turn without scanning files on every normal wake. The probe
uses the private checkpoint index, not the user's Git index; remote SSH files
are not fingerprinted.
Direct user input and an explicit Continue reset the repetition chain. Feeding
the same report back or relaunching the same failed task does not reset it.

1. **Warn the agent** once the applicable repeated-evidence threshold is met.
   Include the reason and give the agent a chance to change course. Use the
   existing editable prompt for the warning, not a hardcoded agent instruction.
2. **Back off** on continued repetition. The successive retry delays are 1, 2,
   4, … minutes, capped at `notice_loop_backoff_max_seconds` (default **900**).
   Wait automatically; do not drop the notice or mark the session as needing
   human attention solely because it is delayed. Live progress resets the chain.
3. **Pause** only if evidence persists after the configured
   `notice_loop_backoff_rounds` (default **3**). Show the evidence and notify the
   user; leave pending notices intact. Continue or direct input can resume.

The worker evaluates before the next automatic delivery; direct input and an
already-running turn are not preempted. Normal batching and fan-in are separate
from backoff. Failures bypass fan-in but not runaway guards. A native runtime
can act on some of its own background reports before the worker controls the
next wake, so this is not continuous supervision of external runtimes. Timing
and evidence need to be reconstructible from the journal, including across
restarts, rather than dependent on process lifetime. For live configuration,
the head sends `UpdateNoticeLimits` on edits and reconnect to workers that
advertise `notice_settings_v1`; older workers retain their previous settings.

## Consumption: awareness, not an automatic spending cap

There is **no fixed default token or dollar spending cap** on unattended work.
An explicitly enabled optional hourly notice token cap and separately
configured spending budgets are independent controls. The forecast below is
informational: no automatic pause, backoff, routing change or model message.

Plan quotas are reported as utilization percentages and reset dates, not an
absolute token or dollar allowance. The head checks committed usage against
fresh same-window provider observations. With a rise of at least one percentage
point over 5–30 minutes and at least two metered calls, estimate percentage
points per recorded token across all local sessions on the same connection and
model; then apply only the target session's recent unattended token rate. The
calculation includes recorded cache reads/writes. The head compares the projected
time to exhaustion to the time left until reset. If likely to exhaust sooner,
notify at low priority through the existing `quota` notification channels:
“~X% of the window an hour; resets in Y”. Pick the earliest projected exhaustion
if more than one window qualifies. Atomically claim the notice once per session,
including across head restarts and settings edits; this is a best-effort send,
not a guarantee that a configured channel delivered it.

The estimate is intentionally conservative about attribution. It does not use
saved display-only observations as prior samples, expired or stale windows,
model-specific sublimits, smaller noisy changes, or calibration intervals with
multiple models. It runs after usage ingestion,
not on a wall-clock timer; sparse telemetry, missing worker headers or delayed
provider reports can prevent a forecast. Unknown activity elsewhere on the same
plan can still affect utilization. A percentage rate is not a bill, a provider
quota promise, or proof of a runaway. Existing 80%/95%/exhausted quota
notifications are unaffected.

## Coordination: hop ceiling and real worker pressure

Do not rate-limit ordinary session messages by default. Useful cooperating
sessions can exchange many messages; repeated back-and-forth without progress
is governed by the evidence policy. Keep the **50-hop** ceiling as a last resort.
Use the default maximum subagent depth of **5** and default `max_running` of
**0** (no count cap). An explicitly configured running count limit still applies.

Independently, check the executing worker's available memory and system load
before starting or continuing a subagent. If measured headroom is insufficient,
show that the work is queued because the worker is busy, and retry automatically
when headroom returns. Waiting must remain cancellable. A local pressure signal
does not predict resources used by future tools or measure a remote SSH host;
it is not an out-of-memory guarantee or a reason to interrupt useful running
work. A busy worker is not evidence of a runaway session.

## Verification

Exercise same-failure retries, alternating messages, novel report/file/task
progress, reset, and suppression of duplicate loss evidence. Use a paused or
injected clock for batching, exponential backoff and quota projections. Mock
quota data should distinguish no data, reset rollover, stale/noisy measurements,
attendance, and forecast exhaustion before versus at/after reset. Verify
once-only claiming across replay/concurrency and database reopen. Resource
pressure tests should use injected samples rather than stress the machine.

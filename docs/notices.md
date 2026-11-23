# Notice scheduling

Notices are durable inputs, not user messages. Background completions, subagent
reports and session messages join a running turn at its next step. An idle
session merges them after **3 seconds of quiet**, with a **30 second maximum**
from the first arrival. Both windows are configurable in Agents → Limits.
Delivery is event-driven; tests advance Tokio's paused clock or use Deliver now,
never sleep to assert that a window elapsed.

## Fan-in

Task accepts `group` and `wake` (`all`, the default, `count:N`, or `any`). A group
is scoped to its parent session and consists of the tasks already launched with
that name when delivery is considered. Use a fresh name for each fan-out. A
success waits for the threshold; failures and lost children bypass the group's
threshold and merge delay. A delivery includes the finished members and a short
group summary. A running turn still receives reports at its next step.
If a `count:N` threshold exceeds the group's size, finishing every known member
also releases it; the summary explains the unmet threshold instead of waiting
for members that do not exist.

## Expected wakes and runaway detection

A user message roots an expected-work chain. Tasks launched by that turn, and
by notice turns responding to its descendants, remain expected transitively.
Neither expected nor unexpected work is subject to a fixed wake count. A busy
fan-out with distinct reports can keep waking its parent without tripping the
runaway guard. Session messages are not proof of user intent, and a high hop
ceiling remains as a last resort.

The guard looks for repeated evidence without progress: for example, effectively
the same notice over and over, or relaunching the same task and getting the same
failure. Different task IDs or wrapper timestamps do not make the same failure
new. Genuine progress, such as a distinct report or task, breaks the repetition;
a checkpoint commit ID alone does not prove a file changed.
The worker retains checkpoint tree identities and checks the current local tree
before a repetition intervention, so a shell write on the deciding turn can clear it.
Remote SSH workspaces do not have this local file-progress signal.

1. **Warn the agent** about the repetition and give it a chance to change course.
2. **Slow automatic delivery** if repetition continues. Backoff grows over
   successive rounds. Nothing is dropped; no user action is needed to wake again.
3. **Pause and notify you** only when repetition persists through backoff. The
   session shows the evidence. Continue or a direct message resets the guard.

Agents → Limits controls the repeated-evidence warning thresholds and the
backoff rounds and maximum delay. Group failures bypass batching, not the guard.
The liveness monitor ([liveness](liveness.md)) still notifies you of an actual
work loss independently of notice delivery. Multiple waiters or deliveries for
one incident are not evidence of multiple failures.

## Spending

Only turns started entirely by notices contribute to a rolling hour's spending.
There is **no token cap by default**. If you enable one in Agents → Limits, it
uses recorded input plus output tokens, including cache accounting as recorded. A turn
started by a direct user input never counts, even if notices join it. The guard
is evaluated before the next carbon6-scheduled wake, so a single turn may
cross the budget. Claude Code can start a turn on its own task notifications
before the carbon6 scheduler can intervene. When those notifications are
recorded as the inputs starting the turn, its usage counts toward the next
carbon6-scheduled wake and the notifications do not reset the progress chain.
Historical usage and notice provenance are reconstructed from the log.
Continue acknowledges earlier spending and authorizes another stretch of
unattended work. Raise the limit opens Agents → Limits. Set the cap to 0 to turn
it off.
The old `auto_turns_per_hour` JSON property is accepted but
ignored, and its wire tag is never reused.

## Settings changes on running sessions

Settings edits take effect at the next notice delivery decision and survive
session reload. The head pushes updates to running sessions on workers that
advertise `notice_settings_v1`; it also refreshes those workers on reconnect.
Older workers do not receive the live update and keep their existing behavior
until upgraded.

## Visibility and recovery

NoticeQueued records contain a reason. The head sends an explicit `hold` kind
alongside that display reason, including for older journals. Clients do not
infer attention from wording. The shared `NoticeHoldReason` classification
distinguishes automatic waiting from waiting for the user:

* **Proceeding by itself:** collecting a burst of reports, or waiting for a
  group the agent requested (`all` or `count:N`). This is a quiet neutral status,
  not an attention alert. For example, “Collecting reports; delivering
  automatically in a few seconds”, or “The agent is waiting for its group
  da-audit: 4 of 5 finished; waiting for 1”, naming pending task descriptions
  and explaining “The agent wakes automatically; no action needed.” “Deliver
  now anyway” remains a secondary link for optional early delivery. These holds count
  as working, not as needing the user, in session lists, Activity and work.
* **Slowed:** the guard found repetition and is delaying the next automatic
  wake. This is an automatic wait, not an attention alert.
* **Needs you:** paused by the runaway guard or the optional spending cap, stuck
  recovery, non-waking notices waiting for your next message, or an unknown
  hold that cannot promise automatic progress. This uses amber attention styling
  and the reason, with Continue, Raise limit, Retry or Deliver now as appropriate.
  For example, “Paused · Needs you — 2 notices held” with “spending limit”. A guard pause
  also emits a user notification; there are no silent guard holds.

Live recovery stops and unresolved work losses override an older automatic
snapshot: the UI suppresses its automatic-wake claim and early-delivery control
until the stop is resolved, showing the existing recovery or loss controls instead.

Failures and lost group members release the barrier immediately through the
existing liveness failure-report path, unless a guard requires your attention;
they never silently wait for an impossible group threshold. Your next message
delivers all held notices together, regardless of group or batching conditions.

A repeated NoticeQueued with the same command id is a metadata update (for
example, a changed group count or guard reason), not another input. Clients
retain first-queue ordering/grace and the latest reason. The eventual User
event consumes that command id once, so reports appear once and are delivered
once even if the hold reason changed several times.
The session stream also sends an authoritative `notices` snapshot, independently
of its transcript window. Old pending reports therefore remain visible on a
fresh tail load; consumption and rewinds replace the snapshot rather than
leaving stale holds in the pill.

Queued inputs remain in the session log until a User event consumes them.
Worker restart and actor supervision reconstruct held inputs, group membership,
usage and guard state from effective events. Recovery may restart a short merge
window, but never loses an input or counts it twice. New protocol fields use new
tags, and old logs/clients remain readable.

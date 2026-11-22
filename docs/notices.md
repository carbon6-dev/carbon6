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

## Expected wakes and progress

A user message roots an expected-work chain. Tasks launched by that turn, and
by notice turns responding to its descendants, remain expected transitively.
Expected work is not subject to an arbitrary wake count. Session messages are
not proof of user intent, and existing session-message hop limits still apply.
Expected work is nevertheless subject to the same progress and spending guards:
being asked to do work does not authorize an infinite retry loop.

A chain is consecutive notice-only turns since the last direct user input.
Distinct started task descriptions, changed checkpoint files (when available),
and reports different from the preceding report are cheap progress signals.
The current checkpoint journal stores commit IDs, not changed paths or tree
hashes, so commits alone are deliberately not treated as file progress; an
unchanged snapshot can have a new commit ID. No extra git scan is performed.
Repeated failure reports from the same task description are not progress merely
because task IDs or wrapper timestamps differ. Relaunching a failing description
more than **3 times** pauses further unattended wakes. Otherwise **5 consecutive
wakes without progress** pause them. Any direct message or Deliver now continues
the session and resets the chain. Group failures bypass batching, not guards.

The deterministic liveness monitor (docs/liveness.md) also supplies lost-child
reports. A **second consecutive loss** for the same description pauses further
unattended wakes and tells the user through this same guard, rather than
starting another automatic relaunch. Multiple deliveries or waiters for one
incident count once. Successful completion or direct user input resets the
loss streak. A liveness loss also notifies the user independently, so a worker
disconnect or an existing spending hold cannot conceal it.

## Spending

Only turns started entirely by notices contribute to a rolling hour's spending.
The default budget is **5,000,000 tokens** (input plus output, using recorded
usage; cache accounting follows the existing usage representation). A turn
started by a direct user input never counts, even if notices join it. The guard
is evaluated before the next carbon6-scheduled wake, so a single turn may
cross the budget. Claude Code can start a turn on its own task notifications
before the carbon6 scheduler can intervene. When those notifications are
recorded as the inputs starting the turn, its usage counts toward the next
carbon6-scheduled wake and the notifications do not reset the progress chain.
Historical usage and notice provenance are reconstructed from the log.
Continue acknowledges earlier spending and authorizes another stretch of
unattended work. Raise the limit opens Agents → Limits; save there, then Continue
applies those settings to the paused session and records them for recovery.
The old `auto_turns_per_hour` JSON property is accepted but
ignored, and its wire tag is never reused.

## Visibility and recovery

NoticeQueued records contain a reason. The head sends an explicit `hold` kind
(`collecting`, `group`, or `needs_you`) alongside that display reason, including
for older journals. Clients do not infer attention from wording. The shared
`NoticeHoldReason` classification distinguishes two kinds of waiting:

* **Proceeding by itself:** collecting a burst of reports, or waiting for a
  group the agent requested (`all` or `count:N`). This is a quiet neutral status,
  not an attention alert. For example, “Collecting reports; delivering
  automatically in a few seconds”, or “The agent is waiting for its group
  da-audit: 4 of 5 finished; waiting for 1”, naming pending task descriptions
  and explaining “The agent wakes automatically; no action needed.” “Deliver
  now anyway” remains a secondary link for optional early delivery. These holds count
  as working, not as needing the user, in session lists, Activity and work.
* **Needs you:** paused by no-progress, relaunch or spending guards, stuck
  recovery, non-waking notices waiting for your next message, or an unknown
  hold that cannot promise automatic progress. This uses amber attention styling
  and the reason, with Continue, Raise limit, Retry or Deliver now as appropriate.
  For example, “Needs you — 2 notices held” with “spending limit”. A guard pause
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

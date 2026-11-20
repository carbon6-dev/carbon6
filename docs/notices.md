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

## Spending

Only turns started entirely by notices contribute to a rolling hour's spending.
The default budget is **5,000,000 tokens** (input plus output, using recorded
usage; cache accounting follows the existing usage representation). A turn
started by a direct user input never counts, even if notices join it. Spending
is evaluated before the next unattended turn, so a single turn may cross the
budget. Historical usage and notice provenance are reconstructed from the log.
Continue acknowledges earlier spending and authorizes another stretch of
unattended work. Raise the limit opens Agents → Limits; save there, then Continue
applies those settings to the paused session and records them for recovery.
The old `auto_turns_per_hour` JSON property is accepted but
ignored, and its wire tag is never reused.

## Visibility and recovery

NoticeQueued records contain a reason: merging reports, waiting for a group's
threshold, or paused for progress/spending. The waiting-notices pill retains
Deliver now (Continue when paused). A pause also emits a user notification;
there are no silent guard holds. Your next message delivers all held notices
together, regardless of group or batching conditions.

Queued inputs remain in the session log until a User event consumes them.
Worker restart and actor supervision reconstruct held inputs, group membership,
usage and guard state from effective events. Recovery may restart a short merge
window, but never loses an input or counts it twice. New protocol fields use new
tags, and old logs/clients remain readable.

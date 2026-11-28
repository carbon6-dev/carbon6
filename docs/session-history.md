# Session history: your prompts

The session's history is a deterministic index of its input messages, not an
agent-written summary. It makes no model calls. It supersedes the disabled
turn outline in [catchup.md §3](catchup.md#3-timeline); catch-up cards and the
existing jump/loading/flash mechanism remain unchanged.

## Reading and moving

A thin ruler at the transcript's right edge maps elapsed wall-clock time to
height (oldest at the top). Marks identify prompts; busy days collapse into
density marks at small scales. Day ticks, and hour ticks for today, preserve
quiet gaps. Hover/focus previews the nearest prompt, its timestamp and its
turn duration. Click/drag jumps using the transcript's existing reach and flash
path, including events outside the loaded window. Arrow keys select adjacent
prompts; Page Up/Down select a day earlier/later. The ruler is keyboard focusable
and labelled. On phones its narrow strip opens a bottom sheet on tap/long press.

The glance contains **Your prompts**, newest first, grouped by local calendar
day (Today, Yesterday, otherwise weekday/month/day). Search filters only input
text. Rows show time, first line, provenance, Jump to it, and the shared Copy
control (copies the complete original prompt). Up next rows represent accepted
input not yet recorded by the worker and cannot jump until recorded. A checkbox,
off by default, shows the first sentence of the turn's last text reply. It is
extracted, never generated. The same list appears in the phone sheet.

The ruler is an absolutely positioned sibling of the transcript, not part of
its scrolling contents. Refreshing the index must never change scroll height,
scroll position, or the transcript's owner/window. Prompt and day keys remain
stable across refreshes. Lists are bounded on screen, with explicit Show more;
the ruler draws a bounded number of density buckets rather than thousands of
DOM nodes.

## Data contract

`GET /api/v1/sessions/{id}/prompts` (`sessions.prompts`, read-only) has the same
realm visibility checks as session reads. `since` is inclusive, `until`
exclusive, in Unix milliseconds; `after_ts` and `after_seq` form a stable
exclusive cursor for equal timestamps. `limit` defaults to 500, capped at 1000.
Results are chronological by `(ts, seq)`, with a continuation cursor and the
whole history's bounds. Queued inputs are returned separately on the first
page (without fabricated event sequence numbers).

Each recorded User event appears once: seq, timestamp, complete text, first
line, origin (`user`, `session`, or `follow_up`), source session/title, and its
turn's start/end/elapsed milliseconds. A follow-up is input recorded while a
turn runs. Null end/duration means running or not yet assigned, not zero.
Messages blocked before a turn remain visible without an invented duration.
Background/system events are not fabricated as user messages. Legacy User
events remain readable. Rewound-away events are excluded, like the transcript.

The head incrementally indexes persisted events in bounded event pages, caching
the index per session alongside the existing timeline cache. Rewinds rebuild
from effective events. No transcript scan or model call is done per hover,
filter, drag, or jump. A browser refresh updates index data only; navigation is
an explicit user action. Dates use the browser's timezone, including DST.

## Verification

Deterministic head tests cover exact inputs, origins, shared-turn timing,
equal-time cursor boundaries, ranges, rewind invalidation and realm denial.
UI unit tests cover local day/hour grouping, elapsed-time mapping, keyboard
selection and filtering. The registered private-fixture browser suite covers
desktop hover/jump/flash, refresh scroll stability, and the 390px phone sheet
in Chromium and WebKit. Implementing agents write but do not run builds/tests;
the integration agent owns execution. No live head/workers or paid evals.

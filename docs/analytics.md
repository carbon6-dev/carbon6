# Analytics — Design

Status: built (2026-10-23). A first step toward docs/measurement.md: the measures carbon6 can take from its own logs, with nothing asked of you. Outcomes, value and leverage are still deferred.

## 1. What it shows

**Analytics** in the header opens the page in a tile. For the last day, 7, 30 or 90 days, and only for sessions with the label filter's labels, it shows:

- **Agent time:** from each finished turn's start to its end, summed over sessions and subagents. It splits into:
  - **model time:** waiting on model responses, measured from the event before each call to its usage;
  - **tool time:** tools' own durations (parallel calls each count);
  - **other:** everything else in the turn (hooks, approvals, the worker's own work).
- **Agents at once:** the most turns running at the same moment on a day.
- **You:**
  - your messages, split into **new tasks** (a session's first message) and **corrections** (later messages, interrupts and rewinds);
  - **answers** to approvals and questions;
  - **agent time per message of yours,** a rough measure of how far each message carries.
- **Waiting on you:** how long a finished turn waited for your reply, and an approval or question for your answer (median and 90th percentile). This is what to shorten to get more done.
- **Cost and tokens,** from the usage table at today's prices (Settings → Prices).
- **By model:** time, calls, time per call, output tokens per second of model time, tokens, cost.
- **By tool:** time, calls, median duration, share failed.
- **By a label key you pick** (`repo` at first; any key, like `client`): agent time, sessions, turns, your messages, corrections per task, cost.

Each day chart has a hover readout per day and a table.

## 2. How

- **Marks.** As each event is stored, the head writes a timing mark to the `activity` table for events that say who was working: your message, turn start and end (with its reason), model call (usage, with the model), tool result (tool, its duration, failed), approval and question states, rewind. Events stored before the table existed are marked once at startup (`activity_backfilled`).
- **The report** (`GET /api/v1/analytics?days=&labels=&by=`) walks the marks of the range per session in order. It also reads a day before the range, for turns already under way. Then it adds the `usage` table's calls. Model and tool time count when their turn ends, like agent time, so a running turn doesn't count yet.
- **Not measured:** your time in carbon6 or elsewhere (presence, docs/measurement.md §7), and whether the work was kept (outcomes). "Waiting on you" includes nights and weekends: read the median, not the sum.

## 3. API

`GET /api/v1/analytics` (operation `usage.analytics`) returns `AnalyticsReport`:

- `total`, `by_day` (every day of the range), `by_model`, `by_group` (values of the label key `by`; `""`: sessions without it): rows of `ActivityRow`;
- `by_tool`;
- `waits`;
- `unpriced_models`.

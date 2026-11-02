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

## 4. Plan limits

Subscriptions have usage windows rather than prices: a Claude Code subscription (5 hours, the week, a model's week, overage) and a ChatGPT plan (a short window and the week). carbon6 shows how much of each is used and when it starts afresh:

- **Where:** under each such connection in Settings → Connections (a bar per window: "5h: 34% used · resets in 2h 10m"), in **Plan limits** on the Usage page, and wherever you pick a connection (the reply box's and a side chat's model picker, the new-session form): each option reads "name — week 29% left · renews Thu", and the chosen one shows the same beside the picker, amber from 80% used (or when the provider warns), red when used up.
- **Claude Code** reports its subscription's windows itself as its sessions run (`rate_limit_event` in its stream: `unifiedWindows` with each window's utilization, reset time and status). The worker passes each report to the head (`EphemeralEvent.rate_limit`), which keeps it for the session's connection. It costs nothing and is as fresh as the last turn on that connection; a report about one window keeps the others.
- **ChatGPT plans** are read by the head with its own sign-in from the usage endpoint Codex reads for its `/status` (`https://chatgpt.com/backend-api/wham/usage`, with the workspace as `chatgpt-account-id`): `primary_window` and `secondary_window`, each with `used_percent`, `limit_window_seconds` and `reset_after_seconds` or `resets_at`, wherever they are in the answer. Every 3 minutes, and soon after a turn ends on one when the last read is over a minute old. A failed read keeps the last numbers and says why. The head setting `chatgpt_usage_url` points it at a stand-in or a proxy.
- **API keys and local servers** have no plan limits: their spend is the Usage page's, against budgets.
- **Notifications** (event `quota`, on by default): when a window passes 80% and 95%, and when it is used up, once per window and level until the window starts afresh.
- **API:** `GET /api/v1/connections/quotas` (`connections.quotas`): `ConnectionQuota` per connection (`source`, `windows` of `QuotaWindow`: `name`, `used_percent`, `resets_at`, `status` allowed / warning / rejected; `updated_at`; `error`). Kept in the head's memory: after a restart they come back with the next turn or read.
- **Not yet verified:** the ChatGPT endpoint was built from Codex's own field names and tested against a stand-in; carbon6's ChatGPT sign-in may not be accepted there, in which case the connection shows the read's error and no windows. Claude Code's report was checked against Claude Code 2.1.314.

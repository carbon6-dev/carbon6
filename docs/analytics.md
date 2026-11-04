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

- **Where:** in the message box, the model chip ends with the tightest window's share left ("opus · high · 90%"; its hover: "Week: 90% left, resets Sun Nov 8, 9:00 am"), and its menu starts with a **Plan** section: a row per window with a bar of what's left, "N% left", and when it resets in your local time ("today 3:40 pm", "tomorrow 9:00 am", "Sun Nov 8 · 9:00 am"). Under each such connection in Settings → Connections (a bar per window: "5-hour: 34% used · resets today 3:40 pm"), in **Plan limits** on the Usage page, and wherever you pick a connection (a side chat's model picker, the new-session form): each option reads "name · 90% left", and the chosen one shows "90% left" beside the picker. The colour goes by what's left: grey over 50%, yellow from 50%, amber from 20% (or when the provider warns), red under 5% (or when used up).
- **Claude Code** reports its subscription's windows itself as its sessions run (`rate_limit_event` in its stream: `unifiedWindows` with each window's utilization, reset time and status). The worker passes each report to the head (`EphemeralEvent.rate_limit`), which keeps it for the session's connection. It costs nothing and is as fresh as the last turn on that connection; a report about one window keeps the others.
- **ChatGPT plans** come from rate-limit headers on model replies to the head (suggestions, session names, catch-up, Ask, and ChatGPT members of pools), using the same OAuth sign-in as the model call. The headers are `x-codex-primary-used-percent`, `x-codex-primary-window-minutes`, `x-codex-primary-reset-at` and the corresponding `secondary` headers: percentages used, window lengths in **minutes**, resets as **Unix seconds**. These names and units follow Codex's `codex-rs/codex-api/src/rate_limits.rs` and `codex-rs/protocol/src/protocol.rs` (`RateLimitWindow`). The head names windows by length (e.g. 300 minutes → `5h`, 10080 → `week`), merges partial reports and clears any earlier read error. No extra request is needed. The old usage endpoint (`https://chatgpt.com/backend-api/wham/usage`; head setting `chatgpt_usage_url` for a stand-in or proxy) remains a fallback every 3 minutes and after stale turns, only until headers arrive or it answers `rejected_by_access_enforcement`: carbon6's OAuth app cannot read that endpoint. That refusal is logged once per connection per head run, stops further polling and is not a plan-limit error.
- **API keys and local servers** have no plan limits: their spend is the Usage page's, against budgets.
- **Notifications** (event `quota`, on by default): when a window passes 80% and 95%, and when it is used up, once per window and level until the window starts afresh.
- **API:** `GET /api/v1/connections/quotas` (`connections.quotas`): `ConnectionQuota` per connection (`source`, `windows` of `QuotaWindow`: `name`, `used_percent`, `resets_at` in milliseconds, `status` allowed / warning / rejected; `updated_at`; `error`). Kept in the head's memory and saved in `quotas_last` at most once a minute (with a trailing save for newer reports), then restored after a restart until a fresh report arrives.
- **Not yet:** ChatGPT sessions on workers do not forward these headers to the head; that needs an additive protocol change. Until a head-side call uses a ChatGPT connection, its numbers may be absent or old. Header parsing and the head's quota API are tested against a stand-in, not a live ChatGPT account. Claude Code's report was checked against Claude Code 2.1.314.

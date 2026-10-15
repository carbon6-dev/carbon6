# Measurement — Design

Status: design only (2026-10-14). Nothing here is built yet.

## 1. Goals

carbon6 exists to make one person 10x to 100x more productive. Today nothing says whether it does. Measurement answers three questions, every week:

1. **What did I get done?** Outcomes: changes merged, replies sent, reports delivered.
2. **What did it cost me?** My attention: time spent steering, reviewing and answering. Model spend comes second.
3. **Where did my attention go?** Which kinds of work need me most, and why, so the next improvement goes there.

**The headline number is leverage:** how many hours of finished work you get per hour of your attention. Throughput alone can be gamed, so it's always shown beside acceptance: how much of the work you kept without redoing it.

**Not goals:**
- **Measuring anyone else.** carbon6 has one user, and these numbers are for you.
- **Keystroke or screen recording.** Attention is measured by presence (7), never by content.
- **New kernel code for an app.** Measurement is built from series, collections, triggers, decisions and views (docs/os.md §4). The only additions are signals carbon6 alone can see, such as when a turn ran or when you were looking (2).

## 2. What carbon6 can see, and what it can't

| Signal | Where it comes from | Seen? |
|---|---|---|
| Agent working time | each turn's start and end in the session log | yes, also for past sessions |
| Your messages that steer a session | user messages after the first, interrupts, rewinds | yes, also for past sessions |
| Your answers | decisions: approvals, questions, choices, with when they opened and when you answered | yes, also for past decisions |
| A session waiting on you | an open approval or question, or a finished turn with no reply from you yet | yes, also for past sessions |
| Model spend | the usage series (`model_spend`) | yes |
| Your time in carbon6 | presence in the UI (7) | from now on only |
| Your time outside carbon6 | reviewing in an editor, on GitHub, in a meeting | **no**: entered by hand on an outcome, if you want (4) |
| Whether the work was any good | your rating of the outcome (4) | only when you rate |
| What it would have taken you by hand | your estimate (5) | only when you estimate |

Everything carbon6 can see in past logs is backfilled (9), so the first dashboard already has history. Attention starts the day presence tracking is turned on.

## 3. Measures

Each measure is a series (docs/os.md §4.4) with the dimensions `realm`, `label` (the session's labels, one point per label) and `root` (the root session, so side chats and subagents roll up into the session they serve).

| Series | Unit | Recorded when | Meaning |
|---|---|---|---|
| `agent_ms` | ms | a turn ends | how long the agent worked: turn start to end, summed across subagents |
| `steer` | count | you send a message after the first, interrupt, or rewind | each time you corrected or redirected a session (dimension `how`: message, interrupt, rewind) |
| `answer` | count | a decision is answered | each time carbon6 needed you (dimension `kind`: approve, question, choose, …; `via`: ui, link, session) |
| `wait_on_you_ms` | ms | a decision is answered, or you reply to a finished turn | how long work waited for you (dimension `for`: approval, question, reply) |
| `attention_ms` | ms | every heartbeat (7) | your time in carbon6, attributed to what you were looking at (dimension `on`: session, decision, view, memex, other) |
| `outside_ms` | ms | you enter it on an outcome | your time outside carbon6 for that outcome |
| `outcome` | count | an outcome is rated (4) | finished work (dimension `rating`: accepted, reworked, dropped; `kind`) |
| `outcome_value_ms` | ms | an outcome is estimated (5) | what the work would have taken you by hand |

**What's derived from them** (computed in queries and views, never stored):

| Derived | Formula | What it says |
|---|---|---|
| **Leverage** | `outcome_value_ms` (accepted) ÷ (`attention_ms` + `outside_ms`) | hours of finished work per hour of you: the 10x–100x number |
| **Acceptance** | accepted ÷ rated outcomes | whether the work is any good; shown beside leverage, always |
| **Steering per outcome** | `steer` ÷ outcomes | how much hand-holding work needs; it should fall week over week |
| **Interruptions per outcome** | `answer` ÷ outcomes | how often carbon6 needed you |
| **Wait on you** | `wait_on_you_ms`, median and 90th percentile | how much you are the bottleneck |
| **Parallelism** | `agent_ms` ÷ wall-clock time, per day | how many agents ran for you at once, on average |
| **Cost per outcome** | `model_spend` ÷ accepted outcomes | what finished work costs in models |

## 4. Outcomes

An **outcome** is a unit of finished work: a record in an `outcomes` collection in each realm.

```
outcome(id, title, kind /* configurable: change, reply, report, decision, research, ops … */,
        sessions /* the sessions labelled record:<id>, see os §4.5 */,
        rating /* accepted | reworked | dropped | unrated */, rated_at,
        value_ms? /* estimate, 5 */, value_by /* you | agent */, outside_ms?,
        note?, created_at)
```

- **One per root session, by default.** When a session's turn ends with nothing more queued, carbon6 creates the outcome (unrated) and labels the session `record:<outcome>`. Side chats and subagents belong to their root's outcome. Linking another session to an existing outcome is relabeling it, so several sessions can serve one outcome.
- **Rating takes one click,** where you already are when work finishes:
  - in the session header, the Inbox and Mission cards: ✓ Accepted, ↺ Reworked (I had to fix or redo part of it), ✗ Dropped (not useful);
  - optionally with an estimate (5) and time spent outside carbon6, as chips (15m, 1h, 4h, 1d) or a number.
- **Unrated is allowed.** Measures count unrated outcomes separately and never guess a rating. A view lists unrated outcomes for a weekly sweep.
- **Agents can propose, not decide.** An agent's `outcome_report` tool fills in the title, kind and a value estimate at the end of its work. The rating is always yours.
- **Kinds are configuration:** a list in the realm's settings, with a description each, so agents can choose one.

## 5. The baseline: what would it have taken by hand?

Leverage needs a baseline: how long the work would have taken you without carbon6. There is no way to measure that, so it's estimated, and always labelled as an estimate.

- **The agent proposes** a value in `outcome_report`: what a competent engineer, or you, would need by hand, with one line of reasoning ("about 2h: three files, a migration and tests").
- **You confirm or correct it** when rating, with a chip or a number. A corrected value is marked `value_by: you`, and views show what share of the value comes from your own estimates.
- **The estimating prompt is configuration** (a prompt record), so you can calibrate it, for example "estimate my time, not a junior's".
- **Calibration over time:** once a few dozen outcomes have your corrections, the weekly review (8) reports the agent's typical over- or under-estimate, and you can adjust the prompt.

## 6. Where measures come from

| Measure | Source | How |
|---|---|---|
| `agent_ms`, `steer` | the head, from session events it already receives | a point at turn end, at each of your messages after the first, at interrupt and rewind |
| `answer`, `wait_on_you_ms` (approvals, questions) | the head, when a decision is answered | `answered_at − created_at`, by kind and how it was answered |
| `wait_on_you_ms` (reply) | the head | from a turn ending with nothing queued to your next message in that session |
| `attention_ms` | the UI's heartbeat (7) | `POST /api/v1/presence` |
| `outcome`, `outcome_value_ms`, `outside_ms` | a trigger on the `outcomes` collection | record updated with a rating → "record a value" actions (os §4.3); no code |

Only the first three rows are carbon6 code. They are the signals only carbon6 can see, so they belong in the kernel, as `model_spend` already does. Everything about outcomes is configuration: a collection, triggers, a prompt and views, shipped as a root package that can be edited like any app (os §5).

## 7. Attention: presence, not surveillance

- **What counts:** the UI sends a heartbeat every 15 seconds while the tab is visible and you've used the mouse, keyboard or touch in the last 60 seconds. Each heartbeat adds 15 seconds to `attention_ms`, attributed to the focused column's object: a session (with its realm, labels and root), a decision, a view, the memex, or "other".
- **Phones count too:** the same heartbeat from the phone layout.
- **Answering by link:** a decision answered from a notification's link adds a fixed amount (30 seconds by default, configurable), since that page has no heartbeat.
- **What is never sent:** what you typed, what was on screen, or where the mouse was. Only "present, looking at X, for 15 seconds".
- **Off switch:** Settings → Measurement turns presence off. The other measures don't depend on it, but leverage is then computed against `outside_ms` only, and says so.
- **Several tabs or tiles:** only the focused tile's focused column counts, and two tabs never double-count: the head keeps the latest heartbeat per 15-second slot.

## 8. Seeing it

All of these are views (os §4.5) in a **Measurement** page, shipped as configuration.

- **This week:** leverage, acceptance, outcomes by rating, steering and interruptions per outcome, wait on you (median and 90th percentile), parallelism, cost per outcome. Each is shown beside last week and the four-week average.
- **Where attention went:** attention by what you were looking at, by label and by kind of outcome. This shows which kinds of work cost you most per outcome.
- **Bottlenecks:** the longest waits on you this week, and what they waited for. These are candidates for a policy that removes the question.
- **Worst sessions:** the outcomes with the most steering or that were reworked, with links. These feed the improvement loop (the next design), which mines them for skills, prompts and policies to propose.
- **Unrated:** outcomes to rate, as a feed with the rating buttons.

**The weekly review** is a schedule trigger that starts a template on Monday morning:
- an agent reads the week's series and outcomes;
- it writes a short report as an output: the numbers, what changed, the three biggest attention costs, and one proposal for each;
- the report links to the views;
- the prompt is a prompt record you can edit.

**Thresholds** (series events, os §4.4) can notify you when acceptance drops below a level, or the wait on you climbs. They're off by default.

## 9. Backfill

On first install, a one-time job reads every session's log and every decision, and records the historical points for `agent_ms`, `steer`, `answer` and `wait_on_you_ms` with their original times.
- Past sessions get unrated outcomes, so the Unrated view can be swept in one sitting, or ignored.
- Attention has no history, so leverage starts on the day presence is turned on. Until then, views show the measures that have history and say which ones are missing.

## 10. Guarantees

- **Never guessed:** unrated means unrated, and estimates are labelled with who made them.
- **Quality beside quantity:** leverage is never shown without acceptance.
- **Presence only:** no content, screen or keystrokes ever leave the browser.
- **Your data:** everything stays in the head's database, inside its realm, like any series or collection. Deleting the outcomes collection and the series removes it all.
- **No new paths:** measures are ordinary series and outcomes are ordinary records, so the API, `carbon6_api`, views, triggers and agents work with them without special cases.

## 11. Build order

Each step is usable on its own.

1. **Session measures and backfill:** `agent_ms`, `steer`, `answer`, `wait_on_you_ms` recorded by the head, with history backfilled. A first chart view.
2. **Presence:** the heartbeat, `POST /api/v1/presence`, `attention_ms`, and the off switch.
3. **Outcomes:** the collection, creation at turn end, the rating buttons and chips, `outcome_report`, the triggers that record `outcome`, `outcome_value_ms` and `outside_ms`.
4. **The Measurement page and the weekly review:** the views, the review template and its trigger, optional thresholds.

## 12. Open questions

- **Rating friction:** whether one click per finished session is acceptable, or whether rating should be sampled (for example every third outcome) once the numbers settle.
- **What counts as one outcome:** one per root session is simple. Long sessions that deliver several things may need splitting: a "new outcome from here" action, or the agent reporting several.
- **Time outside carbon6:** whether to estimate it from signals instead of entering it, for example editor activity through the IDE integration.

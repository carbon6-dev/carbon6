# Measurement — Design

Status: design only (2026-10-14). Nothing here is built yet.

## 1. Goals

carbon6 exists to make one person 10x to 100x more productive. Today nothing says whether it does. Measurement answers three questions, every week:

1. **What did I get done?** Outcomes: changes merged, replies sent, reports delivered.
2. **What did it cost me?** My attention: time spent steering, reviewing and answering. Model spend comes second.
3. **Where did my attention go?** Which kinds of work need me most, and why, so the next improvement goes there.

**The headline number is leverage:** hours of finished work you keep per hour of your attention. Throughput alone can be gamed, so it's always shown beside retention: how much of the work survived into what you actually used.

**Measurement asks nothing of you.**
- Outcomes are rated from what happened to the work afterwards (4).
- The value of the work is estimated from your own unassisted work (5).
- You can override either, but you never have to.

**Not goals:**
- **Measuring anyone else.** carbon6 has one user, and these numbers are for you.
- **Keystroke or screen recording.** Attention is measured by presence (7), never by content.
- **New kernel code for an app.** Measurement is built from series, collections, triggers, decisions and views (docs/os.md §4). The only additions are signals carbon6 alone can see: turn timing, steering, waits, presence, and what git says about the work (6).

## 2. What carbon6 can see, and what it can't

| Signal | Where it comes from | History? |
|---|---|---|
| Agent working time | each turn's start and end in the session log | yes |
| Your messages that steer a session | user messages after the first, interrupts, rewinds | yes |
| Your answers | decisions: approvals, questions, choices, when they opened and when you answered | yes |
| A session waiting on you | an open approval or question, or a finished turn with no reply from you yet | yes |
| Model spend | the usage series (`model_spend`) | yes |
| What happened to the work | git (survival, reverts, follow-up fixes), drafts against sent mail, records' later edits, outputs opened (4) | yes, wherever the artifact still exists |
| Your own speed | your commits made without carbon6, your hand-written replies, your presence and editor time (5) | git history: yes; presence: from now on |
| Your time in carbon6 | presence in the UI (7) | from now on |
| Your time outside carbon6 | the IDE integration's editor activity, where it's installed (5); otherwise unseen | from now on |

## 3. Measures

Each measure is a series (os §4.4) with the dimensions `realm`, `label` (one point per session label) and `root` (the root session, so side chats and subagents roll up into the session they serve).

| Series | Unit | Recorded when | Meaning |
|---|---|---|---|
| `agent_ms` | ms | a turn ends | how long the agent worked, summed across subagents |
| `steer` | count | you send a message after the first, interrupt, or rewind | each time you corrected or redirected a session (`how`: message, interrupt, rewind) |
| `answer` | count | a decision is answered | each time carbon6 needed you (`kind`; `via`: ui, link, session) |
| `wait_on_you_ms` | ms | a decision is answered, or you reply to a finished turn | how long work waited for you (`for`: approval, question, reply) |
| `attention_ms` | ms | every heartbeat (7) | your time in carbon6 (`on`: session, decision, view, memex, other) |
| `outside_ms` | ms | editor activity reported by the IDE integration | your time outside carbon6 that carbon6 can see |
| `outcome` | count | an outcome settles (4) | finished work (`kind`; `rated_by`: signals, judge, you) |
| `outcome_retained` | 0–1 | an outcome settles | how much of the work survived |
| `outcome_value_ms` | ms | an outcome is valued (5) | what the work would have taken you by hand (the median estimate) |

**Derived** (computed in queries and views, never stored):

| Derived | Formula | What it says |
|---|---|---|
| **Leverage** | Σ(`value` × `retained`) over settled outcomes ÷ (`attention_ms` + `outside_ms`) | hours of kept work per hour of you: the 10x–100x number |
| **Retention** | Σ(`value` × `retained`) ÷ Σ`value` | whether the work is any good; always shown beside leverage |
| **Steering per outcome** | `steer` ÷ outcomes | how much hand-holding work needs; it should fall week over week |
| **Interruptions per outcome** | `answer` ÷ outcomes | how often carbon6 needed you |
| **Wait on you** | `wait_on_you_ms`, median and 90th percentile | how much you are the bottleneck |
| **Parallelism** | `agent_ms` ÷ wall-clock time, per day | how many agents ran for you at once, on average |
| **Cost per kept hour** | `model_spend` ÷ Σ(`value` × `retained`) | what kept work costs in models |

Weighting value by retention means a large change that was mostly redone counts for little, and an unused report counts for nothing.

## 4. Outcomes, rated from what happened to the work

An **outcome** is a unit of finished work: a record in an `outcomes` collection in each realm.

```
outcome(id, title, kind, sessions /* labelled record:<id>, os §4.5 */,
        artifacts /* commits, branch, draft, records, outputs: what the work produced */,
        retained /* 0–1 */, rated_by /* signals | judge | you */, evidence /* Markdown */, confidence,
        state /* open | provisional | settled */, settles_at,
        value_ms, value_low, value_high, estimates /* each estimator's value, 5 */, estimator_version,
        created_at)
```

- **One per root session, by default.** When a turn ends with nothing more queued, carbon6 creates the outcome and labels the session `record:<outcome>`. Side chats and subagents belong to their root's outcome. Linking another session to an outcome is relabeling it.
- **Artifacts are recorded as the work happens:**
  - commits made in the session's folder (from the worker's checkpoints and git);
  - mail drafts the session wrote;
  - records it created or changed;
  - outputs it published.
- **The kind** comes from the artifacts (code, reply, records, report), or else from the agent's `outcome_report`. Kinds are configuration.

### 4.1 Retained fraction, per artifact

| Artifact | Retained | Signals of rework | Signals of a drop |
|---|---|---|---|
| **Code** | lines the session added or changed that still exist on the target branch at settling (git blame, following renames) ÷ lines it added or changed | your commits, or another session's, changing those lines within the window (follow-up fixes) | its commits reverted; its branch or worktree never merged; its changes discarded |
| **Mail** | 1 − normalized edit distance between the agent's draft and what you sent | heavy edits before sending | draft deleted, never sent |
| **Records** | fields still unchanged at settling ÷ fields it set | edits soon after | record deleted |
| **Outputs** | 1 if opened, downloaded, linked or cited by a later session; 0.5 if superseded by a newer version after you steered | a newer version after steering | never opened |
| **A recommended decision** | 1 if you chose the recommended option | another option chosen | — |

- **Several artifacts:** an outcome with several artifacts is weighted by each artifact's estimated value (5).
- **Session behaviour adjusts confidence, not the score:**
  - heavy steering, or a retry (a new session on the same task within a day, found by the memex's similarity search), lowers confidence;
  - a session that ends with no artifact and no use of what it said becomes a candidate drop.

### 4.2 The judge, only where artifacts can't speak

For outcomes without artifacts (research, answers, advice), and to break ties when signals disagree, an **LLM judge** decides:
- It reads the transcript, what you did next (your next messages, sessions started, decisions answered), and the artifacts' fate.
- It returns a retained fraction with its evidence.
- Its prompt is a prompt record, and its model is a task setting, so it can be cheap.
- Its ratings are labelled `rated_by: judge` and shown separately in views, so they never pass for measured ones.

### 4.3 Settling

- **Provisional, then settled:** a rating is provisional until `settles_at`, then fixed. The window is configurable per kind: 7 days for code, 1 day for mail, 3 days for records and outputs. Code that looks fine on day one can be reverted on day three.
- **Re-checked once:** at settling, the signals are read again. A settled outcome doesn't change afterwards, except by your override.
- **Overrides are optional:** you can override any rating with a value and a note (`rated_by: you`). Nothing ever asks you to.

## 5. Value: what would it have taken you by hand?

The counterfactual can't be observed, so it's estimated by independent methods, calibrated against your own unassisted work, and shown with a range.

### 5.1 The estimators

1. **Your speed, from git history.**
   - Take commits you made without an agent: before carbon6, and any you write by hand now. They're recognized by author and by having no carbon6 session.
   - Group them into work stretches: consecutive commits with gaps under two hours (configurable).
   - Fit a model of stretch time against the change's features: lines added and changed, files, test files, languages, new code against edits, and how familiar the files were to you (prior commits).
   - Apply it to the session's changes: how long **you** would take.
2. **Replaying the agent's steps at human speed.**
   - Each step the agent took is priced at what it costs a person: reading a file (by lines), a search and reading the results, a build or test run (its own wall time), writing text or code (by size), browsing a page.
   - The rates are configuration, with defaults, and are calibrated from what carbon6 sees of you: presence on pages, editor activity, how long your own builds take.
3. **The agent's estimate.** `outcome_report` includes the agent's own estimate with a one-line reason. It's a third, weaker vote, and its prompt is a prompt record.

**Mail and records** use estimators 2 and 3, plus your writing speed: from your hand-written replies (thread opened to reply sent, when presence saw it) and their length.

### 5.2 Combining them

- **The median** of the available estimates is `value_ms`; the lowest and highest are `value_low` and `value_high`.
- **Wide disagreement** (the highest more than three times the lowest) lowers the outcome's confidence and flags it in the views, instead of being averaged away.

### 5.3 Calibration, continuously and without you

- **Every unassisted task is a calibration point.** When you do something yourself (a commit you wrote, a reply you typed), carbon6 sees both what the estimators would have predicted and what it actually took (presence plus editor time, or the work stretch).
- **Corrections from the points:** the weekly job refits estimator 1 and the step rates of estimator 2. It also reports how far off the agent's estimates tend to be, as a ratio, which is then applied to estimator 3.
- **A versioned estimator:** each refit is a new `estimator_version`. Changing the estimator recomputes every outcome's value with it, so history stays comparable. The absolute level may be off, but week-over-week trends are real, and trends are what you steer by.

## 6. Where measures come from

| Measure | Source | How |
|---|---|---|
| `agent_ms`, `steer` | the head, from session events it already receives | a point at turn end, at each of your messages after the first, at interrupt and rewind |
| `answer`, `wait_on_you_ms` | the head, when a decision is answered, or you reply to a finished turn | `answered_at − created_at`; turn end to your next message |
| `attention_ms` | the UI's heartbeat (7) | `POST /api/v1/presence` |
| `outside_ms` | the IDE integration | editor activity, by the same rules as presence |
| Artifact fate (code) | **a git job on workers** | the head asks the worker that holds the repository: blame for the session's lines, reverts, follow-ups, merges. It runs at provisional time and at settling. |
| Artifact fate (mail, records, outputs) | the memex and the head | drafts against sent, record history, output downloads and citations |
| Your speed model | the git job, weekly | your commits' work stretches and features, sent to the head to fit |
| `outcome`, `outcome_retained`, `outcome_value_ms` | a trigger on the `outcomes` collection | record settles → "record a value" actions (os §4.3) |

**Where the line is drawn:**
- **Code in carbon6:** the signals, the git job and the estimators. Only carbon6 can see turns, presence, workers' repositories and the logs.
- **Configuration:** the collection, triggers, prompts (judge, `outcome_report`), windows, rates and views. They ship as a root package you can edit like any app (os §5).

## 7. Attention: presence, not surveillance

- **What counts:** the UI sends a heartbeat every 15 seconds while the tab is visible and you've used the mouse, keyboard or touch in the last 60 seconds. Each heartbeat adds 15 seconds to `attention_ms`, attributed to the focused column's object: a session (with its realm, labels and root), a decision, a view, the memex, or "other".
- **Phones count too:** the same heartbeat from the phone layout.
- **Answering by link:** a decision answered from a notification's link adds a fixed amount (30 seconds by default, configurable), since that page has no heartbeat.
- **What is never sent:** what you typed, what was on screen, or where the mouse was. Only "present, looking at X, for 15 seconds".
- **Off switch:** Settings → Measurement turns presence off. Leverage is then computed against `outside_ms` only, and the views say so.
- **No double counting:** only the focused tile's focused column counts, and the head keeps one heartbeat per 15-second slot across tabs and devices.

## 8. Seeing it

All of these are views (os §4.5) on a **Measurement** page, shipped as configuration.

- **This week:** leverage, retention, kept hours, steering and interruptions per outcome, wait on you (median and 90th percentile), parallelism, cost per kept hour. Each is shown beside last week and the four-week average, with the share of outcomes still provisional.
- **Where attention went:** attention by what you were looking at, by label and by kind of outcome, so you can see which kinds of work cost you most per kept hour.
- **Bottlenecks:** the longest waits on you, and what they waited for. These are candidates for a policy that removes the question.
- **Least retained:** settled outcomes with the lowest retention or the most steering, with their evidence. These feed the improvement loop (the next design).
- **Low confidence:** outcomes where estimators disagree widely or the judge decided, so you can see what the numbers rest on. No action is needed.
- **Calibration:** each estimator's error against your unassisted tasks, over time.

**The weekly review** is a schedule trigger that starts a template on Monday morning:
- an agent reads the week's series and outcomes;
- it writes a short report as an output: the numbers, what changed, the three biggest costs to your attention, and one proposal for each;
- the report links to the views;
- the prompt is a prompt record.

**Thresholds** (series events, os §4.4) can notify you when retention drops or the wait on you climbs. They're off by default.

## 9. Backfill

On first install, a one-time job:
- records the historical points for `agent_ms`, `steer`, `answer` and `wait_on_you_ms` from every session log and decision;
- creates outcomes for past sessions and rates them from what happened since. Old changes are already settled: git shows whether they survived;
- fits your speed model from your repositories' history, and values past outcomes with it.

Attention has no history, so leverage starts on the day presence is on. Until then, views show kept hours and the other measures, and say which ones are missing.

## 10. Guarantees

- **Nothing asked of you:** every outcome is rated and valued automatically. Overrides are optional.
- **Labelled by source:** each rating says whether it came from signals, the judge or you, and carries its evidence. Each value carries its range and estimator version. Judged ratings and wide ranges are shown as such.
- **Quality beside quantity:** leverage is never shown without retention, and value only counts in proportion to what was kept.
- **Comparable history:** a new estimator version revalues everything, so trends never mix versions.
- **Presence only:** no content, screen or keystrokes leave the browser or the editor.
- **Your data:** everything stays in the head's database, inside its realm, as ordinary series and records. The git job reads repositories on your workers and sends back only counts and features, never code.

## 11. Build order

Each step is usable on its own.

1. **Session measures and backfill:** `agent_ms`, `steer`, `answer` and `wait_on_you_ms` recorded by the head, with history. A first chart view.
2. **Outcomes and artifacts:** the collection, creation at turn end, artifacts recorded from sessions, `outcome_report`.
3. **The git job and retention:** survival, reverts and follow-ups on workers; mail, records and outputs from the memex and the head; provisional and settled ratings; the judge; backfill of past outcomes.
4. **Value:** the speed model from git, the step replay, the agent's estimate, the ensemble, versioning.
5. **Presence:** the UI heartbeat, the IDE integration's editor time, `POST /api/v1/presence`, the off switch.
6. **The Measurement page and the weekly review:** the views, calibration, the review template and its trigger, optional thresholds.

## 11a. Decisions

| Question | Decision |
|---|---|
| Rating | Automatic: a retained fraction from what happened to the work, provisional until settled; the judge only where there are no artifacts or signals disagree; your override optional |
| Baseline | Automatic: the median of your git speed model, a replay of the agent's steps at human speed, and the agent's estimate; calibrated from your unassisted work; versioned |

## 12. Open questions

- **Settling windows:** whether 7 days for code is long enough to catch reverts and follow-up fixes in how you work.
- **Your history:** which repositories, authors and time range count as your unassisted work (for example your email addresses in git, and the date you started using agents).
- **Several outcomes in one session:** a long session that delivers several things may need splitting, by its artifacts (one per branch, draft or output) or by the agent reporting several.

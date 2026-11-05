# Watchers

Status: decided with you (2026-11-05), being built. Decisions are in §9.

Some sessions run for days. Learning from them (retrospectives), reviewing them, and keeping them consistent with each other can't wait for them to end. A **watcher** is a standing agent with its own agenda. It follows one session, a set of sessions, or everything, and acts on what it sees as things happen. carbon6 already has a few watchers in all but name: the work item's note, suggested next messages, session names and catch-ups each read sessions and react. Watchers make that one mechanism.

## 1. What a watcher is

| | |
|---|---|
| **Scope** | one session, sessions matching labels (`repo:…`, `project:…`), a project, or everything in a realm (root: every realm) |
| **Agenda** | an editable prompt: what it looks for and what it does about it |
| **Wakes on** | moments that matter, not every token: a turn ends, a commit, a test or CI failure, N tool calls, a correction from you, idle for a while, a schedule, or a batch (hourly or daily) |
| **Reads** | what changed since it last looked (turn digests, diffs, tool failures, log lines), not the whole transcript again |
| **Can do** | only what it's allowed: a note to the watched agent, a mark on the session's tile, a decision card or notification for you, a lesson proposal, a memory note, a line in a document, a task card, a change to a work item's note |
| **Remembers** | its own notes and its position, per watched session or for its scope |
| **Model and budget** | routed as `task:watch:<name>` (usually a small model), with a daily token budget and rate limits |

## 2. Built in, or pre-installed

- **Code:** only the runtime and the outputs.
  - The runtime: waking on events, digests since the last position, memory, budgets, rate limits, and the guard's hook into policies.
  - The outputs: what a watcher may do (a note, a mark, a decision, a lesson, a document line, a work note).
- **Pre-installed definitions:** every watcher carbon6 ships with. You can edit, turn off, copy or delete them, as with prompts and templates. A definition holds the agenda, scope, wake events, model route, budget, allowed outputs, severity thresholds, and whether it's on.
  - An update changes a definition you haven't touched. One you've edited keeps yours, with the new default available to compare.
  - Definitions could ship as packages, so you can share your own.

## 3. Observable like any session

- **Its conversation is a session.**
  - Each wake is a turn, showing what woke it and the digest it got, its reasoning and tool calls, and what it did.
  - A wake with nothing new is a one-line skip, not a model call.
- **Links go both ways.** Its findings link to the turns they're about. The watched session shows its watchers as marks in its tile bar, and a watcher's note appears in the watched transcript, labelled, linking back to the turn that produced it.
- **Its memory and position** are shown and editable. Its tokens count under its name in Usage and Activity.
- **One session or many:** a watcher on one session gets a watcher session per watched session (opened like a side chat). A watcher over a project or labels gets one session for the whole scope, and each turn says which session it's about.
- **You can talk to it** ("ignore style nits in this repo", "why did you flag that?"). You can also pause it, mute it for one session, or stop it.
- **Kept out of the way:** watcher sessions show on a Watchers page and through marks. A filter on the session list shows them when you want.

## 4. Lifetime

- **Long-lived but mostly asleep.** There's no process and no model call between wakes. The head notes the events a watcher cares about, and a wake is one short turn.
- **It survives restarts.** Its conversation, memory and position are in the head's database. After a restart it reads what it missed since its position.
- **It runs in the head** (like an Ask chat), so it needs no worker. Only its outputs reach other sessions.
- **Lifetime depends on scope.**
  - A watcher on one session ends with it, after a last turn (for example, a final retrospective pass).
  - A watcher over a project or labels lives while it's enabled, and attaches to matching sessions as they start.
- **Months of turns stay manageable.** It compacts like any long session. What matters lives in its memory and position.

## 5. Uses

1. **Continuous retrospectives.**
   - Wakes on correction signals: you redirect, interrupt, rewind, deny an approval, or edit right after the agent. Also wakes on turn ends that had friction.
   - Proposes lessons while the session runs, each as a change to something carbon6 already uses: a memory note (scoped by label), a prompt line, a skill, a policy, a routing rule, a template default, or a task card for the repo.
   - A weekly roll-up over everything does three things: it finds repeats, checks whether applied lessons helped (did the signal recur?), and proposes pruning notes and rules that never fire.
2. **Review as things progress.** Wakes when files change or a commit lands, and reviews only the new diff against the work's "done when" and the plan document. Findings go to the working agent as a note it sees at its next step. You get a mark only for serious ones.
3. **Consistency across sessions.**
   - Scoped to a project or repo, it keeps a **decisions ledger** document of names, APIs, conventions and choices as sessions make them.
   - It checks each session's new turns against the ledger, and tells both sessions and you when they diverge. Examples: one session renames a field another uses, or two sessions edit the same files on different workers.
4. **Stuck detection.** An agent looping on the same error, retrying in circles, waiting forever on a background job, or idle waiting for you. It nudges the agent, or tells you.
5. **Drift from the goal.** The session wandered into an unrelated refactor, or is polishing past "done when".
6. **Guard.** Risky actions: pushing to main, deleting outside the worktree, secrets in output, a destructive migration. This has to act before the tool runs, so it hooks into policies: hold the call, a cheap model judges, you decide.
7. **Cost.** Quota burn, a poor prompt-cache hit rate, a large model on a small task, a context near full. It suggests a switch or a compaction.
8. **CI and tests after a push.** Watches the run and feeds failures back into the session that pushed.
9. **Progress reporting.** Keeps work and project notes and status documents current, and writes a standup digest. Today's work note becomes this watcher.
10. **Capturing knowledge.** Turns decisions made in sessions into ADR-style entries or knowledge facts as they happen.
11. **Handoffs.** Session B waits on something session A produces. It tells B when it lands.
12. **Keeping docs true.** Updates "As built" sections, or flags where code and design doc disagree.
13. **Improving carbon6 itself** (§6).
14. **Standing quality directives** (§7).

## 6. A watcher that improves carbon6

Scope: everything (it runs in root). It's a retrospective whose subject is carbon6 rather than your work.

- **It watches:**
  - carbon6's own failures: head warnings and errors, "the web UI broke" reports, failed calls to carbon6's tools, failed connector syncs and kit jobs, CI and test flakes;
  - friction in sessions: an agent using shell or curl for what a carbon6 tool should do, repeated `find` searches, gate refusals that recur, approvals you always allow;
  - what you say about carbon6 in sessions ("this panel flickers", "I have to say continue");
  - unused features and cost oddities.
- **It produces** task cards for the carbon6 repo, each with evidence and a prompt that stands alone, plus a weekly "carbon6 improvements" document ranked by how often a problem happens and what it costs you. It never changes carbon6 itself: every change goes through a card you start.
- **What keeps it useful:**
  - It knows carbon6: the repo's docs, the operation catalog, recent commits and open cards. So it doesn't propose what exists or is already planned, and links to an existing card rather than adding a duplicate.
  - Cheap filtering comes first: log lines and reports are grouped and counted without a model, and only groups over a threshold reach the model. It wakes on batches.
  - It needs evidence: something seen three times, or one clear failure with a trace.
  - It reads every realm but writes only about carbon6, quoting as little of your content as it can.
- **Its limit:** it can't change its own agenda or scope without you.

Examples from 2026-11-03/05 it would have caught:
- "unknown connection kind `claude_code`" in the head's log (task card #127);
- `find` needing four searches for `work.todos.remove`;
- the NUL-byte error;
- the ChatGPT usage endpoint answering 401;
- a CI runner running out of disk.

## 7. Standing quality directives

You often ask for a quality in one implementation: "make sure memory is accounted for in this function". The directive is usually more general than the function. A watcher (the retrospective, or one listening for directives) notices a directive that reaches beyond the task at hand, and asks you how far it should go:

1. **Here only:** nothing more.
2. **Audit now:** a task card for a one-off session that checks the codebase (or the scope you pick) for the same quality. For example, "is memory accounted for consistently?". It reports or fixes what it finds.
3. **From now on:** the directive becomes a standing **standard** in that scope.
   - **Where possible, a mechanical check:** a lint rule, a test, a CI step, or a line in the repo's instructions. A check is cheaper and more reliable than a model, so the watcher proposes a task card to add one when the quality can be checked mechanically.
   - **Otherwise, a judgement call:** the standard is a **memory note** in the scope (repo, project, labels), with an **enforced by** field: which watcher checks it (usually review), or which mechanical check now covers it. It keeps its wording, scope, and the turn it came from, like any note. (Decided 2026-11-05: standards are memory notes, not a separate list.)
     - New sessions in the scope get it as they get every memory note today.
     - The review watcher (§5.2) reads the notes in its scope that name it as their enforcer when it reviews a diff.

So directives don't each become a watcher (which would multiply watchers and cost). They become memory notes that one review watcher enforces. A standard that never finds anything, or that a mechanical check now covers, is proposed for retirement in the weekly roll-up.

## 8. What makes it work rather than nag

- **Cheap by design.** It wakes only on moments that matter, reads only what changed, uses a small model and stays within a budget. When nothing changed, it skips without calling a model.
- **A high bar for speaking.**
  - Findings have a severity, there's a rate limit per session, and repeats are dropped.
  - The watched agent can reply ("acknowledged", or "disagree, because…") so a watcher doesn't repeat itself.
  - You can mute a watcher per session.
- **You stay in charge of changes.** Watchers propose lessons, standards, ledger entries and policies; you approve them. Only notes to the watched agent go straight in, and you choose that per watcher.
- **Realms stay separate.** A watcher in a realm acts in that realm. Only root's watchers see across realms.
- **One mechanism.** Over time, suggestions, naming, catch-ups and the work and project notes move onto watchers: one Settings page, one budget view, one way to see why something was said.

## 9. Decisions (2026-11-05)

- **Notes into the watched session** go straight in, but only for findings rated important or above, and at most a few per hour per session. The rest go to the Watchers page and appear as a mark on the session. Each watcher has a switch that sends everything to you instead.
- **Pre-installed, and on by default:**
  - stuck detection: on;
  - continuous retrospective: on;
  - incremental review: on for sessions labelled `repo:…`;
  - carbon6 improvement: off (you turn it on in root).
- **Retrospective lessons:**
  - Memory notes scoped to one repo or one worker are applied automatically, and listed in the weekly document so you can undo them.
  - Prompts, policies, routing rules and more widely scoped notes always ask you.
- **Cadence:** lessons as sessions run, plus a weekly document that rolls them up and proposes pruning.
- **Repo lessons** become task cards for you to start, never automatically.
- **Standards** are memory notes with an "enforced by" field (§7).

## 10. How it would be built (proposed)

1. **The runtime:**
   - definitions as editable settings, wake events, digests since a position, memory, outputs, budgets and rate limits;
   - a Watchers page with each watcher's findings and cost;
   - watcher sessions shown like side chats.
2. **Pre-installed:** continuous retrospective, incremental review and stuck detection, off by default and enabled per label.
3. **Standards and directives:** the "enforced by" field on memory notes, the "here / audit / from now on" question, and review reading the notes it enforces.
4. **The decisions ledger and consistency watcher, and the guard's hook into policies.**
5. **The carbon6 improvement watcher.**
6. **Moving the existing ad-hoc watchers over:** the work and project notes, suggestions, naming, catch-ups.

## 11. As built (2026-11-05)

Steps 1–5 are built. Step 6 (moving the work and project notes, suggestions, naming and catch-ups onto watchers) is not: those keep their own code for now.

- **Where it lives.**
  - `crates/c6-head/src/watchers.rs`: the runtime, outputs, ledger, guard, signals and schedule.
  - `crates/c6-api/src/watchers.rs`: definitions and the shipped ones (`preinstalled()`).
  - Migration `0063_watchers.sql`: `watcher_state` (position, notes, counts per watcher and key), `watcher_spend` (tokens a day), `watcher_findings`, `watch_signals`.
  - The agendas are the editable prompts `watch.stuck`, `watch.retro`, `watch.retro-weekly`, `watch.review`, `watch.consistency`, `watch.guard`, `watch.improve`, plus `watch.answer` (the answer's shape, added after every agenda).
  - The UI is in `crates/c6-ui/src/watchers.rs`: Settings → Watchers (console), and a Watchers line in the session's Glance.
- **Definitions.** The setting `watchers` holds only the definitions you saved. A shipped watcher you never saved follows carbon6's version; one you saved keeps yours (`newer_default` says when carbon6's differs). Reset takes yours back.
- **`watchers` in the head's config.** A head run from its config file runs the shipped watchers as they ship (`watchers = false` turns them all off). A head built in code, such as a test's, starts them off. Definitions you saved apply either way.
- **Pre-installed, as decided (§9):**
  - **on:** `stuck` (wakes on `tool_failures`, `loop`), `retro` (wakes on `correction`, `session_end`), `review` (sessions with a `repo:` label, wakes on `commit`, `edits`) and `retro-weekly`;
  - **off:** `consistency`, `guard` and `improve`.
- **Waking.** Every batch of a session's events with a boundary in it pokes each watcher that applies to it: a user message, a tool result, a turn's end, an approval, a rewind.
  - A wake (one at a time per watcher and session, coalescing what comes meanwhile) reads up to 400 events from its position and works out what happened, with no model.
  - The digest uses plain display text, stripping provider citation markers without changing the recorded text or annotations. Queued quiet notices are left out of the digest and wake reasons. When the agent's next input takes them, their `task-notification` events appear as agent notices, not user messages or corrections.
  - **What wakes it:**
    - `correction`: an interruption, a rewind, a denied approval, or a message of yours that reads like one ("no", "don't", "instead", "make sure", "always"…);
    - `loop`: the same call three times;
    - `tool_failures`: three failures in a row, or three of one tool (a shell command that exits non-zero counts as a failure, though the tool call itself succeeded);
    - `commit`, `edits` and `turn_end`.
  - Only a match with the watcher's wakes calls its model. Anything else counts as a skip and writes nothing to its session.
  - A watcher's first wake reads from a little before the session's end, not from its start.
- **One model call per wake, not a tool loop.** The agenda and what changed go to the model, which answers JSON (findings, its new notes, a reply). The head then does what the findings ask, within the watcher's `outputs`. This was simpler, cheaper and easier to bound than giving a watcher tools.
  - The turn in its session shows what woke it and the digest, its answer, its usage, and a quiet notice per thing done.
  - Its model: a routing rule for `task:watch:<name>`, else the watched session's (when the head can ask it), else the assistant's, else what a new session would get. Up to 8,192 output tokens, room for a thinking model's reasoning (with less, qwen3.6 sometimes answered with nothing).
- **Outputs.**
  - **Notes to the agent:** a message from the watcher's session (`sessions::message_from_watcher`), only at or above `note_severity`, and at most `notes_per_hour` per session; the rest are marks.
  - **Marks:** a finding on the session, shown in the Glance's Watchers line. carbon6's marks (`marks.rs`) are colours of aspects, so a watcher mark is a finding rather than one of those.
  - **Lessons:** memory notes scoped to `repo:…` or `worker:…` are applied at once (written by `watcher:<name>`). Others become an approval card, and Allow adds the note.
  - **Directives:** a card asking Here only / Audit now / From now on.
    - Audit now: a task card.
    - From now on: a memory note with `enforced_by: review`, and a task card for a mechanical check when the model said one could check it.
  - **Changes** to a prompt, policy or routing rule: a card. Allow records your agreement; the watcher doesn't edit those settings itself. A memory note to retire is deleted on Allow.
  - **Task cards** start from the watched session. The improvement watcher's start from the newest session in a carbon6 checkout, else its own session.
  - **Documents** (the weekly retrospective) go to home when the watcher's realm is root.
- **Standards.** Memory notes gained `enforced_by` (kept when an edit doesn't say). The review watcher's prompt gets the notes in the session's realm whose `enforced_by` names it and whose scope matches the session.
- **Review** reads the session's uncommitted changes from its worker (`git.diff`) along with the digest. After a commit the diff is empty, so the digest's tool calls carry what was changed.
- **The decisions ledger** is one document per project (else per realm), made on first use and put on the project as notes. The consistency watcher appends a line per new decision, and tells sessions that go against an entry.
- **The guard** is a policy action, `judge`. While the guard watcher is on, the head adds a built-in policy with its pattern.
  - A worker's matching call asks the head (`session.guard`), which asks the guard's model. A risky call waits for your approval with the guard's reason; one judged safe runs.
  - If the head can't judge, the call waits for you.
  - Policies apply where carbon6's loop runs the tools (and to carbon6's tools under Claude Code). Claude Code's own tools follow Claude Code's permissions.
- **The improvement watcher.**
  - A tracing layer in `c6h` (`SignalLayer`) keeps the head's warnings and errors.
  - The web UI's crash reports and failed calls to carbon6's tools are counted too.
  - All are grouped by their message with ids and numbers taken out (`watch_signals`), without a model.
  - Hourly, or when you run it, only groups at or over `threshold` that grew since it last looked go to the model.
- **The schedule.** Every minute the head runs the watchers over everything (scope `all`) that are due: hourly, daily or weekly, counted from their last wake. One that never woke is due a period after the head first had it on (the setting `watchers.first_on.<name>`), not at once, so a weekly roll-up doesn't run at every fresh start.
  - One with nothing new skips without a model call: the weekly roll-up with no findings, the improvement watcher with no grown groups.
  - On start, the head knows its watchers' sessions again and wakes those whose sessions moved while it was down.
- **Lifetime.** Archiving a session ends its watchers. Those that wake on `session_end` take a last turn first, then their sessions are archived. Archiving a watcher's session stops it following.
- **Out of the session list.** Watcher sessions are left out of the session list. Per-session ones open from the Glance; all of them from Settings → Watchers' findings.
- **Not built:**
  - step 6;
  - `idle`, `daily` signals of a session (only scheduled watchers use daily/weekly);
  - a watcher replying in the watched session's transcript other than through its notes;
  - watchers as packages.

### Seen with a real model (2026-11-05)

A scratch head (port 17890) and worker, with local Ollama qwen3.6:27b as both the sessions' and the watchers' model. The head ran from a config file, so the shipped watchers were on.

- **Stuck detection.**
  - The first session was told to retry a failing `cat` five times. That woke stuck detection (`loop`, 2,983 tokens), and it rightly found nothing: the user had asked for the retries.
  - This showed that a shell command exiting non-zero isn't a tool error, so `tool_failures` never counted failed commands. Fixed: such output counts as a failure.
  - The second session polled for a file that never came, varying its commands, so no wake reason matched (3 skips, no model call). Long waits aren't a wake reason yet.
  - Woken by hand (`watchers.run`), stuck detection found it stuck in 1 turn (2,406 + 197 tokens) and sent the agent an important note: "Stop the long polling loop… inspect the status/logs of the deployment process". The note was queued for the agent's next step, behind its running command.
- **The retrospective.** Your correction ("no, don't retry blindly… make sure every shell script you write here starts with set -euo pipefail") woke it (`correction`, 3,708 tokens). It proposed one lesson for every session, about bounding polling loops, as an approval card (decision 1), and Allow added the memory note. qwen raised no directive from that message.
- **Review.** After the agent wrote `hello.sh` and committed it, review woke on `edits`/`commit`, with a standard ("Every shell script starts with set -euo pipefail.", `enforced_by: review`) in its prompt.
  - qwen answered with nothing, so there's no finding. The failed wake didn't move its position, so its next wake reads the same changes again.
  - A weekly roll-up and one retrospective wake also failed, with an empty answer or with Ollama's "error decoding response body" while it served several calls at once.
  - A scheduled watcher that never woke used to wake at start: that's how the weekly roll-up ran there. Fixed: it waits a period from when the head first had it on.
  - Watchers now get 8,192 output tokens, room for reasoning.
- **Totals for the run:** stuck woke 2 times and skipped 18; the retrospective woke 2 and skipped 9; most of the session's turns were skips, with no model call.

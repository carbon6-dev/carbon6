# Watchers

Status: ideas, being shaped with you (2026-11-05). Nothing is decided or built yet. Open questions are in §9.

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
   - **Otherwise, a judgement call:** the standard goes into a **standards list** for the scope (repo, project, labels). Each standard keeps its wording, scope, the turn it came from, and how it's enforced.
     - The review watcher (§5.2) reads the list when it reviews a diff.
     - New sessions in the scope are told the standards that apply, as memory notes are today.

So directives don't each become a watcher (which would multiply watchers and cost). They become entries that one review watcher enforces. A standard that never finds anything, or that a mechanical check now covers, is proposed for retirement in the weekly roll-up.

## 8. What makes it work rather than nag

- **Cheap by design.** It wakes only on moments that matter, reads only what changed, uses a small model and stays within a budget. When nothing changed, it skips without calling a model.
- **A high bar for speaking.**
  - Findings have a severity, there's a rate limit per session, and repeats are dropped.
  - The watched agent can reply ("acknowledged", or "disagree, because…") so a watcher doesn't repeat itself.
  - You can mute a watcher per session.
- **You stay in charge of changes.** Watchers propose lessons, standards, ledger entries and policies; you approve them. Only notes to the watched agent go straight in, and you choose that per watcher.
- **Realms stay separate.** A watcher in a realm acts in that realm. Only root's watchers see across realms.
- **One mechanism.** Over time, suggestions, naming, catch-ups and the work and project notes move onto watchers: one Settings page, one budget view, one way to see why something was said.

## 9. Open questions

- Should a watcher's notes go straight into the watched session, or wait for you at first?
- Which watchers come pre-installed and on, and which ship off? Candidates: retrospective, incremental review, stuck detection, improving carbon6.
- For retrospectives:
  - Approve every lesson, or auto-apply narrowly scoped memory notes and review the rest?
  - Per-session lessons plus a weekly document, or only the weekly one?
  - Should lessons about a repo become task cards automatically?
- For standards: is the standards list a new thing, or a kind of memory note with an enforcement field?

## 10. How it would be built (proposed)

1. **The runtime:**
   - definitions as editable settings, wake events, digests since a position, memory, outputs, budgets and rate limits;
   - a Watchers page with each watcher's findings and cost;
   - watcher sessions shown like side chats.
2. **Pre-installed:** continuous retrospective, incremental review and stuck detection, off by default and enabled per label.
3. **Standards and directives:** the standards list, the "here / audit / from now on" question, and review reading the list.
4. **The decisions ledger and consistency watcher, and the guard's hook into policies.**
5. **The carbon6 improvement watcher.**
6. **Moving the existing ad-hoc watchers over:** the work and project notes, suggestions, naming, catch-ups.

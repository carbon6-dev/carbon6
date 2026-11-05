# Task cards — Design

Status: built (2026-10-20). Decided: shown in the session that raised them and in Decisions, under "Suggested tasks", at low priority.

## 1. What it does

While an agent works, it notices things worth doing that don't belong in the current change: dead code, a stale doc, a missing test, a confirmed TODO, a security issue spotted in passing. Instead of doing them, which bloats the change, or forgetting them, it proposes each as a **task card**:

- A title and one or two lines on why now.
- **Start session** is the primary button. It begins a new session that does the task: same worker, folder and realm, with a self-contained prompt the agent wrote.
- **Do it here** is an outlined secondary button. It has the session that proposed it do it, as your next message to it; its tooltip names that session.
- **Save for later** is an identical outlined secondary button. It keeps the card as a to-do in the work its session belongs to (docs/work.md), creating new work if it belongs to none. Its tooltip names the work when available.
- **Not now** is quiet; it puts the card away.
- **Save elsewhere…** is a small, muted control after the buttons. It opens a dialog offering **New work** and a searchable list of the card's realm's open work items. This uncommon path does not repeat the main Save action. The dialog uses a portal over the workspace, with a scrollable list, so the card and message box cannot clip it; outside click, Close or Escape dismisses it.

The four actions share one line, wrapping on narrow screens. They stay compact on desktop and have at least 44px touch targets with a coarse pointer. There is no split button or arrow. Task cards have no note field, Work on it, Tomorrow or Dismiss controls; other decisions keep their existing controls. Both the transcript and Decisions use the same card renderer. Saved cards are ordinary work to-dos, with their existing start and completion controls.

The current session carries on undisturbed, never waiting on a card.

## 2. Raising one (the agent)

- **A tool:** `task_propose {title, why, prompt, worktree?, labels?}`.
  - `prompt` must stand alone, including file paths and enough context to act without this conversation. The new session sees only that.
  - `worktree: true` starts the task in a fresh git worktree, so it doesn't touch the current one. That's the default when this session uses one.
  - Returns the card's id.
- **And its pair,** `task_withdraw {id, why}`: the agent withdraws a card that went stale, for example because it fixed the thing after all or the user asked for it in this session.
- **When to propose:** that guidance is an editable prompt, `session.task_cards` (Settings → Prompts), added to the session's instructions when the tool is on. Not tool text in code. The default says:
  - **propose:** dead code, stale docs, missing coverage, a confirmed TODO, a security issue noticed in passing;
  - **don't propose:** vague code-smell observations, trivial fixes better done now, low-confidence hunches, or anything the user asked for in this session;
  - at most a few per session; a duplicate of an open card's title is the same card.
- **Which sessions get the tool:** coding sessions, i.e. the tool profiles for Claude, GPT and generic models; on by default. Not subagents, whose findings go back to their parent, and not the assistant.
- **Claude Code sessions** don't get the tool, since that agent runs in Claude Code's CLI with its own tools.

## 3. What a card is

A decision, so it's kept, listed and answered like everything else that needs you:

- `kind: "task"`, `priority: "low"`, `group: "Suggested tasks"`;
- `ref: "task:<session>:<id>"`, `options: ["Start it", "Do it here", "Add to this work", "Add as new work", "Not now"]`;
- `context: {session, prompt, worker, cwd, repo, worktree, labels}`;
- `document`: the why, then the prompt in a collapsed block, so you see exactly what the new session will be told.

The stored option labels above remain the action identifiers for compatibility with existing cards and the head. The UI maps **Start session** to `Start it`, **Save for later** to `Add to this work`, and **New work** to `Add as new work`. Do it here and Not now keep their identifiers. For an existing destination the answer accepts `Add to work:<id>`, including on old cards without that stored option. Only task-card decisions accept this dynamic option; the head validates that the work exists, is open and is in the card's realm, which the answering caller must be able to see. A failed save leaves the decision open. No database migration is needed.

Where it shows:

- **The session:** under **Suggested tasks**, as a compact card with its answers. **Waiting for you** appears only when non-task decisions are listed too; a task-only panel has no empty waiting heading.
- **Decisions:** in the "Suggested tasks" group, low priority, below what waits on you. It sends no notification to your phone, since task cards aren't urgent.

## 4. Starting one

**Start session** creates a session from the card's own prompt, not a helper to settle the decision:

- same worker (or any online one, if it's gone), folder, repo and realm;
- a fresh worktree if `worktree`;
- model by routing, with the labels `task-card` and `from:<session>`;
- title = the card's title.

The card is answered with "Start it" and names the new session ("working session ↗"). Starting one is never automatic.

The new session is on its own from there: nothing it does is reported back to the session that proposed the card. You see its result in the new session (linked from the card), and the `from:<session>` label ties the two together.

### 4.1 The other answers

- **Do it here** sends the card's prompt to the session that proposed it, as a message from you ("Do the task you proposed here … (task card #N: title)"). If the session is in a turn, the message waits for it, as yours do.
- **Save for later** adds the card as a to-do (docs/work.md §12) to the open work, in the card's realm, that holds the session (its root, for a subagent): its title as the text, its prompt as the prompt, `from: decision:<id>`, and the session as its origin. If no work holds the session, it makes new work, as below. The work name follows the live session list; the action always uses the session's current work when clicked.
- **Save elsewhere… → New work** makes a work item titled like the card, with that one to-do.
- **Save elsewhere… → an existing work item** files the same to-do, with its prompt and origin, into the chosen open work item, without moving the proposing session.
- **Starting it later:** ▶ Start on the to-do opens the new-session form where the proposing session worked, with the card's prompt (by `work.todo`). The session joins the work and can tick the to-do off.

## 5. Not doing

- Cards between realms: a task card starts in its own session's realm.
- Agents starting tasks themselves: that's what subagents are for.
- Re-raising a dismissed card: dismissed is final for that session. A new session may raise it again.

## 6. Steps

1. **Tools:** `task_propose` and `task_withdraw` in c6-tools, and the head's `session.task_propose` and `session.task_withdraw`, which open and settle the decision with its context.
2. **Prompt:** `session.task_cards`, appended to the base prompt for sessions with the tool.
3. **Start it:** the decision's answer starts the session (`decisions::act` for `task:`), worktree included.
4. **UI:** the compact card in the session's Suggested tasks, with the prompt collapsed. Decisions groups it under Suggested tasks too.
5. **Tests:**
   - a scripted agent proposes a task: the card appears in the session and in Decisions;
   - Start it creates the session with the card's prompt, worker, folder and realm;
   - withdraw settles the card;
   - a duplicate title raises the same card;
   - a subagent doesn't get the tool.

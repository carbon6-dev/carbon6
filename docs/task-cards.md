# Task cards — Design

Status: built (2026-10-20). Decided: shown in the session that raised them and in Decisions, under "Suggested tasks", at low priority.

## 1. What it does

While an agent works, it notices things worth doing that don't belong in the current change: dead code, a stale doc, a missing test, a confirmed TODO, a security issue spotted in passing. Instead of doing them, which bloats the change, or forgetting them, it proposes each as a **task card**:

- A title and one or two lines on why now.
- **Start session** is the primary button. It begins a new session that does the task: same worker, folder and realm, with a self-contained prompt the agent wrote.
- **Do it here** is an outlined secondary button. It has the session that proposed it do it, as your next message to it; its tooltip names that session.
- **Save for later** is an identical outlined secondary button. It keeps the card as a to-do in the work its session belongs to (docs/work.md), creating new work if it belongs to none. Its tooltip names the work when available.
- **Not now** is quiet; it puts the card away.
- **Save elsewhere…** is a small, muted control after the buttons. It opens a dialog offering **New work** and a searchable list of the card's realm's open work items. New work first opens an editable name seeded from the selected card's title; Enter or **Create work** confirms, while Cancel or Escape creates nothing. This uncommon path does not repeat the main Save action. The dialog uses a portal over the workspace, with a scrollable list, so the card and message box cannot clip it; outside click, Close or Escape dismisses it.

The four actions share one line, wrapping on narrow screens. They stay compact on desktop and have at least 44px touch targets with a coarse pointer. There is no split button or arrow. Task cards have no note field, Work on it, Tomorrow or Dismiss controls; other decisions keep their existing controls. Both the transcript and Decisions use the same card renderer. Saved cards are ordinary work to-dos, with their existing start and completion controls.

The current session carries on undisturbed, never waiting on a card.

## 2. Raising one (the agent)

- **A tool:** `task_propose {title, why, prompt, worktree?, labels?}`.
  - `prompt` must stand alone, including file paths and enough context to act without this conversation, which is not forwarded automatically. The new session also gets its session instructions and environment; this is context delivery, not file or realm isolation.
  - `worktree: true` starts the task in a fresh git worktree, so it doesn't touch the current one. That's the default when this session uses one.
  - Returns the card's id.
- **And its pair,** `task_withdraw {id, why}`: the agent withdraws a card that went stale, for example because it fixed the thing after all.
  - If this session's card was already answered, the tool succeeds without changing it and says what happened, for example `task card #177 already closed: Not now, nothing to do`. It still refuses unknown ids and cards proposed by another session.
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

The stored option labels above remain the action identifiers for compatibility with existing cards and the head. The UI maps **Start session** to `Start it`, **Save for later** to `Add to this work`, and confirmed **New work** to `Add as new work`, with the name in the structured answer's `values.work_title` string (never concatenated into the action). The head accepts that extra field only for task-card New work answers, validates its type and the normal work-title limits, and passes it to `work::keep_todo`. Quotes, colons, backslashes, Unicode and markup-like text are data. Old clients omitting it still use the card title. Do it here and Not now keep their identifiers. For an existing destination the answer accepts `Add to work:<id>`, including on old cards without that stored option. Only task-card decisions accept this dynamic option; the head validates that the work exists, is open and is in the card's realm, which the answering caller must be able to see. A failed save leaves the decision open. No database migration is needed.

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

The click sends your message to the proposing session: `Start session: task card #177 "Remove the dead retry module". session:<new-session-id>` It starts a turn or joins the current one and shows in the transcript when taken, like a typed message. The new session opens beside it (on a phone, you go to the new session). This does not report the new session's progress or result.

New task-card starts label their first message **From a task card**. New button
answers to the proposing session, including Do it here, say **Answer to a task
card**. Both use the neutral sender card with the same Edit and Rewind
eligibility as unknown input, not a guessed human identity. The label applies
only to that message, never to later messages merely because the session has a
`task-card` label. Already stored events retain their current presentation.

### 4.1 The other answers

- **Do it here** sends the card's prompt to the session that proposed it, as a message from you ("Do the task you proposed here … (task card #N: title)"). The message explicitly says the card is already closed and the agent need not close or withdraw it. If the session is in a turn, the message waits for it, as yours do.
- **Save for later** adds the card as a to-do (docs/work.md §12) to the open work, in the card's realm, that holds the session (its root, for a subagent): its title as the text, its prompt as the prompt, `from: decision:<id>`, and the session as its origin. If no work holds the session, it makes new work, as below. The work name follows the live session list; the action always uses the session's current work when clicked.
- **Save elsewhere… → New work → Create work** makes a work item with the confirmed name, initially the card's title, with that one to-do. Renaming the work does not rename the to-do or alter its prompt or origin.
- **Save elsewhere… → an existing work item** files the same to-do, with its prompt and origin, into the chosen open work item, without moving the proposing session.
- **Starting it later:** ▶ Start on the to-do opens the new-session form where the proposing session worked, with the card's prompt (by `work.todo`). The session joins the work and can tick the to-do off.

Start session, the Save answers (including Save for later's new-work fallback), and Not now send text in your voice. The task-card answer's transcript text is exactly what the proposing agent gets:

- `Start session: task card #177 "Remove the dead retry module". session:<new-session-id>`
- `Save for later: task card #177 "Remove the dead retry module" (to-do on "Cleanup").`
- `Not now: task card #177 "Remove the dead retry module".`

The Save text names the actual destination work, including Save elsewhere's destinations. Titles are quoted and newlines escaped, so the message stays one line. It is a real message, not a separate system note. Do it here already sends the task as a task-card answer and gets no extra message. A failed answer sends no message.

Every button sends a real message: it starts a turn if the proposing session is idle and joins a running turn at its next step, exactly as if you typed it. Delivery uses the ordinary durable `SessionCommand.input` queue and worker command-id deduplication. The message appears once in the transcript when taken, and reaches the agent once, including after a head restart, worker reload or restoration of a trimmed session. Until taken it appears in the ordinary input queue, not as a held notice.

The `command: "card-answer"` marker distinguishes these user messages from typed corrections for Watchers; their quoted task titles must not trigger a correction finding. Otherwise they are ordinary user events, including in search, catch-up and the turn outline, with the usual message controls and icons.

Start session includes the new session's id as a clickable `session:<id>` mention in the answer message. After success, the UI opens it beside the current tile; on a phone it goes to the new session. Only card-answer messages gain mention rendering; ordinary user messages remain plain text. A failed answer keeps its error on the card.

No database migration is needed. The head saves `UserInput.task_card_origin`
(fresh protobuf tag 17) in its durable command and stamps
`UserMessage.task_card_origin` (fresh tag 16) when that command's event arrives.
The enum distinguishes start and answer. Older workers ignore the added field
and still receive ordinary inputs with the existing command marker; the head
recovers provenance by session and command id with one lookup per incoming
batch containing command input, even after a restart. No worker
change or capability gate is required. Old commands without metadata and stored
events are not reclassified, including when replayed. An edited pending input
instead records the replacement caller's provenance. The unused quiet-note
command, quiet queued-notice field, capability and synthetic transcript-arrival
conversion remain removed.

An archived or stopped proposer cannot prevent the card's action: the save, dismissal or new session still succeeds, and the head logs why the proposing session could not receive the message.

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
   - each non-waking answer shows immediately as your message, streams at the same journal position as reload, and reaches the next input once, without a new turn, status change, queue count or held-notice pill;
   - consuming or redelivering an answer never adds another transcript line; the UI renders the normal right-aligned user bubble, not a system note;
   - busy and offline delivery, head/worker restarts, journal trimming/restoration, rejected notes and older workers preserve quietness;
   - withdrawing an answered card succeeds as a no-op and reports the answer without exposing another session's cards;
   - Do it here explicitly tells the agent its card is already closed, and a redundant withdrawal returns a successful already-closed tool result;
   - a subagent doesn't get the tool.

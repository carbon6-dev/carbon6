# Task cards — Design

Status: built (2026-10-20). Decided: shown in the session that raised them and in Decisions, under "Suggested tasks", at low priority.

## 1. What it does

While an agent works, it notices things worth doing that don't belong in the current change: dead code, a stale doc, a missing test, a confirmed TODO, a security issue spotted in passing. Instead of doing them, which bloats the change, or forgetting them, it proposes each as a **task card**:

- A title and one or two lines on why now.
- **Start it** begins a new session that does the task: same worker, folder and realm, with a self-contained prompt the agent wrote.
- **Not now** dismisses the card.

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
- `ref: "task:<session>:<id>"`, `options: ["Start it", "Not now"]`;
- `context: {session, prompt, worker, cwd, repo, worktree, labels}`;
- `document`: the why, then the prompt in a collapsed block, so you see exactly what the new session will be told.

Where it shows:

- **The session:** under **Waiting for you** (that panel is already built), as a compact card with Start it and Not now.
- **Decisions:** in the "Suggested tasks" group, low priority, below what waits on you. It sends no notification to your phone, since task cards aren't urgent.

## 4. Starting one

**Start it** creates a session like "Work on it" does today, but from the card's own prompt:

- same worker (or any online one, if it's gone), folder, repo and realm;
- a fresh worktree if `worktree`;
- model by routing, with the labels `task-card` and `from:<session>`;
- title = the card's title.

The card is answered with "Start it" and names the new session ("working session ↗"). Starting one is never automatic.

## 5. Not doing

- Cards between realms: a task card starts in its own session's realm.
- Agents starting tasks themselves: that's what subagents are for.
- Re-raising a dismissed card: dismissed is final for that session. A new session may raise it again.

## 6. Steps

1. **Tools:** `task_propose` and `task_withdraw` in c6-tools, and the head's `session.task_propose` and `session.task_withdraw`, which open and settle the decision with its context.
2. **Prompt:** `session.task_cards`, appended to the base prompt for sessions with the tool.
3. **Start it:** the decision's answer starts the session (`decisions::act` for `task:`), worktree included.
4. **UI:** the compact card in the session's Waiting for you, with the prompt collapsed. Decisions groups it under Suggested tasks.
5. **Tests:**
   - a scripted agent proposes a task: the card appears in the session and in Decisions;
   - Start it creates the session with the card's prompt, worker, folder and realm;
   - withdraw settles the card;
   - a duplicate title raises the same card;
   - a subagent doesn't get the tool.

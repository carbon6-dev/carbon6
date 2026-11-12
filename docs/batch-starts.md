# Starting several tasks together — Design

Status: decided (2026-11-12), not built. Decisions in §7.

## 1. The problem

Task cards (docs/task-cards.md) and work to-dos (docs/work.md §12) start one at a time: **Start session** on a card, ▶ Start on a to-do. Often a few belong together: three cleanups in one module, a test and the doc that describes it, the to-dos of one work item that touch the same files. Started one by one, each session reads the same code again, their branches conflict, and you review several changes where one would do.

The card already has four actions and a fifth control. Batching must not add to it.

## 2. The idea

Nothing changes on a card or a to-do row. Where two or more can be started, the **group's header** gets one quiet link, **Start together…**. It opens a small picker:

- the open cards (or to-dos) of that group, each with a checkbox, none ticked, in the order shown, with each one's title and where it would run;
- one primary button, **Start N in one session**, enabled from two ticked items;
- for cards all from one session, a secondary **Do them here**;
- Cancel.

The picker is a separate dialog. The to-do row's checkbox keeps meaning "done", and selecting never looks like ticking off.

Where the link shows:

| Place | Group | Shown when |
|---|---|---|
| A session's panel | **Suggested tasks** | 2+ open cards |
| Decisions | **Suggested tasks** (all sessions) | 2+ open cards |
| A work item | **To do** | 2+ open to-dos and task cards together |

**Cards and to-dos together.** A work item's picker lists its open to-dos and, below them, the open task cards raised by the sessions it holds. Each card is marked "task card". Starting a batch that includes cards first saves each picked card as a to-do on this work (as Save for later does, answering the card) and then starts them all as to-dos (§3). So every task in the batch is tracked and ticked off the same way. Cards from sessions outside this work still get there by Save elsewhere….

## 3. What starting together does

**Cards → Start N in one session.** One new session, started at once as a single card's is (no form):
- **Where:** the worker, folder, repo and realm the cards share. A fresh worktree if any card asks for one. Labels: `task-card`, and `from:<session>` for each proposing session.
- **Title:** the first card's title, then "+ N more".
- **First message:** the editable prompt `session.task_batch` (Settings → Prompts). By default it lists each card's title and its own prompt, and says:
  - do them in this order;
  - keep each a separate commit where the repo uses commits;
  - report each one's outcome, done or not and why, in one line each.
- **Each card** is answered "Start it" with the new session's id.
- **Each proposing session** gets one message naming all of its cards that went: `Start session: task cards #177 "…", #181 "…". session:<id>`.

**Cards → Do them here.** One message to the proposing session, with all their prompts. Each card is answered "Do it here".

**To-dos (with any cards) → Start N in one session.** As ▶ Start does today, the new-session form opens, joining the work, filled in as for a single to-do. Its first message comes from the editable prompt `work.todos`: the to-dos with their prompts, what the work is and where it stands, and how to tick off each one (`todo_done` per to-do, with its own evidence). The new session becomes each to-do's `session`. Ticking stays per to-do, so an item it doesn't finish stays open.

## 4. Rules that keep it simple

- **Same place only.** Items batch only if they would run in the same realm, worker, folder and repo. The picker greys out the rest, with the reason on hover ("in falcon, the others in carbon6"). No splitting into several sessions behind your back.
- **At most 10 per batch,** and the combined message stays within the to-do prompt limit (20,000 bytes × 10). The picker says when you've hit either.
- **Once and only once:** each card's answer and each to-do's `session` go through the same paths as a single start. The batch is all or nothing: if the session can't be created, no card is answered.

## 5. Not doing

- Selection mode or checkboxes on the cards themselves.
- Batches the agent proposes (`task_propose` stays one task). A session that proposes related cards puts them side by side, where Start together finds them.
- Batch Save for later or Not now: those stay per card (except that a card picked on a work item is saved there as part of its start, §2).
- Batching across workers or repos; starting each in its own session from the picker (that's the existing Start).

## 6. Steps

1. **Head:**
   - `POST /api/v1/decisions/task-batch {ids, how: "start" | "here"}`, which checks the cards are open, share a place and number 2–10, then starts or delivers and answers each card;
   - `GET /api/v1/work/{id}/todos/start?todos=a,b,c`, giving the batch's first message and the session to start from;
   - `GET /api/v1/work/{id}/cards`, the open task cards of the work's sessions, and `POST /api/v1/work/{id}/todos/from-cards {ids}`, which saves them as this work's to-dos (answering each) and returns their to-do ids; the form's start then names those with the others;
   - the catalog ops for both, so the assistant and API have them.
2. **Prompts:** `session.task_batch` and `work.todos`, editable.
3. **UI:** one `StartTogether` picker component, used by the session panel's and Decisions' Suggested tasks and by the work item's To do.
4. **Tests:**
   - starting 3 cards makes one session with the 3 prompts in order; each card is answered once, and each proposing session gets one message;
   - cards in different folders aren't offered together;
   - a failed start answers no card;
   - a batched to-do start marks each to-do's session, and ticking one leaves the others open;
   - Do them here sends one message;
   - a work item's picker lists the open cards of its sessions; a batch with one card and two to-dos saves the card as a to-do (answered once), starts one session for all three, and ticking each works as for any to-do;
   - cancelling the form after picking cards leaves them saved as to-dos, not lost and not re-raised.

## 7. Decisions (2026-11-12)

1. **Cards and to-dos batch together** on a work item (§2). That picker includes the open cards of the work's sessions.
2. **Cards start at once, to-dos open the form,** as their single starts do. A batch with any to-do (including a work item's batch with cards) opens the form, where the combined message can be edited.
3. **Order:** as shown in the picker, to-dos first, then cards. The agent may say why it would change the order, but follows it.
4. **Commits:** the default prompt asks for one commit per task where the repo uses commits. It's an editable prompt, so a repo's own conventions can override it.

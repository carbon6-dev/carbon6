# Session goals — Design

Status: built (2026-11-13).

## 1. What it does

You set a goal on a session: what done looks like ("the parser handles empty input, with docs"). The session then keeps working toward it, turn after turn, without you saying "go on". It stops when:
- the goal is reached;
- it needs you;
- it runs out of turns or time;
- it stops making progress;
- you pause or stop it.

Every turn ends when the agent thinks it's done or needs you. A goal decides what happens next.

## 2. Setting one

- **`/goal <what done looks like>`** in the message box (a built-in command, run by the head unless a custom command takes the name). It sets or replaces the session's goal, with defaults of 20 turns and 8 hours.
- **The API and Ask:**
  - `PUT /api/v1/sessions/{id}/goal` `{text, max_turns?, max_hours?}`, op `sessions.goal.set`;
  - `PATCH …/goal` `{state: paused | active | stopped}`, op `sessions.goal.state`;
  - `DELETE …/goal`, op `sessions.goal.clear`;
  - `GET …/goal`, op `sessions.goal.get`.
- **Top-level sessions only:** not subagents or watchers.
- **An idle session starts at once** with `session.goal_start`. A working one is checked when its turn ends.

## 3. After each turn

The head claims each turn end once (`checked_seq`), then:
1. **Your say first.**
   - A turn you stopped pauses the goal.
   - One that failed puts it at needs you.
   - A turn the worker's restart cut short is left to carry on by itself (docs/restarts.md).
   - If you've queued a message, it goes first, and the turn after it is checked instead.
2. **A check:** one model call with `session.goal_check`, answering JSON `{verdict, why, left}`. It reads the goal, what the last check said was left, and a digest of the turn (the watchers' digest). The model comes from a `task:goal` routing rule, else the session's own connection (when the head can call it), else Ask's.
3. **The verdict:**
   - **achieved** (evidence, not a claim): the goal is reached, and you're notified.
   - **needs_you:** it waits, and you're notified (high).
   - **continue:** the head sends `session.goal_continue` ("Goal check (turn N of M): not reached yet. Left: …").
4. **It stops instead of continuing** (and you're notified) when:
   - two checks in a row say the same thing is left (no progress);
   - its turns are used;
   - its time is up.

Notifications use the event `goal`, which is on by default for notification channels (Settings → Notifications). A pause or a new goal while a check runs wins: the check's result is dropped.

## 4. Seeing it

A strip above the message box, with the goal and where it stands:
- "working toward it · turn 3 of 20", with what is left;
- paused, reached, needs you or stopped, with why.

It has Pause and Stop while active, Resume while paused, needing you or stopped, and ✕ to clear. A reached goal can't be paused or resumed; set a new one. The live session list carries each session's goal (`api::Session.goal`).

## 5. What it is not

- Not a plan or a checklist: work's to-dos are (docs/work.md).
- Not a scheduler: a goal moves only when a turn ends, by sending the session a message. So it works for every connection, Claude Code included.
- Not a hard-coded loop: the three messages are editable prompts (`session.goal_start`, `session.goal_check`, `session.goal_continue`).

## 6. As built

- **Head:** `crates/c6-head/src/goals.rs` (`on_events` from the event ingest), migration `0066_goals.sql`, the routes and catalog ops above, the `goal` notify event, and `watchers::ask_json`/`digest` shared with watchers.
- **UI:** `crates/c6-ui/src/goal.rs` (the strip) and `/goal` in the composer.
- **Tests:**
  - `goals::a_goal_keeps_a_session_going_until_it_is_reached`: reached after a continue, stalled, needs you, out of turns, paused mid-turn with resume carrying what is left, and a reached goal refusing pause and resume;
  - the head's `goals::tests`;
  - the UI's `goal::tests`.
- **Not done:** a Goal field on the new-session form (type `/goal` after starting), and offering "looks done" on the session's work when its goal is reached.

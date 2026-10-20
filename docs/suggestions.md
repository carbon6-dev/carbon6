# Suggested next messages — Design

Status: built (2026-10-20). Decided: a cheap model chosen by routing (`task:suggest`), else the session's own; made only while someone is looking.

## 1. What it does

When a turn finishes and the session is waiting for you, the message box offers what you'd likely type next, as faint text:

- "Run the tests again"
- "Commit this"
- "Yes, use eu-west"

**Tab** accepts it, so it becomes your message, which you can edit or send. Typing anything else replaces it, and **Esc** dismisses it. It's a guess to save typing; carbon6 never sends it on its own.

## 2. When one is made

- **Only for a finished turn.** A turn that ended in an error or was interrupted gets none: what to do next there is yours to decide.
- **Only while you're looking.**
  - The session's page asks for a suggestion when its last turn has ended, its message box is empty, and nothing is queued.
  - A session nobody opens costs nothing.
  - Each turn gets one suggestion at most, cached by the session's position (`last_seq`). Reopening the page or a second tab gets the cached one.
- **Not while something else is asked.** No suggestion while the session shows a card you answer in place: an approval, a question, or a decision under **Waiting for you**. A task card doesn't count, since it asks nothing of you. The answer goes there, not in the box.
- **Off per realm,** in Settings → Agents and tools → **Suggest next messages** (on by default).

## 3. How it's made

- **Input.** The last exchanges of the session, at most about 4,000 tokens, newest kept first:
  - your last message;
  - the agent's final text;
  - the names of the tools it used and whether they failed, but not their output;
  - the session's title and labels.

  Nothing else from the session goes to the model. Secrets are never in the log; tool output is left out.
- **Prompt.** `session.suggest`, an editable prompt (Settings → Prompts). Placeholders:
  - `{{title}}`, `{{labels}}`;
  - `{{exchanges}}`.

  The default asks for one short line in the user's voice, or `-` when there's no clear next step. Then nothing is shown.
- **Model.**
  1. The model-routing rule that applies to the session's labels plus `task:suggest`, in the session's realm (then root's). Add one to send suggestions to a small, fast model.
  2. Without such a rule, the session's own connection and model.

  It runs in the head, one call with no tools, low effort, and at most 1,024 output tokens, so a thinking model has room to reason. The answer is cut to one line. A reply with no text at all counts as a failed call, not as "no next step".
- **Accounting.** The call counts toward the session's usage and budgets: its tokens are a usage row of the session's own, with the suggestion's model. A blocking budget means no suggestion.
- **Clean-up.** The answer is trimmed to one line of at most 200 characters, and quotes and "User:" prefixes are stripped.

## 4. API

- `GET /api/v1/sessions/{id}/suggestion` returns `{text, for_seq}`, made now or cached. It returns 204 when there's none (off, not finished, something asked, `-`, or the call failed). Operation `sessions.suggestion`.
- **Cache:** a table `suggestions` with (session_id PK, for_seq, text, made_at). A newer `last_seq` replaces the row.

## 5. UI

- The message box shows the suggestion as placeholder-styled text inside the textarea, only while it's empty and focused or idle.
- **Tab** fills it in, with the cursor at the end; **Esc** hides it until the next turn.
- It's fetched once when the turn ends (the live stream says so) and the page is visible.
- Two counts are kept, how many were shown and how many accepted, for the leverage measurement later (docs/measurement.md).

## 6. Steps

1. The prompt `session.suggest` with its default, and the realm setting (on).
2. Head: the `suggestions` table, the route with the cache and the conditions in §2, and the model call through routing (`task:suggest`) with usage counted.
3. UI: ghost text, Tab and Esc, fetched on turn end.
4. Tests:
   - a finished turn gets the mock's line;
   - an errored turn, a pending question, and the setting off get none;
   - the cache serves the second request without a model call;
   - a routing rule with `task:suggest` picks its model.

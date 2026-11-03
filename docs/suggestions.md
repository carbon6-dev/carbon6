# Suggested next messages — Design

Status: built (2026-10-20). Decided: a cheap model chosen by routing (`task:suggest`), else the session's own; made only while someone is looking. Session names (§7) built 2026-11-02.

## 1. What it does

When a turn finishes and the session is waiting for you, the message box offers what you'd likely type next, as faint text:

- "Run the tests again"
- "Commit this"
- "Yes, use eu-west"

Tap **Use suggestion** below the box, or press **Tab**, to accept it. It becomes your message, which you can edit or send; accepting it never sends it. Typing anything else replaces it, and **Esc** dismisses it. It's a guess to save typing; carbon6 never sends it on its own.

## 2. When one is made

- **Only for a finished turn.** A turn that ended in an error or was interrupted gets none: what to do next there is yours to decide.
- **Only while you're looking.**
  - The session's page asks for a suggestion when its last turn has ended, its message box is empty, and nothing is queued.
  - A session nobody opens costs nothing.
  - Each turn gets one suggestion at most, cached by the session's position (`last_seq`). Reopening the page or a second tab gets the cached one.
- **Not while something else is asked.** No suggestion while the session shows a card you answer in place: an approval, a question, or a decision under **Waiting for you**. A task card doesn't count, since it asks nothing of you. The answer goes there, not in the box.
- **Off per realm,** in Settings → Suggestions → **Suggest next messages** (on by default).

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

  The default asks for one short line in the user's voice; when the agent's last message asks something or offers to do something, the likely short answer ("yes, deploy"); and `-` only when there's no question, offer or obvious follow-up. Then nothing is shown.
- **Model.**
  1. The model-routing rule that applies to the session's labels plus `task:suggest`, in the session's realm (then root's), whose connection that realm may use (a rule naming another realm's connection is passed over). Add one to send suggestions to a small, fast model; for one realm only, add it in the console under Settings → Model routing → Rules for <realm>.
  2. Without such a rule, the session's own connection and model.

  It runs in the head, one call with no tools, low effort, and at most 1,024 output tokens, so a thinking model has room to reason. The answer is cut to one line. A reply with no text at all counts as a failed call, not as "no next step".
- **Accounting.** The call counts toward the session's usage and budgets: its tokens are a usage row of the session's own, with the suggestion's model. A blocking budget means no suggestion.
- **Clean-up.** The answer is trimmed to one line of at most 200 characters, and quotes and "User:" prefixes are stripped.

## 4. API

- `GET /api/v1/sessions/{id}/suggestion` returns `{text, for_seq}`, made now or cached. It returns 204 when there's none (off, not finished, something asked, `-`, or the call failed). Operation `sessions.suggestion`.
- **Cache:** a table `suggestions` with (session_id PK, for_seq, text, made_at). A newer `last_seq` replaces the row.

## 5. UI

- The message box shows the suggestion as placeholder-styled text inside the textarea, only while it's empty and focused or idle.
- Tap **Use suggestion** (a touch-sized button below the box), or press **Tab**, to fill it in and focus the box with the cursor at the end. Neither sends the message. The button is shown only while a suggestion is available; **Esc** hides it until the next turn.
- It's fetched once when the turn ends (the live stream says so) and the page is visible.
- Two counts are kept, how many were shown and how many accepted, for the leverage measurement later (docs/measurement.md).

## 6. Steps

1. The prompt `session.suggest` with its default, and the realm setting (on).
2. Head: the `suggestions` table, the route with the cache and the conditions in §2, and the model call through routing (`task:suggest`) with usage counted.
3. UI: ghost text, tap-to-accept button, Tab and Esc, fetched on turn end.
4. Tests:
   - a finished turn gets the mock's line;
   - an errored turn, a pending question, and the setting off get none;
   - the cache serves the second request without a model call;
   - a routing rule with `task:suggest` picks its model.

## 7. Session names

A session's title starts as the first line of its first message, cut at 60 characters. Once its first turn finishes, it gets a short name instead ("Realm switching speed").

- **When.** The first turn that finishes (an errored or interrupted one doesn't count) names it, from your first message and the agent's final reply. A first message of three words or fewer ("hi", "continue") says too little: the next finished turn names it instead, from both turns, once. The name is made once and never revised by itself.
- **Silently, with Undo.** The title changes in place: the sidebar, tabs and the session's header follow it live (the head's live list). A session open at the time shows a quiet note for a few seconds, `Named "Realm switching speed"`, with "from the first turn" and **Undo**, which puts the first line back. There's nothing to accept or dismiss.
- **Your names win.** A session you renamed is never named by itself, before or after its first turn, and an Undo counts as your choice. A session started with a title (a template's, a trigger's, `title` in the API) keeps it.
- **Which sessions.** Your top-level sessions and conversations with the assistant. Subagents and side chats keep their titles.
- **Off per realm,** in Settings → Suggestions → **Name sessions** (on by default; `names` in `/api/v1/settings/suggestions`). Off when a session's first turn finishes means it isn't named later either.
- **Model.** A routing rule that applies to the session's labels plus `task:name` (the realm's rules first, then root's, on a connection the realm may use), else the session's own connection and model, exactly as for `task:suggest` (§3). One call with no tools, low effort, at most 1,024 output tokens, counted in the session's usage; a blocking budget means no name. A call that fails leaves the first line.
- **Prompt.** `session.name` (Settings → Prompts), with `{{exchanges}}`: your messages and the agent's final replies of the turns it's named from (each cut at 2,000 characters; never tool output). The answer is cleaned to one line: quotes, a "Title:" label and closing punctuation are dropped, it starts with a capital, and it's cut to 60 characters at a word.
- **Rename's Suggest.** For older sessions (or another try): **Suggest** in the Rename box fills it with a name made the same way, to start from; nothing changes until you save. `GET /api/v1/sessions/{id}/name/suggestion` returns `{title}` (operation `sessions.name.suggest`; 502 when the call fails).

**State without a table.** A new session that may be named has a settings key `session_naming:{id}`: `first` (its first finished turn names it), `next` (its first message had few words) or `naming` (the call is out). Naming it, renaming it (`PATCH /api/v1/sessions/{id}` with a title, which Undo is), a failed call, or the setting being off removes the key; without it, a session is never named by itself. Sessions from before this have none. The new title is written only if the key is still there and the title is still the one it replaces, so a rename while the call is out wins. The session's live stream says `{"type": "named", "title", "from"}` for the note.

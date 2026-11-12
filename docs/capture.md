# Capturing thoughts while a session runs — Design

Status: built (2026-11-12). Decisions in §8, as built in §9.

## 1. The problem

While a session works, you think of things: something to do next, a question to ask it when it's done, a doubt about a line it just wrote. Typing them into the message box sends them to the agent mid-turn, which isn't what you want. Not writing them down loses them.

You need a place for them that's one keystroke away, that doesn't disturb the agent, and that remembers what prompted each thought.

## 2. Two kinds of thought

| Kind | What it is | Where it goes |
|---|---|---|
| **To-do** | Something to do later, probably in another session: "add a test for the retry path" | The enclosing work's To do list (docs/work.md §12), so it can be started, batched (docs/batch-starts.md) and ticked off |
| **Follow-up** | Something to ask or tell this session once it's done: "why did you skip the migration?" | This session's **Up next** list, held until you send it |

A quote of the transcript can come with either kind, linking back to where it came from (§5).

## 3. From the message box

Two commands, in the slash menu like `/export` and `/clear`, and run by the UI unless a custom command took the name:

- **`/todo <text>`** adds a to-do to the session's work. It never goes to the agent.
  - If the session is in no work, it makes new work named after the session, as a task card's Save for later does.
  - The box clears and a brief line confirms it: "To-do added to ‹work›".
- **`/next <text>`** adds a follow-up to this session's Up next. It never goes to the agent until you send it.

Both work while the agent is running, and neither starts or joins a turn. Plain text, without a command, still sends as a message, as today.

## 4. Up next

A thin strip just above the message box, shown only while the session has follow-ups:

- **While running:** collapsed, "Up next · 2".
- **When the turn ends:** it expands and is highlighted, listing the follow-ups.
  - Each has **Send**, which sends it as your message (with its quote, §5), Edit, ✕, and **To-do**, which moves it to the work's to-dos.
  - **Send all** sends them as one message, numbered.
- **Nothing is sent by itself.** You read the result first, then decide.
- **In the session list,** a session with unsent follow-ups shows a small count, so it isn't forgotten.

Follow-ups belong to the session. If the session is archived with any unsent, they become to-dos on its work (marked "follow-up"), so nothing is lost.

## 5. From the transcript: what prompted the thought

Select text in any transcript entry, whether the agent's reply, a tool result or your own message. A small popover appears by the selection, with:

- **Quote:** puts `> quote` into the message box (the box's text is kept).
- **To-do:** opens a one-line input under the popover, seeded with nothing. Enter adds the to-do with the quote attached.
- **Ask next:** the same, adding a follow-up with the quote attached.

Without a selection, each entry's existing ⋯ menu offers the same three for the whole entry. That's the way in on a phone, where selection menus are unreliable.

**The link.** A captured quote keeps `{session, seq, quote}` (the entry's sequence number and the selected text).
- Wherever the to-do or follow-up shows, the quote appears as a muted line, "‘…retries are disabled here…’ ↗".
- Clicking it opens the session at that entry (`View::Session` with `Target { at: seq, q: quote }`, as a search hit does) and flashes the quoted words.
- Sending a follow-up with a quote sends the quote as a block above your words, with a `c6:session:<id>@<seq>` link. The agent sees exactly what you're asking about, and the transcript shows it as a link back.

## 6. Not doing

- **A third kind** ("just a note"). A to-do with no prompt is the note: it waits on the work and you remove it when it's done with.
- **Sending follow-ups automatically** when the turn ends.
- **Capturing into other sessions' or other work's lists** from here: it goes to this session's work, or this session's Up next. Moving a to-do between works stays where it is today.
- **Agent-written follow-ups.** Agents raise task cards; Up next is yours.

## 7. Steps

1. **Head:**
   - **Follow-ups:** a session's follow-ups (text, optional quote `{seq, quote}`, created), with `GET/POST/PATCH/DELETE /api/v1/sessions/{id}/followups` and catalog ops;
   - on archive, they become to-dos on the session's work;
   - `Todo.quote: Option<{session, seq, quote}>` on work to-dos;
   - `POST …/todos` takes it.
2. **Mentions:** `c6:session:<id>@<seq>` links render as a link to the entry, and `assistant::mention_refs` reads them (the entry's text, like other mentions).
3. **UI:**
   - `/todo` and `/next` in the composer's slash handling;
   - the Up next strip;
   - the selection popover and the entry ⋯ items;
   - the quote line on to-dos (work item, Up next) with its link;
   - the session list's follow-up count.
4. **Tests:**
   - `/todo` while a turn runs adds a to-do to the session's work and sends nothing to the worker;
   - `/next` holds a follow-up, the strip expands when the turn ends, and Send delivers it once with its quote;
   - archiving with unsent follow-ups moves them to the work;
   - a quoted to-do's link opens the session at that entry.

## 8. Decisions (2026-11-12)

1. **Commands:** `/todo` and `/next`.
2. **Archiving:** unsent follow-ups become to-dos on the session's work.
3. **Notifications:** a "finished" notification says how many follow-ups are waiting.

## 9. As built (2026-11-12)

- **Head:**
  - Follow-ups are a table (`0065_followups.sql`) with `crate::followups`, served at `/api/v1/sessions/{id}/followups[/send|/{f}|/{f}/todo]` (ops `sessions.followups.*`).
  - `POST /api/v1/sessions/{id}/todos` (`work::session_todo`, op `sessions.todos.add`) adds to the session's work. If it has none, it makes new work named after the session, holding it.
  - `Todo.quote` and `Followup.quote` are `{session, seq, text}`, the text kept to 1,000 characters.
  - `api::Session.followups` counts what's waiting for the session list.
  - A to-do's quote goes into the first message of a session started on it.
- **UI:**
  - `/todo` and `/next` are composer commands (in the slash menu, run by the UI unless a custom command takes the name). Each confirms on the status line under the box.
  - `capture::UpNext` sits above the message box.
  - `capture::SelectionCapture` is the popover for a selection within one entry. `capture::EntryCapture` adds Quote, To-do… and Ask next… to the phone's Message actions.
  - `capture::QuoteLine` shows a quote on a work item's to-dos and in Up next. It opens the session at the entry (`Target.at`, now also in the URL as `&at=`).
- **Different from §5:** a sent follow-up carries its quote as a Markdown quote (`> …`) above your words. It has no `c6:session:<id>@<seq>` link, because your messages show as plain text and the agent already has the transcript. The link back stays on the follow-up and the to-do.
- **Tests:** `batch_capture::notes_while_it_works_and_to_dos_with_cards_start_together` (the head) and `capture::tests::a_quote_links_to_its_entry` (the UI).

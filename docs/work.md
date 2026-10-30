# Work: what you've left open — Design

Status: built (2026-10-29; §11 is what was built). Decided with you:
- every session you start and every request you take on **belongs to a piece of work**: an agent guesses which, and you fix it with one click;
- work is **closed by you**: an agent offers "looks done" when it sees the evidence, and one key closes it;
- work holds sessions, Brief requests and commitments, plain to-dos you type, and outside things (pull requests, CI runs, documents);
- open work **finds you**: a Work panel of its own, a section in the Brief, nudges, and every session's header;
- every item in the header menu **opens a panel of its own** instead of replacing the one you're looking at.

## 1. The problem

You start something in a session. While it runs you start something else, or a message comes in and you begin on that. Each is reasonable alone; over days they pile up, half-finished, and some are forgotten. carbon6 tracks each session, each request and each decision, but nothing ties them to **the thing you meant to get done**, and nothing says whether that thing is finished. A session is too small a unit: one piece of work takes several sessions, a mail thread and a pull request, and one session may serve two pieces of work.

## 2. What a piece of work is

A **work item** is something you intend to finish:

| Field | |
|---|---|
| title | a few words: "Fix the Slack picture sync", "Answer Priya about the contract" |
| done when | one line saying what finished looks like ("merged and released", "she has an answer") |
| attached | sessions, Brief requests and commitments, mail and chat threads, decisions, outside links (PRs, CI runs, docs), notes |
| state | worked out from what's attached (§3), not kept by hand |
| where was I | a short note: what's done, what's left, the next step (§5) |
| realm, labels | as for sessions |
| opened, last moved, closed | times |

Work is flat: no projects, sub-items or dependencies. A label groups work items if you want that. Its steps are a flat checklist of **to-dos** (§12), not nested work.

A **plain to-do** ("renew the domain") is a work item with nothing attached yet. Starting a session from it attaches that session.

## 3. Its state

Worked out from what's attached, in this order (the first that holds):

| State | When |
|---|---|
| **needs you** | a session ended its turn waiting on you; a decision is open; a draft is ready to send; an agent offers "looks done" |
| **moving** | a session is running |
| **waiting on others** | you replied and a commitment says you're waiting; a PR is in review; a CI run is running |
| **gone quiet** | nothing attached has moved for a while (a setting, 3 days to start) |
| **open** | otherwise (a to-do no one's started) |
| **done** / **dropped** | you closed it |

"Gone quiet" is the case this exists for: work you've stopped thinking about but never finished.

## 4. How work is made and attached

Making work must cost nothing while you're busy, or it won't happen.

- **Starting a session**, from the new-session form, a reply box's "Work in a session", a to-do or "New like this": the form has a **Work** field. It defaults to the work you started from (the Work panel, a work item, a session already in some work). Otherwise an agent reads the first message and picks an open work item or proposes a new title. Its guess shows in the session header, and one click changes it.
- **A Brief request** becomes work when you act on it beyond a single reply: you start a session from it, or reply saying you'll come back to it (a commitment "you owe"). It joins the work its conversation already belongs to, if any.
- **An agent in a session** can attach what it makes: a pull request it opened, a CI run, a document. Sessions get a `work` tool (attach, note), not the power to close.
- **You** can make work by hand, attach or detach anything, merge two items, or split one.
- **Unfiled** sessions are fine. A session nobody attached (the guess was "none of these") just stays out of Work.

The guess and its prompt are editable settings (`work.attach`), like every agent behavior.

## 5. Where was I

Each work item carries a note that answers "where was I?" in three parts: what's been done, what's left, and the next step. An agent rewrites it when something attached moves (a session's turn ends, a reply comes in, a PR changes state), and only then. That keeps it cheap. The prompt is an editable setting (`work.summary`).

The note is what makes coming back cheap. **Resume** on an item opens its last session. **Pick up** starts a new session whose first message is the note plus the attached things, with the configuration of the last session.

## 6. Closing

An agent offers **"looks done"** when the evidence says so: the PR merged, the request was answered, the session said it finished and nothing's waiting. The offer puts the item in "needs you", with the evidence. One key closes it; dismissing it keeps it open. Nothing closes without you. You can always close or drop by hand, and reopen.

## 7. Where you see it

- **The Work panel.** It opens like Mail and Chat, as a panel of its own, so the sessions you were looking at stay. Items are grouped by state: needs you, gone quiet, moving, waiting on others, open. Each row has the title, the "where was I" note's next step, what's attached as small chips, and how long since it moved. Keys as in the Brief: J/K, Enter, E (done), H (snooze until tomorrow), # (drop), R (resume).
- **The Brief:** a "Left open" section with the items in needs you and gone quiet.
- **Nudges:** a notification when an item goes quiet, or needs you and you haven't looked for a while. Rules and quiet hours are in Settings, as for the Brief's interruptions.
- **Every session's header:** its work item (or "Not part of any work · Add"), with its state and its other sessions and threads, one click away.

### 7.1 The header menu opens panels of their own

Today a header menu item (Sessions, Decisions, Settings…) replaces the panel you're focused on, and only the Apps menu opens its own. From now on **every header menu item opens in a panel of its own**: the one already showing it, or a new one. So opening Work, Decisions or the Brief never takes away the session you were in. ⌘-click still makes a new panel even when one already shows it.

## 8. How it's built (on the primitives)

Per docs/os.md, no app-specific machinery:

- **Records:** work items are a system collection `work` in each realm, with links to memex things and items (threads, requests, people) and refs to head objects (`session:<id>`, `decision:<id>`, URLs). Its history and optimistic updates come with it.
- **State:** a trigger on session, decision and memex request events recomputes the state of the work items those objects are attached to. "Gone quiet" is a periodic check.
- **Agent steps:** the attach guess, the "where was I" note and "looks done" are model calls with editable prompts, routed like the memex's understanding calls (a cheap model is fine), and shown on the Activity page.
- **Outside things:** a PR or CI run's state is read only when a session already has the means (gh, the forge's MCP) and records it with the `work` tool. carbon6 itself knows no forge or CI (lesson of docs/follow-through.md).
- **The panel** is a view. Its keys and layout are the Brief's.

## 9. Not doing

- No trackers polling outside systems, and no automatic closing. These were the parts of follow-through that didn't land.
- No hierarchy, priorities or due dates to start with. Commitments keep their own due dates and show them.
- No retroactive filing of old sessions beyond a one-time offer: the agent proposes work items for the open sessions and requests of the last two weeks, and you accept or skip each.

## 10. Build order

1. Header menu items open their own panel (§7.1).
2. The `work` collection, attaching by hand, the Work panel, and the session header line.
3. State from attached things, and "gone quiet".
4. The attach guess in the session form and for Brief requests. The `work` tool for sessions.
5. "Where was I", Resume and Pick up.
6. "Looks done", the Brief section, nudges, and the one-time offer for the last two weeks.

## 11. As built

- **The menus (§7.1).** Every header link outside a tile opens in a tile of its own (`How::OwnTile`): the one already showing it, else a new one. ⌘-click still makes a new one.
- **Records.** Work items are records of the system collection `work` in each realm (`api::work::WorkFields`: title, done_when, status, attached, note, offer, snoozed_until, moved_at, told, closed_at). The head writes them itself (`crates/c6-head/src/work.rs`), each change a version in the record's history. The Collections page shows them read-only. `moved_at` and `told` change without a version, because they are not edits. Attached things are refs: `session:<id>`, `request:<id>`, `thread:<account>:<thread>`, `chat:<workspace>:<channel>[:<ts>]`, `decision:<id>` or a URL, each with who attached it (`you`, `guess`, `session:<id>`) and, for outside things, a status (running, waiting, done, failed) and a detail.
- **State** is worked out on every read from what's attached. Sessions come from the head (running; needs_you when a tool call waits for approval or a decision is open on it; failed; idle). Requests come in one `memex.requests.some` call per realm. Decisions come from their table. The order is §3's, with "gone quiet" (nothing moved for `quiet_days`) before an idle session's "your move", so a turn that ended long ago goes quiet. A request still open is "you owe a reply". One snoozed in the Brief is waiting.
- **Events.** The triggers' hooks for sessions (finished, failed, needs approval) and decisions, and the memex's `request.opened` and the new `request.changed` (set, closed, or a new message joined it), mark the work holding that thing as moved and bring its note up to date. A subagent's or side chat's turn moves its root session's work.
- **Agents' steps** call the model a routing rule for `task:work` picks, else the session's own (the latest attached session's for notes). Their tokens are recorded in the realm's usage as `work:<step>`, and they show on the Activity page. The prompts are `work.attach` (the guess), `work.summary` (the note, and `finished` with evidence for "looks done"), `work.pickup` (Pick up's first message) and `work.propose` (finding work in what's open). Notes are written in the background, one at a time per item; a change while one is written writes it again after. A "looks done" you dismissed isn't offered again for the same evidence.
- **Sessions you start** (`POST /api/v1/sessions`, so not templates, triggers or subagents) take `work` (an item's id, `new`, `none` or `guess`) and `work_refs` (what they were started from: a reply box's request, thread or chat). The work already holding a ref is used first. The guess runs only when asked: the new-session form asks by default ("An agent picks"), so reply-box and form sessions are guessed, while API and CLI callers (`c6 run`) opt in with `guess` and otherwise cost no extra model call. Settings → Work can turn the guess off. "Start fresh" carries the old session's work.
- **The `work` tool** lets a session read its work (`get`) and attach what it made (`attach`: a URL with title, status and detail; attaching again updates the status). It also adds and ticks off to-dos (§12). It never closes work.
- **API:** `GET/POST /api/v1/work`, `GET /api/v1/work/of?ref=`, `GET/PATCH /api/v1/work/{id}`, `POST …/attach`, `…/detach`, `…/merge`, `…/refresh`, `GET …/pickup`, `POST /api/v1/work/proposals`, and `GET/PUT /api/v1/settings/work` (per realm: guess, notes, quiet_days, nudge, needs_hours). All of them are in the catalog for the assistant.
- **Nudges.** Every five minutes the head checks open work. It sends a notification (event `work`, on by default) when an item goes quiet, or has needed you for `needs_hours`, once per state until it moves again.
- **UI** (`crates/c6-ui/src/work.rs`):
  - **The Work panel** (`/work`) lists open work by state, with each item's why, next step and attached things. Its keys are J/K, Enter, E, H, # and R. It has "+ Add", "Find work in what's open" (proposals to accept or skip), and closed work behind a checkbox.
  - **A piece of work** (`/work/<id>`) opens beside the list. It shows the note with ↻, what belongs to it with links (sessions, the Brief's request, Decisions, Mail, Chat, URLs), ✕ to take a thing off, and Attach. It has Resume (opens the latest session where it's shown, else in a tile of its own), Pick up (the session form, seeded from the latest session, with `work.pickup`'s first message, joining this work), Done, Tomorrow, Drop, Merge, Reopen, and the "looks done" banner with Close it and Not yet.
  - **Message links** open the original mail/chat conversations in a Messages view (`/conversations/<encoded-ref>`), not the Brief or a generic inbox. A `request:<id>` link resolves its saved sources with `memex.requests.get`, even after it is answered, dropped or snoozed out of the Brief; a request with several sources shows each. The attachment keeps its request ref for work state and filing.
  - **Merged requests** keep durable redirects in each realm's knowledge database (`request_redirects`), including chains. `memex.requests.get` follows them to the surviving request and its messages; `memex.requests.some` gives the survivor's state under each requested id, so old Work attachments and their history need no rewrite. Change events include the old ids and touch Work only in the originating realm. Merges made before redirects existed cannot be reconstructed automatically: the old request was deleted without a forwarding address.
  - **The new-session form** has a Work field: an agent picks, new, none, or an open item. "New like this" picks the session's own work.
  - **Each top-level session's header** shows its work, its state and a "change…" menu.
  - **The Brief** gets a "Left open" section, and "Keep as work" (or the work's name) on each request.
  - **The Work link in the header** counts what needs a look.
  - **Settings → Work** holds the settings above.
- **Not built:** a commitment you make in a reply doesn't make work by itself (use "Keep as work"). Mail threads and chat conversations attached to work have no live state of their own (their request does). The one-time offer is the "Find work in what's open" button, always there rather than offered once.

## 12. To-dos

Status: built (2026-10-30). Each piece of work has a checklist of to-dos: the steps you mean to take, each one a session's worth, to start when you're ready. It stays flat: no nesting, due dates, priorities or assignees, and at most 50 per item.

- **A to-do** (`api::work::Todo`, in the item's `todos`) is a line of text, and optionally a `prompt` (what a session started on it is told, else the text). It notes where it came from (`from`: `decision:<id>` for a task card), the session that raised it (`origin`), the latest session started on it (`session`), who added it, and, once ticked off, when, by whom (`you` or `session:<id>`) and an agent's evidence. A new to-do whose text matches an open one (ignoring case) is that one.
- **Adding them.** You type them on the work item (`POST /api/v1/work/{id}/todos`). A task card's "Add to this work" and "Add as new work" add one with the card's prompt (docs/task-cards.md §4.1). A session adds one with the `work` tool (action `todo`: text, prompt), to the work it belongs to.
- **Starting one.** ▶ Start opens the new-session form, joining this work, filled in from the to-do's origin session (its worker, folder, model and labels), else the work's latest, with the first message from `work.todo` (an editable prompt: the to-do's prompt, what the work is, where it stands, and how to tick it off). `GET /api/v1/work/{id}/todos/{todo}/start` gives that message and the session to start from. A session created with `work` (the item's id) and `todo` is the to-do's `session`, and is named after the to-do unless you named it. The form sends `todo` only while the Work field still names that item.
- **Ticking off.** You tick it (or untick it) on the work item (`PATCH …/todos/{todo}` with `done`). The session's agent ticks it with the `work` tool (action `todo_done`: id and the evidence in one line), as `work.todo` asks. Ticking moves the work and brings its note up to date. Only you remove a to-do (`DELETE …/todos/{todo}`).
- **State.** Open to-dos don't make work need you. When nothing else says what it's doing, it's open, "2 of 5 to-dos left: <the next one>". The note's agent reads the checklist (`[x]` done, `[ ]` left) with what's attached, and no "looks done" is offered while a to-do is left. Pick up lists the checklist too. Merging work joins the checklists.
- **UI.** The work item has a **To do** section between the note and what belongs to it: a checkbox, the text (its prompt on hover), "task card" and "✓ agent" marks, a link to its session, ▶ Start and ✕, and an "Add a to-do…" line. Each row of the Work panel shows the count done of all ("☐ 1/3").
- **The assistant** has `work.todos.add`, `work.todos.edit`, `work.todos.remove` and `work.todos.start`.

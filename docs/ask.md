# Ask as your everyday chat — Design

Status: built (2026-11-02; §6 is what was built). Decided with you:
- **No separate Chat feature.** Ask (docs/assistant.md) already runs in the head with no worker, can do everything the UI can, and reads workers' files and git. It becomes the place to just talk to a model, about anything or about your setup.
- **Conversations you come back to:** a **Chat** list of your Ask conversations, newest first, to continue, rename or archive.
- **Work on a machine waits for its answer:** "run the tests on rust-dev-02 and tell me what fails" starts a subagent there and waits for its report.
- **Any model, per conversation:** an API or ChatGPT connection in Ask's own loop, or **Claude Code**, which the head then runs on its own machine whenever `claude` is on its PATH (§3).
- **Your mail, chat and calendar stay out unless you bring them in:** a switch per conversation, or @-mentioning one thread, person, project or doc.

## 1. Conversations

An Ask conversation is a session the head runs (label `assistant`), in the realm whose page you asked from. Nothing about that changes. What's new is a way back to them:

- **Chat** in the header (in a tile of its own, like the other menu items) lists your conversations: title, model, when it last moved, a dot while it runs. Each opens beside the list; **Rename** and **Archive** use the session operations every session has.
- Opening Chat also opens your latest conversation beside the list, so it reads as "back to where I was".
- **New chat** at the top of the list: the question, the model (an API or ChatGPT connection, or the assistant's default), and the switch for your mail, chat and calendar (§4). ✦ Ask (⌘J) still asks from anywhere with the page you're on as context; its conversations are the same ones and show in the list. (⌘K stays search.)
- A conversation continues like any session: you write in its reply box, and its model can be changed there (Claude Code shows switched off, with why, when the head's machine has no `claude`).
- Reply-box drafts keep their attachments with their text across reloads, using
  the same browser-local, realm-scoped storage and 20 MB total draft-payload cap
  as worker sessions (README, **Drafts survive a reload**). Older attachments may
  be evicted; the box reports anything it couldn't keep. The initial Ask dialogs
  do not accept uploads; this applies to an existing conversation's reply box.

## 2. Work on a machine: delegation that waits

Ask can't run commands itself: it runs in the head, which works on no files. When a question needs a machine, it starts a **remote subagent** (docs/remote-subagents.md) and **waits for its report**, as a worker session's `Task` does:

- **Where it may go is decided by you, beforehand: your templates that agents may start** (Settings → Templates, "Agents may start it"). Each names the worker, folder, worktree, connection and model. The assistant can't make up a worker or folder. That's the same rule remote subagents follow, so one list of approved places serves both.
- Its tool, `c6_delegate` (template, `on` for a template with several places, prompt, description), lists those templates with where they run. With none, it says so and the assistant tells you where to add one.
- The subagent is a child of the conversation: it shows as a thread under the call, its cost counts with the conversation, and Stop stops it too.
- **The call waits** until the subagent's turn ends, then returns its report (or, when it didn't finish, a digest of what it did). A report that comes back after the head restarted, with nobody waiting, starts a turn of the conversation with it, so nothing is lost.

## 3. The model per conversation

New chat takes a connection and model; the reply box's picker switches it later (a `ModelSwitched` in the log, as in every session).

**Claude Code.** On a Claude Code connection the conversation runs the Claude Code CLI **on the head's machine**, with the worker's Claude Code runtime (`c6_worker::claude_code`, through `c6_worker::hosted`):
- **Only Ask's tools.** Claude Code starts with `--tools ""` (none of its own: no shell, no files, no web), `--strict-mcp-config` (no MCP server but carbon6's) and `--disable-slash-commands`, in an empty folder of its own (`<data_dir>/assistant/<id>`). Ask's tools (`c6_discover`, `c6_describe`, `c6_call`, `c6_ask`, `c6_delegate`) are served to it over carbon6's MCP bridge, all loaded up front (with its own tools off there is no tool search).
- **Ask's instructions** (`assistant.base`) go in as its appended system prompt (`--append-system-prompt-file`).
- **Ask's rules hold:** the tools are the same code as in Ask's own loop, so destructive operations still ask first, secrets still go only to you, and operations are admitted as the conversation's realm. Claude Code itself can't reach anything else.
- **When it's there:** `claude` on the head's PATH (or the usual install places, as a worker looks). `GET /api/v1/assistant/claude` says whether; without it the pickers show Claude Code switched off with the reason, and starting or switching a conversation to it is refused. The plan-limit hints show as for any connection.
- **Its process** stays up between turns and closes after 15 idle minutes; the next turn resumes the conversation. A conversation that moves onto Claude Code from another model gives it the conversation so far as context.

## 4. Your mail, chat and calendar

Ask's knowledge scope stays the default (no mail, chat, calendars or photos), as docs/assistant.md §6 decided. Two ways to bring yours in:

- **For a whole conversation:** the switch "Include my mail, chat and calendar" (new chat, and on each conversation in the list) gives it the label `assistant:personal`, which adds those kinds (`email`, `chat`, `event`) to its scope for everything it reads.
- **One item:** @ in the reply box (or New chat) offers, beside the memex's people and things, your projects, docs and the mail threads, chat messages and events that match. A mention is written as a link (`[@title](c6:item:<id>)`, `c6:project:…`, `c6:doc:…`, `c6:work:…`). When the message reaches the conversation, the head reads each mentioned item **with your access** and gives it to the model after your words, marked with where it came from (`<mentioned ref=… kind=…>`). Only that item comes in; the conversation's scope doesn't change.

## 5. Not doing

- Ad-hoc places to delegate to (a worker and folder picked per message) beyond templates: one approval mechanism is enough, and a template is one click to add.
- A separate chat product: the Slack-like Chat page (docs/chat.md) stays about Slack.

## 6. As built (2026-11-02)

- **API:** `POST /api/v1/assistant` takes `connection`, `model` and `personal`; `GET /api/v1/assistant/mentionables?q=&realm=` (operation `sessions.ask.mentionables`) and `GET /api/v1/assistant/claude` (`sessions.ask.claude`). `api::ASSISTANT_PERSONAL` (`assistant:personal`) adds `api::PERSONAL_KINDS` in `memex::scope_for`; Ask's `c6_call` reads the conversation's labels at each call, so the switch applies at once.
- **Delegation:** `c6_delegate` in `c6-head/src/assistant.rs` starts `templates::start_subagent` with the conversation as parent, records `SubagentSpawned` (with `remote`), and waits on the `DelegationEnded` the head sends the conversation (handled by its actor; with nobody waiting, it becomes a `task-notification` turn). From an Ask conversation a template must be one agents may start in its realm (or a grant must allow it), since the conversation has no worker of its own. Conversations in the console (root) can't delegate: sessions don't run in root.
- **Mentions:** `assistant::with_mentions` reads `c6:item:`, `c6:project:`, `c6:work:` and `c6:doc:` links in your message (an item with every kind in scope) and puts them in the input's `prompt` (what the model gets; the log keeps your text and the prompt), at most 8,000 characters each. The reply box of a conversation the head runs offers them after @ (`Suggestion::Ref`, inserted as `[@title](c6:ref)`).
- **Mentions in any session** (2026-11-09):
  - **The picker:** an ordinary session's message box offers the same @ list: projects, docs, work, and the mail threads, chat messages and events that match. It's limited to the kinds the session's knowledge scope allows (`GET /api/v1/assistant/mentionables?session=<id>`, scope from `fabric::session_scope`).
  - **Sending:** when a message or a new session's first prompt mentions something, the head reads each `c6:` link within that scope (`assistant::with_mentions_in`). The result goes to the worker as `UserInput.prompt`, which is what the model reads; the transcript keeps your words.
  - **Outside the scope:** an item of a kind outside the scope isn't read, and the model is told so. Bring chat or mail into a session with Settings → Memex scopes by label.
- **Claude Code:** `c6_worker::hosted::HostedClaude` (one per conversation, in its actor) runs `claude_code::Process` with a `Launch` whose `extra_args` lock it down; the MCP bridge's `Entry.journal` is now an `EventLog` (the worker's journal, or the head's store). `HeadConfig.claude_bin` sets the binary for tests.
- **UI:** the header's **✦ Ask** opens a box to ask from any page (⌘J), with **Your conversations →** to the list, **Ask** (`/ask`; `/chats` still opens it; `chats.rs`; "Ask conversations" in the narrow menu). There is no separate Chat entry: **Chat** is the Slack and messages app (docs/chat.md). The list has New chat (model picker with the plan-limit hints and Claude Code's availability, the personal switch), the list of the realm's conversations (running dot, model, the personal switch, Rename, Archive), and the latest opened beside the list when none is in view. ⌘J (✦ Ask) still asks from any page; ⌘K stays search.
- **Seen working** (scratch head, local qwen3.6 27b): "Run `sh check.sh` in my scratch project on my worker" called `c6_delegate` with the template, the subagent ran it on the worker, and Ask answered with its output. On Claude Code (Haiku, the installed CLI) a conversation called `mcp__carbon6__c6_call` (`system.version`) and answered with the version.
- **Not built:** a thread or event chosen by @ is read whole only as far as `memex.get` gives it (a mail thread's latest message, not every message); Claude Code's own subagents aren't mirrored for Ask (its Task tool is off anyway).

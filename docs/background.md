# Background work and session messages — Design

Status: built (2026-10-31), steps 1–4; §9 lists what each leaves to later.

The main agent stays free to talk while its work runs in the background. A background task that ends always reaches its session, whether the session is mid-turn, idle or reloaded after a restart. Sessions can also message each other within a realm, across workers, fire and forget.

## 1. Where things stand

- **Claude Code ran one process per turn** (before step 1). carbon6 started `claude -p`, wrote the prompt to stdin and closed it. The CLI does not exit at its first `result`. It waits for its background tasks, takes each `task_notification` and starts a turn of its own on it. It exits when none are left, or when `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS` runs out (default 10 minutes), killing the tasks still running. carbon6 read until the process exited, so one carbon6 turn stayed running for as long as any background task did. As a result:
  - Your messages wait in the inbox until the process exits ("inputs that arrive mid-turn are left in the inbox and start the next turn").
  - Esc kills the process group, background tasks included. They are marked lost.
  - A background task that never ends, such as a server, keeps the turn open for good.
- **carbon6's `Task` and `SendMessage` for Claude Code were foreground only** (before step 2), because nothing could deliver their notices.
- **Notices live in memory until a turn takes them.** That covers background-task ends (`task_event`), subagent reports (`ChildReport`), the inbox, `deferred` and the hourly counter `auto_turns`. The head only redelivers your own messages, so a worker restart, or an actor that stops after a failed journal write, drops any notice not yet taken.
- **Held notices wait for your next message.** Over `AUTO_TURNS_PER_HOUR` (20), a notice goes to `deferred`, and nothing retries it when the hour frees up. A fan-out of subagents plus a few shell tasks reaches 20 quickly.
- **Sessions cannot message each other.** `SendMessage` reaches only the caller's own subagents, in process, on the same worker (`subagent.rs:424-452`). The head can put a user message into any session on any worker, durably (`sessions::send_input`). It cannot send a notice, and no tool asks it to message another session.

### What streaming input does (tested 2026-10-30, CLI 2.1.311)

`claude -p --input-format stream-json --output-format stream-json --verbose --replay-user-messages`:

- **The process stays up between turns.** Closing stdin makes it exit 0.
- **Each turn runs from `system/init` to `result`.**
- **It wakes itself.** A background task that ends while the process is idle gets a `task_notification`, and the CLI starts a turn of its own on it. That turn has no echoed user message.
- **A message written mid-turn joins the running turn** at its next model call. With `--replay-user-messages` it is echoed back when the CLI takes it, not when it was written.
- **`{"type":"control_request","request_id":…,"request":{"subtype":"interrupt"}}` stops the running turn.** The CLI answers `control_response` success, then sends `result` with `error_during_execution`. Background tasks keep running and still wake the agent later. A queued message becomes the next turn.
- **The echo carries the `uuid` we sent** (`isReplay: true`). `result` also lists the inputs that turn took (`user_message_uuids`).
- **`set_model` switches the model in place.** It answers with the new model and echoes a local command, with a `uuid` of its own.
- **`stop_task` with the CLI's `task_id` stops one background task.** Its `task_notification` says `stopped`.
- **Closing stdin while a task runs:** the CLI waits for the task, takes its notification in a turn of its own, then exits.
- **An unknown control request** gets `control_response` with `subtype: "error"`.

## 2. Goals

1. Messaging a Claude Code session while it works reaches it at its next step, as in carbon6's own sessions.
2. Esc stops the turn, not its background work.
3. Every background task that asked to notify reaches its session, through restarts and the hourly limit.
4. carbon6's `Task` and `SendMessage` run in the background under Claude Code too.
5. A session can send a message to another session in its realm, on any worker.

Not goals:
- Background tasks that survive a worker restart (shells run detached, then reattached). That is later work.
- Messages across realms (grants). That is later work too.
- Ask-and-wait messages between sessions.

## 3. Words

- **Notice:** input the agent did not get from you: a background task's end, a subagent's report, or a session message. It joins a running turn at its next step. In an idle session it either starts a turn (**wakes** it) or is **held** for your next message.
- **Self-started turn:** a turn the Claude Code CLI starts on its own `task_notification`.
- **Session message:** a notice sent by another session's agent with `message_session`.

## 4. A long-lived Claude Code process

### Lifecycle

- **Start.** A Claude Code session's actor starts its process with the first input that needs it. It passes `--input-format stream-json --replay-user-messages`, plus today's flags (`--resume` or `--session-id`, model, effort, settings and `--mcp-config`). A new conversation's preamble goes in front of its first message, as now.
- **Keep it up** while any of these holds:
  - a turn is running;
  - a native background task is running (`Background::list`, kind `claude_code`);
  - a carbon6 background subagent of the session is running;
  - the last turn ended less than `claude_code_idle_minutes` ago (default 15).
- **Close it** by closing stdin, and kill the process group if it hasn't exited within 10 s. The next input starts a new process with `--resume`.
- **The model is switched in place** with `set_model` at the next turn, so a switch keeps the process and its tasks.
- **Restart it** when what it was started with changes and no background task would be lost: its conversation, its effort or its hooks (`claude_code::Launch`). While tasks run, the old process carries on.
- **A handoff to carbon6's own loop** waits while Claude Code's own tasks run (§8), then stops the process (`Process::stop`).
- **The CLI's own wait limit** (`CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS`) applies only once its input is closed (tested 2026-10-31: with input open, a task outlived a 15-second limit and woke the agent). carbon6 closes it only when idle with no tasks, or when the worker is gone, so the limit is set to 5 seconds: a process whose worker crashed stops its tasks and exits instead of working on unread.
- **The idle time** is the `claude_code_idle_minutes` setting (default 15).

### Inputs

- The actor writes each input to stdin as soon as it has expanded it: commands, mentions and `UserPromptSubmit` hooks run as today. It no longer waits for the turn to end.
- **Inputs that start a turn** are logged with `TurnStarted` when they are written, as before.
- **Inputs written during a turn** wait in an `unconfirmed` list. Each is written with a `uuid`; its echo, or the `result` listing it, confirms it. On confirmation the turn appends that input's `UserMessage`, so the log holds what the model saw, in the order it saw it. The head's command row becomes done then, as now.
- **Inputs that may join a turn** are those `Inbox::join` gives. A `/compact` or a model switch next in line waits for a turn of its own.
- **If the process exits** with inputs it never took, they go back to the front of the inbox for the next process.
- **Images** go to Claude Code as image blocks; before, they were replaced by a note.

### Turns

- **`system/init`** → `TurnStarted`; the session is running.
- **`result`** → `TurnEnded`:
  - `success` → Completed;
  - `error_during_execution` after our interrupt → Interrupted;
  - anything else → Error.
- **A turn with no confirmed input before it** is self-started. Before its `TurnStarted`, the actor appends a `UserMessage` with `command: "task-notification"`. Its text is the summaries of the `task_notification`s seen since the last `result`, so the transcript shows why the agent woke. The UI labels it "woken by bg…".
- **The mapper** keeps its per-process state (calls, mirrored subagents, native tasks) across turns. It closes mirrored subagents at their call's result, as now, and at process exit.
- **Self-started turns** don't count toward the hourly limit. carbon6 can't prevent them without killing the process; it only shows them.

### Stopping

- **Esc** sends the interrupt request. If no `result` arrives within 5 s, the turn falls back to SIGINT, then SIGKILL, of the process group, and marks the native tasks lost.
- **Stopping one task.** The Tasks control's Stop works for Claude Code's tasks too: the worker sends `stop_task` with the CLI's id, and the task reads "stopped by you". This replaces the planned Stop all.
- **Worker restart.** A running process is recorded in `<state>/claude/<conversation>.pid` (its process group and conversation; `Process::record_in`), and the record goes when the process does. At startup, before any session resumes, `recover()` stops every recorded process that is still a `claude` running that conversation (`sweep_orphans`; a reused process id is left alone), SIGINT then SIGKILL. Its native tasks were recorded as started without an end, so the session's reload records them lost, and the agent is told (below). The conversation resumes on the next input. A process dropped without being closed takes its process group with it.
- **Lost Claude Code tasks are told to the agent.** Claude Code delivers its own tasks' notices, except for a task lost with its process (a worker restart, a crash of the CLI, a handoff). Then carbon6 sends the agent a notice naming the task, saying that files it changed stay and that it may need redoing. The notice wakes an idle session, within the hourly limit.

### carbon6's tools and notices

- **The MCP bridge registration lasts as long as the process,** not one turn (built in step 1). Its `Entry` holds the running turn's cancel token, set at each turn's start (`Registration::set_cancel`), so stopping a turn still stops its foreground subagents.
- **`Task` and `SendMessage` can run in the background** (step 2). The bridge also serves `TaskOutput` and `TaskStop` (`mcp__carbon6__…`): carbon6's `bash_output` and `kill_shell`, for carbon6's subagents only, since Claude Code's own tools don't know carbon6's ids. `Task` names `mcp__carbon6__TaskStop` as what stops one. A report reaches the process as described next: it joins a running turn or starts one, and if the process has closed in the meantime, the next one resumes the conversation and takes it.
- **A carbon6 notice for a Claude Code session** (a background subagent's report, a session message) follows the usual rule:
  - written to stdin now if a turn is running;
  - written now if the session is idle and wakes;
  - held otherwise.
  It is wrapped as `<task-notification>` or `<session-message>`, as in carbon6's own sessions.

## 5. Notices that are never lost

### Record before deliver

- When a notice is queued for a session, its actor first appends `NoticeQueued{command_id, text, command, wake}` to that session's log (`Actor::notice`). Then it delivers it: into the inbox, or held (`deliver_notice`). The head stores the event but leaves it out of the transcript, which shows the notice when a turn takes it.
- Its `command_id`:
  - a background task's end: `task:<task_id>:<state>`, so the same end is never recorded twice;
  - a subagent's report: `subagent:<uuid>` (it is recorded in the parent's log before anything else, so it needs no fixed id);
  - a remote subagent's report ([remote-subagents.md](remote-subagents.md)), which the head sends until the parent logs `SubagentReported`: `subagent:<the head's command id>`, so one sent again is not told twice;
  - for a session message (step 4), the head's command id, which also gains `from_session_id`.
- **When a turn takes the notice,** its `UserMessage` has the same `command_id`.
- **On load** (`SessionManager::load`, `notices_of`), every `NoticeQueued` whose `command_id` has no `UserMessage` is delivered again, under the same rule as when it first arrived (without announcing a hold again). That covers worker restarts and actors that stopped mid-turn. A Claude Code session's notice counts as taken once the CLI echoes it.
- **Later (step 4):** the head treats `NoticeQueued{command_id}` as a command applied, so it doesn't resend a session message the worker has already recorded.
- **Waiting notices are shown.** The events API carries `NoticeQueued` (not drawn in the transcript), so the UI knows a notice no turn has taken yet. While the session is idle, a pill above the message box lists them ("2 notices waiting for the agent", each with its first line and sender) with **Deliver now**: `POST /api/v1/sessions/{id}/notices/deliver` sends the worker `ReleaseNotices`, and the held notices start one turn, whatever the hourly limit.
- **Restart order.** `recover()` already writes `lost` for background tasks open at a restart, and those `lost` ends go through the same path.

### The hourly limit

- When a notice is held for the limit, the actor waits until the oldest automatic turn leaves the hour (`retry_at` in the actor's loop). If the session is still idle then, all held notices start **one** turn together (`release_held`). Notices that arrive together already share a turn.
- `auto_turns` is rebuilt on load from the log: the turns of the last hour whose inputs were all notices. So a restart doesn't reset the limit. Claude Code's self-started turns record no `NoticeQueued`, so they don't count.
- The limit is a setting: Settings → Agents → Limits (`subagent-limits`: `auto_turns_per_hour`, default 20; `claude_code_idle_minutes`, default 15), sent in each `CreateSession`. A session keeps the values it started with.
- Held without `wake` (a subagent's report on a turn you started without *Wake the parent*) still means "with your next input", as in subagents.md §6. It is now recorded, so it survives a restart.

## 6. Session messages

### Tools

- **`message_session`** (`MessageSession` in the Claude tool set, `mcp__carbon6__MessageSession` for Claude Code) takes `to` (a session id) and `message`. It returns at once:
  - "sent";
  - "queued: its worker is offline";
  - or why it was refused.
- **`list_sessions`** (`ListSessions`, `mcp__carbon6__ListSessions`) lists the realm's top-level sessions, so the agent can find a target, most recently active first, at most 50, each followed by its side chats ("side chat of *X*"), without the caller. It takes an optional `query` (matched against title, labels and folder) and returns each session's id, title, state (running, idle or offline), worker and labels. Archived sessions are not listed.
- Both are built-in tools in every carbon6 profile (`c6_tools::messages`), so they are valid names in routing rules' tool lists and named agents' tool lists. A Claude Code session gets them over the MCP bridge on any connection, as its tool list allows.
- `message_session` is a separate tool from `SendMessage`. `SendMessage` delegates to the caller's own subagent and may wait for its report; this one only delivers.

### Path

```
message_session ─► HeadCall session.message {session_id, to, message, hops}
                   head: checks (below) ─► an input from that session ─► commands table ─► target's worker
                                                       (durable; redelivered on reconnect)
```

- **Proto.**
  - `UserInput` gains `from_session_id` (10), `from_title` (11) and `hops` (12); `UserMessage` the same fields as 9–11. An input with `from_session_id` is a notice, not your message: no commands, mentions or prompt hooks.
  - `UserMessage` carries the same fields; `NoticeQueued` carries them and the prompt.
- **The target's worker** appends `NoticeQueued` and queues it, with `wake: true` and the hourly limit (§5). The head marks the command done when it ingests that `NoticeQueued`, so a held message isn't sent again on reconnect.
- **The recipient sees** `<session-message from="<id>" title="<title>">…</session-message>`, followed by the realm's `session.message_received` prompt (Settings → Prompts). The prompt says how to reply with `message_session` and to reply only when an answer is needed.
- **The sender is the calling session itself,** a side chat included, so replies go to it. Subagents don't get the tools (`messages::is_message_tool`, as task cards): their head calls go in their top-level session's name, and what they find goes to their parent. Its realm decides what it may reach.

### Head checks

`sessions::message_from`:

- The target exists and is in the sender's realm. Other realms are refused until grants cover messaging.
- The target is not the sender, not archived, not `observed`, not a subagent and not failed. A subagent's turns report to its parent, which would be told about work it never asked for. Side chats may be messaged.
- **Hops.** A session's hop count is that of the last session message delivered to it (`SessionTools::message_hops`); a message it sends carries one more. Your own messages reset it to 0. Beyond `max_message_hops` (default 4) the head refuses, so two agents cannot keep waking each other.
- **Rate.** At most `messages_per_hour` (default 30) per sending session, counted in the head's memory (a head restart starts it again). The text is capped at 20,000 characters.

### UI

- **Recipient:** the message shows as a bubble with "✉ from *title*" above it, which opens that session. Its prompt (with the wrapper) is under the bubble, as for an expanded command.
- **Sender:** the call's card reads "→ sent to *title* ↗" (the title from its result), which opens the session; the message is in the card.
- **Attributed, never yours:** the activity preview ("✉ *title*: …"), a subagent's peek, the Markdown export ("## ✉ From *title*"), search (its own kind, "message from a session"), suggested next messages (not counted as yours) and the memex's reading of the session ("a message from session *title*").

## 7. Settings (no new strings in code)

| Where | Name | Default |
|---|---|---|
| Settings → Agents → Limits (built) | `claude_code_idle_minutes` | 15 |
| Settings → Agents → Limits (built) | `auto_turns_per_hour` | 20 |
| Settings → Agents → Limits (built) | `max_message_hops` | 4 |
| Settings → Agents → Limits (built) | `messages_per_hour` | 30 |
| Settings → Prompts (built) | `session.message_received` | that it comes from another agent, and to reply only when needed |

## 8. Edge cases

- **The target is rewound past a session message.** The message stays in the log, folded with the rewound events. The sender is not told.
- **A Claude Code target before §4 ships.** A session message starts a turn between turns, as head input does today. §4 is built first.
- **The CLI exits on its own** (a crash, or a sign-in that ran out). Unconfirmed inputs go back to the inbox, any open turn ends Error, and the next input starts a new process.
- **A handoff while background tasks run.** A switch away from Claude Code waits while Claude Code's own tasks run (`Actor::handoff_waits`), since they would stop with its process; the log says so once, and messages after the switch wait with it. Claude Code may still wake for its tasks meanwhile. When they end, or you stop them in the Tasks list, the switch goes ahead. carbon6 background tasks continue in either runtime.
- **Two sessions message each other at once.** Each gets the other's message as a notice. The hop count stops a loop of replies.

## 9. Steps

1. **Long-lived Claude Code process (§4 lifecycle, inputs, turns, stopping).** Built 2026-10-30: `claude_code::Process`, `session::claude_turn` and the actor's idle branches. The fake `claude` in the tests reads stream-json from stdin. Tests: one process serves every turn; a message sent mid-turn joins the turn; Esc leaves a background task running and the turn Claude Code starts when it ends is logged as woken. Cleanup after a worker crash, and notices for lost Claude Code tasks, followed on 2026-10-31. (The idle time became a setting in step 3.)
2. **carbon6's notices and background tools under Claude Code (§4).** Built 2026-10-31: bridge registration per process (in step 1); `Task` and `SendMessage` with `run_in_background`, plus `TaskOutput` and `TaskStop`. Test: a background subagent's call returns at once, its report reaches the same process as a message, and that message's turn is logged as a `task-notification`.
3. **Notices that are never lost (§5).** Built 2026-10-31: `NoticeQueued`, re-delivery on load, the limit's timer, and both settings. Tests: a log's pending notices and automatic turns (`notice_tests`); a notice held by the limit survives a worker restart and reaches the agent once, with your next message (`it/notices.rs`). The head treating `NoticeQueued` as applied came with step 4, and the waiting-notices pill with Deliver now on 2026-10-31.
4. **Session messages (§6).** Built 2026-10-31: the proto fields, `session.message` and `session.sessions` in `fabric.rs`, the tools (also over the MCP bridge), the head's checks, the prompt and the recipient's bubble. Test (`it/messages.rs`): a session lists the realm's sessions and messages one on another worker, which wakes with it as a message from that session; its reply, beyond a chain limit of 1, is refused. The sender's card and attribution followed on 2026-10-31. Left for later: grants for other realms.

## 10. Decisions (settled 2026-10-30)

1. Claude Code sessions come first; they are what the main agent mostly runs on.
2. Session messages are fire and forget, within a realm. Grants for other realms are deferred.
3. An idle Claude Code process closes after 15 minutes, never while a background task runs.
4. Esc interrupts only the turn. Background work is stopped task by task (Claude Code's tasks too, with `stop_task`), rather than by the planned Stop all.
5. Session messages get their own tool, `message_session`, rather than a wider `SendMessage`.
6. The CLI's self-started turns don't count toward the hourly limit; carbon6 only shows them.

## 11. Open

Settled 2026-10-31: side chats are listed under their sessions; subagents don't get the messaging tools.

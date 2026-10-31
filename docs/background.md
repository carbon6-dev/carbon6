# Background work and session messages — Design

Status: draft (2026-10-30). Steps 1 and 2 (§4) are built; see §9 for what they leave to later steps.

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
- **A handoff to carbon6's own loop** stops the process at once (`Process::stop`). Its tasks are recorded as lost.
- **The CLI's own wait limit** is set to a day (`CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS`, as `MCP_TOOL_TIMEOUT` is for carbon6's tools), so it never kills tasks itself.
- **The idle time** is fixed at 15 minutes (`claude_code::IDLE`) until the settings of step 3.

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
- **Worker restart (not built yet).** The plan: journal the process group when the process starts; `recover()` kills that group if it is still alive and marks its native tasks lost. Today the process dies with a worker that stops cleanly (`kill_on_drop`). After a crash, its stdin closes, so it ends once its tasks do. Either way, the conversation resumes on the next input.

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

- When a notice is queued for a session, its actor first appends `NoticeQueued{command_id, command, text, from_session_id, wake}` to that session's log. Then it puts the notice in the inbox, or holds it.
- Its `command_id` is fixed by what it is about:
  - `task:<task_id>:<state>`;
  - `subagent:<child>:<turn>`;
  - for a session message, the head's command id.
- **When a turn takes the notice,** its `UserMessage` has the same `command_id`.
- **On load** (`SessionManager::load`), every `NoticeQueued` whose `command_id` has no `UserMessage` is queued again, under the same rule as when it first arrived. That covers worker restarts and actors that stopped mid-turn.
- **The head** treats `NoticeQueued{command_id}` as a command applied, so it doesn't resend a session message the worker has already recorded. The UI shows held notices as a pill above the message box ("2 notices for the agent · Send now").
- **Restart order.** `recover()` already writes `lost` for background tasks open at a restart, and those `lost` ends go through the same path.

### The hourly limit

- When a notice is held for the limit, the actor sets a timer for when the oldest automatic turn leaves the hour. If the session is still idle when the timer fires, all held notices start **one** turn together. Notices that arrive together already share a turn.
- `auto_turns` is rebuilt on load from the log: the `TurnStarted` events that followed a notice in the last hour. So a restart doesn't reset the limit.
- The limit becomes a setting: Settings → Agents, `auto_turns_per_hour`, default 20.
- Held without `wake` (a subagent's report on a turn you started without *Wake the parent*) still means "with your next input", as in subagents.md §6. It is now recorded, so it survives a restart.

## 6. Session messages

### Tools

- **`message_session`** (`MessageSession` in the Claude tool set, `mcp__carbon6__MessageSession` for Claude Code) takes `to` (a session id) and `message`. It returns at once:
  - "sent";
  - "queued: its worker is offline";
  - or why it was refused.
- **`list_sessions`** (`ListSessions`) lists the realm's top-level sessions, so the agent can find a target. It takes an optional `query` (matched against title, labels and workspace) and returns each session's id, title, status (running, idle or offline), worker, labels and last activity. Archived sessions are not listed.
- Both are valid names in routing rules' tool lists and named agents' tool lists.
- `message_session` is a separate tool from `SendMessage`. `SendMessage` delegates to the caller's own subagent and may wait for its report; this one only delivers.

### Path

```
message_session ─► HeadCall session.message {session_id, to, text, hops}
                   head: checks (below) ─► send_input as a notice ─► commands table ─► target's worker
                                                       (durable; redelivered on reconnect)
```

- **Proto.**
  - `UserInput` gains `from_session_id` (9) and `hops` (10). An input with `from_session_id` is a notice, not your message.
  - `UserMessage` and `NoticeQueued` carry both fields.
- **The target's worker** appends `NoticeQueued` and queues it, with `wake: true` and the hourly limit (§5).
- **The recipient sees** `<session-message from="<id>" title="<title>">…</session-message>`, followed by the realm's `session.message_received` prompt (Settings → Prompts). The prompt says how to reply with `message_session` and to reply only when an answer is needed.
- **The sender is the calling session itself,** a side chat or subagent included, so replies go to it. Its realm decides what it may reach.

### Head checks

- The target exists and is in the sender's realm (`realms::of_session`). Other realms are refused until grants cover messaging.
- The target is not the sender, not archived, not `observed`, and not a subagent. A subagent's turns report to its parent, which would be told about work it never asked for.
- **Hops.** A message sent from a turn that a session message started or joined carries that message's `hops + 1`. Your own messages reset it to 0. Beyond `max_message_hops` (default 4) the head refuses, so two agents cannot keep waking each other.
- **Rate.** At most `messages_per_hour` (default 30) per sending session. The text is capped at the inline output limit.

### UI

- **Sender:** the call shows as a card, "→ sent to *title* ↗".
- **Recipient:** the message shows as a bubble "from *title* ↗", styled as a notice, not as yours.
- **Search and the session list** don't count session messages as your input.

## 7. Settings (no new strings in code)

| Where | Name | Default |
|---|---|---|
| Settings → Agents | `claude_code_idle_minutes` | 15 |
| Settings → Agents | `auto_turns_per_hour` | 20 |
| Settings → Agents | `max_message_hops` | 4 |
| Settings → Agents | `messages_per_hour` | 30 |
| Settings → Prompts | `session.message_received` | how to reply, and only when needed |

## 8. Edge cases

- **The target is rewound past a session message.** The message stays in the log, folded with the rewound events. The sender is not told.
- **A Claude Code target before §4 ships.** A session message starts a turn between turns, as head input does today. §4 is built first.
- **The CLI exits on its own** (a crash, or a sign-in that ran out). Unconfirmed inputs go back to the inbox, any open turn ends Error, and the next input starts a new process.
- **A handoff while background tasks run.** Today the handoff stops Claude Code's process, and its tasks are recorded as lost. Later it should wait until the native tasks end, or until you stop them; the UI should say which. carbon6 background tasks continue in either runtime.
- **Two sessions message each other at once.** Each gets the other's message as a notice. The hop count stops a loop of replies.

## 9. Steps

1. **Long-lived Claude Code process (§4 lifecycle, inputs, turns, stopping).** Built 2026-10-30: `claude_code::Process`, `session::claude_turn` and the actor's idle branches. The fake `claude` in the tests reads stream-json from stdin. Tests: one process serves every turn; a message sent mid-turn joins the turn; Esc leaves a background task running and the turn Claude Code starts when it ends is logged as woken. Left for later: cleanup at recovery after a worker crash, and the idle-time setting (step 3).
2. **carbon6's notices and background tools under Claude Code (§4).** Built 2026-10-31: bridge registration per process (in step 1); `Task` and `SendMessage` with `run_in_background`, plus `TaskOutput` and `TaskStop`. Test: a background subagent's call returns at once, its report reaches the same process as a message, and that message's turn is logged as a `task-notification`.
3. **Notices that are never lost (§5).** `NoticeQueued`, re-queue on load, the limit's timer and setting, the head treating it as applied, the held-notices pill.
4. **Session messages (§6).** Proto fields, `session.message` and `session.list` in `fabric.rs`, the tools (including over the MCP bridge), head checks, the prompt and the UI.

## 10. Decisions (settled 2026-10-30)

1. Claude Code sessions come first; they are what the main agent mostly runs on.
2. Session messages are fire and forget, within a realm. Grants for other realms are deferred.
3. An idle Claude Code process closes after 15 minutes, never while a background task runs.
4. Esc interrupts only the turn. Background work is stopped task by task (Claude Code's tasks too, with `stop_task`), rather than by the planned Stop all.
5. Session messages get their own tool, `message_session`, rather than a wider `SendMessage`.
6. The CLI's self-started turns don't count toward the hourly limit; carbon6 only shows them.

## 11. Open

- Should side chats be valid targets? This design allows them: they are top-level enough, and their replies go to the side chat.

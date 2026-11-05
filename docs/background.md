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
- **`{"type":"control_request","request_id":…,"request":{"subtype":"interrupt"}}` stops the running turn.** The CLI answers `control_response` success, then sends `result` with `error_during_execution`. Background commands keep running and still wake the agent later. A queued message becomes the next turn.
- **But an interrupt also ends Claude Code's own background agents** (tested 2026-11-01, CLI 2.1.311). An Agent-tool subagent started with `run_in_background` (`task_type: local_agent`, `is_backgrounded: true`) was still working when an interrupt arrived during a later turn. The CLI then sent `task_updated` with `patch.status: "killed"` and `task_notification` with `status: "stopped"` for it, and for the agent's own command, and `background_tasks_changed` listed no tasks. A background command (`local_bash`) survives an interrupt (tested 2026-10-30). A plain interrupt can't spare them; **Send now** (next) can.
- **Send now interrupts only the main turn** (tested 2026-11-02, CLI 2.1.311). The interrupt request takes more fields: `send_now` (bool), `message_uuid`, `cancel_queued`, `for_user_message_uuids` and `worker_epoch`. With `send_now: true` and `message_uuid` naming a user message already written to stdin (and `cancel_queued` unset), the CLI takes a path that never stops background agents. Its `control_response` says how in `response.send_now`:
  - `"delivering"`: it moves whatever holds the turn (a foreground command, a foreground subagent, an MCP call) to the background, sends `task_updated` with `patch.is_backgrounded: true` for it, and delivers the message into the **same** turn, which ends `success`. Nothing is stopped: the moved command and the background agents finished normally.
  - `"stopped"`: it stops only the current turn (`result` with `error_during_execution`, the foreground command `stopped`), and the message starts the next turn. Background agents run on.
  - `"nothing_waiting"`: the message had been taken already.

  `"delivering"` needs the message to be a person's: `"origin": {"kind": "human"}` on the user line, beside its `uuid`. Without it the answer is `"stopped"`, which still spares the agents. Remote flags gate the path; they default on. An older CLI answers without `send_now` (and may have taken it as a plain interrupt).
- **The echo carries the `uuid` we sent** (`isReplay: true`). `result` also lists the inputs that turn took (`user_message_uuids`).
- **`set_model` switches the model in place.** It answers with the new model and echoes a local command, with a `uuid` of its own.
- **`stop_task` with the CLI's `task_id` stops one background task.** Its `task_notification` says `stopped`.
- **Closing stdin while a task runs:** the CLI waits for the task, takes its notification in a turn of its own, then exits.
- **An unknown control request** gets `control_response` with `subtype: "error"`.

## 2. Goals

1. Messaging a Claude Code session while it works reaches it at its next step, as in carbon6's own sessions.
2. Esc stops the turn, not its background work. (Claude Code's own background agents stop with it, which carbon6 can't prevent, so Stop and Esc ask first while any run; Send now spares them. See §1 and §4.)
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
- **Background work keeps a session showing as running.** A session's displayed status already counts its running side chats and subagents (docs/activity.md, "Session indicators"); it counts its running background tasks too. The head keeps them in `running_tasks` (migration 0059) from the `BackgroundTask` events it ingests, and `Session.tasks_running` counts those that tell the agent when they end plus all of Claude Code's own, which carry no such flag; `Session.servers_running` counts servers and watchers (`notify: false`), which don't keep it running. `display_status` and the finished notification follow `tasks_running`. In the session, a quiet line above the message box ("⏳ 1 task running · 1 server ›") opens the Tasks list, where each task has Inspect and Stop; it never shows the commands. Preview and decisions: doc:01a19626-9a15-7120-9780-560c6f1f6d55.
- **With the agent, not queued.** Once an input reaches the agent while it works (written to the Claude Code process, or pushed into a running carbon6 turn's inbox), the worker logs `InputDelivered{command_id}`. The head sets the command's `with_agent` (migration 0058), and the queue API's `QueuedInput.with_agent` says so. In the UI a queued message is dashed; one with the agent is solid, with **⏵ Send now**. The agent reads input only between steps, so behind a long tool call it waits until that call ends. "Queued" means only that the head holds it (the worker is offline, or it waits for the session's turn to start).
- **Send and Send now.** While the agent works, the message box has one **Send ▾** button. **Send** (Enter, or ⌘/Ctrl+Enter, the only key that sends on a touch screen) gives the message to the agent for its next step. **Send now** (⌘/Ctrl+Shift+Enter, or from ▾) gives it to the agent at once: it is sent with `send_now` (`SendMessage.send_now`, `UserInput.send_now` field 14). **⏵ Send now** on a message already with the agent does the same for it (`POST /api/v1/sessions/{id}/queue/{command_id}/now`: an `Interrupt` with `send_now` set to its command id, sent like any interrupt, not queued). While the agent is idle, a message sent now is an ordinary message.
  - **Claude Code:** the turn writes the message (with its `uuid` and `origin: human`), then an interrupt with `send_now: true` and `message_uuid` (§1; `Process::deliver_now`). "delivering": the turn goes on with the message and isn't marked interrupted; what held it is a background task now (below). "stopped": the turn ends Interrupted, not failed, and the message starts the next. "nothing_waiting": nothing to do. Each is logged. A turn that ends `error_during_execution` before the answer comes counts as stopped too. An answer without `send_now` (an older Claude Code, which may have taken it as Stop) ends the turn Interrupted, and the transcript says so once per process in a quiet notice: its background agents may have stopped too, and updating Claude Code fixes it. Background agents run on in every other case.
  - **carbon6's own loop:** as before, the step stops and the message starts the next turn (an interrupted turn takes no waiting input into itself; `Turn::run`). Its background subagents aren't affected. The head's assistant conversations do the same.
  - **Older workers** (no `send_now` in their capabilities): the head sends an interrupt before the message, the old Send now. On an older worker ⏵ Send now is a plain interrupt.
  - A message sent now that can't join the running turn (it switches the model, or is `/compact`) waits for the turn to end, like Send.
- **Your messages are a person's.** Each message of yours the worker writes to Claude Code carries `"origin": {"kind": "human"}`; carbon6's notices (task-notification, session-message) don't (`claude_code::is_yours`).
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
- **Self-started turns** don't count toward the hourly limit. carbon6 can't prevent them without killing the process; it only shows them, except the one right after you stop the agent, which it stops (Stopping, below).

### Stopping

- **Esc** sends the interrupt request. If no `result` arrives within 5 s, the turn falls back to SIGINT, then SIGKILL, of the process group, and marks the native tasks lost. Background commands survive the interrupt request; Claude Code's own background agents don't: the CLI stops them with the turn, and they're recorded as stopped (§1). Stop sends the same request. Send now doesn't (above), and never falls back to killing the process. carbon6's own background subagents (`mcp__carbon6__Task` with `run_in_background`) run in carbon6's loop and aren't affected.
- **Stop and Esc ask first while Claude Code's background agents run.** A popover by the Stop button says "This also stops N background agents. Stop anyway?", with Stop anyway, Cancel and, when the box has text, **Send at the next step instead** (an ordinary Send). Esc twice (the Stop button reads "Esc" after the first) opens it instead of stopping, and Esc closes it. N counts the session's running Claude Code tasks with `task_type: local_agent` that don't hold the turn: the worker records the CLI's `task_type` from `task_started`, and whether the task is in the foreground (`is_backgrounded: false`), on each `BackgroundTask` (`native_type`, field 9; `foreground`, field 10), which the events API carries (`tasks::background_agents`). A side chat's box doesn't ask (its session is another).
- **A command or subagent the CLI moves to the background** (Send now's "delivering") arrived as a foreground `task_started` (`is_backgrounded: false`). Its `task_updated` with `patch.is_backgrounded: true` records it as a background task: a `BackgroundTask` `started` again with `foreground: false` (the Tasks list keeps when it started; `Background::background_native`). A subagent moved so isn't ended by its call's result, which comes then; it ends with its task (docs/subagents.md §8).
- **What ends because you stopped the agent doesn't wake it.** On 2026-11-02 a Stop (with "This also stops 1 background agent") ended the turn Interrupted, and at once a turn began by itself ("▶ the agent was told", "A background task ended.") in which the agent asked what to do. It was Claude Code's (2.1.311): its transcript shows the notification of a background agent that had finished during the turn (queued, `queue-operation enqueue`), dequeued 3 ms after the interrupted result as a turn of its own (`turnOrigin: task_notification`); the journal holds no `NoticeQueued`, so neither carbon6's wake (`WAKE_GRACE`) nor its lost-task notice played a part. So:
  - **The stop's doing** is what ends or starts from your Stop or Esc (the interrupt request) until 5 seconds after the turn ends (`claude_code::STOP_QUIET`). Claude Code reports what the interrupt stopped as `stopped` or `killed`, and flushes its queue at once, so 5 seconds is ample; a task that *completes* or *fails* ended on its own, and from then on what follows is news again.
  - **A turn Claude Code starts by itself in that time** for what it had queued (nothing of yours waiting, and none of its tasks completed or failed since the stop) is stopped as it begins: the worker sends the interrupt on its `system/init` (`Process::turn`, `quiet`), and the turn takes no input of yours (that waits for a turn of its own). If it ends before the model says anything, as it does in practice, it logs no turn (its number is used again: `Process::take_unshown`); if the model got to say something first (an `assistant` message; Claude Code's own "[Request interrupted by user]" line doesn't count), it shows, ending Interrupted. carbon6 can't keep Claude Code from starting it: the CLI dequeues its queue as soon as the result is out, before anything carbon6 writes could reach it.
  - **The notices it was started for stay in Claude Code's conversation**, so the agent reads them with your next message, and the transcript shows them word for word: each as "↳ the agent was told" (a `UserMessage` with `command: "task-notification"`), then a quiet line, "Stopped before replying" (a `Notice` with `quiet`), saying the agent reads it with your next message. Their text is only in Claude Code's transcript (tested 2026-11-02 with 2.1.311: the stream shows the turn's `system/init` and nothing of what started it; `--replay-user-messages` leaves out task notifications, and `task_notification` carries a summary, not the `<task-notification>` text with its note and result). So the worker reads `$CLAUDE_CONFIG_DIR/projects` (else `~/.claude/projects`), `<cwd with each character but a letter or digit as ->/<conversation>.jsonl` (else any folder's file of that name): the main conversation's `user` messages from a task notification (`origin.kind: task-notification`) after the stopped turn's last message, matched by its `uuid` (the stream's uuids are the transcript's), else written since the turn ended, up to whatever follows them (`claude_code::queued_in`). The CLI writes them before it asks the model; the worker waits up to 2 s for them. It never makes the text up: when they can't be read, the transcript says "Claude Code queued a notice we couldn't read" and names the tasks Claude Code reported ended in the turn you stopped.
  - **Tasks lost with the process** because Claude Code didn't stop within 5 s (so its process was stopped) are told to the agent with your next message (`NoticeQueued` with `wake: false`, held like a report without *Wake the parent*; the waiting-notices pill can deliver them sooner).
  - **Unchanged:** a task that completes or fails after the stop wakes the agent as before (Claude Code's own turn for it shows, and `WAKE_GRACE` still applies); Send now stops no agents and isn't a stop here; carbon6's own background subagents and its own loop are as they were.
- **Stopping one task.** The Tasks control's Stop works for Claude Code's tasks too: the worker sends `stop_task` with the CLI's id, and the task reads "stopped by you". This replaces the planned Stop all.
- **Worker restart.** A running process is recorded in `<state>/claude/<conversation>.pid` (its process group and conversation; `Process::record_in`), and the record goes when the process does. At startup, before any session resumes, `recover()` stops every recorded process that is still a `claude` running that conversation (`sweep_orphans`; a reused process id is left alone), SIGINT then SIGKILL. Its native tasks were recorded as started without an end, so the session's reload records them lost, and the agent is told (below). The conversation resumes on the next input. A process dropped without being closed takes its process group with it.
- **A finished task Claude Code doesn't wake for is told to the agent.** Claude Code usually starts a turn of its own when one of its tasks ends while it waits, but not always: a 20-minute build that ended at 02:12 on 2026-11-02 got its `task_notification` and no turn, and the agent waited until you wrote "continue" (other sessions showed the same, up to 25 minutes). So when one of the session's own tasks completes or fails while no turn runs (not one its subagents own: `task_started.owned_by_subagent`), and Claude Code hasn't started a turn for it within 10 seconds (`claude_code::WAKE_GRACE`), the actor sends the agent a notice naming the task, with its status and output file, as a `task-notification` input. It wakes an idle session within the hourly limit, like the other notices; if Claude Code does wake by itself, nothing is sent.
- **Lost Claude Code tasks are told to the agent.** Claude Code delivers its own tasks' notices, except for a task lost with its process (a worker restart, a crash of the CLI, a handoff). Then carbon6 sends the agent a notice naming the task, saying that files it changed stay and that it may need redoing. The notice wakes an idle session, within the hourly limit, unless the process was stopped because you stopped the agent (above): then it waits for your next message.

### Claude Code's own tools

Built 2026-11-03 (design: carbon6 doc "Claude Code sessions: default-deny tool whitelist"). Calls of carbon6's tools over the bridge are checked against carbon6's policies; calls of Claude Code's own tools are not, and before this a session had all of them: its subagents (`Agent`, `SendMessage`, `ListAgents`, `TaskStop`), web search and fetch, schedules (`Cron*`, `ScheduleWakeup`, `RemoteTrigger`), `Monitor`, `Workflow`, artifacts, notifications, plus every MCP server of your settings, the repository's `.mcp.json` and claude.ai's connectors (mail and Slack among them), and skills, plugins and hooks from settings files. Now a session keeps only Claude Code's **coding tools**, and carbon6's tools come over MCP for the rest.

- **The whitelist** is `Bash`, `Read`, `Write`, `Edit` and `ToolSearch` (`session::CLAUDE_CODE_NATIVE_TOOLS`), narrowed by a routing rule's tool list like carbon6's tools are (any profile's naming: a rule without `shell` gives no `Bash`; `ToolSearch` always, since it only finds carbon6's tools). `session::claude_code_native` gives the list, or none for the escape hatch below; it goes into the process's `Launch` (`native`), so a change starts the process again at its next quiet turn.
- **Default-deny, in four layers** (`claude_code::Process::spawn`):
  1. `--tools <the list>` and `--disable-slash-commands`: Claude Code's other tools, skills and commands are never offered to the model.
  2. `--strict-mcp-config` with carbon6's `--mcp-config`, and `ENABLE_CLAUDEAI_MCP_SERVERS=false`: no MCP server but carbon6's.
  3. `--setting-sources ""`: no settings file (user, project or local), so neither your settings nor a repository's bring hooks, plugins, skills, agents or MCP servers. What carbon6 needs comes in its own `--settings` (`session::claude_code_settings`): the extra settings below, the head's hooks, and the built-in plugins `telemetry` and `plugin-authoring` turned off (`enabledPlugins`; `agents-md` stays, since it gives a project without CLAUDE.md its AGENTS.md, and carbon6 leaves project instructions to Claude Code). Auto-memory and CLAUDE.md still load (tested 2026-11-03).
  4. A `PreToolUse` hook on every tool (matcher `*`, first in `--settings`) runs `c6w claude-guard <the list>` (`claude_code::guard`): a call of a tool on the list or of carbon6's (`mcp__carbon6__*`) runs; any other is refused with exit 2 and a reason the model sees. Claude Code lets a call through when a hook fails, so the guard refuses whatever it cannot read rather than failing open. It catches what gets past 1–3: a tool a later Claude Code adds, or a server that connects mid-session.
- **Extra Claude Code settings** (Settings → Agents → Tools in Claude Code sessions; `settings` in the setting, `ClaudeCodeToolChoice.settings_json`, field 6): a JSON object merged into carbon6's `--settings`, in either mode, since with coding tools only Claude Code reads no settings files of its own. The default, when none are set, is `{"includeCoAuthoredBy": false, "gitAttribution": false}`: no Claude co-author or attribution lines in commits and pull requests (`c6_api::CLAUDE_CODE_SETTINGS_DEFAULT`); an empty object means none. Keys carbon6 sets itself or that would undo the lockdown are refused on save and dropped by the worker (`c6_api::CLAUDE_CODE_SETTINGS_PROTECTED`: `hooks`, `disableAllHooks`, `allowManagedHooksOnly`, `enabledPlugins`, `extraKnownMarketplaces`, `env`, `mcpServers` and the project MCP server switches).
- **The startup check.** The first `system/init` of each process (`Process::check_init`) is logged (version, tools, MCP servers, plugins). If a locked-down process has anything beyond its list, carbon6's tools, carbon6's server and `agents-md`, the transcript says so once (a quiet `Notice`): a sign that a Claude Code no longer honours a flag.
- **Web.** With Claude Code's own web tools off, carbon6's come over MCP: `WebFetch` always, `WebSearch` where a search provider is set up (Settings → web search) and, for Brave and Tavily, the session has its key secret (`session::claude_code_extras`). Calls go through carbon6's policies.
- **The escape hatch, for now:** Settings → Agents → "Tools in Claude Code sessions" → Claude Code's own tools → **All, as without carbon6** (`native_all` in the setting and in `ClaudeCodeToolChoice`, field 5): none of the above, as before, except that carbon6 now always passes `--settings` with the extra settings (and the head's hooks). It travels with every message like the rest of the setting. To be removed once coding tools only has been verified in use.
- **Not a sandbox.** `Bash` can still reach the network, run scripts or another agent CLI; this limits which tools the model is offered and which calls go through carbon6, not what a shell command can do.
- **Older workers** ignore field 5 and the whitelist: their sessions keep all of Claude Code's tools until the worker is updated. An older head sends no `native_all`, so a new worker locks down.

### carbon6's tools and notices

- **Which of carbon6's tools Claude Code gets** is a setting: Settings → Agents → "Tools in Claude Code sessions" (`claude-code-tools`, per realm). Since 2026-11-02 it is **automatic** by default: every carbon6 tool the session has in carbon6's own loop, without those Claude Code keeps its own of (files, shell, plan, skills, its own background tasks; web too when it keeps all its tools): the subagent tools (`Task`, `SendMessage`, `TaskOutput`, `TaskStop`), `doc`, `MessageSession`, `ListSessions`, `PublishFile`, `Remember`, `Forget`, `Notify`, `TodoAdd`, decisions, task cards, kits, templates, apps, `Work`, series and records, the API reference, and the memex, mail, calendar and chat tools where the session has them (`session::claude_code_tool_names`). **Chosen by hand** offers only those named (any profile's naming; an empty choice means none); a list saved before automatic existed reads as chosen by hand. A routing rule's tool list narrows either, except the subagent tools and `doc`, which it never narrowed. The choice travels as `ClaudeCodeToolChoice` (`automatic`, `tools`, `core`, `core_chosen`, `native_all`) in `CreateSession.claude_code` (field 35) and in **every message's** `UserInput.claude_code` (13), so a running session takes a change up with its next message; the head also fills fields 33 and 34 (automatic: every tool) for older workers.
- **Some tools are loaded up front; the rest wait behind Claude Code's tool search.** Claude Code defers MCP tools: it shows the model their names and loads one when the model searches for it, which keeps each turn's prompt small. The bridge marks the **core set** with `_meta: {"anthropic/alwaysLoad": true}` in `tools/list` (Claude Code 2.1.314 honours it per tool), so those are in the prompt from the start. The core set is part of the setting ("up front" in its UI); by default `Task`, `SendMessage`, `doc`, `Work`, `Remember`, `Forget`, `TaskPropose` and `TaskWithdraw` (`session::CLAUDE_CODE_CORE_TOOLS`). carbon6 doesn't turn tool search off (`ENABLE_TOOL_SEARCH`): loading every tool would add tens of thousands of tokens to each turn's context.
- **carbon6's guidance is in Claude Code's system prompt** (`--append-system-prompt`), where carbon6's loop has it, instead of only the MCP server's instructions (which Claude Code weighs less; they keep one line about the tools). It is the editable prompt **`session.claude_code`** (Settings → Prompts: that the tools are `mcp__carbon6__<Name>`, what they cover, and to find those not loaded with tool search), then the guidance for the tools the session got: the memory section with your notes with `Remember` or `Forget`, the task-card guidance (`session.task_cards`) with both task-card tools, the memex brief with the memex tools (`session::claude_code_system_prompt`). It stays the same for the life of a process, so it is cached.
- **A running session takes up changes at its next quiet turn.** What a process was started with is its `Launch`: its conversation, effort, hooks (`--settings`), the system prompt above, the bridge's tools with those up front, Claude Code's own tools it keeps, and the worker's build. When a turn needs a different one (a change of the setting, of your memory notes or of the prompts, or a new worker), the process is closed and started again with `--resume`, so the conversation continues, and the transcript says "Claude Code started again" (a `Notice` with `quiet`: shown, never sent as a notification). Never mid-turn, and never while the process has background tasks or agents of its own running (a restart would end them): it keeps the old settings until a turn starts with nothing running (`Process::idle_until`).
- **carbon6's policies apply to the bridge.** Claude Code runs the head's command hooks itself (`--settings`), and they see `mcp__carbon6__*` calls too, but it doesn't know carbon6's policies (Settings → Policies). So the bridge checks each call against the session's policies before running it (`Policies::check`), under both the tool's own name (`doc`) and the name Claude Code knows it by (`mcp__carbon6__doc`). A deny rule refuses the call; an ask rule holds it, logged as an approval at the Claude Code call it belongs to, until you answer in the UI (an interrupt ends the wait). Tools exposed over the bridge from now on get this for free.
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
- **Waiting notices are shown.** The events API carries `NoticeQueued` (not drawn in the transcript), so the UI knows a notice no turn has taken yet. While the session is idle, a pill above the message box lists them ("2 notices waiting for the agent", each with its first line and sender). When the hourly limit holds a notice, its `deliver_at_ms` gives the next free slot; the pill says "delivers itself at 21:14" in your local time. Older workers/log entries without that field show no delivery time. **Deliver now**: `POST /api/v1/sessions/{id}/notices/deliver` sends the worker `ReleaseNotices`, and the held notices start one turn, whatever the hourly limit.
- **Restart order.** `recover()` already writes `lost` for background tasks open at a restart, and those `lost` ends go through the same path.

### The hourly limit

- When a notice is held for the limit, the actor waits until the oldest automatic turn leaves the hour (`retry_at` in the actor's loop). At exactly one hour that slot is free. If the session is still idle then, all held notices start **one** turn together (`release_held`). Notices that arrive together already share a turn, including a new notice that arrives just as a slot frees up. If a restored log counts more turns than the limit (for example after **Deliver now**), the timer waits until enough turns expire, rather than retrying a past deadline.
- `auto_turns` is rebuilt on load from the log: the turns of the last hour whose inputs were all notices. So a restart doesn't reset the limit. Claude Code's self-started turns record no `NoticeQueued`, so they don't count.
- Startup restores actors for cached sessions with undelivered waking notices active within the last **24 hours**, even if they have no unfinished background tasks. The journal filters `sessions.last_activity` in SQL (`NOTICE_RECOVERY_WINDOW_MS`), so older sessions' logs aren't read and days-old notices don't cause an upgrade burst. Older notices wait for your next message or **Deliver now**; using the session again loads its actor and re-arms its timer as usual. A recovered log rebuilds both the held batch and its timer; no user message or head command is needed to re-arm delivery in a recent session. If the slot is already free, automatic delivery waits for the head connection (and thus model credentials), rather than consuming the notices in a failed offline turn. Before this recovery step, the timer existed but a session with only held notices could remain unloaded after a restart.
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
| Settings → Agents → Tools in Claude Code sessions (built) | `claude-code-tools.native_all` | off: Claude Code keeps only its coding tools (§4) |

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
4. Esc interrupts only the turn. Background work is stopped task by task (Claude Code's tasks too, with `stop_task`), rather than by the planned Stop all. Exception, found 2026-11-01: the CLI's interrupt also stops its own background agents (§1). Follow-ups: ask before an interrupt while background agents run, which needs the native `task_type` recorded (built 2026-11-02, with Send now as the CLI's `send_now` interrupt, which spares them: §4); and steer long background work to carbon6's subagents.
5. Session messages get their own tool, `message_session`, rather than a wider `SendMessage`.
6. The CLI's self-started turns don't count toward the hourly limit; carbon6 only shows them.

## 11. Open

Settled 2026-10-31: side chats are listed under their sessions; subagents don't get the messaging tools.

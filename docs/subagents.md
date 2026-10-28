# Subagents — Design

A subagent becomes a session like any other: it has an actor, a log, a place in its parent's transcript you can watch live, and a reply box. Stopping one, or losing it to a worker restart, leaves its work in place. The agent that started it, or you, can read that work and continue the same subagent with its history instead of starting over. Subagents can start subagents of their own.

## 1. Where things stand

What exists today (native sessions):

- **The log is kept.** `Task` → `subagent::Runner::run` creates a child session in the worker journal (`SessionStarted.parent_session_id` / `parent_call_id`). Its events sync like any session's. The head creates its row on the first event (`store::create_subagent_session`) in the parent's realm, with the parent's labels. The child's cost counts toward the parent, one level deep (`usage::cost_of`).
- **But it is not an actor.** The child's turn runs inside the parent's tool call, with an in-memory `Conversation` and one turn. It has no inbox and gets no `Checkpoint`. Once that call returns, nothing can run it again.
- **The head refuses it as a session.** Messages ("run once"), rewinds, side chats and `Restore` are all refused for `parent_call_id` sessions.
- **The UI only links to it.** The Task card has an *open subagent* link, which shows up once the child's row exists. The link opens the child in its own column, with no reply box. Nothing in the parent's transcript shows the child's progress.
- **Interrupts lose the thread.** Stopping the parent turn cancels the child. `run_turn` ends `Interrupted`, the runner returns `Err("ended without a report")`, and the parent's tool result is a bare error. Only a successful report carries `[subagent session <id>]`. So after an interrupt, the parent model can't point to the work that was done.
- **Restarts mislabel the work.** `recover()` closes the child's open turn (`WorkerRestarted`), so its log survives. The parent's background notice still says "lost … its output is gone", which isn't true.
- **The parent log has no record of the child.** `SubagentSpawned` is listed in architecture §5 but was never built. The only link is the child's `SessionStarted`, so the parent can't show a child before the head has ingested it.
- **No nesting.** A subagent's tools are its parent's minus `Task`.
- **Claude Code sessions drop subagents.** Stream messages with a `parent_tool_use_id` are thrown away (`claude_code.rs`).

## 2. Goals

1. You can watch a subagent from the parent session, live, inline, the way side chats work.
2. A stopped, failed or lost subagent leaves its partial work readable by you and by its parent agent.
3. The parent agent or you can continue the same subagent with its history: to resume it after an interrupt, to send a finished one a follow-up, or to steer one that is running.
4. Subagents can start subagents, within limits you set.
5. Claude Code sessions show their subagents too, read-only.

Not a goal: rewinding a subagent.

## 3. Words

- **Subagent:** a child session started by a tool call, rather than by you branching off (that is a side chat). It is still `parent_id` plus `parent_call_id`. Its parent may itself be a subagent. The **main agent** is the top-level session at the root of the tree.
- **Delegation:** one request that starts a turn in a subagent. It comes from a `Task` call (the first one), a `SendMessage` call from its parent, or a message from you. Each delegation causes one turn.
- **Report:** the final assistant text of a delegation's turn, if that turn completed.
- **Outcome:** how a delegation's turn ended: `completed`, `interrupted`, `failed` or `lost` (the existing `TurnEndReason`).
- **Waiting call:** a foreground `Task` or `SendMessage` call in the parent that has not returned yet. It waits on exactly one delegation.

## 4. Data model

### Worker: the subagent is an actor

`Runner::run` no longer runs the loop itself. It asks the session manager for a child actor and talks to it like any other session:

1. **Create.** Create the child with a `CreateSession` that carries a new `Delegation` field: parent session, parent call, the named agent (its prompt, allowed tools and model, as `agent_json`) and `depth` (the parent's plus one; the main agent is 0). The journal stores this spec, so a reload rebuilds the same tools: `build_tools`, filtered by the agent, with the agent's system prompt and model. `Task` and `SendMessage` are included while `depth < max_subagent_depth` (§7). That fixes the problem that today's journaled spec is the parent's.
2. **Send input.** Send the prompt as `PendingInput`. It now has a `delegated_by` call id and an optional `on_turn_end: oneshot<Outcome>`.
3. **Wait or don't.** Foreground: wait for the oneshot. Background: register the task as today and let the oneshot finish it.
4. **Cascade interrupts.** If the parent's cancel token fires, the runner sends `Interrupt` to the child actor; it does not just drop a future. A subagent interrupted this way interrupts its own foreground subagents the same way, so a stop runs down the tree.

The child then gets everything an actor already has for free: inbox delivery at model boundaries, reload from the journal, history repair for tool calls that never got a result, `Restore`, and its own subagent runner. Nesting therefore needs no separate code path. It takes no `Checkpoint`s: subagents are not rewound, and the parent's checkpoints cover the same folder.

### Events

| Event | Log | New fields | Purpose |
|---|---|---|---|
| `SubagentSpawned` | parent | `call_id`, `child_session_id`, `agent`, `description`, `background_task_id` | Written before the child starts. The parent's log now links to its children itself, so the UI can show a thread at once and recovery knows which calls were delegations. |
| `UserMessage` | child | `+ delegated_by` (a parent call id, or empty when you sent it), `+ wake` | Each turn of the child is tied to its delegation. The UI marks turns "from its parent" or "from you"; `wake` is what you chose for reporting back (§6). |
| `SessionStarted` | child | `+ agent`, `+ depth` | Shown in the UI; used again on reload. |

No new turn-end reasons are needed.

### Head

- **Migration:** `sessions.agent` (the named agent, `''` for general-purpose), `sessions.end_reason` (the last `TurnEnded` reason, set at ingest), `sessions.root_id` (the top-level session, set at creation, `NULL` for a top-level one; migration 0040 fills it in for existing side chats and subagents) and `sessions.observed` (§8). With these, thread headers and the subagents strip (§9) can show states without loading each child's log, and whole trees can be queried without recursion.
- **`GET /sessions/{id}/children`** returns these columns, plus the `delegated_by` call of each child turn. `?tree=1` returns the whole subtree by `root_id`.
- **Lift the refusals.** Messages to subagent sessions are allowed (§6). Side chats on subagents are allowed too; they're seeded from the child's log as usual. `Restore` is allowed, so a trimmed child can still be resumed. Rewind stays refused, since it would change what the parent was already told.
- **Trees, not one level.** Everything that follows `parent_id` one step today follows the whole tree:
  - cost (`usage::cost_of`), so a session's cost includes every level below it;
  - find in session, which today covers side chats only;
  - search's `open=` chain, which can now mix side chats and subagents (a side chat on a subagent of a subagent);
  - archiving, where a parent archives its whole subtree.
- **Journal trimming** drops a child only once its root is trimmed.

## 5. Lifecycle

```
          Task / SendMessage call, or your message
                        │
         ┌──────────► running ◄───────────────┐
         │              │                     │  continue: SendMessage from
         │      ┌───────┼────────┬─────────┐  │  its parent, or your reply /
         │      ▼       ▼        ▼         ▼  │  Resume
         │  completed interrupted failed  lost┘
         │      │   (you or parent (error) (worker restarted)
         │      │    stopped it)
         │      └── report → waiting call, or notice to its parent
         │
   new delegation
```

A subagent is idle between delegations, like any session. Its state in the UI is its status (`running` or idle) plus `end_reason`.

### What the parent gets

- **Completed (foreground):** the report, as today, with `[subagent <id>]` always appended.
- **Anything else (foreground):** a tool result that isn't a bare error. It carries the outcome, the subagent's id, a digest of its partial work, and how to continue. For example:

  ```
  <subagent id="0199…" status="interrupted" by="user" turns="1" tool_calls="14" elapsed="2m 10s">
  Last message: "Found the three call sites; now updating parser.rs…"
  Changed: crates/c6-head/src/parser.rs, crates/c6-head/src/store.rs
  Errors: 1 (cargo check: unresolved import)
  Subagents: 0199…a2 (interrupted), 0199…b7 (completed)
  </subagent>
  ```

  Then comes the line saying how to continue, which is an editable prompt (§10). The digest is built from the child's events:
  - the last assistant text;
  - its write and edit tool calls (exact paths, not a git diff, so the parent's and siblings' edits don't show up);
  - failed calls;
  - the subagents it started and their states.

  It is mechanism output, not instructions.
- **Background:** the same text, in the `task-notification` notice.
- **Lost (worker restart):** the existing `lost` handling changes in two ways.
  - Background tasks say the log is kept and can be resumed (no more "its output is gone").
  - When an actor reloads, any `SubagentSpawned` whose call has no `ToolResult` gets one written before its open turn is closed, with the digest and `status="lost"`. Each level does this for its own calls. The model then sees the id, not the generic repair placeholder.

### Interrupts

| You do | Foreground subagents | Background subagents |
|---|---|---|
| Stop the **main** turn (Esc) | Interrupted too, all the way down. Each one's partial result is recorded as its call's result. | Keep running (their own token, as today). |
| Stop a **subagent** (its thread's Stop) | Its turn ends, and so do its own foreground subagents. The waiting call returns `interrupted`, and the parent carries on and decides what to do. | Same as the kill tool or `StopTask`; the notice says `stopped_by_user`. |
| **Kill tool** from the parent model | n/a | Interrupts the child's turn; its log is kept and it can be resumed. |

Stopping one subagent is the main new control: you can stop one that has gone off track without throwing away its parent's turn.

## 6. Continuing a subagent

### From its parent: `SendMessage`

A new tool, `SendMessage` in the Claude tool set and `send_message` in the others, with `to` (a subagent id) and `message`:

- **The subagent is idle:** the message starts a new delegation, and its whole conversation continues with the same tools, agent and model. It also keeps the same cache key, so continuing it soon after reuses the prompt cache. The call waits for the report, unless `run_in_background` is set (then it is tracked like a background `Task`).
- **The subagent is running in the background:** the message goes into its inbox and arrives at its next model boundary. The call returns at once. This is how a parent steers its own background subagents.
- **Allowed targets:** an agent can message only its own children (`parent_id` = caller). Messaging any other session is refused.

`bash_output` / `BashOutput` take a subagent id (a background task id or a session id). They return the digest above plus any new events since the last read, so a parent can check on a background subagent without waiting for it.

Why a separate tool and not a `resume` parameter on `Task`:

- `Task` stays "start one"; its `subagent_type` and `description` would mean nothing on a resume.
- A separate tool can steer a running subagent, which a `Task` resume would have to refuse.
- Weaker models misuse overloaded schemas less often.
- Current Claude Code continues agents with a separate `SendMessage` tool, so Claude models expect this shape.

### From you

A subagent's thread (§9) has a reply box and, after an interrupted, failed or lost turn, a **Resume** button. Resume sends the resume prompt.

- **While a call is waiting on the subagent,** your message goes into the subagent's inbox. It arrives at the next model boundary, so you are steering it, and the parent keeps waiting for the report.
- **When no call is waiting,** your message starts a new delegation that you own. When that turn ends, its report goes to the subagent's parent as a `task-notification` notice:
  - **If the parent is running,** the notice arrives at its next model boundary. It is acting, so it needs to know what changed.
  - **If the parent is idle,** the notice waits and arrives with the parent's next input. It doesn't start a turn by itself.
  - **With *Wake the parent* ticked,** an idle parent starts a turn on the notice right away. This counts toward the hourly auto-turn limit.

  The parent never acts on stale information, and nothing runs behind your back unless you ask for it. To only *ask* a subagent something, use a side chat on it (↳ Ask); side chats don't report back.
- **In a tree,** the rule repeats one level at a time. A report from a woken parent goes to its own parent under the same rule, and `wake` carries up only as far as you ticked it (stored on the `UserMessage`).

### History after an interrupt

This comes from `history_from_events`. A tool call that was cut off gets its repaired "interrupted" result, so the model sees exactly where it stopped.

## 7. Nesting

- **Limits (settings):**
  - `max_subagent_depth`: the main agent is depth 0, its subagents depth 1, and so on.
  - `max_running_subagents` per tree: the number running at once below one top-level session.

  At the depth limit, the subagent isn't given `Task` or `SendMessage`. Over the running limit, `Task` returns an error saying how many are running. A named agent whose tool list leaves out `Task` can't nest, whatever its depth.
- **Inheritance:**
  - Every level gets the root's tool hooks (not Stop), secrets, publisher, memory and notice sinks, as `subagent::Parent` does now.
  - Every level stays in the root's realm and keeps its labels.
  - The routing rule's `subagent_model` applies at every depth (a named agent's model still wins).
- **Who talks to whom:**
  - Reports and notices go up one level.
  - `SendMessage` goes down one level.
  - You can reach any level from the UI.
- **Auto-turn chains.** A report bubbling up through idle subagents could set off a chain of turns. That only happens with *Wake* ticked; the per-session hourly auto-turn limit then applies at each level.

## 8. Claude Code sessions

The worker turns `parent_tool_use_id` traffic into an **observed** child session instead of dropping it. On the first message for a `Task` tool_use, it creates a child (`SessionStarted.parent_call_id` = that tool_use id, title from its description) and appends assistant and tool-result events to it. Nested ones nest the same way, by their own `parent_tool_use_id`.

The UI shows it as a read-only thread: no reply box, no Resume, no Stop. Claude Code owns that subagent's lifecycle, and its own agent can continue it. The head sets `sessions.observed`, so it refuses messages to these sessions.

## 9. UI

### The subagent thread

A `Task` call's card turns into a thread under the call, like a side chat but in violet:

```
● Task  Find all callers of parse_spec   (code-explorer · haiku-4.5)   ⏸ interrupted · 14 calls · 2m10s · $0.04   ↗
  │ ▸ Read crates/c6-head/src/store.rs
  │ ▸ Grep "parse_spec("  → 3 files
  │ ● Task  Check the tests   (general · haiku-4.5)  ✓ done · 6 calls      ← a nested thread
  │ Found the three call sites; now updating parser.rs…
  │ ▸ Edit parser.rs  ✕ cargo check failed
  │ ─ interrupted by you ─
  │ [ Resume ]  [ Reply to the subagent…                      ]  ☐ Wake the parent
```

- **Header:** the named agent, the description, the model, a state pill (● running, ✓ done, ⏸ interrupted, ✕ failed, ⚠ lost), tool calls, elapsed time and cost (its subtree included). ↗ opens the subagent in its own column.
- **Live tail while it runs:** a foreground subagent opens showing its last few events (a "peek"). Clicking the header expands the full transcript; the open/closed choice is saved per thread, as for side chats. Tool calls start collapsed. The prompt isn't repeated, since the Task card above already shows it.
- **When it completes,** the thread collapses to the header and the report shows as the call's result. Turns are labelled by delegation: "from its parent (this call)", "continued by its parent ↗ call", "from you".
- **A `SendMessage` call** gets a small card ("↻ continued *Find all callers…*", or "→ sent to *…* while running") that scrolls to the subagent's thread. The new turn appears there, so one subagent stays in one place.
- **Nested subagents** are threads inside threads. Beyond two levels of indentation, a nested subagent shows as a one-line row (header only) that opens in its own column, so deep trees don't squeeze the page.
- **Controls:** Stop while it runs (it stops the subagent's own foreground subagents too); Resume after a turn that did not complete; the reply box at any time; ↳ Ask (a side chat on the subagent) on its messages. Observed (Claude Code) threads have none of these.

### The subagents strip

This answers the open item in architecture §17 about rendering a session with many subagents. Next to `RunningTasks`, the session header gets a **Subagents** pill with counts for the whole tree (`3 running · 5 done · 1 interrupted`). It opens the tree: an indented list showing each subagent's state, agent, description, elapsed time and cost. Clicking one scrolls to its thread, expanding the threads above it. Several `Task` calls in one message show as a stack of compact one-line threads, so a fan-out of 8 doesn't push the conversation off screen.

### A subagent's own page

The breadcrumb shows the chain, for example "*main session* › *Find all callers…* › *Check the tests*". Each step jumps to the call that started the next. The page has the reply box, with the same rules as the thread's, and its own subagents strip for its subtree.

## 10. Settings (no new strings in code)

**Prompts** go in Settings → Prompts, with shipped defaults, next to `session.subagent`:

- `session.subagent_resume`: what the subagent is told on Resume ("You were stopped partway through. Your work so far is above. Continue the task…").
- `session.subagent_partial`: the line after the digest in the parent's tool result (how to continue with `SendMessage`, or that it can start fresh).
- The compaction prompts gain a line asking to keep subagent ids that may need continuing. This edits the existing editable prompts, not code.

**Limits** go in Settings → General: `max_subagent_depth` and `max_running_subagents`.

## 11. Edge cases

- **The parent rewinds past a Task call.** The subagent stays (the logs are append-only), folded with the rewound events. Only you can continue it, since the parent no longer knows about it.
- **Edits run at the same time.** Subagents still share the root's work tree, at any depth. Nothing changes here, but the digest lists only the subagent's own edits.
- **`SendMessage` to a subagent the caller is waiting on.** This can't happen, because a waiting caller isn't running. Two calls in one message that target the same idle subagent are refused for the second one ("already running").

## 12. Steps

As built: all six steps on 2026-10-14.

- A background subagent's session is found through `SubagentSpawned.background_task_id`, so `BackgroundTask` gained no field.
- A reloaded or restored subagent keeps the tools its `SessionStarted` lists, so `Delegation.agent_json` is informational.
- Without its parent on the worker (both were trimmed), a restored subagent runs with hooks of its own.
- An agent may message a subagent that has a `SubagentSpawned` in its own log, by session id or background task id.
- Resume is `POST /sessions/{id}/resume`: it sends the realm's `session.subagent_resume` prompt as a message with `command: "resume"`, and only after a turn that was interrupted, failed or lost.
- The limits are the head's `subagent-limits` setting (Settings → Agents), sent in each spec as `max_subagent_depth` and `max_running_subagents`. The running limit counts delegations a parent waits on or runs in the background (not your own messages to subagents) per top-level session, on the worker.
- Tree-wide on the head: cost (`usage::cost_of`, and the usage report's per-session rows), archiving, `GET /sessions/{id}/children?tree=1`, find in session (a subagent's matches sort at its `SubagentSpawned` event) and search links (`OpenIn.side_chats` lists side chats and subagents to expand).
- The UI learns that a subagent started or stopped working, without an event in the parent, from `Session.subagents_running` in the live session list (counted per tree by `root_id`); transcripts and the ◆ list refresh their children when it changes. A collapsed thread follows the subagent's stream only while it runs.
- Claude Code's subagents: `claude_code::Mapper` creates a mirror session the first time a message carries a `parent_tool_use_id`, under the session (or mirrored subagent) whose call it is, and logs `SubagentSpawned` there. It records the subagent's task, whole messages (not partial deltas) and tool results, and ends its single turn when the call's result arrives, or as interrupted (or failed) if the process stops first. Its spec has `Delegation.observed`; the head sets `sessions.observed` (migration 0041) from its `claude_code` profile and refuses messages, Resume and restores for it.
- Threads show the subagent's `SessionStarted` line (its tools and prompt matter for a named agent) and skip nothing else; the peek shows the last three steps. A subagent uses its parent's *live* tool hooks and policies, so calls held for approval wait in the parent as before; `SessionManager` keeps each actor's `SessionTools` for this and loads the parent first when a subagent is loaded after a restart. Secrets go by the root session (`Delegation.root_session_id`), since a new child's row may not be on the head yet.

1. **Actor-backed subagents.** `Delegation` in `CreateSession`, `SubagentSpawned`, the child built and reloaded as an actor, interrupts cascaded through the actor. Depth limit 1, so visible behavior stays as today. Tested by killing the worker mid-subagent and reloading.
2. **Partial results.** The digest; foreground and background results for every outcome; recovery writing results for dangling delegations; the corrected `lost` text; the head columns.
3. **Continuing.** `SendMessage`, `bash_output` for subagents, the head accepting input (with `wake`), `Restore` and side chats for children, the prompts.
4. **Nesting.** `Task` and `SendMessage` for children under the limits, tree-wide cost, find in session, search chains and archiving.
5. **UI.** Threads with live peek, nested threads, Stop, Resume, the reply box, the subagents strip, the breadcrumb.
6. **Claude Code observed children.**

## 13. Decisions (settled 2026-10-14)

1. **Stopping the main turn** stops its foreground subagents, all the way down.
2. **Messaging a subagent that a call is waiting on** steers it; the parent keeps waiting.
3. **Reporting back:** your follow-ups always report to the parent. A running parent hears at its next step; an idle one hears with its next input, unless you tick *Wake the parent*.
4. **Continuing:** a separate `SendMessage` tool rather than `resume` on `Task`.
5. **Claude Code subagents** are shown read-only.
6. **Subagents can start subagents.**
7. **Default limits:** `max_subagent_depth` = 3 and `max_running_subagents` = 16 per tree.

## 14. Where a subagent runs

A subagent may run on another connection and model than its parent: a cheap model for searches, or a GPT model under a Claude session. carbon6 runs it in its own loop whatever the parent runs on (never by starting another CLI), so it works for every main model, Claude Code included.

### Choosing

The connection, model and effort come from the first of these that says, most specific first:

1. **The `Task` call:** `connection` (a name or id from the list in the tool's description), `model` (a model id, or `connection/model`) and `effort`. The model sets them when you ask ("use a subagent on my blacktitan ChatGPT connection with gpt-6.1-sol at high"). A connection without a model runs its default model.
2. **A named agent's `model`:** an alias (`opus`, `sonnet`, `haiku`) on an Anthropic connection, or `connection/model` on any.
3. **The session's default for subagents:** picked in the session (the **Subagents** row under the message box; `PUT /sessions/{id}/subagent-model`), else the routing rule's (**Subagent connection**, **Subagent model**, **Subagent effort**).
4. **The parent's own model.**

The effort carries over only while the subagent stays on the same connection. Its own subagents inherit the session's default.

### What may be used

The head sends each session the connections its realm may use (`CreateSession.subagent_connections_json`), without Claude Code ones, since running a Claude Code connection would start the CLI. The worker refuses anything else with an error that lists the available connections. The parent's own connection is always allowed, unless it is Claude Code. A worker gets a connection's credentials only if some realm it serves may use that connection.

### Tools and prompt

Where the subagent's model takes the parent's tool profile, it shares the parent's tools as before. Otherwise (a GPT subagent under a Claude parent, or any subagent of a Claude Code session) it gets tools of its own in its model's profile, with the same filters: the named agent's tools, the depth limit, no task cards. Its system prompt is the parent's with the skills index renamed for the profile. Under Claude Code, whose prompt is its own, it gets the prompt a carbon6 session would have. `SessionStarted.tool_profile`, `tools` and `context_window` are the subagent's own, so a reload rebuilds the same set.

### Records

`SubagentSpawned.model` and the child's session row (`connection_id`, `model`, `effort`) name where it runs, so the thread header shows "connection · model" and its cost is priced on its own connection. Changing the session's default writes `SubagentModelSet` (applied with its command id, replayed on reload) and updates `sessions.subagent_connection_id`, `subagent_model` and `subagent_effort`. A change made while a turn runs applies to that turn's next `Task` call.

### Claude Code sessions

Claude Code never sees carbon6's tools, so during each of its turns the worker serves `Task` and `SendMessage` to it as a local MCP server: streamable HTTP on 127.0.0.1, behind a secret path that lasts for one turn, passed with `--mcp-config`. Claude Code knows them as `mcp__carbon6__Task` and `mcp__carbon6__SendMessage`. They run foreground only, because nothing could deliver a background notice to Claude Code. The subagent shows as a thread under the call (matched by tool name and arguments in the log), and stopping the turn stops it. Claude Code's own `Task` subagents are still mirrored read-only (§8). A subagent of a Claude Code session gets the head's hooks and policies itself, since carbon6 runs none for its parent.

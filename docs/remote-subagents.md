# Remote subagents — Design

A session on one worker can start a session somewhere else and treat it as its subagent. "Somewhere else" can be another worker, or another folder, repo or connection on the same worker. The config comes from a template you made beforehand: worker, folder, worktree, connection, model, effort and labels, the same choices as the new-session dialog. The parent waits for the child's report, or lets it run in the background. It can continue the child with `SendMessage`, and you can watch the child as a thread in the parent's transcript. This is the subagent model of docs/subagents.md, stretched across workers, with the head connecting the two ends.

## 1. Where things stand

Two existing pieces each cover half of this:

- **Subagents** (docs/subagents.md) have the relationship: `SubagentSpawned`, waiting calls, reports, partial digests, `SendMessage`, interrupts that cascade, threads, tree-wide cost. But a subagent is an actor on its **parent's worker**, in the parent's work tree, with the parent's tools and hooks. §14 lets it use another connection and model, never another machine or folder.
- **`template_start`** (docs/packages.md §6, `c6-tools/src/templates.rs` → `fabric::handle` `session.template_start` → `templates::start`) has the placement. A session can start a template, and with a `workers`/`start` grant (`grants::may_reach`) that template can run on other workers, hosts or SSH targets. But it is fire-and-forget. The tool returns session ids. The new session gets only a `started-by:<id>` label, with no parent link, no report back, no `SendMessage`, no thread and no shared stop. Claude Code sessions don't get it at all: the MCP bridge serves only `Task`, `SendMessage` and `doc`.

Templates (`c6_api::Template`) already hold most of the dialog's choices: `workers`, `ssh`, `hosts`, `cwd`, `connection`, `model`, `effort`, `labels`, `prompt`. They're missing `worktree`, a title and a way to mark a template for use by agents.

## 2. Goals

1. An agent starts a pre-made template as a subagent on whatever worker the template names, and gets its report back as if it were a local `Task`.
2. It works from any session on any worker, Claude Code sessions included (through `mcp__carbon6__Task`).
3. Everything a subagent has keeps working across workers: foreground and background runs, reports, partial digests on interrupt/failure/loss, `SendMessage` to continue or steer, your reply box and Resume, Stop cascading down, the violet thread and ◆ strip, tree-wide cost.
4. Either worker can restart or go offline without losing the link. A report that can't be delivered waits on the head.
5. You decide ahead of time what agents may reach: which templates, and so which workers and folders. An agent can't make up a worker and folder.

Not goals (v1): an agent choosing a worker or folder freely (no template), moving files between the two machines, fleet fan-out as one subagent call (`template_start` still does fleets).

## 3. Words

- **Remote subagent:** a subagent whose session the head created from a template, rather than one the parent's worker created as an actor. It's "remote" even when the template puts it on the same worker. What matters is that it's a full session of its own, with its own folder, config and tools.
- **Subagent template:** a template with **Agents may start it** ticked (§4).
- **Link:** the head's record that a parent call is waiting on, or owns, a remote child (§5).

## 4. Templates

New `Template` fields, editable in **Settings → Templates** and through `settings.templates.put`:

| Field | Meaning |
|---|---|
| `worktree: bool` | Run in a new git worktree of `cwd` on a new `c6/…` branch, as the dialog's checkbox does. |
| `agents: bool` | **Agents may start it.** The template shows up in `Task`'s list for sessions in its realm (§6). Off: only `template_start`, runs, triggers and automations can use it. |
| `title: String` | Optional title for its sessions; `{{input}}`-style fields allowed. Empty: the template's name, as today. |

**Prompt.** The `Task` call's `prompt` fills `{{input}}`. If the template's prompt is empty, the call's prompt is the first message. A template prompt with no `{{input}}` gets the call's prompt after it, separated by a blank line. That way the template can carry standing instructions ("You work in the infra repo on mumuzz; run `make plan`, never `apply`") and each call adds the task.

**Targets.** A subagent template must resolve to **one** session. One worker, one SSH target or one host works as is. A template with several targets (`*`, a list, host labels) is allowed only if the call picks one with `on` (§6). Otherwise the call is refused with the list of choices.

**"New template from this session".** The session details page already has "New session like this one". Next to it goes **Save as template**, which opens the template editor filled from the session: worker, folder, worktree, connection, model, effort, labels. This makes "pre-create a template" a one-click step from a session that's already set up the way you want.

### Permission

Ticking **Agents may start it** is your decision, made in Settings, so it acts as the grant. Saving a template with `agents: true` that names workers, hosts or SSH targets also writes the `workers`/`start` grant for that template in its realm, and unticking it revokes that grant. `templates::start`'s own-worker rule stays as it is for everything else. An agent can still ask for a grant with `grant_request` for a template you didn't tick, and the template only appears in its list once you allow it.

Cross-realm templates follow `template_start`'s rules (exported by the realm's app, plus a grant). The report goes back to the caller, as a template operation's result already does (docs/apps.md §7).

## 5. Data model

### The parent's worker

`Task` takes a new `template` argument (§6). When it's set, the subagent runner doesn't create a local actor. Instead it:

1. writes `SubagentSpawned` with a new `remote` field (`worker`, `template`, `target`) before anything starts, like a local spawn;
2. calls the head with `session.subagent_start` (`session_id`, `call_id`, `template`, `on`, `prompt`, `description`, `background`, the parent's `depth` and root);
3. foreground: waits on a oneshot keyed by `call_id`. Background: registers a background task as today, and the same oneshot finishes it;
4. on its cancel token firing, calls `session.subagent_interrupt` (`call_id`) instead of interrupting a local actor.

The oneshot is fulfilled by a new head→worker `SessionCommand`, **`DelegationEnded`** (`call_id`, `child_session_id`, outcome, report, digest). The runner turns it into the same tool result or `task-notification` a local subagent produces. The text is identical, so models and prompts need no changes.

`SendMessage` and `bash_output` look up the target's `SubagentSpawned`. If it's `remote`, they call `session.subagent_message` / `session.subagent_peek` on the head instead of a local actor.

### The head

- **`session.subagent_start`** (in `fabric::handle`, next to `template_start`): resolves the template as `templates::start` does (one target), checks `agents` or a grant, then checks the depth and running limits for the tree (§7). It creates the session through `sessions::create_as` with three new `CreateSession` fields: `parent` (`session_id`, `call_id`), `root` and `depth`. The child row gets `parent_id`, `parent_call_id`, `root_id` and `agent = "template:<name>"`, so everything that already follows trees (cost, archiving, `children?tree=1`, find in session, search links) treats it as a subagent. The new worker gets a `Delegation` with `remote: true`. It runs as a normal session with **its own** tools, hooks, skills and MCP servers from its realm and labels, not its parent's. It replies at once with the child's id. Waiting happens through the link, not by holding the head call open for hours.
- **`delegations` table** (migration): `child_id`, `parent_id`, `call_id`, `background`, `state` (`waiting` | `owned` | `ended`), `pending_report` (JSON or null), `delivered_at`. This is the link.
- **At ingest**, a `TurnEnded` in a child with a link builds the outcome. For `completed` that's the report; otherwise it's the digest of docs/subagents.md §5. The head builds the digest from the child's events, which it already has, so the digest code moves from `c6-worker` into a shared crate. The head then sends `DelegationEnded` to the parent's worker. If that worker is offline, or the command is rejected, it stays `pending_report` and is sent again when the worker says `Hello`. Delivery is acked, so it's at-least-once, and the runner ignores a `call_id` it has already resolved.
- **Turns you start** in the child (reply box, Resume) mark the link `owned` by you, following docs/subagents.md §6. When such a turn ends, the head sends the report as a `task-notification` notice to the parent, with `wake` as you ticked it. It's the same rule, carried by the head instead of the worker.
- **`session.subagent_interrupt`** sends `Interrupt` to the child's worker. **`session.subagent_message`** is the existing message path with `delegated_by` set, which steers the child if it's running or starts a delegation if it's idle. Only the child's parent may call these, the same rule as `SendMessage` today.

### Events

| Event | Log | New fields |
|---|---|---|
| `SubagentSpawned` | parent | `remote { worker_id, worker_name, template, target, cwd }` |
| `SessionStarted` | child | `Delegation.remote = true` (it's a full session; reload doesn't rebuild the parent's tools) |
| `UserMessage` | child | `delegated_by` as today; for remote turns it's set by the head |

## 6. The tool

`Task` gets two arguments:

- `template`: the name of a subagent template from the list in the tool's description;
- `on`: which of the template's workers, hosts or SSH targets, when it names several.

With `template` set, `subagent_type`, `connection`, `model` and `effort` are refused, because the template fixes them. Why extend `Task` rather than add a tool? Starting a subagent is what `Task` means, `SendMessage` already continues it, and this way Claude Code sessions get it for free through `mcp__carbon6__Task`. The weaker-model argument from docs/subagents.md §6 is about a tool that can both start and resume. Adding a placement choice to "start" is a different case.

**The list.** The head sends the realm's subagent templates in the spec (`CreateSession.subagent_templates_json`, like `subagent_connections_json`): name, description, where it runs (`mumuzz:~/src/infra`, worktree or not), connection·model, and the choices for `on`. The tool's description lists them. A session keeps its list, like its connections. A template that was edited or deleted since then is resolved fresh at call time, and the error lists the current ones.

`template_start` stays as it is, for fire-and-forget and fleets.

**Claude Code parents** stay foreground only (docs/subagents.md §14), so a Claude Code session waits for its remote child within its turn. Native parents may use `run_in_background`.

**Claude Code children** are allowed. Unlike local subagents, a remote child is a real session, so a template may name a Claude Code connection: "run Claude Code in the repo on the build box". Its report is the final assistant text of its turn.

## 7. Limits and inheritance

- **Depth** carries over: the child's `depth` is the parent's plus one, and at `max_subagent_depth` the child gets no `Task`/`SendMessage`, local or remote.
- **Running limit** per tree: local subagents are counted on their worker today. A remote child is counted on the head, which knows each tree's running sessions (`subagents_running`, by `root_id`). `session.subagent_start` refuses over the limit with the count. Local spawns on a worker also ask the head for the tree's remote count, so the limit covers both kinds.
- **Realm and labels:** the child runs in the template's realm (normally the parent's). Its labels are the template's, plus `template:<name>`, `run:<id>` and `started-by:<parent>`. Routing rules and label policies apply to it as to any session. It does **not** get the parent's labels, because its labels describe its own place (repo, worker).
- **Approvals** are the child's own. A call held for approval shows as needs-you on the child, and the parent's thread header shows "waiting on you". This differs from local subagents, whose approvals wait in the parent, because the child has its own policies.
- **Budgets and cost:** the child's cost is priced on its own connection and rolls up through `root_id` as today.

## 8. Lifecycle across machines

| What happens | Result |
|---|---|
| Child completes | The head sends `DelegationEnded` with the report, and the waiting call returns it with `[subagent <id>]`. |
| You or the parent stop the parent's turn | The runner calls `subagent_interrupt` for each foreground remote child, and the head interrupts it. The child's digest comes back as the call's result, as with a local child. Background children keep running. |
| You stop the child (its thread's Stop) | Its turn ends `interrupted`, and the parent's call returns the digest. |
| **The child's worker** restarts | The child's open turn closes `lost` (`recover()`), and the head relays the `lost` digest. Its log is kept, so Resume or `SendMessage` continues it once the worker is back. |
| The child's worker is offline at start | `subagent_start` fails at once ("worker mumuzz is offline"), with any other targets the template has. No queueing in v1. |
| **The parent's worker** restarts | On reload, a `SubagentSpawned { remote }` with no `ToolResult` gets one: "still running on mumuzz; its report will arrive as a notice", with the id. The link becomes `background`, and the report arrives later as a `task-notification` notice. The child keeps working, unlike a local child, which a restart kills. |
| The parent's worker is offline when the child ends | The report waits in `pending_report` and is delivered at `Hello`. |
| The parent rewinds past the call | As in docs/subagents.md §11: the child stays and only you can continue it. Its link is marked `owned`. |
| The parent is archived | Archiving the tree archives the child, as `root_id` already does. A running child is interrupted first. |

## 9. UI

Mostly unchanged, because the thread is driven by `SubagentSpawned` and child rows, and the head streams any session to the browser whatever worker it's on.

- **Thread header:** a remote child shows its place in place of the agent name: `▣ template · mumuzz:~/src/infra (worktree c6/1a2b…) · sonnet-5.5`. ↗ opens it in its own tile.
- **States:** besides the usual pills, "waiting on you" (a held approval in the child) and "report pending" (the child ended while the parent's worker was offline).
- **◆ strip and breadcrumb:** remote children are listed with their worker.
- **Templates page:** the **Agents may start it** checkbox, the `worktree` checkbox, and the template's subagent runs listed under its run history.
- **Session details:** **Save as template** (§4).

## 10. Settings (no new strings in code)

- `session.subagent_partial` and `session.subagent_resume` are reused unchanged.
- **New prompt** `session.subagent_remote_reattach`: the tool result written after the parent's worker restarts (§8).
- The "Agents may start it" templates list is shown in Task's description through a template in **Settings → Prompts** (`session.subagent_templates`), so its wording can be edited too.

## 11. Steps

1. **Templates:** `worktree`, `agents`, `title`; the editor; the grant written by `agents`; **Save as template** on session details.
2. **Head link:** `CreateSession.parent/root/depth`, the `delegations` table, `session.subagent_start`/`interrupt`/`message`/`peek`, the digest moved to a shared crate and built at ingest, `DelegationEnded` with delivery on `Hello`.
3. **Worker runner:** `Task.template`/`on`, `SubagentSpawned.remote`, the oneshot fulfilled by `DelegationEnded`, cancel → `subagent_interrupt`, `SendMessage`/`bash_output` routed through the head, reattach on reload, `subagent_templates_json` in the description. Claude Code parents get this through the existing bridge.
4. **Limits:** depth across workers, tree running count on the head.
5. **UI:** the thread header place, the new states, the Templates page fields.
6. **Verify** with two scratch workers on one scratch head (ports 7790/7791/7792), never the dev state. Test foreground and background runs, Stop from either side, killing each worker mid-run, a parent offline when the report lands, and a Claude Code parent on the local Ollama connection.

## 12. As built (2026-10-31)

All six steps are built. Where the build differs from the plan above:

- **Permission (§4).** *Agents may start it* counts as the grant directly: `templates::targets` lets a session reach the template's workers, hosts and SSH targets when the template has `agents` and is in the session's realm. No grant row is written or revoked. Templates without it still need a `workers`/`start` grant, both for `Task` and `template_start`. Templates are taken from the parent's realm only (and its apps' layers); cross-realm subagent templates are not offered.
- **Prompt.** `Template.prompt` is optional in JSON. Saving a template without one is refused unless `agents` is set. `templates::subagent_prompt` fills `{{input}}` with the task, appends the task when there is no `{{input}}`, and uses the task alone when the prompt is empty. A template with `worktree` needs a folder.
- **Head.** The link is `sessions.remote_template` plus the `remote_delegations` table (migration 0056). `sessions::create_linked` creates the child with `parent_id`, `parent_call_id`, `root_id`, the parent's colour, `remote_template` and `CreateSession.depth`, and records the delegation for the first input before the commands are queued, so a fast turn cannot finish first. The worker calls `session.subagent_start`, `_message`, `_interrupt`, `_state` and `_peek` (`remote.rs`). An agent may reach only its own remote children. `remote::turn_ended` runs at ingest for every `TurnEnded`. It reads the events since the previous turn (the inputs a turn takes are logged before its `TurnStarted`), settles the waiting delegations whose inputs that turn took, or, for a turn of yours, sends `DelegationEnded` with no call. A rejected input, or a child that failed to start, reports `failed`. Restoring a child keeps its depth and renames its delegation to the new input id.
- **Delivery.** `DelegationEnded` is an ordinary durable command for the parent's session. The parent writes `SubagentReported` (a receipt, hidden from the transcript), which marks the command done. The receipt is written after the report is handed over, so a crash in between delivers it again, as a notice.
- **Digest.** It is built from `c6_worker::digest_text`, which the head already links, instead of a new shared crate. The head leaves out `by`; the parent adds `by="user"` when the stop did not come from its own call.
- **Worker.** `Subagents::run_template` is a new runner method; `SendMessage`, `bash_output` and `running` look at `SubagentSpawned.remote`. Waits are registered by `<session>/<call>` in the session manager before the head is asked. A stopped call whose subagent's worker is not connected returns at once ("could not be stopped"), and its report comes later as a notice. A remote child gets no `Delegation`: it is a session of its own, with `CreateSession.depth` (and `SessionStarted.depth`) for the limit, and loses `Task`/`SendMessage` at the depth limit. The running limit is checked on the head when a remote subagent starts, counting every running subagent in the tree; local starts still count only their own worker's.
- **Prompts.** `session.subagent_templates` introduces the list in `Task`'s description; `session.subagent_remote_reattach` is the result written after a restart (and the notice for a background one).
- **UI.** Settings → Templates has **Title**, **Effort**, **New git worktree** and **Agents may start it**. **Save as template** is in the session header, next to *New like this*. A remote thread's header shows `▣ template` and `worker:folder · connection · model` (`Session.remote_template`).
- **Tests.** `crates/c6-worker/tests/it/remote_subagents.rs`, with two real workers: a template's subagent on the other worker with its folder, prompt, report and `SendMessage`, and a refused template; stopping the parent, then your message waking it; the parent's worker restarting while the report arrives.

## 13. Open questions (recommendations in bold)

1. **Ad-hoc placement without a template** (the agent names a worker and folder itself)? **No for v1.** The template is where you decide what's reachable. Later this could be a grant scoped to `{worker, cwd prefix}`.
2. **Does "Agents may start it" stand in for the grant,** or does the grant stay a separate step? **It stands in.** Ticking it in Settings is already your decision, and two steps for one choice is friction.
3. **Should the child inherit the parent's labels?** **No:** template labels plus `started-by`. Inheriting would bring the parent's repo labels to another machine and set off the wrong routing rules and policies.
4. **Queue a start when the target worker is offline?** **No:** fail fast with the alternatives. Queueing raises timeout and "which report is stale" questions that can wait until someone needs it.
5. **File handoff between the two machines** (artifacts, patches)? **Not in v1.** The report carries text, and the digest lists changed paths and, for worktree templates, the branch. Moving code goes through git, which the two agents can already do.

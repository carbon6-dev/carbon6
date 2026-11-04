# Agents act like you

Status: decided with you (2026-11-04); being built.

You asked an agent to "create a new Work in the Falcon Robustness Project and capture these as TODOs in that new Work". It couldn't. Session agents had narrow tools: the `work` tool reaches only the session's own work, `doc` edits documents, `records_*` reads and writes records. Creating work elsewhere, creating a project, or adding to-dos to another work item had no tool. You can do all of these from the UI, and Ask can do them through the operation catalog. Session agents should be able to as well.

## 1. The rule

An agent in a session can do what you can do in its realm, through the same operations, with your consent for what isn't its own:

| What | Agents |
|---|---|
| **Reading** anything in its realm (and what its realm's grants let it read) | free |
| **Making** something new: work, a project, a document, a to-do, a record, a collection, a series, a trigger, a decision, a memory note, a trail… including new things *inside* something that isn't its own (new work in your project, a to-do on your work item, a comment on your document) | free; recorded as made by the session |
| **Changing** something it made (it, its root session, side chats and subagents count as one), or its own work and project as before | free |
| **Changing** anything else (renaming your project, editing your work item, moving your session into other work) | your OK once per thing: an approval card "Let ‹session› change ‹thing›?". Allow lets this session change that thing from then on. Others still ask. |
| **Deleting** what it made | free |
| **Deleting** anything else, and every other destructive operation | your OK each time |
| **Administration**: sign-in and tokens, secrets, connections, workers, realms and grants, apps and packages, updates, settings, notifications, Google and Slack set-up | never. These are yours (or Ask's, with you there). Agents ask for a grant with `grant_request` as before. |
| **Answering approvals** (tool calls, workflow steps, approve decisions) | never, as for Ask (`assistant::yours_to_approve`) |

A thing an agent moves counts as the moved thing changing: bringing your existing work into its project needs your OK for that work item. Bringing in work it made doesn't.

## 2. How

- **One tool for everything**, `carbon6` (`Carbon6` for Claude-family models; on Claude Code it comes through the MCP bridge like the other carbon6 tools). With `find` (words) it lists matching operations with their inputs. With `op` and `input` it runs one, in the session's realm, as `session:<id>`. This is the same catalog and the same `assistant::execute` path Ask and the API use, so every operation the UI has is there, and a new operation is there too as soon as it's in the catalog.
- **The gate** (`crates/c6-head/src/agent_ops.rs`) decides each call:
  - every catalog operation is classified: read, make, change, delete or admin. A test fails when an operation is added without a class, like the catalog's parity test;
  - for change and delete it finds the thing from the operation's path parameters (`work:<id>`, `project:<id>`, `doc:<id>`, `record:<collection>:<id>`, `collection:<name>`, `series:<name>`, `trigger:<id>`, `memory:<id>`, `decision:<id>`, `trail:<id>`, `session:<id>`…) and who made it (`created_by`);
  - consent is kept in one table, `agent_writers (realm, thing, writer, granted_at)`. This generalizes `doc_writers`, whose rows move in.
- **Asking.** A change it may not make yet opens an approval card and returns at once: "asked the user (decision N); nothing changed". When you answer, the session gets a notice and can carry on. Nothing waits on a timer. A destructive call on something not its own opens a card for that one call, and Allow runs it as asked.
- **The existing tools stay**, held to the same rule (they're shorter for the common cases). `work` and `doc` keep their own-work shortcuts. `api` (`Carbon6Api`) stays the reference for writing code against carbon6.
- **Where it came from.** What an agent makes shows the session that made it (a link from the work item, project or to-do to the session).
- **Guidance** is in the editable base prompt (`base`), not in code: use `carbon6` for anything in carbon6 the narrow tools don't cover. Its find results carry each operation's own description.

## 3. Audit

Catalog areas and what agents get (R read, W write, D destructive, S shows a secret):

| Area | Operations | Agents |
|---|---|---|
| work | `work.*` (18) | all but `work.settings.put`. Own work free; others' by the rule. `work.add`, `work.todos.add` free. |
| projects | `projects.*` (18) | all. `projects.add`, `projects.work` (new work) free; `projects.edit`/`phases`/`detach`/`merge` by the rule; `projects.work` with an existing work id needs that work's OK. |
| docs | `docs.*`, `decks.*` (32) | all but `docs.settings.put`, `decks.themes.put`, `docs.google_connect`/`google_remove`. `docs.to_google` (D) and exports by the rule. Today's `doc` permissions (work's and project's documents free, Settings → Docs) carry over. |
| collections | `collections.*`, `records.*` (12) | all; a collection's records belong to who made the collection unless the record says otherwise. |
| series | `series.*` (7) | all. |
| decisions | `decisions.*`, `attention.*` (9) | `decisions.add`, reads, `edit` of its own. Never `answer` on an approve decision. |
| automation | `triggers.*`, `templates.*`, `workflows.*` (14) | reads, `templates.run`/`workflows.run` (as `template_start` today), `triggers.create` free, others' triggers by the rule. Never `workflows.steps.decide`, `triggers.rotate` (S). |
| knowledge | `memories.*`, `claims.*`, `things.*`, `memex.*` (26) | all but `settings.memex.put`, `memex.upload`/`scan`. Your memory notes by the rule. |
| sessions | `sessions.*` (47+) | reads; housekeeping of its own family (title, labels, mark). Others' sessions by the rule. `sessions.create`/`send`/`side_chat` keep going through `task`, `template_start` and `message_session`, which have their own limits. Never `sessions.approvals.decide`. |
| views | `views.*` (3) | all. |
| apps | `apps.operations.call` | as `app_call` today. Install, upgrade, remove, packages and installs: admin. |
| explorer, usage, hosts | reads (11) | free. |
| fleet | `workers.*` (16) | reads of the session's own worker (files, git). Enroll, revoke, labels, realms, terminals: admin. |
| account, connections, settings, notifications, system, mail, chat, assistant | (75) | admin, except reads that show no secret (`realms.list`, `system.version`, `connections.list`, `connections.quotas`). |

## 4. Steps

1. **The gate and the tool.** `agent_ops.rs` (classes, things, owners, `agent_writers` with `doc_writers` moved in), the fabric method `session.op`, the `carbon6` tool in c6-tools and through the MCP bridge, approval cards and the notice on your answer. The test that every operation has a class.
2. **The narrow tools on the same gate.** `work` gains `create` (title, done when, project) and takes a work id for `todo`/`attach`/`get`. `doc` uses `agent_writers`. Records go through the gate.
3. **Where it came from** on work, projects and to-dos, and the base prompt's guidance.
4. **Seen working.** On a scratch head, with a real model: "create a new Work in the ‹project› and capture these as to-dos" makes the work with its to-dos in one turn; renaming your project asks once; deleting your work item asks every time.

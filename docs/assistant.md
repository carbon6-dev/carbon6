# The carbon6 assistant — Design

An agent inside carbon6 that can do anything you can do in its UI, answer questions about carbon6 and its setup, and ask you for whatever it needs.

> "What can I do in carbon6?"
> "Which models are set up for vision?"
> "Create an enrollment token for a new worker."

## 1. Decisions

| Question | Decision |
|---|---|
| One agent or several? | One agent type with many short conversations. It is not one endless conversation, and it is not specialised agents. |
| Where does it run? | In the head. Its tools are head operations, and it must work before any worker is attached. |
| Confirmations | Ask before destructive operations. Everything else runs without asking unless your own policies say otherwise. |
| Secrets | They never pass through the model. Results that contain one are shown to you as a card. |

## 2. Shape

- **Entry points:**
  - "Ask carbon6", from ⌘K, the header and the phone layout;
  - an assistant session in the sessions list, like any other.
- **Each question starts a short session** of the kind `assistant`. It carries the page you're on as context, such as "viewing worker `build-1`" or "Settings → Memex".
  - Sessions are cheap and searchable, and run concurrently.
  - What should last across conversations goes to memory notes and the memex, not into an ever-growing transcript.
- **Long-running work is delegated:** "every morning…" or "watch CI…" becomes an automation, workflow or ordinary session that the assistant creates through the same operations. The assistant does not stay running.
- **Model:** a setting, `assistant_model` (a connection and model; default: the default session model). Model routing by label applies as usual. Any API connection works, including a ChatGPT plan, and a Claude Code connection when `claude` is on the head's machine (docs/ask.md §3). Each conversation can pick its own.

## 3. The operation catalog

UI parity comes from a catalog, not from hand-written tools.

- **Every API route has an entry:**
  - `op`: a stable dotted name, e.g. `workers.enrollments.create`, `settings.memex.get`;
  - `area`: sessions, fleet, connections, memex, mail, automations, settings, …;
  - `summary` and `description`, with the description taken from the route's doc comment;
  - `method` and `path`;
  - `input` and `output`: JSON Schemas generated from the `c6-api` types;
  - `effect`: `read`, `write`, `destructive` (delete, revoke, overwrite, send) or `secret` (the output holds a secret);
  - `choices`: for inputs whose valid values come from live data, the op that lists them, e.g. `connection ← connections.list`.
- **A parity test** fails when a route has no catalog entry, or an entry has no route. That keeps "everything in the UI is doable through the assistant" true as carbon6 grows.
- **The docs** (README, docs/*.md) are indexed by section, so "how do I…" answers can quote them.
- The same catalog later drives a generic `c6 <op>` in the CLI.

## 4. Tools

The assistant has five tools, so its context stays small however many operations there are.

| Tool | Does |
|---|---|
| `c6_discover(query, area?)` | Searches the catalog and the docs. Returns ops (name, summary, effect) and doc passages. With no query, it lists the areas: "what can I do". |
| `c6_describe(op)` | Returns an op's full input and output schema and where its choices come from. |
| `c6_call(op, input)` | Runs an op through the head's own handlers, as you. A result with a secret is replaced by `{"shown_to_user": true, …non-secret fields}`, and the secret goes to the UI as a card. |
| `c6_ask(title, fields)` | Asks you for inputs. See §5. |
| `memex_search` / `memex_get` | Already exist; for questions about your knowledge, within its scope (§6). |

The system prompt tells it to discover before acting, and never to guess values. When a required input is missing or ambiguous, it asks.

## 5. Asking for inputs

`c6_ask` takes fields in JSON Schema terms (type, enum, default, description) plus `choices_from` (an op). The UI renders a form in the transcript:

- dropdowns filled from live data, such as your actual connections and workers;
- defaults filled in;
- validation against the schema;
- it works on a phone.

The tool waits for the submitted values, the same way an approval waits. Cancelling ends the tool call with "cancelled by the user".

## 6. Safety

- **Destructive operations ask first.** `c6_call` on a `destructive` op raises the existing `Approval` event: the op, the input, and a one-line consequence ("revokes worker build-1; its sessions stop"). The call waits on the existing approval endpoint, with the same inbox, notifications and signed approval links. This check lives in the head (the head runs this loop), so it doesn't depend on the worker's policy engine. Your own policies (tool `c6_call`, input regex on the op) still apply on top: `ask` or `deny` for anything else.
- **Secrets:** see `c6_call`. API tokens, enrollment tokens and login links reach you only. The transcript keeps a redacted copy.
- **Scope:** by default the assistant's memex scope is the default one, so no mail, chat, calendars or photos. It holds for everything the assistant reads of the memex: its memex tools and the operations it calls (mail, chat, calendar and contacts operations, searches, commitments, the summaries the memex writes for it), however they reach the memex; an operation's own `scope` can only narrow it. A scope rule can widen it; its labels are `assistant`. Mail can't instruct it unless you let mail in. Even then, destructive operations still ask first.
- **Identity:** it acts as you, with your permissions, and every call is logged as a tool call in its session.
- **Head restarts:** an assistant turn cannot survive its head process. On startup, an open turn belonging to the head is closed with `worker_restarted`, preserving the events and tool results already recorded. Its pending approvals expire and its questions are cancelled, settling their decision cards too. It is not resumed or retried automatically; sessions on external workers are left alone.
- **Model waits:** each model request has a ten-minute deadline, including transport retries, and a five-minute idle limit after its first model event. SSE keepalive bytes do not reset these limits. Expiry ends the turn with a phase-specific error, without automatically retrying that expired request or replaying completed tools. Tool execution and approval waits are not subject to the model deadline. Cancelling a turn still cancels either model-wait phase immediately.

## 7. Build order

1. **Catalog and reading:**
   - catalog plus parity test;
   - the head-hosted `assistant` session kind, using `c6-agent`'s loop in the head;
   - `c6_discover`, `c6_describe`, and `c6_call` for `read` ops;
   - "Ask carbon6" in the UI;
   - evals: "what can I do", "which models are set up for vision", "is any worker offline", …
2. **Acting:**
   - `write`, `destructive` and `secret` ops;
   - the approval check for destructive ops;
   - secret cards;
   - `c6_ask` forms.
   - Evals: "create an enrollment token for a new worker" (asks for a name, shows a card), "add an Ollama connection", "revoke worker X" (asks first).
3. **Context:**
   - page context;
   - suggested next steps ("this worker is offline: re-enroll?");
   - `c6 <op>` generated from the catalog.

## 8. As built

- **Code:**
  - `c6-head/src/catalog.rs` holds the operations: every HTTP route the UI uses, plus the memex methods it calls.
  - `c6-head/src/assistant.rs` holds the head's worker connection, the conversation actors and the tools.
- **The head's own worker:** a `workers` row with the role `head`.
  - It is attached to the registry in-process and hidden from the worker list.
  - It cannot be revoked, deleted or used for ordinary sessions.
  - Its conversations' commands go through the normal command queue, and their events go through `sessions::ingest`.
  - A conversation's actor rebuilds its state from the log after a restart.
  - Requests for files, git or terminals on it get an error right away.
- **`c6_call` runs operations as you**:
  - HTTP operations go through the head's own router, with an in-process `InternalCall` marker that the sign-in check accepts. Outside requests cannot set it.
  - Memex operations go through `memex::call` with `made_by: assistant`.
- **Asking first:**
  - before destructive operations;
  - before a list replacement that drops entries (compared by name or id with the current list);
  - when one of your policies matches `c6_call` and its input (`ask`). A `deny` refuses the call.
  - Approvals are ordinary `Approval` events, decided through the existing endpoint.
- **Questions:**
  - The question is a `Question` event (asked, answered, cancelled); your answer is an `Answer` command (`POST /api/v1/sessions/{id}/answers/{call_id}`).
  - Secret fields come back to the model as placeholders. `c6_call` replaces them with the value.
  - The log records `[secret]`.
- **Secret outputs:**
  - The secret is an ephemeral `SecretCard` on the session's live stream. It is shown while the conversation is open and is never stored.
  - The tool result says `[shown to the user]`.
- **API:**
  - `POST /api/v1/assistant` (prompt and page context);
  - `GET`/`PUT /api/v1/settings/assistant`;
  - `GET /api/v1/catalog` and `POST /api/v1/catalog/call` (for the CLI: destructive operations need `confirmed`).
- **CLI:** `c6 ops [words]` and `c6 op <name> key=value…`.
- **UI:**
  - ✦ Ask in the header (⌘J), with the focused column as context;
  - question forms;
  - secret cards;
  - Settings → Assistant.
- **Since docs/ask.md (2026-11-02):** Ask is also carbon6's everyday chat. It waits for work it delegates to a machine (`c6_delegate`, through the templates agents may start), its conversations are listed under **Chat**, each takes its own model, Claude Code included (run on the head's machine with only Ask's tools), and mail, chat and calendar come in by a per-conversation switch or an @-mention.

### Realms (OS step 8)

- **A conversation works in one realm:** the realm whose page you open it from. From the console it is root, which administers carbon6 and reaches every realm.
- **Its operations are admitted like that realm's API token.** It reaches the realm's sessions, collections, decisions, triggers, views, series and knowledge. Administration (workers, connections, realms, grants, settings) answers 403, and the assistant is told to send you to the console for it.
- **Its instructions** are the `assistant.base` prompt as the realm has it (a realm can override it), with `{{realm}}` naming the realm.
- **Commons** is not a place to work in.
- **From root it starts sessions in a realm, with your approval each time.** Sessions never run in root (or commons), so for work on a machine the assistant in root calls `sessions.create` naming the realm. Before anything starts you get an approval (also a decision and a notification) that names the realm, the worker and, when given, the folder, model, title and labels, with the first message in full. The first message is the one way root's knowledge reaches the realm, so the call carries nothing else: no attachments, work item, to-do or references, and no secret you gave the assistant. The first message is at most 4,000 characters, so the approval always shows all of it.
  - **Allow:** the session starts in that realm as that realm's API token would start it, under the realm's policies, routes, connections and workers and none of root's. It is labelled `started-by:console`. The assistant gets the session back, with its id, and can follow it, since root reads every realm.
  - **Deny** (or no answer): nothing starts, and the assistant is told not to try another way.
  - In any other realm nothing changes: the assistant starts sessions only in its own realm, without asking.
- **Whatever else it puts in a realm from root waits for your approval the same way.** Root reads every realm, so anything the assistant there writes into a realm could carry what that realm should not know (another realm's data, root's administration). Each such call asks first: the approval names the realm and what it acts on (the session's title, the template, the trigger, the decision, the work item…) and shows exactly what goes there, in full: a message's or side chat's text, a template's or workflow's input, else the operation and its whole input as JSON. It refuses what the approval could not show: files, attachments, a model switch with a message, a secret you gave the assistant, more than 4,000 characters.
  - **Its realm:** an existing object's own (a session, trigger, decision, memory note, work item or project); for anything new, documents and knowledge, the realm the call names (`realm` in its input, or `body.realm`), else home, where root's new things have always gone. Into root itself nothing is asked: that is root's own.
  - **Allow:** it runs as that realm's API token would, under the realm's rules and settings and none of root's, and is marked as the console's where carbon6 records who did something: a message reaches the session's agent as a message from “Ask in the console” (that conversation, like another session's, not yours); a side chat and a template's sessions are labelled `started-by:console`; a workflow run's trigger is `console`; documents, project changes and records are written by `console`.
  - **Deny** (or no answer): nothing happens, and the assistant is told not to try another way.
  - In any other realm nothing changes: its assistant acts in its own realm without asking, as before.
- **What asks, and why.**

  | Operations | Why it asks |
  |---|---|
  | `sessions.create`, `sessions.send`, `sessions.side_chat`, `sessions.queue.edit` | text goes to a realm's agent, or a session starts there |
  | `templates.run`, `workflows.run`, `triggers.run`, `decisions.session` | starts sessions or runs in the realm (a template's or workflow's input is text for its agents) |
  | `triggers.create`, `triggers.update` | a trigger acts in the realm later, with the input and action parameters you approved |
  | `decisions.add`, `decisions.answer` | a decision's text, or an answer's values and note, which go to the agent waiting on it |
  | `memories.create`, `memories.update` | memory notes are what the realm's sessions start with |
  | `work.add`, `work.edit`, `work.attach`, `work.todos.add`, `work.todos.edit` | work items and to-dos (a to-do's prompt is a session's first message) |
  | `projects.add`, `projects.edit`, `projects.work`, `projects.links`, `projects.phases` | a project's title, purpose, phases, links |
  | `docs.create`, `docs.edit`, `docs.rename`, `docs.comment`, `docs.reply` | documents and comments the realm's agents read and answer |
  | `collections.create`, `collections.update`, `records.create`, `records.put`, `series.create`, `series.update`, `series.record`, `views.act` | the realm's data (a view's action may also start its template) |
  | `apps.operations.call` | an app's operation writes its data or starts its session, as the app |
  | `memex.notes.create`, `memex.trails.append`, `memex.trails.update`, `memex.relations.write`, `claims.add`, `things.create`, `knowledge.record`, `knowledge.set`, `mail.drafts.set` | knowledge the realm's agents search (from root, home's), and a reply box's draft a session may pick up |
  | `docs.import`, `docs.add_file`, `memex.upload`, `memex.ingest` | refused from root: a file cannot be shown in the approval; add it in the realm itself |

- **What does not ask, and why.**

  | Operations | Why not |
  |---|---|
  | `settings.*` (skills, prompts, agents, templates, workflows, policies, routes…), `realms.*`, `grants.*`, `connections.*`, `workers.*`, `apps.install`/`upgrade`/`remove`, `installs.*`, `packages.*`, `google.client.put`, `google.connect`, `google.install`, `slack.client.put`/`install`/`connect`/`sync`, `docs.google_connect`, `decks.themes.put`, `work.settings.put`, `docs.settings.put` | administration, root's own job (from root these write root's settings, which realms inherit); installs ask through their own decision |
  | Reads of every kind | root may read every realm |
  | `sessions.update`, `sessions.labels.set`, `sessions.colour.set`, `sessions.subagent_model.set`, `sessions.seen`, `sessions.viewed`, `sessions.suggestion.seen`, `marks.set`, `attention.seen`, `work.seen`, `decisions.edit` | housekeeping (a title, labels, a mark, seen, snooze or dismiss) that carries nothing to an agent |
  | `sessions.interrupt`, `sessions.resume`, `sessions.notices.deliver`, `sessions.queue.dismiss`, `sessions.queue.pull`, `work.refresh`, `projects.refresh` | steers a session or note with what is already in the realm (a resume uses the realm's own prompt) |
  | `sessions.approvals.decide`, `workflows.steps.decide` | an allow or deny with no text; the same as in any realm |
  | `work.detach`, `work.merge`, `projects.detach`, `projects.merge`, `projects.docs`, `projects.plan`, `projects.proposed`, `projects.propose`, `projects.sessions.*`, `docs.restore`, `docs.export`, `docs.resolve`, `docs.comments.send`, `decks.bring_in` | rearranges or sends on what is already in the realm |
  | `memex.summarize`, `memex.review.decide`, `memex.commitment.set`, `memex.scan`, `google.sync`, `mail.requests.*`, `mail.triage.*`, `mail.drafts.link`, `mail.drafts.clear` | made from the realm's own knowledge, or a status |
  | `mail.draft`, `mail.modify`, `calendar.*`, `contacts.*`, `chat.*` | your own Google and Slack accounts, outside carbon6; sending mail and deleting ask already (destructive) |
  | `workers.fs.mkdir`, `workers.terminals.open`, `notifications.test` | a machine's folder, a terminal you use yourself, a test notification |
  | Destructive operations (deletes, `sessions.rewind`, `mail.send`…) | ask already, as everywhere |

## 9. Open questions

- Should the assistant be able to start ordinary sessions on workers and follow them (for example "run the tests in proj X and tell me")? It can through the ops; the question is whether it should wait for results in the same conversation.
- Voice input on the phone.

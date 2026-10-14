# carbon6 as an operating system — Design

Status: design only (2026-10-13). Nothing here is built yet.

## 1. Goals

carbon6 should work like an operating system for agents. It provides general primitives, and **apps** are built from them with configuration only: prompts, schemas, triggers and views. An app such as a service factory or a personal CRM behaves the way it does because of its configuration, not because carbon6 has code for it.

- **The rule:** if an app needs code in carbon6, a primitive is missing. Add the primitive, never the app.
- **Why:** follow-through (kits, trackers, playbooks) was built as features for one use case, and was removed on 2026-10-13 because it didn't land. The general versions of those ideas are records, triggers, decisions and app packages (8).
- **Isolation:** many apps run side by side in **realms**. Information moves between realms only through flows you declare, instead of everything sharing one pool.
- **Prompts are configuration.** Every instruction carbon6 gives an agent is an editable default, including the ones that are still in code today: the knowledge guidance in briefs, and the prompt for a session started from an action item.

**Not goals:**
- **More than one user.** carbon6 stays single-user. Realms isolate apps from each other, not people.
- **Sandboxing by default.** Agents' shell access stays as it is today, except for policies you configure. Realms isolate the data carbon6 manages; how far that goes on a shared machine is spelled out in 3.9.
- **A general-purpose OS.** The primitives are the ones agent apps need, and no more.

## 2. The model

| OS concept | carbon6 | Status |
|---|---|---|
| Isolation | **Realms** (3) | New |
| Processes | Sessions, subagents, side chats | Exists; gains a realm |
| Machines | Workers, available to some realms or dedicated to one (3.2) | Exists; gains availability |
| Programs | Skills, agents, commands, templates, workflows, hooks, MCP servers | Exists; realm-scoped (4.6) |
| Files | Workers' file systems, session outputs | Exists |
| Knowledge | The memex: items, graph, search, notes, trails, facts | Exists; partitioned by realm (4.7) |
| Structured storage | **Collections** of typed records (4.1) | New |
| Asking the user | **Decisions** (4.2) | New; replaces approvals, action items and questions |
| Events and scheduling | **Triggers** (4.3) | Generalizes automations |
| Numbers over time | **Series** (4.4) | New; usage and budgets move onto it |
| Screens | **Views** (4.5) | New |
| Packages | **Apps** (5) | New |
| Permissions | Realms, grants, policies, secrets, budgets, realm-scoped API tokens | Partly exists |

## 3. Realms

### 3.1 What a realm is

A realm is a named boundary that **owns** objects. Every object belongs to exactly one realm. Nothing outside a realm can see what it owns, except through a grant (3.7).

```
realm(id, name, description, created_at, created_by /* you | app install */, app /* the app it was made for, if any */)
```

Realms and labels do different jobs:

| | Realm | Label |
|---|---|---|
| How many per object | Exactly one | Any number |
| Meaning | Owns it; a boundary | Describes it; used for matching |
| Default | Invisible outside the realm | Visible wherever its scope reaches |
| Used for | Isolation and information flow | Choosing configuration inside a realm |

Labels keep working as they do today, inside a realm.

### 3.2 What belongs to a realm, and what is shared

**Owned by a realm:**
- sessions and their history (side chats and subagents stay in their parent's realm);
- the memex's contents: items, things and links, facts, notes, trails;
- collections, records and series;
- decisions;
- programs: skills, agents, commands, templates, workflows, hooks, MCP servers;
- triggers (automations);
- views;
- secrets, policies, budgets and memory notes.

**Shared infrastructure,** like hardware in an OS:
- **Workers:** each worker is either available to a list of realms, or dedicated to one. A session can only start on a worker its realm may use.
- **Model connections and routes:** each is available to a list of realms (all, by default).
- **The memex service:** one service, with its data partitioned by realm.
- **The head:** it enforces everything (3.8).
- **An overall budget cap,** above the realms' own budgets.

### 3.3 Root, home and commons

- **root** holds carbon6's own administration: workers, connections, sign-in, API tokens, realms and grants. It also holds the **defaults every realm inherits**: base skills, policies, the system prompts and the briefing guidance. Sessions don't run in root, except the assistant's administration conversations (3.6).
- **home** is where everything that exists today moves (3.10), and it's the default realm for new sessions. Your mail, calendar, contacts and photos live here.
- **commons** holds knowledge meant for every realm, such as facts about your infrastructure. Realms publish into it on purpose (3.7); nothing arrives there by default.

**Inheritance:** configuration flows down from root. A realm can override what it inherits: a skill, agent or command with the same name and labels in the realm wins, and policies from root apply before the realm's own. **Data never flows up on its own.**

**Realms are flat:** there are no sub-realms. This is easier to reason about, and nesting can come later if apps need it.

### 3.4 Labels inside realms

- **Labels are per realm.** `client:acme` in two realms is two different things in the memex's graph. If they should be the same thing, that link goes through commons (3.7).
- **Automatic labels** (`worker:`, `repo:`, `folder:`, `model:`) work in every realm as they do today.
- **"Applies to"** matches labels within the realm, as today.

### 3.5 Sessions

- **One realm per session,** chosen when it starts. The default is remembered per folder, the way labels are, so a folder used for the factory starts factory sessions.
- **No moving.** A session can't move to another realm. Its data can be copied through a grant, and the copy carries where it came from.
- **What a session sees:** its brief, search results, facts, records, decisions, secrets and tools all come from its realm, plus whatever grants let in.

### 3.6 The assistant

- **One per realm.** It can do what the UI can do inside that realm.
- **Administration** (workers, connections, realms, grants, app installs) runs in root, and only when you open it there.
- An assistant never acts in another realm. It may suggest a grant, which you approve (4.2).

### 3.7 Grants: information flow between realms

A grant is the only way data crosses a realm boundary:

```
grant(id, from_realm, to_realm,
      scope /* a collection (with an optional filter), a memex kind or saved query, a series, or named operations */,
      mode /* read | append | snapshot | call */,
      expires_at?, created_at, note)
```

| Mode | Means |
|---|---|
| read | The other realm can query what's in scope, live. |
| append | The other realm can add to what's in scope, but not change or delete it. |
| snapshot | A one-time copy. |
| call | The other realm can run named operations in this realm, for example sending mail from home's accounts. Each call is still subject to this realm's policies, so a send can still ask you first. |

**Guarantees:**
- **Provenance travels.** Anything that crosses carries its origin realm and the grant it came through, and keeps them through later copies.
- **Every access is logged.** You can see what flowed where.
- **Revoking stops future flow.** It can't recall what was already copied, and the UI says so.
- **Grants mean what they say.** A session that can read A and write B can copy A's data into B. So a grant is defined as "A's data may flow into B", not "B may read A". This is honest information-flow control, not a stronger promise than the system can keep.
- **Only you create grants.** An agent can request one, as a decision.

**Publishing to commons:** a realm with an `append` grant to commons proposes items, such as facts. They wait in commons' review queue until you accept them. Every realm can read commons by default; you can turn that off per realm.

### 3.8 Enforcement

- **The head is the only place access is decided.** Everything sessions do with carbon6 data passes through it: workers call the head over the fabric on behalf of a session, and the head looks up the session's realm.
- **The memex** gets the realm and the grants' scopes with every call. They replace today's label-based scopes (memex design 2.3).
- **API tokens are scoped to realms.** An integration's token reaches only its realm's data (plus that realm's grants), so the code an app writes can't read your mail.
- **Secrets** are delivered only to sessions in their realm. A secret scoped to a `worker:` label still goes only to that worker.

### 3.9 What realms do not isolate

- **Files on a shared worker.** Sessions are not sandboxed (1), so two realms' sessions on the same worker can read each other's files. When that matters, dedicate a worker to the realm (3.2): for example, give a factory its own VM.
- **What a model provider sees.** A shared model connection sends every realm's prompts to the same provider. Making a connection available only to some realms (3.2) controls which providers a realm's data can reach.

### 3.10 Moving in

- **Your data moves into home,** and so does your configuration that grants abilities: secrets, MCP servers, skills, agents, commands, hooks, templates and memory notes. A new realm doesn't inherit them unless you move them to root or share them.
- **Restrictions move into root,** so every realm inherits them: policies and budget caps. A new realm is never less protected than home.
- **Memex scopes by label** become label scopes inside home.
- **Until you create a second realm, nothing behaves differently.** The full upgrade plan is in 13.

### 3.11 As built (step 1)

- **Realms:** a `realms` table in the head holds root and home. Migration `0028` gives sessions, memory notes, action items, automations, workflow runs, secrets and usage a `realm`. Everything moves to home, except the assistant's conversations, which do administration and move to root. API tokens get a `realm` that is empty for a root token, so every existing token stays a root token. Upgrade checks prove each move against the data.
- **Callers:** the auth layer identifies a `Caller` for every request:
  - **Root callers:** you, signed in (or with sign-in off), and a root token reach every realm.
  - **Realm-scoped tokens:** a token limited to a realm passes `realms::admit` first. It reaches its realm's sessions (and everything under a session), search, action items, memory notes, automations, template runs and the memex, each checked per object. Another realm's object answers 404. It can read workers, connections and their models, commands, the catalog, the OpenAPI document and its own realm. Everything else is administration and answers 403, including `/api/v1/catalog/call` (it would run operations as an internal call) and `/api/v1/live`.
  - **Tokens and sign-in:** a valid token scopes even with sign-in off.
  - **Creating tokens:** `c6h api-token --realm home`, or the realm selector under **Settings → Sign-in → API tokens**.
- **Sessions:**
  - **Where they run:** a session runs in the realm it is created in: the API's `realm`, else the caller's, else home. Never root (only the assistant's conversations are there), and only in a realm that exists.
  - **Inheritance:** side chats and subagents take their parent's realm. Template runs, automations and workflow runs start their sessions in their own realm.
  - **What a session gets:** only its realm's memory notes and secrets.
  - **Secret requests:** a worker asks for a secret on behalf of a session (`secret:NAME@<session>`), and the head checks the secret is in that session's realm. A bare name, from an older worker, gets only home's secrets.
- **The memex:**
  - **Migration:** `0007` gives the memex's data a `realm` (all home).
  - **Every call carries a realm:** the head stamps the caller's or session's realm on each call, overwriting any realm a client sends. Root keeps no knowledge, so the assistant's conversations are no longer sent to the memex. The memex refuses any realm but home.
  - **Per-realm queries:** queries partitioned by realm, and things keyed per realm, come with step 4, together with grants on memex data. Until then nothing crosses, because only home's data is there.
- **Test databases:** `c6h-v0027.db` was regenerated with an assistant conversation, so the move to root is exercised. `c6h-v0028.db` and `memex-v0007.db` hold the new shape. The `realms` integration test covers tokens, admission, sessions in realms, and secrets, memory notes and action items not crossing realms; a second realm is inserted directly for it.

### 3.12 As built (step 4a)

- **Creating realms:** `POST /api/v1/realms` (id: lowercase letters, digits and `-`), rename, and delete. Only a realm that owns nothing can be deleted, and root and home stay. A new realm gets its own system collections at once.
- **Availability:** workers and connections carry a `realms` list (migration `0032`; empty means every realm), set with `PUT /api/v1/workers/{id}/realms` and `…/connections/{id}/realms`. A session cannot start on a worker, or with a connection, that its realm may not use.
- **Per-realm settings:** a Settings request with `?realm=` reads and writes that realm's own lists. A request-scoped value (`SETTINGS_REALM`) tells the settings store which realm, so every settings endpoint and form works per realm unchanged.
- **UI:** a realm switcher in the header, remembered per browser ("All realms" by default; shown once a realm beyond home exists). Sessions, decisions and collections show the realm's own, new sessions start in it, and Settings edits its lists, with a note saying so. **Settings → System → Realms** creates and deletes realms, and sets which realms each worker and connection serves.

### 3.13 As built (step 4b)

- **The memex keeps each realm's knowledge in a database of its own:** home's memex (the one `c6m` runs) opens another realm's memex under `realms/<id>/` when first asked.
  - **Isolation by construction:** each has its own items, things, links, facts, trails and passages, so search, briefs, facts, notes and things are per realm, and nothing in the queries could leak.
  - **Upgrades:** each database upgrades itself through the same machinery.
- **Routing:** every call carries its realm (the head stamps it), and home's memex hands another realm's call to that realm's memex.
  - **Home's only:** mail, calendars, contacts, photos, folders and upgrades are refused for other realms.
  - **Their jobs:** another realm's memex runs only the understanding and embedding jobs.
- **Back to the head:** what a realm's memex raises carries its realm. Its disputes open decisions in that realm, and its events reach only that realm's automations.
- **Deleting a realm** queues `memex.drop_realm`, which removes its database and blobs.

### 3.14 As built (step 4c)

- **Grants:** a `grants` table (migration `0033`) and a log of each use (`grant_uses`). Only you create and revoke them: `/api/v1/grants`, and **Settings → System → Grants**, which lists each grant with its uses. A grant is between two realms that hold data, never root.
- **Kinds and modes:**

| Kind | Modes | Scope |
|---|---|---|
| collection | read, append, snapshot | `{collection, where?}`: `where` limits a read to some records |
| memex | read | `{kinds?}`: none means what the session may see anyway |
| operations | call | `{methods: [...]}`, such as `memex.mail.draft` |

- **Collections:**
  - **Reaching another realm's collection:** a realm-scoped token gives `?realm=`, and an agent gives `realm` on `records_*`. The head checks for a grant on every call.
  - **What each mode allows:** read covers listing, querying, getting and history. Append covers adding only. Changing or deleting another realm's records is never granted.
  - **Grants on some records:** a grant's `where` is a second filter that must also hold. A query cannot widen it, and getting a record outside it answers 404.
- **Provenance:**
  - **Appends:** what a realm adds through a grant records who added it and through which grant (`token:factory grant:<id>`).
  - **Snapshots:** a snapshot copies the records into the receiving realm's collection of the same name, creating it with the same schema if needed. Each copy is labelled `from:<realm>` and `grant:<id>`, and linked to its original.
- **Knowledge:** a session's (or a realm token's) memex search also searches the realms that granted it reads, limited to the grant's kinds. Hits from there carry `realm`. Getting an item by id falls back to those realms.
- **Operations:** a granted operation runs in the realm that owns it, which is how a realm drafts mail through home's accounts. Each call is logged on the grant.
- **Requests:** an agent's `grant_request` creates a requested grant and a decision (Allow or Deny). Your answer puts the grant in force, or denies it, and the session hears the answer as a message. Revoking a request settles its decision.

### 3.15 As built (step 4d)

- **Commons is a realm** (migration `0034`), created on upgrade.
  - **Who reads it:** every realm, without a grant row, until you untick **reads commons** for that realm under Settings → System → Realms.
  - **What can't happen in it:** sessions don't run there, tokens can't be limited to it, and it can't be deleted.
  - **Grants:** a grant can't point into it.
- **Reading:**
  - **Collections:** commons' collections answer `?realm=commons` (or `realm` on `records_*`).
  - **Search:** a realm's memex searches include commons' knowledge.
  - **Briefs:** a session's brief adds what commons knows about the same things, under "From commons".
- **Publishing:**
  - **Grants:** a realm needs an append grant from commons, either on a collection or on knowledge (kind memex, mode append, which exists only for commons).
  - **Records:** adding a record to commons' collection then answers 202, with a decision in commons. The record is checked against the schema up front, and nothing is in commons until you choose **Publish**.
  - **Facts:** an agent's `commons_publish` proposes a record, or a fact of its realm's knowledge by id.
  - **When you choose Publish:** a record goes in with `from:<realm>` and "published from realm X, reviewed by you". A fact goes in as reviewed, with its evidence. Reject publishes nothing.
- **Deleting a realm** now also revokes its grants (their log stays).

## 4. Primitives

Every primitive is scoped by realm, has operations in the catalog (so the assistant, the CLI, the API reference and the OpenAPI document get them automatically), and has agent tools where agents need them.

### 4.1 Collections and records

A **collection** is a typed table that agents and code can use for anything.

```
collection(realm, name, description, schema /* JSON Schema */, schema_version,
           keys /* unique fields */, labels, created_by)
record(id, realm, collection, fields JSON, links /* memex things and items */,
       labels, version, created_by /* session | you | trigger | api token */, created_at, updated_at)
record_history(record_id, version, fields JSON, changed_by, changed_at)
```

- **Strict schemas.** Unknown fields and wrong types are rejected, as with ingest attributes, so agents can't drift the data. Enums, required fields, formats and unique keys are enforced.
- **Changing a schema:**
  - **additive changes** (a new optional field, a new enum value) apply at once;
  - **anything else** needs a migration: a description of how existing records change, applied after you approve it as a decision.
- **Optimistic updates.** Writes give the version they expect, so two sessions can't silently overwrite each other.
- **History.** Every change is kept, with who made it.
- **Links into the memex.** A record can link to things (a person, a repository) and items (an email, a document) in its realm.
- **Search and briefs.** Records are mirrored into the memex as items of kind `record`, the way facts are, so search finds them and briefs can mention them.
- **Queries:** a small JSON filter language with equality, ranges, `in`, text match on string fields, sorting, grouping with counts, and paging. There's deliberately no SQL, so queries stay safe and portable.
- **Effects:** deleting a record or dropping a collection is destructive, and asks first (4.2).
- **Where they live:** in the head's database. The head is the always-on core, next to the triggers that react to changes. The memex is optional and may run on another machine, so it only holds the search mirror.

#### As built (step 2a)

- **Storage:** tables `collections`, `records` and `record_history` in the head (migration `0029`). The schema checker (`schema.rs`) is carbon6's own: it covers types, properties, required fields, enums and consts, items, `anyOf`/`oneOf`/`allOf`, `$ref` into `$defs`, ranges, lengths and patterns. Objects are closed unless `additionalProperties` says otherwise; an object schema with no properties at all stays open, for free-form maps.
- **Schema changes:** a new schema is accepted only if every record already there still fits, and new keys only if they are already unique. A change that would reject records answers "needs a migration"; that path arrives with decisions (step 3).
- **Queries:** the JSON filter covers `$eq`, `$ne`, `$in`, `$nin`, `$gt(e)`, `$lt(e)`, `$contains` (text, or an item of a list), plus words, sorting, paging and `group_by` counts. Field names are checked (letters, digits and `_`) before they reach SQL.
- **History:** every create, update and delete is kept in `record_history`, with who made it: `you`, `token:<realm>` or `session:<id>`.
- **API:** `/api/v1/collections…`, with `?realm=` for you (a realm's token is always in its own realm). Every route has a catalog operation, so the assistant, the API reference and the OpenAPI document cover collections.
- **Agents:** sessions get `records_query` (which lists the collections when given none), `records_get`, `records_put` and `records_delete`, which reach only the session's realm.
- **The memex:** records of non-system collections are mirrored into it as kind `record` (in the default scope) and removed when deleted.
- **UI:** **Collections** shows a page per collection, with a table, search, forms generated from the schema (required fields first; a choice for enums, a checkbox for booleans, a comma-separated list for lists of strings, JSON for anything else), each record's history, and deleting.

### 4.2 Decisions

A **decision** is any time carbon6 needs you.

```
decision(id, realm, kind /* approve | choose | provide | confirm | question */,
         title, document /* Markdown: the case, the evidence, the consequences */,
         options /* each with a label and what happens */, form /* fields, for provide and question */,
         default_on_expiry?, deadline?, priority, group,
         asked_by /* session | workflow | trigger | policy | grant request */, context /* links */,
         state /* open | answered | expired | withdrawn */, answer, note, answered_via, answered_at)
```

- **One primitive for every kind of question.** These existing features become kinds of decision, with the same list, the same notifications and the same links:
  - approving a tool call under a policy;
  - a workflow's approve step;
  - action items;
  - the assistant's questions;
  - confirming a destructive operation;
  - grant requests and app installs.
- **Answering:** in the To do list (renamed Decisions), in the session or view that asked, or from a notification's signed link, so you can answer from your phone without opening carbon6.
- **Waiting or not:**
  - **blocking:** the session waits, as tool-call approvals do today;
  - **non-blocking:** the decision is recorded, and its answer fires an event (4.3) that starts whatever comes next.
- **Expiry:** with a deadline, an unanswered decision takes its `default_on_expiry`, or "no" if it has none, which is what happens today.
- **Batching:** decisions in the same group are presented together, and a scheduled digest lists what's open. This is what makes "my only involvement is approval" practical.
- **The document matters.** A decision should be answerable from the document alone, so the asking prompt (a skill) says what it must contain: the case, the evidence, the cost, the risk, and what each option does.

#### As built (step 3)

- **Storage:** migration `0031` turns `todos` into `decisions` in place: ids kept, `detail` becomes `document`, `resolution` becomes `note`, done becomes answered. New columns hold options, a form, a deadline with a default answer, the answer, how it was answered, and `ref`, naming what waits on it.
- **Kinds and states:** kinds are approve, choose, confirm, provide, question, dispute and follow_up. States are open, snoozed, answered, dismissed, expired and withdrawn.
- **What opens one** (`decisions.rs`):
  - **A tool call held by a policy** (`approval:<session>:<call>`): Allow or Deny, a one-hour deadline, default Deny.
  - **A workflow's approve step** (`workflow:<run>:<step>`): Go ahead or Stop.
  - **The assistant's questions** (`question:<session>:<call>`): its fields as a form. A question asking for a secret gets no form; it is answered in the conversation, and secrets are never stored in decisions.
  - **An agent's `decision_request`** (`session:<session>:<id>`): non-blocking. The agent carries on, and the answer arrives in its session as a message ("Your question was answered (decision #N …)").
  - **`todo_add`, the memex's disputes, and you:** nothing waits on these.
- **Answering:** answering one answers what waits on it (the approval, the workflow, the question, or a message to the session). If that fails, the decision stays open. Answering the waiting thing elsewhere (in the session, through its link, or by it timing out) settles the decision. Answers are checked: an option must be one offered, required fields must be filled, choices must be valid, and there are no extra fields. Nothing that waits can be dismissed.
- **Deadlines:** every half minute (`sweep`), an open decision past its deadline takes its default answer, or "no", and becomes expired.
- **Notifications:** a new decision with options sends a `decision` notification (on by default), with a signed link (`/approve/{token}`, prefix `dc:`) whose page offers the options as buttons.
- **Reconcile at start:** the head opens decisions for whatever already waits without one (held tool calls, approve steps), so an upgrade loses nothing.
- **API:** `/api/v1/decisions…` (list, count, get, add, `answer`, edit to snooze, dismiss or reopen, `session`). Every route has a catalog operation. The **Decisions** page replaces **To do**, and `/todos` still opens it.

### 4.3 Events and triggers

Automations generalize into **triggers**: an event, an optional filter, and an action.

| Events | |
|---|---|
| Time | a schedule (cron), as today |
| Inbound | a webhook call, as today |
| Records | created, updated (optionally: this field changed, or changed to this value), deleted |
| Decisions | answered (with which option), expired |
| Series | crossed a threshold, or stopped arriving |
| Sessions | finished, failed, needs approval |
| Memex | new items matching a query, as today's memex events |

| Actions | |
|---|---|
| Start | a template or a workflow, with the event as `{{input}}` |
| Write | create or update a record, with fields mapped from the event |
| Record a value | add a point to a series |
| Ask | open a decision |
| Notify | send a notification |

- **Mapping without code.** A webhook or event can be mapped to record fields or series values with JSONPath expressions. For example, a payment provider's webhook writes a revenue point without starting a session. Anything harder starts a template, where an agent does the work.
- **Loop protection.** Every action carries the chain of events that caused it. Chains deeper than a limit, or a trigger firing too often (a rate limit per trigger), stop and notify you. Each trigger can have a budget.
- **History,** as automations have today: each run, its cause, its outcome, its cost.

**As built (step 5):**
- **Automations became triggers in place:**
  - **Migration `0035`** renames `automations` to `triggers` and `automation_runs` to `trigger_runs`. Ids, schedules, inputs, webhook token hashes (so URLs keep working) and run history are kept and checked.
  - **New columns:** `event_params`, `filter`, `action` (`start` for existing automations), `action_params`, `per_hour`, and each run's `cause`.
  - **API:** `/api/v1/triggers`, with `/api/v1/automations` kept as its old name.
- **Events:** schedule, webhook, memex (as before), plus:
  - **record:** created, updated or deleted in a collection. The event has the record, the fields before, and which fields changed.
  - **decision:** opened, answered (with the option), expired or dismissed.
  - **session:** a turn finished or failed, or the session needs approval; optionally only sessions with some labels.
- **Filters and mapping:**
  - **Filters:** JSONPath conditions on the event (`$.record.fields.stage`, `$.body.amount`), with the same comparisons as record queries plus `$exists`.
  - **Mapping:** in action parameters, a string `"$.path"` becomes that value, and `{{$.path}}` inside text becomes it as text.
- **Actions:**
  - **start:** a template or workflow, as before. `{{input}}` is the event (a webhook's body, the mail summary, or the event as JSON), or `action_params.input` mapped from it.
  - **write:** creates a record, or updates the one `id` names (merging fields).
  - **ask:** opens a decision.
  - **notify:** sends a notification as event `trigger`, which channels carry by default.
- **Loop protection:**
  - **Chains:** an action runs with the chain of triggers that led to it. At `MAX_DEPTH` (5) the run is stopped and you're notified.
  - **Rate:** a trigger that ran `per_hour` times in the last hour is skipped, also with a notice. Manual runs don't count.
  - **History:** each run records its cause.
- **Digests:** a schedule's event has `open_decisions {count, high, text}`, so a digest is a schedule that notifies, filtered on the count. The Triggers form has a preset for it.

### 4.4 Series

A **series** is numbers over time.

```
series(realm, name, unit, description, dimensions /* e.g. service */, retention, labels)
point(series, ts, value, dimensions JSON)
threshold(series, condition /* e.g. sum over 7 days < 10, no point for 2 hours */, dimensions?, then /* an event */)
```

- **Queries:** sum, average, minimum, maximum and last value, over windows, grouped by dimension.
- **Sources:** trigger mappings (4.3), agents (`series_record`), the API, and carbon6 itself.
- **Built in:** carbon6 records model spend, tokens and session counts per realm and label as series. **Usage and budgets move onto series:** a budget is a threshold whose action is to stop spending.

**As built (step 6):**
- **Storage:** migration `0036` adds `series` (per realm: unit, description, dimensions, retention) and `points` (time, value, dimensions, source).
- **API:** `/api/v1/series` lists, creates, changes and deletes series; `…/{name}/points` adds a point, and `…/{name}/query` summarizes.
- **Queries:** sum, avg, min, max, last or count over the last N minutes (default a day) or from/to. Optionally only points with some dimension values, per dimension value (`group_by`), or per hour or day (`bucket`).
- **Where points come from:**
  - the API;
  - agents, with `series_record` and `series_query` (which lists series when given none);
  - triggers, with the "record a value" action (`{series, value: "$.body.amount", dims}`);
  - host checks, which now keep history in home's `host_up` and `host_check_ms` (each host's latest check moved in on upgrade).
- **Model usage as series:** `model_spend` (at today's prices), `model_tokens` and `model_calls`, per realm, by model and connection, computed from the usage ledger. They read like any series and are never written to.
- **Thresholds are trigger events:**
  - **The condition:** `{series, agg, minutes, op, value, where?}`, or `{series, silent_minutes, where?}` for points that stop arriving.
  - **When it fires:** once when the condition becomes true (`crossed`) and once when it stops being true (`cleared`).
  - **When it's checked:** at once when a point is recorded, and every minute (also for silence and model usage). A sweeper drops points past their series' retention.
- **UI:** the Collections page lists the realm's series with their 24-hour and 7-day sums, and creates and deletes them. The Triggers form has the series event and the "record a value" action.

### 4.5 Views

A **view** is a configured screen, rendered by carbon6's UI.

```
view(realm, name, kind /* table | board | record | chart | feed | page */,
     source /* a collection query, a series query, decisions, sessions, memex query */,
     fields, group_by, sort, actions, position /* in the realm's menu */)
```

- **Kinds:**
  - **table:** records as rows;
  - **board:** records as cards in columns by a field, such as a stage;
  - **record:** one record with its fields, links, history, sessions and decisions;
  - **chart:** series over time;
  - **feed:** events, decisions or sessions as a timeline;
  - **page:** a Markdown page with embedded views, as a dashboard.
- **Actions** on a view or a record:
  - start a template with the record as its input;
  - set a field, which moves a card between columns;
  - answer a decision;
  - open the record's sessions.
- **Agents' one-off pages** stay as Outputs. Views are for what an app shows every day.
- **Each realm has its own menu** of views, next to the sessions, decisions and memex pages every realm has.

### 4.6 Programs

Skills, agents, commands, templates, workflows, hooks and MCP servers keep working as they do, but belong to a realm and inherit from root (3.3).

- **System prompts are programs too.** The prompts carbon6 itself uses become root skills you can edit or override per realm, never strings in code:
  - the base system prompt;
  - the knowledge guidance in briefs;
  - how to write a decision's document;
  - the prompt for a session started from a decision;
  - the compaction prompt.
- **A workflow's approve step** becomes a decision (4.2).

#### As built (step 2c)

- **Where prompts live:** carbon6's prompts are records of the `prompts` system collection in root. A realm can override one by name.
- **Defaults** ship in code (`prompts::DEFAULTS`; the memex's and the action-item prompt in `c6_api::prompts`, the agent's in `c6_agent`):
  - **added at every start;**
  - **unedited prompts follow the code:** a prompt nobody has edited (`updated_by = system`) follows its default when the default changes;
  - **edited prompts are never touched.**
- **The prompts:**
  - **sessions:** the base prompt, the side-chat note, the subagent note, and the compaction prompt with its two openings (automatic, and `/compact`);
  - **the assistant's instructions;**
  - **the action-item session;**
  - **the memex:** the brief's knowledge guidance, the reading system prompt and per-kind instructions (threads, papers, sessions, other), a thing's summary, system pages, and photo descriptions.
- **Placeholders** are `{{name}}` (`c6_api::prompts::fill`).
- **How each reaches its model:**
  - **sessions:** the head sends the base prompt as the session's realm has it. It sends the worker's own prompts (compaction, subagent) in the spec's `prompts_json`.
  - **the memex:** it gets its prompts with its configuration. Editing one tells it to reload.
- **Settings → Prompts** lists them with their defaults. Each can be saved, or returned to its default, which an empty text also does. The API is `/api/v1/settings/prompts`.
- **A gap fixed:** a session started from an action item now runs in the item's realm.

### 4.7 Knowledge

- **The memex is partitioned by realm.** Search, briefs, facts, notes and trails stay inside the realm and its grants.
- **Shared knowledge goes through commons** (3.7), with you reviewing what's published.
- **One person in two realms:** a person in home and the same person in the CRM realm are different things, unless they're linked through commons or a grant. The CRM realm reaches home's people through a read grant on them.

### 4.8 Agent tools

Sessions get tools for the primitives, which only reach their realm and its grants:

| Tool | Does |
|---|---|
| `records_query`, `records_get` | Queries and reads records |
| `records_put` | Creates or updates a record, with the version it expects |
| `records_delete` | Deletes a record (destructive: asks first) |
| `decision_request`, `decision_get` | Asks you something, blocking or not; reads the answer |
| `series_record`, `series_query` | Adds a point; reads a summary |
| `carbon6_api` | The API reference, for code that uses carbon6 (exists) |

The memex tools (`memex_search`, `knowledge_record` and the others) stay, scoped by realm.

### 4.9 The API

- **Every primitive has catalog operations,** so it's in `/api/v1/openapi.json` and the `carbon6_api` reference automatically.
- **Tokens carry realms** (3.8). A token made for an app's integration reaches only that app's realm.
- **Ingest** (`memex.ingest`) writes into the token's realm.

## 5. Apps

### 5.1 The package

An app is a package of configuration, with **no code**:

```yaml
app: service-factory
version: 1.0.0
description: Finds gaps in app ecosystems, builds services on a shared base, runs them.
realm: new                 # or: existing
needs:                     # filled in or approved at install
  secrets: [CLOUD_TOKEN, PADDLE_API_KEY, GITHUB_TOKEN]
  workers: dedicated       # or: any
  grants:
    - {from: commons, mode: read, scope: "memex: fact"}
contents:
  collections: [...]
  series: [...]
  views: [...]
  skills: [...]
  agents: [...]
  templates: [...]
  workflows: [...]
  triggers: [...]
  policies: [...]
  budgets: [...]
  memory: [...]
  decision_groups: [...]
```

### 5.2 Installing, upgrading, exporting, removing

- **Install:** carbon6 shows what the package will create, as a decision. Then it creates the realm, asks you for the secrets and grants it needs, and creates its contents.
- **Upgrade:** a three-way comparison between the installed version, your edits since, and the new version.
  - Your edits are kept.
  - Conflicts come to you as a decision.
  - Schema changes follow the migration rule in 4.1.
- **Export:** a realm's configuration becomes a package, without its data or secret values. This is how an app you built by hand becomes one you can reinstall or share.
- **Remove:** a destructive decision. It offers an export of the data first, then deletes the realm and everything in it.
- **Distribution:** packages are files. There's no marketplace.

### 5.3 Agents writing apps

An agent can draft a package, using the `carbon6_api` reference and a skill that explains the format. Installing it is still your decision (5.2). This is how carbon6 grows new apps without anyone changing carbon6's code.

## 6. Worked examples

These test whether the primitives are general. Each app is written only in configuration; anything that couldn't be is recorded in 6.3.

### 6.1 Service factory

A realm that finds gaps in app ecosystems, builds small services on a shared base, launches them and runs them, with you approving at five gates.

- **Realm:** `factory`, with a dedicated worker for building and deploying. Model connections as you choose. No grant to home: it never sees your mail or photos. It reads commons for infrastructure facts.
- **Collections:**
  - `candidates`: the gap, the ecosystem, the evidence (links), a score per rubric criterion, the stage (`found → approved → validating → build → rejected`), and the decision ids for its gates.
  - `services`: the name, the candidate it came from, the repository, where it's deployed, its marketplace listing, the accounts and secret names it uses, the stage (`building → in_review → live → sunsetting → retired`), and its kill criteria.
  - `incidents`: the service, what happened, its severity, its state, and the sessions that worked on it.
- **Series:** `revenue`, `hosting_cost`, `model_cost`, `uptime` and `support_tickets`, each with the dimension `service`.
- **Skills:** the gap-finding rubric and its sources; the validation playbook; building on the base (the template repository); the launch checklist; operating a service; writing a gate's decision document; support replies.
- **Templates:**
  - `scan`: research gaps, then write `candidates`;
  - `deep-dive`: one candidate's case;
  - `validate`: a landing page or waitlist;
  - `build`;
  - `launch`;
  - `operate`: a daily check;
  - `triage`: one support message;
  - `sunset`: shut a service down.
- **Triggers:**
  - weekly → `scan`;
  - a candidate created with a score above the bar → `deep-dive` → decision "pursue?" (gate 1);
  - gate 1 answered yes → `validate` → decision "spend $X to test?" (gate 2), then "build or kill?";
  - a candidate entering `build` → `build` → decision "release?" (gate 3);
  - gate 3 answered yes → `launch` → decision "submit the listing?" (gate 4);
  - daily → `operate`;
  - a payment provider's webhook → a `revenue` point, mapped without a session;
  - an email to the support address (through a memex grant on the factory's own inbox) → `triage`;
  - a `revenue` threshold (its sum over 60 days below its costs) → `sunset` → decision (gate 5).
- **Views:** a board of candidates by stage; a board of services by stage; a page with revenue against costs per service; the incidents feed.
- **Policies:** ask before spending, submitting forms, publishing, deploying to production, or sending email outside approved templates. Deny deleting repositories.
- **Budgets:** model spend per month for the realm, plus per service via the `model_cost` series threshold.

### 6.2 Personal CRM

A realm that keeps you in touch with people who matter, using your mail without the CRM being able to read the rest of it.

- **Realm:** `crm`, on any worker.
- **Grants from home:**
  - **read** on people (things) and on mail metadata (sender, recipients, date, subject) for people in the CRM;
  - **call** on `mail.draft`, so the CRM can prepare emails. Sending stays with you: home's policy makes every send a decision.
- **Collections:**
  - `relationships`: a link to the person thing, the tier, how often you want to be in touch, the last contact, and notes;
  - `touchpoints`: the relationship, the date, the channel, a summary, and a follow-up.
- **Triggers:**
  - mail with a person in the CRM (a memex event through the grant) → a `touchpoints` record and an updated last contact, mapped without a session;
  - weekly → a session lists relationships past their cadence, drafts check-ins (`mail.draft` through the call grant), and asks one batched decision: "send these five?";
  - a touchpoint's follow-up date → a decision reminding you.
- **Views:** a table of relationships sorted by how overdue they are; a person's record with their touchpoints and mail.
- **Skills:** how to write a check-in in your voice; what counts as a touchpoint.

### 6.3 What the examples showed

- **Calling across realms** (sending mail on the CRM's behalf) needed the `call` grant mode. It's in 3.7 because of this example.
- **Series dimensions** (per service) are needed so one series covers a portfolio.
- **"Stopped arriving"** as a series event (uptime pings stop) is needed to detect a dead service.
- **A record's stage driving work** is covered by record triggers on field changes, with no special state-machine primitive. Schema enums keep the stages valid.
- **Decisions in batches** were needed by both apps: "send these five?", and a weekly digest of open gates.
- Neither example needed code in carbon6.

## 7. Guarantees

1. **Default deny between realms.** Only grants cross, with provenance and a log.
2. **One enforcement point.** The head decides every access to carbon6 data.
3. **Strict data.** Records and attributes match their schemas, and changes are versioned with who made them.
4. **Nothing irreversible without you.** Deletes, drops, uninstalls, spending (through policies), grant changes and app installs are decisions.
5. **Bounded automation.** Triggers have depth limits, rate limits and budgets.
6. **Behavior is visible and editable.** Every prompt carbon6 uses is configuration you can read and change.
7. **Honest limits.** What isn't isolated (3.9) is stated, and the UI says so where it matters.

## 8. What changes in today's features

In short; the details are in 12, and how existing data is upgraded is in 13.

| Today | Becomes |
|---|---|
| Labels as the only scope | Labels inside realms |
| Memex scopes by label | Realm + grants, with label scopes inside a realm |
| Tool-call approvals, action items, workflow approve steps, assistant questions | Kinds of decision |
| Automations (schedules, webhooks, memex events) | Triggers, with more events and actions |
| Usage and budgets | Series and thresholds |
| Settings shared by everything | Root defaults, inherited and overridden per realm |
| API tokens that reach everything (except sign-in) | Tokens scoped to realms; the root token reaches everything |
| Prompts in code (briefing guidance, action-item prompt, compaction) | Root skills you can edit |
| Environment facts in one pool | Facts per realm; shared ones through commons |

## 9. Build order

Each step is usable on its own. Each step also moves the features that duplicate its primitive onto it (12), and upgrades existing data (13), in the same step, so there is never a second way of doing the same thing for long.

0. **The upgrade machinery (13):** backups before migrating, restoring on failure, checks that compare before and after, the upgrade report, `c6h upgrade` and `c6m upgrade` (with `--check` to try a copy), and test databases in today's shape that every later step is tested against. Nothing visible changes.
1. **Realm plumbing:** root and home, a realm on every object, a session's realm, the head and the memex checking realms, realm-scoped API tokens. There is no way to create a second realm yet, so nothing visible changes.
2. **Collections:** schemas, records, history, the JSON filter queries, the agent tools, the memex mirror, generated forms, a table and record view. **Root → realm inheritance is built here, once,** on collections.
   - **Ports:** every list in Settings becomes a system collection with generated forms; hosts, memory notes, facts, commitments and trails become collections. carbon6's own prompts and the memex's understanding prompts become root skills.
3. **Decisions:** the primitive, batching and digests, answering by link.
   - **Ports:** tool-call approvals, action items, workflow approve steps, the assistant's questions, confirmations of destructive operations, the memex review queue and fact disputes.
4. **Realms you can create, grants and commons:** the realm switcher; workers and connections available per realm; the four grant modes, provenance, the log; the commons review queue. Grant requests are decisions (3), and realms differ by configuration (2).
5. **Triggers:** record, decision, series and session events; mappings; loop protection.
   - **Ports:** automations, cron schedules, webhooks and memex events become triggers; notifications become a subscriber to events.
6. **Series:** points, queries, thresholds.
   - **Ports:** usage, model prices, budgets, host checks and the memex's model-call budget.
7. **Views:** board, chart, feed, page and actions; each realm's menu.
   - **Ports:** the usage page and the decision list become views.
8. **The assistant as a program:** a template with the catalog tools and a root skill, running in a realm like any session.
9. **Apps:** the package format, install, upgrade, export, remove; a skill for agents writing packages.
10. **The examples:** the service factory and the personal CRM as example packages. Building them is the final check that no app needs code.

## 10. Decisions

| Question | Decision |
|---|---|
| Isolation unit | Realms: one per object, default deny, flows only through grants |
| Name | Realm |
| Nesting | Flat under root, for now |
| Assistant | One per realm; administration in root |
| Workers | Available to some realms, or dedicated to one |
| Apps | Configuration only; a missing primitive is added, never the app |
| Users | One |
| Sandboxing | Unchanged: not by default; dedicated workers when files must be isolated |
| Architecture | A microkernel: a small kernel of orthogonal primitives; system services built on its public operations (12) |
| Duplicated features | Rebuilt on their primitive in the step that builds it (12, 9) |
| Existing data | Upgraded automatically in each step, after a backup, all or nothing, nothing dropped (13) |
| Where existing configuration goes | What grants abilities (secrets, tools, programs) to home; restrictions (policies, budget caps) to root |
| Query language for collections | The small JSON filter (4.1) for now; a richer language only if apps need it |
| Settings forms | Generated from schema annotations (descriptions, examples, field order, presets as records). A hand-written form is deleted only when the generated one covers everything it did. |

## 11. Open questions

- **One person in two realms:** whether commons should hold shared identities (people, organizations) by default, or only by explicit linking.
- **Claude Code sessions:** they reach carbon6 through MCP. The realm checks are in the head, so they hold, but their tool set needs the same realm-aware tools.
- **Charts and pages:** whether the view kinds in 4.5 cover what apps need, or a page kind with richer layout is needed.
- **Records at scale:** the head's SQLite fits personal-scale apps. If an app needs millions of records, collections may need their own store.

## 12. Revisiting what's built

carbon6 follows a microkernel and Unix approach. A small kernel provides orthogonal primitives, and system services are built by combining them through the same public operations apps use. The test for anything built: **could it have been written on the public API, or does it reach into private internals?**

### 12.1 The kernel: stays, becomes realm-aware

- **Processes and IPC:** the fabric and registry (workers and memex nodes), the session runtime with journaling and sync, live streams.
- **Identity and security:** sign-in, API tokens, the PKI and TLS, secrets, and policies. Policies are the kernel's permission check on tool calls; an "ask" opens a decision.
- **The system-call table:** the catalog. A test already checks that every route is an operation. The UI, the CLI, the assistant, the API reference and the OpenAPI document all go through it.
- **Exec:** templates, which say how to start a process. Fleet runs are a template with several targets.
- **Files:** outputs.
- **The new primitives:** realms and grants, collections, decisions, triggers, series and views.
- **A small search index** for sessions, because the memex is optional (12.4).

### 12.2 Duplicates of a primitive: rebuild on it

| Built today | Duplicates | Becomes |
|---|---|---|
| Every list in Settings: MCP servers, skills, commands, agents, hooks, templates, workflows, notification channels, policies, model routes, prices, budgets, memex folders and scopes. Each has its own JSON blob, routes and hand-written form. | Collections | System collections, inherited from root and overridden per realm by one mechanism. Forms are generated from each schema. Presets and inline help become schema annotations (descriptions, examples) and preset records. |
| Single settings: the public URL, web search, the assistant's model, memex settings | Collections | Records in a `config` system collection |
| Tool-call approvals; action items (`todos`); workflow approve steps; the assistant's questions; the memex review queue (merge suggestions, low-confidence links, naming faces); fact disputes | Decisions | Kinds of decision: one list, one set of notifications, one way to answer |
| Automations, cron schedules, webhooks, memex events | Triggers | Triggers; schedules and webhooks are event sources |
| Usage, model prices, budgets, host checks, the memex's model-call budget | Series | Series and thresholds |
| Facts (a hand-built table with a strict schema, history and a memex mirror) | Collections | A `facts` collection; facts were the prototype of collections |
| Commitments, trails, the hosts inventory | Collections | Collections with links into the memex |
| Memory notes (`remember`, `forget`), memex notes (`memex_note`), facts (`knowledge_record`), action items (`todo_add`) | Overlapping tools | One knowledge model: memory notes become a collection whose records are always in briefs. Agents get `records_*`, `decision_request` and the memex tools, each doing one thing. |

#### As built (step 2b)

- **The lists are system collections:** skills, agents, commands, hooks, MCP servers, templates, workflows, policies, model routes, prices, budgets, notification channels, hosts, and memex folders and scopes.
  - **Where they live:** policies, budgets, routes and prices in root, the rest in home (`lists.rs`).
  - **Moving in:** migration `0030` splits each stored list into records, in order (a new `position` column), and keeps the original values in `legacy_settings`. Checks prove every entry arrived unchanged and in order.
  - **Schemas follow the code:** each system collection's schema comes from the entry's Rust type, refreshed at every start.
- **Reading and writing through the settings store:** the head's settings store routes these keys to the collections. Every existing reader, endpoint and form works unchanged.
  - **Reading** a list assembles it from records.
  - **Writing** one updates the records matched by each entry's identity (name, model or path; for skills, agents and commands, name and labels; for hooks, their event, matcher and command). Each entry keeps its own history.
- **Inheritance** (`lists::effective`): a session gets its realm's effective lists, used for its skills, agents, hooks, MCP servers, policies, commands and routing.
  - **Skills, agents, commands, MCP servers and the rest:** root's entries, with the realm's replacing root's of the same identity.
  - **Policies, budgets and hooks:** root's first, then the realm's.
  - **Routes:** the realm's first.
- **Memory notes:** records of the `memory` collection, keeping their ids. The table stays as `legacy_memories`. Remember, forget and the Settings form keep working, now with history.
- **Writing system collections:** system collections can be read through the collections API, but writes go through Settings (or remember/forget). A session therefore cannot add itself a hook or a policy through `records_put`.
- **Upgrades** now apply pending migrations one version at a time, with each version's checks run straight after it, while the tables those checks read still exist.

### 12.3 Services and apps: move outside the kernel

| Built today | Is really | Becomes |
|---|---|---|
| The assistant: runs inside the head on a hidden worker row, with its own tools | A program | A template with the catalog tools and a root skill, running in a realm (3.6). The kernel provides a way to run sessions inside the head, as a kind of worker. |
| Notifications and web push | A service | A subscriber to events; channels are a collection |
| Workflows | The shell | Kept as the way to combine programs. Approve steps are decisions, notify steps are trigger actions, and run state can be records. |
| The memex's understanding steps (summaries, topics, commitments, captions, system pages) | Services, with prompts in code | Prompts become root skills; jobs start from ingest events through triggers |
| Gmail, Calendar and Contacts sync | Drivers | Unchanged; they already store through the ingest handlers |
| The mail client, calendar and photos pages | Built-in apps | Their rich UIs stay, using only public operations. Mail rules, "needs reply" and triage become triggers and views. |

### 12.4 Search

The head has its own full-text index of sessions, and the memex has another. The memex is optional, so the head keeps a small kernel index. When a memex is connected, it is the search service, and session search goes through it.

### 12.5 Keeping the boundary

In a single Rust head, "services use only public operations" is a rule, checked in code review and by tests: services call the catalog's operations, not each other's internals. If it proves necessary, services can later run as separate processes that can only reach the catalog.

## 13. Upgrading existing data

carbon6 already holds real data. Every step of the build order upgrades it to the new representation automatically, when the new version starts.

### 13.1 Principles

1. **A backup first.** Before a step changes data, the head copies its database (SQLite `VACUUM INTO`) next to it, with the version and time in the name. The memex does the same for its store. Backups stay until you delete them. `master.key` is never touched.
2. **All or nothing.** Each step's migration runs in one transaction per database. If it fails, it rolls back and the head refuses to start, saying why. The database is unchanged, so the previous version still runs on it.
3. **Nothing is lost.** Every field moves somewhere. Anything without a place in the new shape is kept in a `legacy` field on the new record, never dropped.
4. **Identity is kept.** Session, item, thing and fact ids stay the same, so links, `@`-mentions and history keep working. Where an id has to change (action items' numbers become decision ids), a map is kept, so old references such as "action item #12" still resolve.
5. **Checked.** After migrating, counts and totals are compared per kind of object: settings entries, action items, automations, sessions, spend. An upgrade report is kept under **Settings → System → Upgrades**.
6. **Large data in the background.** Big memex tables (items, parts, vectors) get their realm in batches, with a progress marker, while the memex keeps serving. A step finishes only when every batch is done.
7. **Old tables stay for one release,** renamed `legacy_*` and read-only. A later release drops them only after you confirm, as a decision.
8. **Try it first:** `c6h upgrade --check` runs the migration on a copy of the database and prints the report without touching the real one; `c6h upgrade` upgrades without starting the head. The same goes for `c6m`.
9. **Going back** means restoring the backup. There is no automatic downgrade, because new data written after the upgrade has no old shape.
10. **Tested against real shapes.** Every migration is tested on databases created by each earlier version (synthetic data, never yours), from that version's schema through the latest.

### 13.1a As built (step 0)

- **The machinery** is `c6_core::upgrade` (feature `upgrade`), used by the head's and the memex's stores:
  - **Backup:** pending migrations are found by comparing the migrator with `_sqlx_migrations`. Before any runs, the database is copied with `VACUUM INTO` to `backups/<name>-v<from>-to-v<to>-<UTC time>.db`.
  - **Checks:** after migrating, every table's row count is compared with before. A table may shrink or vanish only if a pending migration names it in the store's `Plan` (`Allow`). `Check`s compare a query on the backup (attached as `before`) with one on the upgraded database.
  - **Restore:** a failed migration, lost rows or a failed check close the pool, put the backup back (removing the WAL files), and return an error naming the backup.
  - **Reports:** each upgrade's report is stored in the database's own `upgrades` table.
- **Commands:** `c6h upgrade [--check]` and `c6m upgrade [--check]`. `--check` copies the database to a temporary directory and upgrades the copy.
- **Seeing them:** `GET /api/v1/system/upgrades` (operation `system.upgrades`) and **Settings → System** list the head's and the memex's reports.
- **Test databases:**
  - `crates/c6-head/fixtures/c6h-v0027.db` and `crates/c6-memex/fixtures/memex-v0006.db` hold synthetic data in today's shape. A real head, worker and memex filled them through the API, then one synthetic row went into each table the API didn't reach. The head's includes a session waiting for approval and a workflow waiting at its approve step; the memex's includes a mail thread, documents, notes, a trail, and facts with a dispute.
  - **Regenerating them:** the `make_head_fixture` and `make_memex_fixture` tests (`--ignored`) regenerate them. Run them before a step changes a schema, so the old shape is kept as a new file.
  - **Testing against them:** `head_fixtures_upgrade_and_serve` and `memex_fixtures_upgrade_and_serve` upgrade every saved database (with `--check` first, then for real). They then serve it, by starting a head on it and opening the memex's store, and check that its data comes back.
- **Not built yet:** the `legacy_*` convention (13.1, rule 7) has nothing to keep so far. It arrives with the first step that moves data.

### 13.2 Head: what moves where

| Data today | After |
|---|---|
| `sessions`, `events`, `run_sessions` | Unchanged, in home |
| Settings lists: `agents`, `commands`, `skills`, `hooks`, `mcp_servers`, `templates`, `workflows`, `memex_folders`, `memex_scopes` | Records in system collections, in home (step 3) |
| Settings lists: `policies`, `budgets` | Records in system collections, in root, so every realm inherits them (step 3) |
| Settings lists: `routes`, `prices`, `channels`, `hosts` | Records in system collections, in root (routes, prices) or home (channels, hosts) (step 3) |
| Single settings: `public_url`, `web_search`, `assistant`, `memex` | Records in the `config` collection, in root (step 3) |
| `memories` | Records in the `memory` collection, in home (step 3) |
| `todos` | Decisions, with kinds and states mapped and an id map (step 4) |
| Pending tool-call approvals, and workflow runs waiting at an approve step (`workflow_runs`) | Open decisions, still answerable (step 4) |
| `automations`, `automation_runs` | Triggers and their runs. Webhook URLs keep working: their stored hashes move unchanged (step 5) |
| `usage` | Points in the spend and token series, by session, model and labels. Totals per day are checked against the old table (step 6) |
| `host_checks` | Points in a `host_check` series by host (step 6) |
| `secrets` | Home (step 1) |
| `api_tokens` | Root tokens, which reach every realm, as today. New tokens can be scoped (step 1) |
| `workers`, `connections` | Available to every realm, as today (step 1) |
| `passkeys`, `auth_sessions`, `login_links`, `enrollments`, `push_subscriptions`, `google_accounts` | Root, unchanged |
| `memex_outbox` | Unchanged; queued entries are for home |
| Search tables | Rebuilt with realms: they are derived |

### 13.3 Memex: what moves where

| Data today | After |
|---|---|
| Items, parts, vectors, things, links, evidence, trails, mail, calendars, contacts, media, faces, files, sync state | In home (step 1, in batches). Things are keyed per realm. |
| `facts` | A `facts` collection in home, in the head, keeping ids. The memex keeps its search mirror. This runs as a background job when the memex connects after the upgrade (step 3). |
| `commitments`, `trails` | Collections in home, the same way (step 3) |
| `review` | Open decisions (step 4) |
| `model_calls` | Points in the memex model-call series (step 6) |

### 13.4 Workers

Workers' journals hold sessions' events, and need no migration. The head decides a session's realm, so workers don't need to know it. New protocol fields are additive, so workers on the previous version keep working until they're updated.

### 13.5 The order across machines

- **Head first.** Until the memex is upgraded, the head treats it as home-only and leaves the realm features that need it (grants on memex data, commons) unavailable.
- **The memex next,** whenever it next starts on the new version. It migrates in the background (13.1, rule 6).
- **Workers whenever you like** (13.4).

## 14. Decisions made while building

Choices the design left open, made while building, for review.

| Step | Decision | Why |
|---|---|---|
| 0 | The upgrade machinery lives in `c6_core::upgrade` (feature `upgrade`), shared by the head and the memex | One implementation of backups, checks and restore for both databases |
| 0 | The fixtures are filled through the real API, then one synthetic row goes into each table the API didn't reach | Realistic shapes where it matters (settings, sessions, approvals), and full coverage everywhere else |
| 1 | The assistant's conversations move to root | They do carbon6's administration (3.6) |
| 1 | Root keeps no memex knowledge: its sessions' digests are no longer sent to the memex | Root is administration; the memex has no root partition |
| 1 | The memex gets realm columns and a gatekeeper in step 1; per-realm queries and things keyed per realm come in step 4 | Grants on memex data (step 4) redo the same queries; until a second realm can exist, only home's data is there and nothing can cross |
| 1 | A realm-scoped token may read workers, connections and their models, commands, the catalog and the OpenAPI document | It needs them to start sessions; they hold no realm's data |
| 1 | Usage, live updates, settings and workflows are root-only for tokens | They span realms, or are configuration (until step 2 makes configuration per realm) |
| 1 | A valid token scopes even with sign-in off | A script given a realm's token expects to stay in that realm |
| 1 | Secrets are asked for as `secret:NAME@<session>`, not with a new protocol field | Keeps the worker's per-session cache separate with no protocol change; older workers still get home's secrets |
| 1 | Tokens cannot be limited to root | A root token is one with no realm |
| 2a | The schema checker is written in carbon6, not a JSON Schema library | Collections need a strict subset (closed objects), clear messages, and no large new dependency |
| 2a | A schema change is allowed exactly when every existing record still fits | That is "additive" stated as a rule the head can check; anything else needs a migration (step 3) |
| 2a | Records are mirrored into the memex only for non-system collections | System collections are configuration (step 2b), not knowledge |
| 2a | Forms list required fields first, then the rest alphabetically | JSON objects come back sorted, so the schema's own order is lost |
| 2b | The head's settings store routes the list keys to system collections | Every endpoint, form and reader keeps working with no rewrite; only what builds a session's configuration asks for the realm's effective lists |
| 2b | Settings forms stay hand-written for now | They write the same records; generating them from the schemas (decision above) can replace them one by one once a generated form does everything a hand-written one does |
| 2b | The single settings (public URL, web search, the assistant's model, memex settings) stay as they are | They are root configuration, not lists; there is no gain yet in making them records |
| 2b | Facts, commitments and trails stay in the memex for now | They are already strict tables there; moving them into the head means rewriting the memex's knowledge code, which step 4 revisits along with per-realm memex queries |
| 2b | System collections are written through Settings and remember/forget only, never through the generic records API | Their writers check them; and a session must not be able to give itself hooks or loosen policies |
| 2b | Upgrades check each migration right after it runs | A later migration may rename a table an earlier check reads |
| 2c | carbon6's prompts are a `prompts` system collection in root, not root skills | Skills are things agents load by name; these are carbon6's own instructions, with defaults that follow the code until edited |
| 2c | A prompt nobody edited follows its default when the default changes; an edited one never does | Improvements reach you unless you chose otherwise |
| 2c | Tool descriptions stay in code | They describe what the code does, and change with it; editing one could misdescribe a tool |
| 3 | Decisions are the `todos` table, renamed and widened, not a new table | Ids stay the same by construction, so references keep working |
| 3 | An agent's `decision_request` never blocks; the answer arrives as a message | No new waiting machinery on workers; the agent keeps working, and the answer resumes it like any message |
| 3 | Tool-call approvals keep their own path (the session's buttons, the signed link) and open a decision alongside | The worker's approval protocol stays as it is; the decision is the one place you see everything, and either side settles the other |
| 3 | A question asking for a secret is never answered through decisions | Answers are stored; secrets must not be |
| 3 | The memex's review queue stays in the memex for now | It needs the memex to raise and resolve decisions over the fabric; step 4 revisits the memex |
| 3 | Digests are left for triggers (step 5) | A digest is a scheduled notification of open decisions, which is exactly a trigger |
| 4a | A realm is deleted only when it owns nothing | Removing a realm with its data belongs to an app's uninstall (step 9), with an export first |
| 4a | Per-realm settings use a request-scoped realm read by the settings store | Every settings endpoint and form works per realm without being rewritten |
| 4a | The realm switcher lives in the browser, and "All realms" shows everything | Single user: it is a view, not a permission |
| 4b | Another realm's memex knowledge is a separate database, not realm columns on shared tables | Isolation by construction: no query can leak, things are naturally per realm, a realm's knowledge is deleted with its file, and grants (4c) become reads across databases |
| 4b | Deleting a realm drops its memex knowledge too | A realm is deleted only when it owns no sessions, records or decisions; what the memex derived from them goes with it |
| 4c | A grant's `where` is kept as a second filter, not merged into the query's | Merging lets a query's own condition on the same field replace the grant's |
| 4c | Changing or deleting another realm's records has no grant mode | The design's modes are read, append, snapshot and call; a realm that should change records owns them |
| 4c | A memex read grant with no kinds allows what the session may see anyway | The grant opens the realm; the session's scope still decides the kinds, as in its own realm |
| 4c | A granted mail or calendar operation also lets that call see email and events | The grant names the operation; without the kinds it could not work |
| 4c | Grants are root-only in the API; agents ask with `grant_request` | Only you create grants (3.7) |
| 4c | Searching across granted realms merges hits by score, each tagged with its realm | One search answers across what the realm may read, and you can see where each hit came from |
| 4d | Commons is a realm like any other, made on upgrade | Collections, knowledge, briefs and grants work there unchanged; only its rules differ |
| 4d | Reading commons needs no grant row; a per-realm switch turns it off | "Every realm reads commons by default; you can turn that off per realm" (3.7), without a grant per realm to manage |
| 4d | A proposal is a decision holding the item; nothing is written to commons until you publish it | Nothing unreviewed is ever readable by other realms, and rejecting leaves nothing to clean up |
| 4d | Knowledge gets an append mode only for commons | Publishing facts needs it; a realm's knowledge is otherwise its own |
| 4d | A published fact enters commons as reviewed | You reviewed it as you published it |
| 4d | Commons' proposals are decisions in commons | The review queue belongs to commons, and you see all realms' decisions anyway |
| 4d | Deleting a realm revokes its grants instead of deleting them | The log of what flowed is kept |
| 5 | Automations became triggers by renaming their tables, not by moving them | Ids, webhook URLs and history stay by construction; `/api/v1/automations` keeps answering for scripts |
| 5 | Actions run in the request that caused them, not in the background | The chain of triggers rides along with the action (a task-local), and what you change is consistent when the call returns; a start action only starts sessions |
| 5 | The chain is not carried through sessions a start action begins | A session's later events are its own; the hourly limit still bounds triggers that start sessions that fire them |
| 5 | Built-in notifications stay as they are; triggers add a notify action for any event | The channels' event lists already are a subscription; a trigger is how you subscribe to anything else |
| 5 | Changes to system collections (settings) are not record events | They are written through Settings, not as records, and firing on configuration changes invites loops |
| 5 | Series events and "record a value" wait for series (step 6) | They need the primitive |
| 5 | Per-trigger budgets are the existing budgets on the `automation:<name>` label | Start actions' sessions already carry it; other actions cost nothing |
| 6 | Model usage stays in its ledger and is offered as computed series, not copied into points | The ledger is keyed per call (exactly-once from workers), and cost is applied at today's prices so fixing a price corrects history; stored spend points could not do either |
| 6 | Budgets stay budgets (a list in Settings that stops spending) | They already check the same spend the `model_spend` series shows; thresholds on `model_spend` add warnings and actions on top |
| 6 | Host check history goes into home | Hosts are your infrastructure, and home's triggers can then watch them; root holds no data |
| 6 | Thresholds fire on the change (crossed, cleared), not on every check | A threshold that stays crossed would otherwise notify every minute |
| 6 | A threshold with nothing recorded counts from when the trigger was made, for silence | A heartbeat that never started is as silent as one that stopped |
| 6 | Series aren't covered by grants yet | No example needed series across realms; a grant of kind series can follow the collection grant's shape when one does |
| 6 | The memex's model-call budget stays in the memex | It is the memex's own database; it can become a series when the memex reports points to the head |

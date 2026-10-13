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


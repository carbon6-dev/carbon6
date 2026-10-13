# Follow-through and environment knowledge — Design

> **Status (2026-10-13):** follow-through (§2–4: reactions, next steps, trackers, kits), self-configuration (§5) and playbooks were built and then **removed**: they did not land well. **Environment knowledge (§6) and action items (§7) are built** and stay. The rest of this document is the design as it was, kept for a later attempt.

A coding session doesn't end at `git push`. The push creates things outside carbon6: a branch, a pull request, a CI run, and later perhaps a deployment. Each has its own lifecycle. A session should **follow through**:
- offer the obvious next step (open a PR);
- keep track of what happens next (the CI run);
- do so in the conversation, while the work goes on.

None of this is built into carbon6. Which CI a project uses, where it runs, how to reach it and how to read it is **per project**. Agents discover it themselves, configure carbon6 for it, and curate what they learn as **environment knowledge** that every later session starts with.

The first real case: GitHub, plus in-house CI. The CI runs in our own cloud, polls GitHub, starts Argo workflows, and is reachable only over WireGuard.

## 1. Decisions

| Question | Decision |
|---|---|
| What is built in? | Mechanisms only: reactions, next steps, trackers, kits, knowledge, action items. No CI system, forge or cloud is known to carbon6. |
| Who configures a project? | The agent: it discovers, writes the kit, dry-runs it, and records what it learned. You review when you choose. |
| Does the agent stop to ask? | No. Questions, doubts and things to confirm become **action items** in a list you work through when you want. The agent carries on with what it has. |
| Who sees environment knowledge? | Every session. Each fact says how it is known (§6.2) and where it applies (§6.3), so agents can tell what to rely on. |
| What gets written down? | Everything an agent figures out, labeled with how it knows it. |
| How long is a fact trusted? | By its volatility (§6.4): structural facts until contradicted; slow-changing ones re-checked after a while; volatile states are not facts at all. Every fact can be re-checked when it is used. |

## 2. Reactions and next steps

A **reaction** is a rule scoped by labels, usually `repo:…`:
- **When** a session does something. The trigger is a tool call (tool name, input pattern, success, output pattern) or a session event (turn ended, commit made).
- **Check:** optionally run something, a command or an MCP tool, on a chosen node (§4), whose output becomes facts for the rest of the rule.
- **Then** do one or more of:
  - suggest a next step;
  - start a tracker;
  - notify you;
  - tell the agent something (a line added to its context);
  - add an action item.

Templates use what the trigger and the check produced: `{{branch}}`, `{{sha}}`, `{{check.number}}`.

A **next step** is a suggestion shown under the turn as a button ("Create a PR for `feature-x`"). Clicking it sends it as your message. The agent sees open suggestions, so "yes, do that" works. Reactions make them, and the agent can propose them too (a `suggest` tool).

## 3. Trackers

A **tracker** follows one external thing for a while: a CI run, a deployment, a PR's reviews. It belongs to the session, anchored at the event that started it (the push), and appears there as a **live thread**: a side chat that updates in place while the main conversation continues.

Its definition is:
- **Where:** the session's worker; a worker with given labels (for example `net:ci`, for one inside WireGuard); or the head.
- **Probe:** a command, an MCP tool or an HTTP request whose output gives the state; or incoming webhooks matched by repo, branch and commit.
- **Phases:** from the probe's output.
  - `waiting`: the thing does not exist yet, as when CI has not picked up the push, with a time limit;
  - `running`: with progress;
  - a final outcome: `succeeded`, `failed` or `cancelled`.
- **Display:** one line ("CI: running · build ✓ · test 4/9") and a link.
- **Interval** and **give-up time**.
- **On the end:** notify; suggest a next step; for a failure, open an **investigation** in the tracker's thread. The investigating agent:
  - reads the logs (with the probe's means) and explains the cause;
  - **proposes a fix**: it makes the change on a new branch off the failing commit (`fix/<branch>-<sha>`), commits it there, and shows the diff;
  - does not push. It offers next steps to push the fix branch (and open a PR), or to apply the change to your branch.

The main agent can read every tracker's state ("CI for `abc123` failed in `test`"). Tracker updates are not added to the main conversation's context unless the agent asks for them or the tracker ends.

**Every observation a tracker makes is kept, like sessions**: each change of phase or progress, with its time and the probe's output. A tracker's thread can be read back in full at any time, and its history is what "this step often fails" patterns are learned from (§6.4).

## 4. Project kits

A **kit** is everything that makes a project work in carbon6, grouped under its labels:
- reactions and trackers;
- and the existing settings that apply to those labels: skills, MCP servers, hooks, secrets (by name), policies and routes.

Settings gets a project view: for a label, everything that applies to it, in one place. A kit can be copied to start another project.

Each kit has a **state**:
- `proposed`: written, not yet proven;
- `active`: proven by a dry run;
- `broken`: a check or probe fails;
- `disabled`.

**Activation:**
- A kit whose actions only read, suggest and notify (the usual case: checks, probes, suggestions, investigations) becomes active as soon as its dry run passes, with an action item for you to review it when you like.
- A kit that would change things on its own (push, comment on a PR, deploy, or change secrets or policies) waits for your approval.

## 5. Self-configuration

**Worker sessions get the operation tools** (`c6_discover`, `c6_describe`, `c6_call`; see docs/assistant.md), so the agent that has the repo, `gh` and the network can configure carbon6:
- Reads are free.
- Writes follow the kit rules above: anything that would run commands on its own is proposed and dry-run.
- Policies and secrets change only with your approval.
- A session can never widen its own permissions.

**The setup playbook** is a built-in, generic procedure (a skill), not knowledge of particular systems:
1. **Read what's known:** the memex's environment knowledge for this repo and the systems it touches. On a second repo using the same CI, setup only verifies and adapts.
2. **The repo:** CI, deploy and infrastructure files (workflow templates, Kubernetes and Helm, Makefiles, docs), for what runs, where, and how a run is tied to a commit.
3. **The forge:** commit statuses and checks on a recent commit (which name the CI and link to it), required checks, webhooks.
4. **The network:** which workers can reach the CI's address, with which CLIs and credentials.
5. **carbon6:** existing secrets, hosts, worker labels and kits.
6. **Write:** the kit; facts (§6) for everything learned, with evidence; action items for what it could not settle (a missing credential, an ambiguity).
7. **Dry-run** on a recent commit, then activate (§4) or mark the kit broken with an action item.

**When it runs:**
- Automatically, once per repo, in a side chat of the first session in a repo with no kit.
- When you ask ("set up follow-through for this repo").
- When a kit breaks (self-repair). A tracker that cannot find its run, or a credential that is rejected, starts a repair. The repair compares the facts with what it now sees, updates both, and adds an action item saying what changed.

## 6. Environment knowledge

### 6.1 What is kept

All of it lives in the memex.

- **Systems** are things:
  - `ci:argo@internal`, `cluster:build`, `network:wireguard`, `worker:build-1`, `repo:github.com/you/app`;
  - kinds include `system`, `service`, `cluster`, `network`, `host`, `worker`, `repo`.
- **Relationships** are links:
  - `repo` *built by* `ci`;
  - `ci` *runs in* `cluster`;
  - `cluster` *reachable from* `worker` *over* `network`;
  - `ci` *reports to* `github`.
- **Facts** are claims about a system, with their evidence, status, scope and volatility (below). For example: "workflows carry the commit as label `git-sha`".
- **Procedures** are short how-tos linked to a system ("get a failed step's logs", "re-run CI"), kept as notes or trails.
- **System pages** are a living summary per system, written by the memex's understanding model from its facts, and editable by you.

Never values of secrets: only the name of the carbon6 secret that holds one.

### 6.2 How a fact is known (its status)

Every fact carries one:

| Status | Meaning | Rely on it? |
|---|---|---|
| `reviewed` | You confirmed it | Yes |
| `verified` | Re-checked by its recorded check, with evidence (command, output excerpt, when, where) | Yes, within its volatility |
| `observed` | Seen once by a session, with evidence; not re-checked | As a strong hint; re-check before acting on it |
| `inferred` | Concluded from documents or other facts, which it links to | As a hypothesis; verify first |
| `disputed` | A newer observation contradicts it | No; an action item is open |
| `retired` | No longer true (kept, with when it stopped being true) | No |

Agents write everything they learn: as `observed` when they saw it, as `inferred` when they concluded it. A fact moves up when a check confirms it or you review it, and down when something contradicts it.

### 6.3 Where a fact applies (its scope)

A fact is about one or more systems, and may have **conditions**:
- labels (`repo:…`, `env:prod`);
- a branch;
- a namespace;
- a time ("since 2026-09");
- free text ("for release builds").

Agents also record **negative facts**, such as "the Argo label convention does not apply to `repo:…/legacy`, which builds with Make", so a wrong generalization is not repeated.

Briefs and searches show each fact with its status, age and scope. Agents are told, in their instructions, to treat facts as evidence with a status, not as truth, and to record contradictions instead of silently working around them.

### 6.4 How long a fact is trusted (its volatility)

The agent that records a fact assigns its volatility; curation can adjust it.

- **`stable`:** structural facts that change only when someone redesigns something, such as which CI system a repo uses or how runs are labeled. They are trusted until contradicted, and re-checked opportunistically when they are used.
- **`slow`:** addresses, versions, credential names, which worker reaches what. They become due for a re-check after 30 days.
- **`volatile`:** current states, such as "CI is failing" or "the cluster is degraded". These are not facts: trackers and sessions keep them as observations (forever, like sessions), and only their patterns become facts ("the `e2e` step often fails on Mondays").

Any fact with a **check** (the command that proves it) is re-checked by the next session that relies on it once it is due, and that session records the result. A fact without a check stays at its status, and ages visibly in briefs.

### 6.5 Curation

- **New facts replace old ones.** A new observation about the same thing ends the old fact's validity (`valid_to`, which links already have) instead of standing beside it.
- **Contradictions become `disputed`,** with an action item; nothing is overwritten silently.
- **System pages** are rewritten from current facts when they change.
- **Duplicates** (the same claim found by two sessions) are merged, keeping both pieces of evidence.

### 6.6 Use

- **Session briefs** include the repo's environment knowledge: the systems around `repo:…` in the graph, their pages, key procedures, and facts with their status and age.
- **The setup playbook and self-repair** start from it.
- **Any session can search or ask** about it ("how do I get CI logs for app?"), as can the assistant.

## 7. Action items

A list of things for you to settle, when you want to, with their context. It's carbon6's to-do list, not only knowledge's.

**Kinds of item:**
- confirm a fact (`observed` or `inferred` ones that matter);
- resolve a dispute;
- review a kit (activated, or waiting for approval);
- provide something (a credential);
- answer a question an agent could not settle;
- follow up on something (an agent's "you may want to…").

**What each item carries:**
- its context: links to the facts, systems, sessions and events behind it, and the evidence;
- who made it and when;
- a priority.

Items stay listed until you settle them: on an Action items page, as an inbox section, and in a count in the header. Similar items are grouped, so one dispute doesn't become five.

**To settle an item:**
- **Answer inline** for simple ones: confirm, correct, retire, "doesn't apply here", provide.
- **Or start a session from it:** carbon6 starts a session, on the right worker for the systems involved or with the assistant, with the item's context loaded and the job of helping you settle it. The outcome is recorded on the item and on the facts.
- **Snooze or dismiss** it.

Agents never wait on action items. They work with what they know, with its status, and the item records what they assumed.

## 8. Build order

1. **Mechanism:**
   - reactions (tool and input matching, checks on a chosen node);
   - next steps;
   - trackers (polling, the `where`, phases, live threads, investigations on failure);
   - kits with state and dry runs;
   - the project view in Settings.
2. **Knowledge and action items:**
   - facts with status, scope, volatility and evidence;
   - a memex tool to record facts;
   - curation (replacing, disputes, system pages, re-checks when used);
   - knowledge in briefs;
   - the action items list, with sessions started from items.
3. **Self-configuration:**
   - operation tools in worker sessions, with the write rules;
   - the setup playbook (automatic once per repo);
   - self-repair.
4. **Push:** webhook-fed trackers, when CI can reach carbon6.

## 9. As built

Only environment knowledge and action items remain.

- **Knowledge:**
  - Facts are the memex's `facts` table. Each is mirrored as a searchable `fact` item linked to its systems; `fact` is in every session's scope.
  - Sessions use `knowledge_record`, `knowledge_verify`, `knowledge_list` and `knowledge_set`. Observed facts need evidence. The same claim (normalized) about the same systems adds evidence instead of a duplicate. `contradicts` disputes a fact and raises an action item; `replaces` retires one.
  - Briefs include an "Environment knowledge" section: facts about the session's things and the systems they mention, ranked by status. Facts for other labels come last and are marked.
  - System pages are written from facts by the understanding model, within its budget.
- **Action items:**
  - Stored in the head's `todos` table: `GET`/`POST /api/v1/todos`, `PATCH /api/v1/todos/{id}`, and `POST /api/v1/todos/{id}/session`, which starts a session with the item's context on the worker it names.
  - Sources: sessions (`todo_add` and `todo_resolve`) and the memex (disputes).
  - The UI has the To do page (with a count in the header), and facts with Confirm, Correct and "No longer true" on items and on system pages.
- **Removed:**
  - kits, reactions, next steps, trackers, investigations, dry runs;
  - the worker's `cmd.run`, and `UserInput.context`;
  - sessions' `c6_discover`/`c6_describe`/`c6_call`, the setup and repair side chats, and playbooks.
  - Their head tables and settings are dropped by migration `0027`.

## 10. Settled since

- Investigations propose fixes, on a branch of their own, without pushing (§3).
- Tracker observations are kept forever, like sessions (§3).
- Kits that only read, suggest and notify switch on by themselves after a dry run (§4); setup runs once per repo automatically (§5).

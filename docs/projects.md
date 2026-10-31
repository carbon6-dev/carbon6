# Projects: long-running efforts made of work — Design

Status: proposed (2026-10-30). Decided with you:
- a **project** is a long-running effort (an epic) made of work items that each finish quickly, plus what outlives them: docs, decks, a plan, phases;
- **phases are optional**: a project can be just its work and its docs;
- **sessions belong to a project only through work.** Planning is a work item too ("Plan phase 2");
- **the agent guesses a new work item's project**, as it guesses a session's work, and "Find work in what's open" proposes projects as well;
- projects **refer to docs and decks** (docs/docs.md), which people and agents write and keep changing, and which export to Google Docs and Slides or standard files.

## 1. The problem

Work (docs/work.md) is deliberately short-lived: you finish it and close it. Some efforts last months: shipping realms to a team, a migration, a book. They're made of many pieces of work, done one after another or side by side, and they carry things no single piece owns: the plan, the design doc, the deck for the review, the decisions made along the way. A label can group the work, but it can't hold the plan, say which phase you're in, or tell you the effort has stalled while every piece of it looks done.

## 2. The levels

| | Lifetime | Holds | Its steps | Closed by |
|---|---|---|---|---|
| **Session** | minutes to hours | a conversation | turns | its turn ending |
| **Work** | days | sessions, requests, threads, PRs, docs | to-dos, each a session | you, after "looks done" |
| **Project** | weeks to months | work, docs and decks, links | phases (optional), each made of work | you, after "looks done" |

Each level does the one above it the same way: a to-do starts a session for its work, and a phase's next steps become work for its project.

## 3. What a project is

| Field | |
|---|---|
| title | "Realms for the team" |
| **purpose** | what it's for and what success means: "The team uses realms daily; 3 people on it by December" |
| **phases** | optional. An ordered list, each with a title, "done when", an optional target date, and a state (upcoming, current, done). At most one is current |
| **work** | the work items in it. A work item belongs to at most one project, and may name one of its phases |
| **documents** | the docs and decks it refers to, each with a role: `plan`, `spec`, `notes`, `review` or none. At most one `plan` |
| **links** | records, decisions, outside URLs |
| **where we are** | a short note: the current phase, what's moving, what's stuck, the next decision (§6) |
| status | active, paused, done or dropped. You set it |
| realm, labels | as for work |
| opened, last moved, closed | times |

**Projects are flat.** A project doesn't contain projects, and work items don't depend on each other.

**Documents belong to the project, not to its work.** A doc or deck made while doing a piece of work (the design doc of phase 1, the deck for its review) is attached to the project, so it stays when the work closes. Work can refer to the same document too. A project's **plan** is a doc like any other: made with the project from the editable template `project.plan` (purpose, phases, open questions), and changed by you and by sessions.

## 4. Its state

Worked out on every read from its work, its phases and its docs, the first that holds:

| State | When |
|---|---|
| **needs you** | any of its work needs you; a phase offers "looks done"; the project offers "looks done"; an agent answered a doc comment you haven't seen |
| **moving** | some of its work is moving or waiting on others |
| **stalled** | it's active, and **no work in it is open**. Everything may be done, but nothing is advancing the project. The project's version of "gone quiet" |
| **gone quiet** | its open work has all gone quiet |
| **on track** | otherwise |
| **paused / done / dropped** | you set it |

A phase whose target date (a date on the phase, or a date chip the plan gives it) has passed while the phase is still current shows **late** next to the state. Dates are the only scheduling projects do.

## 5. How work joins a project

- **New work** (made by hand, by "Keep as work", by a task card or by the session form's "new") gets a project guess, like a session's work guess: an agent reads the title and what it was made from, and picks an active project or none. The guess shows on the work item, and one click changes it. The prompt is the editable `project.attach`. It only runs when the realm has active projects.
- **Work started from a project** (the project's "+ Work", or a phase's next step) is in it, with that phase.
- **Find work in what's open** (the Work panel) also proposes projects: groups of open and recently closed work, and labels that already work as projects, each with a title, purpose and the work in it, for you to accept or skip. The prompt is `project.propose`.
- **You** can move work in or out of a project, set its phase, merge two projects, or split one.

## 6. Where we are

The note answers "where does this stand?": the current phase and its progress, what's moving, what's stuck or waiting, the next decision, and what changed since you last looked. An agent rewrites it when the project moves: a work item in it opens, closes or needs you; a phase changes; its plan doc changes. Not on every session event, which keeps it cheap. The prompt is the editable `project.summary`.

## 7. Phases

- A phase's state changes only when you say so. When all work in the current phase is closed, an agent offers **"phase looks done"** with the evidence: the closed work and what each one ended with. Accepting it makes the next phase current, and offers to bring the plan doc up to date (a session started with `project.plan_update`).
- **Next steps.** "+ Work for this phase" makes a work item in the phase. **Plan the next phase** makes a work item "Plan <phase>", whose session reads the plan and proposes the phase's work as to-dos or new work items for you to accept.
- A project without phases skips all of this. Its "looks done" comes from its purpose: the agent offers it when the work is closed and the evidence matches what success means.

## 8. Context for sessions

The most useful thing a project gives is **context**. A session started in a work item that is in a project gets, in its first message, the project's purpose, the current phase, the "where we are" note and a list of its docs and decks with their roles. It reads the ones it needs with the `doc` tool (docs/docs.md §9), instead of being told the background again by you. The text is the editable prompt `project.context`, and a realm can turn it off.

The `work` tool's `get` includes the project. A session may attach a doc, deck or link it made to the **project** (`attach` with `to: "project"`), so what it made outlives its work. It can't change a project's purpose, phases or status, or close it.

## 9. Where you see it

- **The Projects panel** (`/projects`, in the header menu, in a tile of its own): active projects by state, each with its phase strip, "where we are"'s first line, the count of open work, and how long since it moved. Paused and closed projects are behind a checkbox.
- **A project** (`/projects/<id>`):
  - the purpose and "where we are" with ↻;
  - the phase strip, the current phase marked, with its target date;
  - open work grouped by phase (or flat), each row as in the Work panel;
  - docs and decks with their roles, the plan first, each opening in a panel beside it, with Export (Google Docs and Slides, or files) on each;
  - links, then closed work as history;
  - actions: + Work, Plan the next phase, Pause, Done, Drop, Merge, Reopen, and the "looks done" banners.
- **The Work panel** groups or filters by project, and each row shows its project.
- **A session's header** shows `Project › Work`.
- **The Brief's "Left open"** section includes stalled projects and projects that need you.
- **Nudges** (Settings → Work): a notification when a project stalls or a phase goes late, once per state until it moves.

## 10. How it's built

On the primitives, as work is:
- **Records:** projects are the system collection `projects` in each realm (`api::projects::ProjectFields`). The head writes them, each change a version in the record's history. Work's `WorkFields` gains `project` and `phase`.
- **Refs:** `project:<id>` and `doc:<id>` join the refs Work already knows, for chips, links and the assistant.
- **State** is worked out on read from the work's states (one batched call per realm, as Work does for requests). Triggers on work changes and `doc.changed` mark the project moved and bring its note up to date.
- **Agent steps** (`project.attach`, `project.summary`, `project.propose`, phase and project "looks done") are model calls routed by the rule for `task:projects`, else `task:work`. They are recorded in usage as `projects:<step>` and shown on the Activity page.
- **API:** `GET/POST /api/v1/projects`, `GET/PATCH /api/v1/projects/{id}`, `POST …/work`, `…/docs`, `…/links`, `…/detach`, `…/merge`, `…/refresh`, `…/phases` (add, edit, reorder, advance), and the project fields in `GET/PATCH /api/v1/work/{id}`. All are in the catalog for the assistant.

## 11. Build order

Projects need documents (docs/docs.md §14) up to step 2 for the plan and doc refs. Before that, a project's plan is a link.

1. The `projects` collection, the project page and panel, moving work in and out, `Project › Work` in session headers and the Work panel.
2. State from its work, "stalled", "where we are" and nudges.
3. Documents on projects: roles, the plan doc from `project.plan`, `doc:` refs, attaching to the project from the `work` tool.
4. Phases: the strip, work per phase, "phase looks done", Plan the next phase, late phases.
5. Context for sessions (`project.context`).
6. The project guess for new work, and project proposals in "Find work in what's open".

## 12. Not doing

- No nested projects, dependencies between work items, priorities, Gantt charts or estimates. Phases may have a target date; that's all.
- No automatic closing of phases or projects. Nothing closes without you, as with work.
- No progress percentages. A phase shows "3 of 5 work items closed", which is a count, not a promise.
- Docs/work.md §2 said work is flat with "no projects". Work itself stays flat; projects sit above it.

## 13. As built

### Step 1: projects, the panel and page, moving work (2026-10-31)

- **Records.** Projects are records of the system collection `projects` in each realm (`api::projects::ProjectFields`: title, purpose, status, phases, documents, links, note, offer, moved_at, told, closed_at), written by the head (`crates/c6-head/src/projects.rs`), each change a version in the record's history; the Collections page shows them read-only. Root keeps none. `moved_at` changes without a version when work moves in or out. Phases, documents with roles, links and the note have their fields now and their steps later.
- **Work in a project.** A work item names its project and phase in its own fields (`WorkFields.project`, `.phase`), so it is in one project at most and moving it in moves it out of any other. It moves in from the project (`POST /api/v1/projects/{id}/work` with a phase or none), from the work item (`PATCH /api/v1/work/{id}` with `project`, `""` for out, and `phase`), or when a project is made with `work`; it moves out with `POST …/detach` (`work`) or the work item's menu. A project and its work are in one realm; a phase the project doesn't have is refused. Every work item comes back with its project's title (`project_title`).
- **Merging** (`POST …/merge`, `from`): the other's work, documents (one plan at most: this one's wins) and links join, its purpose if this has none; the other is deleted, its history kept.
- **State.** For now a project's state is its status (`on_track` while active), with how many pieces of work are open as its "why"; docs/projects.md §4's states come with step 2.
- **API and catalog:** `GET/POST /api/v1/projects` (`closed=true` for done and dropped), `GET/PATCH /api/v1/projects/{id}` (title, purpose, status, labels, dismiss an offer), `POST …/work`, `…/detach` (work, doc or ref), `…/merge`; operations `projects.list`, `.add`, `.get`, `.edit`, `.work`, `.detach`, `.merge` (area `projects`, realm-scoped).
- **UI** (`crates/c6-ui/src/projects.rs`): **Projects** in the header and the menu, before Work, counting those that need a look. The panel (`/projects`) lists active and paused projects (closed behind a checkbox) with their state, first line of "where we are" (else why), and counts; "+ New project" asks for a title and purpose and opens it. A project (`/projects/<id>`) shows its title and purpose (Edit), state, Pause / Done / Drop / Merge… (Resume or Reopen once not active), its open work with each item's state, why and ✕ (out of the project), "+ Bring in open work…", its documents, and closed work below.
- **Elsewhere.** The Work panel shows each row's project (◇) and filters by project ("Every project", "In no project", or one). A work item shows "Project:" with a link and a menu to move it. A session's header reads `Work: <Project> › <Work>`.
- **Not built in this step:** phases and the strip, "where we are", project state from its work (stalled, gone quiet, needs you), nudges, documents with roles and the plan (steps 2–4); `links` (they have a field and can be detached, not yet added); splitting a project.

### Steps 2 and 3: state, where we are, nudges, documents and the plan (2026-10-31)

- **State** (`projects::state_of`, worked out on every read from its work as Work works it out): what you set (paused, done, dropped); **needs you** (its "looks done", a phase's, or a piece of work that needs you, with that work's why); **moving** (work moving or waiting on others); **stalled** (active with no work open: "No work in it yet", or "All its work is closed, and nothing new is open"); **gone quiet** (its open work has all gone quiet); else **on track**. **Late** shows beside the state when the current phase's target date (YYYY-MM-DD, the head's time zone) has passed. A doc comment the agent answered joins "needs you" with comments.
- **Where we are** (`project.summary`, an editable prompt): a few lines on the current phase, what's moving, what's stuck, the next decision, and what changed. It is written in the background, one at a time per project, when the project moves: work moves in or out, opens or closes, a document joins it or changes role, a document of it settles a change (`doc.changed`), or you press ↻ (`POST …/refresh`, `projects.refresh`). Not on every session event. The model is a routing rule's for `task:projects`, else `task:work`'s, else the latest session's of its work; usage is recorded as `projects:where we are` and shows on the Activity page. It reads each work item's state, why and next step, the phases and the documents. The same answer may offer **"looks done"** (only once all its work is closed; a dismissed offer isn't made again for the same evidence), shown as a banner with Close it / Not yet. Realms that turn work notes off (Settings → Work) get no project notes either.
- **Nudges.** Every five minutes, with work's: a notification (event `work`, Settings → Work's nudges) when a project stalls or its current phase goes late, once per state until it moves (`told`).
- **Documents with roles** (`POST /api/v1/projects/{id}/docs`, `projects.docs`): a document of the realm joins with a role (plan, spec, notes, review, or none); posting it again changes the role; a new plan leaves the old one without a role, so there is one plan. The plan is listed first. `POST …/links` (`projects.links`) links a record (`record:<collection>:<id>`), decision, session or URL. Both come off with `…/detach`.
- **The plan.** A new project starts with a plan doc, "<title>: plan", from the editable template `project.plan` (title, purpose, phases, open questions), unless it is made with `plan: false`. It is a doc like any other.
- **Sessions and project documents.** A session whose work is in a project edits that project's documents without asking (as its work's). A document a session creates is put on its work's project too (no role). The `work` tool's `get` shows the project (title, purpose, where we are, documents with roles), and `attach` with `to: "project"` puts a `doc:<id>` (with a role) or a link on the project instead of the work.
- **UI.** The project page has the "looks done" banner, **Where we are** with ↻, a "late" mark, its **Documents** (role menu per document, ✕ to take one off, "+ Add a document…" with a role, "+ New doc" and "+ New deck" made for the project and opened beside), and **Links** (add with a name, ✕). The Brief's **Left open** lists projects that need you or stalled (◇) before work. A document's panel lists the projects it is in (with its role) under "Refers to it".
- **Seen with a real model.** On local qwen3.6 27b (a thinking model) the "where we are" call twice spent work's 2,048-token output budget on reasoning and gave no JSON (the head logs it and keeps the last note). Project notes now get 8,192 tokens, and the same model then wrote the note.

### Step 4 (milestone 11): phases, Plan the next phase, and project context (2026-10-31)

- **Phases** (`POST /api/v1/projects/{id}/phases`, `projects.phases`, one change at a time): `add` (title, done when, a target date YYYY-MM-DD; the first phase of a project starts current, later ones upcoming), `edit`, `remove` (its work stays in the project, in no phase), `move` (to a place), `start` (makes a phase current; the one that was becomes upcoming), `advance` (the current phase is done and the next not-done one current), `dismiss` (not done after all). A phase's state changes only through these: nothing closes a phase or a project by itself.
- **"Phase looks done."** The same `project.summary` call that writes "where we are" now also answers `phase_done` and `phase_evidence`; the head keeps the offer only when the current phase has work and all of it is closed. It shows as "<phase> looks done: <evidence>" (the project needs you) with **Next phase** (`advance`, the evidence kept on the done phase) and **Not yet** (`dismiss`; the same evidence isn't offered again). After Next phase the page offers **Update the plan**. Realms with customized `project.summary` prompts get phase offers only once their prompt asks for those two fields.
- **Plan the next phase** (`POST …/plan`, `projects.plan`): makes a work item "Plan <phase>" in the project and that phase (the phase named, else the next upcoming, else the current one) and returns the first message of a session on it, from the editable prompt **`project.plan_phase`** (the project, the phase, all phases, the plan, its documents and work), with the project's latest session to start like. The page opens the new-session form with it (worker, folder, connection and model from that session; you change anything before starting). With `update: true` it is "Update the plan after <phase>", by **`project.plan_update`** (the phase just done, the next, the evidence, the phase's work).
- **Proposed work.** The planning session proposes the phase's work with the `work` tool's new action **`propose`** (title, done when), allowed only for work in a project: each lands on the project (`ProjectFields.proposed`, in the session's work's phase, else the current one; at most 50 waiting; the same title once), shown under **Proposed work** with Accept (a work item in the project and that phase, `POST …/proposed`, `projects.proposed`) and Dismiss.
- **Project context** (`project.context`, an editable prompt; Settings → Work turns it off per realm, `WorkSettings.project_context`): a session started in a work item that is in an active project gets, after its first message, the project's purpose, the current phase with its "done when", "where we are", and its documents with ids and roles, so it reads what it needs with the `doc` tool. This applies to sessions started with `work` set to a work item (the new-session form, to-dos, Pick up, Plan the next phase) and to sessions a comment thread starts. A session started without a first message gets none (it would start a turn), and neither does one whose work is guessed after it starts.
- **UI.** The project page has the **phase strip** (each phase with "n of m closed" or "no work yet", its target date, ✓ when done, the current one ringed, **late** in red when its target has passed); click a phase to edit it, make it current, move it ← →, or remove it; "+ Phases" / "+ Phase" adds one. Open work is grouped by phase (done phases with nothing open are left out, then "In no phase"), each group with **+ Work** (a new work item in that phase) and **Plan it**; each row has a phase menu. Settings → Work has the project-context switch.
- **Fixed on the way:** the project page made its callbacks inside the part of the page that redraws, so a second change landing after the first redrew the page used a disposed callback and panicked (moving two work items between phases quickly did it). They are made once per page now.
- **Seen with a real model** (local qwen3.6 27b, on a scratch head, in the browser): on "Team onboarding" with phases Checklists and Roll out, Plan the next phase opened the form with the planning message and the project context and the project's last session's worker and model; the session read the checklists with the `doc` tool and proposed "Brief the team on the checklists" and "Run pilot cohort through both checklists and confirm first-week shipping", each with a concrete "done when", both in Roll out; both were accepted from the page. Closing Checklists' work brought "Checklists looks done: Release checklist written, Onboarding checklist written"; Next phase made Roll out current and offered Update the plan, which opened its form. The same model first gave the note as a JSON list of lines, which the head read as no note and so kept nothing (the phase offer included): a note given as lines is now joined, and an offer is kept even with no note. The session also called `doc` once with no action and recovered from the error.
- **Not built:** proposals as to-dos (the planning session's `todo` adds to-dos to its own planning work, not the phase); a phase's work moving on its own when the phase changes; "late" phases beyond the current one; nudges for a phase that looks done (it shows as needs you).

### Step 5 (milestone 12): the project guess and projects found in work (2026-10-31)

- **The project guess** (`project.attach`, an editable prompt): a new work item made by hand, from a session ("new", or the agent's own new work for a session) or as a kept task card gets an agent's guess of its project, in the background, only when the realm has active projects and Settings → Work lets agents pick. The model is a routing rule's for `task:projects`, else `task:work`'s, else the work's latest session's; with none, there is no guess. A guess naming no active project of the realm changes nothing, and a work item someone put in a project meanwhile keeps that one. The work item shows the project with **guessed** and **keep** (keeping, or moving it, makes it yours: `WorkFields.project_guess`). Work made for a project (+ Work, planning work, an accepted proposal; `POST …/work` now takes a `title` to make a new item in the project) gets no guess.
- **Projects found in your work** (`project.propose`, `POST /api/v1/projects/propose`, `projects.propose`): "Find work in what's open" on the Work panel also asks for projects: groups of three or more open or recently closed (30 days) work items in no project with one purpose, labels on several items counted as a hint, the projects you have listed so none is proposed twice; at most five, each with a title, a purpose and its work (only real work in no project, two or more). They show as **Projects found in your work**, each with **Make the project** (made with its work and a plan doc, opened beside) and **Skip**.
- **Seen with a real model** (local qwen3.6 27b routed for `task:projects`): "Pair each new hire with a mentor for their first week" was guessed into Team onboarding; "Renew the TLS certificate on the build server" stayed in no project. `project.propose` was tested with a stand-in model only.
- **Not built:** splitting a project; a guess for work moved out of a project; proposals that include labels as projects without their work.

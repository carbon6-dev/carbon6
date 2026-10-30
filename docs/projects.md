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

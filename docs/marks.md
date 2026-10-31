# Marks: telling sessions apart at a glance — Design

Status: built (2026-10-30). Decided with you:
- a session has more to tell apart than its own colour: **its worker, its work, its project, its realm, its repo, its model**;
- each **value** of each aspect (this worker, that piece of work) has a **mark**, the same everywhere it shows;
- twelve colours are too few: marks are **20 colours, plain or in one of 4 patterns** (horizontal stripes, vertical stripes, diagonal stripes, dots): 100 marks;
- the aspect is told by **shape**, the value by mark, so two dots in a row never need a legend.

## 1. Marks

A mark is a colour (`sky`, `navy`…) or a colour in a pattern (`navy/dots`). The colours: sky, emerald, amber, rose, violet, teal, orange, indigo, lime, pink, cyan, fuchsia (the original twelve), then red, yellow, blue, brown, navy, olive, maroon and slate. The patterns: `hstripes`, `vstripes`, `diagonal`, `dots`, drawn in the colour on a pale tint of it. New things take marks in order, all plain colours before any pattern (`c6_api::marks::all`), the one least used first.

A session's own mark is its `colour` (any mark now; old sessions keep theirs). Its tile's bar is a tint of the colour with the pattern faint over it; its dot, its rail row, its dock line and its list row wear the mark itself.

## 2. Aspects and shapes

| Aspect | Shape | Key | Mark when you've picked none |
|---|---|---|---|
| the session itself | ● circle | (its `colour`) | picked when it starts: least used among the realm's recent sessions |
| worker | ■ square | `worker:<id>` | picked when first seen: least used among workers |
| work | ◆ diamond | `work:<id>` | picked when first seen: least used among open work |
| project | ⬢ hexagon | `project:<id>` | picked when first seen: least used among active and paused projects |
| realm | ▲ triangle | `realm:<id>` | home sky, commons amber, others from the name (as before) |
| repo | ⬟ pentagon | `repo:<name>` | from the name |
| model | ⬭ pill | `model:<name>` | from the name |

Marks you or the head picked live in the head (`marks` table) and reach every browser with the live feed (`LiveEvent::Marks`), so a change shows everywhere at once. Taking away a repo's, model's or realm's mark goes back to the one from its name; a worker, work item or project gets one picked again. Sessions in the live list carry the open work they serve and its project (`Session.work`; subagents and side chats their tree's). A session belongs to a project only through its work (docs/projects.md).

## 3. Where they show

- **A tile's bar**: the session's dot, then its shown aspects as shapes; when the tile is wide (a container query) each is named too. Pointing at one dims every tile whose session doesn't share that value. Clicking one shows the value, the other sessions with it, a way to its page (worker, work), for work the "change…" control, and the palette.
- **A tile's left edge**: a stripe in its work's mark, so tiles of the same work line up by colour.
- **The rail, the dock, the sessions list**: the session's mark, and its shown aspects as small shapes (named chips in the list and the rail's card).
- **Workers** (list and page), **Work** (list, item page, the Brief's "Left open", a session's work line), **Projects** (list, page, its work, and project links in Work): the value's own mark, clickable to change on the worker, work and project pages and in the workers list.
- **The realm switcher**: each realm's triangle.

Which aspects show is a per-realm setting (`session_facets`, Settings → Appearance): worker, work, project and realm by default. A realm shows only when it isn't home, and not on a realm's own page.

## 4. API

- `GET /api/v1/marks`: every mark given, by key (`marks.list`). A realm's caller sees its own work's.
- `PUT /api/v1/marks` `{key, mark}`: gives a value its mark, `mark: ""` takes it away (`marks.set`).
- `PUT /api/v1/sessions/{id}/colour` `{colour}`: a session's mark (`sessions.colour.set`).
- `GET/PUT /api/v1/settings/session_facets` `{shown: [...]}` (`sessions.facets.get`/`put`).

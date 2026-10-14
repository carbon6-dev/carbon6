# Realm explorer — Design

A page per realm that shows what the realm holds, how its parts connect, and lets you walk from any object to the objects related to it, across data and knowledge alike. It's the "file manager" of the OS (docs/os.md): read-mostly, every object one click from its neighbors, and edits handed to the pages that already own them.

## 1. Goals

- **Inventory at a glance:** what a realm holds, how much, and what changed lately.
- **Navigation by relationship:** a record leads to the things it links to, the sessions about it, the triggers watching its collection, and the decisions it raised. A thing in the knowledge leads to the records that mention it.
- **One place for data and knowledge:** collections and series sit next to memex things, facts and items. Where they meet (record links such as `client:acme`, the memex mirror of records) is visible.
- **Information flow is visible:** grants in and out, commons, and the app the realm came from.
- **No new truth:** the explorer only reads existing APIs (plus one summary endpoint). Editing stays in Collections, Settings, Decisions and the rest; the explorer links there.

## 2. Where it lives

- **`/realms`: the map.** Every realm as a card (name, app, counts, last activity, spend this month), with grants drawn as arrows between them and commons beside them. "All realms" in the header lands here.
- **`/realms/<id>`: the explorer** of one realm. Picking a realm in the header switcher offers it, and the Views menu links to it ("Explore crm").
- **`/realms/<id>/<kind>/<id>`: an object** inside the explorer, so every object has an address you can bookmark, share in a tile, or give the assistant as context.

## 3. Layout

Three panes, collapsing to one on phones:

```
┌──────────────┬──────────────────────────────┬────────────────────────┐
│ Inventory    │ List / overview              │ Object                 │
│              │                              │                        │
│ Overview     │ (what the left pane picked:  │ (what the middle pane  │
│ Data         │  a table, a timeline,        │  picked: its fields,   │
│  collections │  a graph)                    │  and "Connected" —     │
│  series      │                              │  every related object, │
│ Knowledge    │                              │  grouped by kind)      │
│  things      │                              │                        │
│  facts       │                              │                        │
│  items       │                              │                        │
│ Work         │                              │                        │
│  sessions    │                              │                        │
│  decisions   │                              │                        │
│  triggers    │                              │                        │
│ Config       │                              │                        │
│  views…      │                              │                        │
│ Flow         │                              │                        │
│  grants      │                              │                        │
└──────────────┴──────────────────────────────┴────────────────────────┘
```

A filter box over the middle pane searches within what's shown, and a realm-wide search (records, knowledge, sessions) sits at the top.

## 4. The inventory (left pane)

Each entry shows a count, and a dot when something changed in the last day.

| Group | Entries | Source |
|---|---|---|
| Overview | (the realm's dashboard, 5) | summary endpoint |
| Data | each collection (record count); each series (points, last value) | collections, series |
| Knowledge | things by kind (people, organizations, systems, topics); facts by status; items by kind (email, document, note, session…); trails | memex graph, knowledge.list, memex.months |
| Work | sessions (running / waiting / done), decisions (open / answered), triggers (with failing ones flagged), workflow runs | sessions, decisions, triggers |
| Config | views, templates, skills, agents, commands, hooks, MCP servers, policies, budgets, prompts overridden, memory notes, secrets (names only) | settings lists (`?realm=`), with "inherited from root" shown separately |
| Flow | grants in, grants out, commons (published by this realm, read from it), the app and its version | grants, apps |
| Infrastructure | workers and connections that serve this realm | workers, connections |

## 5. The overview

The landing view of `/realms/<id>`:
- **Header:** name, description, the app (version, "upgrade available" when a newer package was loaded), reads-commons switch.
- **Activity:** the last day's events across the realm, in one timeline (records created or changed, decisions opened or answered, sessions finished, trigger runs, points recorded, knowledge added).
- **Health:** failing triggers, decisions waiting, sessions waiting for approval, thresholds crossed, budget use.
- **Shape:** the collections and series as tiles, sized by count, with their schemas a click away; the realm's top things (most linked).
- **Flow:** a small map of this realm's grants and commons.

## 6. Objects and "Connected"

Every object in the right pane shows its own content, then **Connected**: every related object, grouped by kind, each a link that opens it in the same pane (with back and forward). This is what makes it an explorer rather than a set of lists.

| Object | Shows | Connected |
|---|---|---|
| Collection | description, schema (as a field table), keys, counts per enum field | its records; views using it; triggers on it; grants on it; series with the same dimension names |
| Record | fields, links, labels, history (diffable versions) | things it links to (`client:acme`), records linking to it, sessions labelled `record:<id>`, decisions whose context names it, trigger runs it caused, its memex mirror item |
| Series | unit, dimensions, retention, a chart (zoomable window, group by dimension) | triggers with thresholds on it, triggers recording into it, views charting it |
| Thing (person, org, system…) | its summary, facts about it (with status and evidence), timeline | items mentioning it, related things (from the graph), records linking to it, sessions about it (by label) |
| Fact | claim, status, evidence, where it applies | its things, the session that recorded it, the decision that settled a dispute |
| Item (mail, document, note, session digest) | its text or summary, source | things it mentions, the record it mirrors (for records), the session it came from |
| Session | title, status, labels, cost | records it wrote (`created_by session:<id>`), decisions it raised, items and facts it produced, the trigger run that started it |
| Decision | document, options, answer | what it's about (record, grant, app, session), the trigger or session that opened it, what answering it started |
| Trigger | event, filter, action, recent runs (with their cause chain) | the collection, series or template it involves; triggers that cause it and that it causes (from run causes) |
| Grant | from, to, kind, scope, mode, uses | the collection or knowledge it opens; its use log |
| View | its definition, a live preview | the collection or series it shows |

## 7. Graph mode

A toggle on the middle pane draws the current neighborhood as a graph: the selected object in the middle, its Connected objects around it, colored by kind. Clicking a node recenters on it. Two built-in graphs:
- **Realm wiring:** collections, series, triggers, templates and views, with edges for "watches", "writes", "starts", "shows". It answers "what happens when a record changes here?" (the trigger chain, as config).
- **Knowledge neighborhood:** a thing and its related things, plus the records linked to each (memex graph plus record links).

Graphs are bounded (two hops, 60 nodes) and fall back to the lists.

## 8. What the head adds

The UI uses existing endpoints, except:

- **`GET /api/v1/realms/{id}/summary`:** counts per inventory entry, last-change times, health flags, spend this month, in one call (the left pane and overview would otherwise make ~20 requests).
- **`GET /api/v1/realms/{id}/connected?kind=&id=`:** the Connected groups for one object, computed in the head where the joins are cheap (record links, `record:` labels, `created_by`, decision contexts and refs, trigger runs' causes, grant scopes). Knowledge links come from the memex (`memex.related`, `memex.mentions`) for the realm, through its grants.
- **`GET /api/v1/realms/{id}/activity?since=`:** the merged timeline (record history, decisions, session ends, trigger runs, points, new knowledge).

All three answer realm-scoped tokens for their own realm (admitted like collections), so the assistant in a realm can use them too, and add operations to the catalog.

## 9. Build order

1. Summary and activity endpoints; `/realms` map with cards; `/realms/<id>` with the inventory and overview.
2. Lists and object panes for data (collections, records, series) and work (sessions, decisions, triggers), with the Connected endpoint for them.
3. Knowledge (things, facts, items) and its links to records.
4. Graph mode (wiring, then knowledge neighborhood).
5. Flow (grants, commons, apps) and the map's arrows.

## 10. Open questions

- **Root:** does root get an explorer (workers, connections, realms, root's lists), or only the map?
- **Home's personal data:** mail, calendar and photos already have their own pages. Should the explorer list them as item kinds (counts and links out), or leave them out to keep the explorer about structure?
- **Editing in place:** read-only with links out (proposed), or quick edits (a record's field, a decision's answer) in the object pane?

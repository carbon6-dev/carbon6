# Realms in the UI — Design

Status: built (2026-10-24). Decided:
- each realm is its own app at `/r/<realm>/…`, enforced by the head;
- administration is a separate console at `/admin`;
- a realm page shows only a count of what waits in other realms;
- the app opens on the last realm you used.

## 1. Why realms don't work today

docs/os.md §3 makes realms a boundary, and the head enforces it for realm tokens and the assistant. The UI doesn't use that boundary:

- **Your browser is root.** A signed-in browser sees every realm. The realm picker only adds `?realm=` to some requests: 34 of the UI's ~179 API calls.
- **So the picker is a filter that many pages ignore.** The live session list, the activity dock, the inbox, mission control, search, usage, analytics and the memex show all realms, or mix them.
- **Your browser's state is shared across realms.** Tiles, history, pinned sessions, finished-unseen sessions and the label filter stay the same when you switch. Pages change partly.
- **"All realms" mixes administration into everything.** Workers, connections and sign-in sit next to one realm's skills and secrets in the same Settings.

Fixing each call site doesn't last: every new page is another place to forget the realm. The boundary has to be the default, enforced where the data is.

## 2. The model: two places

**A realm** (`/r/<realm>/…`): everything you do in one realm. That covers its sessions, inbox, decisions, mission control, analytics, collections, views, memex, mail, assistant and its own settings. You can't reach another realm's data from there. You can switch realm, or go to the console.

**The console** (`/admin/…`): carbon6 itself. It manages realms, grants, workers, connections and routing, the defaults every realm inherits, spending across realms, apps and packages, sign-in and tokens, updates, upgrades and backups, and the administration assistant. It shows no realm's sessions or knowledge, only counts and totals.

- **Opening the app** (`/`) goes to the last realm this browser used, or home the first time.
- **Switching realm** loads the app again, rooted in the other realm. ⌘-click opens it in a new tab, so two realms can sit side by side in two tabs. The header always shows which realm a tab is in: its name and a colour.
- **Old links** (`/sessions/<id>`, notification links) still work. The head knows the object's realm and redirects to `/r/<realm>/sessions/<id>`.

## 3. Enforced by the head

- **A browser request names its place.** The UI sends `X-C6-Realm: <realm>` from a realm page, or `X-C6-Realm: admin` from the console. The head makes the caller:
  - `Caller::in_realm(realm)`: the same caller a realm's API token gets, with the same admission;
  - or root, for the console.
  
  A browser request that names neither is refused. A UI bug fails closed: it can't fall back to seeing everything.
- **Every operation has a scope, in the catalog.** Each `Op` gets one of:
  - **realm:** the caller's realm's data. Lists are filtered to it, an object of another realm is "not found", and new objects are created in it.
  - **shared:** infrastructure a realm uses but doesn't own, read-only and narrowed to what the realm may use. Examples: the workers and connections available to it, their models, the command list, the catalog, the list of realm names (for the switcher).
  - **admin:** the console only. A realm caller gets 403.
  - **device:** this browser's own business, allowed anywhere: push subscription, sign-in status, the version.

  `realms::admit` reads the scope from the catalog instead of its hand-kept list. Every route must have a catalog op (the API reference test already checks this), so every route has a scope.
- **Realm tokens and the assistant don't change.** They already use this caller; they just get the fuller list.
- **`?realm=` stays for root tokens and the CLI,** which can address any realm. The UI stops using it.

### 3.1 Operations by scope

| Area | Realm | Shared (read, narrowed) | Admin |
|---|---|---|---|
| Sessions | all session ops, search, session decisions, suggestions, grouping | | imports settings |
| Decisions, memory notes, collections, records, series, views | all | | |
| Memex, mail and calendar | all, in the realm's partition | | Google client id |
| Automation | templates, workflows and their runs, triggers | | |
| Settings lists (skills, agents, commands, hooks, MCP servers, policies, budgets, prompts, secrets, channels, views, templates, workflows, memex folders and scopes) | the realm's own entries, with what it inherits from root shown read-only | | root's entries (the defaults) |
| Realm toggles (web search, suggestions, assistant, subagent limits) | the realm's | | root's defaults |
| Usage, analytics | the realm's sessions only | | across realms |
| Explorer | its own realm | | every realm, the map |
| Workers | fs, git and terminals on a worker the realm may use | the workers it may use | enroll, revoke, delete, which realms |
| Connections, routing, prices | | connections and models it may use | create, test, delete, which realms; routes; prices |
| Hosts | check a host | the inventory | edit |
| Apps, packages, installs | its own apps' operations and accounts | | install, upgrade, remove, packages |
| Realms, grants | grants involving it (read) | realm names | create, edit, delete; grants |
| Notifications | test a channel | | public URL |
| Push, sign-in status, version | (device) | | |
| Sign-in (passkeys, browsers, tokens), updates, upgrades | | | all |

## 4. The realm app

- **The header:**
  - the realm's name and colour, and a switcher menu (your realms, then **Console**);
  - the nav: Sessions, Inbox, Decisions, Mission, Analytics, Collections, Views, Memex, Mail (when the realm has it), Settings, Ask;
  - "N waiting in other realms" when there is something (§6).
- **Settings** has only what the realm owns:
  - its lists, with root's inherited entries shown read-only above its own;
  - its toggles;
  - Appearance (this browser).

  System pages move to the console.
- **The live feed** (`WS /api/v1/live`) sends only the realm's sessions, and only the workers it may use. So the dock, inbox, mission control and the session list are right by construction.
- **This browser's state is kept per realm:** the workspace (tiles and history), finished and pinned sessions, the label filter, analytics and grouping choices. Keys become `c6.<realm>.…`. The theme stays one per browser.
- **The assistant** is the realm's, as built (os §3.6).

## 5. The console

The same shell, with its own nav:
- Realms: list, create, edit and delete, each realm's workers and connections, open it;
- Workers;
- Connections and routing;
- Defaults: root's skills, agents, commands, hooks, MCP servers, policies, prompts and secrets, which realms inherit;
- Spending: prices, the overall cap, usage and analytics across realms;
- Apps and packages;
- Grants and commons review;
- Sign-in and tokens; Notifications (the public URL);
- Updates; Upgrades and backups;
- Ask: the administration assistant (it runs in root).

## 6. Other realms: a count only

`GET /api/v1/elsewhere` (scope: device) returns, for each other realm, how many things wait on you there: approvals, open questions and decisions. It returns no titles or content. The header shows the total, and its menu lists the realms with a count each, each linking to that realm's inbox. Browser notifications are per realm already (their links carry the realm).

## 7. Tests (they fail iff the boundary breaks)

1. **Every operation has a scope:** the catalog test fails on an op without one.
2. **Realm isolation over every list:** a head with two realms gets objects of each kind in both. For every realm-scoped list op, a realm-A browser caller gets none of B's objects. Asked for one of B's by id, it gets "not found".
3. **Admin stays admin:** every admin op, called as a realm browser caller, returns 403. A browser request with no `X-C6-Realm` is refused.
4. **The live feed and usage are narrowed:** a realm-A live connection never carries a B session; analytics for A count only A's turns.
5. **Old links redirect** to the object's realm.

## 8. Limits

- **A shared worker shares its disk.** Two realms that may use the same worker can both browse its files and open terminals on it. To keep a realm's files apart, dedicate a worker to it (os §3.2).
- **Imports' realm rules are administration.** They decide which realm a Claude Code or Codex session lands in.

## 9. As built

- **Realm toggles are each realm's, root's by default:** suggestions, web search, the assistant's settings and subagent limits. Root's (set in the console) applies to a realm until its page changes it; the realm's own is kept as `<key>@<realm>`. The page says which, with **Use root's** to go back (`GET`/`DELETE /api/v1/settings/own/{name}`). Prompts work the same way, per prompt. Sessions read the value for their realm. The memex's settings stay the console's: the memex is one service.
- **Inherited list entries:** a realm's list pages (skills, agents, commands, hooks, MCP servers, policies, budgets…) show root's entries in use there, read-only ("Also in use, from root"); its own entry of the same name replaces one.
- **The console's lists are root's:** in the console, Settings' list pages (skills, policies, budgets, secrets…) edit root's entries, the defaults every realm inherits. Pages that are each realm's alone (memory, memex folders and scopes, mail, views, triggers, workflows) aren't in the console.
- **Saved browser state** lives under `c6.<realm>.…` and `c6.admin.…`. Home starts from what was saved before (`c6.…`). The theme and the last place (`c6.place`) are per browser.
- **Old addresses:** `/` goes to the last place. `/sessions/<id>` asks the head for its realm (root's go to the console). Console settings pages go to the console, anything else to the last place.

## 10. Steps

1. **Head:** scopes in the catalog; `admit` reads them; browser callers from `X-C6-Realm`, with no header refused. Tests 1–3.
2. **Head:** narrow what realm callers get from the live feed, usage and analytics, search, the explorer, workers and connections; old-link redirects; `/api/v1/elsewhere`. Tests 4–5.
3. **UI:** routes under `/r/<realm>/` and `/admin/`; landing on the last realm; the API layer sends the header; browser state keyed per realm; "All realms" removed.
4. **UI:** the realm header (switcher, colour, the count), and realm Settings (own and inherited entries).
5. **UI:** the console.
6. **Docs:** os.md §3, README.

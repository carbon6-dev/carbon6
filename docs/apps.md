# Apps: code, data and tools — Design

Status: proposed (2026-10-16). Nothing here is built. It changes what docs/os.md §5 and docs/packages.md describe, and §8 lists what that replaces.

## 1. The problem

Today an app *is* a realm (os.md §5). Installing one creates a realm that holds its code (skills, templates, schemas, views), its processes (sessions) and its data (records, files it publishes). Two things follow that are wrong for many apps:

- **Your work lands in the app's realm.** A deck you make with the decks app runs as a session in `decks` and is recorded there. It doesn't show in home's session list, it can't use home's knowledge without a grant, and removing the app deletes your decks.
- **A deployed app drifts.** Settings edits an app realm's skills and templates like any other realm's, so what runs is no longer the version that was installed. Upgrades then need a three-way merge, with conflicts for you to settle, and nothing can say which version is really running.

Both come from treating an app's code and the data made with it as one thing. An operating system keeps them apart: a program is installed once and read-only, and the documents you make with it are yours, in your home folder.

## 2. Words

- **Code:** what a package ships. Skills, agents, commands, templates, workflows, hooks, triggers, views, policies, budgets, routes, prices, memory notes, collection schemas, and the records it ships (such as decks' themes).
- **State:** what is made after install: sessions, records, decisions, series points, memex knowledge, files, setting values, and memory notes written later.
- **Tool:** an app that does nothing until a realm uses it, and whose results belong to that realm. Decks is a tool.
- **Daemon:** an app that acts on its own (triggers, schedules), may hold secrets or a dedicated worker, and owns data only it should touch. The service factory is a daemon.
- **Enable:** to make a tool available in a realm. It's the tool's equivalent of a library being added to a realm, except that nothing is copied.

## 3. Two kinds of app

A package says which kind it is: `"kind": "tool"` or `"kind": "daemon"`. Packages written before this change are daemons, which is how they behave today.

| | Tool | Daemon |
|---|---|---|
| Its realm holds | its code only | its code and its state |
| Its sessions run in | the realm that started them | its own realm |
| Records its templates write | in the realm that started them | in its own realm |
| Used from another realm through | enabling it there (4.2) | grants (os.md §3.7), as today |
| Removing it | removes its code; your decks stay yours | deletes its realm and its state, as today |

A tool still gets a realm, named as now (`decks`). That's where its code lives, read-only (5), and where its shared records are (4.3). No session runs there, and nothing is written there after install except by an upgrade.

## 4. Tools

### 4.1 Installing

Installing a tool works the way installing an app does now: one decision listing what it is, what it requires and what it asks for. It creates the tool's realm with its code. On its own, installing makes nothing available anywhere; enabling does.

### 4.2 Enabling in a realm

Enabling a tool in a realm (home, say) is a decision of its own, per realm, listing what the tool gets there:

- **Kits** it requires, with their sandbox permissions, for that realm's sessions.
- **Secrets** it needs set in that realm, and **grants** it needs into that realm.
- **Its collections** created in that realm from its schemas (4.3).
- **Its triggers,** if it has any, set up in that realm and running there.

A tool enabled in two realms is approved twice, so enabling it somewhere new never quietly widens what it can reach. Disabling it removes its triggers and kits from that realm. The realm's records stay, and the realm can still read them.

### 4.3 Collections

A tool's manifest says, per collection, where its records live:

- **`"scope": "realm"`** (decks' `decks`): the schema is the tool's code, and each enabling realm gets its own collection with that schema. Its records are that realm's state.
- **`"scope": "shared"`** (decks' `themes`): the records the tool ships, in its own realm, readable by every realm that enables it. An enabling realm can add records of its own to the same collection, and they win over shipped ones with the same key. That's how a brand kit becomes a fourth theme in home without changing the tool.

### 4.4 Running

A template from a tool, started in realm R (from R's UI, a view's action in R, a trigger in R, or `template_start` from a session in R), runs in R:

- **Its session is R's.** It shows in R's lists, uses R's memory notes, secrets, memex and grants, and R's policies and budgets apply.
- **Its configuration is layered:** root's, then the tool's, then R's own. What R has wins over the tool, and the tool's wins over root, the way realms override what they inherit (os.md §3.3).
- **Its records** go to R's collections. Shared collections read from the tool's records plus R's (4.3).
- **Its views** show in R, over R's records.

The skills and agents of every tool enabled in R are in R's sessions too, not just in the tool's templates, the same as a library's would be.

## 5. App code is immutable

This holds for both kinds.

- **Nothing changes an app's code** except installing, upgrading, downgrading or removing it, each a decision showing the plan. The head refuses writes to an app realm's settings lists, to its collection schemas and to its shipped records, whoever asks: you in Settings, the assistant, an agent's `records_put`, or an API token.
- **Settings shows an app realm read-only,** with how to change it: a setting (5.1), an override (5.2) or a fork (5.3).
- **The code is checked against the install.** An install keeps its digest. The head computes the digest of what's in the realm when it starts and before an upgrade, and reports any difference as a problem on the install.

### 5.1 Settings an app declares

What an app wants you to configure, it declares, as a JSON Schema in the manifest:

```json
"settings": {
  "type": "object",
  "properties": {
    "default_theme": { "type": "string", "description": "The theme to suggest first" },
    "slide_size": { "enum": ["16:9", "4:3"], "default": "16:9" }
  }
}
```

- **Where values live:** with the install for a daemon, and per enabling realm for a tool. They are state, so upgrades keep them.
- **The form** is generated from the schema, as Settings forms are (os.md §10).
- **Prompts read them** as `{{settings.<name>}}`, and agents get them in their brief.
- **Upgrades:** a new setting takes its default; a setting the new version drops loses its value.

### 5.2 Overrides

A realm that uses a tool overrides any of the tool's skills, agents, commands or templates by having its own with the same name and labels. The tool is unchanged, and the override is visibly the realm's. A daemon's realm can't be overridden from inside: it holds only the daemon's code, and that's immutable.

### 5.3 Forks

To change an app beyond its settings, export it as a new package (`decks-acme`, with `"forked_from": "decks"`), change that, and install it. A fork is a different app with its own versions. An install can be switched to a fork of its package, the same way it's upgraded; for a daemon that keeps its realm and state.

## 6. Upgrades and downgrades

With nothing to drift, an upgrade replaces version A's code with version B's, and a downgrade is the same operation backwards.

- **No merge.** The three-way comparison and its conflict decisions go (8).
- **Schemas** follow os.md §4.1: an additive change applies at once; anything else needs a migration, a decision, applied to every realm with records of that collection (each enabling realm, for a tool). A downgrade that would need one asks the same way, or is refused when the package gives none.
- **Approval:** the decision lists what changes, and asks again only for permissions that grew (packages.md §4).
- **Rollback:** the previous version stays in the registry, so going back is a downgrade.

## 7. Requiring apps

- **A tool requiring a tool** enables the required one wherever the requirer is enabled, in the same decision.
- **Anything requiring a daemon** gets what it gets today: the daemon in its own realm, and a grant to start its exported templates, which run there.
- **Libraries** become tools with no templates or collections: configuration enabled by reference instead of copied. Their code is immutable like any app's.

## 8. What this replaces

- **The three-way upgrade** (os.md §5.2a) and its conflict decisions.
- **Editing app realms** in Settings and through the API.
- **Libraries copied into the requiring realm** (packages.md §3).
- **`template_start` into a tool's realm:** a tool's template starts in the caller's realm, and needs no grant.
- **Unchanged:** daemons' realms and state, grants, kits and their sandbox, the registry and digests, the install decision.

## 9. Upgrading what's installed

- **Apps installed today** are daemons until their package says `"kind": "tool"`. Upgrading to such a version turns the install into a tool.
- **Drift:** when an install becomes immutable, the head compares the realm with its install's digest. If you had edited it, a decision offers two choices: export your edits as a fork and install that, or drop them and take the package as it is.
- **State left in a tool's realm:** sessions can't move (os.md §3.5), so a daemon turned tool keeps its old sessions in its realm, readable as before. Records in its `"scope": "realm"` collections are offered for copying into a realm you choose, with their history and where they came from. Nothing is deleted without asking.
- **decks** 1.2.0 becomes a tool: `decks` per realm, `themes` shared.

## 10. Build order

1. **Immutability:** refusing writes to app code, read-only Settings, the digest check, settings an app declares, and upgrades and downgrades without a merge.
2. **Tools:** `kind`, enabling per realm with its decision, collection scopes, sessions and records in the caller's realm, layered configuration.
3. **Libraries as tools,** forks, and switching an install to a fork.
4. **decks 1.2.0** as a tool, and the upgrade of existing installs (9).

## 11. Decisions

| Question | Decision | Why |
|---|---|---|
| Where a tool's results live | In the realm that used it | They're that realm's work: its sessions, knowledge and policies apply, and removing the tool leaves them |
| Where an app's code lives | In its own realm, read-only | One copy, upgraded once, checkable against its digest |
| Changing an app | Only by install, upgrade, downgrade or remove; customization through declared settings, overrides in the using realm, or a fork | A deployed app is exactly a version; nothing drifts, so nothing needs merging |
| Kinds of app | Tool and daemon, declared in the package | A daemon's own realm is right for code that acts alone and owns its data; a tool's isn't |
| Permissions for a tool | Approved per enabling realm | Enabling a tool somewhere new never quietly widens what it reaches |
| Shared reference data (themes) | Shipped records in the tool's realm, plus the realm's own, which win | Defaults that come with upgrades, and your own additions without a fork |

## 12. Open questions

- **The CRM:** a tool (your relationships in home, beside your mail) or a daemon (its own realm, reaching mail through grants, as in os.md §6.2)?
- **A tool's memory notes:** whether the tool's shipped notes reach the realm's sessions, or only its templates' sessions.
- **Tools in commons:** whether a tool's shared records could live in commons, so realms that don't enable it can still read them.

# Apps: instances, owned data and open reading — Design

Status: proposed (2026-10-16). Nothing here is built. It changes what docs/os.md §5 and docs/packages.md describe; §13 lists what it replaces. Where knowledge is kept, and what becomes of the memex, is in docs/knowledge.md.

## 1. The problem

Today an app *is* a realm (os.md §5): installing one creates a realm holding its code, its sessions and its data, and other realms reach in only through grants. A realm decides three things at once:

1. **Who can see data,** which is a question of trust: one business shouldn't see another's.
2. **Who can change data,** which is a question of consistency: only the CRM knows what makes a relationship valid.
3. **Where an app's code lives.**

Tying the three together goes wrong for the apps we want:

- **A service (decks).** A deck you make lands in the `decks` realm, out of your session list and away from your knowledge. Once made, it should be yours and need nothing more from decks.
- **A processor (CRM).** It reads input (mail, business context), keeps working data of its own (summaries, scores), and publishes results (relationships) that add context to the input. As a realm it can only see its input through grants, and its results are hidden from the realm the input came from.
- **A mirror (Gmail).** It keeps a copy of data whose source of truth is elsewhere, and must be the only writer of that copy. Today it's built into the memex rather than written as an app (docs/knowledge.md §6).
- **Many tenants.** Running unrelated businesses, you want a CRM for each, each fed from one business's data, with separate results. Today each is a realm wired up with its own grants.
- **Silos.** Every app realm hides its data by default. But nobody knows in advance which app's data will be useful to which agent; that's the point of the knowledge graph.

And app realms can be edited in Settings, so a deployed app drifts from the version installed, and upgrades need a three-way merge.

## 2. Principles

- **Realms are trust domains, drawn by you.** Home, each business, anything that must stay apart. Installing an app never creates one.
- **Reading is open within a realm.** Every agent in a realm can read every collection in it, and every piece of data describes itself well enough for an agent written by anyone to use it.
- **Writing is owned.** Data belongs to whoever keeps its consistency: an app instance, or the realm itself. Only the owner changes it, directly or through the operations it publishes.
- **The graph doesn't judge.** Links between things are claims, each with an owner; anyone may add their own, and readers decide whose to believe (9).
- **Code is immutable.** A package version never changes. An instance changes only by upgrading, downgrading or switching to a fork, and is configured through settings its package declares.

Unix works this way: `/etc/passwd` is readable by every program, and only `passwd` and `chsh` change it, because only they know its rules.

## 3. Words

- **Realm:** a trust domain (os.md §3), unchanged except that apps no longer get one each.
- **Package:** immutable code: a manifest and its files, at a version (docs/packages.md).
- **Instance:** a package version installed in a realm, with a name (the package's, by default), its own identity, its settings and the data it owns. One package can have many instances: `crm` in `biz-a` and `crm` in `biz-b`, or two in one realm with different names.
- **Owner:** what may write a collection: an instance, or the realm (for collections you make by hand).
- **Operation:** a write path an instance publishes, with typed input, that keeps its data consistent (7).
- **Data card:** what a collection says about itself, so any agent can interpret it (8).
- **Published / internal:** a collection's stability. Published is a contract; internal is readable but may change without notice.
- **Handoff:** output an instance makes for whoever asked, owned by the requester from then on (10).
- **Thing:** a node in the realm's graph: a person, an organization, a place, an event, a topic (9.1).
- **Claim:** an edge or a fact, with its owner and provenance (9.2).
- **Reader policy:** which owners' claims a reader believes, per label (9.3).

## 4. Realms

- **You create realms** for real boundaries. home is where you start; a realm per business is the multi-tenant case (11).
- **Grants stay** the way data crosses realms (os.md §3.7): read, append, snapshot and call.
- **Policies and budgets** of a realm apply to everything in it, instances' sessions included. An instance can't override them.
- **An untrusted package** is installed in a realm of its own, which you create for it, with only the grants you choose. No other mechanism is needed: a realm is the trust boundary, wherever the package is installed.

## 5. Instances

### 5.1 Installing

Installing a package version in a realm is one decision, listing:

- **What it reads** in that realm (5.2), and grants it needs from other realms.
- **What it owns:** the collections it creates (6).
- **What it publishes:** its operations, and the skills and templates it offers the realm's sessions.
- **What it runs on its own:** its triggers.
- **Kits,** secrets and workers, as packages ask for them today (packages.md §4).

Approval is per instance, so installing the same package in another realm asks again.

### 5.2 What an instance reads

A package declares what it reads, and you approve it at install, the way Android asks for an app's permissions:

```json
"reads": {
  "items": { "kinds": ["email", "person"] },
  "collections": ["contacts.*"],
  "realms": []
}
```

- **Within its realm:** item kinds and things in the graph, and collections by name, or `<instance>.*` for everything an instance owns. Files on workers aren't covered: realms don't isolate them (os.md §3.9).
- **Other realms:** only through grants, which the install asks for as today.
- **What this limits** is what the *instance's* sessions can see. It never hides the instance's own data from anyone else in the realm: that stays open (2).

### 5.3 Sessions

An instance's sessions (from its templates, triggers and operations) run in its realm.

- **Identity:** they act as the instance, `app:<instance>`, which is what history records (os.md §4.1) and what ownership checks (6).
- **Labels:** `app:<instance>`, so they can be found and filtered. They show in the realm's lists.
- **Configuration:** root's, then the realm's, then the instance's code. In its own sessions the instance's skills, agents and templates win over the realm's, because they are its logic. The realm's policies and budgets always apply.
- **What it sees:** what 5.2 allows, plus everything it owns.

### 5.4 Settings

What a package lets you configure, it declares as a JSON Schema:

```json
"settings": {
  "type": "object",
  "properties": {
    "check_in_weeks": { "type": "integer", "default": 6, "description": "Default cadence for a new relationship" },
    "voice": { "type": "string", "description": "How check-ins should sound" }
  }
}
```

- **Values belong to the instance.** They are state, so upgrades keep them; a new setting takes its default, and a dropped one loses its value.
- **The form** is generated from the schema (os.md §10).
- **Prompts read them** as `{{settings.<name>}}`, and the instance's sessions get them in their brief.

### 5.5 Code is immutable

- **The head refuses writes** to an instance's code: its skills, agents, commands, templates, workflows, hooks, triggers, views, operations and collection schemas, whoever asks (Settings, the assistant, agents, tokens).
- **Settings shows an instance's code read-only,** with the ways to change it: its settings (5.4), an upgrade (12), or a fork (5.6).
- **The digest is checked.** An instance keeps its package's digest; the head compares the code it holds with it at start and before an upgrade, and reports any difference.
- **What an instance offers the realm** (skills, templates) can be overridden in the realm, for the realm's own sessions: a skill with the same name and labels in the realm wins there, as realms override what they inherit (os.md §3.3). The instance's own sessions keep using its own.

### 5.6 Forks

To change an app beyond its settings, export it as a new package (`crm-acme`, with `"forked_from": "crm"`), change it, and install that. A fork is a different app with its own versions. An instance can be switched to a fork of its package the way it's upgraded (12), keeping its name, settings and data.

## 6. Ownership

Every collection has an owner: an instance, or the realm.

| | Reads | Writes |
|---|---|---|
| **The owner** (its sessions) | ✓ | ✓, directly (`records_put`, `records_delete`) |
| **Others in the realm:** you, the assistant, the realm's sessions, other instances (within their reads), the realm's tokens | ✓ | only through the owner's operations (7) |
| **Other realms** | through a grant | through a `call` grant on an operation |

- **Realm-owned collections** (ones you make by hand, and handoffs, 10) are written by anything in the realm, as collections are today.
- **Claims** (9.2) follow the same rule: an owner writes and retracts only its own.
- **Record triggers** fire on every write, whoever made it.
- **Forcing a write.** You can write to an instance's collection directly, as yourself. It's a decision that names the collection's invariants from its data card (8), and it's recorded in the history as forced. The assistant and agents can't force.

## 7. Operations

An operation is how anyone but the owner changes the owner's data. The package declares it:

```json
"operations": [
  {
    "name": "set_cadence",
    "description": "Set how often to be in touch with someone",
    "input": { "type": "object", "required": ["relationship", "weeks"], "properties": { "relationship": { "type": "string" }, "weeks": { "type": "integer", "minimum": 1, "maximum": 52 } } },
    "write": { "collection": "relationships", "id": "$.relationship", "fields": { "cadence_weeks": "$.weeks" } }
  },
  {
    "name": "merge_people",
    "description": "Two relationships are the same person: merge them and their touchpoints",
    "input": { "type": "object", "required": ["keep", "merge"], "properties": { "keep": { "type": "string" }, "merge": { "type": "string" } } },
    "template": "merge-people"
  }
]
```

- **Three kinds:**
  - **write:** a record update mapped from the input, the way trigger actions map events (os.md §4.3). It runs at once, in the head, and is checked against the schema. It can only touch the fields it names, and can require a condition on the record first (a JSON filter, os.md §4.1).
  - **template:** a session of the instance, with the input as `{{input}}`, for changes that need judgment. The caller gets the session's id; the result arrives when it ends.
  - **kit:** a kit tool of the instance, run at once with mapped arguments, for changes made outside carbon6 (sending mail, an RSVP). Its output is ingested as the instance (docs/knowledge.md §6.2).
- **Callers:** agents with `app_call {instance, operation, input}`; the UI, whose forms for an instance's collections call its operations; views' actions; triggers (an `operation` action); other realms through a `call` grant on `op:<instance>.<name>`.
- **A published operation is a contract,** like a published collection (8): changing its input incompatibly, or removing it, is a breaking change (12).

## 8. Data that explains itself

Every collection has a **data card**, and every record carries its **provenance**, so an agent that has never seen the app can use its data, and knows what it may rely on.

**The data card** (in the package for an instance's collections; written by you, with the assistant's help, for the realm's):

- **What it is:** a description, and per field what it means, its units and its examples (in the schema's descriptions).
- **Owner and stability:** the instance and package version, and `published` or `internal`.
- **Where it comes from:** its inputs (item kinds, collections, operations) and how records are derived from them, in words.
- **Invariants,** in words: what must stay true ("a relationship's last contact is the date of its newest touchpoint"). Readers learn what the data guarantees; writers see why they must go through operations.
- **Freshness:** how and how often it's updated.
- **Operations** that change it.

**Provenance on each record:** the instance and version that wrote it, the session or operation, and **derived-from links** to the items and records it came from (an email, a meeting note). os.md §4.1's history already keeps who changed what; derived-from is new.

**Where agents see it:** `records_query` and `records_get` return the card the first time a session reads a collection; search results that include records carry the card's summary; the API and the realm explorer show it.

**Internal collections** are readable like any other. Tool results and the API mark them: *internal to `crm`: may change without notice; build on its published collections instead.* Nothing is hidden, and nothing depends on them by accident.

## 9. The knowledge graph: claims

The graph is where data from every owner meets. It stores claims and doesn't decide which are true; the meaning is in the eye of the reader. This is how RDF's named graphs and Wikidata's claims with references work.

### 9.1 Things

- **Things are the realm's shared nodes:** people, organizations, places, events, topics. They belong to the realm, not to an app.
- **Anyone may create one.** Gmail and the CRM may each create "Alice". That two things are the same is a claim too (9.2).

### 9.2 Claims

- **An edge** is `(from, label, to, owner)`; **a fact** is `(thing, attribute, value, owner)`. Each carries provenance: the owner's package version, the session or operation that made it, when, an optional confidence, and derived-from links to its evidence (8).
- **Anyone who can read both ends may add their own claims.** Gmail asserts that a message was `sent_by` a person; a contact-matching app may assert the same edge, or a different one. Both stand.
- **Only the owner retracts or changes its claims.** Disagreeing means adding your own claim, never deleting another's.
- **You are an owner too.** A correction is a claim you make. Default policies rank you first (9.3), so you never edit an app's data; you outrank it.
- **Identity is claims:** `same_as` and `not_same_as` between things, owned like any edge. Merging people stops being a judgment the store makes; whoever resolves identities writes claims, and readers choose whose to follow.
- **A record's links** to things and items (os.md §4.1) are edges owned by the record's owner.
- **Edges into another realm** exist only under a grant; a reader without one doesn't see them.
- **Removing an instance** retracts its claims, after offering an export. Each owner has a write quota, so a faulty app can't flood the graph.

### 9.3 Reader policies

Most readers (search, briefs, an agent asking "who sent this?") don't want to weigh every claim each time. So a realm states default policies, per label:

```json
{ "label": "sent_by", "believe": ["you", "gmail", "*"] }
{ "label": "same_as", "believe": ["you", "google-contacts", "people-linker"] }
```

- **A policy is configuration,** in the realm's settings, and every reader can override it, per query or per agent.
- **With no policy,** a reader gets every claim, with its owner, and decides.
- **Ignoring an owner** entirely is a policy too.
- **Search, briefs and the graph views** apply the realm's defaults unless asked otherwise.

### 9.4 Shared vocabulary

Claims are only comparable if readers can tell which labels mean the same thing.

- **A small vocabulary in root,** readable by every realm: labels (`sent_by`, `sent_to`, `about`, `mentions`, `part_of`, `attended`, `same_as`, …, the core set of memex.md §4.3) and item kinds (`email`, `event`, `contact`, `document`, `photo`, …), each with its meaning and schema.
- **An owner describes its labels** in its data card, and maps each to a shared one where it fits. Readers find equivalent claims by different owners through the mapping; labels with no mapping are still readable, with the owner's description.
- **The vocabulary grows by publishing,** like commons (os.md §3.7): a realm proposes a label, and it's added when you accept it.

### 9.5 Enrichment

A processor makes its input richer without changing it:

- **Results link to what they're about.** A CRM relationship is linked to the person thing, so the person now shows the relationship, cadence and last contact, in the graph, in search and in briefs.
- **Results link to what they came from.** A touchpoint's derived-from links point at the email, so any agent can check the evidence.
- **The input is never written.** The CRM can't change mail or contacts; it adds claims and records it owns, beside them.

## 10. Handoff

A service makes something for whoever asked and keeps no hold on it.

- **The output belongs to the requester's realm.** Files are published as the realm's Outputs; records go in realm-owned collections. They carry provenance (made by `decks` 1.2, from this brief, in this session) but no write lock.
- **The service keeps only what it needs.** decks owns an internal `jobs` collection (brief, outline, stage) while it works; once the deck is delivered, the job is history.
- **Later changes don't need the service.** Anyone can edit the delivered deck. decks' `revise` operation takes any deck file and notes, whether decks made it or not.

## 11. Many tenants

- **One instance per business realm.** `crm` in `biz-a` reads biz-a's mail and contacts and owns biz-a's relationships; `crm` in `biz-b` the same for biz-b. The realm boundary keeps them apart; neither instance is configured to be separate.
- **One package, one build.** Instances share their package version's code and kits; each has its own settings and data.
- **Different versions:** each instance upgrades on its own, so one business can try a new version first.
- **Across tenants on purpose:** an instance in one realm (a portfolio view in home, say) reads the businesses' published collections through grants you make.

## 12. Upgrades and downgrades

- **No merge.** Code can't drift (5.5), so an upgrade replaces version A's code with B's, and a downgrade does the reverse. The previous version stays in the registry for rollback.
- **Owned schemas** follow os.md §4.1: an additive change applies at once; anything else needs a migration, approved as a decision. A downgrade that would need one asks the same way, or is refused when the package gives none.
- **Contracts.** Changing a published collection's schema incompatibly, or a published operation's input, or removing either, is a breaking change: the package's major version must change, and the upgrade decision lists what depends on it in the realm (views, triggers, other instances' reads, grants). Internal collections carry no contract.
- **Approval** asks again only for what grew: reads, grants, kits' permissions, triggers.

## 13. What this replaces

- **An app is a realm** (os.md §5): apps become instances in realms you choose.
- **The three-way upgrade** (os.md §5.2a) and its conflict decisions.
- **Editing an app's configuration** in Settings and through the API.
- **Libraries copied into the requiring realm** (packages.md §3): a library is a package with no collections, installed as an instance whose skills and templates it offers the realm.
- **Template grants for apps requiring apps** (packages.md §6): a package that requires another uses an instance of it in the same realm (installed with it, or one already there) through its operations, or one in another realm through a `call` grant.
- **The memex as judge of the graph:** its automatic merges and links become claims by system packages, and its review queue becomes decisions that assert your claims (docs/knowledge.md §7).
- **The tool and daemon kinds** of this doc's first draft: ownership decides who writes, not a kind of app.
- **Unchanged:** realms, grants, policies and budgets; kits and their sandbox; the registry, digests and install decisions; collections, decisions, triggers, series and views.

## 14. Upgrading what's installed

- **Each app realm** (decks, today) gets a decision: move the app into a realm you choose (home, say) as an instance there, or keep it where it is as an instance in its own realm.
- **Moving:** sessions can't move (os.md §3.5), so the old realm keeps them, readable. Its records are copied to the new instance's collections with their history and where they came from. The old realm is then yours to delete, which asks as it does today.
- **Drift:** the head compares each app's configuration with its install's digest. If you had edited it, a decision offers to export your edits as a fork and install that, or to drop them.
- **Collections you made by hand** become realm-owned, with data cards to write (the assistant drafts them, for you to approve).
- **decks 1.2.0:** a `make` operation (template), internal `jobs`, deck files and a realm-owned `decks` record as handoff, a shared `themes` collection published by the instance, and `revise` taking any deck file.

## 15. Build order

1. **Ownership:** owners on collections, writes refused to non-owners, forced writes as decisions, and the instance identity in history.
2. **Instances:** installing a package in a realm you choose, by name, more than once; reads approved at install; sessions as the instance; settings.
3. **Immutable code:** read-only instance code, the digest check, upgrades and downgrades without a merge, forks.
4. **Operations:** write and template operations, `app_call`, the `operation` trigger action, UI forms and views' actions through operations, and `call` grants on them.
5. **Data cards and provenance:** cards in packages and for realm collections, derived-from links, cards in tool results, search and the explorer, internal marked.
6. **Claims:** things anyone may create, owned edges and facts, reader policies with realm defaults, the shared vocabulary in root, and the memex's links moved over as claims. This comes with docs/knowledge.md's step 1, which puts the graph in the head.
7. **Moving existing apps** (14), libraries as instances, and decks 1.2.0.

## 16. Decisions

| Question | Decision | Why |
|---|---|---|
| What a realm is | A trust domain you draw; apps don't get one each | Trust boundaries are about people and businesses, not about which app made the data |
| Reading within a realm | Open to every agent, with data that describes itself | Nobody knows in advance which data will be useful to which agent; silos waste the knowledge graph |
| Writing | Only the owner, directly or through its published operations; you can force, as a decision | Only the owner knows its invariants |
| What an instance may read | Declared by the package, approved per instance at install | Limits what untrusted code sees without hiding anyone's data |
| Untrusted packages | Install them in a realm of their own | The realm is already the trust boundary; no other mechanism needed |
| Internal data | Readable, marked as internal and unstable | Open without inviting dependence on what may change |
| Code | Immutable per version; settings, forks and upgrades are the only changes | A deployed app is exactly a version, so nothing drifts and nothing merges |
| Many tenants | One instance per realm | The separation comes from the realm, not from configuring the app |
| Service output | Handed off to the requester's realm, with provenance | It's the requester's work, and needs nothing more from the service |
| Links and facts | Claims with owners; anyone adds their own, only the owner retracts | The graph stores and doesn't judge; each owner keeps its own consistency |
| Which claims to believe | The reader's choice, with realm defaults per label, you first | Meaning is in the eye of the reader, but most readers want a sensible default |
| Things (people, places) | The realm's, created by anyone; identity by `same_as` claims | No app is canonical; data from every owner meets on the same nodes |
| Mail, calendar, contacts | In the realm whose accounts they are, owned by that realm's mirror app instance; stored by the head (docs/knowledge.md) | The memex is the database, not an owner; the mirror is the one writer that keeps the copy true |

## 17. Open questions

- **Write operations beyond mapping:** some invariants need a computed value (a count, a maximum). Is a small expression language enough, or should those be template operations?
- **Policies that depend on content:** "believe the CRM's `works_at` only above confidence 0.8" needs conditions in a policy, not just an order of owners. Is an order enough to start?

Settled since the first version: mail, calendar and contacts are in the realm whose accounts they are, owned by that realm's mirror apps, and stored by the head (docs/knowledge.md); and two apps tracking the same idea is no conflict, since things are shared and claims are owned (9).

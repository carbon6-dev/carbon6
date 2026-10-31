# Docs and decks: living documents people and agents write together — Design

Status: proposed (2026-10-30). Decided with you:
- docs and decks are **first class** in carbon6: head objects of each realm, like sessions and work. Decks stop being an app you install (docs/decks.md is replaced by §5 here);
- **people and agents both write them**, and keep changing them over time;
- the model is Claude's docs: tabs of blocks with stable ids, edits as small guarded operations, pending sections an agent fills while you watch, comments that can be sent to an agent;
- projects (docs/projects.md) and work (docs/work.md) **refer to docs and decks**; they outlive the session that wrote them;
- both **export to Google** (Docs, Slides) and to standard files (Markdown, HTML, PDF, Word, OpenDocument, PowerPoint).

## 1. The problem

Agents in carbon6 write a lot: plans, reviews, research, specs, decks. Today it ends up in one of four places, and each is wrong for something that lasts:
- **the transcript:** gone from view once the session scrolls on, and editable by nobody;
- **a file in a repo:** fine for code docs, but tied to one worker and folder, invisible to the Brief, Work and search, and awkward to comment on;
- **an Output page:** a one-off render, not something you or the next agent keep editing;
- **the decks app:** HTML files in a worker's folder, a record that tracks their stage, and an app and a kit to install in each realm before any of it works.

A plan for a project is written once, then changed by you, by the agent that does phase 2 and by a reviewer. A deck for its review is revised after every rehearsal. Both need one place that keeps every version, says who changed what, takes comments, and hands the result to people who use Google or Office.

## 2. What a document is

A **document** is a doc or a deck. Everything in §3 and §6 to §13 holds for both; §4 is about docs, §5 about decks.

| Part | |
|---|---|
| kind | `doc` or `deck` |
| title | |
| tabs | a doc's tabs, in order; most docs have one. A deck has one tab, its slides |
| blocks | the content (§3). Each has a stable id |
| comments | threads anchored to quoted words, a block, or a slide (§8) |
| history | every change, with who made it (§7) |
| exports | where it was last exported to, per format and Google file (§10) |
| realm, labels | as for sessions |
| created by, created, changed | `you`, `session:<id>`, `assistant`, or an API token |

**Markdown is the exchange format.** You and agents send and read Markdown, with chips as `<?c6 block k?>` tokens and their values in a `blocks` map beside it. A deck's Markdown separates slides with `---` and gives each slide's layout and notes in a short header (§5). Agents are fluent in Markdown, and it exports as is. The store keeps blocks, so ids survive edits and comments stay anchored.

**Stable ids.** A block's id is `<writer>.<clock>`, issued when it is written and never reused. A paragraph keeps its id when its words change. Edits, comments and "changed since" reads all address blocks by id.

## 3. Blocks, chips and embeds

Text blocks: paragraphs, headings, lists, task lists, tables, code, quotes. Chips sit inside text; embeds take a line of their own.

| Kind | What it shows |
|---|---|
| **date** | a date someone may act on (due, review, as of). Projects read them (docs/projects.md §4) |
| **mention** | a person (from contacts, docs/relationships.md) or you |
| **dropdown** | one option of an enum the document defines (Status: Not started / In progress / Done), so a tracker table's column stays a closed set |
| **ref** | any carbon6 thing, live: `session:`, `work:`, `project:`, `decision:`, `request:`, a mail thread, a chat, a record, another document, tab or slide. It shows the thing's title and state as it is now |
| **pending** | a section or slide still to be written, with its intent ("Risks: what could slip and the fallback"). §6 |
| **chart** (embed) | drawn from rows kept in the document, so you can hover a value or open its data. No pictures of charts |
| **diagram** (embed) | SVG an agent writes, sanitized. You edit its labels in place |
| **image / file** (embed) | a blob (docs/knowledge.md §5), uploaded or made by a session |
| **view** (embed, docs only) | a view of the realm (docs/os.md §4.5): a collection's table, a board, a feed. It stays live |

Views of kind **page** stay as they are: a dashboard you configure. A doc is something you write.

## 4. Docs

A doc is prose with exhibits: a plan, a spec, a review, research, meeting notes, a tracker table. One concise tab is the norm; a further tab is for material with its own bulk (sources, raw data, working notes).

## 5. Decks

A deck is a sequence of **slides**. What the decks app did (an outline you approve, a look you choose, slides built and checked by eye) carries over, on the same store, operations, comments and history as docs.

- **A slide** is a block with a **layout**, the blocks in each of its regions, **speaker notes**, and a hidden flag. Its regions hold ordinary blocks with their own ids, so you edit, comment on and restore a bullet on slide 7 as you would a paragraph.
- **Layouts:** title, section, title and body, two columns, big number, quote, image, chart, diagram, and **html**. An html slide holds sanitized HTML and CSS scoped to the slide (no scripts), for a visual no layout gives. The others are structured, so they stay editable everywhere they go, PowerPoint and Google Slides included.
- **Themes** are records you edit in Settings → Decks: CSS custom properties for colours, type and accents over carbon6's base slide CSS. **Paper**, **Night** and **Signal** ship as editable defaults; a brand kit makes a fourth (later). A deck names its theme, and changing it restyles every slide.
- **Size:** 16:9 at 1280×720; 4:3 as an option.
- **Rendering:** carbon6 draws slides itself from the blocks and the theme, in the UI and for exports. Nothing has to be built or installed to see a deck.
- **Checking by eye.** The `doc` tool's `look` returns pictures of slides (one, several or all), drawn by the render kit (§10). Agents look at every slide they make and fix what they see, as the decks app's review did.
- **The flow** (an outline as pending slides, each with its message; asking you to approve it and to choose a theme, as decisions; building; looking; fixing) is the editable prompt `decks.flow`. The decks app's skills become editable prompts too: `decks.storyline`, `decks.design`, `decks.visuals` and `decks.review`. A session with the `doc` tool gets them when it works on a deck.
- **Presenting:** Present opens the deck full screen, with a presenter view (notes, next slide, timer) in a second window.

## 6. Writing while you watch

Agents write documents the way Claude does:
1. **Birth:** the outline first. A title, then one `pending` block per section (a doc) or per slide (a deck), with its intent. The document opens in a panel of its own straight away, so you see the plan before any research.
2. **Fill:** one section or slide per call. Each call replaces a pending block. The document updates live over the live feed, so you watch it fill instead of waiting for a wall of text.
3. **A pending block shows who is writing it** ("session Fix sync is writing this"). You can edit around it; editing the pending block itself takes it from the agent (its fill then fails, and it says so).

How agents write docs (lead with the answer, sections sized to the ask, tables for comparisons, charts for series, diagrams for flows, sources cited) is the editable prompt `docs.style`. Nothing about style is in code.

## 7. Editing, conflicts and history

**Operations.** Every change, yours or an agent's, is a batch of operations applied atomically:
- `insert` Markdown before or after a block, at the end of a section, or at the end of a tab; for decks, a slide before or after a slide;
- `replace` a block, a run of words (`find` by quoting them, `nth` when they repeat), a table cell, or a slide's layout or notes;
- `delete` and `move` blocks and slides; table row and column operations;
- `set` a chip, the theme, rename the document or a tab, add or order tabs.

**Guards, not locks.** A read returns the document's `rev` and each block's hash. Replacing or deleting a whole block needs `ifRev` and `ifHash`. If someone changed it since, the op is refused (`guard_mismatch`) and nothing in the batch lands. **Their edit wins:** the agent rereads that block and says what it kept. A `find` that no longer matches is refused the same way. There is no `force` for agents; you can overwrite from the history.

**Reads stay small.** Agents read an `outline` (block and slide ids, headings, first words), one block, table or slide in full, a `search` inside the document, or `since` a rev (only the blocks changed since). They never need the whole document to change a word.

**History.**
- Each batch is a version: the ops, who sent them, and for a session, the turn. Every 50 versions the head keeps a snapshot, so any version opens quickly.
- **History** lists changes by author run ("session Plan phase 2, 14 edits, 3:10–3:22"), with a diff of each (for decks, before and after pictures of the slides changed). Any version can be opened, and **Restore** brings back a version, or one block or slide of it, as a new change.
- Blocks show who last wrote them on hover ("you", "session Fix sync"), from the ops.
- Text an agent deleted is brought back from history, never retyped from memory (`docs.style` says so).

## 8. Comments

- **A thread** is anchored to 3 to 6 quoted words, to a block, to a slide, or to the whole tab. If what it was anchored to is edited away, the thread stays, marked "detached", showing what it had.
- **Send it to an agent.** A comment can be sent (`to: agent`) instead of just left. It goes to the session that last wrote that part, if that session is idle. Otherwise it starts a session, from the document's project or work if it has one (the latest session there), else from Settings → Docs, with the prompt `docs.comment`: the document, the thread, and how to answer. This is what decks.md §4's "comments per page" asked for.
- **The agent answers in the thread.** An edit made for a comment carries `answering: <thread>`, so the thread links to the change. Then the agent replies in the thread, saying what it changed (or why it didn't). It never resolves a thread you wrote in; you do.
- **State.** A thread sent to an agent and not yet answered is the agent's move, so it adds nothing to what needs you. Once the agent replies, the document's work or project **needs you** until you resolve the thread or reply.
- **Comments from people outside carbon6** are not in scope; comments made in Google after an export stay in Google (§10).

## 9. Where documents are kept, and who writes them

- **Per realm, in its knowledge database** (docs/knowledge.md §3): tables `docs`, `doc_tabs`, `doc_blocks`, `doc_ops` (the history), `doc_snapshots`, `doc_threads`, `doc_comments` and `doc_exports`. They share transactions with records and the graph. Themes are records of the system collection `deck_themes`.
- **Search:** each tab (each deck, with its notes) is mirrored as an item of kind `doc` or `deck`, so memex search, the Brief and agents find them. The mirror is rewritten when a change settles, not on every op.
- **Blobs** for images and files go where the realm keeps its blobs.
- **Limits:** 2 MB of text per tab, 20 tabs per doc, 200 slides per deck, 500 ops per batch.
- **You** write in the document panel (§12).
- **Sessions** have a `doc` tool: `create` (birth: kind, title, Markdown with pending blocks, a deck's theme), `read` (outline, block, slide, search, since), `edit` (a batch of ops with guards), `look` (pictures of slides, or of a doc's pages), `comment` (create, reply), `export` (§10), `list` (documents of its work or project, or search). Claude Code sessions get the same through the worker's per-turn MCP server (docs/subagents.md, "Claude Code sessions" under §14), as `mcp__carbon6__doc`.
- **Which documents a session may write:** those of its realm. A session in a work item reads and writes that work's and its project's documents without asking. Others in the realm it can read; writing to one is a decision you approve once per document. A realm can loosen or tighten this (Settings → Docs).
- **The assistant** gets every operation through the catalog, like everything else.
- **API:** `GET/POST /api/v1/docs` (both kinds), `GET/PATCH/DELETE /api/v1/docs/{id}`, `POST …/ops`, `GET …/read?view=outline|block|slide|search|since`, `POST …/look`, `GET …/history`, `POST …/restore`, `GET/POST …/threads`, `POST …/threads/{id}/comments`, `POST …/threads/{id}/resolve`, `POST …/export`, `POST /api/v1/docs/import`. Deleting a document is destructive and asks first.
- **Events** for triggers: `doc.created`, `doc.changed` (settled, with the authors), `doc.commented`, `doc.thread_resolved`, `doc.exported`.

## 10. Export and import

**Formats.**

| | Docs | Decks |
|---|---|---|
| Markdown | ✓ (with a folder of images) | ✓ (slides as `---` sections, notes as comments) |
| HTML | ✓, one self-contained file | ✓, one file that presents |
| PDF | ✓ | ✓, a page per slide, notes optional |
| Word (.docx) / OpenDocument (.odt) | ✓ | |
| PowerPoint (.pptx) / OpenDocument (.odp) | | ✓ |
| Google | **Google Docs** | **Google Slides** |
| Pictures | | PNG per slide |

**Fidelity.** Text, headings, lists, tables, links and images carry over as themselves. In a deck, structured layouts map to placeholders, so titles, bullets and notes stay editable in PowerPoint and Slides. Charts and diagrams go as pictures (with the data as a table in the notes or an appendix), and html slides go as pictures. Chips go as text: a date as the date, a ref as a link back to carbon6, a dropdown as its option.

**Where it's made.** The head writes Markdown and HTML itself, from its own blocks. Everything else runs in the **render kit** on a worker (docs/knowledge.md §4: the head never parses or renders outside formats). The kit is the decks-render kit grown: Chromium, poppler, python-pptx, pandoc and open fonts. It now **ships with carbon6** as a system kit, available in every realm with nothing to install; a worker builds its image the first time it's needed (it needs Docker or Podman, as kits do). With no worker able to run kits, export offers Markdown and HTML, and says why the rest are missing.

**Google.**
- Exporting to Google makes a Word or PowerPoint file in the kit, and the head uploads it to Google Drive, converted to a Google Doc or Slides. Converted, it is an ordinary Google file you can edit, share and comment on in Google.
- It uses a Google account of the realm with the `drive.file` scope, which reaches only the files carbon6 creates, not the rest of your Drive. If the realm's Google sign-in (the Google apps, docs/knowledge.md §13.6) doesn't include that scope yet, the first export asks you to grant it; with no Google sign-in, Settings → Docs connects one through the same provider.
- **The document remembers the file.** Exporting again offers **Update the Google file** (a new revision of it in Google) or **Make a new one**. Updating replaces what's in Google, so if the Google file changed since the last export, carbon6 says so first. A link on the document opens the Google file.
- Going the other way (bringing Google's edits and comments back) is not in this design (§15).

**Exports are files too.** Each export is kept as a blob with the version it was made from, listed under Export on the document, so you can send the PDF someone saw last week. Sessions can export with the `doc` tool and attach the result to work (for example, a PDF to mail).

**Import.** A Markdown, Word or PowerPoint file (uploaded, or from a worker's folder) becomes a doc or a deck. The kit reads it into Markdown, and the head stores the result. Slides PowerPoint can't give as structure come in as html or image slides. Google Docs and Slides import through Drive's export to Word and PowerPoint.

## 11. Sharing

- **Inside a realm**, every document is readable by its people and sessions. Grants (docs/os.md §3.7) share a document, or the documents with a label, with another realm, read-only or to comment.
- **Outside carbon6**, you share an export: a file, or the Google file with Google's own sharing. carbon6 has no public links; it runs on your tailnet.

## 12. Where you see them

- **The document panel** (`/docs/<id>`) opens in a tile of its own. A doc shows its tabs and text; a deck shows a slide sorter beside the slide being edited, with its notes below. Both show comment threads in the margin, a "being written by…" line while an agent fills it, History, Export (formats, the Google file, past exports), Present for decks, and what refers to it (projects, work, sessions).
- **Editing.** Click a block to edit it in place as Markdown, with chips as small widgets. Typing sends ops when you pause or leave the block. Tables edit cell by cell, task lists tick, dropdowns pick. On a slide, pick its layout and theme from menus and drag slides to reorder. A rich editor comes later (§14 step 7).
- **The Docs list** (`/docs`, in the header menu): recent documents of the realm, docs and decks together, filtered by kind, label, project or author, and searched.
- **Refs everywhere:** a document chip in a session transcript, on a work item or a project opens the document panel beside it.
- **Sessions:** when a session writes a document, the transcript shows a card for it with what changed (for a deck, the slides changed, small), instead of the Markdown.

## 13. Leaving the decks app

- **Decks made with the app** come in once: **Bring in decks** (Settings → Decks, offered while the app is installed) makes a deck per record of its `decks` collection, with its title, brief as the first slide's notes, theme, and slides from its `deck.html`, each section as an html slide. Its PDFs are kept as past exports. The app's themes come in as theme records.
- Then the app can be removed (the usual uninstall, which asks first). `examples/apps/decks.json` and the separate `decks-render` package leave the repository; the render kit lives with carbon6.
- docs/decks.md is marked replaced by this document. Its decisions stand: HTML is the master, PDF always, PPTX with text editable and visuals as pictures, SVG charts and diagrams, three themes, generated images opt-in later, no stock images.

## 14. Build order

1. **Store and API:** documents (both kinds), tabs, blocks with stable ids, slides with layouts and notes, the Markdown exchange, ops with guards, history and snapshots, catalog entries and events.
2. **The document panel** for docs, with block editing, the Docs list, and refs in sessions, work and records. Live updates over the live feed.
3. **The `doc` tool** for sessions (and Claude Code's MCP server), `docs.style`, pending blocks and writing while you watch.
4. **Decks:** slide rendering with themes, the slide sorter and editor, Present, the render kit as a system kit, `look`, the `decks.*` prompts, and bringing in the app's decks.
5. **Comments,** sending a thread to an agent, `docs.comment`, `answering`.
6. **Export and import:** Markdown and HTML from the head; PDF, Word, OpenDocument and PowerPoint from the kit; Google Docs and Slides with `drive.file`; past exports.
7. **Chips and embeds** (date, mention, dropdown, ref, image and file, view, then chart and diagram), search mirroring, grants, and a rich-text editor.

## 15. Not doing

- **Real-time co-editing of the same paragraph** (CRDT keystroke merging). Guards per block are enough for people and agents taking turns. Two people typing in one paragraph get a refusal and a reread.
- **Syncing back from Google or Office.** An export is a copy. Edits and comments made in Google stay there; bringing them back is a later design (reading the Google file's revisions and comments into ops and threads).
- **Files in repos as documents.** A repo's `docs/*.md` stays code, edited by sessions as files. A session can export a document into a repo, and a later design can add a two-way link.
- **Spreadsheets.** A table in a doc is for tracking and comparing, not formulas.
- **Documents as an app.** Apps (docs/apps.md) can embed and link documents, and refer to them from records, but documents themselves are part of carbon6, like sessions and work.

## 16. As built

### Step 1: store and API (2026-10-30)

- **Where.** Each realm's knowledge database (`knowledge/memex.db` for home, `knowledge/realms/<realm>/memex.db` for the others) holds `docs`, `doc_tabs`, `doc_blocks`, `doc_ops` and `doc_snapshots` (memex migration 0029). Root keeps no documents. Threads, comments and exports get their tables with their steps. The head opens a realm's database through the memex (`Memex::of_realm`); the code is `crates/c6-head/src/docs/` (`markdown.rs` the exchange, `model.rs` the document in memory and its operations, `mod.rs` storage, history and events, `http.rs` the routes), and the types are `c6_api::docs`.
- **Blocks.** heading, paragraph, item (one list item with its continuation lines), table, code, quote, html, rule, pending, embed and slide. A slide's regions' blocks are blocks of their own with the slide as parent. Order is a fractional position within (tab, parent, region), so an insert writes only the new block. Ids are `<writer>.<clock>`: `u` for you, `s<6 chars of the session id>`, `a` for the assistant, `t` for a token, and a clock per document; ids are never reused, including after a restore.
- **The Markdown exchange.** A doc's Markdown is split at block boundaries and each block keeps its source as is. `<?c6 block k?>` alone on a line is a pending block (`{type: pending, intent}`) or an embed (chart, diagram, image, file, view: kept, shown in later steps); inside text it is a chip (date, mention, dropdown, ref), kept in the block's `attrs.chips` under an id of its own, the token renamed to it. A deck's slides are separated by `---` (not inside code fences). A slide's header is HTML comments: `<!-- layout: x -->`, `<!-- hidden -->`, `<!-- notes: … -->`; `<!-- region: right -->` moves what follows into a region. Unmarked, a heading first goes to the title region and the rest to the layout's first other region; the first slide without a layout is `title`, the others `body`. Writing a slide back adds only the markers reading it needs, so it reads back the same. Layouts and their regions are `c6_api::docs::LAYOUTS`.
- **Operations** (`POST /api/v1/docs/{id}/ops`, `DocBatch`): `insert` (after or before a block, at the end of a heading's section, into a slide's region, at the end of a tab; at a deck's top level the Markdown is slides), `replace` (a block's Markdown, guarded; `find`/`with` with `nth`; a table `cell`; a slide's `layout`, `notes`, `hidden`), `delete` (guarded), `move`, `table` (add_row, delete_row, add_column, delete_column), `set` (title, theme, size, a tab's title, the order of tabs, a chip's value), `add_tab` and `delete_tab`. A batch applies to a copy of the document and lands whole or not at all. Replacing a block keeps its id for the first block of the new Markdown (a pending block filled becomes its heading); a rewritten slide's region blocks keep their ids in order. A layout that drops a region moves its blocks into the first other region.
- **Guards.** A whole-block replace or delete needs `ifRev` and `ifHash`; without them it is refused (400). A hash that changed, or a `find` that no longer matches, refuses the batch with 409 and `guard_mismatch: … Their edit wins: read it again`, naming who changed it. A slide's hash covers its layout, notes and every block on it.
- **Reads** (`GET …/read`): `outline` (ids, kinds, hashes, headings with levels, first words, pending intents and writers, slides with their layout, title and blocks), `block` and `slide` (Markdown with its `blocks` map, hash, version), `search` (q, also in a slide's notes), `since` (rev: the blocks changed since, those removed, tabs, a title change), `markdown` (a tab or all) and `full`; `at=` reads any earlier version.
- **History.** Every batch is a row of `doc_ops`: who sent it, the turn (for sessions, from step 3), the operations as sent, and what changed (each block, tab and the document's fields as they are after; null for gone; the ids added). Version 1 and every 50th keep a snapshot; an earlier version is the snapshot before it with the later changes replayed. `GET …/history` lists versions with what each added, changed and removed and a line saying so. `POST …/restore` brings back a version, or one block or slide of it, as a new change by you.
- **API.** `GET/POST /api/v1/docs` (filters kind, label, by — `session` for any session — and q), `GET/PATCH/DELETE /api/v1/docs/{id}` (PATCH renames through a `set` op, so a rename is a version, and changes labels), `POST …/ops`, `GET …/read`, `GET …/history`, `POST …/restore`. Every route takes `?realm=`, else the caller's realm, else home. Delete hides the document (kept in the database with its history). All are in the catalog (area `docs`, realm-scoped), so the assistant, CLI and API have them. `look` comes with decks (step 6), threads with comments (step 9), export and import with the render kit (step 7).
- **Events.** Each change goes out at once on the live feed (`LiveEvent::Doc`, to callers who see the realm). Triggers get `doc` events: `created` at once, and `changed` once a document's changes settle (3 seconds with none), with the authors and the versions it covers. `commented`, `thread_resolved` and `exported` are accepted names, fired by their steps.
- **Limits** as §9: 2 MB of text per tab, 20 tabs, 200 slides, 500 operations per batch.

### Step 2: the document panel, the Docs list and refs (2026-10-30)

- **The Docs list** (`/docs`, Docs in the header and the menu; `crates/c6-ui/src/docs.rs`): the realm's documents, recent first, docs and decks together, filtered by kind, title words, a label and who wrote them (you, sessions, the assistant), with "✎ n to write" while pending blocks wait and who is writing them. "+ New doc" and "+ New deck" ask for a title and open the new document beside the list. It reloads on every document change on the live feed. Filtering by project comes with projects (docs/projects.md).
- **The document panel** (`/docs/<id>`, beside what opened it): the title (click to rename), version, who changed it last and when, labels, "Being written by …" while pending blocks wait, tabs when there are several, then the blocks. A block shows who last wrote it on hover. Clicking a block edits its Markdown in place (chips as their tokens, their values kept). The change is sent when you pause typing for 1.5 seconds, when you leave the block, or with ⌘↵; Escape drops it; an emptied block is deleted. Each change carries the version and hash the block was read at; after a save the guard moves to what you saved. A live update while you type keeps your draft (it belongs to the document, not the block). If someone changed the block meanwhile, their edit wins: it's shown, and what you wrote is in the message to copy. A slide's (and an embed's) Markdown comes from the head, so it is edited with its header. Below the blocks, a box adds Markdown at the end of the tab (⌘↵). Pending blocks show their intent and who is writing them, and can't be edited from here until filled.
- **History** (on the panel): versions grouped into runs by the same author within half an hour ("you, 7 edits, 17:33–17:41"), each with what it did, **Open** (the document as it was, read only, with **Restore block** per block) and **Restore** (the whole version, as a new change). Before and after pictures of slides come with decks.
- **Live.** The panel and the list reload on `LiveEvent::Doc` for them.
- **Refs.** Work attaches `doc:<id>` (by hand, the API, or a session's `work` tool) and shows the document's title as it is now, opening it in the panel; a deleted document shows as gone. The panel lists the work that refers to it ("Refers to it"). A `doc:<id>` in a session transcript's Markdown (not in code) is a link that opens the document beside the transcript. A record field holding `doc:<id>` shows as a link in the collection's table. Projects are added to "Refers to it" with projects.
- **Not built in this step:** chips render as text (📅 date, @mention, ▾ option, ↗ ref) and embeds as a placeholder until step 12; table cells, task-list ticks and dropdown picks are edited through the block's Markdown; tabs can't be added from the panel yet (the API's `add_tab` can); a doc's list items render as separate lists, so they sit a little further apart than in one list.

### Step 3: the `doc` tool, `docs.style`, and writing while you watch (2026-10-31)

- **The tool.** Every native session has `doc` (`Doc` for Claude-profile models; `c6_tools::records::Doc`), and every Claude Code turn's MCP server offers it as `mcp__carbon6__doc`, now even when no subagents are offered (the server used to start only with `Task`). It calls the head (`session.doc`, `crates/c6-head/src/docs/session.rs`) in the session's realm, as `session:<id>` (a side chat or subagent as its parent). Actions: `guide` (the text of `docs.style`), `list` (its work's documents and its own first, then the rest; q and kind), `create` (kind, title, markdown, blocks, theme, labels; the result has the ids and keys its blocks got, their hashes, and a note on filling), `read` (view and block, as the API), `edit` (ops, as the API; `answering` for comments later), `history` and `restore`. `id` takes the id or `doc:<id>`.
- **Its work.** A document a session makes is attached to the work it belongs to (`doc:<id>`, by the session), so it outlives the session and shows on the work item.
- **Who writes what** (Settings → Docs, per realm (the store's per-realm keys gained `docs` with projects step 1: it was root's for every realm before), `/api/v1/settings/docs`, `docs.settings.get/put`): `work` (the default) lets a session edit the documents it made and those attached to its work; editing any other refuses the call ("the user is asked…", nothing changes) and opens a decision "Let <session> edit "<title>"?" (Allow, Deny; reference `docwrite:<realm>/<doc>:<session>`). Allow lets that session edit that document from then on (`doc_writers` in the realm's knowledge database, migration 0030); others still ask. `realm` lets sessions edit any document of the realm; `own` only the ones they made, and never asks. Every session reads every document of its realm. A project's documents join "its work's" with projects.
- **What models get wrong** (two runs with a real model, qwen3.6 27b on local Ollama, writing a three-section checklist). The first run named the block it fills `block` (the document being `id`), left out `op`, asked for `action: outline`, and once filled the wrong id from the bare id→hash map it was given, overwriting a bullet. Now: each operation takes `block` as another name for its `id`; the tool's schema spells out every field of an operation; a malformed edit's error shows a valid one; a read view named as the action (`outline`, `block`, …) is read; and `create` and `edit` answer with the blocks they wrote or changed (`blocks` / `changed`: id, kind, first words, hash) and the blocks still `pending` (id, intent, writer) instead of bare hashes. The second run, on these, went guide → create → three fills with no error.
- **Pending blocks.** A pending block records who is writing it. Its writer fills it without a guard (one `replace` of its id per section or slide), since it can't have changed under them while it is still pending; anyone else needs the guard. Once someone edits it, it isn't pending any more, so the writer's unguarded fill is refused ("…is theirs now").
- **`docs.style`** is an editable prompt (Settings → Prompts) with the birth-first procedure (title and a pending block per section before any research, then one fill per edit) and the style (lead with the answer, sections sized to the ask, tables for comparisons, sources cited, refs as `doc:<id>`, guards and "their edit wins", restore deleted text from history, never retype it). The tool's description is short and points to `guide`; the decks' prompts come with decks.
- **While you watch.** A document a session creates opens in a tile of its own at once (the session stays where it is) when the session is on screen (on the live feed's `created` change by `session:<id>`), and fills in as the session edits it. In the transcript a `doc` call's card names its action and title, and once it returns shows a card for the document (made, or how many blocks changed) that opens it. A `doc:<id>` the model writes in backticks links too.
- **Tests:** `documents::sessions_write_documents_with_the_doc_tool` (a scripted model: guide, create with a pending section in the session's work, fill without a guard, an edit of your document refused with a decision, allowed, then landing), `claude_code_docs` (Claude Code's MCP path, with a stand-in `claude` that speaks the protocol over curl: `tools/list` offers `doc`, `tools/call` creates a document by the session, in its work; the real CLI isn't installed here), and a unit test of the pending rule.
- **Not built:** the turn a change came from (`doc_ops.turn` stays empty: the worker doesn't pass it); the card doesn't show which slides changed (with decks).

### Step 4 (milestone 6): decks (2026-10-31)

- **Drawing slides.** `c6_api::slides` turns a slide into a 1280×720 `<section class="slide l-<layout>">` with a `div.region.r-<region>` per region (two-columns puts left and right in `.cols`), a footer with the deck's title and the slide's number (not on title, section and html slides), and `BASE_CSS` (from the decks app's base CSS, every rule under `.c6deck`). A theme's `:root` custom properties are scoped to the deck's wrapper, so decks with different themes share a page. Markdown is rendered by the caller (the UI's renderer, which shows raw HTML as text). Blocks of an html slide go through `sanitize_html`: scripts, frames, objects, forms, links, `on*` handlers and `javascript:` URLs are taken out. Pending slides and embeds draw as dashed placeholders. `deck_html` makes one file of a deck (hidden slides left out unless asked), for exports.
- **Themes** are records of the system collection `deck_themes` in each realm (`DeckTheme`: name, mood, css); Paper, Night and Signal (the decks app's) are written the first time a realm's themes are read. `GET/PUT /api/v1/decks/themes` (`decks.themes.get`, `.put`) reads and replaces them: names unique, CSS only (no `</style>` or scripts), at least one. A deck names its theme (`set` op, `theme`); an unknown name draws with the realm's first theme. **Settings → Decks** edits them, each with a sample title slide, + Theme, Remove, Save.
- **The deck panel** (the document panel for a deck): Theme menu, slide count, **▶ Present** and **Presenter view**; a sorter of thumbnails (hidden ones dimmed) where a slide dragged onto another moves there (`move`), and **+ Slide**; the slide being edited, drawn to the panel's width (it follows the panel's size), with its layout menu (`replace` layout; blocks of regions the new layout lacks move into its first other region), Hidden, **Edit slide** (its Markdown with header and regions, guarded; double-click the slide too), Delete slide (guarded), and its speaker notes (saved when you leave the box). The slide you were on stays picked when the deck changes. A pending slide shows who is writing it.
- **Present** (`/docs/<id>/present`, a window of its own): the deck full screen, hidden slides left out; arrows, space, Enter, Page Up/Down, Home, End and clicks move. **Presenter view** (`?presenter=1`): the slide, ◀ ▶, "n / total", a timer, the next slide and the notes. The two windows follow each other over a `BroadcastChannel`.
- **Prompts.** `decks.flow` (outline as pending slides → approve as a decision → theme as a decision → fill → look → fix → hand over), `decks.storyline`, `decks.design` (the slide Markdown, layouts and regions, rules), `decks.visuals` and `decks.review` are editable prompts, from the decks app's skills rewritten for the doc tool. The `doc` tool's `guide` with `kind: deck` (or the id of a deck) returns them with the realm's themes and their moods.
- **Not built in this step:** `look` (pictures of slides for an agent) comes with the render kit (step 7), since drawing slides into pictures runs on a worker. 4:3 decks are stored (`size`) but drawn 16:9. Before and after pictures of changed slides in History, and the transcript card's list of changed slides, aren't built.

### Milestone 7: the render kit, export, import and `look` (2026-10-31)

- **The render kit** (`c6-render`, its files in `crates/c6-head/kits/render/`: Dockerfile, `render.py`, `kit.json`) is the decks-render kit grown: Debian with headless Chromium, poppler, pandoc, python-pptx, LibreOffice Impress and open fonts. It ships inside carbon6: the head sends its build with each job (`docs::export::spec()`, the image named after its files' content, `c6kit/c6-render:<hash>`), so there is nothing to install or approve in any realm; a worker with Docker or Podman builds the image the first time it's needed (a few minutes), in the usual kit sandbox (no network, a read-only root, 2 CPUs, 3 GB, 5 minutes). Tools: `pdf` (an HTML file printed by Chromium, a page per slide for decks), `slides` (a PNG per slide, optionally from `first` to `last`), `docx` and `odt` (pandoc from Markdown), `pptx` (from `deck.json`), `odp` (the pptx turned by LibreOffice), and `import`.
- **PowerPoint** keeps text editable: title and section slides use PowerPoint's title and section layouts, two-columns its two-content layout, the others title and content (a big number as large text, a quote with its attribution); bullets keep their levels; speaker notes go in the notes. html, chart, diagram and image slides, and slides holding an embed or raw HTML, go as a full-slide picture (the slide drawn by Chromium from carbon6's own HTML). 4:3 decks get 4:3 slides. Hidden slides stay out.
- **Export** (`POST /api/v1/docs/{id}/export` with `format`, `docs.export`): a doc to md, html, pdf, docx or odt; a deck to md, html, pdf, pptx, odp or png (a zip of a picture per slide). Markdown and HTML are made by the head from its own blocks (chips as their text, pending blocks left out, raw HTML sanitized; a deck's HTML is carbon6's own slides with its theme, one file that prints a page per slide); the rest by the kit. With no worker able to run kits, those formats fail saying why, and Markdown and HTML still work. **Every export is kept** (memex migration 0031 `doc_exports`): a blob in the realm's knowledge folder with the version it was made from, who made it and when; `GET …/exports` (`docs.exports`) lists them and `GET …/exports/{export}/file` downloads one (named after the document). Triggers get `doc.exported`.
- **Import** (`POST /api/v1/docs/import`: a file's name and its content in base64, `docs.import`): Markdown is read by the head (it is carbon6's own format); Word, OpenDocument text and HTML go through pandoc in the kit, PowerPoint through python-pptx (a deck: each slide's title, its text as bullets with their levels, its notes, and a line saying how many pictures the slide had: pictures don't come in yet). `<?c6 block` in a file's text stays text. The **Docs list** has **Import…** (a file from this device), opening what it made.
- **The panel's Export** shows the formats for the kind (PDF, Word or PowerPoint, OpenDocument, Pictures, HTML, Markdown), downloads what it makes, and lists what was made before, with the version, size and when.
- **The `doc` tool** gets `export` (a format; the file is kept with the document, and the answer says where it is) and **`look`**: pictures of slides `first` to `last` (at most 12 at once; hidden slides count), or of a doc's pages, drawn by the kit and returned to the model as images, so an agent checks its slides by eye as decks.review asks.
- **Verified** with the real kit on this Mac (Docker Desktop): a deck exported to PowerPoint (six slides, every title, bullet and note editable) and PDF (six slide-sized pages matching the panel), a doc to Word, OpenDocument and PDF, the Word file imported back with its headings and lists; and a session on a local vision model (qwen3.6 27b) calling `look`. In the test suite the kit is a stand-in (kit jobs run in this process), since building the image needs Docker and minutes.
- **Fixed on the way:** a worker could start without saying it has a container runtime: its kit sweeper and its first connection looked for one at the same moment, and the second took "looked a moment ago" for "none", so the head sent it no kit jobs until it reconnected. Lookups now wait for the one in flight.
- **Not built:** pictures and files inside imported documents (they need blobs as embeds, step 12); Google Docs and Slides (milestone 10); a doc's export carries its tabs as sections of one file.

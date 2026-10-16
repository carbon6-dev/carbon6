# Knowledge in the head — Design

Status: being built (2026-10-16); §13 records what is built and where it differs. It goes with docs/apps.md: that doc says who owns and changes data, this one says where knowledge is kept and what becomes of the memex. It replaces docs/memex.md §2 (architecture), §4.4 (storage) and §6.1–6.5 (Google accounts and sync) as §11 lists; the rest of memex.md (what knowledge is, the pipelines' work, the mail client's behaviour) still holds.

## 1. The idea

The memex is a database: a knowledge graph with search. Like Postgres, it should store, index and enforce, and leave deciding what the data says to the programs that write it. Two things follow:

- **The head keeps the knowledge graph.** It's not optional and not a separate daemon: carbon6 always has it, beside the records, decisions and triggers it already keeps. `c6m` goes away.
- **Everything the memex *does* becomes an app or a kit.** Gmail, Calendar and Contacts sync are apps, like decks (§6). Extraction, OCR, embeddings and face detection are kits on workers (§4). Understanding (summaries, facts, links between items and things) is apps that write claims (apps.md §9).

## 2. Why the head

The memex was a separate service so it could run where the disk and the GPU are (memex.md §2.1). That's a placement reason, and §3–§5 meet it in other ways. Against it:

- **Two stores, one ownership model.** Records live in the head and knowledge in the memex, so a write that touches both can't be one transaction, and collections are mirrored into the memex for search.
- **Access is decided in the head** (os.md §3.8), then enforced again by the memex from a scope the head stamps on each call.
- **One more service** to enroll, run, upgrade and back up.

With the graph in the head:

- **One store** for records, items, things, claims and facts, with one change feed for triggers and one transaction across them.
- **One access check,** where it's already made.
- **One service,** one backup, one upgrade path for the database.

## 3. What the head keeps

- **Per realm, a knowledge database of its own:** `<data_dir>/realms/<realm>/knowledge.db` (SQLite, WAL), holding items, parts, things, claims, facts, notes, trails and vectors. home is `realms/home/`. Deleting a realm deletes its folder; a large realm doesn't slow the others.
- **Collections and records** move into the same file, so records and the graph share transactions, and records are graph citizens rather than a mirror (os.md §4.1's "where they live" changes).
- **The head's own database** (`c6h.db`) keeps what isn't a realm's knowledge: sessions and their events, workers, connections, settings, decisions, installs.
- **Search:** full text (FTS5) in each realm's file, as today; vectors in the same file with an approximate index (HNSW) built in memory when a realm is first searched, and dropped when it's idle, within a size you set.
- **Blobs:** see §5.

The `c6-memex` crate becomes a library linked into `c6h`: its schema, queries, graph and search move over; its fabric service, enrollment and RPC relay go.

## 4. The head stores; it never parses or computes

Mail, PDFs, Word files and images come from outside. A parser bug in the head would expose the process that holds `master.key`. So:

- **Parsing runs in kits on workers.** HTML to text, PDF and Word extraction, OCR, image and video decoding: sandboxed as kits are (packages.md §4: no network, a read-only root, resource limits). The head receives text, metadata and parts through ingest (§6.4).
- **Models run on workers.** Embeddings, captions, transcripts and faces: as kit runs, or through model connections from sessions. The head stores the vectors and serves the index.
- **What the head still does with content:** checks it against the item kind's schema (§7), hashes it, indexes it for full text, and stores it.

A head with no worker can store what's uploaded with its text already extracted, and search it; it can't extract.

## 5. Blobs live where you choose

Your memex is already 16 GB, mostly mail bodies, photos and videos. The head's machine shouldn't have to hold a photo library.

- **A blob has a location:**
  - `head:` the head's data folder (content-addressed, `blobs/ab/cdef…`), for mail and uploads, as the memex keeps them now;
  - `worker:<name>:<path>`: where media already lives (memex.md §4.1), read through that worker when needed;
  - object storage (S3 and alike) later.
- **The head keeps metadata, parts, thumbnails and indexes;** the bytes stay where they are.
- **A realm can choose** where its new blobs go (Settings → System), so a business's mail can live on that business's machine.
- **Backup:** each realm's `knowledge.db`, plus the blobs at `head:`. Blobs elsewhere are backed up where they live.

## 6. Mirror apps: Gmail, Calendar and Contacts

They are apps, installed as instances in a realm (apps.md §5), exactly as decks is. What makes them different is the pattern, not their place in carbon6:

- **decks is a service:** it runs when asked and hands its output over (apps.md §10).
- **Gmail is a mirror:** it keeps a copy of data whose source of truth is elsewhere. It must be the only writer of that copy, which ownership gives it (apps.md §6): the Gmail instance owns its mail items. Archiving, labelling or sending is an operation of the instance, which writes to Gmail and then updates the copy.

memex.md §6 still describes what they do (full history import, incremental sync, both-way changes, Gmail as the source of truth, spam kept out of agents' context, write-back rules for contacts, conflicts settled field by field). What changes is that they're written as packages on the primitives below, so an IMAP, Outlook, CalDAV or CardDAV app is written the same way, and none of them is special.

### 6.1 The packages

| Package | Owns | Operations | Triggers |
|---|---|---|---|
| `gmail` | `email` and `attachment` items, `sent_by` / `sent_to` / `cc` claims, labels and read state; internal `sync_state` (history IDs per account) | `send`, `draft`, `archive`, `label`, `mark_read`, `trash`, `import` (mbox, Maildir, Takeout) | sync every 2 minutes, every 30 seconds while the mail UI is open |
| `google-calendar` | `event` items and things, `attended` / `invited` claims; internal sync tokens | `create`, `update`, `move`, `delete` (with "this one / following / all"), `rsvp` | sync on a schedule |
| `google-contacts` | person things it creates, contact fields as claims, groups; internal sync tokens | `create`, `update`, `write_back` (the `carbon6` group rule of memex.md §6.5) | sync on a schedule |

- **Accounts:** one instance per realm, with that realm's accounts in its settings. Personal Gmail in home, a business's Workspace account in that business's realm; nothing crosses unless you grant it.
- **Policies** of the realm still hold: sending, inviting and writing back ask first by default, as `mail_send` does today.
- **Mail is untrusted input** (memex.md §6.6), still: agents get it as quoted data, and sessions that process it run with the restricted tool set.

### 6.2 The primitives they need

The rule (os.md §1): if an app needs code in carbon6, a primitive is missing. These are the missing ones. Each is general; none mentions Google.

1. **Triggers that run a kit, with no agent.** A schedule or event trigger gets the action `kit`: run a kit tool of the instance with mapped arguments. No session, no model. Syncing every two minutes must not start an agent.
2. **OAuth providers declared by packages.** A package names a provider: authorization URL, token URL, scopes, and the secrets that hold its client ID and secret. The head runs the flow at its redirect address and keeps the refresh token as a secret of the instance's realm. A kit run gets a short-lived access token in its environment, never the refresh token. Today's Google sign-in (memex.md §6.1) becomes one declared provider.
3. **Network allow-lists for kits.** `network` gains a list of hosts (`["gmail.googleapis.com", "oauth2.googleapis.com"]`), enforced by the worker. packages.md §8 listed this as later; mirror apps need it.
4. **Kit output that becomes owned data.** A kit run can return an ingest batch (items, parts, things, claims, record writes) as a file it writes. The head ingests it as the instance that ran the kit, checked like any write of that owner (apps.md §6). The kit gets no token for the head, so the sandbox stays closed. Extraction kits (§4) feed the graph the same way.
5. **Kit operations.** Beside *write* and *template* operations (apps.md §7), an operation can run a kit tool: send, archive, RSVP, edit a contact. It runs at once, returns its result, and its output is ingested as in 4.
6. **Shared item kinds.** `email`, `attachment`, `event`, `contact`, `photo`, `document` and the rest are defined once, in root's vocabulary (apps.md §9.4), each with a schema and a data card. Gmail, Outlook and IMAP produce the same `email`, so readers never care which app it came from.
7. **View kinds for rich data.** The mail client, the calendar and the photo library can't be made from tables and boards, so they become view kinds (os.md §4.5): `messages`, `calendar` and `gallery`, each rendering items of a shared kind, from whichever owner. An app binds one to its items and its operations: the mail view's Archive button calls the owning instance's `archive`. The UI code stays in carbon6, but it isn't Gmail's code.

### 6.3 Events

New mail, a changed event or a new contact is an item or record event (os.md §4.3), so mail rules, triage and meeting preparation are triggers in the realm, as os.md §12.3 already planned.

### 6.4 Ingest

`memex.ingest` (memex.md §4.5) becomes the head's ingest: the same batches typed by kind, written as the caller. A client with a realm token writes as `import:<kind>` into that realm, as today; an instance writes as itself. Parsing moves out (§4): a batch carries extracted text and parts, or names a blob for an extraction kit to read.

## 7. Understanding as apps

The memex's understanding steps (summaries, topics, commitments, captions, linking items to things, merging people) become system packages, installed in each realm that wants them:

- **They write claims** they own (apps.md §9): a `mentions` edge, a `same_as` between two people, a fact with its evidence. A wrong one is corrected by your own claim, which reader policies rank first; nobody edits another owner's claims.
- **They start from events** through triggers, and their prompts are their package's skills, editable by forking, not strings in code.
- **The review queue** (merge suggestions, low-confidence links, unnamed face clusters) is decisions, as os.md §4.2 planned; answering one asserts your claim.

## 8. Your memex, moved

An upgrade moves an existing memex into the head, all or nothing, after a backup:

- **Per realm,** the memex's database (`~/Library/Application Support/c6m/memex`, and each `realms/<id>/`) becomes `realms/<realm>/knowledge.db` under the head's data folder. Items keep their ids.
- **Blobs** stay where they are and are referenced in place: the content-addressed store becomes the realm's `head:` blobs by location, and media keeps its `worker:` locations. Nothing is copied that doesn't need to be.
- **The graph:** entities become things; links become claims, their `made_by` becoming the owner (you, or the system package that replaces the pipeline that made them, at its first version); evidence becomes derived-from links.
- **Collections** in the head's database move into each realm's knowledge file; the memex's record mirror is dropped.
- **Mail, calendar and contacts:** a `gmail`, `google-calendar` and `google-contacts` instance is installed in home with your accounts, and takes ownership of what was synced, with its sync state (history IDs, sync tokens), so nothing is fetched again. Your OAuth client and tokens move to the declared provider unchanged.
- **Then `c6m` is stopped and removed:** its Homebrew service, systemd unit, enrollment and worker row. The upgrade says when it's safe to delete its state folder; it never deletes it itself.
- **Checks** compare counts and samples between the old and new stores, per realm, before the switch.

## 9. Build order

1. **Knowledge in the head:** the `c6-memex` store as a library in `c6h`, per-realm knowledge files, collections moved in, search and vectors served by the head, the memex API answered by the head.
2. **Never parse:** extraction, OCR and embeddings as kits on workers; ingest carrying parts; blob locations.
3. **Kit primitives:** kit trigger actions, kit operations, kit output as owned data, network allow-lists.
4. **OAuth providers** declared by packages.
5. **Shared item kinds** in root's vocabulary, and the `messages`, `calendar` and `gallery` view kinds.
6. **The Gmail, Calendar and Contacts packages,** and understanding as system packages.
7. **Moving an existing memex** (§8) and removing `c6m`.

Steps 1–2 depend on apps.md's ownership (its step 1); 6 depends on its instances and operations.

## 10. Decisions

| Question | Decision | Why |
|---|---|---|
| Where knowledge lives | In the head, always; one knowledge file per realm | One store, one access check, one service; the graph isn't optional |
| Parsing and models | On workers, in sandboxed kits; never in the head | Untrusted content stays away from the process that holds the master key |
| Blobs | Where you choose: the head, a worker, later object storage | The head's machine needn't hold a photo library |
| Collections | In the realm's knowledge file, beside the graph | Records and knowledge share transactions; no mirror |
| Gmail, Calendar, Contacts | Apps on general primitives, one instance per realm | Nothing special; other providers are written the same way |
| What the memex's programs become | Kits (parsing, models) and system packages (sync, understanding) | Storage is the kernel; everything else is replaceable |

## 11. What this replaces

- **memex.md §2 (architecture), §2.1 (a separate service), §2.4 (what the memex may do on the head):** the head is the store; there's no fabric service to authorize.
- **memex.md §2.3 (scopes by labels):** realms and instances' approved reads (apps.md §5.2).
- **memex.md §4.4 (storage):** §3 and §5 here.
- **memex.md §6.1–6.5 (Google accounts, mail, calendar and contacts sync):** the packages of §6, on the primitives of §6.2. What they do is unchanged.
- **os.md §4.1 "where they live":** collections move from `c6h.db` into each realm's knowledge file.
- **The `c6m` binary, its packages (Homebrew, .deb) and `make dev-memex`.**

## 12. Open questions

- **Long-running kits.** Polling every 30 seconds starts a container each time. Is that fine at personal scale, or do mirror apps need a kit that stays running between runs?
- **A head with no worker.** Should a head ship a minimal extraction kit it can run itself, sandboxed, so uploads of plain text and Markdown work with no worker at all?
- **Vectors at scale.** Millions of vectors per realm fit in memory on one machine; whether the index should live on disk (an on-disk ANN index) before that becomes a problem.

## 13. As built

### 13.1 Knowledge in the head (step 1)

- **The memex is a library of the head.** `c6h` opens it at start (`knowledge.rs`) and keeps it in `HeadState`.
  - Its link to the head is in-process: `HeadLink::local` sends its calls (settings, events, decisions, notices) to `fabric::memex_call`, and its credential requests to the same broker workers use.
  - The head's memex calls (the outbox, briefs, status, the API, workers' relayed calls) go straight to it, each with a time limit.
- **Where knowledge lives:** `<data_dir>/knowledge/`, in the memex's own layout: home's `memex.db` and blobs at its root, each other realm's under `realms/<id>/`. This differs from §3's `realms/<realm>/knowledge.db` names, but the separation is the same.
- **A separate `c6m` is turned away** when it attaches ("the memex is part of the head now"). Moving an existing memex in, and removing `c6m`, is step 7.
- **Not yet:** parsing and models still run where the memex ran, which is now the head's process. Steps 2 and 6 move them out to kits and system packages.
- **Deviation: collections stay in `c6h.db`** rather than moving into each realm's knowledge file (§3). Every settings list, the memory notes and the prompts are records of system collections, so moving collections would split the head's own configuration across per-realm files. The record mirror to search is in-process now, so it is synchronous rather than a fabric hop. One store for both is left as a follow-up.
- **Briefs and memory notes:** with a knowledge store always present, every session gets a brief. Memory notes now always come in the session's Memory section (filtered by their scope), and briefs leave them out instead of repeating them. Before, a scoped note could show in an unrelated session's brief.

### 13.2 Moving an existing memex in, and removing c6m (step 7)

- **On start:** the head looks where `c6m` kept its state on this machine: c6m's usual folder, and `/var/lib/c6m`. A head run from its config file looks; a test head (no file) never does.
  - If the head's knowledge is still empty and that folder's `memex/` holds knowledge, the folder is moved in as `<data_dir>/knowledge`. It's a rename, so nothing is copied on one disk.
  - Whatever the head had there is kept aside as `knowledge.before-<time>`.
  - Memex worker rows are retired, and a follow-up says how to remove `c6m` (`brew services stop c6m && brew uninstall c6m`, or `apt remove c6m`).
- **While `c6m` runs** (it holds its lock), nothing moves. A follow-up asks you to stop it, and the move happens on the next start.
- **Elsewhere:** `c6h import-knowledge <memex folder> [--copy]`, with the head stopped, for a memex on another machine (copy its folder over first) or another disk.
- **Removed:** the `c6m` binary and crate entry, its Homebrew formula, its .deb and systemd unit, `make dev-memex`, and the memex enrollment in Settings. The memex library (`c6-memex`) stays, now linked into `c6h`.
- **Not yet:** Gmail, Calendar and Contacts still sync from the memex code inside the head; they become apps with step 6.

### 13.3 Kit jobs, allow-lists and OAuth providers (steps 3 and 4)

- **Kit jobs** (`kitjobs.rs`): the head runs a kit tool on a worker with no session and no model. Workers with a container runtime say so (capability `kits`) and answer `kit.job`.
  - The job runs in a folder of its own under the worker's state, with its inputs written there first, in the sandbox the kit's permissions describe.
  - What it leaves in `out/` comes back, up to 64 MB.
  - A first run may build the kit's image; the head allows 35 minutes on top of the run's own limit for that.
- **Kit output that becomes owned data:** an `out/ingest.json` is kept as the app that ran the job (`app:<name> kit:<kit>.<tool>`), each part checked like any write of that owner:
  - `records` (by `id`, or by a `key` field's value, else new);
  - `things`;
  - knowledge `items`;
  - `claims`.
- **Triggers:** the `kit` action, `{instance, kit, tool, args}` (an app's own trigger runs its own app's kit), with the event mapped into `args`.
- **Operations:** kind `kit` (`{kit, tool, args}`, mapped from the input). The kit must be one the package requires.
- **Network allow-lists:** a kit's `network` can be `hosts`, with `hosts: [...]`.
  - Its runs go on an internal container network with no route out. A tinyproxy container sits on that network and on the default bridge, and denies every host but the listed ones (`HTTPS_PROXY` in the run).
  - The proxy image (`c6kit/egress`, from Alpine) is built on first use.
  - Tested on Docker Desktop: the listed host is reached and others are refused.
- **OAuth providers** (`providers.rs`, migration `0044`): a package declares `providers` with `authorize_url`, `token_url`, `scopes`, an optional `userinfo_url` and `account_field`, extra `params`, and the realm secrets that hold your OAuth client (`client_id_secret`, `client_secret_secret`, which must be among the secrets it asks for).
  - You connect accounts from its entry under Settings → System → Packages.
  - The head runs the sign-in at `/auth/oauth/callback` and keeps each account's refresh token encrypted (`oauth_accounts`).
  - Kit jobs get `C6_ACCOUNTS`: a JSON list of `{provider, account, access_token, expires_at}`, refreshed when near expiry, never with the refresh token.
  - Removing the app forgets its accounts.

### 13.4 Never parse (step 2)

- **c6-extract:** the parsers the memex had in-process now live in a crate of their own, `crates/c6-extract`: mail (mail-parser, HTML to text), PDF, Word and HTML documents with paper recognition, EXIF and thumbnails, HEIC (libheif), video containers, keyframes and sound (ffmpeg), and faces (tract with your ONNX models).
  - It is a **system kit**. Its source, `Dockerfile` and its own `Cargo.lock` are embedded in `c6h`, and the worker builds the image: Rust in a build stage, ffmpeg and libheif in the image. On Docker Desktop that takes about 2 minutes, once.
  - It runs with no network, 2 CPUs, 2 GB and 10 minutes a run.
  - It has one tool, `extract`, which runs `jobs.json`: a tool name (`document`, `mail`, `attachment`, `media`, `faces`, `video`) and jobs. Results go to `out/result.json`, and files to `out/<job>/`. A job that panics is reported as that job's error.
- **Approval, then upgrades:** the first time something needs reading, the head adds the package and asks you to install it (Decisions). Answering Install retries what waits.
  - A newer kit in a later carbon6 is offered as an upgrade.
  - A unit test ties the kit's version to a digest of its source (`kit.digest`), since the registry refuses changed content under the same version.
- **The queue** (`extractions` in each realm's knowledge database, migration 0009): one job per key, such as `doc:<source>:<id>`, `mail:<source>:<id>`, `gmail:<account>:<id>`, `folder:<location>`, `faces:<item>`, `video:<item>`, `reparse:<item>`.
  - A runner sends batches of one tool and one place: up to 24 jobs, and up to 24 MB of the store's own files.
  - What comes back is kept as before: items, the mail store, papers' citation graph, media rows, faces grouped into people, keyframes and transcripts.
  - When no worker can run it, or you haven't approved it yet, jobs back off from a minute, doubling, to an hour. The status shows how many wait (`extracting`) and why.
- **Where files are** (§5): a job's input is `head:<blob>` (uploads, ingested and synced mail), or `worker:<name>:<path>`.
  - A watched folder names its worker (`worker` on Memex folders; empty means the worker on the head's machine, matched by hostname). The worker lists it (`fs.walk`), and its files are mounted read-only into the kit's run at `/mnt/N`. Nothing is copied.
  - Originals open through the worker (`fs.read_file`, up to 40 MB).
  - Bare paths in folders kept before this mean the head's machine, so nothing is read again.
- **Ingest and uploads wait** up to a minute for the kit (90 seconds for an upload), then answer as before: created, updated, duplicate or rejected. Past that, an item is `queued`.
  - Plain text and Markdown are kept without the kit, so a head with no worker can still take them (§12's question, answered for text).
  - Files the store holds itself are capped at 40 MB, since they travel to the worker inside one message.
- **Mail is read once:** its text and HTML are kept as blobs, and its attachments' names in the mail row. A thread shows them without parsing. An attachment's bytes are read from the message by the kit when you open it.
  - Mail kept before this is read again in the background, 100 at a time. Until then a thread shows its searchable text.
- **Removed from c6-memex:** htmd, image, kamadak-exif, mail-parser, pdf-extract, tract-onnx, zip and regex; and running ffmpeg and sips on the head.
  - Tests use the same code in-process (`c6_memex::extract::InProcess`, behind the `in-process-extract` feature that only tests enable).
  - `C6_TEST_KITS=1` runs the real kit end to end: an approval, a Docker build, a folder on a worker read in place, and mail with an attachment.
- **Not done:** embeddings still go to a model connection from the head (§4 allows it); OCR of scans is not in the kit yet; object storage for blobs (§5) is later.

### 13.5 Shared item kinds and their views (step 5)

- **Item kinds in root's vocabulary:** `email`, `attachment`, `event`, `contact`, `document`, `paper`, `photo`, `video` (and `note`, `session`, `record`). Each has a `schema`, the JSON Schema of its items' `meta` (for example, an email has a `thread_id`; an event has `end` and `all_day`), and a data `card` (sources, invariants, freshness).
  - Vocabularies seeded before this get the schemas, cards and new kinds added. Entries you changed keep your changes.
  - A schema is checked when you save the vocabulary.
- **Apps' items are checked:** an item in a kit's `out/ingest.json` of a kind with a schema must fit it, or it is refused, with the reason among the run's errors. Items carbon6 makes itself (the Gmail sync until step 6, extraction) already fit.
- **View kinds** `messages` (email), `calendar` (event) and `gallery` (photo and video). Their source is `{"items": kind, "owner"?: app}`.
  - The owner defaults to the app that brings the view. A view of your own with no owner shows anyone's items.
  - `GET /api/v1/views/{name}/items` lists them: newest first with `before`, or from a time on, oldest first, with `from` and `to`. `…/items/{id}/thumb` gives a picture: a photo's thumbnail, or the blob an item's `meta.thumb` names.
- **Actions on items:** `POST /api/v1/views/{name}/act` with `item` calls an operation of the owning app (`op`, or `app.op`), its input mapped from the item (`$.id`, `$.title`, `$.meta.thread_id`), as that app. It can also start a template with the item as input (sessions labelled `item:<id>`).
- **In the UI** (`itemviews.rs`):
  - **messages:** threads (by `thread_id`) beside the chosen thread's messages, with the actions acting on its newest message;
  - **calendar:** a week by day, with times or "all day";
  - **gallery:** a grid of thumbnails, with photos and videos side by side, and one opened larger with its actions.
  - Each has paging back in time.
  - The Mail, Calendar and Photos pages stay as they are until step 6 moves mail, calendar and contacts into apps.


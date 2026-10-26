# Memex — Design

Status: v0.2 (2026-10-11). The whole build order (section 10) is built; the "As built" sections record where it differs. The Google flows and face recognition are tested only against fakes. v0.2 added Google accounts, calendar and contacts, and records the decisions in section 11.

> **Proposed change:** [knowledge.md](knowledge.md) moves the knowledge graph into the head and removes `c6m`. It replaces §2 (architecture, including §2.1, §2.3 and §2.4), §4.4 (storage) and §6.1–6.5 (Google accounts and sync, which become apps). The rest of this doc still describes what the memex's knowledge is and what its pipelines and mail client do.

> "A memex is a device in which an individual stores all his books, records, and communications, and which is mechanized so that it may be consulted with exceeding speed and flexibility." (Vannevar Bush, *As We May Think*, 1945)

## 1. Goals

The memex is your personal knowledge store, and it holds the context carbon6 works from.

- **Everything in one place:**
  - 20 years of email, and the email you send and receive from now on;
  - documents and research papers, with notes;
  - photos and videos;
  - web pages your agents read;
  - carbon6's own sessions, outputs and decisions.
- **A graph, not just an index:**
  - people, organizations, places, topics, projects, events, claims, and the links between them;
  - every link has evidence and a time.
- **Associative trails:** named paths through items and things, with commentary. This is Bush's central idea.
- **Context flows both ways:**
  - carbon6's work becomes knowledge in the memex;
  - the memex supplies context to sessions, and its events start agents.
- **carbon6 replaces your email client, calendar and address book,** for any number of Google accounts (Gmail and Google Workspace), with the memex as their store and context.
- **Media understanding:** faces (named people), places, objects and scenes in photos and videos.

**Not goals:**
- multi-user;
- sharing;
- encryption at rest: that's left to the disks (FileVault, LUKS, an encrypted NAS volume).

## 2. Architecture

> Replaced, if [knowledge.md](knowledge.md) is built: the head keeps the graph (§2–§3 there).

```
                         c6 fabric (mTLS, private network / tailnet)
   browser ──HTTP/WS──► c6h (head) ◄────────────── c6w workers (sessions, tools)
                          ▲  │ relays memex calls      ▲
                          │  │ (workers → head → c6m)  │ compute jobs
                          │  ▼                         │ (optional)
                        c6m (memex service) ───────────┘
                          │  store: SQLite (meta, graph, full text) + vector index + blobs
                          │  sources: IMAP/Gmail/JMAP, folders on itself or workers, uploads
                          └─ model calls: through c6 connections (your choice per task)
```

### 2.1 A separate service on the fabric

- **`c6m`** is a new binary and a new crate, `c6-memex`. It runs anywhere: the head's machine, a home server or a NAS. It can be placed wherever the disk and the GPU are.
  - It starts on the same Mac as the head, but nothing may assume that. It reaches the head only over the fabric, has its own data directory, refers to media by location (`worker:<name>:<path>` or `memex:<path>`), and reaches models through carbon6 connections. Moving it to another machine is a re-enrollment plus copying its data directory.
- **It joins the fabric the way workers do:**
  - it enrolls with a token from the head (Settings → Memex → "Connect a memex");
  - it receives a client certificate from the head's CA, with the role `memex` in its subject;
  - it dials the head and keeps one HTTP/2 stream open. The head never dials it, so it works behind NAT.
  - Exactly one memex is attached per head; this can be relaxed later.
- **Reaching the memex:**
  - the head calls it over that stream with request/response RPCs (the existing `RpcRequest`/`RpcResponse` frames, under the method namespace `memex.*`);
  - workers reach it through the head (worker → head → memex), with the session's scope stamped on by the head (see 2.3). A direct worker-to-memex path can come later, as an optimization.
- **The memex reaches the head** over the same stream, through a small, explicit API (2.4), so it can start agents and use carbon6's connections, secrets and notifications.

### 2.2 The memex API

The API is JSON over the fabric. Each method is also exposed by the head under `/api/v1/memex/…` for the UI and `c6`, with the head checking sign-in.

| Area | Methods |
|---|---|
| Items | `ingest` (any type, by MIME type and its handler: 4.5), `types` (the handlers and their attribute schemas), `items.get`, `items.content` (raw bytes, such as the original email or the full image), `items.delete` (with what derives from it) |
| Search | `search` (hybrid: keyword, meaning, graph filters, time; results grouped by kind), `similar` (more like this item) |
| Graph | `entity.get` (a card: facts, aliases, timeline), `entity.neighbors`, `entity.merge`, `entity.split`, `link.add`, `link.retract`, `timeline` |
| Writing | `notes.put` (an insight or a note), `trails.*` (create, append a step, comment, list) |
| Context | `context.brief` (labels plus optional text → a token-budgeted brief with sources), `mention.complete` (for @-mentions) |
| Mail | `mail.accounts.*`, `mail.list` (views: inbox, needs reply, waiting, a label or folder), `mail.thread`, `mail.flags` (read, star, archive, label, trash), `mail.draft.*`, `mail.send`, `mail.rules.*` |
| Sources | `sources.*` (add, configure, pause, re-scan), `jobs.status` (pipeline progress and backlog) |
| Review | `review.list` (merge suggestions, low-confidence links, unnamed face clusters), `review.decide` |
| Events | `events.subscribe` (the head subscribes once and forwards to automations and the UI) |

### 2.3 Scopes: what each caller may see

The memex holds more sensitive data than anything else in carbon6, so every call carries a **scope**:

- **The UI (you):** sees everything.
- **An agent session:** sees what its labels allow. The head works this out from the `memex_scopes` setting: rules of the form *labels → allowed sources and topics*, edited in Settings like policies. For example:
  - `email:` only for sessions labelled `mail`;
  - research papers for every session;
  - a client's material for `client:acme`.

  The head stamps the scope on each relayed call, and the memex enforces it in every query. A worker can't widen it.
- **Automations and workflows:** get the scope of the sessions they start, decided by those sessions' labels.

### 2.4 What the memex may do on the head

The memex's certificate authorizes only these actions:

- `workflows.start(name, input)` and `templates.run(name, input, labels)`: for memex event triggers, such as "new mail from a key contact → triage";
- `notify(event, title, message, labels)`;
- `credentials.get(connection or secret)`: short-lived credentials for model calls and mail servers. This is the same broker workers use, with secrets scoped by the labels `memex` and `source:<kind>`;
- `sessions.digest(since)`: carbon6's own events, for its ingestion (3.1).

It can't run shell commands, change settings, or create sessions outside templates and workflows.

## 3. Context flowing both ways

### 3.1 carbon6 → memex: work becomes knowledge

- **Session digests.** When a top-level session's turn ends, the head queues a digest job:
  - the memex reads the session's events and writes a digest item: what was asked, what was decided and found, what was produced, which files, hosts and repos were involved;
  - it links the digest to the things involved, through the session's labels and the names mentioned;
  - the transcript stays in carbon6, and the digest links to it.
- **Outputs:** published files become items, linked to their session.
- **Web pages** read by the browser tool or web fetch become web items: URL, title, text and when they were read.
- **Memory notes** become notes on things. `remember` and `forget` keep their meaning, and the head's `memories` table migrates into the memex once one is attached.
- **Decisions:** approvals, denials and workflow results become events linked to the policy, the host and the session.
- **Deliberate writes:** agents call `notes.put` and `trails.*`; every write is attributed to the session that made it.

### 3.2 memex → carbon6: knowledge becomes context

- **Labels resolve to things.** `client:acme` → the Acme organization, `host:web1` → that machine, `repo:…` → the project. People are linked to labels by `person:<id>`, with aliases resolved.
- **Context brief at session start.** The worker asks for `context.brief` with the session's labels (and its first message) and puts the result in the system prompt, under its own budget (about 4 KB by default, adjustable). It covers key facts, open commitments, recent events, relevant insights and trails, each with a reference. This replaces today's Memory section.
- **@-mentions:** `@` completes people, organizations, projects, threads, papers and photos (through `mention.complete`), and attaches their cards to the message.
- **Agent tools,** within the session's scope:
  - `memex_search` (hybrid);
  - `memex_get` (an item or a thing with its neighborhood);
  - `memex_related` (follow links);
  - `memex_timeline`;
  - `memex_note` (save an insight or a link);
  - `memex_trail` (add a step).
- **Event triggers:** a new trigger type for automations, *memex event*, with filters such as:
  - `mail.received` (from, to, labels, rules);
  - `commitment.due`;
  - `item.added` (source, topic);
  - `entity.changed`.
- **Inbox:** mail that needs a reply and commitments due appear in carbon6's inbox next to sessions waiting for you.
- **Pages link both ways:** a thing's page lists the sessions, workflows and outputs about it, and a session lists the things it touched.

### 3.3 Keeping the loop honest

- **Two kinds of claims:**
  - **observed:** from structure (headers, metadata), from you, or from a source document's own text, with the passage cited;
  - **inferred:** model extractions, agent insights, guesses at identity.
- **Inferred claims never count as evidence for further inferences** until they're confirmed, either by you or by observed support.
- **Every claim carries:**
  - its evidence: items and passages;
  - who or what made it: you, a pipeline and its version, or a session;
  - how confident it is;
  - when it held: from and until.
- **Briefs show their sources.** Correcting a claim, in a brief, on a thing's page or in the review queue, records an observed correction that outranks the inferences.
- **Re-running a pipeline** with a better model replaces that pipeline's inferred claims and leaves observed ones alone.

## 4. Data model

### 4.1 Items: what goes in

```
item(id, kind, source_id, external_id, content_hash, blob_ref | location_ref,
     title, created_at /* when it happened */, added_at /* when it entered */,
     meta JSON, deleted_at)
kinds: email, attachment, document, paper, note, photo, video, audio, web,
       session_digest, output, event
```

- `location_ref` points to media where it already lives: `worker:<name>:<path>`, or a path on the memex host. The memex never copies terabytes; it stores thumbnails, keyframes and derived content.
- `blob_ref` is the content-addressed store for what has no other home: email (the raw RFC 822 message), uploads and attachments.

### 4.2 Derived content: rebuildable

```
part(item_id, ordinal, kind /* section, passage, ocr, caption, transcript_segment, frame */,
     text, start/end (offsets, page, or time in media), pipeline, version)
vector(part_id | item_id | face_id, space /* the embedding model */, dims, data)
face(item_id, box, frame_time, embedding, cluster_id, person_entity_id?)
object(item_id, label, score, box, frame_time)
place(item_id, lat, lon, accuracy, geocode JSON)
```

### 4.3 The graph

```
entity(id, kind /* person, org, place, topic, project, event, claim, host, repo, label… */,
       name, aliases JSON, attrs JSON, merged_into)
link(id, src_entity, rel, dst_entity | dst_item, valid_from, valid_to,
     confidence, basis /* observed | inferred */, made_by /* you | pipeline@version | session */)
evidence(link_id, item_id, part_id?, quote?)
note(id, body, made_by, created_at)  -- insights; linked like anything else
trail(id, name, description) ; trail_step(trail_id, ordinal, target, comment)
```

- **Relations** are an open vocabulary with a core set:
  - people and organizations: `sent`, `received`, `cc`, `member_of`, `works_at`, `knows`;
  - content: `about`, `mentions`, `authored`, `cites`;
  - media and events: `appears_in`, `located_at`, `attended`;
  - work: `decided`, `committed_to`;
  - claims: `contradicts`, `supports`;
  - structure: `part_of`.
- **Every link is backed by at least one evidence row** or made by you.
- **Identity:**
  - people resolve by email address, name variants, signature blocks and co-occurrence;
  - faces join people when you name a face cluster, or by a confident match to a named one;
  - automatic merges happen only above a high threshold, and the rest go to the review queue;
  - a merge keeps aliases and can be undone (`split`).

### 4.4 Storage

> Replaced, if [knowledge.md](knowledge.md) is built: per-realm knowledge files in the head, and blobs where you choose (§3, §5 there).

- **SQLite (WAL)** for items, parts, the graph, mail state and jobs. Full-text search uses FTS5 with trigram and word tokenizers, as the head already does.
- **A vector index per embedding space:** an in-process approximate index (HNSW) in Rust, with the vectors also in SQLite so the index can be rebuilt. Sized for millions of vectors on one machine.
- **Blobs** in a content-addressed directory (`blobs/ab/cdef…`).
- **The graph** is queried with recursive SQL from Rust; there's no separate graph database to operate.
- **Encryption at rest** is the storage layer's job (per your decision).
- **Backup:** the SQLite file and the blob directory. All derived content can be rebuilt.

### 4.5 Getting data in: one ingest API, typed by handlers

**The problem.** Ingest has grown one path per kind of data: document upload, folder scan, notes, an internal generic ingest the head uses for sessions and memory, and Gmail sync. Each new source (an mbox archive, Takeout, a scanner, a feed) would add another API. Making the API general must not cost any guarantee that a dedicated API gives.

**The rule.** Getting data in is general, and what the data means stays specialized:
- **One way in:** a single ingest call, one envelope, one set of retry and result semantics.
- **The type is the item's MIME type:** `message/rfc822`, `application/pdf`, `text/html`, `image/jpeg`, `text/calendar`, `text/vcard`, and so on. Standard formats are already self-describing, so the memex invents no type system. An email is "a specialized document" at the format level already.
- **Meaning stays in handlers:** each registered handler keeps its own parsing, identity, storage tables and derived structure (threads, people, faces, papers).
- **No common data model:** forcing email into `{title, body, author, date}` would lose recipients, threads, Message-ID, labels and flags, which is exactly the watering-down to avoid. What every handler shares is the base item (4.1: title, searchable text, when it happened, links to things), which feeds search and briefs. Each handler then adds its own records beside it, so the base item plus extension is how "email is a document" shows up in storage.

#### The ingest call

`memex.ingest`, public through `/api/v1/memex/rpc` (signed in, or with an API token):

```json
{"items": [{
  "type": "message/rfc822",
  "data": "<base64 of the bytes>",
  "source": {"kind": "mbox", "id": "archive-2009"},
  "external_id": "Work.mbox#88123",
  "attributes": {"folder": "Work/2009", "read": true, "starred": false},
  "labels": ["personal"]
}]}
```

| Field | Meaning |
|---|---|
| `type` | The MIME type; it picks the handler. |
| `data` | The item's bytes in its standard format, base64. Text types may pass `text` instead. |
| `source` | Where it comes from. Items are unique by source and `external_id` (the `items` table already enforces this). |
| `external_id` | The source's own id for the item, so re-sending is harmless. |
| `attributes` | Facts that aren't inside the bytes: a folder, flags, a received date, a page URL. Checked against the handler's schema. |
| `labels` | Which sessions may see it (2.3), checked as uploads are checked today. |

The reply has one result per item: `created` with its id, `updated` (the same source and id with new content; documents only, since a message never changes), `duplicate` with the existing id, or `rejected` with the handler's reason (for example "not an RFC 822 message: no headers"). A batch never fails as a whole because of one item.

#### The type registry

`memex.types` lists the handlers. Each one declares:

- **Types:** the MIME types it accepts.
- **Attribute schema:** a JSON Schema with strict keys. Unknown keys and wrong types are rejected, so attributes are typed per handler, not a free-form bag.
- **Identity rule:** how it recognizes an item it already has. Mail uses Message-ID (or, without one, a hash of key headers and the body); documents the content hash; papers DOI or arXiv id, then content hash; photos the content hash.
- **Limit:** the largest item it takes.
- **Produces:** the item kinds it creates (an email produces `email` plus `attachment` items).

Importers read the registry, so a new source needs no new API, only a client.

#### Guarantees, and how each is kept

| Guarantee | A dedicated API gives it by | The ingest API keeps it by |
|---|---|---|
| Valid input | its request schema | the handler parsing the standard format (a stricter contract than a bespoke JSON shape), plus the strict attribute schema |
| Right identity and dedup | its own code | the handler's identity rule, plus source and `external_id` for retries |
| Specialized structure | its own tables | handlers keep their own tables (`mail`, `mail_labels`, media, papers) |
| Limits per type | its own limit | declared per handler and enforced before parsing |
| Two-way sync is never faked | only sync code writes mail | **origin is set by the server.** Every ingested item is `imported` and read-only. Only built-in connectors create `mirrored` items, the ones whose changes are written back. The ingest API cannot create a mirrored item. |
| Privacy | its own rules | the handler classifies (spam, mailing lists; spam never reaches agents), and labels are checked as today |
| Clear errors | its own messages | the per-item `rejected` reason |

#### Mail, the first handler

- `message/rfc822` → the parsing that `mail::store_message` does today, without requiring a Gmail account.
- **Accounts and threads:** an imported message belongs to its source (for example `mbox:archive-2009`) instead of a Gmail account. It is threaded by `References` and `In-Reply-To`, since it has no Gmail thread id.
- **Labels and flags:** attributes `folder` (a label), `read`, `starred`, and `received` (a fallback date).
- **Imported and mirrored copies of the same message:**
  - Imported mail that Gmail sync already has is a duplicate; the mirrored copy wins.
  - If Gmail sync brings in a message that was imported earlier, the item is upgraded to mirrored and keeps its id, so links and notes made meanwhile survive.

#### What stays separate

- **Connectors** (Gmail, Calendar, Contacts sync) pull on a schedule, keep cursors and write back, so they are not uploads. Internally they call the same handlers with origin `mirrored`, so synced and imported mail share one code path.
- **Actions** (send, RSVP, edit a contact, label a thread) are not data coming in. Each means something different, so they keep their own methods.
- **Containers** (mbox, Maildir, zip, Takeout) are split by the importer, on its side, into single items. Multi-gigabyte files don't belong in one request, and the importer can keep its own position to resume. A server-side splitter for small `application/mbox` uploads may come later as a convenience.

#### What it replaces

- **`memex.upload`** goes through the document handler, so a file already in the memex is that item. The head's `/api/v1/memex/upload` stays as the UI's wrapper.
- **Folder scan** keeps its own walk and change tracking, and stores each file with the same code the document handler uses.
- **Ingested sources** are always `import:<kind>[:<id>]`, so a client can never write into a connector's source (`gmail:…`) or the head's.

#### As built (2026-10-13)

- **Handlers:** mail (`message/rfc822`) and documents (PDF, Word, Markdown, plain text, HTML, reStructuredText). Photos and videos still come in only from watched folders, because the memex keeps media where it lives instead of copying it.
- **Imported mail:**
  - It belongs to a mail account named after its source (`import:mbox:archive`) and shows in the mail views and label views. Its folder is a label.
  - Flags: `read` defaults to true. A folder named Spam or Junk marks the message spam, so it stays out of agents' context.
  - Labels and read state change only in the memex: there is no Gmail to tell. A reply from an imported thread fails with "no Google account", because there is no account to send it from.
  - It never fires new-mail automations, since it's archive mail.
  - When Gmail sync later brings in the same Message-ID, the item becomes the synced one and keeps its id.
- **Kept as they were:**
  - **Notes** stay `memex.note`, because notes carry who made them and appear in trails.
  - **The head's own items** (session digests, outputs, memory) still send the older single-item form of `memex.ingest`, which its outbox may hold across upgrades. The head's gateway accepts only typed batches from outside, so a token cannot use that form.
- **Through the head:** `memex.ingest` and `memex.types` are callable on `POST /api/v1/memex/rpc` with an API token, are listed in the operation catalog, and are therefore in the API reference agents get (`carbon6_api`) and in `/api/v1/openapi.json`. A request is up to 40 MB, and a message up to 25 MB.

## 5. Pipelines

Jobs live in a durable queue in the memex database, each with a priority, a budget and a version. They run in tiers:

| Tier | What | Cost |
|---|---|---|
| 0. Structure | parsing: email headers and threads (Message-ID, References, In-Reply-To), people and organizations from addresses and domains, file metadata, EXIF time, GPS and camera, PDF metadata, references, video duration | no models; the whole archive in hours |
| 1. Text and search | text extraction (HTML → text, PDF text, OCR for scans), chunking into passages, embeddings | one embedding per passage |
| 2. Understanding | per thread: summary, topics, commitments, entities; per paper: claims, findings, methods, limitations, the citation graph; per photo: caption, objects, scene, faces; per video: transcript, keyframes, faces | model calls, prioritized |
| 3. Synthesis | topic summaries, timelines, relationship summaries, contradictions across papers, "what changed" digests | on demand or scheduled |

- **Priority:**
  - what you or an agent just touched;
  - new arrivals (live mail);
  - things you've marked as important;
  - then recent before old.
- Tier 2 for the 20-year archive fills in gradually in the background, under a budget (Settings → Memex → Budgets, using phase 4's budgets with the label `memex`).
- **Models are your choice per task** (Settings → Memex → Models). Each task picks a carbon6 connection and model, or a built-in local model:

  | Task | Default suggestion |
  |---|---|
  | Embeddings (text) | a local embedding model through Ollama |
  | Embeddings (images, for search by description) | a CLIP-style model |
  | Extraction and summarization | any chat model |
  | Image captions and scenes | a vision model |
  | Faces: detection and embeddings | a built-in ONNX model |
  | Objects | a built-in ONNX model, or the vision model |
  | Transcription | Whisper, local |
  | Reverse geocoding | an offline place dataset, local |

  - A routing rule can force a source onto particular models, for example `source:email` → local only.
  - Built-in ONNX models run in `c6m` on its CPU or GPU, or on a worker that advertises the capability `memex-compute`, for media too large to move.
- **Versioning:** each derived row and inferred link records the pipeline and model version. Upgrading a task's model re-queues its tier for the affected items.

### 5.1 Understanding, as built (step 4)

- **Settings → Memex** picks a connection and model for three tasks: understanding, vision and transcription. A fourth setting, "model calls a day", is one budget shared by understanding and vision (default 500). The memex calls the model directly with a credential from the head; Claude Code connections can't be used for these tasks. Understanding and vision also take a reasoning setting: understanding defaults to none (it sorts and drafts for the brief many times a day, and a local thinking model such as Qwen 3.6 takes about 5 seconds a conversation without reasoning, 90 with), vision to low.
- **What gets read**, newest first:
  - email threads, as one unit, and again when a thread has new mail; mailing lists, spam, trash and drafts are skipped;
  - sessions, documents, papers and web pages.
- **What comes back**, as JSON:
  - a summary (`items.summary`);
  - topics (`topic:<slug>` things, linked `about`);
  - for threads and sessions, commitments: who, what, and by when;
  - for papers, claims: `claim:<hash>` things linked as `claims`, `uses_method` or `has_limitation`, with the sentence kept as evidence.
- **Everything is inferred:** `basis = inferred`, `made_by = understand-v1@<model>`.
  - Changing the model re-reads everything and replaces the old model's links.
  - Commitments you marked done or dropped are kept.
- **Inputs are observed items only.**
  - A model never reads another model's output.
  - "Summarize what I know" about a thing reads the items about it, then saves a note marked inferred, listing its sources.
- **Review queue:**
  - People with the same full name, such as a paper's author and someone you write to, are suggested as merges.
  - Merging moves links, keys and trail steps, keeps the old name as an alias, and deletes the duplicate.
  - The old key still finds the merged person.
  - Nothing is merged automatically.
- **Graph and timeline:**
  - `memex.graph` returns a thing's neighbours (things it shares items with, and things it links to directly) and the links among them. The UI draws it radially; closer and larger means more in common.
  - `memex.months` returns items per month, for the timeline.
  - Both are on every thing's page, under the Memex page's Review and Commitments tabs.

## 6. Mail, calendar and contacts

> §6.1–6.5 become the `gmail`, `google-calendar` and `google-contacts` apps if [knowledge.md](knowledge.md) is built (§6 there). What they do stays as written here.

carbon6 replaces the mail client, the calendar and the address book, starting with Google: any number of Gmail and Google Workspace accounts. The provider layer is an interface, so IMAP/SMTP, JMAP, CalDAV and CardDAV can be added later without changing the rest.

### 6.1 Google accounts

- **Adding an account:** Settings → Mail & calendar → Add Google account. This is OAuth through the head's existing broker (the same one used for ChatGPT sign-in), with the tokens stored encrypted on the head. The memex gets short-lived access tokens over the fabric (2.4) and never holds the refresh token.
- **One-time setup:** Google requires an OAuth client from a Google Cloud project you own. carbon6 shows the steps:
  - enable the Gmail, Calendar and People APIs;
  - create a Web client with carbon6's redirect address, and paste in its client ID and secret, which are kept as secrets.
  - **Personal Gmail:** set the app to "In production". In "Testing", Google expires refresh tokens after 7 days. You'll see Google's "unverified app" notice once, which is expected for an app you made for yourself.
  - **Workspace:** an "Internal" app avoids both issues.
- **Scopes:**
  - `gmail.modify` (read, label, archive, drafts; no permanent deletion);
  - `gmail.send`;
  - `calendar` (calendars and events);
  - `contacts` (read and write).

### 6.2 Mail sync

- **Through the Gmail API:**
  - history import of every message (`messages.list` and `get` in raw format, rate-limited and resumable; 20 years can take a day or more and fills in newest first);
  - then incremental sync with `history.list` from the last history ID. Push through Pub/Sub would need a public endpoint, so the memex polls, every 30 seconds while the UI is open and every 2 minutes otherwise.
- **Gmail stays the source of truth.** Changes sync both ways: labels (including Inbox, Starred, Important, Spam and Trash), read state, archive, and drafts.
- **No exclusions:** everything is brought in, including Spam and Trash. Spam is marked as such and never reaches agents' context.
- **Multiple accounts** appear in one unified inbox or one at a time. Replies default to the account and address the thread came to.
- **Archives no longer on Gmail** can be imported from mbox, Maildir or Google Takeout files. They're marked as imported, not synced, and deduplicated against synced mail by Message-ID.
  - carbon6 ships no importer for these. Any tool can send messages through `memex.ingest` (4.5), splitting the archive on its side.

### 6.3 The mail client, in carbon6's UI

- **Views:** Inbox, Needs reply, Waiting on others, Starred, Sent, Drafts, Archive, per label, and Search.
- **Threads** are shown as conversations.
- **Reading:**
  - HTML mail is shown in a sandboxed iframe (no scripts, an opaque origin);
  - remote images are blocked until you allow them per sender, so tracking pixels don't load;
  - attachments are previewed the way Outputs are.
- **Composing:** Markdown or plain text, sent as multipart (text and HTML). Includes:
  - signatures, per account;
  - attachments from files, Outputs or the memex;
  - undo send (a short hold), send later, and reminders to follow up.
- **Rules:** filters on sender, recipient, subject and content (or an agent's classification) that apply labels, archive, set priority, or start a workflow. Gmail's own filters keep working on Gmail's side.
- **Keyboard first** on desktop, and a phone layout (phase 5's work applies).

### 6.4 Calendar

- **Google Calendar API sync:**
  - every calendar you own or subscribe to, per account;
  - events, recurrences (with exceptions), attendees and responses, reminders, conference links;
  - incremental sync with sync tokens, both ways.
- **The calendar UI:** day, week, month and agenda views; several accounts and calendars overlaid, with per-calendar colors and visibility.
  - **Events:** create, edit, move and delete, including recurring ones ("this one", "this and following", "all").
  - **Invites:** RSVP to invites, from the calendar or from the invite email itself. ICS invites in mail are matched to the calendar event.
  - **Free/busy** across accounts, for scheduling.
- **In the graph,** events are event things, linked to their attendees (person things), places, and the mail threads and documents about them.
  - Before a meeting, a brief can gather the attendees' recent threads, open commitments and related documents.
- **Agents:**
  - **Scheduling:** find times across your calendars and propose them in a reply.
  - **Meeting preparation:** a workflow before each meeting, delivered as a notification.
  - **Follow-up:** after the meeting, draft notes and commitments.
  - Anything that writes to the calendar or sends an invite is held for your approval by default, like `mail_send`.
- **Reminders** use carbon6's notifications, so web push reaches your phone.

### 6.5 Contacts

- **Google People API sync** of contacts and contact groups, per account, both ways, incremental with sync tokens.
- **Contacts are person things** in the graph. Google contacts, other accounts' contacts, and people seen only in mail resolve into one person each, with all their addresses, phones, organizations and photos (including named face clusters).
- **Contacts built from mail:**
  - people you've exchanged mail with in both directions become contact candidates;
  - so do details found in signatures (title, phone, company), as inferred facts with evidence.
- **Writing back to Google:**
  - candidates you confirm, or that pass a rule (for example "replied to at least twice"), are created in Google Contacts, in a contact group named `carbon6` so they stay separable;
  - enrichments of existing contacts are written back only for observed facts, or ones you confirmed. Inferred guesses never overwrite what's in Google.
- **Conflicts** are settled field by field: the most recent change wins. Changes you made in carbon6 win over carbon6's own inferences.

### 6.6 Agents and mail

The triage, requests and drafts sketched here are built as the brief: see [brief.md](brief.md).

- **Triage** (a workflow, on new mail or a schedule):
  - classify, summarize, extract commitments;
  - fill Needs reply and Waiting;
  - send a daily digest.
- **Drafting:** "draft a reply" puts the thread, the relationship with the sender and relevant memex context into a session. The draft appears in Drafts for you to edit.
- **Sending is held for your approval by default.** Mail tools come with a built-in policy: ask before `mail_send`. You can relax it per recipient or label.
- **Email content is untrusted input:**
  - it's passed to agents as quoted data, never as instructions;
  - sessions that process mail run with a restricted tool set by default: no shell, no browser input, no mail sending without approval;
  - links found in mail are never followed automatically.

  This addresses prompt injection, the main risk of an agent reading mail.

## 7. Photos and videos

- **Sources:** folders on the memex host or on workers, and Apple Photos or Google Photos exports. Watched folders pick up new media.
- **Per photo:**
  - time, place and camera (EXIF);
  - a thumbnail;
  - a caption and scene from a vision model;
  - objects;
  - faces: detected, embedded and clustered, then named by you in the review queue. Naming a cluster links it to a person thing, so photos join the same graph as email.
- **Per video:**
  - duration, keyframes (by scene change), and a transcript with timestamps;
  - faces and objects per keyframe;
  - captions per scene.
- **Search** works by person, place, time, object, description ("the whiteboard photo from the Acme offsite") or similarity.
- **Trips and events** are suggested as event things from clusters in time and place.

### 7.1 As built (step 5)

- **Sources:**
  - Photos and videos in memex folders (Settings → Memex folders) are read in place. Only thumbnails, face crops and keyframes are stored.
  - Photo types: JPEG, PNG, WebP, GIF, TIFF, plus HEIC via macOS `sips`.
  - Video types: MOV, MP4, M4V and 3GP. AVI and MKV are listed by file only.
  - Deleting a file removes its item.
- **What the file gives:**
  - EXIF: time (with its offset, else the machine's zone), GPS and camera, plus the orientation, used for the thumbnail.
  - A video's container: duration, creation time and location (ISO 6709, from `©xyz` or Apple's key).
- **Places:** set "Places (GeoNames file)" to a cities file on the memex's machine, such as `cities1000.txt` from GeoNames.
  - Nothing is downloaded for you.
  - The nearest city within 50 km becomes a `place:geonames:<id>` thing, linked with `at`.
  - Setting the file places existing photos too.
- **Events and trips:** photos and videos with gaps under 6 hours form a group.
  - A group of 10 or more is an event thing (`event:photos:<first item>`), linked with `part_of` (inferred).
  - It is a trip when it spans more than 20 hours and goes beyond 100 km of home, home being the most common place.
  - Events are rebuilt after scans.
- **Vision:** the vision task (Settings → Memex) sees the thumbnail, or up to 4 keyframes of a video. It returns a caption, which becomes the title, plus the scene, objects and any text in the picture.
  - All of these are searchable.
  - Objects become `object:<name>` things, linked with `shows` (inferred).
  - Newest first, within the shared daily budget.
- **Faces:** you supply two ONNX files on the memex's machine:
  - an UltraFace-style detector (input 1×3×H×W, outputs scores N×2 and boxes N×4);
  - an ArcFace- or MobileFaceNet-style embedder (1×3×112×112).
  - They run on the CPU with tract.
  - A face joins the person whose average face is closest (cosine ≥ 0.45). Otherwise it becomes a new "Unnamed person N".
  - After 3 photos, an unnamed person goes to the review queue with their face. Naming them merges them into the person of that name, if there is exactly one (for example, the person you email); otherwise a new person is made.
  - Photos link to people with `depicts` (inferred).
- **Videos:** with ffmpeg on the memex's machine (or `C6_FFMPEG`), a video gets up to 6 keyframes (the first frame and scene changes).
  - With a transcription task, its sound goes to an OpenAI-style `/audio/transcriptions` (up to 90 minutes), and the timestamped transcript joins its text.
- **Photos page:**
  - a grid by month;
  - search by description;
  - people (with faces), places and events as filters;
  - a photo view with face boxes, its things, a map link and keyframes;
  - images through `/api/v1/memex/media/{id}/thumb` and `/api/v1/memex/face/{person}`.

## 8. UI

- **Memex** in the menu:
  - Search across everything, grouped by kind, with filters for person, topic, source and time;
  - thing pages: a card, a timeline, related things, items, and the sessions about it;
  - a graph explorer, centred on a thing, with filters and depth;
  - trails: list, view, edit, and "start a trail from here";
  - the review queue;
  - sources and jobs: progress, backlog, errors and budgets.
- **Mail** in the menu: the client (section 6).
- Throughout carbon6:
  - @-mentions complete things;
  - thing chips on sessions link to their pages;
  - the inbox shows mail and commitments next to sessions.

## 9. Security and privacy

- **The fabric's mTLS and enrollment** apply to `c6m`, and its certificate role limits what it may ask of the head (2.4).
- **Scopes** limit what each session can see (2.3). By default, sessions see nothing from email or photos unless a scope rule allows it.
- **Model choice per source** decides where your data goes; local-only is one rule away.
- **Deletion** removes an item, its blobs, parts, vectors, faces and evidence, and any links left without evidence.
- **Untrusted content** (mail and web pages) is fenced off from agents' instructions (6.3), and its links aren't followed automatically.

## 10. Build order

Each step is usable on its own:

1. **Service and foundation:**
   - `c6m` with enrollment and the fabric relay;
   - the store, items, search (keyword and meaning), and the agent tools;
   - carbon6 → memex: digests, outputs, web pages, and memory migrated;
   - labels → things, and context briefs;
   - scopes;
   - the Memex Search page.
2. **Documents and papers:** extraction, passages, the citation graph, claims, notes and trails.
3. **Mail, calendar and contacts (Google):**
   - accounts and OAuth setup;
   - Gmail history import and sync, and the structure tier;
   - identity resolution and the review queue;
   - the mail client UI, sending with approval, rules;
   - Calendar sync and UI, invites and RSVP;
   - People sync and contacts built from mail;
   - triage, drafting, scheduling and meeting-prep workflows, memex event triggers, and inbox integration.
4. **Understanding tiers:** summaries, topics, commitments, the graph explorer and timelines. (Built: see 5.1.)
5. **Photos and videos:** EXIF and places, captions, objects, faces and naming, transcripts. (Built: see 7.1.)
6. **One ingest API (4.5).** (Built: see 4.5, "As built".)
   - `memex.ingest` and `memex.types`, the handler registry, and per-item results;
   - mail as the first handler: imported mail, threading by headers, upgrade to mirrored;
   - upload, folder scan, notes and the head's internal ingest moved onto it.

## 11. Decisions

| Question | Decision |
|---|---|
| Architecture | A separate service (`c6m`) with an API, reachable over the fabric from the head and workers, and able to call the head to start agents |
| Models | You pick a model per task |
| Mail | carbon6 replaces the mail client: Gmail and Google Workspace, any number of accounts |
| Calendar | Full calendar, synced both ways |
| Contacts | Synced both ways with Google Contacts, plus contacts built from mail and written back |
| Media | Faces, places, objects and scenes |
| Where `c6m` runs | This Mac first, with nothing assuming it |
| Exclusions | None: everything is brought in (Spam is kept out of agents' context) |
| Google Cloud project | You have one; adding an account asks for its OAuth client |
| Old mail | No Thunderbird importer. Archives can come in through `memex.ingest` from any tool, deduplicated by Message-ID |
| Getting data in | One ingest API typed by MIME type, with registered handlers that keep their own identity, schema and storage; connectors and actions stay separate (4.5) |
| Encryption at rest | Left to the disks |

## 12. Open questions

None blocking. To revisit when we reach them:

- Where your photo and video libraries live.

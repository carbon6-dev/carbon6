# Conversations: people, attachments, and sessions from any reply box — Design

Status: built 2026-10-28 (§5 is what was built, step by step).

Mail, Chat and the Brief show conversations. Three things a real client (and a good assistant) gives you that they don't yet:

1. **Who someone is**, from any name you see: their card.
2. **Attachments shown as they are**: pictures as pictures, files as files you can open.
3. **A session on it**: from any box where you write a reply, start an agent session that has the whole context (the conversation, the people, the request, the draft), with the session form's choices (worker, folder, model, effort, labels). The session can **replace the draft** in that box; you read it there, and send it or not.

Related: [relationships.md](relationships.md) (cards), [brief.md](brief.md) (requests and drafts), [chat.md](chat.md), [slack.md](slack.md).

## 1. People: a card from every name

- **Every person's name** in Mail (a message's sender, to and cc), Chat (a message's author, and @mentions) and the Brief (a request's person) can be clicked. It opens a small card:
  - their card and their organization's, when the memex has them (`memex.relations.cards` by address);
  - none yet but there's history: **Write a card** (`memex.relations.write`, one model call);
  - their address, and in chat their Slack title and status;
  - a link to the person in the memex.
- **Chat people** are matched by the address Slack gives (`chat_people.email`). Someone with no address (a bot, a guest who hides it) gets only the Slack details.

## 2. Attachments, as a client shows them

### 2.1 Mail

- **Pictures** (`image/*` except SVG) show as thumbnails under the message; a click opens the picture full size in a new tab. PDFs open in a new tab; other files download. Each shows its name, type and size.
- **Pictures inside the message** (`<img src="cid:…">`) show in place. The extract kit (1.1.0) now gives each attachment's Content-ID. A message whose HTML names `cid:` pictures asks for them in one call (`memex.mail.inline`), and the page puts them into the message as `data:` URLs. The sandboxed frame's rules don't change: it still loads nothing from the network until you allow remote images.
- An attachment shown inside the message isn't listed again below it.

### 2.2 Chat

- **Slack pictures** are downloaded by the sync (slack 1.4.0), as Slack's clients show them: the 720-pixel version (`thumb_720`, else the original up to 10 MB). Each run's file share (24 MB) and request budget apply as for documents.
- They're kept in the memex as **files**: a blob and a row (`kept_files`: source, external id, name, type, size), not items. So they don't show up as photos or documents and aren't searched. `GET /api/v1/memex/file?external_id=…` gives a file's bytes: such a file, or an item with that external id (Slack's documents are items already).
- **In a message:** a picture shows inline (up to 360 px tall); a click opens it full size. Other files show their kind, name and size, and open in Slack. Pictures not downloaded (messages synced before 1.4.0, or still waiting) show as a file to open in Slack.

## 3. A session from a reply box

### 3.1 Where

Every box where you write a reply: the Brief's draft, Mail's compose (a reply, or a new message), and Chat's composer (a conversation or a thread, on the Chat page or in the Brief).

### 3.2 Starting it

**Work on it in a session** (beside Send) opens the new-session form in a popup: your usual worker, folder, connection, model, effort, worktree and labels, all changeable, with the first message already written:

- the prompt `memex.compose.session` (Settings → Prompts, per realm), with `{{context}}` filled in;
- the context (`memex.compose.context`, built from the memex, no model):
  - the conversation: a mail thread's last 20 messages (from, to, date, subject, text), or a chat conversation's or thread's;
  - the request, if there is one: what's asked, by when, why it's here;
  - the people in it, with their cards;
  - the draft so far, and where it goes (the box's **target**: `thread:<account>:<thread>`, `chat:<workspace>:<channel>[:<thread ts>]`, `compose:<id>` for a new message, and the request's id).

You can edit the first message before starting. The session runs in the realm you're in, so its memex tools (mail_thread, chat_messages, requests, memex_search) read the same mail and chat.

### 3.3 The session replaces the draft

- Agents get a **draft** tool: "replace the reply drafted for the user in a conversation" (target, text, and the request when there is one). It never sends. The text goes where the box reads it:
  - a request's draft (`requests.draft`, marked as the session's);
  - else the box's draft slot (`drafts` table: target, text, the session, when).
- **The box follows it.** While the session is linked, the box shows "Session: ‹title›" (a link to it). Each time the session finishes a turn, the box reads its draft again, and when the session changed it, the new text replaces what's in the box, with a line saying so ("Rewritten by the session at 14:02"; Undo puts yours back).
- **You send it** from the box as always: Send in the Brief and Mail, Enter in Chat. Nothing the session does sends anything.
- A slot is kept until you send or discard from that box, and the link to the session with it, so a reload shows both.

### 3.4 Why not just the assistant

The in-head assistant answers questions about carbon6. This is a full agent session (tools, files, subagents, your chosen model) on one conversation, with a way back into the reply box.

## 4. Steps

1. **People cards:** the person chip and its card in Mail, Chat and the Brief.
2. **Mail attachments:** thumbnails and file chips; Content-IDs in the extract kit (1.1.0), `memex.mail.inline`, pictures inside messages.
3. **Chat attachments:** slack 1.4.0 downloads pictures; the `files` table and its ingest type; `GET /api/v1/memex/file`; pictures and files in messages.
4. **Context and drafts:** `memex.compose.context`, the `compose.session` prompt, the `drafts` table and `memex.drafts.*`, the agents' `draft` tool.
5. **The button:** Work on it in a session, in the Brief, Mail and Chat; the box following the session's draft.
6. **Docs:** chat.md and brief.md point here.

## 5. As built

### 5.1 People cards (step 1, 2026-10-28)

- `people.rs`: `Person`, a name that opens the card (Esc or a click outside closes it). The card shows `memex.relations.cards` by address. With none, it offers **Write a card** (`memex.relations.write`, `person:<address>`), and says when there's too little history for one. It links to a memex search for the address.
- **Where:** a mail message's sender, to and cc; a chat message's author, with their Slack title and status; @mentions (chat's `Known` now carries each person's address and title); a Brief request's person.

### 5.2 Mail attachments (step 2)

- **Pictures** show as thumbnails (up to 144 px tall, a click opens the full picture); other files as chips with an icon for their kind (`mail::FileChip`, shared with chat).
- **The extract kit 1.1.0** adds each attachment's `cid` and a tool, `inline`, that returns a message's image attachments with a Content-ID in one call. `MailAttachment.cid`; `memex.mail.inline` (`{item_id}` → `[{cid, mime, data}]`).
- **In the message:** only when its HTML names `cid:`, the page asks `memex.mail.inline` and replaces each `cid:<id>` with a `data:` URL before the sandboxed frame renders it. Attachments shown that way aren't listed below. Mail parsed before 1.1.0 has no `cid` on its attachments, so its inline pictures show in place and are also listed.

### 5.3 Chat attachments (step 3)

- **slack 1.4.0** queues pictures (PNG, JPEG, GIF, WebP) with documents: Slack's `thumb_720`, else `thumb_480` or `thumb_360`, else the original up to 10 MB. A thumbnail counts 256 KB toward the run's 24 MB share until it's downloaded. Its type is read from its first bytes (a thumbnail can differ from the original's).
- **The memex** keeps them through a new ingest type, `file` (`image/png`, `image/jpeg`, `image/gif`, `image/webp`; apps only, up to 10 MB). Each is a blob and a row in `kept_files` (migration 0022), not an item: never searched, never in Photos. `memex.file` (`{external_id}`) returns a kept file, else an item with that external id that has a blob.
- **`GET /api/v1/memex/file?external_id=…`** serves it like a mail attachment (nosniff, sandboxed, inline for pictures and PDFs).
- **In a message:** a picture shows from there, up to 360 px tall. If it isn't there (`<img>` fails), the file shows as a chip that opens in Slack.

### 5.4 Context and drafts (step 4)

- **A box's key:** `request:<id>`, `thread:<account>:<thread>`, `chat:<workspace>:<channel>[:<thread ts>]` or `compose:<id>`.
- **`memex.compose.context`** (`{for, draft, to}`) returns the first message, built from the `memex.compose.session` prompt (Settings → Prompts; `{{context}}`, `{{for}}`):
  - a request: what they need, then what its draft is written from (its conversations, their cards, other mail with them, what's open, when you're free, your voice);
  - a conversation: its last messages, and the cards of up to four people who wrote in it;
  - a new message: the cards of whoever it's to;
  - always, the draft so far.
- **`drafts`** (migration 0023): a box's text (not for a request, whose draft is the request's), its session, who wrote it last and when.
  - `memex.drafts.get` reads a box's draft. For a request, that's the request's text and target.
  - `memex.drafts.set` replaces it. For a request, it sets the request's draft text (making a draft in its latest conversation if there was none) with `by`: `session:<id>` when a session wrote it, else `you`.
  - `memex.drafts.link` records the session working on a box. `memex.drafts.clear` forgets the box.
  - `RequestDraft.by` says who wrote a request's draft when it wasn't the drafting model.
- **The agents' `draft` tool** (`for`, `text`) is given to sessions whose scope includes mail or chat. It calls `memex.drafts.set`, which sessions may call (`fabric::SESSION_METHODS`, with `memex.drafts.get`); the head stamps the session as its author.

### 5.5 The button (step 5)

- **`compose::WorkInSession`** is in the Brief's draft card, Mail's compose (a reply's box is its thread; a new message gets `compose:<time>`), and Chat's composer.
  - It asks `memex.compose.context` and opens `sessions::SessionDialog`: the new-session form with your remembered choices and the first message written. The form now takes `first_message` and reports the session it started (`on_session`).
  - The session is linked to the box (`memex.drafts.link`), and the box shows "Session: ‹title›".
- **Following:** when the linked session's status goes from running to anything else (a turn ended), the box reads its draft. If the session wrote one since the box last took one, it replaces the box's text with "Rewritten by the session at ‹time› · Undo". Opening the box reads it too, so a draft written while it was closed comes in.
- **Sending or discarding** from the box clears its draft (`memex.drafts.clear`). In the Brief, a request with no draft gets the session's on its first rewrite, with where it goes.

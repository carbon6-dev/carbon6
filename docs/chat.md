# Chat — a Slack-like client — Design

The Chat page today is a list of conversations, a message list and a thread pane. This redesign makes it work like Slack's own client: organized, with unreads and mentions, threads, activity, saved items, search, and a full composer. It stays a client of the memex: messages are `chat` items (docs/slack.md §4), and writes go through the slack app's operations.

## 1. What Slack's client does, and what we do

| Slack | Here | How |
|---|---|---|
| Workspace switcher | ✓ | A rail of workspaces, when more than one is connected |
| Sidebar: Starred, custom sections, Channels, Direct messages, Apps | ✓ | Sections are carbon6's own (Slack's API doesn't expose yours) |
| Unread (bold) and mention badges | ✓ | Read state from Slack (`last_read`) and from reading here |
| Mute, star, move to a section, hide | ✓ | Kept per realm |
| Show unreads only; sort A–Z or by recent | ✓ | |
| Quick switcher (⌘K) | ✓ | Conversations and people |
| Unreads view | ✓ | Every unread conversation's new messages, in one place |
| Threads view | ✓ | Threads you're in, with replies since yours or since you looked |
| Activity: mentions and reactions | ✓ | Messages that mention you; reactions to yours |
| Saved for later | ✓ | Kept per realm (Slack's saved items have no API) |
| Search with `in:` `from:` `has:` `before:` `after:` `is:thread` | ✓ | Full-text search over chat items |
| Messages grouped by sender, day dividers, "New" line | ✓ | |
| Avatars, display names, status | ✓ | From `users.list` |
| Formatting: bold, italic, strike, code, blocks, quotes, lists, links | ✓ | Rendered from Slack's markup |
| Emoji: `:name:` in text and reactions | ✓ | Common emoji built in; custom ones show as `:name:` (needs `emoji:read`) |
| Reactions: add, toggle yours, picker | ✓ | `my_reactions` on each message |
| Hover actions: react, reply in thread, copy link, mark unread, save, edit, delete, open in Slack | ✓ | |
| Edit and delete your messages | ✓ | New operations `update` and `delete` |
| Thread reply "Also send to channel" | ✓ | `reply_broadcast` |
| Composer: formatting bar, @mention, #channel and :emoji: completion, drafts, ↑ to edit your last message | ✓ | Drafts are kept in the browser |
| Load older messages on scroll; jump to a message | ✓ | `before`, `around` |
| Conversation details: topic, purpose, members, files, links | ✓ | Topic and purpose from Slack; people are who wrote there |
| Files: name, type, open | ✓ | Opens the memex document when it was read, else Slack |
| Keyboard: ⌘K, ⌥↑/↓, ⌥⇧↑/↓ (unreads), Esc (mark read) | ✓ | |
| Huddles, canvases, lists, workflows, file uploads, pins, bookmarks, presence | — | §8 |

## 2. Data

### 2.1 From the slack kit (slack 1.3.0)

A run's batch gains `chat`, the head passes it to the memex (`memex.chat.put`, as the app):

- **`conversations`**: `{workspace, channel, name, kind, topic, purpose, members, user, last_read, archived}`. Each conversation when what Slack says about it changed (the listing every run gives topic and purpose free; `last_read` comes from `conversations.info`, asked for a conversation when a run found new messages in it, and at its hourly re-read). `user` is a DM's other person.
- **`people`**: `{workspace, id, name, email, avatar, title, status, bot}`, everyone in the member list when it's read (daily), and each person from another organization when learned.

Messages (`chat` items) gain in `meta`:

- `my_reactions`: the reactions that are yours;
- `bot`: posted by an app;
- files gain `url`, their Slack address.

These fill in as messages are read again (the week's re-read, an import); old messages are not re-read for them.

New operations: `update` (`chat.update`, your message's new text) and `delete` (`chat.delete`); `post` takes `broadcast` (a thread reply also sent to the channel). An agent's edit or delete asks first, like its posts.

### 2.2 In the memex

Tables in the realm's memex:

- `chat_conversations`: what §2.1 says, plus `read_ts` (read up to here: Slack's `last_read`, or where you read here — whichever changed last).
- `chat_people`: the people.
- `chat_prefs`: per conversation: `starred`, `muted`, `section`, `hidden`. And the sections, in order, under one row (`channel = ''`).
- `chat_thread_reads`: a thread you've looked at, read up to a reply.
- `chat_saved`: messages saved for later, and when done.

Unread: messages (not thread replies, not yours) after `read_ts`. Mentions: unread messages that mention you, and every unread message of a DM or group. Muted conversations show neither unless they mention you.

A thread's new replies: replies after the later of your last reply and where you read it, not yours. Threads you're in: you wrote its first message or a reply, or it mentions you.

### 2.3 Methods

All `memex.chat.*`, needing `chat` in scope:

- `channels`: every conversation with its prefs, `unread`, `mentions`, `last_at`, and the person for a DM.
- `messages {workspace, channel, before?, around?, limit}`: `around` returns the messages either side of a ts.
- `thread`, as now, plus the thread's read state.
- `read {workspace, channel, ts}`: read up to here (and in Slack); `unread {…, ts}` marks unread from a message.
- `thread_read {workspace, channel, thread_ts, ts}`.
- `prefs {workspace, channel, starred?, muted?, section?, hidden?}`; `sections {names}`.
- `people {workspace?}`: for completion, avatars and names.
- `unreads`: unread conversations with their new messages.
- `threads`: threads you're in, newest activity first, with new-reply counts.
- `activity`: messages mentioning you, and your messages with reactions, newest first.
- `saved`, `save {id, done?, remove?}`.
- `search {q, in?, from?, has?, before?, after?, thread?, limit}`: the query's `in:#x`, `from:@y`, `has:link|file|reaction`, `before:`/`after:` dates and `is:thread` are parsed in the page and passed as fields.
- `post` (with `broadcast`), `react`, `edit {…, ts, text}`, `delete {…, ts}`.

## 3. The page

- **Rail** (more than one workspace): each workspace's initials, with its unread dot.
- **Sidebar**: a filter box (⌘K opens the switcher); Unreads, Threads, Activity, Saved, with counts; then Starred, your sections, Channels, Direct messages, Apps. Each section folds, and has a menu: sort A–Z or by recent, show unreads only. Each conversation: its icon (# for public, a lock for private, the avatar for a DM, a count for a group), bold when unread, a badge for mentions, dimmed when muted; a menu to star, mute, move to a section, hide, mark read, open in Slack.
- **Conversation**: a header with the name, topic, members and a details button; messages grouped by sender within five minutes, day dividers, a "New" line where you'd read to; older ones load as you scroll up. Opening it marks it read (in Slack too) after you've seen the newest; Esc does too.
- **A message**: avatar, name, time (the full date on hover), "(edited)", the text rendered, files, reactions (yours highlighted; a click toggles), "n replies · last reply" for a thread. On hover: quick reactions, the picker, reply in thread, save, copy link, mark unread from here, edit and delete (yours), open in Slack.
- **Thread pane** on the right; "Also send to #channel".
- **Details pane**: topic, purpose, people who write here, files and links shared, and the conversation's prefs.
- **Composer**: a formatting bar (bold, italic, strike, code, link, list, quote, code block), Enter to send (Shift+Enter a new line), completion for `@`, `#` and `:`, a draft per conversation, ↑ in an empty box edits your last message, the 5-second undo as now.
- **Views**: Unreads, Threads, Activity, Saved and Search results list messages with their place; a click opens the conversation at the message.

## 4. Tests

- The slack e2e test: conversations and people arrive (topic, `last_read`, avatar); unread and mention counts follow `last_read` and reading here; prefs; a thread's new replies; activity; saved; search with filters; edit, delete and a broadcast reply against the stand-in Slack.
- Kit unit tests for what's emitted only on change.
- The page: built, and checked in the browser against a scratch head with a stand-in workspace.

## 5. Steps

1. Data: the kit's `chat` batch, meta fields and operations (slack 1.3.0); the memex tables and methods; the vocabulary's `chat` fields. Tests.
2. The sidebar: rail, sections, unreads and mentions, prefs, quick switcher.
3. The conversation: rendering, grouping, the New line, hover actions, reactions, scrolling back, jumping.
4. The composer and the thread pane; edit and delete.
5. The views (Unreads, Threads, Activity, Saved, Search) and the details pane.

## 6. Decisions

- Sections, stars, mutes and saved items are carbon6's, not Slack's: Slack's API has none of them for user tokens. They're per realm.
- Read state is shared with Slack both ways for conversations (`conversations.mark` and `last_read`); thread read state is carbon6's only (Slack has no API for it).
- Old messages aren't re-read for the new fields (a full re-read costs hours of rate limit); the re-read week and imports fill them in.
- Drafts live in the browser (a per-viewer convenience).

## 7. As built

All five steps landed together (2026-10-27).

- **Data:** the slack kit (1.3.0) reports conversations and people in its batch's `chat`; the head hands it to `memex.chat.put`, which only apps' kits may call (the page's RPC refuses it). A conversation is also known from its first message (`chat::seen`), so ones the kit hasn't described yet still show. Migration 0021 fills `chat_conversations` from the messages already kept.
- **You** in a workspace: the person your messages (`from_me`) are from; `memex.chat.people` marks them `me`.
- **Read state:** `read_ts` follows Slack's `last_read` whenever that changes (read or marked unread in Slack), and your reading here (`mark_read`, `unread`, which also tell Slack). A conversation never read anywhere counts nothing unread.
- **The kit's record schema** (`slack_sync`) gained `info` and `last_read`: the collection refuses fields its schema doesn't name, so every new field there needs a line in `apps/slack.json`.
- **The page** (`crates/c6-ui/src/chat/`): `mod.rs` (state, rail, sidebar, switcher, keys, panes), `message.rs` (the list and a message), `text.rs` (Slack's markup and emoji), `composer.rs`, `views.rs`, `settings.rs`. ⌘K is taken before the sessions search (a capture listener) while Chat is open.
- **Details → People** lists who has written there lately (Slack's member list is another call per conversation; the count comes from Slack).
- **Emoji:** about 400 common ones built in; others show as `:name:`.
- The Brief's chat requests use the same conversation and thread components.

## 8. Not now

- File uploads (`files:write`), pins (`pins:read`), bookmarks (`bookmarks:read`), custom emoji (`emoji:read`): each needs a scope, and so a new manifest and connecting again.
- Presence (a call per person), huddles, canvases, lists, workflows, scheduled messages, reminders.
- Notifications outside the page.

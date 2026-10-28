# Slack in the memex — Design

Status: built (2026-10-24; §11 is what differs). Decided:
- carbon6 syncs your view of Slack, with a user token;
- it polls, as Gmail does;
- posting and replying are operations, held for your approval;
- shared files are downloaded and read into the memex.

## 1. What it does

Every Slack workspace you connect is mirrored into a realm's memex, as Gmail is (docs/knowledge.md §6):

- **Everything you can see:** channels you're in, private channels, DMs and group DMs, with their whole history, threads, reactions, and the files shared in them.
- **Kept current:** new messages within a couple of minutes (seconds while you're looking). Edits and deletions are caught too.
- **Searchable and linked:** messages are memex items. Slack users are people things, linked to the people your mail and contacts already know, by email. Channels are things too. Briefs, search and facts include Slack like everything else.
- **In carbon6:** a Chat page per realm shows channels, messages and threads. You can post, reply and react there.
- **For agents:** sessions whose memex scope includes chat can search and read it, as quoted, untrusted data, the way they read mail. They can post and reply, but each post waits for your approval unless a policy says otherwise.

## 2. Shape: a mirror app, like Gmail

Nothing in carbon6's core is about Slack. It's written as packages on the primitives Gmail uses (docs/knowledge.md §6.2, §13.3, §13.6):

| Package | What it is |
|---|---|
| `slack` (kit) | A Rust binary in its image that talks to Slack and keeps nothing. Network: `hosts` slack.com and files.slack.com only. |
| `slack` (app) | Owns the `chat` items, the channel and Slack-user things, and their claims. It holds the internal `slack_sync` collection, a schedule trigger that runs the kit with no agent, and kit operations. |

- **Per realm:** one app instance per realm, with that realm's workspaces as its accounts. A work workspace in its business's realm and a community in home stay apart unless you grant otherwise.
- **The source:** `crates/c6-slack`, part of carbon6, like `crates/c6-google`. Its packages are listed under Packages and installed, with your approval, only in the realms you choose.

## 3. Signing in: your own internal Slack app

- **Why your own app:** Slack limits apps distributed outside its Marketplace to one history request a minute (15 messages each). Internal apps, built by a workspace for itself, keep the normal limits. That makes a full sync practical. So, as with Google's OAuth client, you create the app once and carbon6 uses it.
- **Made easy:** Settings → Slack shows a ready **app manifest**, with the user scopes and this head's redirect address filled in. You paste it into "Create an app → From a manifest", then paste the app's client ID and secret back. They are kept as root's secrets `SLACK_CLIENT_ID` and `SLACK_CLIENT_SECRET`, and a realm may set its own.
- **Connecting a workspace:** you sign in through Slack's OAuth (v2, user scopes), and the workspace becomes an account of the realm's app.
- **User scopes:**
  - `channels:history`, `channels:read`, `groups:history`, `groups:read`, `im:history`, `im:read`, `mpim:history`, `mpim:read`;
  - `users:read`, `users:read.email`, `team:read`, `files:read`;
  - `chat:write`, `reactions:write`;
  - `channels:write`, `groups:write`, `im:write`, `mpim:write` (to mark read).
- **The address:** Slack needs an HTTPS redirect address. That's the head's HTTPS name, such as its Tailscale name, which your browser reaches. Slack itself never calls the head.
- **What the provider primitive needs** (general, not Slack's own):
  - a provider can say which query parameter carries its scopes (`user_scope`);
  - and where the token sits in the token response (`authed_user.access_token`);
  - and that a token may not expire, so there's no refresh token. A workspace with token rotation on returns one, which is used as usual.

  Kit runs get the token in `C6_ACCOUNTS` as today.

## 4. What lands in the memex

- **A new shared item kind, `chat`,** in root's vocabulary (docs/knowledge.md §13.5), for any chat service (Slack now; Teams, Discord or Matrix later). Its `meta`:
  - `service` and `workspace`;
  - `channel`, `channel_name` and `channel_kind` (public, private, dm, group);
  - `ts`, plus `thread_ts` for a reply;
  - `user`, `reply_count`, `reactions` (name to count), `edited` and `permalink`.

  The item's text is the message with mentions and channel links resolved to names. Its time is the message's.
- **Things:**
  - each Slack user is a person: linked by email to the person mail and contacts know, else a new one with the Slack handle;
  - each channel is a thing of kind `channel`.
- **Claims:** `sent_by` (message to person), `in_channel` (message to channel), `mentions` (message to person) and `member_of` (person to channel). The graph then answers "what did Ana say about the release in #eng".
- **Files:** each becomes its own item (`document`, `photo` and so on), downloaded through files.slack.com and read by the extract kit like a mail attachment, up to 40 MB each. The message links to its files (`attached`). Bigger files keep their name, type and link.
- **What it isn't:** Slack's search or AI summaries. The memex does its own: summaries, facts and the review queue run on chat items as on mail.

## 5. Syncing

The `slack_sync` collection holds, per account, the workspace's state:

- a record per channel: its newest message read (`latest_ts`), how far back the import has reached (`oldest_ts`), whether the import is done, and threads with unread replies;
- the user list's time, and `retry_after` after a rate limit.

The kit reads them as `state/slack_sync.json` and writes back what changed.

- **Import:** each channel is read backwards from now in pages (`conversations.history` with `latest`, 200 a page), with each thread's replies (`conversations.replies`), until its history is done.
  - A run stops at about 40 requests and writes `again`, so the trigger runs it again at once.
  - DMs and channels with recent activity go first, so what you're likely to want arrives early.
- **Keeping up:** every 2 minutes, and every 30 seconds while a Chat page is open:
  - `users.conversations` for new channels, and those you joined or left;
  - for each channel with activity since its `latest_ts`, `conversations.history` with `oldest`;
  - and the replies of threads whose `latest_reply` moved.
- **Edits and deletions:** polling doesn't see them, so the last 7 days of each active channel are read again every hour. Changed messages are updated. Ones gone from Slack are marked deleted: they stay in the memex marked so, unless the realm's setting says to drop them.
- **Rate limits:** a 429 stops the run and records `retry_after`. Runs wait until it has passed. What the run read before it is kept: a thread cut short waits in `pending`, and the page it was on is saved. Each run spends at most 40 requests, within Slack's tier-3 limit of about 50 a minute for history. While an import is under way, keeping up and the re-read take at most half of them, the conversations checked longest ago first, so the import moves on.
- **Users:** `users.list` once a day, and whenever a message names someone unknown.
- **Leaving:** a channel you leave stops syncing. What was synced stays (it's yours), unless you remove the account, which removes its items with it.

## 6. Writing back: operations

The app's kit operations, run as the app:

| Operation | Input | Does |
|---|---|---|
| `post` | channel, text, optional thread_ts | `chat.postMessage` as you; the new message comes back through the output, as Gmail's sent mail does |
| `react` | channel, ts, name | `reactions.add` (or `remove`) |
| `mark_read` | channel, ts | `conversations.mark` |

- **Approval:** `post` is held for your approval by a built-in policy (like `mail_send`), for agents. What you send from the Chat page goes at once, with a few seconds to undo, as mail does. A realm's policy can let an agent post to some channels without asking.
- **Untrusted input:** Slack messages are other people's words. Agents get them quoted, and sessions processing them run with the restricted tool set, as for mail (memex.md §6.6).

## 7. In the UI

- **Settings → Slack** (per realm):
  - the manifest to create the app, and its client ID and secret;
  - connected workspaces (connect, remove), and each one's import progress: channels done, messages so far, and any rate-limit wait;
  - the realm's options: keep or drop deleted messages, the file size limit.
- **Chat** (a page in a realm's menu once the app feeds it; now a full client, docs/chat.md): a new view kind, `conversations`, beside `messages`, `calendar` and `gallery`.
  - On the left: channels by workspace, with unread counts. In the middle: messages. A thread opens on the right.
  - A box to post or reply. Reactions; mark read.
  - It acts through the owning app's operations, like the mail client.
- **Search:** chat messages appear in memex search, and link to their place in the Chat page and to Slack (`permalink`).

## 8. Tests

They fail only when what they check is broken (docs/testing.md):

- **Against a stand-in Slack** (a local server speaking the Web API's shapes), with the kit's code in-process (`kitjobs::run_locally`):
  - the sign-in's address carries the user scopes, and the token is taken from `authed_user`;
  - an import across pages and runs (`again`), including thread replies;
  - keeping up: a new message, a new reply on an old thread, a new channel;
  - an edit and a deletion caught by the re-read;
  - a 429 with `Retry-After`: the run stops, and the next run waits;
  - a file read into an item and linked;
  - people linked by email to an existing contact;
  - an agent's `post` held for approval, then sent; and a post from the Chat page with undo;
  - realm isolation: a workspace in one realm is invisible in another.
- **The image builds,** and reaches slack.com over TLS through the egress allow-list (like Google's).

## 9. Steps

1. **Vocabulary and views:** the `chat` kind (schema, card), the `channel` thing kind, `chat` as a memex scope kind, and the `conversations` view kind in the UI.
2. **The provider primitive:** the scope parameter, the token's place in the response, and tokens that don't expire.
3. **The kit** (`crates/c6-slack`): users, channels, import, keeping up, threads, the re-read window, files and rate limits; with the stand-in Slack and tests.
4. **The app package:** provider, feeds `chat`, `slack_sync`, the trigger, operations, and the built-in approval policy for `post`.
5. **The UI:** Settings → Slack (manifest, client, accounts, progress) and the Chat page.
6. **Docs:** README, and docs/knowledge.md's list of mirror apps.

## 10. Not now

- **Slack's Events API** (real time): needs the head at a public address. Polling covers it; events can be added later, with polling as the backstop.
- **Enterprise Grid org-wide installs,** and channels you aren't in.
- **Huddles, canvases and lists.**
- **Slack Connect channels:** they sync like any channel you're in, but their other organisation's people stay Slack users only, unless their email matches someone you know.

## 11. As built

- **Packages:** the kit is `slack-api` (the app is `slack`, and two packages can't share a name). Both are in `crates/c6-slack`, listed under Packages at start, and installed with your approval. A unit test ties their versions to a digest of their source (`kit.digest`).
- **Sign-in:** the provider primitive gained `scope_param`, `scope_separator`, `token_path`, `name_field`, and `account_field`/`name_field` as paths into the token's answer when there's no userinfo address. Tokens with no expiry and no refresh token are kept and never renewed. Slack's sign-in comes back to the head's `/auth/oauth/callback`, then to the realm's Settings → Slack.
- **The client:** Settings → Slack writes the realm's own `SLACK_CLIENT_ID` and `SLACK_CLIENT_SECRET`; root's apply where a realm has none.
- **Pace:** each run spends at most 40 requests per workspace.
  - Conversations with a message in the last day, or threads waiting, are checked every run. The rest are checked in turn, the one checked longest ago first, at most every 5 minutes, leaving a quarter of the budget for the re-read. (Until 1.3.1 the rest went by their last message, so the same few took every turn and a new message in any other quiet conversation never arrived.)
  - The last week of active ones is read again every hour.
  - The import uses what's left, a page at a time (DMs and groups first), and asks to run again at once while it lasts.
  - A thread's replies are read when it's new, and when its latest reply moves, every page of them. Threads the budget doesn't reach wait in the conversation's `pending` and are read first next time, and runs go on at once while any wait.
- **People from other organizations** (Slack Connect channels and DMs) aren't in `users.list`: each is looked up with `users.info` before their first message is kept. Slack limits that method on its own, so it isn't counted in the budget.
- **Upgrades:** each conversation's record says which version of the sync made it (`rev`). One made by an older version is read again whole, so its messages take the newer names and threads; its files aren't fetched again. The hourly re-read fetches files only for messages it hasn't seen.
- **Deletions remove the item,** as a mirror should (rather than keep it marked). Only a re-read of the whole week can delete: one cut short by the budget doesn't guess.
- **Files:** documents Slack calls PDF, Word, Markdown, text, HTML or reStructuredText (up to 40 MB) are downloaded, up to 24 MB a run (the rest wait in the workspace's queue, each once; a bigger file takes a run of its own; until 1.3.2 one bigger than 24 MB held up the whole queue), and read by the extract kit. Other files keep their name, type and size in the message's `meta.files`.
- **The memex:** `memex.chat.channels`, `memex.chat.messages`, `memex.chat.thread` (an index on `meta`'s workspace and channel), and `memex.chat.post`, `react` and `mark_read` through the app's operations. `memex.feeds` says `chat` once a workspace is connected.
- **Agents:** the tools `chat_messages`, `chat_thread` and `chat_post` in sessions whose memex scope includes `chat`. The built-in approval policy for sending mail covers `chat_post` too.
- **The UI:** Settings → Slack (the app, the manifest with a Copy button, the client, workspaces with their import progress, Sync now), and Chat (☰ → Chat once a workspace is connected). Chat shows conversations by workspace, then messages, with a thread on the right. Posting takes back for 5 seconds (Undo). It has quick reactions and Open in Slack, marks conversations read in Slack as you open them, and syncs every 30 seconds while open.
- **Not done:** members of a channel (`member_of` claims), and unread counts per conversation.

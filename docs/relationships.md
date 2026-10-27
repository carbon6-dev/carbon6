# Relationships: mail read with its history

Carbon6 should handle each new email the way someone who has followed all your mail since the beginning would. It should know:
- who the sender is to you;
- what has passed between you, including what isn't quoted in this thread;
- how you usually respond to them.

It should also stay quick: new mail gets sorted and drafted within seconds. Meanwhile the picture of all your mail fills in behind the scenes, newest first, until it reaches your oldest mail.

Related: [brief.md](brief.md) (sorting and drafts), [memex.md](memex.md) (reading, the graph), [activity.md](activity.md) (seeing it run).

## 1. Today, and what's missing

- **Sorting** (triage.rs) sees a thread's last six messages. The only history it gets is a yes/no on whether you know the sender. It doesn't know:
  - how many times you've written to each other;
  - whether you ever answered;
  - what earlier threads were about.

  Mailing-list headers settle a thread as a newsletter before anything else is checked. Sales tools send with those headers, so cold pitches end up filed as newsletters. A sender counts as known if you wrote to them once, or to anyone at their company, and the model may not call a known sender's mail a pitch.
- **Reading** (understand.rs) works through threads newest first, one at a time. It saves a summary, topics, commitments and people for each. No step brings a thread's people's other threads into it, and nothing sums up a relationship.

## 2. The design: three layers

### 2.1 The participant index (no model)

The table `correspondents` has one row per address you've exchanged mail with. Each row holds:
- domain and name;
- messages they sent you, and messages you sent them (to or cc);
- threads they started or wrote in, and how many of those you answered;
- first and last message;
- how many of their messages were bulk-sent: mailing-list headers, or a sending subdomain (`e.`, `em.`, `mail.`, `news.`, `updates.`, `info.`, `marketing.`, `go.`, `comms.` and so on);
- whether the address looks automated (`noreply`, `notifications`, and the like).

A domain's numbers are the sum of its addresses. Shared domains (gmail.com and the like) are never summed. The whole table is rebuilt from the mail table in one pass, at most once a minute and only when mail has changed. That takes about a second for tens of thousands of messages.

### 2.2 Thread insight (exists)

This is the summary, topics and commitments reading writes for each thread. It's kept per thread, and redone only when the thread has news or the model changes. Anything below that needs a thread's insight reuses what's there.

### 2.3 Relationship cards (model)

A card is kept for each person (`person:<address>`) and each organization (`org:<domain>`, never a shared domain). It holds:
- who they are to you, as one of: customer, prospect, partner, investor, vendor, colleague, candidate, recruiter, cold outreach, service, personal or other;
- their role, as far as the mail shows;
- the story so far, in a few lines;
- what's open between you;
- how you deal with them (you answer within hours, you ignore them, and so on);
- the range of dates and the threads it covers.

**Built from** the index's numbers and the summaries of their threads, newest first, at most twelve.

**Kept up to date:** when reading finishes a thread, every person and organization in it is marked for an update. An update gives the model the old card plus the new threads' summaries, so a card grows over time without rereading everything.

## 3. A new email

1. **The index's numbers** for the sender and their domain go into every sorting prompt, for example: "You wrote to bo@acme.com 0 times and to acme.com 14 times; Bo wrote 3 messages in 2 threads since 2026-03-02, you answered 0; all 3 were bulk-sent."
2. **Their card goes in too** (person and organization), when there is one. When there isn't one but there is history (you wrote to them, or there's more than one thread), the card is built on the spot, with a limit:
   - their five most recent threads that haven't been read are read first, and those summaries are saved;
   - then the card is written and saved;
   - then the email is sorted.

   A first email from someone with a long history costs a few calls once. After that it costs one call.
3. **What rules still settle:**
   - your own last word, as `yours`;
   - your marks: pitch, or known;
   - your corrections.

   Everything else goes to the model. The old rules' verdicts go into the prompt as hints: "has mailing-list headers", "sent from an automated address". "Pitch" is allowed for anyone, including senders you know; the history and the card show whether this is a vendor you work with or one you keep ignoring.
4. **Drafts** get the same cards, so a reply knows who it's to.

## 4. Behind the scenes

The reading loop's order is:
1. sorting new mail, with any cards it needs on the spot;
2. drafts;
3. card updates for people marked for an update, the most recently active first;
4. reading, which walks back through time newest first.

Automated senders' threads (no-reply addresses, notifications, receipts) are skipped by reading; they're sorted, but not summarized. Progress shows on the Activity page: threads read, threads left, cards, and the oldest month reached.

Both the walk and the cards keep their place, so a restart continues where it left off.

## 5. Calls at once

Each realm's loop makes one model call at a time by default. A local server answers one at a time unless it's told otherwise (`OLLAMA_NUM_PARALLEL`). **Calls at once** (Settings → Memex, 1 to 8) lets the loop run that many steps side by side, and sorting new mail always keeps one of them for itself.

## 6. Cost, on a local model at about 5 s a call

| What | Calls | Time |
|---|---|---|
| The index | none | about a second |
| A new email | 1 (a few the first time for someone with history) | seconds |
| The last 90 days (about 2,000 threads) | about 2,000 plus cards | about 3 hours |
| All mail (about 33,000 threads) | fewer, with automated mail skipped | about 2 days nonstop |

The daily call budget (Settings → Memex) still applies. With a local model, set it high.

## 7. Steps

1. **The index and history in sorting.**
   - The participant index (2.1).
   - Its numbers in every mail sorting prompt (3.1).
   - Rules keep only your last word, marks and corrections. Everything else goes to the model, with the rules' verdicts as hints and "pitch" allowed for anyone.
   - Once, everything in the last 14 days is sorted again, except what you sorted yourself.
2. **Cards.**
   - The `cards` table and the card prompt (`memex.relations.card`).
   - Cards built on the spot when sorting needs them (3.2).
   - Cards in sorting and draft prompts.
   - Cards on a person's or organization's page in the memex.
3. **The walk.**
   - Reading skips automated senders.
   - Finished threads mark their people's cards for an update.
   - Card updates run in the loop (section 4).
   - Progress shows on the Activity page.
4. **Calls at once:** the setting, and a loop that runs that many steps side by side.

## 8. As built

### 8.1 Step 1: the index and history in sorting (2026-10-27)

- **The participant index** (`relations.rs`, table `correspondents`). It is rebuilt from the mail table when mail has changed, at most once a minute, before sorting runs. On a mailbox of 54,000 messages a rebuild takes about 0.2 s and finds about 6,000 correspondents. Steps 2 and 3 read it.
- **Sorting counts each sender's history live** from the mail table, for the address and for its domain, in about 25 ms. The index can be a minute behind, and a conversation must be sorted with exact numbers. The counts:
  - how many times the owner wrote to them, and to their domain;
  - their messages and threads, and the range of dates;
  - how many of their threads the owner answered;
  - how many of their messages were bulk-sent.

  Notes are added for a shared mail provider, a bulk-sending subdomain and an automated address. All of it goes first in the mail sorting prompts, under "History with the sender".
- **What rules still settle:** your last word (`yours`), and your pitch mark. Everything else goes to the model, with "What the rules noticed" as hints:
  - mailing-list headers;
  - a sales-sequence tool's mark in the links or text;
  - a sequence's follow-up phrase.

  The sorting prompt for known senders now allows `pitch` (except for senders you marked known). The pitch prompt treats bulk-sent selling as a pitch and a publication as a newsletter, and both prompts allow `noise`.
- **With no model at all,** the old rules still sort what they can: automated mail is a notification, mailing lists are newsletters, sequence marks and follow-ups are pitches. Once a model is chosen, everything the rules sorted is sorted again by the model.
- **Migration 0017** sorts the last 14 days again, except what you sorted or marked yourself.

### 8.2 Step 2: cards (2026-10-27)

- **The `cards` table** (`cards.rs`) is keyed `person:<address>` or `org:<domain>`. The card prompt is `memex.relations.card` (Settings → Prompts). A card records relationship, role, story, open and how, plus:
  - the dates and number of the threads it has read;
  - the model that wrote it;
  - a `dirty` flag, which step 3 sets.
- **Writing:** a card reads their twelve newest threads, or when growing, only those since the card's newest. Each thread is listed by date and subject with its summary. When the card is written on the spot, up to five unread threads are read first, through the reading walk's own reader, so their summaries are saved for everything else too. The model gets those threads, the history numbers, and the card so far when there is one.
- **In sorting:** before a mail is sorted, the sender's card and their organization's card are used.
  - A missing card is written on the spot when there's history: you wrote to them, or there's more than one thread with them. An organization card needs you to have written to someone there. Neither is written for automated addresses or shared providers.
  - A card is only written on the spot when today's budget has room for it and the sorting.
  - The cards go into the prompt under "Who they are to the owner".
- **In drafts:** the request's person's and organization's cards, as written (none is written for a draft).
- **Ops:** `memex.relations.cards` returns a thing's or an address's cards. `memex.relations.write` writes or grows a card now.
- **UI:** cards show on a person's or organization's page in the memex, and next to a request in the Brief.

### 8.3 Step 3: the walk (2026-10-27)

- **Reading skips automated senders:** mail from `noreply`, `no-reply`, `donotreply`, `notification…` or `mailer-daemon` addresses, as well as lists and spam, as before. Those threads are still sorted.
- **News for cards:** each thread read successfully is noted, in table `card_news`, for its people:
  - senders, and the people you wrote to;
  - only those with history, counted live: you wrote to them, or more than one thread;
  - and their organizations, when you've written to someone there.

  A card written on the spot takes in its threads' news as it's written.
- **The card step** runs in the reading loop after drafts and before reading. It takes the person or organization with the most recent news, folds in up to twelve news threads (with the card so far), and clears them. The walk reads newest first, so cards grow back through time. A failed fold clears its news, so it isn't retried in a loop.
- **Progress:** `memex.relations.progress` reports threads read of all readable threads, the oldest date read back to, cards, cards waiting to take in news, and correspondents. The Activity page shows it for each realm. On a mailbox of 54,000 messages there are about 18,000 readable threads; the query takes about 40 ms.

### 8.4 Step 4: calls at once (2026-10-27)

- **The setting** is Settings → Memex → **Calls at once** (`calls_at_once`, 1 to 8, default 1). It lives with the memex's other model settings rather than on the connection: the memex is what runs calls side by side, and connections have no edit path.
- **Lanes:** the reading loop runs as eight lanes, of which the first *calls at once* are active.
  - With one lane, it does everything in order: sorting, drafts, cards, reading, pages.
  - With more, lane 0 only sorts and drafts, so new mail never waits behind the walk. The other lanes do everything.
- **Claims:** steps side by side never take the same work. Each step claims what it works on (`sort:`, `draft:`, `card:`, `read:`, and `pages`) in memory, and gives it back when done. Reading picks the newest unclaimed thread. A card being written by one step is used as it is by another step's sorting. A thread being read by one step isn't read again on the spot by another.
- **Budget:** each lane checks the daily budget before its call, so lanes side by side may overshoot it by up to *calls at once* − 1.

### 8.5 Sorting keeps first claim on model calls (2026-10-27)

- **The bug:** when conversations wait to be sorted and sorting can't make a call (no model, or no budget left as it looked), the rest of the reading loop's pass used to go on to drafts, cards and reading. A budget raised in between went to them instead of to the waiting mail.
- **The fix:** `triage::step` now says Worked, Idle, or **Waiting**. When it says Waiting, nothing else in the pass calls the model, so the next pass sorts first.

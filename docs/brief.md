# The brief: mail and chat as requests — Design

Status: built (2026-10-26; §10 is what was built, step by step). Decided:
- the home of mail and chat is **the brief**, not a list of messages;
- an agent **drafts a reply for every request that needs you**, before you open it;
- **nothing goes out without your keypress**, at first; later you may approve playbooks that act alone;
- you're **interrupted only for what's urgent** by your rules; everything else waits for the next brief;
- **cold pitches** (vendor outreach Gmail lets through) are caught first, cost almost nothing, get no drafts and never "need you".

## 1. The idea

You don't want to read 200 messages; you want to make the 6 decisions in them. carbon6 already holds your mail, Slack, calendar, contacts and files in one memex, with the same people across them. So the unit you work through is a **request**: what someone needs from you, from whichever channel (or several) it came in on. Agents do the work around each request — sorting, summarizing, finding context, drafting — and you decide, one key at a time.

## 2. What you see

### 2.1 The brief (Mail & chat's home)

```
Since yesterday 6 pm · 214 new · 9 need you · ~25 min

NEEDS YOU (9)                                             draft
 ● Ana (Acme)      Approve contract redlines · by Thu   ✉+💬  ✓
 ● Raj             Move the 1:1? proposes Wed 3 pm       💬    ✓
 …
FIRST CONTACT (2)  · someone new, not a pitch
 ○ Lee (Northwind)  Asks about a pilot of the scheduler  ✉
YOU OWE (2)
 ○ "I'll send the deck Friday" — to Mark, Mon             due tomorrow
WAITING ON OTHERS (3)
 ◌ Beta Corp: the NDA back — asked 5 days ago             nudge drafted
FYI (38) — five lines
COLD PITCHES 23 · NEWSLETTERS 90 · NOTIFICATIONS 60  (archived)
```

- Each line says in a few words why it's there; people are the memex's people.
- The message list (today's Mail and Chat pages) stays, one key away, for search and browsing.
- A brief per realm: mach5's mail and Slack together; home's apart. A combined view across realms can come later.

### 2.2 Working a request

Enter opens it: the thread(s) on the left; on the right what's asked, the context the agent found (earlier threads, Slack, calendar, files, commitments with this person) and the **draft**, with the parts only you can decide left as choices.

| Key | Does |
|---|---|
| ⌘↵ | Send the draft (5 s to undo), close the request, open the next |
| R | Edit the draft |
| E | Done without replying (archive) |
| H | Remind me: a time, or "if no reply by…" |
| L | Relabel (needs you / first contact / FYI / pitch / noise), with an optional note |
| S / N | Pitch / not a pitch (§4.5) |
| J / K | Next / previous · ⌘K everything else |

A request from several channels is answered where its substance is (a contract by mail), with an optional one-line note on the other ("replied by email").

### 2.3 During the day

- **Interruptions:** only a request from someone in your "important" rule, or with a deadline today, notifies you (the notification channels you set). The rest wait.
- **Briefs** at times you choose (default: 8:30 and 13:30), plus whenever you open the page.
- **Close of day:** what you handled and what's still yours.

## 3. The pipeline

Per realm, for every new mail message and chat message (what syncs brings, and what you send):

1. **Pitch filter** (§4) — cheap, mostly no model.
2. **Triage** — one model call per thread that passed: a category and a one-line reason.
3. **Requests** — for "needs you" and "first contact": what's asked, by when; joined to an open request from the same person if it's the same ask (§5).
4. **Context and draft** — for "needs you": the agent gathers context from the memex and writes the draft (§6).
5. **Commitments** — "I'll send it Friday" (yours) and "they'll get back to me" (theirs), as today's commitments, now from chat too, with due dates that bring them back.

It runs in the memex's understanding loop (understand.rs), like summaries: direct model calls, no session per message, within the realm's daily budget, newest first. Steps 1–3 run as mail arrives; drafting runs for open requests in order of urgency, so the drafts you'll see first are ready first.

**Categories:** needs you · first contact · FYI · newsletter · notification · cold pitch · noise. Only the first two become requests.

## 4. Cold pitches

Sequence tools (Apollo, Outreach, Salesloft, Instantly, Lemlist, HubSpot sequences) send one at a time from real, authenticated mailboxes with a personal first line, so spam filters pass them. What gives them away is your **relationship** with the sender and the **pattern**, not the words.

1. **Relationship (decides most, free):** you've written to this person or anyone at their domain; they're in contacts, calendar or Slack; the thread has a message from you. Anyone with history is **never** a pitch.
2. **Fingerprints (free):** sequence tools' headers and tracking links, tracking pixels, a lookalike or very new sending domain (getacme.io, tryacme.com), a "Re:" on a thread you never wrote in, a scheduling link from a stranger.
3. **Sequences:** "bumping this", "circling back", "did you see my last note?" in a thread you never answered. One caught step marks the sender, so the rest of the sequence is caught too.
4. **The model,** only for first contacts the above can't settle: pitch or first contact, with a reason.
5. **Your corrections:** S marks the sender (optionally the domain, or the tool); N marks them known. These are rules you can see and edit (§7).

**Safety net:** a first contact that names a customer, deal, project or person the memex knows, or answers something you sent, is never a pitch: it's **first contact**, shown with a summary, drafted only once you engage. Pitches are archived in carbon6, counted in the brief, searchable, and listed by company in a weekly glance.

## 5. Requests

A new memex table, per realm:

| Field | |
|---|---|
| id, person (thing), title, asked (what they need), due | |
| category | needs you / first contact |
| sources | the items it came from: mail threads, chat messages or threads |
| status | open · done · snoozed (until, or "if no reply") · dropped |
| draft | the draft reply (text, channel, the choices left open), and the context it used |
| reason | why it's here, one line |

- **Same ask, two channels:** a new message from a person with an open request is compared with it (the model, with both texts); a match joins it as another source. Unsure: two requests marked "possibly the same" (one key merges). Never silently hidden.
- **Closing:** you send, mark done, or the thread shows it handled (they withdrew; the meeting got booked) — the triage step notices on the next message.
- Requests are memex data like commitments: agents in a session whose memex scope allows can read them (`requests` tool), and the brief is built from them.

## 6. Drafts

- Written with: the thread(s); the person's recent mail and chat with you; open commitments both ways; your calendar (for scheduling, a real free slot); files the thread names, found in the memex; and your **voice**: a few of your recent replies to this person (else to anyone), so it sounds like you.
- Where only you can decide, the draft offers choices ("approve 3 and 7; on 5: keep 2x / accept 1x") rather than guessing.
- Kept with the request, not in Gmail's Drafts, until you edit or send; sending goes through the mail or Slack app's operation, as from the Mail page (undo window). It doesn't go through the agents' approval policy, because your keypress is the approval.
- Re-drafted when the thread moves on.
- Cost: one larger model call per "needs you" request. The brief shows the day's spend; the realm's budget caps it; past the cap, drafting waits until you open the request.

## 7. Your rules, as settings

Nothing here is a fixed string in code (it's carbon6's rule): each is an editable setting, per realm, falling back to root's.

| Setting | What |
|---|---|
| `memex.triage` prompt | How to sort: what "needs you" means for you, with your notes from L appended as examples |
| `memex.triage.pitch` prompt | The model's part of the pitch filter |
| `memex.triage.signals` | The sequence tools' marks and follow-up phrases |
| `brief.request` prompt | What a request is, and when two are the same |
| `brief.draft` prompt | How to draft: tone, length, sign-off |
| `brief.important` | Who and what may interrupt you |
| `brief.times` | When briefs come |
| sender marks | Your marks: pitch / known, by address or domain (kept in the realm's memex) |

The memex reads prompts from home only today (prompts::for_memex); it will read the realm's own, so mach5 can sort and draft differently from home.

## 8. Building on what's there

| Exists | Use |
|---|---|
| understand.rs loop, budget, prompts | triage, requests, drafts run in it |
| commitments table | "you owe" and "waiting on" (now from chat too, with due dates) |
| needs_reply / waiting views (rules) | needs_reply follows triage's category once a thread is sorted (needs you, a first contact), the rules until then (and no automated senders) |
| Inbox page's "Needs reply" | shows the brief's top requests instead |
| mail and chat operations (send, post, undo) | sending a draft |
| notifier | urgent requests, briefs |
| `mail.received` trigger event | joined by `chat.received`, `request.opened`, `commitment.due`, so you can automate on them |

## 9. Steps

1. **Pitch filter and triage:** relationship and fingerprint checks, the model's part, categories on threads and chat messages, the S/N/L marks and `brief.senders`; the memex reading each realm's prompts. Tests with a mock model and sequence-tool messages.
2. **Requests:** the table, extraction, joining across channels, closing; the `requests` tool for agents.
3. **The brief page:** the sections, the request view, the keys; Mail and Chat stay as lists.
4. **Drafts:** context gathering, voice, choices; sending from the request with undo.
5. **Timing:** briefs at set times, urgent notifications, close of day; `chat.received`, `request.opened`, `commitment.due` events.
6. **Docs:** README, memex.md's §6.6 pointing here.

## 10. As built

### 10.1 Pitch filter and triage (step 1)

- **What's sorted:** mail threads in the inbox, and chat conversations that may be yours (DMs and group DMs, threads you're in, messages mentioning you), with a message in the last 14 days. Each is sorted again when it has news. A conversation is `thread:<account>:<thread>` or `chat:<workspace>:<channel>[:<thread ts>]`, kept in the realm memex's `triage` table (migration 0012).
- **In the memex's reading loop,** before understanding, newest first, sharing its model and daily budget. What rules settle is sorted even with no model or no budget left. The rest waits.
- **A model that fails for now** (unreachable, timed out, 429 or 5xx, or a pool with no member free: docs/pools.md) sorts nothing. The conversation waits, the call goes back to the day's budget, and the connection rests (10 seconds, twice as long after each failure in a row, up to 5 minutes) before anything calls it again. Drafts, cards, the same-request check, reading, pages and vision wait the same way. Other failures (another 4xx, an answer with no JSON) are kept as the answer, until the conversation has news.
- **Time limits:** each model call gets 10 minutes in all (connecting, waiting for the first token and the whole answer) and may pause at most 2 minutes between streamed events. Slow servers fit: a CPU server loading a model takes about 80 s to its first token, a long reading call on one about 60 s, and qwen3.6:35b on a laptop 2 to 6 minutes. A call past either failed for now and waits as above, so a server that stalls mid-answer holds no lane. The Activity page shows it failed, with why.
- **A new model at once:** changing a model in Settings → Memex stops the calls still running on the old one. Their work goes to the new one without waiting, and nothing rests.
- **Room to think:** unless a task's effort is none, a model may think before it answers, and its thinking counts toward the output limit. Each answer's limit gets 8,192 more tokens for it (qwen3.6:35b thought 700 to 3,900 tokens on these prompts).
- **Rules, no model:**
  - you wrote last: `yours`;
  - a list or bulk sender: `newsletter`, or `notification` for automated addresses;
  - a sender you marked as a pitch: `pitch`;
  - a stranger whose mail carries a sequence tool's mark (in its text or HTML), or whose latest is a sequence's follow-up: `pitch`.
- **Known** means you've written to them, or to anyone at their company's domain (not a shared one like gmail.com). It also covers your contacts, people at your meetings, people at your own domain, anyone in a thread you've written in, and senders you marked known. Known people get the `memex.triage` prompt; strangers get `memex.triage.pitch`.
- **Chat:** people in your workspace count as known. Slack messages now say `from_me` and `mentions_me` (slack 1.2.0, from `auth.test`), and older ones are read again for it.
- **Settings:** the prompts are `memex.triage`, `memex.triage.pitch` and `memex.triage.signals` (the marks and `phrase:` follow-ups, one per line). They're editable per realm: the memex now reads each realm's own `memex.*` prompts, where before it read home's for all.
- **Your marks and corrections:** "Cold pitch" and "Not a pitch" mark the sender (by address, or `domain: true` for the whole domain) in the realm's memex. They're listed and removable in Settings → Mail & calendar. "Sort as…" with a note becomes one of the model's examples (the last 20).
- **In Mail:** the views Needs you, First contact and Cold pitches; each thread's label with its reason; the sorting bar on an open thread.
  - Needs you and First contact are what waits on you in the Brief: threads in the inbox whose request is open (not done, dropped, or snoozed until later; a snoozed one comes back when its time comes). Needs reply leaves out a sorted thread whose request isn't open, too.
  - A thread with an open request offers the Brief's actions in its sorting bar: Done, Tomorrow and Drop (the same `memex.requests.set` calls), and the way to the Brief.
- **API:** a conversation's `Triage.request` (from `memex.triage.get` and `MailThread.triage`) is its open request's id.
- **API:** `memex.triage.list`, `get`, `counts`, `set`, `mark`, `marks` and `unmark`; `MailThread.triage`.
- **Not yet:** the pitch filter for Slack Connect strangers (chat counts everyone as known); the brief page and its keys (step 3).

### 10.2 Requests (step 2)

- **When triage sorts a conversation as needs you or first contact,** the model also says what's asked, in one line, and by when. Its request is:
  - the open one already holding that conversation (updated);
  - else, when the same person has one open, the one the model says is the same ask (`memex.requests.same`: same and sure). Same but unsure makes a new request marked as possibly the same, merged with one call;
  - else a new one.
- **The same person** across mail and chat: Slack messages carry the sender's address (`user_email`), so a request is joined by address.
- **Closing:** a conversation sorted again as `yours` (you answered) closes its requests as done (`replied`). Sorted as FYI, newsletter and the like means `settled`. Marking a sender as a pitch drops theirs. You can set a request done, dropped, snoozed until a time, or open again.
- **Sorting a conversation yourself** (Sort as… needs you) opens a request with no model call.
- **Kept** in the realm memex's `requests`, `request_sources` and `request_maybe` (migration 0013).
- **API:** `memex.requests.list` (open by default, the soonest due first), `get`, `set` and `merge`. Agents get the `requests` tool in sessions whose memex scope includes mail or chat.

### 10.3 The brief page (step 3)

- **Brief** (☰ and Apps, once the realm has mail or chat; `/r/<realm>/brief`):
  - Needs you, then First contact: open requests, soonest due first, with who, what, by when and where from (✉ 💬). A request possibly the same as another shows "same? merge", which keeps the older one.
  - You owe, and Waiting on others: open commitments. A click unfolds one: the mail thread it was read from, Done and Drop.
  - FYI: the latest conversations sorted FYI that you haven't seen. A click unfolds one: the conversation, Seen (gone until something new comes in it), Needs me (sorted as needs you, so a request), Not for me (noise). The last two teach the sorting.
  - How many conversations were read in the last 14 days (mail threads, chats, how many by the model), and how many were set aside (cold pitches, newsletters, notifications).
  - It reloads every 30 seconds, unless a request or a row is open.
- **A request opened:** its conversations on the left (the mail thread with reply, or the chat thread or DM with its box), and on the right what's asked, from whom, by when and why: Done, Tomorrow, Next week, Drop.
- **Keys:** J/K, Enter, E (done), H (tomorrow at 9), # (drop), Esc. Settling an open request opens the next.
- **On a phone,** a request's title wraps under its person.
- **Not yet:** drafts (step 4). The FYI five-line summary, the scheduled briefs and urgent notifications (step 5). Relabeling from the brief (it's on the mail thread's sorting bar).

### 10.4 Drafts (step 4)

- **Drafted in the memex's reading loop,** after triage and before understanding, within the same daily budget: an open request that needs you and has no draft, or whose conversations have news since it was drafted (`request_sources.noted_at`). Soonest due first, then the newest. First contacts are drafted when you ask.
- **What the model reads** (`memex.requests.draft`):
  - each conversation's last messages, by target;
  - other recent mail with the person;
  - open commitments between you;
  - the next 7 days' busy times in your primary calendar's time zone;
  - your voice: your last few mails to them (else anyone's) and Slack messages.
- **What it answers:** where to answer (one of the request's conversations), the text, and anything to check first. What only you can decide is left as `[[one | other]]`. Failing to draft is kept with the reason and tried again on news.
- **In the brief:** the draft card shows where it goes (✉ to whom and the subject, or 💬 the thread or conversation), the choices as buttons, and the editable text. Send (⌘↵) works once every choice is made; Draft again is also there.
- **Sending** (`memex.requests.send`) goes through the mail store's queue (with its undo window) or the Slack app's post, as you. The request is done (`replied`). It doesn't go through the agents' approval policy: your keypress is the approval, and agents can't call it.
- **API:** `memex.requests.draft` (now), `reply_to` (who and what a reply goes to), `send`; `Request.draft`.
- **Not yet:** documents the thread names, found in the memex, as context; a cost line in the brief.

### 10.5 Timing (step 5)

- **Settings → Brief** (per realm, else root's; `GET`/`PUT /api/v1/settings/brief`):
  - brief times, default 08:30 and 13:30;
  - the close of day, default 17:30;
  - whether urgent requests tell you at once (on by default).

  Times are in your primary calendar's time zone (UTC without one).
- **The brief** at each time is a notification with the count of requests that need you (and how many are due by tomorrow), the first five, first contacts, and how much was set aside. It isn't sent when nothing needs you. It links to the realm's Brief page.
- **The close of day** says how many requests you handled today and what's still yours, saying which have a draft ready.
- **Late times:** a time more than three hours past (the head was off) is let go. Each is sent once (`brief_sent`, migration 0015).
- **Urgent:** the triage prompt asks the model whether a request that needs you is urgent by your rule (`memex.triage.urgent`: due today, an outage, someone blocked now). An urgent one notifies you at once with high priority, once (`requests.notified_at`).
- **Events:** `request.opened` is a memex event for triggers (filtered by `from:`, as `mail.received` is), with the request's id, who, what, why, category, due and conversation. Notifications from the memex can link to a page.
- **Not yet:**
  - the FYI five-line summary;
  - `chat.received` and `commitment.due` events;
  - snoozing until there's no reply ("H if no reply").

## 11. Open questions

- **Gmail too?** Should a pitch also be labelled and archived in Gmail (so your phone matches), as a rule you approve once? Default: carbon6 only.
- **Report as spam to Google** for clear pitches? It trains Gmail but blocks the sender. Default: no.
- **Slack Connect vendors:** the same pitch filter for external DMs? Default: yes.
- **Playbooks** (acting alone, e.g. accepting a team member's meeting when you're free): designed after this, once drafts have earned trust.

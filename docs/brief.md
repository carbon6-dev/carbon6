# The brief: mail and chat as requests — Design

Status: designed (2026-10-26). Decided:
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
| `brief.triage` prompt | How to sort: what "needs you" means for you, with your notes from L appended as examples |
| `brief.request` prompt | What a request is, and when two are the same |
| `brief.draft` prompt | How to draft: tone, length, sign-off |
| `brief.pitch` prompt | The model's part of the pitch filter |
| `brief.important` | Who and what may interrupt you |
| `brief.times` | When briefs come |
| `brief.senders` | Your marks: pitch / known, by address, domain or tool |

The memex reads prompts from home only today (prompts::for_memex); it will read the realm's own, so mach5 can sort and draft differently from home.

## 8. Building on what's there

| Exists | Use |
|---|---|
| understand.rs loop, budget, prompts | triage, requests, drafts run in it |
| commitments table | "you owe" and "waiting on" (now from chat too, with due dates) |
| needs_reply / waiting views (rules) | replaced by triage's categories |
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

## 10. Open questions

- **Gmail too?** Should a pitch also be labelled and archived in Gmail (so your phone matches), as a rule you approve once? Default: carbon6 only.
- **Report as spam to Google** for clear pitches? It trains Gmail but blocks the sender. Default: no.
- **Slack Connect vendors:** the same pitch filter for external DMs? Default: yes.
- **Playbooks** (acting alone, e.g. accepting a team member's meeting when you're free): designed after this, once drafts have earned trust.

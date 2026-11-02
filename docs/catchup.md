# Catching up on a session — Design

Status: built (2026-11-02), for review live. Decided with you in doc:01a19de1 ("Catching up on a session"). Two features, each opt-in per session: a **catch-up** card when you come back, and a **timeline** that folds the transcript into hours.

## 1. Switches

- **Per session:** ⋯ → **Catch-up** ("a card when you come back") and **Timeline** ("turns folded, by hour"), between its actions and Archive. Kept on the head (`GET`/`PUT /api/v1/sessions/{id}/view`, a settings key per session, no table of its own), so they follow you across devices. A switch moves once the head has it, so a timeline just switched on reads its turns from a head that knows it's on.
- **New sessions:** Settings → Suggestions → *New sessions start with* (per realm, like the suggestions switch; off by default). A session started then gets its realm's choice; changing it later leaves existing sessions alone.
- **Routing:** a rule for `task:catchup` picks the catch-up's model, one for `task:timeline` the timeline's (on a connection the session's realm may use, its realm's rules first, as `task:suggest`); without one, the session's own model. Prompts: `session.catchup`, `session.timeline` (Settings → Prompts).

## 2. Catch-up

- **Since:** the later of what you last saw (the last event when the session was last marked seen, carbon6's seen turn) and your last message to it. A card is offered for 5 steps or more since then, or anything waiting on you. A step is a tool call, or a reply that called none.
- **When:** as a session comes into view (a tile opened, the page shown again), the page asks for its catch-up before it marks the session seen. The paragraph is made only when the card is shown, and kept in the head's memory by session, since and last event.
- **The card:** "Since you looked · 42m", one paragraph from the small model, and rows counted by the head (never the model), each only when not zero and each opening to its items, which go to their place in the transcript:
  - waiting on you (open approvals and questions, the session's open decisions), first;
  - finished (background tasks and subagents that ended; failures noted);
  - files changed (distinct paths from write, edit and patch tools) and commits (`git commit` in shell commands);
  - commands run (shell calls), with failures;
  - messages from other sessions and doc comments delivered.
- **What the model reads:** only what happened since, condensed: messages (first 300 characters), each turn's final reply (500), a line per tool call (its name and target, never its output), and the counts; at most about 6,000 tokens, the newest kept, older lines only counted.
- **Actions:** *Jump to it* scrolls the transcript to where you left off, marked by a dashed "you were here · 42m ago" line; *Caught up ✓* marks it seen and the card goes; × puts the card away (it comes again with what's new next time).
- **Where:** a wide session: the first section of the glance column ("SINCE YOU LOOKED · 42m"). Narrower: a "Catch up · N new" pill at the top of the transcript opens it in a popover; on a phone, in a sheet.
- `GET /api/v1/sessions/{id}/catchup` (`since=<seq>` to count from an earlier answer's point, `summary=true` for the paragraph); 204 while its catch-up is off.

## 3. Timeline

- A **Timeline · All** switch at the top of the transcript. Folded, each hour is a row, "14:00 · 3 turns · 41 steps", closed but for the hour a turn runs in.
- An open hour lists a line per turn: who started it ("You: …", "From <session>", "Task done: …", "Comments on <doc>"), what it did, and its steps. Clicking it shows the turn's events in full, inline (read from the head when the transcript hasn't loaded them).
- What a turn did comes from the small model, one call for an hour's turns when the hour is opened, kept in the head's memory; until then, its last reply's first sentence.
- `GET /api/v1/sessions/{id}/timeline` gives every turn (when it started, who started it, its steps, its reply's first sentence); `?hour=<ms>` the turns of that hour with their lines. Hours are grouped in the page, by the browser's clock. 204 while its timeline is off (the page says so, and shows an error reading it as one, never as "No turns yet.").

## 4. Not yet

- The catch-up says nothing of what changed in subagents' own transcripts beyond their ends.
- The card isn't shared across devices: each page asks for it as the session comes into view there.

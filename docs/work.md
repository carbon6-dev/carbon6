# Work: what you've left open — Design

Status: design (2026-10-29), not built. Decided with you:
- every session you start and every request you take on **belongs to a piece of work**: an agent guesses which, and you fix it with one click;
- work is **closed by you**: an agent offers "looks done" when it sees the evidence, and one key closes it;
- work holds sessions, Brief requests and commitments, plain to-dos you type, and outside things (pull requests, CI runs, documents);
- open work **finds you**: a Work panel of its own, a section in the Brief, nudges, and every session's header;
- every item in the header menu **opens a panel of its own** instead of replacing the one you're looking at.

## 1. The problem

You start something in a session. While it runs you start something else, or a message comes in and you begin on that. Each is reasonable alone; over days they pile up, half-finished, and some are forgotten. carbon6 tracks each session, each request and each decision, but nothing ties them to **the thing you meant to get done**, and nothing says whether that thing is finished. A session is too small a unit: one piece of work takes several sessions, a mail thread and a pull request, and one session may serve two pieces of work.

## 2. What a piece of work is

A **work item** is something you intend to finish:

| Field | |
|---|---|
| title | a few words: "Fix the Slack picture sync", "Answer Priya about the contract" |
| done when | one line saying what finished looks like ("merged and released", "she has an answer") |
| attached | sessions, Brief requests and commitments, mail and chat threads, decisions, outside links (PRs, CI runs, docs), notes |
| state | worked out from what's attached (§3), not kept by hand |
| where was I | a short note: what's done, what's left, the next step (§5) |
| realm, labels | as for sessions |
| opened, last moved, closed | times |

Work is flat: no projects, sub-items or dependencies. A label groups work items if you want that.

A **plain to-do** ("renew the domain") is a work item with nothing attached yet. Starting a session from it attaches that session.

## 3. Its state

Worked out from what's attached, in this order (the first that holds):

| State | When |
|---|---|
| **needs you** | a session ended its turn waiting on you; a decision is open; a draft is ready to send; an agent offers "looks done" |
| **moving** | a session is running |
| **waiting on others** | you replied and a commitment says you're waiting; a PR is in review; a CI run is running |
| **gone quiet** | nothing attached has moved for a while (a setting, 3 days to start) |
| **open** | otherwise (a to-do no one's started) |
| **done** / **dropped** | you closed it |

"Gone quiet" is the case this exists for: work you've stopped thinking about but never finished.

## 4. How work is made and attached

Making work must cost nothing while you're busy, or it won't happen.

- **Starting a session**, from the new-session form, a reply box's "Work in a session", a to-do or "New like this": the form has a **Work** field. It defaults to the work you started from (the Work panel, a work item, a session already in some work). Otherwise an agent reads the first message and picks an open work item or proposes a new title. Its guess shows in the session header, and one click changes it.
- **A Brief request** becomes work when you act on it beyond a single reply: you start a session from it, or reply saying you'll come back to it (a commitment "you owe"). It joins the work its conversation already belongs to, if any.
- **An agent in a session** can attach what it makes: a pull request it opened, a CI run, a document. Sessions get a `work` tool (attach, note), not the power to close.
- **You** can make work by hand, attach or detach anything, merge two items, or split one.
- **Unfiled** sessions are fine. A session nobody attached (the guess was "none of these") just stays out of Work.

The guess and its prompt are editable settings (`work.attach`), like every agent behavior.

## 5. Where was I

Each work item carries a note that answers "where was I?" in three parts: what's been done, what's left, and the next step. An agent rewrites it when something attached moves (a session's turn ends, a reply comes in, a PR changes state), and only then. That keeps it cheap. The prompt is an editable setting (`work.summary`).

The note is what makes coming back cheap. **Resume** on an item opens its last session. **Pick up** starts a new session whose first message is the note plus the attached things, with the configuration of the last session.

## 6. Closing

An agent offers **"looks done"** when the evidence says so: the PR merged, the request was answered, the session said it finished and nothing's waiting. The offer puts the item in "needs you", with the evidence. One key closes it; dismissing it keeps it open. Nothing closes without you. You can always close or drop by hand, and reopen.

## 7. Where you see it

- **The Work panel.** It opens like Mail and Chat, as a panel of its own, so the sessions you were looking at stay. Items are grouped by state: needs you, gone quiet, moving, waiting on others, open. Each row has the title, the "where was I" note's next step, what's attached as small chips, and how long since it moved. Keys as in the Brief: J/K, Enter, E (done), H (snooze until tomorrow), # (drop), R (resume).
- **The Brief:** a "Left open" section with the items in needs you and gone quiet.
- **Nudges:** a notification when an item goes quiet, or needs you and you haven't looked for a while. Rules and quiet hours are in Settings, as for the Brief's interruptions.
- **Every session's header:** its work item (or "Not part of any work · Add"), with its state and its other sessions and threads, one click away.

### 7.1 The header menu opens panels of their own

Today a header menu item (Sessions, Decisions, Settings…) replaces the panel you're focused on, and only the Apps menu opens its own. From now on **every header menu item opens in a panel of its own**: the one already showing it, or a new one. So opening Work, Decisions or the Brief never takes away the session you were in. ⌘-click still makes a new panel even when one already shows it.

## 8. How it's built (on the primitives)

Per docs/os.md, no app-specific machinery:

- **Records:** work items are a system collection `work` in each realm, with links to memex things and items (threads, requests, people) and refs to head objects (`session:<id>`, `decision:<id>`, URLs). Its history and optimistic updates come with it.
- **State:** a trigger on session, decision and memex request events recomputes the state of the work items those objects are attached to. "Gone quiet" is a periodic check.
- **Agent steps:** the attach guess, the "where was I" note and "looks done" are model calls with editable prompts, routed like the memex's understanding calls (a cheap model is fine), and shown on the Activity page.
- **Outside things:** a PR or CI run's state is read only when a session already has the means (gh, the forge's MCP) and records it with the `work` tool. carbon6 itself knows no forge or CI (lesson of docs/follow-through.md).
- **The panel** is a view. Its keys and layout are the Brief's.

## 9. Not doing

- No trackers polling outside systems, and no automatic closing. These were the parts of follow-through that didn't land.
- No hierarchy, priorities or due dates to start with. Commitments keep their own due dates and show them.
- No retroactive filing of old sessions beyond a one-time offer: the agent proposes work items for the open sessions and requests of the last two weeks, and you accept or skip each.

## 10. Build order

1. Header menu items open their own panel (§7.1).
2. The `work` collection, attaching by hand, the Work panel, and the session header line.
3. State from attached things, and "gone quiet".
4. The attach guess in the session form and for Brief requests. The `work` tool for sessions.
5. "Where was I", Resume and Pick up.
6. "Looks done", the Brief section, nudges, and the one-time offer for the last two weeks.

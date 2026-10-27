# Activity

The console's **Activity** page (`/admin/activity`) shows what carbon6 is doing right now:
- model calls as they stream;
- sessions running;
- what each connector's sync is doing;
- the memex's own work.

Everything on it comes from the head's memory and from what syncs already store. Nothing is recorded for it, and a restart of the head clears it.

## Model calls

The head keeps its last 200 model calls in memory (`c6_llm::activity`). Calls still going on are never dropped to make room.

**Which calls are tracked.** A client wrapped with `activity::track` reports every call it makes, which covers every model call the head makes:
- the memex: the brief's sorting, "same request?" checks, drafts, understanding, summaries, system pages and vision (`c6_memex::llm::ask`, whose `what` and `about` name the purpose and the conversation or item);
- the assistant;
- session suggestions.

Agent sessions run their model calls on workers, so they appear under **Sessions running** with a link to the session instead. Embeddings aren't shown.

**What each call shows:**
- what it is for and what it is about;
- the connection, the model and its reasoning effort;
- its state: waiting (no answer yet), thinking, answering, done, failed, or stopped (its caller gave up on it);
- how long it took, time to the first token, tokens in and out, and tokens per second;
- the latest line of its answer.

Click a call to see its prompt, its thinking and its answer. A call still going on is followed as it streams. Each is capped: the prompt at 20,000 characters (its start and end), the thinking and the answer at 8,000.

Above the list, calls in flight are counted by connection, and those still waiting for a first token are counted separately. A local server answers one call at a time, so the waiting count is its queue.

## Connectors

Each app's scheduled kit syncs: mail, calendar and contacts (the Google apps) and Slack. For each one the page shows:
- its schedule;
- the **round** going on now, or the last one: when it started, how many kit runs it took (an import goes on while the kit says there's more), and items and records brought in. Rounds are kept in memory, from `triggers.rs`'s kit action;
- each account's state, from the app's sync collection:
  - Google: the phase (importing, or up to date and following), how much is done, and when it last synced;
  - Slack: conversations imported, files waiting to be read, and how long a rate limit has left.

## Memex

- Model calls today out of the daily budget.
- Files waiting to be read, and what sessions sent that waits to go in.
- For each realm, how many conversations wait to be sorted for the brief, and why when it's stuck (no model chosen, or no calls left today).

## The top bar

While model calls are in flight or a sync is under way, a pulsing dot in the top bar shows how many. Clicking it opens the page.

## API

These are console ops (`system.*`):
- `GET /api/v1/system/activity`: everything, with each call's text left out.
- `GET /api/v1/system/activity/calls/{id}`: one call, whole.
- `GET /api/v1/system/activity/counts`: the top bar's numbers.

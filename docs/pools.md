# Pools

A **pool** is a connection made of other connections. What runs in the head and calls models through one connection spreads its calls over the pool's members instead. For example, the memex can sort and read mail on two Ollama servers at once: one on your laptop, one on your server.

## What uses a pool

Pools are for what runs in the head, which calls models itself:
- the memex: understanding (sorting, drafts, cards, reading, pages), vision, embeddings and transcription (**Settings → Memex**);
- the assistant (**Settings → Assistant**, or a routing rule for `assistant`);
- session suggestions (a routing rule for `task:suggest`).

Sessions don't use pools. A session's worker dials its connection itself, so a session can't start on a pool, and a routing rule names a pool only if it applies to `assistant` or `task:suggest`.

## How a pool picks a member

- **Least busy first.** Each member takes some calls at once (its slots). A call goes to the member with a free slot that has the fewest calls in flight for its slots (on a tie, the first listed).
- **Waiting its turn.** When every slot is taken, the call waits in the head, first come first served, until a slot frees up. It gives up after 10 minutes.
- **Failing over.** A member that fails for now (refuses the connection, times out, or answers 429 or 5xx) rests, and the call goes to another member at once. Its rest is 15 seconds, twice as long after each failure in a row, up to 5 minutes; an answer ends it. A pool's members are tried once each, not retried as a plain connection is.
- **Passing over.**
  - **The model:** each member's model list (`/models`) is kept for 5 minutes, and members without the model asked for are skipped. `name` and `name:latest` are the same model. A member with no model list may have any.
  - **The realm:** a member's own realms (**Console → Connections**) apply too: a member that isn't for the caller's realm is skipped.
- **None left.** With no member to take the call, it fails "for now". The memex then waits and asks again later (docs/brief.md), as it does when a model is unreachable.

## Setting one up

1. Add each server as a connection of its own (**Settings → Connections**), for example `mac` (`http://mac.tailnet:11434/v1`) and `mumuzz` (`http://ollama.svc:11434/v1`).
2. Add a connection of kind **Pool**: name it, and add its members with how many calls each takes at once (Ollama: its `OLLAMA_NUM_PARALLEL`).
3. Choose the pool where you'd choose a connection, for example **Settings → Memex → Understanding**, and a model the members have.
4. Set **Settings → Memex → calls at once** to about the total of the members' slots. The memex never makes more calls side by side than that, so a lower number leaves slots idle and a higher one only queues calls in the pool.

Edit a pool's members with **Edit** on its row, or `connections.update`.

## Rules

- A pool's members are other connections, each once, with 1 to 64 calls at once. A member can't be a pool or Claude Code.
- The memex's rules apply to every member. Embeddings need OpenAI-style members (`/embeddings`: OpenAI or OpenAI-compatible, such as Ollama), and transcription needs `/audio/transcriptions`.
- A pool's models (`connections.models`) are its members' models together.
- A deleted member drops out of the pool.

## Seeing it

The **Activity** page shows each call with the member that took it and the pool, for example `mac (pool ollamas)`. A call refused by a member that went away shows as failed on that member, followed by the same call on another.

## API

- `POST /api/v1/connections` (`connections.create`) with `kind: "pool"`, a `name` and `members`: `[{"connection_id": "mac", "slots": 2}, …]` (a connection's id or name).
- `PATCH /api/v1/connections/{id}` (`connections.update`) with `members` (all of them) or `name`.
- `GET /api/v1/connections/{id}/models` (`connections.models`).

## As built

- The scheduler is `c6_llm::pool`: `Pool::acquire` takes a slot (or waits for one), and `PoolClient` is a model client over a pool. `Pool::run` does the same for embeddings and transcription.
- The head keeps one pool per pool connection (`c6-head/src/pools.rs`). Its members get credentials as the head's own calls do. The memex reaches it through its in-process link (`HeadLink::pool`).
- The members are stored in the `connections.members` column (migration 0050).

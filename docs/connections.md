# Connections

Add a model server in **Settings → Connections**, or with `connections.create`
(`POST /api/v1/connections`). Each connection has a name, kind, base URL, optional
API key and optional `default_model`. `supports_effort` in the response says
whether the connection passes reasoning effort to the model. Pools have members
instead of a server (docs/pools.md). A connection's `realms` list controls where
it is available; empty means all realms.

**Context window** (`context_window`) is an optional token count, from 1,024 to
100,000,000. Empty means automatic. Set it when the server cannot report its
running window, or to raise or lower the automatic figure. Claude Code sessions
keep using Claude Code's own context management.

Edit it with **Edit** on the connection's row, or `connections.update`
(`PATCH /api/v1/connections/{id}`). Omitted or `null` leaves the setting
unchanged; `{"context_window": 0}` restores automatic. Create accepts omitted,
`null` or 0 as automatic. List, create and update return
`context_window: null` for automatic.

## Choosing the window

A setting wins. Otherwise an `openai_compat` caller uses the smaller of the
server's detected window and carbon6's built-in figure for that model. Unknown
servers keep the built-in figure (128,000 for an unknown model). Other connection
kinds use the setting or the built-in figure.

The caller asks three endpoints together, each with a three-second timeout and
the connection's bearer and headers:

- vLLM: `/models` under the base URL, the matching model's `max_model_len`.
- llama.cpp: `/props` under the server root, `default_generation_settings.n_ctx`
  (the window per slot), else `n_ctx`.
- Ollama: `/api/ps` under the root, the matching loaded model's `context_length`.
  `name` and `name:latest` match. `/api/show` reports the trained window and is
  not used.

The root is the base URL without its trailing slash and `/v1`. If several
endpoints report a window, the smallest is used. Detection is cached in memory
in each process, per connection and model, and checked at most once a minute.
A failed check or an unloaded Ollama model keeps the last window seen. A process
restart forgets it.

Worker sessions, subagents and the head's assistant resolve their effective window
before each turn. Automatic compaction starts at about 85% of that window. The UI
meter uses the window recorded for the latest turn, falling back to the session's
start or model switch for older workers. The setting travels with credential
grants: old workers ignore it, and new workers with an old head use automatic
detection. A worker session keeps the setting it started with: grants are cached
per session. Subagents use their root session's grant. A changed setting reaches
new worker sessions; the head's own calls see it without a restart.

The memex estimates prompt tokens as text characters / 3, rounded up, plus 1,024
per image and 256 for the chat template. With a setting or detected window, it
clamps output (including room for thinking) to what remains. A prompt that leaves
less than the task's answer limit fails without calling the model. This is a
failed read, not a temporary connection failure. With no known window, output
limits stay as before. Pools use the smallest effective member window unless
the pool itself has a setting (docs/pools.md).

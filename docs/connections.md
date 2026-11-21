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

## Manually switched aliases

An **Alias** is a stable connection name pointing to one real connection. Use
it for sessions, model switches, subagent defaults, routing rules, named agents,
or head-side model tasks. Unlike a pool, it never balances calls or falls back
to another account. For example, create `chatgpt` pointing to your first
ChatGPT account, then switch it to your second account when you choose.

In **Settings → Connections**, create an Alias and choose its initial target.
Its target's provider kind is fixed at creation: switching a ChatGPT alias
requires another ChatGPT connection, and likewise for other supported providers.
Aliases cannot target aliases, pools or Claude Code connections, and pools
continue to require concrete connections as members. The target's
models are offered when choosing a model. The target cannot be deleted until
the alias has been switched away or removed.

The alias row shows its current target and when it was selected. Choose a
target and press **Switch**, or use the admin operation
`connections.alias.switch`. Switch history records who switched, when, and
the old and new targets. Connections remain user-managed; agents cannot
silently switch accounts.

API creation uses `{"kind":"alias","name":"chatgpt","alias_target_id":"ACCOUNT_ID"}`.
Switch with `POST /api/v1/connections/{id}/alias/switch` and
`{"target_id":"OTHER_ACCOUNT_ID"}`; read history with
`GET /api/v1/connections/{id}/alias/history` (`connections.alias.history`).

Resolution happens for each model call, including subsequent calls within a
running turn or subagent. Existing sessions keep the alias, so no restart or
per-session reconfiguration is needed. An already in-flight request finishes
on the account it started with. Usage and provider quota information belong
to the real account that answered, not to the alias.

Account-bound reasoning and opaque provider state must not cross accounts.
Aliases replay that state only to the account known to have produced it;
unverified state after a worker restart is omitted conservatively. Ordinary
conversation text and tool calls/results remain available across switches.
Reauthorizing a ChatGPT connection must retain its account and workspace;
add a separate connection for a different workspace, then switch the alias.
Legacy sign-ins without a saved verified account identity also require a new
connection rather than reusing an identity that cannot be checked.

Workers do not cache alias credential grants: each call asks the head again.
Consequently an alias call needs a reachable head; it does not keep using an
old target during a head outage. Direct connections retain their existing
credential-cache behavior. Workers must support alias grants; the head refuses
to hand an alias to older workers that would cache its target indefinitely.

**Cold-cache cost:** after a switch, the new account has not cached each
session's conversation. Its first request reads that session's context again
at full input cost; switching does not transfer a provider's prompt cache.
Claude Code sessions use Claude Code's own sign-in, not carbon6 connections,
so aliases do not switch their accounts.

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
per session for direct connections. Subagents use their root session's grant.
A changed setting reaches new direct-connection worker sessions; aliases
re-read their effective setting with each grant, and the head's own calls see
it without a restart.

The memex estimates prompt tokens as text characters / 3, rounded up, plus 1,024
per image and 256 for the chat template. With a setting or detected window, it
clamps output (including room for thinking) to what remains. A prompt that leaves
less than the task's answer limit fails without calling the model. This is a
failed read, not a temporary connection failure. With no known window, output
limits stay as before. Pools use the smallest effective member window unless
the pool itself has a setting (docs/pools.md).

## Web-search citations

OpenAI Responses and the ChatGPT connection send text deltas for the live reply
and completed message items with `output_text.annotations`. Carbon6 keeps those
annotations (including `url_citation` URLs, titles and character offsets) as
display metadata on the journal's text block, and exposes URL citations on the
HTTP text block. This is an additive protobuf field and optional JSON field;
older events need no migration. The original text, including provider citation
markers, is replayed unchanged. Annotation metadata is not sent to the model.
Streamed `response.output_text.annotation.added` events are also kept, as a
fallback if a completed content item omits its annotations.

Resolved citations show as small superscript numbered links with source titles
on hover. Repeated URLs share a number within the reply. Markdown session exports
keep portable numbered links with message-specific reference definitions. A
source list is not added below the reply: the links themselves carry the URL and
title, matching the transcript's existing compact style.

Provider-run searches are kept as opaque tool items and displayed as server tools.
The stored ChatGPT calls inspected on November 5, 2026 contained search queries
or opened-page URLs, not a map from `turn…search…` ids to source URLs. The database
fixtures had no useful citation examples. Carbon6 therefore does not guess links
for old replies, live text awaiting annotations, or unresolved markers: those
markers are hidden without leaving spaces before punctuation. Catch-up and
timeline previews, activity previews, notifications, copied rendered text and
exports use the same display cleanup. Inline and fenced code stay literal.

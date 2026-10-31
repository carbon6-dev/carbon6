# Carbon6 — Architecture

Status: draft v0.1 (2026-10-09). Implements [requirements.md](requirements.md) v0.2.

## 1. System overview

```
                     private network / tailnet
 ┌──────────── browser ────────────┐
 │  Leptos CSR app (wasm)          │
 │  xterm.js · CodeMirror          │
 └──────────────┬──────────────────┘
                │ HTTP/JSON  +  WebSocket            (UI port, plain HTTP, private bind)
 ┌──────────────▼──────────────────────────────────────────────────────┐
 │ c6h                                                                 │
 │  axum API ─ SessionService ─ EventStore(SQLite) ─ BlobStore         │
 │  ConfigService ─ CredentialBroker(OAuth refresh) ─ WorkerRegistry   │
 │  RpcRouter(head→worker req/resp) ─ OTLP exporter                    │
 │  tonic WorkerHub                                                    │
 └──────────────▲──────────────────────────────────────────────────────┘
                │ gRPC / HTTP/2 / mTLS  (worker port; worker always dials)
       ┌────────┴─────────┬─────────────────────┐
 ┌─────┴──────┐     ┌─────┴──────┐        ┌─────┴──────┐
 │ c6w (mac)  │     │ c6w (linux)│  ...   │ c6w        │
 │ sessions   │     │            │        │            │
 │ journal    │     │            │        │            │──► LLM providers
 │ fs/git/pty │     │            │        │            │    (direct, head creds)
 └────────────┘     └────────────┘        └────────────┘
```

- The head (c6h) is the system of record. It serves two ports: a UI/API port and a worker port. Both bind only to private interfaces.
- Workers (c6w) dial the head and keep one long-lived HTTP/2 connection open. Every head→worker request travels back over a stream the worker opened, because the head never dials a worker.
- Workers call LLM providers directly, using short-lived credentials issued by the head.

## 2. Cargo workspace

```
carbon6/
  proto/                  *.proto (source of truth for the wire protocol)
  crates/
    c6-proto/             prost/tonic generated code + pbjson serde impls
    c6-core/              ids, session event model helpers, config schema, errors
    c6-api/               HTTP/WS API types shared by c6h and c6-ui (compiles to wasm)
    c6-llm/               provider clients, canonical message model, caching/thinking logic
    c6-agent/             agent loop, context assembly, compaction, subagents
    c6-tools/             Tool trait and built-in tools
    c6-testkit/           scripted OpenAI-compatible mock server for tests
    c6-mcp/               MCP client integration (wraps rmcp)
    c6-head/              head library: storage, services, API, worker hub
    c6-worker/            worker library: connection mgr, journal, session runtime, fs/git/pty
    c6-ui/                Leptos CSR app (wasm), embedded into c6h at build time
  bins/
    c6h/                  thin main → c6-head
    c6w/                  thin main → c6-worker
```

Key dependencies: tokio, tonic/prost, axum, sqlx (sqlite), reqwest (rustls), serde, schemars, rmcp, ignore/globset/grep-searcher (ripgrep internals), portable-pty, notify, tracing + opentelemetry, rustls + rcgen, uuid (v7), blake3, chacha20poly1305, leptos, tailwind.

Why a CSR SPA rather than Leptos SSR/server functions: requirement R-HEAD-4 calls for an API that automations and scripts can also use. With a SPA, the UI uses the same versioned `/api/v1` that those clients will, so there is no second, UI-only API surface. The built assets are embedded in `c6h`, which keeps it a single binary.

## 3. Identity, enrollment and transport

**Head PKI.** On first start, c6h creates a private CA (rcgen) and a server certificate in its data directory.

**Enrollment.**
1. In the UI, the user clicks "Add worker". The head creates a one-time enrollment token that contains the head's address, the CA fingerprint and a secret.
2. On the worker, the user runs `c6w enroll <token>`. The worker generates a keypair, sends a CSR plus the secret, pins the CA, and receives a client certificate whose subject is its `worker_id`.
3. The worker keeps its identity in `~/.local/state/c6w/identity/`. This is the only thing c6w stores long-term apart from caches.

**Transport.** gRPC over HTTP/2 with mTLS on the worker port. h2 keepalive pings detect dead connections; app-level pings measure latency for the UI.

**Versioning.** `Hello` carries `protocol_version` (an integer) and a list of `capabilities`. The head supports versions N and N−1, and rejects anything else with a clear error shown in the UI. Protobuf field rules are followed: fields are only added, never renumbered.

## 4. Wire protocol

```proto
service WorkerHub {
  // Control plane + durable events. Exactly one per worker connection. (Not named Connect: that collides with tonic's generated client constructor.)
  rpc Attach(stream WorkerMsg) returns (stream HeadMsg);

  // Data-plane streams, opened by the worker in response to a head request
  // ("reverse dial"), so bulk/interactive traffic gets its own h2 stream
  // and never blocks control messages.
  rpc Pty(stream PtyFrame) returns (stream PtyFrame);
  rpc PutBlob(stream BlobChunk) returns (BlobAck);
  rpc GetBlob(BlobRef) returns (stream BlobChunk);
  rpc Telemetry(stream ExportTraceServiceRequest) returns (TelemetryAck); // OTLP proto types
}

message WorkerMsg { oneof msg {
  Hello hello;                 // first message
  SessionEventBatch events;    // durable, journaled
  EphemeralBatch ephemeral;    // streaming deltas, tool output chunks; best effort
  RpcResponse rpc_response;    // reply to a head RpcRequest (fs, git, ...)
  CredentialRequest cred_req;
  Ping ping;
}}

message HeadMsg { oneof msg {
  Welcome welcome;             // ack positions, config snapshot/version, accepted?
  EventAck ack;                // cumulative per-session ack
  SessionCommand command;      // create/resume session, user input, interrupt, set model, ...
  RpcRequest rpc_request;      // fs.list, fs.read, git.diff, pty.open, ...
  ConfigUpdate config;         // new config version (full or delta)
  CredentialGrant cred;        // token response or proactive refresh push
  Pong pong;
}}
```

**Handshake.**
- `Hello{worker_id, protocol_version, c6w_version, os, arch, hostname, capabilities, config_version, sessions:[{session_id, last_journaled_seq}]}`
- `Welcome{head_version, acks:[{session_id, acked_seq}], config: ConfigUpdate?, pending_commands}`

The worker then resends every journaled event with `seq > acked_seq`.

**Head→worker requests.** Each `RpcRequest` carries `{id, deadline, method, payload}`. Small replies come back as `RpcResponse` on the Attach stream. For streaming methods (`pty.open`, large `fs.read`), the worker opens the matching data-plane RPC and tags it with the request id. The head's `RpcRouter` holds the pending requests and the correlation ids.

**Flow control.** HTTP/2 window flow control carries the bytes. On top of it, the worker bounds the number of unacknowledged durable events it has in flight per session. Ephemeral messages are dropped under backpressure; durable ones never are.

## 5. Session model and event log

### 5.1 Entities (head-owned metadata)

- `Worker{id, name, hostname, os, arch, last_seen, version, status}`
- `Workspace{id, worker_id, name, root_path, default_base_branch, use_worktrees}`
- `Session{id (UUIDv7), worker_id, workspace_id?, cwd, worktree?, branch?, parent_session_id?, title, model_ref, status, created_at, archived}`
- `Terminal{id, worker_id, cwd, status}`. A terminal is not a session and is not persisted beyond metadata.

### 5.2 Single-writer log

Each session has one append-only log, and **only the worker writes to it**. The head never appends.

- User input reaches the worker as a `SessionCommand::UserInput{command_id, content}`.
- When the worker accepts the input, it appends a `UserMessage{command_id, ...}` event.
- Until that event syncs back, the UI shows the input as *queued*. Commands live in a head-side queue and are redelivered after a reconnect. `command_id` makes delivery idempotent.

With one writer per log, sync is a cumulative ack per session. No merge or conflict logic is needed.

### 5.3 Durable events

```
SessionEvent { session_id, seq: u64, ts, kind }
kind =
  SessionStarted   { model_ref, system_prompt_hash, tool_profile, cwd, git_head }
  UserMessage      { command_id, parts[] }              // text, image(blob), file refs
  TurnStarted      { turn_id }
  AssistantMessage { turn_id, blocks[], stop_reason, provider_opaque? }   // tool calls are ToolUse blocks here
  ToolResult       { call_id, output_parts[], is_error, truncated, full_output_blob? }
  Usage            { request_id, model, input, output, cache_read, cache_write, reasoning, cost_micros? }
  Compaction       { summary, replaces_through_seq }
  ModelChanged     { model_ref }
  SubagentSpawned  { child_session_id, call_id }
  TurnEnded        { turn_id, reason: Completed|Interrupted|Error|WorkerRestarted }
  Error            { message, retryable }
```

- **Ephemeral events** (not journaled) include `AssistantDelta`, `ThinkingDelta`, `ToolOutputChunk` and `Status`. They are forwarded live to UI subscribers when the worker is connected. The durable events carry the final content, so losing ephemeral events loses nothing.
- **Blobs** (images, large tool outputs, attachments) are content-addressed by blake3 hash. They are uploaded via `PutBlob` before the event that references them, and deduplicated on the head.
- **Storage encoding.** Events are stored on both sides as their protobuf bytes, so nothing is transcoded. The API serves them as JSON via pbjson.

### 5.4 Worker journal and offline operation

- The journal is a SQLite database in WAL mode at `~/.local/state/c6w/journal.db`, with tables `events(session_id, seq, bytes)`, `outbox_blobs`, `acks` and `config_cache`.
- Events are fsynced to the journal **before** they are sent to the head. Rows are trimmed once acked, except that the full log of each *live* session stays cached so turns can be rebuilt without asking the head.
- To resume a session that isn't cached (for example after a worker restart), the worker asks the head for the log (`SessionCommand::Resume{events}`).
- If the worker crashes mid-turn, then on restart it finds a turn with no `TurnEnded`, appends `TurnEnded{WorkerRestarted}`, and does not continue the turn automatically.

## 6. Head internals

| Component | Responsibility |
|---|---|
| `WorkerRegistry` | Connected workers, their outbound channels, liveness, capabilities |
| `SessionService` | Session create/resume/archive, the command queue, routing commands to workers |
| `EventStore` | Durable events (sqlx/SQLite), acks, history queries |
| `BlobStore` | `data/blobs/ab/cdef…`, content-addressed files |
| `LiveHub` | A tokio broadcast channel per session and one for the fleet; fans durable and ephemeral events out to WebSocket subscribers |
| `RpcRouter` | Head→worker request/response correlation, deadlines, streaming hand-off |
| `ConfigService` | Versioned config documents; computes per-worker effective config; pushes `ConfigUpdate` |
| `CredentialBroker` | Encrypted credential storage, OAuth login flows, proactive refresh, `CredentialGrant` |
| `Telemetry` | Receives worker OTLP batches, adds head spans, exports to the configured OTLP endpoint |
| `Api` | axum `/api/v1` REST + WebSocket, serves the embedded UI |

**Storage trait.** All persistence goes through `trait Store` (sessions, events, config, credentials). The implementation is `SqliteStore` using sqlx. A `PostgresStore` can be added later without touching the services.

**Data dir:** `c6h.toml` (bootstrap: bind addresses, data dir, OTLP endpoint), `c6h.db`, `blobs/`, `pki/`, `master.key`. Everything else is configured in the UI and stored in the database.

### 6.1 Config model

Config documents are layered: **global → worker → workspace → session overrides**. Each document has a monotonic `config_version`. The head computes each worker's effective config: model refs, connection metadata (no secrets), MCP server definitions, skills (content), global instructions, tool settings and prompt templates.

Workers cache this config in a non-authoritative way so they can run offline. The head's copy always wins on reconnect.

### 6.2 Credentials

**Storage.** Secrets are encrypted with ChaCha20-Poly1305 under `master.key` (a file with 0600 permissions, or the OS keychain later).

**`CredentialGrant{connection_id, auth: ApiKey|Bearer, base_url, extra_headers, expires_at, context_window}`.**
- API keys are long-lived. Workers keep them in memory only. `context_window` is the connection's setting (0: automatic); old peers ignore it or default to 0. Settings changes reach new worker sessions; the head clears its own cached grants when a connection changes.
- OAuth connections (ChatGPT, Claude subscription): the head alone refreshes them, and does so before access tokens expire. It pushes new grants to every connected worker that uses the connection. Workers never see refresh tokens.
- An offline worker keeps working until its cached access token expires.

**ChatGPT sign-in.** This is OpenAI's "Sign in with ChatGPT" for open-source apps (developers.openai.com/siwc/token-sharing-open-source/sign-in): authorization code with PKCE, dynamic client registration (`dynamic_agent_client` with `agent_name_hint=carbon6` on first use, then the issued `oaiapp_…` id), the head's `ext_agent_host_id` (a `urn:uuid:` made once and kept in the `chatgpt_host_id` setting), and the `chatgpt.tokens.use.direct` scope. The redirect must be the loopback URL `http://127.0.0.1:<port>/callback` (only the port varies). The head uses its UI port, so a browser on the head's machine completes the flow directly. From any other machine, the user pastes the address the browser ended on into the UI. Any number of accounts and workspaces can be signed in, each its own `chatgpt` connection with the client id issued to it (client ids are per account; a new account always registers afresh). The head verifies each sign-in's ID token (RS256 against the keys from OpenAI's discovery document, issuer, audience = the client id, expiry, nonce) and keeps its `sub`, email and workspace (`chatgpt_account_id`, when present) with the tokens. Adding an account that is already here (same `sub` and workspace) replaces that connection's sign-in instead of duplicating it. "Sign in again" reuses the connection's client id with `id_token_hint` and refuses a different account. Refreshes check that a new ID token is still the same account. Claude subscription OAuth is not supported (requirements R-LLM-3).

### 6.3 HTTP API (sketch)

```
GET    /api/v1/workers                         POST /api/v1/workers/enrollments
GET    /api/v1/workspaces   POST/PATCH/DELETE ...
GET    /api/v1/sessions?worker=&status=        POST /api/v1/sessions
GET    /api/v1/sessions/{id}                   GET  /api/v1/sessions/{id}/events?after=SEQ (or ?before=SEQ&limit=N)
POST   /api/v1/sessions/{id}/messages          POST /api/v1/sessions/{id}/interrupt
PATCH  /api/v1/sessions/{id}                   (title, model, archive)
GET    /api/v1/workers/{id}/fs/list?path=      GET  /api/v1/workers/{id}/fs/read?path=&range=
GET    /api/v1/workers/{id}/fs/search?q=       GET  /api/v1/workers/{id}/git/{status|diff|log}?path=
POST   /api/v1/workers/{id}/terminals          DELETE /api/v1/terminals/{id}
GET/PUT /api/v1/config/...                     /api/v1/connections, /api/v1/connections/{id}/oauth/*
GET    /api/v1/blobs/{hash}
WS     /api/v1/sessions/{id}/stream            durable + ephemeral events, resumes from ?after=SEQ; ?tail=N: a window
WS     /api/v1/terminals/{id}/attach           binary frames (data, resize)
WS     /api/v1/live                            fleet/session status feed
```

## 7. Worker internals

```
c6w daemon
 ├─ ConnectionManager   dial, mTLS, Hello/Welcome, reconnect (exp backoff + jitter), outbox replay
 ├─ Journal             SQLite WAL; durable events, acks, config cache
 ├─ SessionManager      HashMap<SessionId, SessionHandle>; spawns one SessionActor task per session
 │    └─ SessionActor   agent loop (c6-agent), tool registry, MCP clients, cancellation tokens
 ├─ CredentialCache     in-memory grants; requests/refresh hooks
 ├─ ProcessManager      background shells, process groups, output ring buffers
 ├─ FsService           list/stat/read/search/watch (ignore, grep-searcher, notify)
 ├─ GitService          git CLI wrapper: status/diff/log/branches/worktrees
 ├─ PtyService          portable-pty; detachable terminals with scrollback ring buffer
 └─ TelemetryExporter   tracing-opentelemetry → buffered OTLP batches → head
```

- **Process groups.** Every spawned process runs in its own process group (`setsid`). Interrupting a tool or a session kills the whole tree.
- **Terminals.** Each runs a login shell in the requested cwd. A PTY outlives the browser connection. On reattach, the worker replays its scrollback ring buffer (default 1 MiB), then streams live output. Terminals die if the worker restarts.
- **Worktrees.** When a session is created with `use_worktrees`, the worker runs `git worktree add <state>/worktrees/<repo>/<slug> -b c6/<slug> <base>` and reports the path and branch to the head, which records them. Archiving a session can remove the worktree; the branch is kept.
- **Git auth.** Git uses the host's own SSH and `gh` configuration (D-1). `c6w git-credential` exists from M0 as a credential-helper entry point. Initially it does nothing; later it will fetch tokens for head-managed git connections.

## 8. LLM layer (`c6-llm`)

### 8.1 Canonical model

```rust
enum Block {
    Text(String),
    Image(BlobRef),
    Thinking { text: String, signature: Option<String> },   // Anthropic
    RedactedThinking(Vec<u8>),
    Reasoning { encrypted: Option<String>, summary: Vec<String> }, // OpenAI Responses
    ToolUse { id, name, input: Value },
    ToolResult { id, content: Vec<Block>, is_error: bool },
}
struct Message { role: Role, blocks: Vec<Block>, provider: Option<ProviderTag> }
```

Opaque reasoning data (thinking signatures, encrypted reasoning) is kept and sent back verbatim to the same provider family. If the session switches to a different family, it is dropped and only the visible text is kept.

### 8.2 Clients

```rust
trait ModelClient {
    async fn stream(&self, req: ModelRequest, cancel: CancellationToken)
        -> Result<BoxStream<'static, Result<ModelEvent>>>;
}
```

| Client | Covers |
|---|---|
| `AnthropicMessages` | Claude subscription (OAuth), and Anthropic API keys for free |
| `OpenAIResponses` | OpenAI API keys, ChatGPT subscription OAuth |
| `OpenAIChat` | Any OpenAI-compatible Chat Completions server (local LLMs, gateways) |

- **HTTP.** reqwest with our own SSE parsing. We use no provider SDK crates, so we keep full control of caching, thinking and beta fields.
- **Retries.** Exponential backoff that honours `retry-after`. Overload and 5xx errors are retried. A context-length error triggers compaction and one retry.
- **Model registry.** For each model: context window, max output, thinking/effort support, caching style, image support, pricing, and **tool profile**. Built-in defaults ship with c6w. A connection's optional `context_window` overrides its window. With no setting, OpenAI-compatible callers use the smaller of the detected running window and the built-in figure; other kinds use the built-in figure. Detection lives in `c6-llm`, cached per process, connection and model for a minute, keeping the last value when a check finds nothing (docs/connections.md).

### 8.3 Cache and reasoning optimizations

- **Stable prefix discipline.**
  - The order is tools → system → instructions → history. Tool lists are sorted deterministically.
  - Volatile context (date, git status) goes in the first user message, not the system prompt.
  - MCP tool schemas are frozen for the life of a session.
- **Anthropic.** `cache_control` breakpoints go on the tools and system block, plus a rolling breakpoint on the latest message (and the one before it, staying within the 4-breakpoint limit). Thinking blocks are kept, with their signatures, across tool-use loops. Extended or adaptive thinking and effort are set per model.
- **OpenAI Responses.** `store: false` plus `include: ["reasoning.encrypted_content"]`, so reasoning carries over statelessly. That way any worker can rebuild any session. `prompt_cache_key = session_id`, and `reasoning.effort` is set per session. ChatGPT-plan requests also send the session id as a `session_id` header, as Codex does: without it the backend served about half of a session's calls from a cache holding only the instructions (42% of input cached over four days of real use), and each of those calls paid for the whole conversation again. The Usage page's **Cached** column shows the share per day, model, connection, label and session, and marks large amounts below 60%; Analytics shows it per model.
- **Chat Completions.** Prefix stability only. Provider-specific extras, such as reasoning fields on local servers, go through `extra_body` set in the model registry.
- **Usage accounting.** Cache read and write tokens are recorded in `Usage` events, and the UI shows the cache hit rate per session.

## 9. Agent loop (`c6-agent`)

```
SessionActor loop:
  recv command
    UserInput    → append UserMessage; if idle start turn, else queue for next request boundary
    Interrupt    → cancel token (model stream + running tools); append TurnEnded{Interrupted}
    SetModel     → append ModelChanged
  turn:
    loop {
      req = ContextBuilder::build(history, model, tool_profile)
      stream model → forward deltas as ephemeral → collect AssistantMessage → append
      if no tool calls: break
      run tool calls (parallel where allowed) → append ToolCall/ToolResult
      drain queued user input into the next request
      if tokens > compact_threshold: compact
    }
    append Usage(s), TurnEnded{Completed}
```

- **ContextBuilder** assembles, in order:
  1. The base prompt template (from the head)
  2. Environment (OS, shell, cwd, whether it's a git repo)
  3. The `AGENTS.md`/`CLAUDE.md` hierarchy from the cwd up to the repo root, plus global instructions from the head
  4. The skills index
  5. MCP server instructions
  6. History after the latest `Compaction`
- **Tool profiles per model family.** Models do best with the tools they were trained on. GPT/Codex models get `shell` + `apply_patch` + `update_plan` (Codex-style). Claude models get `Bash` + `Read` + `Edit` + `Write` + `TodoWrite` (Claude Code-style). Both profiles share the same implementations underneath.
- **Compaction.**
  - Triggered automatically when the next request is estimated to use about 85% of the effective context window, manually from the UI, or by a context-length error. Reported usage wins: only estimated history growth is added to it. Without a report it counts the system prompt, tool specs and history at about 3.5 characters per token.
  - The model is asked to summarize. Its input and output must fit the window, and the summary must fit below the threshold beside the normal system prompt and tools. A cut-off summary is discarded. A bounded digest, then a mechanical digest, stand in when needed. A `Compaction{summary, tokens_before}` event is appended. Later requests use the summary plus the history after it.
  - Prefix caching restarts after a compaction. That's expected.
- **Large tool output.** Output is capped inline (default 30k chars, keeping the head and the tail). The full output goes to a blob and to `<state>/tool-output/<call_id>`, and the model is told the path so it can read more.
- **Subagents.** The `Task`/`spawn_agent` tool creates a child session (`parent_session_id`) on the same worker, with its own log and a restricted tool profile. Its final message becomes the tool result. The UI nests child sessions under their parent.

## 10. Tools (`c6-tools`)

```rust
trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> ToolSchema;          // schemars-generated JSON Schema
    fn concurrency(&self) -> Concurrency;    // Shared (read-only) | Exclusive
    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput;
}
// ToolCtx: cwd, session_id, cancel token, ephemeral sink (streaming output), ProcessManager, FsService
```

| Tool | Notes |
|---|---|
| Read | Line-numbered, offset/limit, images → blobs; PDFs later |
| Write / Edit | Exact string replace, `replace_all`; tracks files read in the session so it can detect files that changed on disk since |
| apply_patch | Codex patch format |
| Bash / shell | Fresh shell per call; cwd persists per session; timeout; `run_in_background` → id; companion output/kill tools |
| Grep / Glob | ripgrep crates embedded in-process (no external `rg` needed), respects `.gitignore` |
| WebFetch | reqwest + HTML→Markdown; size cap |
| WebSearch | Uses the provider's native server-side search tool where one exists; otherwise a configurable search API |
| TodoWrite / update_plan | State goes into the event log; the UI renders it |
| Task / spawn_agent | Subagents (§9) |
| Skill | Loads a skill body on demand (the index is in the prompt) |
| MCP tools | Exposed as `mcp__<server>__<tool>` via rmcp (stdio servers spawned on the worker, HTTP servers dialed from the worker) |

Shared tools run concurrently when the model makes parallel calls. Exclusive tools run one at a time, in the order the model issued them.

## 11. Observability

- **Workers.** Workers instrument with `tracing`, and `tracing-opentelemetry` maps the spans onto the OTel GenAI semantic conventions:
  - `invoke_agent` per turn (`gen_ai.agent.name`, `gen_ai.conversation.id = session_id`)
  - `chat {model}` per model request (`gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.input_tokens` / `output_tokens`, cache token attributes, `gen_ai.response.finish_reasons`)
  - `execute_tool {name}` per tool call (`gen_ai.tool.name`, `gen_ai.tool.call.id`)
- **Transport.** Batches of OTLP protobuf go to the head over the `Telemetry` RPC. Offline workers buffer them in a bounded ring and drop the oldest when it fills. Telemetry is best-effort; token usage is recorded durably in the events.
- **Head.** The head adds its own spans (API, sync, credential refresh) and exports traces, metrics and logs to the configured OTLP endpoint. Capturing prompt and response content in spans is opt-in.

## 12. Web UI (`c6-ui`)

- Leptos CSR with Tailwind, embedded into c6h with rust-embed. It talks only to `/api/v1`, using shared types from `c6-api`.
- **Views:**
  - Fleet: workers, status, sessions
  - Session: streaming transcript, thinking, collapsible tool calls, todo panel, usage and cache stats, model and effort picker, interrupt
  - File browser: tree, viewer, search, git diff
  - Terminals: tabs per worker
  - Settings: connections and OAuth, models, MCP, skills, instructions, workspaces
- **JS interop** (bundled, no CDN): xterm.js for terminals, CodeMirror 6 for viewing code (and editing later). Markdown is rendered client-side, sanitized.

## 13. Security posture (single-user, yolo)

- No login. The protection is the network: both ports bind only to the configured private or tailnet addresses, and c6h refuses to bind `0.0.0.0` unless forced with a flag. Browser-borne attacks (cross-site requests, cross-site WebSockets, DNS rebinding) are refused by `Host`/`Origin` checks (see §16).
- The worker port requires mTLS. Without an enrolled certificate, nobody can impersonate a worker or pull credentials.
- Workers never receive refresh tokens. Revoking a worker in the UI invalidates its certificate (CRL check at handshake) and stops grants to it.
- Tools run as the worker's OS user with no sandbox. Recommended: run c6w under a dedicated user account on shared machines.

## 14. Testing strategy

- **Mock LLM server.** Scripted SSE responses for all three wire formats give deterministic agent-loop and tool tests.
- **In-process integration tests.** Head and worker run in one test process, joined by a real tonic channel:
  - Kill the connection mid-turn and check that the turn completes, the journal replays, and the head's log matches the worker's exactly.
  - Restart the worker mid-turn and check for `TurnEnded{WorkerRestarted}`.
- **Provider fixtures.** Recorded real responses (redacted) guard the parsers for thinking, reasoning and tool-call streams.
- **Golden tests** for ContextBuilder output, to catch prompt changes that break cache prefixes.
- **Eval harness (M5).** A small task suite run against real models to compare tool profiles and prompts.

## 15. Milestones

| M | Scope | Done when |
|---|---|---|
| M0 | Workspace, protos, PKI and enrollment, Attach/heartbeat, fleet page | A worker enrolls and shows as online in the UI |
| M1 | OpenAIChat client, agent loop, core tools (Read/Write/Edit/Bash/Grep/Glob), journal + sync, session view with streaming | A coding task runs end-to-end on a remote worker and survives a disconnect |
| M2 | Anthropic + Responses clients, OAuth connections, caching/thinking, model registry, tool profiles, compaction | Daily use on Claude and GPT models with high cache hit rates |
| M3 | Remote fs browser, git status/diff, worktrees, terminals | IDE-like browsing and shells from the head |
| M4 | MCP, skills, subagents, web tools, background shells, OTLP export | Feature parity with the requirements |
| M5 | Hardening: chaos tests, evals, performance, UX polish | Comfortable as the only harness |

## 16. Implementation notes (M1–M5)

Decisions made while building M1 that refine the sections above:

- **Command delivery is per connection and ordered.** Durable commands (create, input) are rows in the head's `commands` table. The API only inserts a row and wakes the worker's connection task, and that one task sends pending rows in `seq` order. A session's `Create` therefore always arrives before its inputs. Interrupts are not stored; they are sent only if the worker is connected.
- **Queued input is recorded at the request boundary.** The worker appends `UserMessage` when the input actually goes into a model request, not when it arrives. Recording it on arrival would interleave it between tool calls and their results in the log. Until then the head shows the input as queued, and a worker restart simply gets it redelivered.
- **A rejected `Create` fails the session.** The session gets status `failed` with the worker's reason (for example, a missing working directory), and everything queued behind it is rejected.
- **No head↔worker read loop ever blocks on control replies.** Pongs and acks use `try_send`: acks are cumulative, and losing a pong is harmless. This prevents a deadlock when both directions' buffers fill.
- **Providers (M2).** There are three wire clients. Anthropic Messages (`anthropic`), OpenAI Responses (`openai`, and `chatgpt` with plan tokens), and Chat Completions (`openai_compat`).
  - Anthropic: adaptive thinking with `display: "summarized"` and effort via `output_config.effort`. Thinking blocks, including empty ones with only a signature, are stored and sent back unchanged. Caching uses an explicit breakpoint on the system prompt plus top-level automatic caching. Tools use `eager_input_streaming`, and the agent parses tool input strictly.
  - Responses: `store: false` with `include: ["reasoning.encrypted_content"]`. Reasoning items are kept in output order and sent back.
  - A client skips another provider's reasoning blocks.
  - Effort `none` means no reasoning: Anthropic gets `thinking: {"type": "disabled"}`, Responses `reasoning.effort: "none"`, Chat Completions `reasoning_effort: "none"`. Chat Completions sends `reasoning_effort` to every server (Ollama uses it to turn a thinking model's reasoning off); a server that answers 400/422 about it is asked again without.
- **Stop reasons.** Chat Completions' `length` and Responses' `max_output_tokens` are normalized to `max_tokens`. If a response hits `max_tokens` with tool calls, none of them run: the model gets an error result for each. A `refusal` ends the turn with an error.
- **Tool profiles (M2).** The profile is chosen from the model when the session starts and recorded in `SessionStarted.tool_profile`, so the tool list never changes during a session. All three present the same implementations:
  - Generic: read, write, edit, bash, grep, glob, update_plan.
  - Claude: Read, Write, Edit, Bash, Grep, Glob, TodoWrite, with Claude Code's parameter names.
  - Codex: shell (argv), apply_patch (Codex patch format, whole patch validated before writing), update_plan.
- **Compaction (M2) is "simple compaction".**
  - When the next request's size reaches 85% of the effective context window (connection setting, else detection capped by the built-in figure), or the provider reports a context overflow, the same model summarizes the conversation. Automatic and manual compaction use reported usage plus estimated growth since it; without a report, they estimate the whole request. It runs with the same system prompt and tools, so the cached prefix is reused. That size plus the compaction instructions decides whether the full summary fits and how much output room remains. Both summary calls also cap output to fit below the threshold beside the normal prefix and summary wrapper. If the full request will not fit, a digest with no tools uses at most half the window, reduced to leave room for its fixed prompt and output. A cut-off summary is unusable. A mechanical digest stands in if automatic compaction gets no complete summary.
  - If the fixed system prompt and tools leave no room for a useful summary, history is kept. Automatic turns send the request if it fits the window; otherwise they fail with a reason and ask for fewer tools or a larger window. Manual compaction fails with the same reason.
  - A `Compaction` event then replaces the whole history with the summary, and nothing older is sent again. This keeps the history append-only, which providers that check reasoning blocks against the history require.
- **Head→worker RPC (M3).** `RpcRequest`/`RpcResponse` on the Attach stream, with JSON payloads and an id that matches each response to its request. The methods are `fs.list`, `fs.read`, `git.status`, `git.diff`, `git.log` and `pty.*`. The worker handles each one in its own task, so a slow git call never blocks the read loop. The head's API endpoints pass requests straight through to the worker.
- **Terminals (M3).**
  - Each terminal is a `portable-pty` login shell that lives on the worker, with 1 MiB of scrollback, until its shell exits or it is closed.
  - To attach, the browser opens a WebSocket to the head. The head asks the worker (RPC `pty.attach` with a one-time token) to open a `Pty` gRPC stream back to it, then joins the two.
  - The worker sends the scrollback, then live output. It takes the scrollback snapshot and subscribes to live output under one lock, so no output is lost or duplicated between them.
- **Worktrees (M3).** `CreateSession.worktree` runs `git worktree add -b c6/<id> <state>/worktrees/<repo>/<id> HEAD` and starts the session at the same relative path inside it. `SessionStarted` records the branch and repo root. Worktrees are kept when a session is archived; removing them is manual for now.
- **Claude Code backend.** Each turn runs `claude -p --output-format stream-json` in the session's directory and maps its output onto session events (see requirements R-LLM-3).
- **Tools (M4).**
  - Every profile gains background shells (`run_in_background` plus output and kill tools), `web_fetch` (HTML converted to Markdown), and `Task`/`task` subagents (Claude and generic profiles).
  - Anthropic and OpenAI (api.openai.com) sessions also get the provider's own web search tool when their tool lists allow `web_search` / `WebSearch`. `SessionStarted.hosted_search_off` records a denial from the route, named agent or parent; permission carries across providers, reloads and hand-overs, and older records default to allowing it. `SessionStarted.tools` lists only real function tools; a parent's hosted-search permission also allows a child's external search tool. Its calls and results are stored as `Opaque` blocks and replayed verbatim; Anthropic's `pause_turn` resends the request so the provider can continue.
  - The tool set is fixed when a session starts: the profile's tools, `Skill`/`load_skill` if there are skills, then MCP tools sorted by name.
- **MCP and skills (M4).**
  - Configured on the head (settings `mcp_servers`, `skills`) and copied into each `CreateSession`.
  - MCP stdio servers run on the worker in the session directory; HTTP servers are reached from the worker. A server that fails to start shows as an error event and does not fail the session.
  - Skills come only from the head's settings (enabled ones), and the skills index goes at the end of the system prompt.
- **Subagents (M4; actors since docs/subagents.md).**
  - `Task` runs a child session on the same worker, with the same system prompt and model, the parent's tools minus `Task`, and a fresh conversation. Its final report is returned as the tool result.
  - The child is a session with its own actor (`SessionManager::start_subagent`). Its journaled spec carries `CreateSession.delegation` (parent, call, root, the named agent's tool list, depth), so a reload rebuilds the same tools (`subagent::delegated_tools`). The task goes to it as `ActorMsg::Delegate` (logged as a `UserMessage` with `delegated_by` and the subagent note in `prompt` only); the runner waits for that turn's outcome, read from the log when the turn closes. Stopping the parent's turn sends the child `Interrupt`.
  - It uses its parent's live tool hooks and policies (held calls wait in the parent), publisher, memory, notices and head caller (`SessionManager` keeps each actor's `SessionTools` and loads the parent if needed). Secrets are fetched on behalf of the root session, whose row always exists on the head. No checkpoints: subagents are not rewound.
  - The parent logs `SubagentSpawned { call_id, child_session_id, agent, description, background_task_id }` before the child starts.
  - A delegation that did not complete returns a digest from the child's log (`subagent::digest`: status, turns, tool calls, elapsed time, last message, files its own successful write/edit/patch calls changed, errors, its subagents) followed by the editable `session.subagent_partial` prompt, as an error result. `recover()` gives a foreground `Task` call left waiting by a restart that digest (`status="lost"`) before closing the turn; a lost background subagent's notice carries it too.
  - Migration 0040: `sessions.agent`, `end_reason` (the last turn's, set at ingest) and `root_id` (the top-level session of side chats and subagents); `api::Session` has `agent` and `end_reason`.
  - Continuing: `SendMessage`/`send_message` (`to`: a session id or background task id of a subagent this session started) starts a delegation in an idle subagent, or joins a running one's turn (`ActorMsg::Delegate` without a waiter) and returns at once; `BashOutput`/`bash_output` with a subagent id returns its digest. Subagents take your messages (`UserInput.wake`) and `POST /sessions/{id}/resume` (the `session.subagent_resume` prompt, `command: "resume"`). A subagent turn no call waited on ends with `ActorMsg::ChildReport` to the parent: a notice that joins a running turn, starts one with `wake` (within the hourly limit), or else waits for the parent's next input. Side chats on subagents and `Restore` of subagents (spec with `delegation`; tools from `SessionStarted.tools`) are allowed.
  - Nesting: subagents get `Task` and `SendMessage` while their depth is below `CreateSession.max_subagent_depth` (the `subagent-limits` setting, default 3); `SessionManager::working` counts a tree's running delegations against `max_running_subagents` (default 16). Cost and archiving cover the whole tree; `GET /sessions/{id}/children?tree=1` lists it.
  - UI: a `Task` call's card has the subagent's thread under it (`subagent_thread`: collapsed, a live peek of its last steps while it runs, or its transcript with Stop, Resume and a reply box with *wake*); a `SendMessage` card links to the thread. The header's ◆ list (`SubagentsPill`) shows the tree and opens threads through `ForceOpen`. A subagent's page has its lineage and a reply box. `Session.subagents_running` (top-level sessions in the live list) and `root_id` tell transcripts when to refresh their children. Find in session and search follow subagents like side chats.
  - Claude Code sessions mirror Claude Code's own subagents into read-only child sessions (`sessions.observed`, migration 0041), built from the stream's `parent_tool_use_id` traffic.
  - The child's log syncs like any session's. The head creates its row when the first event arrives (`SessionStarted.parent_session_id`). Child sessions are hidden from the main list and take no messages.
- **Telemetry (M4) differs from §11.**
  - Spans are built on the head from session events when a turn ends, rather than emitted by workers. There is one trace per session: `invoke_agent` per turn, `chat {model}` per model response (with `gen_ai.usage.*` including cache read and write), and `execute_tool {name}` per tool result.
  - Spans are exported over OTLP/HTTP protobuf to `otlp_endpoint` or `OTEL_EXPORTER_OTLP_ENDPOINT`. Turns that ran while a worker was offline are exported when they sync.
- **Browser hardening (M5).**
  - There is no login, but a web page in the user's browser can still reach the head: cross-site WebSockets need no CORS approval, simple cross-site POSTs need no preflight, and DNS rebinding can make a hostile name resolve to the head.
  - So every UI/API request must carry a `Host` the head owns: listen address, hostname, `localhost`, advertised name or `extra_names`. Anything else gets 421.
  - Any request with a foreign `Origin` gets 403, which covers terminal WebSockets.
- **Restore and journal trimming (M5).**
  - A worker that has lost a session rejects input as "unknown session" (for example after a reinstall, or because the journal trimmed it). The head then queues `Restore{spec, events}`, which carries its authoritative log, followed by a fresh copy of the input. This happens at most once a minute per session.
  - Because of that, workers trim sessions from their journal once they are fully acknowledged, not mid-turn, and idle for 7 days.
  - Messages up to 64 MiB are allowed in both directions.
- **Wildcard binds.** With `bind = ["0.0.0.0"]` (or `::`, which needs `allow_public_bind`), the head treats every interface address of that family as its own: they go into the server certificate, the advertised worker URL (tailnet first) and the Host allowlist. The dev config binds this way. The head also asks Tailscale (`tailscale status --json`) for its MagicDNS name at startup and answers to it and its short form, so `http://<machine>.<tailnet>.ts.net:7700` works without `extra_names`.
- **Live deltas are coalesced (M5).** The worker batches text, thinking and tool-output deltas over 25 ms and merges consecutive deltas from the same stream before sending.
- **Evals (M5).** `c6-eval` with tasks in `evals/tasks` (see `evals/README.md`).
- **Side chats.**
  - A side chat is a child session (`parent_id`, no `parent_call_id`) that branches after one event of its parent (`fork_seq`). It runs on the parent's worker, in the parent's real directory, with its own model.
  - The head builds its context as a `seed` in `CreateSession`: the ancestors' seed, then the parent's events up to the branch point, plus results of tool calls already made by then. The worker prepends the seed to the history and keeps it in the journaled spec, so reloads and `Restore` keep the context.
  - If the seed came from a different model, signed/encrypted reasoning and provider tool blocks are dropped from it. Claude Code sessions get the seed as a text transcript with the first question. A side chat can switch to and from Claude Code like any session (a handoff; see Model switching), unless it works over SSH.
  - The base prompt gains a note that this is a side conversation that should not change files unless asked. Side chats take messages and can have their own side chats; the UI shows them as collapsible threads under the message they branch from.
- **Model switching.**
  - A message can switch its session to another connection, model or reasoning effort (`UserInput.model`; the UI offers it in the reply box of sessions and side chats). The worker applies it at the start of the next turn, never mid-turn: it records `ModelSwitched` before the user message, then runs on the new model. The head updates the session row when it ingests the event.
  - Signed/encrypted reasoning and provider tool blocks from before the switch are dropped from the history, since only the model that wrote them accepts them. A reloaded session resumes on the last switched-to model.
  - Within a runtime, tools and system prompt stay as the session started (a stable prefix). An effort-only switch keeps the reasoning in the history.
  - **Between runtimes** (Claude Code and carbon6's own loop), a switch is a **handoff**, at the same turn boundary, in the same session and log. The runtimes share nothing native: different agent loops, tools, prompts, and Claude Code keeps its own transcript in its CLI.
    - The worker builds the destination's runtime afresh: into carbon6's loop, the tool profile of the new model, its tools and system prompt as a new session would get them (`own_runtime`: environment, a `SessionStart` hook with source `resume`, memory, task cards, skills); into Claude Code, no carbon6 tools and Claude Code's own prompt. It records them in `ModelSwitched.handoff` (`tool_profile`, `tools`, `system_prompt`, and into Claude Code a fresh `claude_session_id`), and switches policies and hooks in its tool layer on (carbon6's loop) or off (Claude Code runs the hooks itself, from `--settings`).
    - The destination gets the conversation before the handoff as a transcript (`c6_agent::handoff_context`): rebuilt from the effective events (rewinds and compactions applied), your messages, replies, and tool calls with their results (long ones shortened), wrapped in `<carbon6-handoff from="…">` with a note that it is context only and that the other runtime's reasoning, provider blocks and tool state did not carry over. At most `HANDOFF_BUDGET` (120 000) characters, the latest steps kept.
      - Into carbon6's loop: the transcript is the first user message of the history, followed by the events since the handoff as native messages (`conversation_from_log`). The other runtime's token usage no longer counts toward compaction.
      - Into Claude Code: each handoff starts a Claude Code conversation of its own (`--session-id <claude_session_id>`), whose first prompt is the transcript, then "The user now says:" and the message; later turns `--resume` it (`claude_conversation`). A session created on Claude Code still uses its own id. If a resume finds no conversation (its first turn failed early), the new one gets the transcript too. A session imported from Claude Code that carbon6 has not run yet (`continues_claude_import`) hands off `native`: it resumes Claude Code's own conversation by its id.
    - Reloads and restores read the runtime from the log: `runtime_of` applies the last handoff to `SessionStarted`, so tools, prompt and runner are the destination's.
    - Refused: Claude Code for subagents and SSH sessions (by the head, and by the worker if one gets through: it logs an error and stays), for the assistant, and on a connected worker without Claude Code (`claude_code` capability). A rewind to before the last handoff is refused (head and worker): the other runtime's conversation cannot be taken back.
    - The UI says "hands over … on send" in the reply box and marks the handoff in the transcript, with the new runtime's prompt and tools. The model picker leaves Claude Code out for SSH sessions and for subagents' default.
- **Rewind.** Before each turn's first message the worker records `Checkpoint { commit }`: a commit of the session directory in a private git repository under `<state>/checkpoints/<session>.git` (its own index; the work tree's .gitignore files plus a default exclude list apply). `Rewind { to_seq, restore_files }` (head-validated: `to_seq` must be a user message; queued like input, so an edit's new message follows it) interrupts a running turn, optionally restores the files to the last checkpoint before `to_seq` (files created since are deleted), and appends `Rewound`. `effective_events` drops a session's own events from `to_seq` on wherever history is rebuilt (side-chat seeds included, each session's part starting at its `Started`). The log stays append-only: the UI folds rewound events under the marker.
- **Attachments.** `UserInput.attachments` / `UserMessage.attachments` carry `{name, mime, data}` in the log (images: PNG/JPEG/GIF/WebP; anything else is UTF-8 text, `text/plain`). The head validates uploads (10 MB each, 20 MB per message) on `POST /sessions/{id}/messages` and on create, and serves them back at `/sessions/{id}/attachments/{seq}/{index}`; the UI API lists only name, mime and size. `@path` tokens are resolved by the worker actor when the input arrives (`mentions.rs`: files ≤256 KB inline, images ≤5 MB, folder listings ≤300 entries), so replays see exactly what the model saw. In history, images become `Block::Image` (Anthropic `image` base64 source; Chat Completions `image_url` data URL parts; Responses `input_image`) and text files `<file name="…">` blocks after the message text; search indexes text attachments. `fs.find` (worker RPC, `GET /workers/{id}/fs/find`) is the composer's fuzzy finder: gitignore-aware walk, 50k entries max.
- **Slash commands.** Custom commands are the head's `commands` setting (`c6_api::CommandDef`). When a message (or a new session's first prompt) starts with `/name` naming an enabled one, the head puts its name and template in `UserInput.command`/`command_template`, so commands apply as they are when sent. The worker actor expands the message when it arrives (`commands.rs`), before resolving @-mentions: `UserMessage.command` names the command and `UserMessage.prompt` holds what the model gets instead of the text (history, Claude Code input and transcripts use `prompt` when set). Built-ins (`compact`, `init`, `review`) live on the worker; a custom command of the same name wins. `` !`cmd` `` runs `sh -c` in the cwd (30 s, 64 KB). Unknown commands stay text. `/compact [focus]` is a turn of its own: the inbox never merges it with other input, and `compact_turn` asks the model for a summary (with the focus) and records `Compaction`; the `/compact` message itself is left out of history. `/clear` (a new session, same worker, folder and model) and `/export` (`GET /sessions/{id}/export`: Markdown of the effective events, tool calls folded) are UI-side. `GET /api/v1/commands` (built-ins from `c6_api::BUILTIN_COMMANDS` plus enabled custom ones) feeds the composer's picker. No command files are read.
- **Named agents.** The head's `agents` setting (`c6_api::AgentDef`) is copied into `CreateSession.agents_json`. When a session has enabled agents, its Task tool lists them in its description and takes `subagent_type` (an enum including `general-purpose`). The runner gives a named agent its instructions as the system prompt (plus the cwd and date), only the tools it allows (matched across profiles: `Read` = `read`, `Bash` = `shell`, `Edit` = `apply_patch`…), and its `model` when the connection is Anthropic. No agent files are read.
- **Hooks.** Stored on the head as the `hooks` setting (`GET`/`PUT /api/v1/settings/hooks`, a validated list of `c6_api::Hook`: name, event, matcher, folder, command, timeout, enabled) and copied into `CreateSession.hooks_json` like MCP servers and skills, so a session keeps the hooks it started with. No files are read. `hooks.rs` builds a `HookRunner` from the enabled hooks whose folder contains the session's cwd. `c6_tools::Hooks` (on `SessionTools`) is called by the turn loop: `pre_tool_use` before a call (a block becomes an error result), `post_tool_use` after (feedback is appended to the logged result), `stop` when the model would end the turn (feedback is logged as a `UserMessage` with `command: "stop-hook"`, at most 10 times per turn). Subagents get the parent's tool hooks but not Stop. The actor runs `UserPromptSubmit` on receipt (after command expansion and mentions): a block records the message with `UserMessage.blocked` (so the head's command completes; history skips it), context goes into `prompt`. `SessionStart` output is appended to the system prompt at creation. Hooks run in parallel per event, `sh -c` in the cwd, 60 s default timeout. Claude Code sessions instead get them converted to Claude Code settings and passed as `claude --settings`.
- **Labels.** A session's own labels are a JSON array on its row (`sessions.labels`, migration 0010). The automatic ones are derived from `worker_name` (set at creation), `repo`, `folder` and `home` (set from `SessionStarted.repo`/`cwd`/`home` at ingest) and the current `model`, by `c6_api::labels::auto_labels`. `c6_api::labels` holds the matching shared by head, worker, UI and CLI: `applies` (every selector label must match; `folder:`/`repo:` by path-segment prefix, others exactly) and `best` (among same-named items, the most labels wins, the later one on a tie). The head passes its labels (yours plus `worker:`) in `CreateSession.labels` and, with each message, `UserInput.labels`; the worker adds `repo:` (`git remote get-url origin`, normalized to host/owner/name), `folder:` and `model:`. The worker filters agents, skills and hooks when a session's tools are built. For commands the head sends every enabled variant of the name (`command_candidates_json`) and the worker picks with `best`, so label changes apply to the next message. `GET /commands?session=` lists the variants that apply; `GET /sessions/{id}/config` explains what applies and why not (`Applied.skipped`); `PUT /sessions/{id}/labels` edits yours. `GET /search?labels=` keeps sessions whose labels match (checked per candidate session, before the result limit). Side chats copy their parent's labels; subagent sessions copy them at ingest. A hook's older `folder` field acts as a `folder:` selector.
- **Background tasks.** `c6_tools::Background` holds a session's background shell commands and subagents (Task `run_in_background` spawns the subagent run and returns at once; `finish_agent` records the report). With a sink set (the worker's actor, for non-Claude Code sessions), each start and end becomes a `TaskEvent`; the actor logs a `BackgroundTask` event (state started, finished, stopped, stopped_by_user or lost) and, for ends the agent should hear about (`notify` and not stopped by the agent itself), queues a `UserMessage` with `command: "task-notification"` and an XML-ish notice. The inbox delivers it at the next model boundary of a running turn, or starts a turn in an idle session (at most `AUTO_TURNS_PER_HOUR` = 20 per hour; extra notices wait for the next user input and an `Error` event says so). `StopTask` (a direct command like `Interrupt`, `POST /sessions/{id}/tasks/{task}/stop`) kills a task as the user. The worker journal counts open tasks per session; `recover()` resumes those sessions, which mark unfinished tasks `lost`, stop the old process group if it still runs the same command (`pgid` is in the started event), and notify. The UI derives the running list from the events.
- **Secrets.** `secrets` table (migration 0011): name (an environment variable name), value encrypted under `master.key`, description, `applies_to`. `GET /settings/secrets` lists them without values; `PUT /settings/secrets/{name}` sets (value optional when updating). Each new session's spec carries the index (`secrets_json`, no values); the worker lists the applicable names in the system prompt and, at each turn's start, fetches their values as credentials named `secret:<NAME>@<session>` over the existing credential channel (head-decrypted, 5-minute expiry; from the session's realm, else root's; a secret with `worker:` selectors is refused to other workers). `SessionTools::set_secrets` puts them in the environment of bash and background commands; `Secrets::redact` replaces values (4+ chars) with `[secret:NAME]` in their output, streamed and final, and in background-task notices. Subagents share the parent's secrets.
- **SSH targets.** `CreateSession.ssh_target` (API `ssh`; `sessions.ssh_target`, migration 0012) makes a session work on another machine through its worker. `c6_tools::remote::SshHost` runs everything through OpenSSH with `BatchMode`, a shared `ControlMaster` socket under `/tmp/c6-ssh-<uid>`, and `C6_SSH_BIN` for tests. Scripts go over stdin into a self-deleting 0600 temp file run by bash (or sh) with stdin from /dev/null, so secrets never reach a command line; scripts record their process group in a pid file so a timeout, an interrupt or a stopped background task kills it on the target. `SessionTools::with_ssh` routes bash (cwd tracked with a marked line), background commands, read/write/edit/apply_patch (`cat`, `mkdir -p && cat >`, `stat`) and grep (`rg`, else `grep -E`) and glob (`find` listed remotely, matched locally) to the target; `~` is the remote home. The worker's `create_remote` probes the target (home, folder, OS line, git remote for `repo:`), writes a remote environment section and the target's AGENTS.md/CLAUDE.md into the system prompt, and records `SessionStarted.ssh_target`/`system`; hooks and MCP servers run in a local `scratch/<session>` folder; checkpoints and @-mentions are skipped. `pty.open` takes `ssh` for a login shell on the target. Labels gain `host:<host>`.
- **Web search.** The head's `web_search` setting (`c6_api::WebSearchSetting`: provider brave/tavily/searxng, URL, the secret holding the key, `always`) is copied into `CreateSession.web_search_json`. `build_tools` adds `c6_tools::WebSearch` (`web_search` / `WebSearch`) when a provider is set and the model's provider has no server-side search (Chat Completions), or always if asked. The tool reads its key from the session's secrets (so the secret must apply to the session), calls the provider's JSON API, and returns numbered title/URL/snippet results.
- **Outputs.** `c6_tools::Publish` (`publish_file` / `PublishFile`, in every tool set) reads the file (locally or on the SSH target, up to 20 MB) and sends it to the session's publisher (subagents use the parent's); the actor logs a `SessionEvent::Output { name, title, mime, data }`, so each publish is a version with its content as it was. The API lists outputs without content (`EventKind::Output { size }`) and serves them at `GET /sessions/{id}/outputs/{seq}` (`?download=1`), with `nosniff` and, for anything that could run script (HTML, SVG, unknown types), `Content-Security-Policy: sandbox …` so a page the agent wrote gets an opaque origin and cannot use your session; the UI previews those in sandboxed iframes. The Outputs tab groups versions by name.
- **Templates.** The head's `templates` setting (`c6_api::Template`). `templates::run` resolves the targets (named workers, `*` for every online worker, or the first online one; times each SSH target), the connection (name or id) and model, fills the prompt (`{{input}}`, `{{target}}`, `{{date}}`, `{{time}}` in the head's local time), and creates one session per target through `sessions::create`, each labelled with the template's labels plus `template:<name>` and `run:<id>` (an 8-hex run id). `POST /templates/{name}/run` returns the sessions and per-target errors. A template with `agents` can also be started by a session's agent as a remote subagent (`Task` with `template`; docs/remote-subagents.md).
- **Automations.** `automations` table (migration 0013): a template plus a trigger, `schedule` (`cron::Schedule`: five fields with lists, ranges, steps and names, `@daily`/`@hourly`/…, dom/dow OR-ed as in cron, evaluated with `chrono::Local`) or `webhook` (a 24-byte token, stored as SHA-256, shown once; `POST /api/v1/webhooks/{id}/{token}` is exempt from sign-in, compares in constant time, answers 404 to a wrong or disabled token, takes up to 1 MB, pretty-prints JSON into `{{input}}`). A head task wakes every 30 s and fires each enabled schedule with a time in (last check, now]; missed times are not replayed. `fire` runs the template with the label `automation:<name>` and records last run, run id and error. `/api/v1/automations` lists, creates, edits, runs now, rotates a webhook token and deletes.
- **Connectors.** `c6_mcp::ServerConfig` has `applies_to` and, for HTTP servers, `bearer`; `with_secrets` fills `${NAME}` in command, args, env, URL and bearer from the session's secrets, reporting names it lacks. The worker fetches the applicable secret values before building a session's tools (create and resume) and starts only servers that apply and have their secrets; the rest become setup errors in the session. Commands from the head are applied in order by a task of their own, so the connection's read loop keeps going (a create that waits on a credential would otherwise wait on itself). The UI's MCP editor offers presets (GitHub, Slack, PostgreSQL, Playwright browser, remote HTTP).
- **Memory.** `memories` table (migration 0014: id, scope label, text, writing session). New sessions get the newest 500 in `CreateSession.memories_json`; the worker keeps those whose scope matches the session's labels (or is empty) and writes a Memory section into the system prompt (most specific first, newest first, 8 KB), naming the session's labels as scopes. `c6_tools::Remember`/`Forget` send changes to the actor, which logs `SessionEvent::Memory { op, id, scope, text }`; the head applies them to the table when it ingests the event. `/api/v1/memories` lists, creates, edits and deletes.
- **Policies and approvals.** The head's `policies` setting (`c6_api::Policy`: name, `applies_to`, tool-name regex, input regex, `ask`/`deny`, message; regexes checked on save) goes into `CreateSession.policies_json`. The worker's `policy::Policies` (the applicable ones) runs first in a `Chain` with the settings hooks, as `Hooks::pre_tool_use`, which now gets the call id. A denial is the tool's error result. `ask` logs `SessionEvent::Approval { state: "requested", tool, input (4 KB), policy, message }` and waits on a oneshot for up to an hour (the turn loop's interrupt ends the wait; a guard logs `expired`). `POST /sessions/{id}/approvals/{call_id}` sends the direct `Approve` command; the actor resolves the wait and logs `allowed`/`denied`. The head counts open requests in `sessions.awaiting` (migration 0015; reset at `TurnEnded`) and exposes `Session.awaiting_approval`, which the UI uses for the inbox section, the badge, the dock, notifications and whether the transcript card shows its buttons.
- **Memex (phase 6, step 1).** `c6m` (crate `c6-memex`) is a fabric node: it is a `c6_worker::Worker` with an `Extension` that adds the capability `memex` and handles `memex.*` RPCs. The head records a node's role (`workers.role`, migration 0022) at its first attach, from its Hello, and refuses a later change, a second memex, and sessions on the memex. Nodes call the head with `HeadCall`/`HeadCallReply` (hub.proto); `fabric::handle` allows the memex `config.get` and `notify`, and workers the session methods (`memex.search/get/related/note/timeline`) for one of their own sessions, stamping that session's scope (`memex::scope_for`: the default kinds plus `memex_scopes` rules matching its labels) and author before relaying to the memex with `registry.call`. The head feeds the memex through `memex_outbox` (in order, retried, dropped after 5 refusals): `memex::on_events` (from `ingest`) sends outputs, web pages from `web_fetch`/`browser` results, memory changes, and on each top-level turn end a session digest (per turn: asked, tool calls in one line each, the answer; the latest 16 000 characters); `memory::put`/`delete` and, once, all earlier notes too. `sessions::create` asks for `memex.brief` (labels known at creation plus the prompt; 4 s at most) and puts it in `CreateSession.memex_brief`; the worker uses it instead of the Memory section, and offers the memex tools when `memex_tools` (a memex was connected at creation). The memex's store (`memex.db`): `items` (unique by source and external id), `parts` (about 1200-character passages, FTS5 with porter stemming), `vectors` (unit f32 per part and embedding space), `things` (keyed by the label they stand for; `run:`, `wfrun:` and `model:` make none), `links` (`about`, observed, by source) and `evidence`. Search merges keyword (bm25, all words else any) and cosine (brute force over the space's vectors) rankings by reciprocal rank (k = 60) per passage, then per item. A job loop refreshes its settings from the head each minute and embeds passages in batches of 32 through the configured connection's `/embeddings` (credentials as a worker gets them). `GET /memex/*` serves the UI with no scope.
- **Memex documents and papers (step 2).** `docs::extract` reads PDF (`pdf-extract`, plus the `/Title` from the document information), .docx (the runs of `word/document.xml`), HTML (`htmd`) and text. `docs::analyze` takes a PDF as a paper when it has an abstract and a reference list, or a DOI or arXiv id before its references. It takes the title (metadata, else the first line that reads like one), the authors (name-like parts of the lines before the abstract, without affiliation words), and the references (split on `[n]` or `n.` markers, else blank lines; each with its DOI, arXiv id, and a title from quotes or the first sentence after the authors). `Memex::add_document` stores the file in content-addressed `blobs/` (uploads; folder files are read in place), the item (`paper` or `document`; `meta.paper` holds the identifiers), and links made by `docs`: `is` to the paper's own thing (keyed `paper:doi:…`, `paper:arxiv:…` or `paper:title:…`), `authored_by` to `person:name:…` things, and `cites` to the cited papers' things with the reference text as evidence. Thing cards and related things now follow every relation. Watched folders (`memex_folders` setting, sent in `config.get`) are scanned every 5 minutes and when they change. The `files` table (migration 0002) records each file's mtime and size, so only changes are read, and files not seen in a scan are removed. Trails: `trails` and `trail_steps`, `memex.trail*` methods, `/memex/trails` routes, and the `memex_trail` tool (sessions may append and read). `GET /memex/items/{id}/content` serves an original like an output (nosniff, sandboxed when it could run script). Upload bodies may be 80 MB. Errors the memex returns become 400s; losing the connection stays a 500.
- **Memex Google (step 3).**
  - **Head:** `google.rs` holds your OAuth client (the `google_client` setting, its secret encrypted) and `google_accounts` (migration 0023; refresh and access tokens encrypted).
    - `POST /google/connect` returns the consent URL (offline access, the gmail.modify, gmail.send, calendar and contacts scopes) with a one-use state. `GET /auth/google/callback` trades the code and reads the account's address.
    - `sessions::credential` serves `google:<account>` (refreshed on the head) only to the memex node.
    - `config.get` lists the accounts, the People API base and `contacts_auto`.
    - Endpoints can be overridden with `C6_GOOGLE_*` (tests).
  - **Memex mail** (migration 0003):
    - `sync_state` per account and kind.
    - Gmail: `history.list` from the cursor each round (a 404 starts the import again), then one page of `messages.list` (100, `includeSpamTrash`) while importing. Each message is fetched in raw format, parsed with `mail-parser`, kept as a blob, and stored as an `email` item (`spam` when labelled SPAM, out of sessions' reach) plus a `mail` row (the label flags) and `mail_labels`.
    - Links `from`, `to` and `cc` go to `person:<address>` things (`google::person` adds `member_of` to `org:<domain>` except for free-mail domains). `thing_keys` lets a person have several addresses.
    - Views group by thread with their conditions in SQL; `needs_reply` and `waiting` check each thread's last message.
    - `modify` uses `threads.modify`. Drafts and sending build MIME with `mail-builder` (Markdown as HTML via `pulldown-cmark`, plus In-Reply-To and References). `outgoing` holds a message until `send_at` (10 s for undo, or a time you chose) and `send_due` sends it with `messages.send`.
    - New INBOX mail in the live phase is reported to the head as a `mail.received` event, which `automations::on_memex_event` matches against `memex` triggers (`EventFilter`: type plus `from:`/`to:`/`subject:`/`account:` conditions; `@domain` matches an address's domain).
  - **Memex calendar:**
    - `calendarList` and `events.list` (showDeleted, sync tokens; a 410 starts over) fill `calendars` and `events` (masters with `recurrence`, and instances with `recurring_event_id`/`original_start`, cancelled ones kept to hide them) and `event` items linked to `attendee` people.
    - `calendar::events` expands masters with `rrule` in their time zone and drops instances that have an exception row.
    - Changes: "this" patches the instance id `<id>_<UTC start>`; "following" sets UNTIL on the master and inserts a new series; "all" patches the master. RSVP patches the attendee list.
  - **Memex contacts:** `people.connections.list` with a sync token fills `contacts`, each linked to one person thing keyed by all its addresses. Candidates are people in mail you sent to and heard from, and `createContact` files them in the `carbon6` group (made on first use).
  - **Sessions:**
    - `CreateSession.memex_mail` and `memex_calendar` (scope has email or event) add the mail and calendar tools. `mail_thread` wraps messages as `<email …>` and marks them untrusted.
    - `fabric` allows these methods for sessions, and the memex refuses them outside the session's scope.
    - `with_builtin_policies` puts an `ask` policy for `mail_send`, `calendar_create` and `calendar_rsvp` ahead of yours unless `send_needs_approval` is off.
  - **UI:** calls go through `POST /memex/rpc` (allowlisted prefixes, for you); attachments download from `/mail/attachment` (nosniff, sandboxed).
- **Workflows.** The `workflows` setting (`c6_api::Workflow` of `WorkflowStep`s: run/approve/notify, template, input, title, message, `when` = `StepCondition` on an earlier step, `continue_on_failure`; checked on save: templates exist, ids unique, conditions point back, no name shared with a template). `workflows::start` stores a `workflow_runs` row (migration 0020) with the definition as it was and the steps' state (`WorkflowStepRun` JSON), and kicks `workflows::Runner`, whose dispatcher (one driver per run; unfinished runs are kicked at start) runs `drive`: the first pending/running/waiting step; `when` is checked when a step starts (`changed` compares `RESULT:` lines with the step's output in the workflow's previous finished run); a run step calls `templates::run` (labels `workflow:<name>`, `wfrun:<id>`) and registers its sessions in `run_sessions` under `wf:<run>:<step>`, then polls (1 s, up to 6 h) until `automations::session_ended` has set all their outcomes, and takes their last replies as output; approve sets the run `waiting` and sends an `approval` note whose signed link uses `wf:<run>` as the session and the step id as the call (the `/approve` page handles both); `decide` resumes or ends the run (`denied`). `finish` notifies on failure and calls `automations::workflow_finished`, so an automation whose template names a workflow records the workflow's outcome.
- **Browser.** `c6_tools::browser`: `find_chrome` (`C6_CHROME`, the usual macOS apps, then Chrome/Chromium/Brave/Edge on `PATH`) decides whether `build_tools` adds `BrowserTool`. `SessionTools.browser` (a `BrowserSlot`) launches Chromium on first use (`--headless=new --remote-debugging-port=0 --user-data-dir=<state>/browser/<session>`, `kill_on_drop`), reads `DevToolsActivePort`, and speaks CDP over one WebSocket (`tokio-tungstenite`; a writer task and a reader task matching replies to ids; 30 s per call) to one page attached with `flatten`. `read` runs a script that numbers visible interactive elements with `data-c6-ref` and returns `innerText` (30 000 characters); `click` scrolls the element into view and dispatches mouse events at its center; `type` focuses and selects, then `Input.insertText` (a secret's value from the session's secrets); navigations wait for `readyState` complete. Outputs are redacted of secrets. Screenshots (`Page.captureScreenshot`) go to the publisher. A watcher drops the browser after 10 idle minutes; `SessionManager::new` prunes profiles older than 30 days. The tool is `Exclusive`.
- **Web push.** `push.rs`: RFC 8291 aes128gcm (one record: ephemeral ECDH P-256 with `ring::agreement`, HKDF-SHA256 for the CEK and nonce, AES-128-GCM; `seal` checked against the RFC's Appendix A) and RFC 8292 VAPID (an ES256 JWT for the endpoint's origin, 12 h, `sub` the https public URL or a placeholder; the P-256 key is made once, kept in the `vapid_key` setting as PKCS#8). `push_subscriptions` (migration 0021) holds each device's endpoint (https, or http on loopback for tests), p256dh and auth; `POST /push/subscriptions` takes `PushSubscription.toJSON()`. Channel kind `push` sends `{title, body, url, tag}` (url: the approval link, else the session) to every device with `TTL` and `Urgency`; 404/410 removes the device. The UI ships `sw.js` (show the notification; on click, focus or open its URL), `manifest.webmanifest` and icons (Trunk `copy-file` to the root), and `js/push.js` (permission, `pushManager.subscribe` with the head's key).
- **Notifications.** The head's `channels` setting (`c6_api::Channel`: name, kind ntfy/slack/discord/webhook/email, URL or SMTP server, user, recipient, secret name, events, `applies_to`, enabled; checked on save) and `public_url`. `notify::Notifier` is an unbounded queue in `HeadState`; one task sends notes in order, loading channels each time and keeping those whose events and labels match. `sessions::notify_events` (in `ingest`) makes notes for approval requests, `Notice` events (`SessionEvent` kind 26, from the `notify` tool through a `NoticeSink` like memory's), outputs, and the end of top-level turns (completed: the last reply, from `turn_events`; error: the last `Error`); `automations::fire` adds one when a run fails. ntfy posts JSON to the server root (topic, title, priority 2/3/5, click, view actions); Slack and Discord get text; webhooks get the note as JSON (with `session_url`, `approve_url`, labels); email goes through lettre (465: TLS, else STARTTLS). Approval links are `/approve/<b64 session>.<b64 call>.<expiry>.<HMAC-SHA256>` with a random key in the `link_key` setting and a one-hour expiry; `GET` (public: outside `/api/`) shows the request from its `Approval` event and `POST allow=1|0` calls `sessions::decide` if the call still waits.
- **Automation runs.** Migration 0019: `automation_runs` (run id, automation, started/finished, trigger, outcome running/ok/failed/stopped, error) and `run_sessions` (session → run, outcome of its first turn), plus `automations.disable_after`. `automations::fire(…, trigger)` records the run (failed at once if nothing started) and its sessions. `sessions::ingest` calls `automations::session_ended` on each `TurnEnded` (completed → ok, error → failed, else stopped; only the first sets it) and `on_rejected` does for sessions that failed to start; when every session has an outcome the run finishes (failed if any failed or some targets did not start), notifies `automation` on failure, and `after_failure` switches the automation off once its last `disable_after` finished runs all failed. `GET /automations/{id}/runs` adds each run's sessions and cost (`usage::cost_of`, subagents and side chats included); `Automation.recent` has the last ten outcomes.
- **Hosts.** The `hosts` setting (`c6_api::Host`: name, target, worker, labels, description, `check_every` minutes, enabled; checked on save, labels as yours). `sessions::create` resolves an SSH target that is a host's name or target (`hosts::lookup`) to the target and adds the host's labels. `Template.hosts` is a selector over each host's labels plus `host:<name>` (`*`: all); `templates::run` makes one target per enabled match, on the host's worker if online (else the template's first), passing the host name as the SSH target. `POST /hosts/{name}/check` and the 30-second scheduler (`hosts::spawn_checks`, one check per host at a time) call the worker's `ssh.check` RPC (`SshHost::probe("~")` capped at 20 s) and store the result in `host_checks` (migration 0018); a change in reachability (or a first failure) sends a `host` note. `GET /hosts` joins hosts with their last check.
- **Usage and budgets.** The `usage` table (migration 0017: session, seq, ts, connection, model, input, output, cache read/write) gets a row per `Usage` event in `Store::ingest`'s transaction (connection and, if the event has none, model from the session row); `usage::spawn_backfill` fills it once from older events (keyset pages of 2000, setting `usage_backfilled`). Token counts are as `c6-llm` normalizes them (input excludes cache reads; output includes reasoning). `prices` (`c6_api::Price`; exact id, else the longest `prefix*`; numbers or numeric strings) give the cost; `GET /usage?days&labels` aggregates calls since local midnight `days-1` days ago, filtered by `labels::applies` on the session's labels, by day, model, connection, label (yours except run:, plus host:, worker:, repo:) and root session. `budgets` (`c6_api::Budget`: labels, day/week/month in local time, limit, notify/block) are checked after each batch with usage (`usage::after_calls`): a budget whose period spend reaches its limit is recorded in `budget_state` (name → period) so it fires once, sends a `budget` note and, for block, interrupts running sessions with its labels. `sessions::create` (on the labels known then) and `send_input` refuse work while a blocking budget that applies is used up.
- **Model routing.** The head's `routes` setting (`c6_api::Route`: name, `applies_to`, connection name or id, model, effort, `subagent_model`, `tools`, enabled; checked on save) is read when a session is created. `api::CreateSession.connection_id` and `model` may be empty: `sessions::route` keeps a given model; otherwise it takes the first enabled rule whose labels match the session's labels at creation (yours plus `worker:`, `folder:` for an absolute folder and `host:`; `repo:` and `model:` are not known yet) and whose connection is the requested one or unset; then the connection's `default_model` (connection fields and context windows: docs/connections.md), then `Store::latest_model` (the latest top-level session's). Without a connection, the latest session's is used, else the first. The rule's subagent model is stored in `sessions.subagent_model` (migration 0016; side chats inherit it) and sent as `CreateSession.subagent_model`; `subagent::Runner` runs subagents on it (a named agent's model overrides it) and records the model in the child's `SessionStarted`, which the head uses for the child's row. A selector `key:` with no value matches any label with that key. Templates send an empty connection and model unless they set them, and `c6 run` sends what was given.
- **Route tool lists.** `Route.tools` (empty: all) is stored as JSON in `sessions.tools` (migration 0054), copied to side chats and sent as `CreateSession.tools` (field 28). The list is fixed for the session: rule edits do not change running or resumed sessions. The worker filters at the end of `build_tools`, after built-ins and MCP discovery, for creation, journal reload, a hand-over from Claude Code and subagents of another profile. `c6_tools::tool_allowed` shares named agents' profile name matching; MCP names match in full, including case. Validation gets built-in names from the same assembly with all optional services represented, across Generic, Claude and Codex; MCP names are checked by shape. Subagents inherit the list and named agents narrow it further. The assistant and Claude Code's own tools are unaffected; carbon6 subagents of Claude Code parents and a hand-over into carbon6 still apply the saved list. Tool-dependent prompt guidance is omitted when its tools are absent; unrestricted sessions retain their prompt bytes. See [tool lists](subagents.md#tool-lists-from-routing-rules) for configuration and custom prompts. Both the gRPC Create/Restore messages and the worker journal's spec blobs use protobuf: an older worker skips unknown field 28 and continues sending all tools.
- **API tokens and `c6`.** `api_tokens` (migration 0009) holds SHA-256 hashes of `c6_…` tokens; `auth::require` accepts `Authorization: Bearer` for every `/api/` path except `/api/v1/auth/*` (so a token cannot mint login codes, tokens or passkeys). Tokens are created under Settings or offline with `c6h api-token`. `bins/c6` is a thin client over the HTTP API: `run` creates a session (picking the only online worker and the latest session's connection and model by default), then follows it by polling `/events?after=` until the session is idle with nothing queued, mapping the last `TurnEnded` reason to the exit code; Ctrl-C sends an interrupt.
- **Workspace UI.** `c6-ui/src/workspace.rs`: a `Layout` of tiles in up to four columns (each tile keeps its column, `stack`: new tiles fill columns left to right, then stack in the column with the fewest; dragging a tile's bar onto another tile's top or bottom half moves it there, onto its left or right fifth makes a new column), each a framed card of at most two panes (`Column`s: what it was opened on and the item opened from it; `View`: sessions, session with an optional search target, search, workers, worker, settings, inbox, mission…), with its own back/forward history, a cursor (the pane a phone shows) and an active pane (keyboard shortcuts). One rule for `open`: ⌘-click (`How::NewTile`) or ⧉ (`detach`, which moves the item) makes a tile; the Apps menu (`How::OwnTile`) opens an app in a tile of its own (the one showing it, or a new one); from inside a tile, the item goes in its right pane; from the header's other links, it replaces the focused tile. Views are scoped to their pane (`ColumnCtx`): their own scroll (`ScrollBox`, which comes back at the end if it was there; with `follow`, a session's transcript keeps to the end as it grows or renders late until you scroll up), a session's transcript as a window (`Feed`, `Window`: the stream's `tail` sends the last few hundred events from one of your messages, and before them only the small events totals and the model need; the replay is drawn at once; scrolling up loads earlier turns with `events?before=` and keeps your place, back at the end it is trimmed again; a jump back to a search hit draws from there, with later messages as you scroll down), deep links from props rather than the URL, key handlers only in the active pane, find-bar highlights per pane root. Saved in localStorage (`c6.workspace.v2`); the URL is `replaceState`d to the focused pane and parsed on load. Attention: running → idle/failed while not shown flags a session (cleared when shown, dismissed or replied to); pins; both feed the dock, the inbox and mission control (`activity.rs`).
- **Search.** An FTS5 table (`search`, trigram tokens, so terms match as case-insensitive substrings of 3+ characters) beside `events`. Each event becomes entries of a kind (user, assistant, thinking, tool_call with its string arguments unescaped, tool_result, error, summary), capped at 32 KB, inserted in the ingest transaction, so the index cannot drift from the log. It is derived data: when `search.version` differs, a background task re-indexes each session in one transaction (delete then insert), so concurrent ingest neither loses nor doubles entries. `GET /api/v1/search` ranks with BM25, groups by session (best three matches each, snippets from `highlight()` cropped around the first match) and filters by kind and worker. Each hit carries the id of the transcript element showing it (`{session}-e{seq}`, or `call-{id}` for tool output, which shows in its call's card) and, for side chats, the top-level session and the chain of side chats to expand. Links are `/sessions/{top}?q=…&open={side chats}#{element}`: the page expands those side chats as they load, then opens the element, scrolls to it and highlights the terms with the CSS Custom Highlight API. Find in session (`GET /api/v1/sessions/{id}/matches?q=`) returns every match in the session and its side chats (a recursive query over `parent_id`/`fork_seq`) in transcript order: each side chat sorts right after the event it branches from, by creation time; one entry per element.
- **Sign-in.** The UI and API require a session (an HttpOnly cookie holding a random token; the head stores its SHA-256, 90 days sliding). Sessions come from passkeys or one-time login links.
  - **Login links:** `c6h login-link` writes a token (15 minutes, single use) into the database and prints URLs; the page redeems it with a POST, so link previews cannot use it up. Links are for the first sign-in and for recovery.
  - **One-time codes:** a signed-in browser creates one (`POST /api/v1/auth/codes`): 8 Crockford base32 characters (about 10¹² values), shown as `XXXX-XXXX`, valid for 10 minutes, single use, stored hashed beside login links (`kind = 'code'`). They are redeemed only at `/api/v1/auth/code` (never as link tokens), and five wrong codes cancel every outstanding code.
  - **Passkeys:** discoverable credentials with user verification, no attestation. Registration needs a session and takes the browser's `getPublicKey()` (SPKI), so the head needs no CBOR; sign-in verifies the assertion with `ring` (ES256, EdDSA, RS256). The RP id is the host name, so passkeys work on HTTPS names and on `localhost`, not on IPs.
  - **HTTPS:** `ui_tls_head_ca` or `ui_tls_cert`/`ui_tls_key`. Publicly trusted certificates (Let's Encrypt, `tailscale cert`) are not used: Certificate Transparency would publish the head's names. The head CA (`pki/ui-ca.*`) is separate from the worker CA and carries critical name constraints: the DNS names it was created with and private IP ranges. It issues a 397-day leaf at every start; names added later are left out of the leaf with a warning (delete `ui-ca.*` and re-trust to extend). Devices fetch it at `/ca.pem` or `/ca.mobileconfig` (public, also over plain HTTP). TLS and HTTP share the UI port (first byte 0x16); plain HTTP redirects to the HTTPS name except the CA downloads and the ChatGPT OAuth callback, which must be `http://127.0.0.1`. Over HTTP/2 the host comes from `:authority`.
  - Public without a session: static assets, `/api/v1/auth/{status,link,logout,passkeys/login/*}` and the OAuth callback (protected by its `state`). Workers are unaffected (mTLS).
- **Chat Completions quirks.** An assistant message with no text and no tool calls is sent with `content: ""`, because Ollama rejects `null` there. Reasoning is read from either `reasoning_content` or `reasoning`.

## 17. Open items to settle during implementation

- Exact Codex and Claude Code tool schemas and descriptions, to mirror per tool profile. Get them from public sources and evals.
- How to render a long-running session with many subagents in the UI.
- Whether the full event payload or a compact form is stored for `ToolCall.input_json` on very large edits (blob-backed if above N KB).
- Retention policy for blobs and archived sessions.

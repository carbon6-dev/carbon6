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
    c6-agent/             agent loop, context assembly, compaction, subagents, Tool trait
    c6-tools/             built-in tools
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
  AssistantMessage { turn_id, blocks[], stop_reason, provider_opaque? }
  ToolCall         { call_id, tool, input_json }
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

**`CredentialGrant{connection_id, auth: ApiKey|Bearer, base_url, extra_headers, expires_at}`.**
- API keys are long-lived. Workers keep them in memory only.
- OAuth connections (ChatGPT, Claude subscription): the head alone refreshes them, and does so before access tokens expire. It pushes new grants to every connected worker that uses the connection. Workers never see refresh tokens.
- An offline worker keeps working until its cached access token expires.

**OAuth login in the UI.** These are PKCE flows. Some providers redirect to a `localhost` URL, which doesn't reach a head that isn't on your machine. For those, the UI asks you to paste the final redirect URL (or the code) back into the UI. Subscription connection types are compiled behind the `subscription-auth` cargo feature (D-8).

### 6.3 HTTP API (sketch)

```
GET    /api/v1/workers                         POST /api/v1/workers/enrollments
GET    /api/v1/workspaces   POST/PATCH/DELETE ...
GET    /api/v1/sessions?worker=&status=        POST /api/v1/sessions
GET    /api/v1/sessions/{id}                   GET  /api/v1/sessions/{id}/events?after=SEQ
POST   /api/v1/sessions/{id}/messages          POST /api/v1/sessions/{id}/interrupt
PATCH  /api/v1/sessions/{id}                   (title, model, archive)
GET    /api/v1/workers/{id}/fs/list?path=      GET  /api/v1/workers/{id}/fs/read?path=&range=
GET    /api/v1/workers/{id}/fs/search?q=       GET  /api/v1/workers/{id}/git/{status|diff|log}?path=
POST   /api/v1/workers/{id}/terminals          DELETE /api/v1/terminals/{id}
GET/PUT /api/v1/config/...                     /api/v1/connections, /api/v1/connections/{id}/oauth/*
GET    /api/v1/blobs/{hash}
WS     /api/v1/sessions/{id}/stream            durable + ephemeral events, resumes from ?after=SEQ
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
- **Model registry.** For each model: context window, max output, thinking/effort support, caching style, image support, pricing, and **tool profile**. Built-in defaults ship with c6w, and the head can override them.

### 8.3 Cache and reasoning optimizations

- **Stable prefix discipline.**
  - The order is tools → system → instructions → history. Tool lists are sorted deterministically.
  - Volatile context (date, git status) goes in the first user message, not the system prompt.
  - MCP tool schemas are frozen for the life of a session.
- **Anthropic.** `cache_control` breakpoints go on the tools and system block, plus a rolling breakpoint on the latest message (and the one before it, staying within the 4-breakpoint limit). Thinking blocks are kept, with their signatures, across tool-use loops. Extended or adaptive thinking and effort are set per model.
- **OpenAI Responses.** `store: false` plus `include: ["reasoning.encrypted_content"]`, so reasoning carries over statelessly. That way any worker can rebuild any session. `prompt_cache_key = session_id`, and `reasoning.effort` is set per session.
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
  - Triggered automatically at about 85% of the context window, manually from the UI, or by a context-length error.
  - The model is asked to summarize, and a `Compaction{summary, replaces_through_seq}` event is appended. Later requests use the summary plus the history after it.
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

- No login. The protection is the network: both ports bind only to the configured private or tailnet addresses, and c6h refuses to bind `0.0.0.0` unless forced with a flag.
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

## 16. Open items to settle during implementation

- Exact Codex and Claude Code tool schemas and descriptions, to mirror per tool profile. Get them from public sources and evals.
- How to render a long-running session with many subagents in the UI.
- Whether the full event payload or a compact form is stored for `ToolCall.input_json` on very large edits (blob-backed if above N KB).
- Retention policy for blobs and archived sessions.

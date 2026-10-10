# Carbon6 — Requirements

Status: draft v0.2 (2026-10-09)

## 1. Overview

Carbon6 is an AI coding-agent harness, written in Rust, for one developer working on several machines at once. It has two parts:

| Binary | Role |
|--------|------|
| `c6h` (head) | Runs on one central machine. Holds all metadata, configuration, credentials and session data. Serves the web UI. It is the only thing the user interacts with. |
| `c6w` (worker) | Runs headless on each dev node (macOS, Linux). Connects to the head, receives prompts, runs agent turns (LLM calls and tool calls) on that node, and streams session events back to the head. |

**MVP goal:** reach working parity with Codex, Claude Code and Pi, good enough to use every day. Differentiating features come later.
**Future goal:** a platform for agentic automation, so that many tasks run without anyone starting them by hand.

## 2. Deployment and networking

- R-NET-1: Workers reach the head over private IPs (an intranet or a tailnet such as Tailscale). The head is never exposed to the public internet.
- R-NET-2: Workers start the connection to the head; the head never connects to a worker. Workers therefore need no open inbound ports.
- R-NET-3: The worker↔head wire protocol must be compact, fast and low-latency. It uses one long-lived multiplexed connection per worker that carries session events, control messages and remote-file RPCs.
- R-NET-4: Each worker is authenticated to the head (enrollment token → per-worker identity; mTLS or an equivalent).
- R-NET-6: Transport is gRPC (tonic) with bidirectional protobuf streams over one HTTP/2 connection per worker.
- R-NET-5: The protocol carries a version handshake from day one, so that mixed versions can be detected and, later, upgrades can be managed.

## 3. Head (`c6h`)

### 3.1 Data ownership
- R-HEAD-1: The head is the system of record for all metadata: workers, workspaces, sessions, LLM connections, credentials, settings.
- R-HEAD-1a: All configuration lives on the head: model and connection settings, MCP server definitions, skills, global instructions, worker and workspace settings. Workers hold no authoritative state except the project files they work on. The project files include repo-level `AGENTS.md`/`CLAUDE.md`, which are read from the worktree as they are today.
- R-HEAD-1b: Workers may keep a non-authoritative cache of config, credentials and unsynced events so they can operate while disconnected. The head's copy always wins.
- R-HEAD-1c: Storage is SQLite, accessed through a storage trait so Postgres can be added later.
- R-HEAD-2: The head persists and owns all session data (transcripts, tool calls and results, token usage, attachments).
- R-HEAD-3: The head provides a service API that workers use to talk to it (registration, credential fetch, event upload, configuration).
- R-HEAD-4: The head provides an API for the web UI. It should be clean enough that it can later serve automations and scripts (future goal).

### 3.2 LLM connections
The head is where LLM connections are configured. Supported connection types:
- R-LLM-1: OpenAI-compatible API providers (OpenAI, local LLM servers such as vLLM, Ollama and llama.cpp, gateways such as OpenRouter). Both Chat Completions and the Responses API.
- R-LLM-2: OpenAI (ChatGPT) plan via OpenAI's official "Sign in with ChatGPT" for open-source apps (PKCE, dynamic client registration, Responses API).
- R-LLM-3: Claude subscription. **Not implemented as OAuth:** Anthropic's terms (since February 2026) allow Claude Free/Pro/Max OAuth tokens only in Claude Code and claude.ai, and they enforce this server-side. Supported instead through the **Claude Code backend** (`claude_code` connection kind): each turn runs the official `claude` CLI headless on the worker (`claude -p --output-format stream-json`), signed in with the user's own account. Claude Code runs its own agent loop and tools and keeps its own transcript, resumed by the carbon6 session id. carbon6 maps its stream onto session events. Claude via an API key is also supported.
- R-LLM-4: Model calls are made **directly from the worker**, using credentials held by the head (see §5.2).
- R-LLM-5: Use every conversation optimization the provider offers: prompt caching (cache breakpoints and stable prefixes), extended or adaptive thinking with signature passthrough, reasoning-effort controls, encrypted reasoning carry-over (Responses API), streaming, parallel tool calls.
- R-LLM-6: Model and effort can be chosen per session and changed mid-session.

### 3.3 Web UI
- R-UI-1: Web UI built with Leptos and Tailwind, served by the head.
- R-UI-1a: Single user, no login. Access control comes from the network: by default the head binds only to private or tailnet interfaces, never to `0.0.0.0`.
- R-UI-2: A fleet view: workers, their status (online, offline, syncing) and the sessions running on each.
- R-UI-3: A session view: streaming transcript, thinking, tool calls and outputs, token and cost usage, and stop/interrupt.
- R-UI-4: Start a session on any worker, in a chosen workspace or directory.
- R-UI-5: IDE-like remote browsing of a worker's files: file tree, file viewer with syntax highlighting, Markdown rendering, search, git diff view.
- R-UI-6: Read-only access to session history when the worker is offline.
- R-UI-7: Interactive terminals (PTYs) on any online worker, opened in a workspace or directory, rendered in the browser (xterm.js-class) and relayed over the worker connection. Multiple terminals per worker; terminals may be detached and reattached.

## 4. Worker (`c6w`)

- R-WRK-1: Headless daemon. No local UI.
- R-WRK-2: Runs any number of concurrent sessions.
- R-WRK-3: Keeps working while disconnected from the head. Sessions in progress continue. Events are journaled locally and synced to the head on reconnect (see §5.1).
- R-WRK-4: Runs on macOS and Linux. Windows is out of scope for now.
- R-WRK-5: Serves file-browsing, search, git and PTY RPCs to the head (for R-UI-5 and R-UI-7).
- R-WRK-7: Workers advertise configured workspaces (repo roots, defined on the head). A session can also be started at an arbitrary path.
- R-WRK-6: Permissions are "yolo": no approval prompts, no sandboxing. Tools run with the permissions of the worker's OS user.

### 4.1 Agent and tools (parity with Codex and Claude Code)
- R-TOOL-1: Core tools: read, write, edit (string replace) and apply_patch, shell (bash/zsh), grep (ripgrep-class), glob, web fetch, web search.
- R-TOOL-2: Background and long-running shell processes, with output polling.
- R-TOOL-3: Todo/plan tracking tool.
- R-TOOL-4: Subagents (child sessions with their own context).
- R-TOOL-5: MCP client (stdio and streamable HTTP). MCP servers are defined on the head and pushed to workers. Stdio servers run on the worker.
- R-TOOL-6: Project instruction files: `AGENTS.md`, plus `CLAUDE.md` for compatibility.
- R-TOOL-7: Skills (in the Claude Code / Codex skills format), stored on the head and pushed to workers. Repo-local skills in the worktree are also loaded.
- R-TOOL-8: Context management: automatic and manual compaction, token budget tracking, large tool outputs truncated or spilled to a file.
- R-TOOL-9: Image input (screenshots, files).
- Later: hooks, LSP integration.

## 5. Cross-cutting

### 5.1 Session sync and offline operation
- Each session is an append-only event log. Every event has a monotonic per-session sequence number.
- The worker writes events to a local durable journal first, then streams them to the head. The head acknowledges each sequence number, and the worker trims the journal behind that point.
- On reconnect, the worker resumes from the head's last acknowledged sequence number. Uploads are idempotent.
- Single-writer rule: the user's input enters only through the head, and turn events are produced only by the worker. The two logs therefore never conflict, and sync needs no merging.
- While a worker is offline, its sessions cannot receive new prompts. They finish their current turn, and the UI shows the backlog once the worker has synced.

### 5.2 Credentials
- The head stores LLM credentials encrypted at rest.
- For OAuth subscriptions, **the head alone refreshes tokens** and hands workers short-lived access tokens. If several workers refreshed the same rotating refresh token, they would invalidate each other's tokens.
- A worker caches the access token it received and keeps working offline until that token expires. After that, LLM calls fail until the worker reconnects.

### 5.3 Git integration
- R-GIT-1: Each session can run in its own git worktree and branch (default naming `c6/<session-slug>`).
- R-GIT-2: The head UI shows diffs, branch status and commits for a session.
- R-GIT-3: Remote git authentication. MVP: workers use their host's existing SSH keys and `gh` auth. `c6w` ships a git credential-helper entry point from the start, so that per-workspace, head-managed credentials (a GitHub App or PAT, minting short-lived tokens) can be enabled later without changing workers.
- R-GIT-4 (later): Create PRs and track CI from the head.

### 5.4 Observability
- R-OBS-1: The head exports OTLP traces, metrics and logs that follow the OpenTelemetry GenAI semantic conventions (`gen_ai.*` attributes, client and agent spans, tool execution spans, token usage metrics).
- R-OBS-2: Spans that start on a worker (LLM calls, tool calls) are sent to the head over the wire protocol and exported by the head. Trace context propagates end to end.

### 5.5 Distribution and upgrades
- R-DIST-1: Two binaries, `c6h` and `c6w`.
- R-DIST-2: Upgrades are deferred for the MVP (manual install). The protocol version handshake (R-NET-5) is built now.
- Later: the head hosts `c6w` builds; a worker drains its sessions, swaps its binary and re-execs itself. The head can roll out upgrades worker by worker.

### 5.6 Scale
- No hard targets. The design should support a large number of workers, each with many sessions. The head must not hold per-worker state in ways that stop it from scaling.

## 6. Out of scope for v1
- Tasks that span multiple workers.
- Approval modes and sandboxing.
- Public internet exposure of the head.
- Windows workers.
- Multi-user support and authentication.
- Automatic upgrades.

## 7. Decisions log
- D-1 Git credentials: worker host keys for the MVP; credential-helper hook so head-managed tokens can be added later (R-GIT-3).
- D-2 Web UI auth: single user, no login, network-level access only (R-UI-1a).
- D-3 Head storage: SQLite behind a storage trait (R-HEAD-1c).
- D-4 Workspace model: workspaces defined on the head, plus arbitrary paths (R-WRK-7).
- D-5 Wire protocol: gRPC/tonic with protobuf bidirectional streams (R-NET-6).
- D-6 Config: all config on the head; workers keep only a non-authoritative cache (R-HEAD-1a/1b).
- D-7 Terminals: yes (R-UI-7).
- D-8 Subscription access: ChatGPT uses OpenAI's sanctioned "Sign in with ChatGPT" flow, so it needs no feature flag. Claude subscription OAuth is not built, because Anthropic's terms prohibit it in third-party tools (see R-LLM-3).
- D-9 Platforms: macOS and Linux only for now.

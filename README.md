# carbon6

An AI coding-agent harness for running agents on many machines from one place.

- **`c6h`** (head) runs centrally. It owns all configuration, credentials and session data, and serves the web UI.
- **`c6w`** (worker) runs headless on each dev machine (macOS or Linux). It attaches to the head and runs agent sessions there.

See [docs/requirements.md](docs/requirements.md) and [docs/architecture.md](docs/architecture.md).

## What it does

- **Sessions on any worker, from one web UI.** You can watch output stream live (text, reasoning, tool calls and shell output), send messages mid-turn, and stop a turn. Sessions can run in a fresh git worktree.
- **Providers:**
  - Anthropic API (Claude)
  - OpenAI API (Responses), or your ChatGPT plan via Sign in with ChatGPT
  - Any OpenAI-compatible server (Ollama, vLLM, OpenRouter, …)
  - **Claude Code**, which runs each worker's own signed-in `claude` CLI
- **Model features:** prompt caching, reasoning carried between steps, effort per session, and automatic compaction for long sessions.
- **Tool sets per model family:** Claude Code-style for Claude, Codex-style for GPT, and a generic set otherwise. Tools include read, write, edit, `apply_patch`, bash (with background jobs), grep, glob, `web_fetch`, the provider's own web search, plans, subagents, MCP servers and skills.
- **Offline-tolerant.** Workers journal every event before sending it. Sessions keep running when the head is unreachable and sync back exactly when it returns. A worker that loses its journal has its sessions restored from the head.
- **IDE-like access to workers:** browse files, review git changes, and open terminals in the browser.
- **Observability:** the head exports OpenTelemetry GenAI traces over OTLP.

## Build

Run `make` to list the targets.

**With Docker only.** You need nothing else installed:

```bash
make docker-build      # Linux binaries for this machine's architecture → dist/linux-<arch>/
make docker-test       # the full test suite
make docker-head       # run the head in a container on http://127.0.0.1:7700
make docker-token      # an enrollment token from that head
```

- `make docker-build-all` builds both amd64 and arm64.
- `make docker-images` builds the `carbon6/c6h` and `carbon6/c6w` runtime images.
- Docker produces Linux binaries only. A worker on a Mac needs a native `c6w`, built with the local toolchain below.

**With a local toolchain.** You need Rust with the `wasm32-unknown-unknown` target, plus [Trunk](https://trunkrs.dev), which downloads Tailwind and wasm-bindgen on first use. protoc is vendored.

```bash
make build             # web UI, then release c6h and c6w into target/release
make test
```

## Run

On the head machine:

```bash
c6h               # binds private/tailnet addresses only; prints the web UI URL
```

1. Sign in: run `c6h login-link` on the head and open the link it prints (it works once, for 15 minutes). Then add a passkey under **Settings → Sign-in**, and sign in with Touch ID, Face ID or a security key from then on. A new link always gets you back in.
   - To sign in another device (a phone), create a one-time code under **Settings → Sign-in → Sign in another device** and type it into that device's sign-in page. Codes work once, for 10 minutes; five wrong codes cancel all outstanding ones.
2. In the UI, add an LLM connection under **Settings**. For a local Ollama, use `http://127.0.0.1:11434/v1` with no key.
3. On the **Workers** page, click **Add worker**, then run the printed command on the worker machine (or create a token with `c6h enroll-token` on the head):

```bash
c6w enroll c6e1.…
c6w run
```

### Command line (`c6`)

`c6` runs sessions headless, for scripts and terminals. It signs in with an API token: create one under **Settings → Sign-in → API tokens** (or with `c6h api-token` on the head), then:

```bash
c6 login https://head.example:7700 --token c6_… --ca carbon6-ca.pem   # --ca only if the head signs its own certificate
```

- **`c6 run "fix the failing test"`** starts a session in the current folder on the only online worker (or `--worker`, `-C <dir>`, `-c <connection>`, `-m <model>`; the model defaults to your latest session's). It prints the reply as the agent works, with tool calls on stderr, and exits 0 when the turn completes, 1 when it fails, or 130 when interrupted (Ctrl-C interrupts it). The prompt can come from stdin, and `--json` prints events as JSON lines.
- **`c6 send <id> "…"`** continues a session (a unique id prefix is enough). Also: `c6 wait`, `c6 show` (Markdown), `c6 stop`, `c6 sessions`, `c6 search` and `c6 workers`.
- **API tokens** reach the whole API except sign-in management. Revoke them in Settings. `C6_URL`, `C6_TOKEN` and `C6_CA` override the saved login.

### Configuration

- **Head:** `c6h.toml`, optional (see `crates/c6-head/src/config.rs`): `~/Library/Application Support/c6h/c6h.toml` on macOS, `~/.config/c6h/c6h.toml` on Linux, or `--config`.
  - State (database, `master.key`, CAs) lives in `data_dir`: by default `~/Library/Application Support/c6h` (macOS) or `~/.local/share/c6h` (Linux). Back up `c6h.db` and `master.key` together.
  - Add any names you reach the head by to `extra_names`; the head refuses requests addressed to other names. Its Tailscale MagicDNS name is added automatically.
  - Passkeys need HTTPS (or `http://localhost`). `ui_tls_head_ca = true` makes the head issue its own certificate from a private CA, so its names never appear in public Certificate Transparency logs (as they would with Let's Encrypt or Tailscale certificates). Trust that CA once per device:
    - Mac (Chrome and Safari use the macOS Keychain): `curl -so /tmp/carbon6-ca.pem http://<head>:7700/ca.pem`, then `sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain /tmp/carbon6-ca.pem`, and restart the browser. (Or open the file in Keychain Access and set Trust → Always Trust.)
    - iPhone, iPad: open `http://<head>:7700/ca.mobileconfig`, install the profile in Settings, then enable it under General → About → Certificate Trust Settings.
    - Android: download `http://<head>:7700/ca.crt` in Chrome, then Settings → Security & privacy → More security settings → Encryption & credentials → Install a certificate → CA certificate, and pick `carbon6-ca.crt` (menu names vary a little by phone; search Settings for "CA certificate"). Android then shows a "network may be monitored" notice; that is expected for any user-installed CA. Then force-stop Chrome (Settings → Apps → Chrome → Force stop): if you ever clicked through the certificate warning, Chrome keeps showing "Not secure" until it restarts.
    - Elsewhere: add `http://<head>:7700/ca.pem` to the system's trusted certificates (Linux: `/usr/local/share/ca-certificates` + `update-ca-certificates`).
    - The CA is name-constrained to the head's names and private IP ranges, so it cannot vouch for any other site.
  - Or set `ui_tls_cert`/`ui_tls_key` to PEM files. With HTTPS on, plain HTTP on the UI port redirects to the HTTPS name.
  - Sign-in is on by default; `auth = false` turns it off.
  - Set `otlp_endpoint` (or `OTEL_EXPORTER_OTLP_ENDPOINT`) to export traces.
  - MCP servers, slash commands, agents, skills and hooks are configured in the UI (Settings) and stored on the head.
- **Worker:** its identity lives in `~/.local/state/c6w` (Linux) or `~/Library/Application Support/c6w` (macOS).
  - For Claude Code sessions, install `claude` on the worker and sign in there; set `C6_CLAUDE_BIN` if it is not on the worker's PATH.

### Labels

Sessions carry labels, which group them and decide which settings apply.

- **Automatic labels:**
  - `worker:<name>`;
  - `repo:<host/owner/name>`, from the git remote, so a repository is the same on every machine;
  - `folder:<path>`;
  - `model:<model>`.
- **Your own labels:** anything else, like `client:acme`, `spike` or `rust`. Add them when starting a session (they're remembered per folder), in a session's header with **+ label**, or with `c6 run -l`. A side chat keeps its parent's labels, and `/clear` keeps them for the fresh session.
- **Finding sessions.** The session list, search, the inbox, mission control and the activity dock share one label filter, kept in your browser. Type labels into it (suggestions come from your sessions; `repo:github.com/acme` covers the whole org), or click any label on a session. The inbox and the dock note how many sessions the filter hides (the dock shows the filter, with a clear button), the Inbox badge counts only what the filter shows (its tooltip gives the rest), browser notifications come only for sessions the filter shows (the toggle reads "🔔 filtered"), and mission control falls back to the most recent matching sessions. The session list's search also matches labels. On the command line, `c6 sessions -l <label>` and `c6 search -l <label> …` filter the same way.
- **"Applies to".** Commands, agents, skills and hooks each have an **Applies to** field in Settings:
  - empty means everywhere;
  - otherwise, labels a session must *all* have, e.g. `repo:github.com/acme/app` or `client:acme, rust`;
  - `repo:` and `folder:` match path prefixes, so `repo:github.com/acme` covers a whole organization and `folder:~/code` everything under it.
- **Same name, different labels.** Several commands, agents or skills can share a name if their labels differ; a session gets the most specific one that matches. Every matching hook runs.
- **Config tab.** A session's **Config** tab lists what applies to it, and why anything doesn't.
- **When changes apply.** Commands follow label changes at once. Agents, skills and hooks are fixed when a session starts.

### Web search with any model

Claude and OpenAI models search the web through their own provider. For the rest (Ollama and other OpenAI-compatible servers), choose a provider under **Settings → Web search**, and sessions get a `web_search` tool. The options:

- **Brave Search API** or **Tavily:** the API key is a secret, picked by name.
- **SearXNG:** self-hosted, so it needs only its URL, with the JSON format enabled.

To give every model the same search, tick "Offer it to every model".

### Remote machines over SSH

For a machine you can reach by SSH but don't want to install `c6w` on, start a session there *through* a worker. In the new-session form choose **SSH host, through the worker** and enter `user@host` (or an alias from the worker's `~/.ssh/config`) and a folder there (default: home). On the command line: `c6 run --ssh admin@web1 "install and start nginx"`.

- **What runs where.** The agent's shell commands, file reads and edits, grep and glob, and background commands all run on that machine.
- **Connection.** The worker connects with its own SSH setup: keys, agent and `~/.ssh/config`, never passwords (BatchMode). It keeps one multiplexed connection per host.
- **What the agent is told.** The session's system prompt names the machine and its OS. The agent is told it works non-interactively (`sudo -n`).
- **Secrets** reach remote commands through stdin, never through a command line.
- **Session view.** The session shows `⇄ user@host` and gets a `host:` label. Its **Terminal** tab opens an SSH shell in the session's folder there.
- **What stays on the worker.** Hooks and MCP servers run on the worker. Files on the target aren't checkpointed for rewind, and @-mentions refer to the worker's files, so they're off for SSH sessions.

### Sessions without a folder

Not every task belongs in a repository. Leave the folder empty when starting a session (or use `c6 run --scratch`), and the worker gives it a scratch workspace of its own, `scratch/<session>` in the worker's state directory. It's where the agent downloads, drafts and builds things for research, documents and one-off jobs.

### Secrets

Keep API keys, tokens and passwords under **Settings → Secrets**. Each one is an environment variable (`$GITHUB_TOKEN`) for the agent's shell commands, background commands included, in the sessions it applies to (an "Applies to" label list, like other settings).

- **The agent never sees a value.** The system prompt lists only names and descriptions, and a value that appears in command output is replaced by `[secret:NAME]` before the model or the UI sees it.
- **Storage.** Values are encrypted on the head and can only be replaced, never read back.
- **Delivery.** Workers fetch values when a turn starts, keep them in memory for a few minutes, and never write them to their journal. A secret scoped to a `worker:` label is only handed to that worker.

### Background tasks

The agent can start work in the background and carry on: long builds, test suites, servers (`bash` with `run_in_background`), and subagents (Task with `run_in_background`).

- **When a task ends, the agent is told.** It gets the exit code, how long the task ran and the end of its output (for a subagent, its report). If a turn is running, the agent sees it at its next step. If the session is idle, a new turn starts so the agent can react by itself. "Start the build and tell me when it's done" works without you checking back.
- **Opting out.** A task started with `notify: false` doesn't wake anyone. Use it for servers and watchers meant to keep running.
- **The running-tasks control.** The session header shows **⚙ N running**: each task, how long it has run, and a **Stop** button. The agent is told when you stop one.
- **In the transcript**, each ending appears as a one-line entry, with the agent's notice folded underneath.
- **Limit.** Notices start at most 20 turns per hour in an idle session. Beyond that they're held until your next message, and the session says so.
- **Worker restarts.** Tasks die with the worker; the agent is told they were lost, and a command left running by the old worker process is stopped.

### Rewind and edit

Hover one of your messages: **↶ Rewind** takes the conversation back to before it, and **✎ Edit** changes it and sends it again. Either can also restore files: before each turn the worker snapshots the session's folder into a private repository in its state directory (your own git repository, if any, is never touched; folders without git work too; `node_modules/`, `target/` and the like are skipped, as are files your `.gitignore` ignores). Rewound messages stay in the transcript, folded under a marker.

### Attachments and @-mentions

Paste, drop or pick (📎) images and text files into the message box; they go with the message (images as images, for models that see them; text files inline). Type `@` to mention a file or folder in the session's folder: a fuzzy finder completes the path, and the worker attaches the file (or a folder's listing) when the message arrives, so the model has it without a tool call. Files over 256 KB are mentioned but not inlined. Limits: 10 MB per upload, 20 MB per message. The Claude Code backend gets text only.

### Slash commands

Type `/` in the message box for the command list.

- **Built in:** `/compact [focus]` summarizes the conversation so far (optionally focused on something) to free context; `/init` writes an AGENTS.md describing the codebase; `/review [what]` reviews the uncommitted changes, or a branch or PR you name; `/clear` starts a fresh session in the same folder with the same model; `/export` downloads the conversation as Markdown.
- **Your own**, under **Settings → Slash commands**: a name, a description, an arguments hint and the prompt. In the prompt, `$ARGUMENTS` and `$1`…`$9` take the arguments, `` !`cmd` `` the output of a shell command run in the session's folder, and `@path` attaches a file. Changes apply to the next message, and a command named like a built-in replaces it. Start from an example (fix a GitHub issue, write a commit message, explain code).

### Agents and skills

Both live in Settings and are copied into each new session.

- **Agents** (**Settings → Agents**) are specialized subagents the model can launch with the Task tool. Each has:
  - a name;
  - a description, which the model reads to pick one;
  - optionally, the tools it may use (Claude Code's names, e.g. `Read, Grep, Glob`, work with every model; all tools when empty);
  - optionally, a model (`opus`, `sonnet`, `haiku` or a model id; honored on Anthropic connections);
  - its instructions, which become its system prompt.
- **Skills** (**Settings → Skills**) are instructions the agent loads by name when they apply. Only the name and description sit in its prompt until then.
- **On/off.** Each command, agent and skill can be switched off without deleting it.

### Hooks

Hooks are shell commands that run on the worker at set points in every session. Set them up under **Settings → Hooks**; the head stores them and each new session takes a copy. Start from an example (refuse `rm -rf`, protect `.env` files, format after edits, tell the agent the git branch, keep going until tests pass) or write your own.

- **Events:** `PreToolUse` and `PostToolUse` (narrowed by a tool-name regex; Claude Code's names like `Bash` and `Edit` work with every model), `UserPromptSubmit`, `Stop` and `SessionStart`. A hook can be limited to sessions in one folder, and switched off without deleting it.
- **How a command runs.** It runs in the session's folder, gets the event as JSON on stdin (`tool_name`, `tool_input`, `prompt`, …), and has `C6_PROJECT_DIR` set to the repository root.
- **Exit code 2 blocks**, with stderr as the reason:
  - `PreToolUse`: the call is refused and the agent is told why;
  - `UserPromptSubmit`: the message is shown struck through and never sent;
  - `Stop`: the agent keeps working, with the reason as feedback;
  - `PostToolUse`: stderr is added to the tool's result.
- **Exit 0.** Printing `{"decision": "block", "reason": …}` also blocks. Plain output from `UserPromptSubmit` and `SessionStart` is given to the agent as context.
- **Claude Code sessions** get the same hooks, passed to the `claude` CLI as settings.

### Workspace

The UI is a workspace of up to four tiles side by side, each a stack of columns:

- **Columns:** opening something (a session from the list, a search result, a side chat's parent) puts it in a column beside where you clicked, so where you came from stays open — a search stays beside the result you are reading. Earlier columns fold into spines; click one to go back to it (the columns after it fold to the right until you open something new). The header's nav swaps sections in place.
- **Sessions side by side:** from a list (Sessions, Inbox, Mission, search results), the first session opens beside the list; each further one opens in a new tile at the right, up to four (then it replaces the least recently used tile). A session that is already open is shown where it is instead of opening twice.
- **Tiles:** ⌘/Ctrl-click any link (or ⧉ in a column header, or **+ Tile**) to open it in a new tile. ⌘1–4 focus a tile; ⌘[ and ⌘] step back and forward in the focused tile.
- **Activity dock** (left, on wide screens): running sessions, ones that finished or failed while not on screen (amber/red), and pinned ones (☆ in a session's header). **Inbox** lists the same for triage; **Mission** shows them as live cards you can reply from.
- **Phones:** one tile at a time with a switcher, Inbox and **+** at the bottom; a column fills the screen, with ‹ › in its header and the nav in ☰.
- The workspace is saved in the browser; the address bar shows the focused column, so links and reloads land in the right place.

### Search

The search box in the header (⌘K) searches every message, tool call, tool output, thinking block and error in all sessions, including side chats and subagents. Words and `"quoted phrases"` must all match, as substrings (3+ characters each, case-insensitive); `-word` excludes. Results are grouped by session; each match links to its place in the transcript, opened and highlighted. In a session, the find bar (it opens from a search result, or with **Find**) highlights every match on the page (the current one in orange) and steps through every match in the session and its side chats: Enter / ⌘G for the next, Shift+Enter / ⇧⌘G for the previous, Esc to close. The index (SQLite FTS5) is kept in the head's database, updated as events arrive, and rebuilt automatically when its format changes.

## Local development

```bash
make dev-head                     # head on all interfaces using dev/c6h.toml; state in the standard data directory
make dev-token                    # prints a token
make dev-worker TOKEN=c6e1.…      # enrolls and runs a worker against it (later: just make dev-worker)
make dev-login-link               # a one-time link to sign in to the dev UI
```

- For UI work, `cd crates/c6-ui && trunk serve` proxies `/api` to the dev head.
- Evals: see [evals/README.md](evals/README.md).

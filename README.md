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
  - MCP servers and skills are configured in the UI.
- **Worker:** its identity lives in `~/.local/state/c6w` (Linux) or `~/Library/Application Support/c6w` (macOS).
  - For Claude Code sessions, install `claude` on the worker and sign in there; set `C6_CLAUDE_BIN` if it is not on the worker's PATH.

### Rewind and edit

Hover one of your messages: **↶ Rewind** takes the conversation back to before it, and **✎ Edit** changes it and sends it again. Either can also restore files: before each turn the worker snapshots the session's folder into a private repository in its state directory (your own git repository, if any, is never touched; folders without git work too; `node_modules/`, `target/` and the like are skipped, as are files your `.gitignore` ignores). Rewound messages stay in the transcript, folded under a marker.

### Attachments and @-mentions

Paste, drop or pick (📎) images and text files into the message box; they go with the message (images as images, for models that see them; text files inline). Type `@` to mention a file or folder in the session's folder: a fuzzy finder completes the path, and the worker attaches the file (or a folder's listing) when the message arrives, so the model has it without a tool call. Files over 256 KB are mentioned but not inlined. Limits: 10 MB per upload, 20 MB per message. The Claude Code backend gets text only.

### Slash commands

Type `/` in the message box for the command list.

- **Built in:** `/compact [focus]` summarizes the conversation so far (optionally focused on something) to free context; `/init` writes an AGENTS.md describing the codebase; `/review [what]` reviews the uncommitted changes, or a branch or PR you name; `/clear` starts a fresh session in the same folder with the same model; `/export` downloads the conversation as Markdown.
- **Your own**, in Claude Code's format: Markdown files in `.claude/commands/` (in the repository, from its root down to the session's folder) or `~/.claude/commands/`. `frontend/component.md` is `/frontend:component`. Frontmatter can set `description` and `argument-hint`. In the body, `$ARGUMENTS` and `$1`…`$9` take the arguments, `` !`cmd` `` the output of a shell command run in the session's folder, and `@path` attaches a file. A project command replaces a built-in or personal one of the same name.
- Skills in `.claude/skills/*/SKILL.md` and CLAUDE.md/AGENTS.md files are read too (see Configuration).

### Custom agents

Agents in Claude Code's format, `.claude/agents/<name>.md` (in the repository, or `~/.claude/agents/`), become subagent types the model can launch with the Task tool: frontmatter `name`, `description` (the model reads it to pick one), optional `tools` (comma-separated, Claude Code's names, e.g. `Read, Grep, Glob`; all tools when absent) and `model` (`inherit`, `opus`, `sonnet`, `haiku` or a model id; honored on Anthropic connections), and the body as the agent's instructions (its system prompt).

### Hooks

Hooks use Claude Code's `settings.json` format and files: `~/.claude/settings.json`, and the repository's `.claude/settings.json` and `.claude/settings.local.json`.

```json
{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "$CLAUDE_PROJECT_DIR/.claude/check-command.sh"}]}]}}
```

- Events: `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop` and `SessionStart`.
- **How a command runs.** Each one runs in the session's folder with the event as JSON on stdin (`tool_name`, `tool_input`, `prompt`, …) and `CLAUDE_PROJECT_DIR` set.
- **Exit code 2 blocks**, with stderr as the reason:
  - `PreToolUse`: the call is refused and the model is told why;
  - `UserPromptSubmit`: the message is shown struck through and never sent;
  - `Stop`: the model keeps working, with the reason as feedback;
  - `PostToolUse`: stderr is added to the result.
- **JSON output.** `{"decision": "block", "reason": …}` and `hookSpecificOutput.additionalContext` work as in Claude Code.
- **Plain stdout** from `UserPromptSubmit` and `SessionStart` is added as context.
- **Matchers** are regular expressions over tool names. Claude Code's names (`Bash`, `Edit`, `Read`, …) match in every tool set.
- **Claude Code sessions** run their own hooks.

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

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

- **Head:** `~/.config/c6h/c6h.toml`, optional (see `crates/c6-head/src/config.rs`).
  - Add any names you reach the head by to `extra_names`; the head refuses requests addressed to other names. Its Tailscale MagicDNS name is added automatically.
  - Passkeys need HTTPS (or `http://localhost`). `ui_tls_head_ca = true` makes the head issue its own certificate from a private CA, so its names never appear in public Certificate Transparency logs (as they would with Let's Encrypt or Tailscale certificates). Trust that CA once per device:
    - Mac (Chrome and Safari use the macOS Keychain): `curl -so /tmp/carbon6-ca.pem http://<head>:7700/ca.pem`, then `sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain /tmp/carbon6-ca.pem`, and restart the browser. (Or open the file in Keychain Access and set Trust → Always Trust.)
    - iPhone, iPad: open `http://<head>:7700/ca.mobileconfig`, install the profile in Settings, then enable it under General → About → Certificate Trust Settings.
    - Android: download `http://<head>:7700/ca.crt` in Chrome, then Settings → Security & privacy → More security settings → Encryption & credentials → Install a certificate → CA certificate, and pick `carbon6-ca.crt` (menu names vary a little by phone; search Settings for "CA certificate"). Android then shows a "network may be monitored" notice; that is expected for any user-installed CA.
    - Elsewhere: add `http://<head>:7700/ca.pem` to the system's trusted certificates (Linux: `/usr/local/share/ca-certificates` + `update-ca-certificates`).
    - The CA is name-constrained to the head's names and private IP ranges, so it cannot vouch for any other site.
  - Or set `ui_tls_cert`/`ui_tls_key` to PEM files. With HTTPS on, plain HTTP on the UI port redirects to the HTTPS name.
  - Sign-in is on by default; `auth = false` turns it off.
  - Set `otlp_endpoint` (or `OTEL_EXPORTER_OTLP_ENDPOINT`) to export traces.
  - MCP servers and skills are configured in the UI.
- **Worker:** its identity lives in `~/.local/state/c6w` (Linux) or `~/Library/Application Support/c6w` (macOS).
  - For Claude Code sessions, install `claude` on the worker and sign in there; set `C6_CLAUDE_BIN` if it is not on the worker's PATH.

## Local development

```bash
make dev-head                     # head on all interfaces using dev/c6h.toml, state in target/dev
make dev-token                    # prints a token
make dev-worker TOKEN=c6e1.…      # enrolls and runs a worker against it (later: just make dev-worker)
make dev-login-link               # a one-time link to sign in to the dev UI
```

- For UI work, `cd crates/c6-ui && trunk serve` proxies `/api` to the dev head.
- Evals: see [evals/README.md](evals/README.md).

# carbon6

An AI coding-agent harness for running agents on many machines from one place.

- **`c6h`** (head) runs centrally. It owns all configuration and session data and serves the web UI.
- **`c6w`** (worker) runs headless on each dev machine. It attaches to the head and runs agent sessions there.

See [docs/requirements.md](docs/requirements.md) and [docs/architecture.md](docs/architecture.md).

Status: **M1**. You can run coding sessions on any worker from the web UI, against any OpenAI-compatible endpoint (OpenAI, Ollama, vLLM, …). The core tools are read, write, edit, bash, grep and glob. Sessions keep running when the head is unreachable and sync back when it returns.

## Build

Requires a Rust toolchain with `wasm32-unknown-unknown` and [Trunk](https://trunkrs.dev). Trunk downloads Tailwind and wasm-bindgen on first build. protoc is vendored.

```bash
make build    # web UI, then release c6h and c6w
make test
```

## Run

On the head machine:

```bash
c6h               # binds private/tailnet addresses only; prints the web UI URL
```

Open the UI, add an LLM connection under **Settings** (for a local Ollama: `http://127.0.0.1:11434/v1`, no key), then click **Add worker** on the Workers page, and run the printed command on the worker machine (or use `c6h enroll-token` on the head):

```bash
c6w enroll c6e1.…
c6w run
```

Head config lives in `~/.config/c6h/c6h.toml` (optional; see `crates/c6-head/src/config.rs`). Worker identity lives in `~/.local/state/c6w` (Linux) or `~/Library/Application Support/c6w` (macOS).

## Local development

```bash
make dev-head                     # loopback-only head using dev/c6h.toml, state in target/dev
make dev-token                    # prints a token
make dev-worker TOKEN=c6e1.…      # enrolls and runs a worker against it
```

For UI work, `cd crates/c6-ui && trunk serve` proxies `/api` to the dev head.

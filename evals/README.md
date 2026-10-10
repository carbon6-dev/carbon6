# Evals

`c6-eval` runs these tasks against a real model, using carbon6's agent loop and tools without a head or worker. Use it to compare models, tool profiles, effort levels and prompt changes before you adopt them.

```bash
cargo build -p c6-eval
./target/debug/c6-eval --base-url http://127.0.0.1:11434/v1 --model qwen3.6:27b --profile generic
ANTHROPIC_KEY=… ./target/debug/c6-eval --kind anthropic --base-url https://api.anthropic.com --api-key-env ANTHROPIC_KEY --model claude-opus-5-5 --effort high --out opus.json
```

Each task is a directory under `tasks/`:

- `task.toml` holds the `prompt`, an optional `setup` shell script, a `check` shell script (exit 0 means pass) and `timeout_secs`.
- `files/` holds a fixture that is copied into a fresh git repository, where the agent works.

`--keep` keeps each task's working directory so you can inspect it. `--out` writes the full results as JSON.

Real-model runs cost money (except for local models) and vary from run to run. Compare several runs before drawing conclusions.

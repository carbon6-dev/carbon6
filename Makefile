.PHONY: ui build test dev-head dev-token dev-worker

# Web UI → crates/c6-ui/dist, embedded into c6h.
ui:
	cd crates/c6-ui && trunk build --release

build: ui
	cargo build --release

test:
	cargo test

# Local loop: `make dev-head`, then `make dev-token` and `make dev-worker TOKEN=...`.
dev-head: ui
	cargo run -p c6h -- --config dev/c6h.toml

dev-token:
	@cargo run -q -p c6h -- --config dev/c6h.toml enroll-token --name dev

dev-worker:
	cargo run -p c6w -- --state-dir target/dev/c6w enroll --force $(TOKEN)
	cargo run -p c6w -- --state-dir target/dev/c6w run

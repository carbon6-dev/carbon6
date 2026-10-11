# carbon6 build targets. Run `make` for the list.
#
# docker-* targets need only Docker (with buildx); everything else needs a
# local Rust toolchain with the wasm32-unknown-unknown target and Trunk.

.DEFAULT_GOAL := help
SHELL := /bin/bash

DOCKER       ?= docker
DOCKERFILE   := docker/Dockerfile
IMAGE_PREFIX ?= carbon6
TAG          ?= dev
# This machine's architecture, as Docker names it (amd64 / arm64).
ARCH         := $(shell uname -m | sed -e 's/x86_64/amd64/' -e 's/aarch64/arm64/')
UI_PORT      ?= 7700
WORKER_PORT  ?= 7701

.PHONY: help docker-build docker-build-all docker-test docker-images docker-head docker-head-stop docker-token docker-login-link \
        ui build test dev-head dev-token dev-login-link dev-worker clean

help: ## Show this help
	@echo "Usage: make <target> [VAR=value]"
	@awk 'BEGIN { FS = ":.*## " } \
		/^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) } \
		/^[a-zA-Z0-9_-]+:.*## / { printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2 }' $(MAKEFILE_LIST)
	@echo

##@ Docker (no local toolchain needed)

docker-build: ## Build Linux binaries for this machine's architecture into dist/linux-<arch>/
	$(DOCKER) buildx build -f $(DOCKERFILE) --target artifacts --platform linux/$(ARCH) --output type=local,dest=dist/linux-$(ARCH) .
	@ls -l dist/linux-$(ARCH)

docker-build-all: ## Build Linux binaries for amd64 and arm64 into dist/linux_{amd64,arm64}/ (the other arch is emulated: slow)
	$(DOCKER) buildx build -f $(DOCKERFILE) --target artifacts --platform linux/amd64,linux/arm64 --output type=local,dest=dist .
	@ls -l dist/*/

docker-test: ## Run the full test suite in Docker
	$(DOCKER) buildx build -f $(DOCKERFILE) --target test --progress plain .

docker-images: ## Build runtime images carbon6/c6h:dev and carbon6/c6w:dev (IMAGE_PREFIX, TAG)
	$(DOCKER) buildx build -f $(DOCKERFILE) --target c6h --load -t $(IMAGE_PREFIX)/c6h:$(TAG) .
	$(DOCKER) buildx build -f $(DOCKERFILE) --target c6w --load -t $(IMAGE_PREFIX)/c6w:$(TAG) .

docker-head: ## Run the head in Docker: UI on 127.0.0.1:7700, workers on :7701, data in volume c6h-data
	$(DOCKER) buildx build -f $(DOCKERFILE) --target c6h --load -t $(IMAGE_PREFIX)/c6h:$(TAG) .
	-$(DOCKER) rm -f c6h >/dev/null 2>&1
	$(DOCKER) run -d --name c6h --restart unless-stopped \
		-p 127.0.0.1:$(UI_PORT):7700 -p 127.0.0.1:$(WORKER_PORT):7701 \
		-e C6H_ADVERTISE_HOST=127.0.0.1 -v c6h-data:/data \
		$(IMAGE_PREFIX)/c6h:$(TAG)
	@echo "Head running: open http://127.0.0.1:$(UI_PORT)   (logs: docker logs -f c6h)"

docker-head-stop: ## Stop and remove the head container (the c6h-data volume is kept)
	$(DOCKER) rm -f c6h

docker-token: ## Print a worker enrollment token from the running head container
	@$(DOCKER) exec c6h c6h --config /etc/c6h/c6h.toml enroll-token

docker-login-link: ## Print a one-time web UI login link from the running head container
	@$(DOCKER) exec c6h c6h --config /etc/c6h/c6h.toml login-link

##@ Local toolchain

ui: ## Build the web UI into crates/c6-ui/dist (embedded into c6h)
	cd crates/c6-ui && trunk build --release

build: ui ## Build release binaries (c6h, c6w, c6) into target/release
	cargo build --release

test: ## Run the test suite
	cargo test

dev-head: ui ## Run the dev head on all interfaces (dev/c6h.toml; state in the standard data directory)
	cargo run -p c6h -- --config dev/c6h.toml

dev-token: ## Print an enrollment token from the dev head's database
	@cargo run -q -p c6h -- --config dev/c6h.toml enroll-token --name dev

dev-login-link: ## Print a one-time web UI login link for the dev head
	@cargo run -q -p c6h -- --config dev/c6h.toml login-link

dev-worker: ## Run the dev worker (pass TOKEN=... to enroll it first)
	@if [ -n "$(TOKEN)" ]; then cargo run -p c6w -- enroll --force "$(TOKEN)"; fi
	cargo run -p c6w -- run

clean: ## Remove build outputs (target/, dist/, crates/c6-ui/dist)
	cargo clean
	rm -rf dist crates/c6-ui/dist

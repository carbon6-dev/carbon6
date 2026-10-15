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
# Ubuntu release the packages are built on. It sets the minimum glibc, so they
# also install on every later release (22.04: 22.04, 24.04, 26.04, ...).
UBUNTU       ?= 22.04
DEB_DIR       = dist/deb/ubuntu-$(UBUNTU)
CRATE_VERSION = $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
COMMIT_TIME   = $(shell git log -1 --format=%cd --date=format:%Y%m%d%H%M%S)
# Set when HEAD is the release v<crate version>: its packages get the plain
# version. Between releases they get one that sorts after the last release
# and every earlier commit.
RELEASE_TAG   = $(shell git tag --points-at HEAD --list 'v$(CRATE_VERSION)')
# <crate version>+git<commit time>.<commit> between releases.
DEB_VERSION  ?= $(if $(RELEASE_TAG),$(CRATE_VERSION),$(CRATE_VERSION)+git$(COMMIT_TIME).$(shell git rev-parse --short HEAD))
# The author of the commit the packages are built from.
DEB_MAINTAINER ?= $(shell git log -1 --format='%an <%ae>')
BREW         ?= brew
BREW_TAP     ?= carbon6/local
BREW_DIR      = dist/brew
# <crate version>.<commit time> between releases: Homebrew puts it in file://
# URLs, so digits and dots only.
BREW_VERSION ?= $(if $(RELEASE_TAG),$(CRATE_VERSION),$(CRATE_VERSION).$(COMMIT_TIME))
BREW_ENV      = HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1
DEB_ARGS      = --build-arg UBUNTU=$(UBUNTU) --build-arg DEB_VERSION=$(DEB_VERSION) --build-arg "MAINTAINER=$(DEB_MAINTAINER)"

.PHONY: help docker-build docker-build-all docker-test docker-images docker-head docker-head-stop docker-token docker-login-link \
        docker-deb docker-deb-all docker-deb-test brew-dist brew brew-install brew-test release release-notes \
        ui build test dev-head dev-token dev-login-link dev-worker dev-memex clean

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

##@ Ubuntu packages (Docker only)

docker-deb: ## Build .deb packages of c6h, c6w and c6m (systemd units included) and the c6 CLI for this machine's architecture into dist/deb/ubuntu-<UBUNTU>/ (UBUNTU=22.04; they install on later releases too)
	rm -f $(DEB_DIR)/*_$(ARCH).deb $(DEB_DIR)/*-linux-$(ARCH).tar.gz
	$(DOCKER) buildx build -f $(DOCKERFILE) --target debs --platform linux/$(ARCH) $(DEB_ARGS) --output type=local,dest=$(DEB_DIR) .
	@ls -l $(DEB_DIR)

docker-deb-all: ## Build the .deb packages for amd64 and arm64 into dist/deb/ubuntu-<UBUNTU>/ (the other arch is emulated: slow)
	rm -rf $(DEB_DIR)
	$(DOCKER) buildx build -f $(DOCKERFILE) --target debs --platform linux/amd64,linux/arm64 $(DEB_ARGS) --output type=local,dest=$(DEB_DIR),platform-split=false .
	@ls -l $(DEB_DIR)

docker-deb-test: docker-deb ## Install the packages in an Ubuntu container under systemd: head serves, worker and memex attach, c6 sees the worker, upgrade, remove (ARCH=amd64|arm64: the other is emulated)
	$(DOCKER) buildx build -f $(DOCKERFILE) --target deb-host --platform linux/$(ARCH) --build-arg UBUNTU=$(UBUNTU) --load -t $(IMAGE_PREFIX)/deb-host:$(UBUNTU)-$(ARCH) .
	DOCKER=$(DOCKER) packaging/deb/smoke-test.sh $(IMAGE_PREFIX)/deb-host:$(UBUNTU)-$(ARCH) $(DEB_DIR) $(ARCH)

##@ Homebrew (macOS, local toolchain)

brew-dist: build ## Build bottles and formulas of c6h, c6w, c6m (as brew services) and c6 for this Mac's architecture into dist/brew/
	rm -rf $(BREW_DIR)
	packaging/brew/build.sh target/release $(BREW_VERSION) $(BREW_DIR)
	packaging/brew/formulas.sh $(BREW_VERSION) $(BREW_DIR) file://$(abspath $(BREW_DIR)) $(BREW_DIR)/Formula

brew: brew-dist ## brew-dist, then add the formulas to the local tap carbon6/local (BREW_TAP)
	@tap="$$($(BREW) --repo $(BREW_TAP))"; \
	test -d "$$tap" || $(BREW_ENV) $(BREW) tap-new --no-git $(BREW_TAP); \
	mkdir -p "$$tap/Formula" && cp $(BREW_DIR)/Formula/*.rb "$$tap/Formula/" && echo "Formulas in $$tap/Formula"

brew-install: brew ## Install or upgrade c6h, c6w, c6m and c6 from the local tap, and restart those of their services that are running
	@for f in c6h c6w c6m c6; do \
		if $(BREW) list --versions $$f >/dev/null; then $(BREW_ENV) $(BREW) reinstall $(BREW_TAP)/$$f; \
		else $(BREW_ENV) $(BREW) install $(BREW_TAP)/$$f; fi || exit 1; \
	done
	@for f in $$($(BREW) services list | awk '$$1 ~ /^c6[hwm]$$/ && $$2 == "started" { print $$1 }'); do \
		$(BREW) services restart $$f; \
	done

brew-test: brew-dist ## Install the formulas from a scratch tap and run them: head as a brew service, worker and memex attach, c6 sees them, SIGTERM stops them (refuses if c6h etc. are installed)
	packaging/brew/smoke-test.sh $(BREW_DIR)/Formula

##@ Releases

release: ## Cut a release (BUMP=patch|minor|major, or VERSION=x.y.z): check, bump the version, add to CHANGELOG.md, commit, tag v<version> and push, which starts the Release workflow
	packaging/release/release.sh $(if $(filter command line,$(origin VERSION)),$(VERSION),$(or $(BUMP),patch))

release-notes: ## Print the CHANGELOG.md section the next release would add
	@packaging/release/notes.sh

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

dev-memex: ## Run the dev memex (pass TOKEN=... from Settings → Memex to enroll it first)
	@if [ -n "$(TOKEN)" ]; then cargo run -p c6m -- enroll --force "$(TOKEN)"; fi
	cargo run -p c6m -- run

clean: ## Remove build outputs (target/, dist/, crates/c6-ui/dist)
	cargo clean
	rm -rf dist crates/c6-ui/dist

# Releases and CI

Status: built (2026-10-15); the workflows have not yet run on GitHub.

## 1. Goals

- **One command cuts a release:** `make release BUMP=minor`. GitHub Actions does the rest: tests, every package, smoke tests, the GitHub release and the Homebrew tap.
- **CI runs what you run.** Each workflow step is a make target or a script under `packaging/`, so any CI failure can be reproduced with the same command on your machine.
- **Every change is tested** on Linux and macOS, on every push to `main` and every pull request.
- **Nothing half-done.** A release that fails before its tag leaves nothing behind. One that fails in CI publishes nothing, and the fix is a new patch release.

**Not goals (for now):** an apt repository (so `apt upgrade` would find new versions), signed packages, Docker images on a registry, Windows.

## 2. Workflows

| Workflow | Runs on | Does |
|---|---|---|
| **Test** (`.github/workflows/test.yml`) | push to `main`, pull requests, and from Release | `cargo test --locked` on Ubuntu 24.04 and macOS 15; the web UI compiles for wasm32 |
| **Release** (`.github/workflows/release.yml`) | a `v*` tag; or by hand, as a dry run | §4 |

**Toolchain:** the Rust and Trunk versions come from `docker/Dockerfile` (`RUST_VERSION`, `TRUNK_VERSION`), through `.github/actions/rust`. The Docker builds use the same versions, so local and CI builds match.

## 3. Versions

- **Semver.** `make release BUMP=patch|minor|major` (default `patch`), or `VERSION=x.y.z`. The first release is `0.2.0` (`make release BUMP=minor`): `0.1.0` has been the unreleased development version.
- **One version for the workspace.** Every crate has `version.workspace = true`, so a release sets `[workspace.package] version` alone. `c6_core::VERSION`, which workers send to the head and which database upgrades record, is that version.
- **Tags:** annotated, `v0.2.0`. The Release workflow refuses a tag that doesn't match the workspace version.
- **Package versions:**
  - at a release tag: `0.2.0` (deb and brew alike);
  - between releases: `0.2.0+git<commit time>.<commit>` (deb) and `0.2.0.<commit time>` (brew).

  Both sort after `0.2.0` and before `0.2.1`, so upgrades go the right way. The Makefile picks the right one by itself: `RELEASE_TAG` is set when `HEAD` carries `v<crate version>`.

## 4. Cutting a release

`make release [BUMP=… | VERSION=…]` runs `packaging/release/release.sh` on your machine:

1. **Checks:**
   - on `main`, with a clean tree, at `origin/main`;
   - the version is newer than the current one;
   - its tag doesn't exist.
2. **The release commit:**
   - sets the version, and refreshes `Cargo.lock` offline;
   - puts a new section at the top of `CHANGELOG.md` (§5);
   - commits `Release 0.2.0` and tags `v0.2.0`.

   A failure along the way puts everything back.
3. **Push:** asks first (`YES=1` skips the question), then `git push --atomic origin main v0.2.0`. Not pushed, the commit and tag stay local; `git tag -d v0.2.0 && git reset --hard HEAD~1` undoes them.

The tag starts the **Release workflow**:

| Job | Runner | Does |
|---|---|---|
| check | ubuntu-24.04 | the tag matches the workspace version |
| test | Test workflow | Linux and macOS suites |
| linux | ubuntu-24.04, ubuntu-24.04-arm | `make docker-deb-test`: the .debs and `carbon6-<v>-linux-<arch>.tar.gz`, built on Ubuntu 22.04 natively per architecture, then installed under systemd, run, upgraded and removed |
| macos | macos-15 (Apple Silicon), macos-15-intel | `make brew-test`: bottles and binary tarballs, then installed from a scratch tap, the head started with `brew services`, a worker and the memex attached, stopped with SIGTERM |
| publish | ubuntu-24.04, after all the others | `packaging/release/publish.sh` (§6) |

**Dry run:** started by hand (Actions → Release → Run workflow, or `gh workflow run release.yml`), the workflow builds and tests everything and keeps the artifacts on the run. `publish` is skipped. Do this once before the first release.

## 5. Changelog

- **`CHANGELOG.md`, newest first.** Each release adds a section: `## 0.2.0 (2026-10-15)`, then one bullet per commit since the previous tag, its one-line message as written.
- **Release commits are left out** of the list.
- **The first release lists every commit** since the start of the repository.
- **The same section is the GitHub release's notes.**
- **Preview:** `make release-notes` prints the next section without changing anything.

## 6. Publishing

`publish.sh <version> <dir>` runs in the Release workflow, on the artifacts of the other jobs:

1. **The formulas,** written by `packaging/brew/formulas.sh`:
   - one per binary, covering both architectures (`on_arm` / `on_intel`);
   - with a bottle for each;
   - downloading from `https://github.com/carbon6-dev/carbon6/releases/download/v<version>/`.
2. **`SHA256SUMS`** of every file, and **the notes:** the version's section of `CHANGELOG.md`.
   Also **`release.json`**, signed into `release.json.sig` with the secret `RELEASE_SIGNING_KEY` (`manifest.sh`). Without the secret it is unsigned, and machines don't update themselves to that release.
3. **The GitHub release** `v<version>`, with the files below. If the release already exists (a re-run), missing files are uploaded.
4. **The tap:** the formulas are committed to `carbon6-dev/homebrew-carbon6`, so `brew install carbon6-dev/carbon6/c6h` works and `brew upgrade` finds new releases.
   - This needs the repository secret `TAP_TOKEN`: a fine-grained token with *Contents: read and write* on the tap repository only.
   - Without the secret, the formulas are only attached to the release, and the run says so.

The files on a release:

| File | What it is |
|---|---|
| `c6h_<v>_amd64.deb`, `_arm64.deb` (and `c6w`, `c6`; `c6m` until 0.2.2, since the memex is part of the head) | Ubuntu packages with systemd units, for 22.04 and later |
| `carbon6-<v>-linux-amd64.tar.gz`, `-arm64` | the four binaries, for Linux without packages |
| `c6h-<v>.arm64_big_sur.bottle.tar.gz`, `.big_sur.` (and the others) | Homebrew bottles, Apple Silicon and Intel, for macOS 11 and later |
| `c6h-<v>-darwin-arm64.tar.gz`, `-x86_64` (and the others) | each binary for macOS without Homebrew; also the formulas' source |
| `c6h.rb` and the other formulas | the formulas, as pushed to the tap |
| `SHA256SUMS` | checksums of all of the above |
| `release.json`, `release.json.sig` | the manifest carbon6 updates itself from, and its Ed25519 signature ([docs/updates.md](updates.md) §2) |

## 7. Make targets

| Target | Does |
|---|---|
| `make release [BUMP=… \| VERSION=…]` | §4: checks, release commit, tag, push |
| `make release-notes` | prints the next changelog section |
| `make docker-deb-test [ARCH=…]` | the Linux job: builds and tries the .debs under systemd |
| `make brew-test` | the macOS job: builds and tries the bottles. It installs into this Mac's Homebrew, so it refuses where c6h etc. are installed or :7700 is in use, and removes all it installed |
| `make brew-dist` | bottles and formulas (with `file://` URLs) into `dist/brew/`, without a tap |

## 8. Setting it up (once)

1. **Dry run:** run the Release workflow by hand on `main` (§4) and check that every job passes.
2. **Tap repository:** create a public, empty `carbon6-dev/homebrew-carbon6`.
3. **Token:** make a fine-grained token for it with *Contents: read and write*, and add it to `carbon6-dev/carbon6` as the Actions secret `TAP_TOKEN`.
4. **Release:** `make release BUMP=minor` cuts `0.2.0`.
5. **Signing key** for self-updates: `gh secret set RELEASE_SIGNING_KEY < ~/.config/carbon6-release/signing-key.pem`. Its public half is `c6_core::update::RELEASE_KEYS`.

## 9. Open questions

- **Upgrade check across releases.** A release could keep a small `c6h.db` and `memex.db` (`packaging/release/fixtures/<version>/`), and the Release workflow could run `c6h upgrade --check` from the previous release's copies (and of its knowledge store). That catches a migration that breaks upgrades before it ships.
- **The commit in `--version` between releases.** Today a build after `0.2.0` reports `0.2.0`. A build script could add `+git<commit>`.
- **An apt repository.** A signed one, on GitHub Pages, would let `apt upgrade` pick up releases. It needs a signing key in the repository's secrets.
- **Docker images.** `carbon6/c6h` and `carbon6/c6w` could be pushed to `ghcr.io` by the Release workflow.

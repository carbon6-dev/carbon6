# Releases — Design

Status: design only (2026-10-15). Nothing here is built yet.

## 1. Goals

- **One command cuts a release:** `make release BUMP=minor` checks, versions, writes the changelog, builds every artifact, tests them, commits and tags.
- **Built here, not in CI.** Everything runs on the release Mac: Docker builds the Linux packages (as `make docker-deb-all` does today), the local toolchain builds the macOS bottles. No GitHub Actions.
- **Cutting is local; publishing is a separate step.** `make release` touches only this checkout and `dist/`. `make release-publish` is the one step that pushes and uploads, run when you choose to.
- **Nothing half-done.** A release that fails a check leaves no commit, no tag and no artifacts behind.

**Not goals (for now):** an apt repository (so `apt upgrade` would find new versions), signed packages, Docker images on a registry, Windows.

## 2. Versions

- **Semver, from the last tag.** `BUMP=patch|minor|major` (default `patch`), or `VERSION=x.y.z` to set it. The first release is `0.2.0`: `0.1.0` has been the unreleased development version.
- **One version for the workspace.** Every crate takes `version.workspace = true` (today several set `0.1.0` themselves), so the release sets `[workspace.package] version` alone, and `Cargo.lock` follows. `c6_core::VERSION` (sent by workers to the head, recorded with database upgrades) is that version.
- **Tags:** annotated, `v0.2.0`.
- **Package versions:**
  - a release: `0.2.0` (deb and brew alike);
  - a build between releases, as today: `0.2.0+git<commit time>.<commit>` (deb) and `0.2.0.<commit time>` (brew).

  Both sort after `0.2.0` and before `0.2.1`, so upgrades go the right way whichever you install.
- **Telling builds apart:** between releases a build still reports `0.2.0` from `--version`. Adding the commit to the version string (`0.2.0+git…`, from a build script) is a small follow-up (§8).

## 3. `make release`

`make release [BUMP=patch|minor|major | VERSION=x.y.z]`, in order:

1. **Checks:**
   - on `main`, with a clean tree;
   - the new version is greater than the last tag, and its tag doesn't exist;
   - Docker, the local toolchain and Homebrew are there.
2. **Tests:** `make docker-test` (the full suite, on Linux) and `cargo test` (macOS).
3. **The release commit:**
   - sets the version (§2);
   - adds the changelog section (§5);
   - commits `Release 0.2.0`, without tagging yet.
4. **Artifacts**, built from that commit into `dist/release/0.2.0/` (§4).
5. **Smoke tests:**
   - the deb smoke test on amd64 and arm64 (`make docker-deb-test`, both architectures);
   - the bottles installed from a scratch tap and run with scratch state, as `make brew-install` was checked.
6. **The tag:** `v0.2.0`, on the release commit. `SHA256SUMS` is written last.

If a step before the tag fails, the target resets the release commit and removes `dist/release/0.2.0/`. **`make release-undo`** does the same after a successful run, until the release is published: it deletes the tag, resets the commit and removes the artifacts.

## 4. Artifacts

In `dist/release/<version>/`:

| File | What it is | Built by |
|---|---|---|
| `c6h_<v>_amd64.deb` and `_arm64.deb` (and `c6w`, `c6m`, `c6`) | Ubuntu packages, built on 22.04, installable on 22.04 and later | Docker (`docker-deb-all`) |
| `c6h-<v>.arm64_big_sur.bottle.tar.gz` (and the others) | Homebrew bottles for Apple Silicon, macOS 11 and later | local toolchain (`brew`) |
| `c6h-<v>.big_sur.bottle.tar.gz` (and the others) | Homebrew bottles for Intel Macs, cross-built with the `x86_64-apple-darwin` target | local toolchain |
| `carbon6-<v>-linux-amd64.tar.gz` and `-linux-arm64.tar.gz` | the four binaries, for Linux without packages | Docker (`docker-build-all`) |
| `carbon6-<v>-darwin-arm64.tar.gz` and `-darwin-amd64.tar.gz` | the four binaries, for macOS without Homebrew | local toolchain |
| `Formula/*.rb` | the formulas, pointing at the release's download URLs | `packaging/brew/build.sh` |
| `SHA256SUMS` | checksums of everything above | `make release` |

**One build per platform.** The deb and the Linux tarball hold the same binaries, and so do the bottle and the macOS tarball.

## 5. Changelog

- **`CHANGELOG.md`, newest first.** Each release adds a section: `## 0.2.0 (2026-10-15)`, then one bullet per commit since the previous tag, its one-line message as written.
- **Release commits are left out** of the list.
- **The first release** lists everything since the start of the repository (166 commits today), under a one-line summary that you write.
- **The same section is the GitHub release's notes** (§6).
- **Editing:** to change the wording, edit `CHANGELOG.md` after `make release`, then `git commit --amend` and move the tag (`make release-undo` and run it again also works). Publishing reads the file, so your edit carries through.

## 6. `make release-publish`

Run after `make release`, when you're ready:

1. **Checks:**
   - `gh` is installed and signed in (`brew install gh`, `gh auth login`, once);
   - the tag is on `HEAD`;
   - `origin/main` fast-forwards to `HEAD`;
   - the release doesn't exist on GitHub yet.
2. **Pushes** `main` and the tag.
3. **The GitHub release:** `gh release create v0.2.0 --notes-file <the changelog section>`, uploading every file in `dist/release/0.2.0/` except the formulas.
4. **The tap:** the formulas go to a separate repository, `carbon6-dev/homebrew-carbon6`, created once by you.
   - Each formula's `url` and bottle `root_url` point at `https://github.com/carbon6-dev/carbon6/releases/download/v0.2.0/`. Homebrew asks for `<name>-<version>.<tag>.bottle.tar.gz` there, the names §4 uses.
   - The target clones the tap into a scratch directory, replaces `Formula/`, commits `c6h, c6w, c6m, c6 0.2.0` and pushes.
   - Anyone can then run `brew install carbon6-dev/carbon6/c6h`, and `brew upgrade` finds new releases.
5. **Prints** the release URL and the install commands.

**Requirement: public downloads.** Release assets of a private repository need a token to download, which neither `apt install ./…deb` instructions nor Homebrew's bottle downloads handle. Today `carbon6-dev/carbon6` is private (github.com answers 404 without sign-in), so this must be settled before §6 is built (§9).

**Partial failure:** a re-run picks up where it stopped. A pushed tag is skipped, an existing release gets the missing uploads, and the tap commit is skipped if it's unchanged. Nothing published is deleted automatically.

## 7. Make targets

| Target | Does |
|---|---|
| `make release [BUMP=… \| VERSION=…]` | §3: checks, tests, release commit, artifacts, smoke tests, tag |
| `make release-undo` | removes an unpublished release: tag, commit, artifacts |
| `make release-publish` | §6: push, GitHub release, tap |
| `make release-notes` | prints the changelog section the next release would add, without changing anything |

The work is done by `packaging/release/*.sh` scripts, which reuse the existing targets. `docker-deb-all`, `docker-deb-test` and `brew` gain a `PKG_VERSION` override so a release builds with the plain version.

## 8. Build order

1. **One workspace version:** every crate gets `version.workspace = true`, and `make release-notes` is added.
2. **`make release` and `make release-undo`**, with the artifacts this Mac already builds (debs, Apple Silicon bottles, Linux tarballs).
3. **Intel Mac bottles and the macOS tarballs:** add the `x86_64-apple-darwin` rustup target, build with `--target`, bottle tag `big_sur`.
4. **`make release-publish`:** `gh`, the tap repository.
5. **Follow-ups:**
   - the commit in `--version` between releases;
   - an upgrade check from the previous release's databases (§9).

## 9. Open questions

- **Upgrade check across releases.** `make release` could keep a small `c6h.db` and `memex.db` from each release (`packaging/release/fixtures/<version>/`) and run `c6h upgrade --check` / `c6m upgrade --check` from the previous one. That catches a migration that breaks upgrades before it ships. It's worth having once there are real installs to protect.
- **Private repository (blocks §6).** `carbon6-dev/carbon6` is private today, so §6's public downloads don't work. The choices:
  - make the repository public;
  - a public repository for releases only (`carbon6-dev/releases`: assets and the tap, no source);
  - a private tap with a token (`HOMEBREW_GITHUB_API_TOKEN` and a download strategy for private assets): fine for your own machines, unusable for anyone else;
  - your own server.
- **An apt repository.** Today a new release has to be installed by hand (`apt install ./c6h_….deb`). A signed apt repository (on GitHub Pages, or a server you run) would let `apt upgrade` pick releases up. It needs a signing key.
- **Docker images.** `carbon6/c6h` and `carbon6/c6w` could be pushed to `ghcr.io` with the same tag.

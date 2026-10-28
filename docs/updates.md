# Updating carbon6 from GitHub releases — Design

Status: built (2026-10-17). Decided: ask by default, signed releases, busy workers forced after 24 h. Not yet released: the first release with it is the first a machine can update *from*; updating *to* it is by hand.

## 1. Goals

- **A release on GitHub reaches every machine.** The head and every worker move to it on their own, or after one approval, without `brew upgrade` or `apt install` on each machine.
- **Nothing runs that the release didn't sign.** A binary is installed only if its checksum is in a release manifest signed by carbon6's release key. The public half of that key is built into every binary.
- **Nothing breaks mid-work.** A worker waits until its sessions are idle before it updates. The head updates first, and workers follow.
- **A bad release undoes itself.** If the new version doesn't come back healthy, the machine returns to the previous one.
- **One place talks to GitHub.** The head checks for releases and tells the workers. Workers download only from the release URLs the signed manifest names.

**Not goals:** prereleases or channels other than stable, downgrades other than rollback, Windows, and updating the `c6` CLI on its own (it says when it is behind).

## 2. The release manifest

CI's publish job adds two files to every GitHub release:

- `release.json`:
  ```json
  {
    "version": "0.3.4",
    "date": "2026-10-17",
    "protocol_version": 1,
    "upgrade_from": "0.3.0",
    "manual": null,
    "notes": "…the CHANGELOG section…",
    "assets": {
      "c6h-0.3.4-darwin-arm64.tar.gz": "sha256:…",
      "c6h_0.3.4_amd64.deb": "sha256:…",
      "…": "…"
    }
  }
  ```
  - `upgrade_from`: the oldest version this one updates from automatically. Anything older is told to update by hand.
  - `manual`: steps a person must take (like stopping c6m, 0.2 → 0.3). A release with steps is never applied automatically. It becomes a Decision showing the steps.
  - `assets`: every file's checksum. It replaces `SHA256SUMS` as the source of truth, though `SHA256SUMS` stays for people.
- `release.json.sig`: an Ed25519 signature of `release.json`, made in the publish job.
  - The private key is the Actions secret `RELEASE_SIGNING_KEY`.
  - The public key is compiled into `c6_core` (`RELEASE_KEYS`, a list, so a key can be rotated by shipping the next key one release early).
  - Without the secret, the release is published with an unsigned `release.json` and a warning, and nothing updates itself to it.
  - Signing is `openssl pkeyutl -sign -rawin` (`packaging/release/manifest.sh`); a test checks that ring verifies what openssl signs.

`make release MANUAL="…" UPGRADE_FROM=x.y.z` writes `Manual:` and `Upgrade from:` lines into the release's CHANGELOG section, and the publish job reads them back. By default there are no manual steps, and any version updates.

## 3. Finding a release (the head)

- **Every 15 minutes (`update_check_secs`), at start, and on demand,** the head downloads the latest release's manifest: `GET https://github.com/carbon6-dev/carbon6/releases/latest/download/release.json`.
  - It downloads release files only, never GitHub's API: a household shares the API's 60 requests an hour without a token, and the head ran into 403s. Downloads aren't counted against it.
  - It sends the last `ETag`, so an unchanged manifest is a 304 and nothing more is fetched.
  - `latest` is never a draft or a prerelease. Its version is the signed manifest's (the API's tag, asset list and draft flags are no longer needed: the notes and every file's name and checksum are in the manifest).
- **When the manifest changed,** it downloads its `.sig` and checks the signature against `RELEASE_KEYS`.
  - A bad or missing signature is logged and shown in Settings → System. Nothing else happens.
  - A newer release's files are then taken from its own tag (`…/releases/download/v<version>/`), not from `latest`, which moves on with the next release.
- **It keeps the verified manifest** in the `updates` table (version, manifest, signature, where its files are, state) and shows it in Settings → System → Updates, with its notes.
- **A newer release replaces an older one** still waiting: the older one's Decision is withdrawn.

## 3a. The edge channel

For a machine that should run main as it is, not just releases (the developer's own):

- **What CI publishes.** Every green commit on main that isn't a release commit replaces one rolling GitHub pre-release, tagged `edge`. It carries that commit's packages (built anyway, docs/releases.md) and a `release.json` signed like a release's, whose notes are the commits since the last release (`packaging/release/edge.sh`). A pre-release is never `releases/latest`, so stable machines never see it.
- **Its version.** A build between releases is `<release>+git<commit time>.<commit>` (brew's files: `<release>.<commit time>`). Versions compare by release, then commit time (UTC), so an edge build after 0.3.10 sorts after 0.3.10 and before 0.3.11. Each binary knows its exact version, given at build time (`C6_BUILD_VERSION`, set by `make build`), and reports and compares that one (`c6_core::build()`).
- **The setting.** `channel`: `stable` (the default) or `edge`. On edge the head also downloads the `edge` release's manifest (`…/releases/download/edge/release.json`, with its own ETag; while CI replaces the release it isn't there, which isn't an error) and offers whichever is newer, it or the latest release. The mode applies as for releases, so **edge + auto** is continuous deployment: each green commit reaches the machine within a check interval.
- **Homebrew installs.** The tap has releases only, so an edge build replaces a Homebrew install's binary in place, as a tarball install would. The pending update records it, and a rollback puts the previous binary back. The next release goes through `brew upgrade` again.

## 4. When to update (policy)

The setting `updates`:

| Field | Values | Default |
|---|---|---|
| `mode` | `off` · `notify` · `approve` · `auto` | `approve` |
| `auto_for` | `patch` · `minor` (what `auto` applies without asking; higher needs approval) | `patch` |
| `window` | hours in the head's time zone, e.g. `02:00–05:00`; empty is any time | empty |
| `skip` | versions never to apply | `[]` |
| `drain_hours` | how long a worker waits for its sessions to go idle | `24` |
| `channel` | `stable` · `edge` (§3a) | `stable` |
| `parallel` | workers update all at once instead of one after another | `false` |

- **`approve`** files a Decision: "Update carbon6 to 0.3.4?", with the notes and the machines it will touch. Approving it applies the release.
- **`auto`** applies a release within `auto_for` at the next window. Anything bigger falls back to `approve`. So does anything with `manual` steps, or an `upgrade_from` newer than a machine's version.
- **Every update can be triggered by hand,** whatever the mode: Settings → System → Updates → Update now (`POST /api/v1/system/updates/{version}/apply`). The same Update now tries again what failed.

## 5. The protocol

### Messages

Over the calls the hub already carries, so no new protobuf messages, and an old worker simply answers "unknown method" (its update is marked failed):

- **Head → worker**, `update.apply` (an RPC): `{version, manifest, signature, base_url, deadline_ms}`.
  - `manifest` is `release.json` exactly as signed, and `signature` is its `.sig` in hex.
  - `deadline_ms` is when to update even if sessions are still running.
- **Worker → head**, `update.status` (a head call): `{version, state, method, detail}`.
  - `state` is one of `waiting` · `downloading` · `installing` · `restarting` · `done` · `failed` · `rolled_back`.
  - `method` is brew · deb · tarball · source (§6).
  - `detail` is the error, or what it waits on.

The worker checks the manifest's signature again itself, so a compromised head can't push code to workers. It refuses a version that isn't newer than its own.

### The order

1. **The head goes first.** It needs to be at least as new as the workers, since a newer worker may use messages an older head doesn't know.
   1. It downloads its file and checks the checksum against the manifest.
   2. It installs it (§6).
   3. It writes `data/updates/pending.json`: {from, to, tries, started_at}.
   4. It exits with code 75, the "restart me" code. Run by hand (no systemd or launchd), it runs the new version in its own place instead.
   5. The service manager starts the new version. Its database upgrade runs as today: backup first, all or nothing.
2. **The new head checks itself.** On start it reads `pending.json`.
   - Each start counts a try, before the database opens. After the database upgrade, the backup it made is written into `pending.json`.
   - If it got as far as serving (database upgraded, listening), the update has succeeded. It records that and deletes the file.
   - If the new version fails 3 times (the service manager restarts it each time), the fourth start runs the rollback (§7) and exits 75.
3. **The workers follow.** Once the head is healthy on the new version, it sends `update.apply` to each connected worker, one at a time, or all at once with `parallel: true` in the setting.
   - Each worker waits until none of its sessions is running a turn, for at most 24 hours (`drain_hours` in the setting).
     - At the deadline it updates anyway. The interrupted turns are marked interrupted and resume after the restart, like after a crash.
     - It keeps taking turns while it waits. A worker that's never idle updates at the deadline.
     - It reports `waiting` with the number of busy sessions.
   - Then it downloads, verifies and installs, reports `restarting`, and exits with 75. Its `pending.json` is in its state folder.
   - The new version, once attached, reports `done` (or `rolled_back`, from the old version after a rollback).
   - If a worker isn't back within 10 minutes, it's marked failed. A failed worker doesn't hold up the others; Update now tries it again. Workers that are offline update when they next connect.
4. **Settings → System** shows each machine's version and update state.
5. **Open pages notice.** The header always shows the head's version (`GET /api/v1/version`). A page asks again every 30 seconds and when it reconnects. When the head's version is no longer the one the page loaded with, a banner says carbon6 was updated and offers **Reload**.

## 6. Installing, by how carbon6 was installed

Each binary knows how it was installed from where it runs (`current_exe`):

| Method | Detected by | Install | Restart |
|---|---|---|---|
| **brew** | under `$(brew --prefix)/Cellar/` | `brew upgrade carbon6-dev/carbon6/<bin>` (which updates the tap first). Not the downloaded asset: brew stays the owner of the files, and checks the bottle against the formula's checksum. Only CI's publish job writes the tap. Afterwards, `brew list --versions` must show the new version. | exit 75. The formula's `keep_alive successful_exit: false` restarts it from `opt/`, which now points to the new keg. |
| **deb** | `/usr/bin/<bin>` on Linux | carbon6 runs as a non-root user (`c6h`, or the user in `c6w@`), so it can't install packages itself. Each .deb ships `<bin>-update.path` and `<bin>-update.service` (root, oneshot). The daemon leaves the .deb, `release.json`, its `.sig` and then `request` in `/var/lib/<bin>/update/`. For c6h that folder is the c6h user's; for c6w it is writable by everyone, sticky, since any user may run a worker. The path unit starts `<bin> update-install`, as root. It takes the request, checks the signature and the .deb's checksum itself, refuses anything not newer than itself, installs a private copy with `dpkg -i`, and writes `result`. The daemon waits for that, up to 10 minutes. | the package's `postinst` restarts the units, as today |
| **tarball** | anywhere else, with the binary's directory writable | the new binary is written next to the old one, then renamed over it (atomic). The old one is kept as `<bin>.previous`. | exit 75. Without a service manager (run by hand), it re-execs itself. |
| **source** | a debug build, or a binary in a folder it can't write | never updated. Its update fails with "built from source: update it yourself". | — |

All four methods use the same rollback trigger (§7).

## 7. Rollback

- **What "healthy" means:**
  - head: listening, its database upgraded, and its update checks passed;
  - worker: re-attached to the head with the new version.
- **On failure:**
  - tarball: put `<bin>.previous` back;
  - brew: the upgrade runs with `HOMEBREW_NO_INSTALL_CLEANUP=1`, so the old keg stays in the Cellar. Rollback points `$(brew --prefix)/opt/<bin>` back at it (brew has no `switch` any more), and the next `brew upgrade` puts things right. If the old keg is gone, the update is only marked failed.
  - deb: not rolled back. The update is marked failed, and the previous .deb is installed by hand. The database is already safe: a failed upgrade restores its backup.
- **The database** needs nothing extra: a failed upgrade already restores the backup. If the new version upgraded the database and then failed later, the rollback also puts back that upgrade's backup. The pending file names it.
- **A rolled-back version is added to `skip`,** and a Decision says what failed.

## 8. Security

- **Signature, not transport.** HTTPS to GitHub is necessary but not enough. A stolen GitHub token can replace release assets, but it can't sign a manifest without `RELEASE_SIGNING_KEY`.
- **Who can update:**
  - Workers update only on `update.apply` from their head, and only to a version their own built-in keys verify.
  - The head never sends a binary, only the manifest. Workers fetch the assets from the manifest's GitHub URLs.
- **No privilege in the daemons.** On Linux, only the root oneshot installs. It checks the signature itself and installs a copy nobody else can write. Anyone who can write the c6w update folder can only ask for a newer, signed release.
- **The key** was made on the release manager's machine (`~/.config/carbon6-release/signing-key.pem`, mode 0600). It lives there and in the `RELEASE_SIGNING_KEY` secret. Keep an offline copy: losing it means shipping a new key in a release signed by the old one.
- **Tests only:** debug builds take other keys, binaries and versions, set in code: the worker's `Overrides` and the head's `update_*` config. Release builds ignore the worker's.
- **Mode `off`** turns all of it off, including the GitHub check (for machines that mustn't call out).

## 9. Steps (built)

1. **Release side:** `release.json` and `.sig` (`packaging/release/manifest.sh`, from `publish.sh`), and `c6_core::update` (verify, versions, assets), with tests.
2. **Head:** the `updates` and `update_machines` tables, the setting, the GitHub check, the Decision, and Settings → System → Updates (`crates/c6-head/src/updates.rs`).
3. **Install methods, exit 75, `pending.json` and rollback,** shared by head and worker (`c6_core::update`). The tarball install and rollback are unit-tested.
4. **Workers:** `update.apply` and `update.status`, waiting for idle, ordering, and offline workers (`crates/c6-worker/src/update.rs`). `crates/c6-worker/tests/it/updates.rs` runs it end to end: a fake GitHub and a release signed with a test key; the head asks, updates and restarts; then the worker updates and reports done.
5. **Linux:** `<bin>-update.path` and `.service` in the debs. `make docker-deb-test` checks that they run, and refuse an unsigned update as root.
6. **Not built: CI that updates from release N−1** to the build under test, under brew and systemd. Until then, brew and deb installs are tested only up to the helper.

## 10. Open questions

- **Updating `c6`** (the CLI): only telling you, or also through brew/apt when the head updates on the same machine?

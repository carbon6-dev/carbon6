# Updating carbon6 from GitHub releases — Design

Status: designed (2026-10-17). Decided: ask by default, signed releases, busy workers forced after 24 h.

## 1. Goals

- **A release on GitHub reaches every machine.** The head and every worker move to it on their own, or after one approval, without `brew upgrade` or `apt install` on each machine.
- **Nothing runs that the release didn't sign.** A binary is installed only if its checksum is in a release manifest signed by carbon6's release key. The public half of that key is built into every binary.
- **Nothing breaks mid-work.** A worker waits until its sessions are idle before it updates. The head updates first, and workers follow.
- **A bad release undoes itself.** If the new version doesn't come back healthy, the machine returns to the previous one.
- **One place talks to GitHub.** The head checks for releases and tells the workers. Workers download only from the release URLs the signed manifest names.

**Not goals:** prereleases or channels other than stable, downgrades other than rollback, Windows, and updating the `c6` CLI on its own (it says when it is behind).

## 2. The release manifest

The Release workflow adds two files to every GitHub release:

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
  - The release fails if the secret is missing.

`make release` writes `upgrade_from` and `manual` from lines in the CHANGELOG section (`Upgrade from: 0.3.0`, `Manual:`). The default is the previous release, with no manual steps.

## 3. Finding a release (the head)

- **Every 6 hours, and on demand,** the head asks `GET https://api.github.com/repos/carbon6-dev/carbon6/releases/latest`.
  - It sends the last `ETag`, so an unchanged answer costs nothing against the rate limit.
  - It skips drafts and prereleases.
- **When it finds a newer version than its own,** it downloads `release.json` and its `.sig` and checks the signature against `RELEASE_KEYS`.
  - A bad or missing signature is logged and shown in Settings → System. Nothing else happens.
- **It keeps the verified manifest** in the `updates` table (version, manifest, signature, found_at, state) and announces it:
  - in Settings → System ("0.3.4 is available", with its notes);
  - to workers, over the hub (§5).

## 4. When to update (policy)

The setting `updates`:

| Field | Values | Default |
|---|---|---|
| `mode` | `off` · `notify` · `approve` · `auto` | `approve` |
| `auto_for` | `patch` · `minor` (what `auto` applies without asking; higher needs approval) | `patch` |
| `window` | hours in the head's time zone, e.g. `02:00–05:00`; empty is any time | empty |
| `skip` | versions never to apply | `[]` |
| `drain_hours` | how long a worker waits for its sessions to go idle | `24` |

- **`approve`** files a Decision: "Update carbon6 to 0.3.4?", with the notes and the machines it will touch. Approving it applies the release.
- **`auto`** applies a release within `auto_for` at the next window. Anything bigger falls back to `approve`. So does anything with `manual` steps, or an `upgrade_from` newer than a machine's version.
- **Every update can be triggered by hand,** whatever the mode: Settings → System → Update now, or `c6 update`.

## 5. The protocol

### Messages (hub.proto)

```proto
// Head → worker.
message UpdateAvailable {
  string version = 1;
  bytes manifest = 2;    // release.json, exactly as signed
  bytes signature = 3;   // release.json.sig
  bool apply = 4;        // false: just know; true: apply when idle
  uint64 deadline_ms = 5;  // apply by then even if not idle (0: wait forever)
}

// Worker → head.
message UpdateStatus {
  string version = 1;
  enum State {
    UNKNOWN = 0;
    WAITING = 1;      // for sessions to go idle
    DOWNLOADING = 2;
    INSTALLING = 3;
    RESTARTING = 4;
    FAILED = 5;
    ROLLED_BACK = 6;
  }
  State state = 2;
  string method = 3;   // brew · deb · tarball · source (§6)
  string detail = 4;   // the error, or what it waits on
}
```

`Hello` gains `string install_method = 8` and `string previous_version = 9`, the latter set on the first start after an update. These let the head tell a finished update from a rollback.

The worker checks the manifest's signature again itself, so a compromised head can't push code to workers.

### The order

1. **The head goes first.** It needs to be at least as new as the workers, since a newer worker may use messages an older head doesn't know.
   1. It downloads its asset to `data/updates/<version>/` and checks the checksum against the manifest.
   2. It installs the asset (§6).
   3. It writes `data/updates/pending.json`: {from, to, started_at}.
   4. It exits with code 75, the "restart me" code.
   5. The service manager starts the new version. Its database upgrade runs as today: backup first, all or nothing.
2. **The new head checks itself.** On start it reads `pending.json`.
   - If it got this far (database upgraded, listening), the update has succeeded. It records that in `updates` and deletes the file.
   - If the new version doesn't reach that point within 3 tries (the service manager restarts it), the third start runs the rollback (§7). This is counted in `pending.json` by a small check in `main()` before anything else.
3. **The workers follow.** Once the head is healthy on the new version, it sends `UpdateAvailable{apply: true}` to each worker, one at a time, or all at once with `parallel: true` in the setting.
   - Each worker waits until none of its sessions is running a turn, for at most 24 hours (`drain_hours` in the setting).
     - At the deadline it updates anyway. The interrupted turns are marked interrupted and resume after the restart, like after a crash.
     - While waiting, it takes no new turns. They queue on the head, as they do for a worker that's away.
     - It reports `WAITING` with the number of busy sessions.
   - Then it downloads, verifies and installs, reports `RESTARTING`, and exits with 75.
   - On reconnect, `Hello.version` = new and `previous_version` = old means it's done.
   - If a worker isn't back on the new version within 10 minutes, it's marked failed and the next worker waits for you. Workers that are offline update when they next connect.
4. **Settings → System** shows each machine's version and update state.

## 6. Installing, by how carbon6 was installed

Each binary knows how it was installed from where it runs (`current_exe`):

| Method | Detected by | Install | Restart |
|---|---|---|---|
| **brew** | under `$(brew --prefix)/Cellar/` | `brew update && brew upgrade carbon6-dev/carbon6/<bin>`. Not the downloaded asset: brew stays the owner of the files. After the upgrade, the new binary's checksum is compared with the manifest's bottle checksum. | exit 75. The formula's `keep_alive successful_exit: false` restarts it from `opt/`, which now points to the new keg. |
| **deb** | `/usr/bin/<bin>`, owned by the package (`dpkg -S`) | carbon6 runs as a non-root user (`c6h`, or the user in `c6w@`), so it can't install packages itself. The .deb ships `c6-update.path` and `c6-update.service` (root, oneshot). The path unit watches `/var/lib/carbon6/update/request`, which the daemon writes (group-writable to `carbon6`). The root unit checks the request's .deb against the signed manifest itself, then runs `dpkg -i`. | the package's `postinst` restarts the units, as today |
| **tarball** | anywhere else, with the binary's directory writable | the new binary is written next to the old one, then renamed over it (atomic). The old one is kept as `<bin>.previous`. | exit 75. Without a service manager (run by hand), it re-execs itself. |
| **source** | built by `cargo` (`debug_assertions`, or a git build version) | never updated. Settings shows "built from source". | — |

All four methods use the same rollback trigger (§7).

## 7. Rollback

- **What "healthy" means:**
  - head: listening, its database upgraded, and its update checks passed;
  - worker: re-attached to the head with the new version.
- **On failure:**
  - tarball: put `<bin>.previous` back;
  - brew: the upgrade runs with `HOMEBREW_NO_INSTALL_CLEANUP=1`, so the old keg stays in the Cellar. Rollback points `$(brew --prefix)/opt/<bin>` back at it (brew has no `switch` any more), and the next `brew upgrade` puts things right. If the old keg is gone, the update is only marked failed.
  - deb: the old .deb kept in `/var/lib/carbon6/update/` is installed again.
- **The database** needs nothing extra: a failed upgrade already restores the backup. If the new version upgraded the database and then failed later, the rollback also puts back that upgrade's backup. The pending file names it.
- **A rolled-back version is added to `skip`,** and a Decision says what failed.

## 8. Security

- **Signature, not transport.** HTTPS to GitHub is necessary but not enough. A stolen GitHub token can replace release assets, but it can't sign a manifest without `RELEASE_SIGNING_KEY`.
- **Who can update:**
  - Workers update only on `UpdateAvailable` from their head, and only to a version their own built-in keys verify.
  - The head never sends a binary, only the manifest. Workers fetch the assets from the manifest's GitHub URLs.
- **No privilege in the daemons.** On Linux, only the root oneshot installs, and it checks the signature itself.
- **Mode `off`** turns all of it off, including the GitHub check (for machines that mustn't call out).

## 9. Steps

1. **Release side:**
   - the signing key and `release.json` + `.sig` in `publish.sh`;
   - `upgrade_from` and `manual` from the CHANGELOG;
   - the key in `c6_core`;
   - a verify function, with tests.
2. **Head: finding and policy.** The `updates` table and setting, the GitHub check, Decisions, and Settings → System.
3. **Install methods and exit 75,** for the head: brew, tarball, source; `pending.json`; and rollback.
4. **The protocol for workers:** `UpdateAvailable`/`UpdateStatus`, idle-wait, ordering, and offline workers.
5. **Linux:** `c6-update.path`/`.service` in the debs, and the smoke tests in `make docker-deb-test` updating from the previous release.
6. **CI:** `make brew-test` and `docker-deb-test` install release N−1, then update to the build under test through a local fake GitHub (a manifest signed with a test key).

## 10. Open questions

- **Updating `c6`** (the CLI): only telling you, or also through brew/apt when the head updates on the same machine?

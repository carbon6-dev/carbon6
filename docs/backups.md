# Backups

Status: decided with you (2026-11-04); being built.

The head keeps everything on its own disk: its database, the knowledge store and each realm's, the blobs, the CA and the master key. You want it on a local SSD, with copies on a NAS (an NFS mount) and in S3 that stay current, can't be read by whoever holds the storage, and cost little while nothing changes. Copying 30 GB of databases every hour, or restic over a `VACUUM INTO` copy, reads and writes everything each time and only goes back to the last run.

## 1. What you get

- **Lose at most about a minute.** Changes reach each destination within the segment interval (a setting, 30 s by default) while the head is up.
- **Any point in time.** A restore goes back to any moment the destination still keeps, not only to the last run.
- **Cheap while quiet.** What travels is what changed: the databases' new pages, new blobs. A weekly full copy reads each database once and sends only the chunks the destination doesn't have.
- **Unreadable without your key.** Everything is encrypted on the head to a public key. The private key that restores stays with you, offline. A stolen bucket, share or head settings can't read old backups.
- **Two destinations at once,** each at its own pace: if S3 is unreachable for an hour, the NAS stays current and S3 catches up.
- **Checked.** A test restore into a temporary folder runs on a schedule and says when one fails.

## 2. What is backed up

| | How |
|---|---|
| `c6h.db`, `knowledge/memex.db`, `knowledge/realms/*/memex.db` | base copies plus log segments (§3) |
| `knowledge/blobs/` | each blob once, by its hash (they never change) |
| `pki/`, `master.key`, the head's config file | with every base copy |
| `backups/`, `knowledge/**/backups/` (pre-upgrade copies), `updates/` | not backed up |

A database that appears later (a new realm) is picked up when it opens.

## 3. Databases: base copies and log segments

- **The head checkpoints.** carbon6 opens its databases in WAL mode. The head turns off SQLite's automatic checkpoints (`wal_autocheckpoint = 0`) and checkpoints each database itself: after the log grows past a size, or the interval passes.
- **Segments.** Before a checkpoint, the frames added to the log since the last segment are read, compressed (zstd), encrypted and written to every destination as one segment, named by database, generation and position. The checkpoint runs only once every enabled destination has the segment, or after a limit (a setting, 15 minutes by default), so a dead destination can't grow the log without bound. A destination that misses segments starts a new generation (below).
- **Generations.** A generation is a base copy plus the segments after it. A new one starts weekly (a setting), after a restore, after a schema upgrade, and when segments were missed.
- **Base copies without a full copy.** While a base copy is read, checkpoints pause, so the database file doesn't change: writes go only to the log, and the log's frames become the generation's first segments. The file is read in 4 MB chunks. Each chunk is hashed with a key, and only chunks the destination doesn't have are written. A database that changed a little since last week sends a little.
- **Upgrades.** A schema upgrade ends the generation before migrating and starts a new one after.

## 4. Destinations and encryption

- **Kinds.** A folder (an NFS mount, an external disk) and S3-compatible storage (AWS, R2, Backblaze, MinIO). Both store objects by name with put, get, list and delete. S3 keys and secrets are carbon6 secrets.
- **Set in Settings → Backups,** like connections: name, kind, where, retention, enabled. Nothing about your NAS or bucket is in code.
- **Encryption.** Each object is encrypted with a fresh key (XChaCha20-Poly1305), and that key is wrapped to the backup recipient, an X25519 public key in the format of [age](https://age-encryption.org). Settings → Backups makes the key pair and shows the private key once, for you to keep. You can also paste a public key of your own.
- **Names reveal nothing.** Chunks and blobs are named by a keyed hash of their content. The hash key is itself stored encrypted to the recipient, so dedup works without showing what's inside.
- **A manifest per generation,** encrypted, lists its chunks, blobs and segments with their hashes. Every object is checked on upload and on restore.

## 5. Retention

Each destination keeps the last N generations (a setting, 4 by default: about a month at weekly) with their segments. Pruning removes generations past that, then chunks and blobs no kept manifest names. A blob stays as long as any kept generation names it.

## 6. Restore

- `c6h restore --from <destination> [--at <time>] [--to <folder>]`, with the head stopped, rebuilds a data folder. It writes the base copy's chunks back into each database, replays segments up to the time, and puts back the blobs, `pki/`, `master.key` and the config. It asks for the private key (or a file holding it).
- **Moving the head** is a restore on the new machine. Workers trust it as before (same CA); point them at its address.
- **Test restores** run on a schedule (a setting, weekly by default) into a temporary folder, from each destination in turn. Each one checks every database with `PRAGMA integrity_check` and compares row counts with the head's at the same point. This needs the private key, so it runs only if you've given the head a restore key for testing (stored as a secret). Without one, it checks every object's hash and that the manifests chain, which proves the files are there and whole but not that they decrypt.

## 7. Seeing it

- **Settings → Backups:** per destination, the last segment and how far behind it is, the current generation and its base copy, the size kept, and the last test restore and what it found.
- **Notifications** when a destination falls behind past a limit (a setting, 10 minutes by default), a base copy or upload keeps failing, or a test restore fails.
- **Operations** in the catalog (`backups.*`: status, destinations get and put, run a base copy now, test now) for the UI, the CLI and Ask. They're administration: agents don't get them (docs/agent-actions.md).

## 8. Steps

1. **Destinations and encryption.** The folder and S3 kinds; the key pair, wrapping and keyed names; blobs, `pki/`, `master.key` and the config to every destination.
2. **Log segments.** Checkpoints taken over by the head, segments to every destination, the checkpoint limit, generations started on missed segments and on upgrades.
3. **Base copies.** Paused checkpoints, chunk dedup, weekly generations, retention and pruning.
4. **Restore.** `c6h restore` to a point in time; test restores with and without the restore key.
5. **Settings → Backups,** status, notifications and the catalog operations.

Every step is tested against a folder destination and a stand-in S3 on loopback, with no time-based waits (docs/testing.md).

## 9. As built

Built 2026-11-04 on branch `backups`, one commit per step (§8). The code is in `crates/c6-head/src/backups/` (`mod.rs` settings, passes and status; `crypt.rs`; `dest.rs`; `wal.rs`; `capture.rs`; `gens.rs`; `restore.rs`), `crates/c6-core/src/wal.rs`, `crates/c6-ui/src/backups.rs`, and `c6h restore`.

- **The head always checkpoints its databases itself,** backups on or not. Every connection to a database in the head's folder (`c6_core::wal::manual_under`, set when `c6h.db` opens) has `wal_autocheckpoint = 0`, `journal_size_limit` 64 MB, and SQLite's checkpoint on close turned off (`SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE`, through `libsqlite3-sys`). The log therefore keeps every change, across restarts too, until the head has read it. `c6h` subcommands (upgrade, login link…) open their databases the same way, so nothing they write is missed either.
- **A pass, per database, every interval:** on its own connection, `BEGIN IMMEDIATE` (no one else can write), read the frames committed since the last pass, write them as a segment into the local spool (`<data>/backup-state/spool/`, plaintext like the databases beside it), `PRAGMA wal_checkpoint(PASSIVE)` on a second connection, `ROLLBACK`. The lock is held for milliseconds. Nothing waits on a destination.
- **Changed from §3: the checkpoint doesn't wait for destinations.** Segments wait in the spool instead, and each destination's sender drains it at its own pace. The limit is on the spool: a destination with more than `spool_limit_mb` unsent falls back to a new base copy and stops holding the spool. The WAL therefore never grows because a destination is down.
- **Changed from §3: generations are per destination,** so one slow destination never makes the others take base copies.
- **The line of changes** breaks (an epoch, after which earlier base copies take no more segments) when:
  - the database file changed but no checkpoint by the head changed it (another program wrote and checkpointed it, or a failed upgrade put its copy back);
  - the log started over without a full checkpoint by the head;
  - its `upgrades` table has a newer upgrade, which covers the design's "after a schema upgrade".

  The next pass at each destination then takes a new base copy.
- **Base copies** pause that database's checkpoints for the whole copy, upload included. If its log passes 1 GB meanwhile, the copy gives up and the chunks it sent are kept, so the next try sends fewer. After a copy fails partway it waits 5 minutes. One that couldn't reach the destination read nothing, and tries again next pass.
- **Changed from §4: not the age format.**
  - **Encryption:** X25519 to your key, then HKDF-SHA256, then a ChaCha20-Poly1305-wrapped data key, with an XChaCha20-Poly1305 body. Content is compressed with deflate (`flate2`, already in the tree) rather than zstd.
  - **Keys** are text: `c6pub1…` and `c6key1…`.
  - **The head's own key** names objects (HMAC-SHA256) and encrypts the indexes it reads back. It is kept under the master key and, wrapped to your key, at `keys/head`.
- **Objects at a destination:**
  - `keys/head`, `keys/recipient`;
  - `files/<ms>`: the CA, `master.key` and the config file;
  - `blobs/<name>` and `bloblog/<ms>-…`, which say where each blob goes back to;
  - `chunks/<name>`;
  - `db/<db>/<generation>/{manifest, index, wal0, seg/<seq>-<ms>}`.

  A generation is complete once its manifest is there.
- **Retention** keeps the newest `keep` complete generations per database and the chunks their indexes name. It also keeps the files objects from the oldest kept generation on, and always the newest one. **Not done:** blobs are never pruned at a destination.
- **Changed from §6: `c6h restore`.**
  - Its flags are `--from <folder | s3://bucket/prefix> [--endpoint] [--region] [--at <time>] [--to <folder>] [--key-file]`. The key comes from `--key-file`, else `C6_BACKUP_KEY`, else it asks.
  - It refuses a folder that already has a `c6h.db`.
  - It writes the config file as `<name>.restored`, because its `data_dir` may differ, and checks every database with `PRAGMA integrity_check`.
  - It can't restore by a destination's name from the head's settings: those settings are inside what is being restored.
- **Changed from §6: test restores.**
  - With your key kept, a full restore into a temporary folder, with an integrity check of each database. It doesn't compare row counts with the live head.
  - Without it, a check that every object of each database's newest base copy is there. The manifest and log are hashed, and about one chunk in sixteen is read back and hashed, rather than reading the whole backup back each week.
  - A failure notifies (event `backup`, on by default, as is "behind").
- **Settings → Backups** (a console page) covers the key (made there, the private key shown once), destinations, limits, status per destination and per base copy, the restore key for tests, and Send now, New base copies now and Test restore now. Its operations are `system.backups.*`, which are administration, so agents don't get them.
- **Cost while quiet,** measured in the tests:
  - Passes with nothing changed sent no objects and no bytes.
  - Each pass, per database, takes the write lock for a moment, reads the log's header, and runs a checkpoint only if the log has frames.
  - The blob scan reads only blob folders whose time changed since the destination last had all of theirs.

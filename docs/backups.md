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

# Machines

## Purpose and layout

Machines replaces the Workers navigation destination; `/workers` remains an
alias of `/machines`, and existing `workers.*` APIs keep their names. The head
appears first, followed by realm-visible workers. Existing worker controls and
detail layout remain available. Cards show online state, role, version, CPU,
memory and the fullest real filesystem. Details show resource history,
partitions, attributed sessions and an explicit disk investigation action.
The head also reports uptime, its data directory and database sizes, connected
workers and update state. Charts are inline SVG, not a base-bundle dependency,
with readable labels and responsive light/dark styles.

## Collection and transport

A shared stateful sampler collects at a configurable 15-second interval on
Linux and macOS. It reports CPU utilization and core count, load averages,
available/used/total memory, swap, real mounted filesystem capacity, and inode
use where supported. Linux additionally reports `/proc/pressure` and the
boot-relative `oom_kill` counter. Missing platform metrics remain unknown,
never fabricated zeroes. Pseudo, overlay and snap-loop filesystems are excluded.
Process-tree CPU/RSS attribution uses worker-owned session process roots,
including held Claude Code processes; overlapping roots must not double-count.
New worker-link fields use new protocol tags and old peers can ignore them.

## Storage and query

Telemetry is observational and must not delay event ingest or acknowledgements.
A bounded background queue batches samples into a separate SQLite database in
the head data directory, avoiding contention on c6h.db. Retain 15-second points
for 24 hours, five-minute points through seven days and hourly points through
90 days, with configurable retention. Compaction uses deterministic timestamp
buckets, keeps peaks/warnings visible and runs away from the main writer.
Queries bound both time range and output count. An offline machine retains its
last sample but never looks fresh. Clock and sampler seams make tests independent
of wall-clock waits.

## Disk investigation

“What's using the disk?” starts a bounded background read on the selected
machine, not an arbitrary command. It investigates the state directory,
worktrees/scratch and large home directories (including targets and .cargo),
and temporary files. Traversal does not follow symlinks, stays bounded by elapsed
time and entry count, tolerates unreadable paths, and reports partial results,
folder sizes and last-modified times. It is an explicit authorized machine
operation, not an automatically callable free read for agents. No deletion or
cleanup is performed.

## Warnings and admission

By default warn below 10% free disk, sustained low available memory, or a new
OOM kill. Warnings appear on cards and use the existing notification plumbing
through a default-enabled `machine` event. Deduplicate unchanged warnings and
notify again only on a new episode. Include the filesystem holding the worker
journal or head database so SQLite's own ENOSPC error is not the first warning.
Below 5% free on relevant writable disks, new subagents and kit jobs wait with
“queued: disk nearly full”; existing work is not killed. Waiting is cancellable
and admission resumes when space returns. Thresholds and sampling interval are
settings, not predictions or guarantees against resource exhaustion.

## API and verification

`machines.list`, `machines.samples` and `machines.disk_scan` expose the inventory,
bounded time series and explicit investigation with worker-equivalent realm
checks. The head is a distinct role, not a fake enrolled worker. Existing
workers APIs and enrollment remain compatible.

Tests inject process trees, samples and timestamps: transport and queries,
filtering, attribution, retention/downsampling, warning episodes, admission
hold/release and head inventory. UI shaping tests cover empty/single/constant
series and chart domains. Verify workspace/native and wasm clippy, production
Docker UI and size budget, and one final full test after rebasing. Visual checks
use only a scratch head and worker at ports 18690/18691, desktop and 390px,
light and dark; never the user's live services or state directories.

This document is the implementation contract; any remaining limitations must
be recorded in the final report rather than presented as completed behavior.

### Configuration

The head's TOML settings (all optional) are:

```toml
[machines]
sample_secs = 15
raw_retention_secs = 86400
five_minute_retention_secs = 604800
hourly_retention_secs = 7776000
disk_warn_percent = 10.0
disk_hold_percent = 5.0
memory_warn_percent = 5.0
memory_warn_samples = 3
maintenance_rows = 256
```

Sampling and disk admission settings travel in optional Welcome fields; old
peers retain their defaults. Worker environment fallbacks are
`C6_MACHINE_SAMPLE_SECONDS` and `C6_MACHINE_DISK_HOLD_PERCENT`. Admission reads
cached resource measurements from bounded blocking-pool probes rather than
performing filesystem calls on Tokio threads. State, worktrees, kit scratch,
temporary storage and home are mandatory checks; a session/job's additional
cwd is scoped to that launch and cannot block unrelated future work. Queued
kit RPCs carry cancellation IDs, so abandoning or timing out a caller cancels
the worker's wait instead of starting the job later. A blocked OS filesystem
call can occupy a bounded probe slot, but does not prevent waiter cancellation.

### Head backend bounds and history caveats

`GET /api/v1/machines` returns inventory; `GET /api/v1/machines/{id}/samples`
accepts `from_ms`, `to_ms`, and `limit` (default 480, maximum 2000). History
requests are limited to the configured retention (and at most 366 days), 16,384
input rows and 16 MiB of stored JSON. A request exceeding an input budget fails
with an instruction to narrow its range, rather than silently dropping history.
`POST /api/v1/machines/{id}/disk_scan` returns `MachineDiskScan` after the bounded
blocking-pool operation; there is no polling/job registry. Investigation is
console-only and never an automatic agent read, with one active head scan.
Head administration metadata (including database filenames and its data disk)
is console-only; sample session attribution is filtered to the caller's realm.

Accepted sample timestamps are normalized to head receipt time. Source clocks
must be within five minutes of receipt; older/far-future observations are
discarded rather than made to look fresh. Payloads are capped at 128 KiB,
512 sessions, 128 disks and 32 pressure entries; the mailbox holds 128 samples.
Telemetry is best effort and may drop samples under load. `[machines]` in head
TOML configures sampling/retention/thresholds and `maintenance_rows` (default 256,
maximum 4096); each indexed maintenance transaction handles at most that many
expiration/compaction operations, so a backlog is drained over later sweeps.

Compacted points are peak envelopes, not simultaneous observations. CPU/load,
used memory/swap, inode use and OOM counters retain maxima; available memory
and disk retain minima. Pressure strings retain the latest raw observation,
not a pressure peak. Bucket-maximum OOM counters cannot reconstruct reboot
counter resets or exact kill timing; warning episodes are evaluated separately
on incoming raw observations. Session envelopes retain at most 512 top CPU
peaks, disks at most 128 fullest filesystems. Scanner roots are prioritized
and scanned with per-root shares of the total time/entry budget; skipped broad
ancestors, inaccessible paths and limits are reported as partial. A single
unresponsive filesystem syscall is not portably interruptible.

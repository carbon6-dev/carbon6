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
Samples without attribution roots do not enumerate processes. Attribution
disables sysinfo's Linux process-descriptor cache and excludes task/thread
enumeration. A worker-owned permit bounds sampling across reconnects; cancelling
the connection abandons the async wait without allowing another blocked probe.

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
operation, not an automatically callable free read for agents. Investigation
itself never deletes files. A separate build-folder list includes recognized
Rust target and incremental directories, sizes, modification/last-use times,
repository/session attribution, carbon6 ownership, archive state and whether
the folder is active or the scan is partial. Last use is recorded use or
observed descendant modification, not unreliable filesystem atime. Target
sizes include incremental descendants, so rows must not be summed as unique
disk usage.

Current placement is projected durably when sessions are created/imported and
as events are appended, so a move remains attributed after a long conversation.
Legacy journals backfill in 64-event pages, at most 256 events per investigation,
with checkpoints retained across restarts. Until a session's placement is proven,
attribution is partial and cleanup is refused. Missing or unreadable history
stays fail-closed; it is never replaced by a guess from the original workspace.

Placement reads admit up to 256 journal sessions and 256 retained references.
Build discovery is bounded to 64 roots, 100,000 entries, 256 folders, depth 64
and five seconds. Reaching an attribution or discovery bound protects the
outputs rather than authorizing cleanup from an incomplete inventory. Removal
also has entry/depth/time bounds and never follows symlinks. This maintenance
removes generated build output, not source worktrees or branches; the existing
explicit work-ending worktree removal is unchanged.

The console offers a separate removal control on inactive, completely scanned
build folders. Every click requires confirmation naming the exact path and
warning that compiler outputs must be rebuilt. Incremental rows remove only
those incremental outputs. Active or partial rows cannot be removed from the
UI; the worker independently rechecks live references before removal. External
folders require explicit confirmation and are never automatic cleanup targets.
After removal the old scan is cleared: investigate again for fresh sizes and
attribution. The head has no worker build-folder cleanup controls.

Both automatic and confirmed build-folder cleanup require worker-wide
quiescence: no active turns/tasks, live or unconfirmed shell/CLI writers,
pending launches, or open terminals. A background process can change directory
into another session's output, so the worker does not treat its original cwd
as sufficient proof that an unrelated target is unused. Missing cwd children
retain their proven worktree/repository scope; unresolved attribution refuses
cleanup rather than silently dropping an idle or active reference. A provably
absent managed worktree top may be ignored only when its reference is idle.

## Shared compile cache

Each worker's Machines detail has a **Shared compile cache** panel in the
console. It reports disabled/ready/missing state, the effective directory and
size cap, cache size, hits/misses, hit rate and compile time saved. Unknown or
unsupported statistics are displayed as unavailable, not zero. If the worker
estimates saved compilation time from mean compilation duration, cache hits
and cache-read time, the UI labels it **Estimated compile time saved** rather
than presenting it as measured. Status refreshes every minute while the detail
is open and also has an explicit refresh button; refreshes preserve unsaved
configuration edits. Statistics may remain unavailable until the first
compilation starts the cache server.
When sccache is missing the panel shows `cargo install sccache --locked` for the
user to run on that worker. carbon6 does not install it automatically.

Caching is **disabled by default**, independently on every worker. The console
can opt in and configure an absolute cache directory, a positive size cap in
bytes, Rust and C/C++ flags, and the archived build-folder grace period (default
three days). Blank directory/cap inputs restore worker defaults: `sccache` under
the worker state directory and the smaller of 100 GiB or 10% of the containing
filesystem (100 GiB if capacity is unavailable). Rust is selected by default
but has no effect while caching is disabled; C/C++ is off by default.
Configuration is persisted in the worker's
state directory as `compile-cache.json`; it is not written into repository
configuration. It applies on the next turn, not already-running builds. Claude
Code takes changes up through its existing safe-idle replacement path: running
tasks, unconfirmed inputs and in-flight turns are never interrupted for caching.

The cache shares compiler outputs rather than Cargo target directories or
linker outputs. Rust uses `RUSTC_WRAPPER` while preserving an existing wrapper;
opt-in C/C++ uses worker-owned compiler wrappers only where `CC`/`CXX` were not
already selected by the worker or session. Disabling the cache leaves
ordinary builds available and does not delete cached data. Archived automatic
cleanup is restricted to carbon6-owned, inactive, completely scanned folders
after the configured grace; external folders are never automatic targets.
This archived-output maintenance is independent of the compile-cache opt-in:
disabling caching does not disable the three-day archived-folder grace policy.
The worker uses `SCCACHE_SERVER_UDS` under its state directory, keyed by cache
directory and size cap, rather than attaching builds to the user's default
server. A configuration change starts the matching server on demand; existing
builds finish on their original server, which follows normal idle expiry.
Filesystem preparation has a two-second wait and at most two outstanding
blocking probes, including timed-out probes until they finish. Missing tools
or an explicit cap avoid unnecessary capacity probes. Overlong Unix socket
paths are reported as unavailable and caching is skipped, rather than making
ordinary builds fail. Relative worker state paths become absolute before they
are exported to a session.

Console-only worker endpoints are `GET /api/v1/machines/{id}/compile_cache`,
`PUT /api/v1/machines/{id}/compile_cache` with
`{enabled,directory,max_bytes,rust,c_cpp,archived_grace_days}`, and
`POST /api/v1/machines/{id}/build_folders/remove` with
`{path,incremental_only,confirmed}`. They are not general agent operations.
The status response includes `config`, `status`, `install_command`, effective
`directory`/`max_bytes`, and nullable `stats` with optional
`cache_size_bytes`, `cache_hits`, `cache_misses`, `hit_rate` (0–1), and
`compile_time_saved_seconds`, plus `compile_time_saved_estimated` to distinguish
an estimate from a directly reported duration. A failed stats read may include
`stats_error`.

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
Disk admission applies to new work, not subsequent turns or recovery of an
existing child. Existing child turns still obey memory/load admission. The
separate disk-exhaustion recovery path keeps its 16 MiB recovery headroom;
its filesystem monitor uses two background slots and at most 64 cached paths,
not synchronous filesystem calls on the ACK/cancellation read loop.

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
Expiration and both rollup tiers receive fair shares, with rotating priority
for budgets below three. Sustained fleet traffic can exceed any fixed budget;
larger fleets should increase `maintenance_rows` to match their ingestion rate.
Realm attribution checks query only IDs present in the response payload, in
chunks of at most 500 IDs, rather than loading the realm's entire session list.

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

The pre-existing disk-recovery warning policy is separate: its session settings
warn below a percentage or byte floor and route through the `failed` event.
Machines warnings route through `machine`; both may report the same low-space
episode, with independent channel settings. Telemetry database open failure
currently fails head startup; fail-open initialization is a possible availability
hardening follow-up. Investigation reports apparent file lengths, not unique
allocated blocks (hard links, sparse files and compression can differ).

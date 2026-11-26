# Restarts that waste no work

Status: decided with you (2026-11-09): build all three layers.

Workers update themselves on every green main commit (docs/updates.md). An update waits for a session's turn to end, then restarts the worker. That restart kills work still under way after the turn ended:
- background shell commands;
- the Claude Code CLI, with its background agents and shells;
- carbon6's subagents and their turns;
- anything a remote session waits on here.

The goal: a worker can restart at any moment, and nothing that was running is lost or has to start over. That holds for every kind of connection.

## 1. Where things stand

Current session configuration is separate from restartable session state; see
[Session state and current configuration](session-specs.md). Capable workers
receive fresh derived specs before recovered turns can dispatch. Changed
configuration applies at a quiet turn boundary, without resetting conversation,
placement or explicit model choices. For Claude Code it waits for native work
and pending notifications to finish, then uses the existing supervised
replacement path to resume the same conversation with current guidance/tools.
This does not replace the process-preservation layers described below.

- **Busy means "a turn is running"** (`Sessions::running`, `crates/c6-worker/src/update.rs`). A session whose turn ended while its background work runs counts as idle, so the update restarts the worker under it.
- **Everything is the worker's child or lives in its memory.**
  - Shell commands are its child processes; their process groups don't help, because a restart stops them all. systemd stops the whole cgroup (`c6w@.service`: `KillMode=mixed`), and launchd stops the job.
  - The Claude Code CLI is a child too (`claude_code.rs`: `process_group(0)`, `kill_on_drop`). It holds its own background agents and shells.
  - carbon6's own agent loop and its subagents run in the worker's memory.
- **After a restart,** a carbon6 turn in progress fails. A Claude Code session resumes its conversation (`--resume`) but loses its turn, background agents and shells. Background-task notices that were still due are marked lost.
- docs/background.md §2 left "background tasks that survive a worker restart" for later. This is that later.

## 2. Layer 1: wait for quiet

An update restarts the worker only when nothing on it is working:
- **Busy counts all work:** running turns; background shell tasks; Claude Code background agents and shells (`task_started` with no end yet); carbon6 subagents, running or with a report due; and work a remote session waits on here.
- **Draining.** While an update waits, the worker:
  - starts no new background work (a call that would start some runs in the foreground or says the worker is about to update);
  - starts no new sessions;
  - lets turns run on.
- **You see what it waits on.** Settings → Updates lists, per machine, the sessions and tasks holding the update, with **Update now**.
- **Work that never finishes** (a dev server, `tail -f`) can be marked fine to restart:
  - by the agent when it starts it (`restartable: true` on a background shell task);
  - or by you, from the list.

  It doesn't hold an update, and layer 2 keeps it running anyway.
- **The drain deadline** (Settings → Updates, `drain_hours`) stays the last resort.

With layers 2 and 3 in place, the worker restarts as soon as nothing is in a state that can't be carried over. Layer 1 is then the fallback for what can't be: an older runner, and Claude Code where the runner isn't used.

## 3. Layer 2: the runner keeps processes alive

`c6-runner` is a small process that rarely changes. It runs beside the worker and outlives its restarts:
- **Started on its own:** its own systemd unit (`c6-runner@.service`, and a user unit) or launchd job, outside the worker's cgroup or job. The worker talks to it over a Unix socket in its state folder.
- **Shell commands.** The worker asks the runner to start them (its own process group, its own session). The runner:
  - writes their output to files in the state folder;
  - records the exit code when they end;
  - keeps no other state, so the files are the truth.

  After a restart the worker asks the runner what is running, reattaches to each (output so far, then the rest as it comes), and delivers the end as before.
- **The Claude Code CLI.** The runner starts `claude` and holds its stdin and stdout.
  - It keeps a buffer of `claude`'s output, by line number, until the worker confirms it has read it.
  - After a restart the worker reconnects to the same process, reads what it missed, and carries on the turn. The CLI never notices: its turn, background agents and shells keep going.
- **When the runner is missing or old,** the worker runs things itself as today, and layer 1 keeps updates from wasting work.
- **Updating the runner** happens only when nothing runs under it, so rarely.
- **Ownership.** The runner runs as the worker's user. Stopping a session or a task still stops its process; archiving a session ends its `claude`. When the worker is gone for good (uninstalled), the runner stops what it holds.

## 4. Layer 3: carbon6's loop resumes its turns

carbon6's own loop (every connection except Claude Code) keeps each turn's steps in the session's journal.
- **After a restart, a turn that was under way carries on** from its last finished step:
  - finished tool results are kept;
  - a tool call that was running through the runner is reattached (layer 2);
  - one that was running in the worker itself is run again only if it is safe to repeat (reads), otherwise the agent is told it was interrupted;
  - the model reply that was streaming is asked for again. Prompt caching makes that repeat cheap.
- **carbon6's subagents** resume the same way from their own journals, and report to their parent as before.
- **Remote subagents** run on another worker. Their reports reach the parent through the head, which already delivers notices durably.

The worker journal uses one pooled SQLite connection in WAL mode with `synchronous=FULL`. Local reads and writes queue for that connection (60-second acquisition timeout); write transactions begin with `BEGIN IMMEDIATE`, and SQLite has a 30-second busy timeout for an external writer. In a local 382 MB synthetic journal with 65,223 events, event inserts used the composite primary key and session updates used the session ID index. Across 200 warm insert/update/commit cycles, median latency was 0.11 ms and p95 was 0.20 ms. A separate warm replay of a 378 MB single-session fixture fetched and decoded in about 1.15 seconds. These are local measurements, not production guarantees: a large replay occupies the same connection while it fetches rows.

SQLite extended error **4874 is `SQLITE_IOERR_SHMSIZE`**, not
`SQLITE_IOERR_SHMMAP` (5386). In the bundled Unix VFS, SHMSIZE means that
`fstat` on the WAL-index shared-memory file failed, or a write allocating a
new shared-memory page failed. A failed `mmap` has its own SHMMAP code;
memory pressure alone therefore does not establish the cause of 4874.
The November 21, 2026 incident did not capture the underlying OS errno, so
its precise filesystem cause remains unproven. Removing an open `-shm`
file does not itself invalidate its open descriptor; truncating mapped
shared memory can instead cause SIGBUS. Never delete or truncate a live
journal's `-wal` or `-shm` files as a recovery procedure: the WAL may contain
committed events that have not been checkpointed.
See SQLite's [extended result codes](https://sqlite.org/rescode.html#ioerr_shmsize),
the bundled `libsqlite3-sys` amalgamation's `unixShmMap`, and SQLite's
[hot-journal warning](https://sqlite.org/howtocorrupt.html#_deleting_a_hot_journal).

In-process journal recovery closes the pooled connection before rebuilding
the actor. An exact-4874 fault regression reproduces a poisoned connection:
SQLite rolls back its transaction, but SQLx still tracks a nonzero transaction
depth, so another append fails with `InvalidSavePointStatement`. Replacing
the connection permits a new append without replaying the failed write.
This demonstrates a recovery failure mode, not the incident's missing OS errno.
A new connection uses the original options; no failed SQL statement
is automatically replayed. Durable event and command receipts decide what
must still be delivered, including notices retained in the original mailbox.
Closing a connection is not a substitute for resolving an ongoing filesystem
failure, so automatic retries have a configurable time budget rather than an
unlimited loop (see docs/background.md, §5).

Held Claude Code cleanup distinguishes **authority to stop** from **proof of
absence**. FIFO/launch identity authorizes stopping a live group; a fresh
whole-group absence confirmation can retire a group even after that identity
has disappeared. Recovery first polls the worker's own retained wrapper child
with `try_wait`: on Linux an unreaped, exited wrapper can keep signal-zero
group probes live while no member can prove ownership. Waiting to drop that
child until after group-absence confirmation creates a circular wait.
Reaping this child sends no signal and does not prove that descendants have
stopped; the existing whole-group fence still must succeed. An inaccessible,
reused, or otherwise unidentifiable live group is never signaled on the strength
of saved metadata. It remains blocked and escalates for user attention.

## 5. Across connection types

| Connection | Layer 1 | Carried across a restart |
|---|---|---|
| carbon6's own loop (Anthropic API, OpenAI, ChatGPT plan, OpenAI-compatible, pools) | yes | yes: layer 3, with shells through layer 2; at most one model reply is asked again |
| Claude Code | yes | yes: layer 2 holds the `claude` process |
| Remote subagents | yes | they run elsewhere; reports go through the head |

## 6. Steps

1. **Layer 1:** busy counts all work, draining, the list of what holds an update with Update now, `restartable` tasks.
2. **Layer 2:** `c6-runner` (binary, socket protocol, units and launchd job, packaging in the .deb and the bottle), shells through it with reattach, `claude` through it with reconnect, the fallback when it's missing.
3. **Layer 3:** carbon6's turns and subagents resume from the journal after a restart.

Each step is tested with a worker that really restarts mid-work (the chaos tests' way), waiting on conditions, not time (docs/testing.md).

## 7. As built (2026-11-09)

1. **Layer 1: wait for quiet.**
   - **What holds an update.** `Sessions::holding` lists, per session, a running turn and every running background task whose end is awaited: shell commands, carbon6 subagents, and Claude Code's own (its tasks out of the foreground). The update waits until the list is empty.
     - **Servers don't hold it.** Commands started with `notify: false` (the bash tool's existing flag for servers and watchers meant to keep running) don't count. That flag stands in for the design's `restartable`, so no new one was needed.
   - **What you see.** The worker sends the list with its `waiting` status. The head turns it into "waiting on ‹session title›: ‹what›" on Settings → Updates.
   - **Update now.** A machine that is waiting has an **Update now** button (`system.updates.now`, `POST /api/v1/system/updates/machines/{machine}/now`, `update.now` to the worker). It sets that update's deadline to now.
   - **No draining.** Workers still start new background work while they wait. With held commands (2) that work isn't lost anyway, and the drain deadline and Update now cap the wait.
2. **Layer 2: no runner daemon.**
   - Each long-lived process is started detached from the worker in its own process group, with its output in files. The worker takes it up again after a restart, so there is no separate `c6-runner` to install, update or keep in step.
   - **Shell commands** (`Background::spawn_held`, in `<state>/held/<session>/<task>/`):
     - The command runs under `/bin/sh`, which writes its exit code to `code` when it ends. stdout and stderr go to `out` and `err`, and `meta.json` records it.
     - On reload, a session adopts each task it had running (`Background::adopt`) instead of marking it lost. It follows the files to the end and reports the end as usual, with all the output from both workers' time.
     - Detached launch retains Rewind's durable writer reservation and cancellation checks. Adoption reuses that ownership; completion or an exited leader is not proof that its process group stopped (docs/background.md §8).
     - SSH sessions' commands are still run as before.
   - **Claude Code** (`Process::spawn_held`, in `<state>/claude/held/<conversation>/`):
     - **Input:** `claude` reads its input from a named pipe, `in`. A keeper process in its process group holds the pipe open, so no restart ever ends its input. The worker writes to the pipe.
     - **Output:** its output goes to `out`, its exit code to `code`, and how far the worker has read to `offset`. The offset is saved for every line but streaming deltas.
     - **Exit observation:** the worker also watches the wrapper itself, including an exited, unreaped child. A signal can prevent `code` from being written; it must not leave a running turn waiting forever. Final output is drained, the wrapper is reaped when it is this worker's child, and a turn without a result ends with a visible error. The worker retires the remaining held group before releasing its FIFO and returning to idle adoption, using the same keeper-readiness, ownership and absence checks as explicit close. An idle death does not invent a failed turn. A dead wrapper is cleanup-only, never re-adopted just because its keeper survives. This is not proof that every descendant stopped: process-group replacement still requires the normal ownership and absence checks.
     - **Backpressure:** FIFO writes are nonblocking. Stop and wrapper exit remain observable while an input (including a subagent report) waits for pipe capacity. Cancelling a partly written input stops that runtime rather than reusing a truncated JSON line for the next turn.
     - **The bridge:** the MCP bridge comes back on the port it had (`<state>/mcp-bridge-port`). The process's token is registered again, so carbon6's tools keep working for it.
     - **After a restart,** the start-up reload loads every session with a held process. Its actor takes the process up again (`Process::attach`) and reads on from `offset`. The rest of the interrupted turn comes in the way a turn Claude Code starts by itself does, so the session shows it, without starting `claude` again.
     - **Rewind:** adoption selects only the journal's confirmed kept-history branch, never the original discarded conversation or a pending reset. A pending reset owns and stops its old/reset-attempt keeper before the durable absence barrier. Unidentifiable ownership stays fail-closed, with the actor and cancellable inbox alive. Cleanup does not authorize retry: a failed destructive reset still needs a new explicit Rewind. Ordinary worker shutdown keeps held processes alive; explicit reset cleanup kills all descendants even if the leader has already exited.
     - **Missing branch metadata:** a completed public Rewind without its local branch mapping reconstructs only kept history in a fresh conversation. Surviving unselected runtimes are cleanup-owned, never adopted. This cleanup serializes with new intent publication and retry authorization, and rejects a pending reset before sending any signal; it cannot destroy a runtime beneath a newly published untouched intent.
     - **Initialization window (option A).** Fork/reset initialization is non-held until the new branch is confirmed. If its worker dies in that window, Rewind fails closed and only a new explicit Rewind can retry it; startup cannot retry or resurrect discarded history. After confirmation, ordinary model work uses a held process for the confirmed branch. This narrow initialization window deliberately does not have ordinary held-process restart survival.
     - **Ending:** explicit held close/stop kills its process group while keeping the FIFO writer open until ownership-preserving cleanup finishes. EOF must not retire the only identifiable leader before descendant cleanup. A verified FIFO keeper must confirm its stopped state before the group's interrupt grace; merely sending SIGSTOP is not confirmation. Replacement requires confirmed group absence before another runtime can start, including a resume whose native conversation is missing. Its folder is removed only after absence is confirmed.
     - **Replacement uncertainty:** an ordinary absence refusal keeps the running continuation and cancellable inbox alive, rechecking tracked groups every 250 ms without sending input. A held `retiring` marker makes that runtime cleanup-only, even with a positively identified live keeper. After absence, the same continuation preserves its promotion/preamble or already-open turn; the ownership gate serializes resumed initial writes with explicit Rewind publication and releases before the model reply. Journal/filesystem failures remain fatal. The continuation is not crash-durable: after a worker restart, a still-live marked runtime remains nonadoptable until absence or existing explicit cleanup; no new automatic signal retry is introduced.
     - **Quiet context maintenance:** as a turn ends, after idle-only output or native task completion is consumed, or before the next input is sent, a large Claude Code conversation can use this same confirmed-stop/`--resume` path to shed old tool output (docs/subagents.md, “Long conversations shed old tool output”). It never closes a runtime with an unconfirmed input, pending native turn/notification, unread output, or Claude Code's own running background task/agent. carbon6-held shells and subagents survive this restart and do not block trimming at the start of their wait. A read-only candidate must save at least 20% of live message content; after whole-group absence the worker prepares again, verifies that the file has not changed, and atomically renames the trimmed JSONL over the original. A crash before rename leaves the full transcript; after rename the same conversation ID resumes the smaller one. In-place replacement, rather than a new fork ID, keeps Rewind's native branch selection, placement moves and carbon6's conversation identity consistent. The native compacted prefix and carbon6's full event log stay untouched. Missing, malformed or unfamiliar transcripts defer maintenance, not input. Per-realm `claude_code_trim_enabled` (default true) and `claude_code_trim_at` (0: automatic window-aware threshold) are in Agents → Limits. The first resumed call writes a smaller cache prefix, normally repaid within a couple of calls at large context sizes. Maintenance makes no model call; cache warming is a separate policy.
     - **Incomplete launch folders:** discovery skips entries without readable, valid metadata instead of blocking unrelated sessions. Launch identity is atomically published before creating the input FIFO, and pre-spawn failures remove their reservation. Startup inventory and discovery may remove recognized unpublished stubs older than five minutes, but only after unchanged-file checks and proof that no process holds the FIFO; inconclusive ownership checks preserve the folder. A zero-PGID reservation is reclaimable only before any runtime files exist, never after its FIFO appeared. Young or ambiguous reservations remain protected even at the requested conversation's own path, and replacement validates all candidates before removing any.
     - **Ask (head):** Ask's Claude Code on the head still runs as before (`Process::spawn`).
   - **Stopping the worker.**
     - systemd stops only `c6w` now (`KillMode=process` in `c6w@.service` and the user unit). launchd stops the worker's process group, which held processes have left.
     - **Linux OOM is not a restart protocol.** The worker units use [`OOMPolicy=continue`](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html#OOMPolicy=) so systemd does not stop the worker in response to an individual workload's OOM death. `Restart=on-failure` still applies when the main process fails. [`OOMScoreAdjust=-500`](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html#OOMScoreAdjust=) makes the worker a less likely kernel OOM victim, not immune. A user service manager may lack permission to set the negative score; systemd skips permission failures, and `OOMPolicy` is still configured.
     - Linux inherits both the score adjustment and its privileged minimum on fork. Raising a negative adjustment to zero is allowed without `CAP_SYS_RESOURCE` ([fork](https://github.com/torvalds/linux/blob/v6.12/kernel/fork.c), [proc write](https://github.com/torvalds/linux/blob/v6.12/fs/proc/base.c)). Session launchers reset the child before workload code runs, including held wrappers and keepers. Positive scores are preserved. The Linux spawn needs readable `/proc/self/oom_score_adj` (writable when negative); if the reset fails, the workload does not start. This is an initial default, not a sandbox: without privilege the reset doesn't change the inherited floor, so a child may be able to lower its own score again. Existing held processes aren't rewritten on adoption.
     - **No default `MemoryHigh=` or `MemoryMax=`.** Held work leaves the worker's process group, not its systemd cgroup. A [`MemoryHigh=`](https://www.freedesktop.org/software/systemd/man/latest/systemd.resource-control.html#MemoryHigh=bytes) limit would reclaim/throttle across the whole unit, not isolate a runaway session; `MemoryMax=` can force a cgroup OOM. A percentage such as `MemoryHigh=90%` scales with installed RAM, but no fraction is a safe workload budget for every host: the worker may share that RAM with the head, other workers, and unrelated services. Operators can size those limits with drop-ins for their own hosts. A group kill (such as an ancestor's group OOM policy or selection by `systemd-oomd`) can still kill held work and the daemon together.
     - `c6w` exits for an update only after shutting down its async runtime. That ends its sessions' foreground commands (a dropped call kills its whole process group, `KillGroup`) and Claude Code processes it doesn't hold, and leaves held ones running.
   - **Checkpoint Git locks.** In each private checkpoint repo, a persistent per-repo `flock` serializes the entire snapshot or restore. Git inherits the lock descriptor across `exec`, so cancellation or worker death does not make a live Git process's `index.lock` safe to remove. After taking the worker singleton lock, startup removes stale `index.lock` files and skips busy repos. If Git specifically reports that `index.lock` already exists, recovery removes it only when no live Git can own it, then retries just that Git command once. A conservative liveness check also defers deletion when a Git process from an older worker might still hold the lock without the `flock`; new operations wait for those legacy writers too, including a checkout that does not take `index.lock`. Legacy detection compares repository identity using actual process argv (including symlinks and spaces), assumes the same UID and process namespace, and fails closed when ownership is uncertain. Worker state must not be shared by writers hidden in another PID namespace.
3. **Layer 3: turns that were cut short go on.**
   - A failed worker-journal write does not require a worker restart or a new notice. The session supervisor stops and joins the old turn before rebuilding its conversation and sequence cursor from the durable log, closes an open turn with `error`, and resumes through the editable `session.resume_after_failure` prompt. The same mailbox, inbox and background-tool sinks stay alive; pending durable notices are reconstructed without duplicating inputs already taken. Recovery retries with bounded backoff if the journal remains unavailable. A live child is not declared lost merely because its parent's turn failed. A held Claude Code reader may have advanced past the output whose write failed: recovery retires that CLI with ownership and process-group checks, rather than adopting an offset that could skip the reply, then explicitly continues on a replacement CLI. Claude Code's initial `User` receipts and `TurnStarted` commit together before any initial stdin writes: if that journal boundary fails, no receipt survives and the original unsent batch is retried in order, not replaced by a generic continuation. This does not establish delivery across a worker crash after commit but before stdin consumption. Failure to prove cleanup keeps input blocked.
   - Disk exhaustion is not a terminal recovery failure. `SQLITE_FULL`, `ENOSPC`, and write I/O errors on a full state filesystem use an unbounded automatic resource-wait lane, with configurable capped exponential backoff (5 seconds to 5 minutes by default). A cheap free-space check each second wakes recovery when 16 MiB returns. The session names the mount and available bytes, and user/parent notifications say it will proceed automatically. Corruption and failed integrity checks still stop after the normal bound with Retry and Restart. Before a connection is reused after a failed transaction it must be rolled back or replaced; SQLite's own implicit rollback must not leave SQLx's transaction depth nonzero for the next acknowledgement or append.
   - At start-up the worker still closes a turn that a restart cut short (`worker restarted`). It then wakes the session with a notice, so the agent carries on from its log.
     - The notice is the editable prompt `session.resume_after_restart`.
     - The agent also sees "Tool call did not complete (interrupted)" for any call that was running.
   - A subagent's session isn't woken: its parent's call already got what it did, and the parent decides.
   - A turn that waited on a subagent running on another worker isn't woken: that subagent's report wakes it, as before.
   - A Claude Code session whose process was held isn't woken either: it carries on by itself.
   - A pending or applied Rewind that supersedes the interrupted turn suppresses its automatic continuation, both during recovery and if a queued resume reaches the actor later. Restart recovery never counts as an explicit retry of a stuck Rewind.
   - The reply that was streaming when the worker went is asked for again as part of the next turn.
4. **Tests:**
   - `updates::an_update_waits_for_background_work_and_says_on_what`
   - `chaos::a_background_command_outlives_a_worker_restart`
   - `chaos::worker_restart_mid_turn_then_continue`, which now checks that the session goes on by itself
   - `bash::tests::a_dropped_call_kills_its_whole_command`
   - `claude_code::tests::a_held_process_outlives_its_worker_and_is_taken_up`
   - `session::handoff_tests::confirmed_rewind_restart_adopts_only_the_fork_and_deduplicates_input` and `failed_pending_rewind_restart_cleans_keepers_without_retry_and_keeps_inbox`: real held runtimes, journal-selected adoption, no superseded Resume, and cancellable queued input after a failed reset.
   - `claude_code::tests::rewind_cleanup_stops_owned_descendants_after_the_wrapper_exits` and `rewind_cleanup_does_not_signal_a_reused_held_process_id`: descendant absence and positive ownership, not just leader exit.
   - `claude_code::tests::held_cleanup_confirms_keeper_stop_and_accepts_already_absent_group`: on Linux, the CLI observes a stopped keeper at the actual interrupt; cleanup accepts lost keeper proof only after whole-group absence is established.
   - Replacement units cover an already-released held directory, positive live-owner retirement refusal, fatal tracking errors, and both publication/admission orders through initial writes without holding the gate until model output. `claude_code_rewind` additionally checks Linux fixture-owned unreaped group membership, queued take-back, automatic recovery after reap, and exact opened-turn receipts. Its `C6_REPLACEMENT_LIVENESS_CASE=rewind` mode publishes an explicit Rewind before releasing the absence barrier and suppresses the old continuation.
   - `session::handoff_tests::metadata_loss_cleanup_serializes_pending_publication_and_retry_authorization`: deterministic publication-first and cleanup-first orderings preserve the untouched-intent boundary without a database transaction across process cleanup.
   - `claude_code_rewind` (its own binary): the fork/reset initialization crash window fails closed, consumes old retry authorizations, and requires a new explicit Rewind before queued messages can proceed. Missing native conversation fallback retires the previous keeper and preserves late unconfirmed inputs exactly once; completed-Rewind reconstruction cleans surviving unselected runtimes without replaying Rewind.
   - `claude_code_restart` (its own binary): a fake `claude` mid-turn through a real end of the first worker's runtime. It checks that the rest of the turn shows, that `claude` isn't started again, and that a follow-up message reaches the same process.

   Each first worker runs in a tokio runtime of its own that the test ends, the way a worker's process ends.
5. **Not done:**
   - **Wait counts:** a foreground command cut short by Update now or the drain deadline is lost (stopped, as before). Remote subagents waiting on this worker don't count as holding the update.
   - **Ask:** the head's Claude Code is not held.
   - **Mixed output:** a held process's output from before and after a restart isn't merged into one turn in the transcript. The rest of the interrupted turn shows as a turn of its own.

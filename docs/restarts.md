# Restarts that waste no work

Status: decided with you (2026-11-09): build all three layers.

Workers update themselves on every green main commit (docs/updates.md). An update waits for a session's turn to end, then restarts the worker. That restart kills work still under way after the turn ended:
- background shell commands;
- the Claude Code CLI, with its background agents and shells;
- carbon6's subagents and their turns;
- anything a remote session waits on here.

The goal: a worker can restart at any moment, and nothing that was running is lost or has to start over. That holds for every kind of connection.

## 1. Where things stand

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
     - **The bridge:** the MCP bridge comes back on the port it had (`<state>/mcp-bridge-port`). The process's token is registered again, so carbon6's tools keep working for it.
     - **After a restart,** the start-up reload loads every session with a held process. Its actor takes the process up again (`Process::attach`) and reads on from `offset`. The rest of the interrupted turn comes in the way a turn Claude Code starts by itself does, so the session shows it, without starting `claude` again.
     - **Rewind:** adoption selects only the journal's confirmed kept-history branch, never the original discarded conversation or a pending reset. A pending reset owns and stops its old/reset-attempt keeper before the durable absence barrier. Unidentifiable ownership stays fail-closed, with the actor and cancellable inbox alive. Cleanup does not authorize retry: a failed destructive reset still needs a new explicit Rewind. Ordinary worker shutdown keeps held processes alive; explicit reset cleanup kills all descendants even if the leader has already exited.
     - **Missing branch metadata:** a completed public Rewind without its local branch mapping reconstructs only kept history in a fresh conversation. Surviving unselected runtimes are cleanup-owned, never adopted. This cleanup serializes with new intent publication and retry authorization, and rejects a pending reset before sending any signal; it cannot destroy a runtime beneath a newly published untouched intent.
     - **Initialization window (option A).** Fork/reset initialization is non-held until the new branch is confirmed. If its worker dies in that window, Rewind fails closed and only a new explicit Rewind can retry it; startup cannot retry or resurrect discarded history. After confirmation, ordinary model work uses a held process for the confirmed branch. This narrow initialization window deliberately does not have ordinary held-process restart survival.
     - **Ending:** explicit held close/stop kills its process group while keeping the FIFO writer open until ownership-preserving cleanup finishes. EOF must not retire the only identifiable leader before descendant cleanup. Replacement requires confirmed group absence before another runtime can start, including a resume whose native conversation is missing. Its folder is removed only after absence is confirmed.
     - **Ask (head):** Ask's Claude Code on the head still runs as before (`Process::spawn`).
   - **Stopping the worker.**
     - systemd stops only `c6w` now (`KillMode=process` in `c6w@.service` and the user unit). launchd stops the worker's process group, which held processes have left.
     - `c6w` exits for an update only after shutting down its async runtime. That ends its sessions' foreground commands (a dropped call kills its whole process group, `KillGroup`) and Claude Code processes it doesn't hold, and leaves held ones running.
3. **Layer 3: turns that were cut short go on.**
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
   - `session::tests::confirmed_rewind_restart_adopts_only_the_fork_and_deduplicates_input` and `failed_pending_rewind_restart_cleans_keepers_without_retry_and_keeps_inbox`: real held runtimes, journal-selected adoption, no superseded Resume, and cancellable queued input after a failed reset.
   - `claude_code::tests::rewind_cleanup_stops_owned_descendants_after_the_wrapper_exits` and `rewind_cleanup_does_not_signal_a_reused_held_process_id`: descendant absence and positive ownership, not just leader exit.
   - `session::tests::metadata_loss_cleanup_serializes_pending_publication_and_retry_authorization`: deterministic publication-first and cleanup-first orderings preserve the untouched-intent boundary without a database transaction across process cleanup.
   - `claude_code_rewind` (its own binary): the fork/reset initialization crash window fails closed, consumes old retry authorizations, and requires a new explicit Rewind before queued messages can proceed. Missing native conversation fallback retires the previous keeper and preserves late unconfirmed inputs exactly once; completed-Rewind reconstruction cleans surviving unselected runtimes without replaying Rewind.
   - `claude_code_restart` (its own binary): a fake `claude` mid-turn through a real end of the first worker's runtime. It checks that the rest of the turn shows, that `claude` isn't started again, and that a follow-up message reaches the same process.

   Each first worker runs in a tokio runtime of its own that the test ends, the way a worker's process ends.
5. **Not done:**
   - **Wait counts:** a foreground command cut short by Update now or the drain deadline is lost (stopped, as before). Remote subagents waiting on this worker don't count as holding the update.
   - **Ask:** the head's Claude Code is not held.
   - **Mixed output:** a held process's output from before and after a restart isn't merged into one turn in the transcript. The rest of the interrupted turn shows as a turn of its own.

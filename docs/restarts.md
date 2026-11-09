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

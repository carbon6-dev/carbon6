# Context economy — Design

Status: proposed (2026-11-26). Builds on trimming old tool output (docs/subagents.md, 4d941b9; for Claude Code sessions in progress), the replay evals (docs/replay-eval.md), quiet-turn restarts (docs/restarts.md) and the Machines sampler (docs/machines.md).

## 1. Why

What a long session costs is its context size times how often it is re-read, plus a full rewrite whenever its cache expires. On 2026-11-26 four Claude Code sessions sat at about 966k tokens and read 862M of 912M tokens in 36 hours. Many of their waits are builds of about 1.5 hours, longer than Claude Code's cache (about an hour, measured from usage), so every build wait ends in a full rewrite of up to 1M tokens. The replay evals found trimming old tool output keeps quality (−52% read, within the noise) while Claude Code's `/compact` mid-task does not.

carbon6 knows what each session is doing (running a turn, waiting on its background work, waiting on you), every call's cache reads and writes, and when the next message arrives. It can therefore choose, per moment, the cheapest action that doesn't cost the work.

## 2. What carbon6 measures

- **Each connection's real cache lifetime**, learned from usage: the longest gap after which calls still read the cache, and the shortest after which they write it all again. A default per provider until enough calls are seen (Claude Code: 60 minutes; Anthropic API: per the request's TTL; OpenAI: provider default).
- **What a session is waiting on** when its turn has ended: you (nothing running), its own background work (a build, tests, subagents), or a schedule.
- **Its context size and growth**, from the last call's usage.

## 3. The policies

Each is a setting per realm, with defaults below; each shows a quiet line in the transcript saying what it did and the sizes before and after. carbon6's own log always keeps the full history; Rewind goes back before any of them.

### 3.1 Waiting on background work: trim, then keep the cache warm
When a turn ends and the session waits on its own background work:
1. **Trim at once** (the trimming policy), if it saves 20% or more: the next calls read less, and the wait has just begun, so the rewrite is paid on the smaller context. Where that's possible depends on who owns the work being waited on:
   - **carbon6's own loop:** always; trimming changes only the next request.
   - **Claude Code, waiting on work carbon6 holds** (carbon6 background tasks and held shells, which survive a restart, docs/restarts.md §3): trim through the quiet-turn restart with a trimmed transcript.
   - **Claude Code, waiting on its own background tasks or agents:** these die with its process, so carbon6 does **not** restart it to trim. It keeps the cache warm untrimmed until they finish, and trims at the next quiet moment after.
2. **Keep it warm** while the work runs: a minimal call shortly before the cache expires (the margin is the smaller of 5 minutes and 20% of the learned lifetime, with at least 1 minute between pings, so a 5-minute cache is pinged at 4 minutes and a 60-minute one at 55), repeated until the work finishes or a cap is reached (default 12 hours). For carbon6's own loop, a request with the same prefix whose reply is discarded; for Claude Code, a tiny message from the editable prompt `session.keep_warm` ("Your background work is still running; nothing to do yet. Reply only: ok"), shown quietly.
   - Only above a size where it pays (default 50k tokens of context).
   - Never when the session waits on you: you may not come back.
   - Cost: each ping is a cache read of the context (about a tenth of a full read) against a full rewrite on the next real call; for a 1.5-hour build, one ping instead of one rewrite.

### 3.2 Idle and waiting on you: compact at the boundary, while the cache is warm
When a turn ended normally, nothing runs, and the session has been idle for most of its cache lifetime (the same margin as keep-warm: the smaller of 5 minutes and 20% of the lifetime before expiry):
- **Compact with the session's own model** while its cache is still warm (so the compaction reads the context at cache-read price), with a focused prompt (`session.idle_compact`: keep the plan, files changed, what failed and why, exact errors, open questions; keep your last instruction and the last few turns word for word).
- When you come back, the first call writes about 30k tokens instead of about 1M.
- **Default off until measured** (§4): the evals measured compaction mid-task, not at boundaries.

### 3.3 Your new message starts a different task: trim, and offer a fresh start
When your message arrives, carbon6 compares it with the session's recent context (embeddings of the message against recent topics and files touched, which the memex already computes) and a small model classifies it as continuing, related or new:
- **New:** trim old tool output before the agent's next call.
- **Clearly unrelated:** a quiet card, "This looks like a new task. Start fresh with a handover note? [Yes] [Keep context]". carbon6 never drops context on its own here; your choices tune the threshold.

### 3.4 After a long gap anyway: reshape for free
If a session resumes after its cache has expired regardless (no keep-warm applied: it was waiting on you, or keep-warm was off), any pending trimming is applied before that first call: the rewrite is happening anyway, so reshaping costs nothing extra.

## 4. Measuring before defaults

The replay eval (scripts/replay-eval.py) gains two kinds of points from real sessions — **idle boundaries** (a turn ended, nothing running, long idle) and **task shifts** (a new message unlike the context) — and two arms: boundary compaction (3.2) and shift trimming (3.3), each with the noise control (baseline replayed again, every pair judged twice). A policy is on by default only if it stays within the noise; otherwise it stays opt-in.

## 5. Shorter waits: a shared compile cache per worker

Built as a **per-worker opt-in**, configured in Machines. carbon6 never edits a repository's build configuration, shares its `target` folder, or installs tools silently.

- **Environment:** Rust defaults to `RUSTC_WRAPPER=sccache` only when enabled and sccache is executable on PATH or in `~/.cargo/bin`. A worker-inherited or session-provided wrapper wins. `SCCACHE_DIR` defaults to `<worker state>/sccache`; the default cap is the smaller of 100 GiB and 10% of that filesystem's total capacity. An absolute directory and explicit byte cap can override these defaults. C/C++ wrappers are a separate opt-in; carbon6 writes its `cc`/`c++` launcher scripts under its own state directory, never into the repository, and leaves inherited `CC`/`CXX` selections alone.
- **Commands:** the environment reaches carbon6's shell/Bash tools, background and held shells, and the Claude Code launch (including its held-process wrapper). Session secrets override carbon6's defaults without turning non-secret cache paths into redaction patterns. Changes apply to subsequent commands/turns; Claude Code's launch identity includes the non-secret cache defaults, so it takes changes up at its next safe idle restart, never interrupting running tasks. Local cache settings are not exported to an SSH destination, which needs its own worker/cache.
- **Installation and lifetime:** a missing executable shows “sccache not installed” and `cargo install sccache --locked`. Installation is left to the user. sccache starts its server on demand on a worker-specific `SCCACHE_SERVER_UDS`, separate from the user's default server; carbon6 does not terminate it with a session, and the disk cache persists through worker restarts. Changing the directory/cap selects a fresh socket without interrupting builds using the old server, which exits on its normal idle timeout.
- **Visibility:** Machines polls `sccache --show-stats --stats-format=json` on the worker while its detail is open. It shows cache size, hit/miss counts and hit rate. Compile time saved is labelled estimated when derived from aggregate compilation/read durations, not a measured per-hit saving; unavailable metrics remain unknown. The bounded disk investigation separately identifies build folders, sizes, modification/last-use information, session attribution and ownership.
- **Fail-open preparation:** filesystem probes run on the blocking pool with a two-second wait and at most two outstanding probes, including abandoned waits. A missing executable or explicit cap avoids unnecessary filesystem-capacity probes. Relative worker state paths are exported as absolute paths. An overlong Unix socket path is reported as unavailable and caching is skipped rather than breaking ordinary compilation.
- **Cleanup:** archived build-folder grace defaults to three days and is configurable per worker. Active sessions are protected. Disk-pressure cleanup considers only carbon6-owned unused build folders, oldest first, and emits a notice with removed paths and bytes. External folders are inventory only until an explicit confirmed Remove request; incremental-only removal has the same confirmation boundary. See `docs/machines.md` for the precise safety boundaries and limits.

## 6. What it is not

- Not a change to any repository: carbon6 acts on its own sessions' context, environment and worktrees only.
- Not silent loss: every action is shown, reversible with Rewind, and the full history stays in carbon6.
- Not compaction mid-task by default.

## 7. Steps

1. Cache lifetime learning per connection; "waiting on" classification.
2. Keep-warm (3.1) for every connection, Claude Code included; trim at the start of a background wait for carbon6's loop and for Claude Code waiting on carbon6-held work, and after the wait for Claude Code waiting on its own background tasks.
3. Reshape at a forced cold resume (3.4).
4. The eval arms (§4).
5. Idle boundary compaction (3.2) and task-shift trimming and suggestion (3.3), defaults per §4.
6. The shared compile cache and build-folder cleanup (§5).

Tests (docs/testing.md, injected clocks): keep-warm pings exactly before expiry while background work runs and never while waiting on you; trimming once at a wait's start for carbon6's loop and for Claude Code waiting on carbon6-held work; no restart (so no trim) while Claude Code's own background tasks run, and a trim at the next quiet moment after they finish; the cold-resume reshape; lifetime learning from synthetic usage; the shift classifier's suggestion card; sccache environment set only where enabled; cleanup never removes an active worktree's build folder.

### Cache-lifetime implementation progress (November 26, 2026)

Head lifetime learning persists realm/concrete-account/model evidence and exposes it through `session.cache_lifetime`, the read-only connections catalog, and Settings → Connections. Provider defaults remain fallbacks, not learned guarantees. Call intervals come from prompt/assistant events rather than delayed usage timestamps; five successful comparable-prefix idle gaps support the learned lower bound.

Historical learning uses resumable, bounded background work, not a full replay in the first ingestion transaction. After historical usage indexing finishes, usage session IDs are enumerated in keyset pages of 128 outside the writer lock. Short registration transactions persist the enumeration cursor. Ingestion also registers sessions during bootstrap, covering arrivals whose IDs precede that cursor.

Each replay transaction reads at most 128 short event headers for one session and only usage rows within that page. It commits evidence, previous call, event cursor, and partially reconstructed timing together. Once caught up under the writer lock, that session becomes live; foreground learning then processes only bounded new activity, requeueing oversized intervals without advancing the cursor. Sessions receiving usage while pending remain durable replay jobs. Shutdown is checked between transactions, with a scheduling gap for foreground writers; startup resumes persisted jobs. Tests cover a writer committing between pages, ingestion during catch-up, a mid-call checkpoint across reopen, duplicate ingestion, oversized live intervals, and rollback of a failed checkpoint.

### Keep-warm implementation notes (November 26, 2026)

The realm's `subagent-limits.keep_warm` setting defaults to enabled, a 50,000-token prompt threshold, a 300-second maximum margin, and a 43,200-second cap. `disabled_connections` excludes either the selected connection or its concrete alias target. The effective margin is the smaller of that setting and 20% of the lifetime; the interval is at least a minute. Lifetimes of a minute or less cannot be kept warm safely. The original wait's cap survives pings, restarts and moves, and cannot exceed twelve hours even if configured larger. Background classification uses the existing counted-work registry, including held work and native Claude Code tasks. Non-notifying servers, watchers and side chats do not keep a session warm. An autonomous wake timer is reported as waiting on a schedule, not on the user. Empty sessions and active turns are not waiting on the user.

Pings also have to pass a conservative economic check. An explicit, unmeasured exponential remaining-work prior has a mean of 90 minutes. The budget includes prior pings, projected future pings and the resumed call's cache read, and must cost less than half the probability-weighted avoided rewrite. This can deliberately reject short-lived API caches even above the size threshold. Compatible endpoints require observed cache reuse, a learned lifetime and known discounted cache pricing; protocol compatibility alone is insufficient. Local servers without cache telemetry stay off. Provider defaults are 60 minutes for Claude Code, five minutes for Anthropic and ten minutes for OpenAI/ChatGPT until learning supports another value.

Native requests retain the provider-ready prefix (system, tools, history, cache key and concrete account), followed by the completed assistant response and a tiny maintenance input. They request a one-token output limit and discard the reply. The existing ChatGPT-plan client deliberately omits that field because its backend does not accept output caps: that path relies on the acknowledgement prompt and the bounded request deadline, not a hard one-token wire limit. Credentials are fetched afresh and are never stored with the snapshot. A changed endpoint/account or failed authorization skips maintenance; an alias's newly selected account is not a fallback for a prefix cached on the old one. Pools remain unavailable to sessions and subagents under `docs/pools.md`; this policy does not introduce pool routing.

Claude Code receives the editable `session.keep_warm` input through its existing process, without a human User event, turn boundaries, notice-guard spending, or goal/watcher progress. The maintenance UUID and output cursor are persisted before writing so attachment does not resend an ambiguous ping. If it starts actual work instead of acknowledging the input, that work is logged normally. Its native background tasks and agents must never be restarted for maintenance. The actor retains the process while carbon6-held work runs as well.

A durable journal attempt marker fences duplicate sends across recovery and moves. Cache timers and pre-dispatch checks observe both monotonic and wall-clock expiry, so machine sleep cannot extend cache freshness. Maintenance usage is flagged and included in costs, with a separate keep-warm subtotal. Quiet maintenance events do not advance the session's task-progress timestamp. Rewrite savings are **not measured**: multiple pings can protect only one eventual resume, so reporting a rewrite saved per ping would overstate savings. Trimming has a separate integration hook at the start of the background wait; idle compaction, task-shift classification, cold-resume reshaping and the shared compile cache are separate policies, not implemented by keep-warm itself.

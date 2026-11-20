# Gradual sessions

## Contract and delivery order

A session is a conversation with a current **place**, not a permanent binding to
its first working directory. Deliver this in four independently verified steps:

1. Minimal creation and visible place chips.
2. Same-worker placement changes, including entering and leaving worktrees.
3. Between-worker transfers, preserving the conversation and work.
4. Agent-requested changes with one-time user consent.

This document describes the intended contract, not a claim that all four steps
are implemented. A release must explicitly report its implemented subset.

### Implemented subset and refusals

Minimal creation uses scratch by default. Same-worker placement supports an
idle, unarchived, top-level local session running carbon6's own loop. The
Folder/Worktree dialog and `sessions.place.list` / `sessions.place.change`
catalog operations share the same checked command path. Agent changes always
require a fresh approval for the exact request, even when the agent already
owns the session or holds a general write grant.

Cross-worker moves, native Claude Code moves, SSH transitions, delegated or
observed sessions, and moves with configured MCP servers are **not supported**
by this implementation. The worker chip does not offer a migration action.
Worker and realm capabilities are checked again by the command path; an old or
offline worker cannot silently fall back to an unsafe move.

Scratch copies retain their source, refuse symlinks/special files and existing
destination files, and are limited to 200,000,000 bytes, 4,096 entries, and 32
directory levels. Ordinary file permissions are preserved without set-id bits.
Worktree entry carries staged, unstaged, and non-ignored untracked changes;
ignored files remain in the source checkout. Binary, conflicted, or changed
submodule patches are refused. Setup output is retained on the placement event.
Leaving keeps the worktree and branch. Optional merging is fast-forward only
and requires both checkouts clean. Git preparation and the event journal are
not one filesystem transaction: a crash can leave a prepared worktree, and a
successful merge followed by a journal failure can leave the source branch
advanced while the session still reports its old place. Neither case deletes
the original files or changes the authoritative session place without an event.

Placement worktrees use cleanup `off`: retained places are not yet integrated
with archival cleanup. Move back is a new move, not a filesystem rollback; a
recorded worktree's repository, registration, path, and branch are revalidated.
Rewind across a move keeps placement and files in place and reports that fact.

## Creation and presentation

Only a worker and model are necessary; connection routing and last-used model
selection retain their current behavior. An omitted or empty `cwd` means the
worker's existing `<state>/scratch/<session>` workspace. Never substitute the
head's working directory. A worktree still requires a repository folder. SSH
creation retains its existing remote-home default, rather than creating local
scratch and pretending it is remote.

The default form is the prompt plus worker and model selectors. A More
disclosure contains folder, worktree, SSH, labels, and other advanced choices.
Collapsing it must not discard selected values. Templates and task cards keep
their explicit placement and worktree options.

The header describes the current worker, scratch/folder, worktree branch (if
any), and model. Clickable chips open their actual selectors. Do not present an
unimplemented action as an enabled control. The same information remains legible
on a narrow viewport. The composer model selector remains available.

## Placement transaction

Use one placement-change mechanism for user, agent, and Move back requests.
Resolve the session in the caller's realm and verify that the realm may use the
destination worker *before* exposing its filesystem or preparing a transfer.
The worker must be attached. Validate the destination and reject concurrent
changes. Changes are permitted only at turn boundaries, with no running
background tasks; stopping a task requires separate user approval.

Prepare the destination without changing session routing. Persist the successful
placement and its previous place, append a `PlaceChanged` transcript event, then
resume on the new place. Failed preparation leaves the old place authoritative.
Protocol additions use new field tags and never renumber existing ones. The
event records source/destination, branch, copied files, and uncommitted-file count
without leaking file contents. A short agent notice comes from the editable
`session.place_changed` prompt, not hard-coded agent instructions.

Previous places are retained for Move back, not treated as automatic backups.
Revalidate each old destination before reuse. Do not remove the old scratch or
worktree on a move: retain it until archival under the existing cleanup policy.
Inspection of the existing worker finds scratch creation but no scratch pruning:
do not invent automatic deletion as part of minimal creation. Worktree archival
already has remembered `off`/`ask`/`auto` cleanup with dirty/unpushed safety checks
(`docs/dev-starts.md` §5). Extending archival cleanup to retained places must apply
those same checks, not recursively delete arbitrary recorded folders.

## Same worker

Scratch to folder uses the worker's existing `fs_list` picker. Offer explicitly
to copy scratch files or leave them. Refuse overwrites and symlink escapes;
preflight the whole copy before changing the active cwd. Report copied paths.

Entering a worktree reuses `worktree::create`, its template options, and the
worker's worktrees directory. Start at the repository's current HEAD. Capture
tracked and untracked modifications, reject unsupported binary patches before
mutation, validate patch application, then carry the changes into the new
worktree. Never silently drop ignored files, submodules, or conflicting paths:
explain unsupported cases. Leave the original checkout intact on failure.

Leaving a worktree returns to the recorded main checkout, keeps its branch, and
offers an explicit merge-first option. A failed merge does not switch place.
Do not delete a branch or worktree as a side effect of leaving it.

## Another worker

This step remains unimplemented. Do not implement it by changing `worker_id`
and sending `Restore`: a prior destination journal can make Restore a no-op,
and queued commands still carry the old worker ID. The required foundation is
a durable move ID and state machine: fence routing and all command/report
queues; freeze the source actor and its processes durably; confirm the head's
event high-water mark; stage an inactive destination; atomically compare and
swap ownership; then activate with retryable receipts. Recovery must either
finish the same activation or safely abort before ownership changes. Returning
to an earlier worker also needs token-aware journal incarnation replacement.
The worker RPC message limit is smaller than the 200 MB transfer cap, so file
transfer must use bounded chunks rather than one large protobuf payload.

For carbon6's own loop, resume using the head's durable event history, as the
existing resume path already does. Ensure the source stops owning the session
before the destination accepts commands; it must keep no background process.

Claude Code must retain its native conversation identity. Transfer its JSONL
transcript to the destination's configured Claude projects directory under the
encoding of the **new** cwd; preserve the resume id and use `--resume`, reusing
the existing handoff logic. A summary-only handoff is not an acceptable substitute
for a native transcript move. Respect `CLAUDE_CONFIG_DIR`, including in tests;
never inspect or modify the user's live projects directory.

Scratch transfer goes through the head with a 200 MB total cap, bounded reads,
path validation, and a copied-file manifest. Reject traversal, special files,
symlink escapes, and oversize transfers before activating the destination.
Temporary transfer artifacts must be cleaned after success or failure.

For repositories, match a destination repository by normalized remote URL using
the worker's known folders/repos. If none matches, explain that fact before
moving anything; offer an explicit clone into the worker's usual repository
location rather than guessing a directory. Transfer the branch (push when
authorized, otherwise a bundle) and the tracked/untracked patch. Verify the base
commit and patch applicability before switching. Do not silently reset an
existing checkout or push to a remote without the requested authorization.

Checkpoints remain part of the same conversation. Rewinding across a move changes
conversation history only, and explicitly says that files stayed where they are.
Placement must not inherit the model-handoff restriction that forbids rewinding
across some runtime changes.

SSH addition/removal must use the same validated transaction where supported by
the existing remote runtime. Until native transcript, file, and process ownership
transfer are supported for SSH, reject such changes explicitly; continuing to
support SSH at creation is not support for moving an existing session over SSH.

## Agent consent

Expose a realm-checked catalog operation such as `sessions.move`, classified as
a consent-requiring change rather than a read or silent write. The agent supplies
the requested destination and reason. Open a one-time card: “‹session› asks to
move to rust-dev-02: needs a GPU”. Allow authorizes only the exact proposal and
executes it at the next safe boundary; reject/revoke or a changed proposal must
not move the session. Return completion or refusal to the requesting agent as a
notice. Guidance belongs in editable `session.carbon6` (or an editable dedicated
prompt), never a hard-coded instruction string.

## Verification

Use `crates/c6-worker/tests/it`'s Env/mock LLM and the two-worker patterns in
`remote_subagents.rs`. Wait on observable conditions, never scheduling sleeps.
Cover minimal scratch creation; scratch-to-folder with/without copy; tracked and
untracked worktree changes and leaving; both runtimes across workers (isolated
stand-in Claude and config dir); missing-repo refusal; active-background refusal;
agent consent; rewind; and exactly one event and editable notice per success.
Test failure atomicity and realm refusal as well as the happy paths. UI tests
cover reduced-form disclosure and place labels.

Run affected tests during development. Each shipping step requires native and
WASM workspace clippy with warnings denied; UI changes also require the Docker
production UI build and `scripts/ui-size.sh`. Rebase before the final full
`make test`; push only a verified fast-forward step. Capture desktop and 390px
screenshots using only a scratch head at ports 18290/18291 and data under
`/tmp/c6-gradual-vis`, with an isolated worker. Never use the live head, workers,
or user Claude configuration for qualification.

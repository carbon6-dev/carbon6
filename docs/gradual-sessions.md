# Gradual sessions

## Contract and delivery order

A session is a conversation with a current **place**, not a permanent binding to
its first working directory. Deliver this in four independently verified steps:

1. Minimal creation and visible place chips.
2. Same-worker placement changes, including entering and leaving worktrees.
3. Between-worker transfers, preserving the conversation and work.
4. Agent-requested changes with one-time user consent.

This document describes the implementation and its limits, not release
qualification. A release must report its verified subset.

### Implemented subset and refusals

Minimal creation uses scratch by default. Same-worker placement supports an
idle, unarchived, top-level local session running carbon6's own loop. The
Folder/Worktree dialog and `sessions.place.list` / `sessions.place.change`
catalog operations share the same checked command path. Agent changes always
require a fresh approval for the exact request, even when the agent already
owns the session or holds a general write grant.

The Worker chip offers cross-worker moves for idle, unarchived, top-level
local sessions, including native Claude Code. Both workers must be online,
serve the session's realm, and advertise fenced move support. The command path
rechecks eligibility; it does not fall back to an unfenced move. SSH placement
changes, delegated and observed sessions, and sessions with configured MCP
servers are unsupported. Same-worker changes still do not support Claude Code.
Native moves require an executable Claude CLI on the destination, checked
before freezing the source and again before preparing the placement receipt.

Scratch copies retain their source, refuse symlinks/special files and existing
destination files, and are limited to 200,000,000 bytes, 4,096 entries, and 32
directory levels. Ordinary file permissions are preserved without set-id bits.
Worktree entry carries staged, unstaged, and non-ignored untracked changes;
ignored files remain in the source checkout. Binary, conflicted, or changed
submodule patches are refused. Setup output is retained on the placement event.
Leaving keeps the worktree and branch. Optional merging is fast-forward only
and requires both checkouts clean. A durable transactional intent for
same-worker Git operations remains deferred. Git preparation and the event
journal are not one filesystem transaction: a crash can leave a prepared
worktree; a successful merge followed by a journal failure can advance the
source branch while the session still reports its old place. Neither case
deletes the original files or changes the authoritative place without an event.

Placement worktrees use cleanup `off`, and retained places are not integrated
with archival cleanup. Rewind across a move keeps placement and files in place
and reports that fact.

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
event records the source and destination, copied paths, worktree details, and
an uncommitted-file count where applicable, without file contents. A short
agent notice comes from the editable
`session.place_changed` prompt, not hard-coded agent instructions.

Previous places are retained for Move back, not treated as automatic backups.
Moving does not remove the old scratch or worktree, and archiving does not walk
placement history to clean up retained directories. There is no automatic
scratch pruning. Ordinary worktree archival has remembered `off`/`ask`/`auto`
cleanup with dirty/unpushed safety checks (`docs/dev-starts.md` §5); any future
cleanup of retained places must apply those same checks, not recursively delete
arbitrary recorded folders.

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
Do not delete a branch or worktree as a side effect of leaving it. Same-worker
Move back is unchanged: it reuses the recorded directory after validation (for
worktrees, including repository, registration, path, and branch). It does not
roll files back or copy the current workspace over the earlier one.

## Another worker

### Ownership and recovery

The head records a durable move ID and a new ownership epoch. Its sweeper
advances `requested` → `released` → `transferred` → `adopted` → `done`:

1. `requested`: Admit against the placement the caller saw, with an idle
   session and no pending commands or other move. Inspect the source and
   preflight the destination before freezing. Release stops the source actor
   and any idle native Claude process, persists a worker fence, and returns the
   last event sequence. Running turns, queued work, and running background
   tasks are refused, not migrated.
2. `released`: Wait for the head's durable event high-water mark to match the
   frozen source, then transfer the snapshot. A source that advances beyond its
   freeze watermark cannot be adopted.
3. `transferred`: Prepare the new workspace and import the session spec and
   journal as a blocked destination incarnation. A newer epoch can replace a
   stale journal from an earlier visit to that worker; this is not a `Restore`
   sent after changing `worker_id`.
4. `adopted`: In one head transaction, compare and swap worker and epoch at
   the frozen watermark, recheck realm access, and append exactly one
   `PlaceChanged` event. Retry destination activation for the same move ID and
   epoch if interrupted. After adoption there is no rollback to the source;
   if the destination is offline, the move waits for it to reconnect.
5. `done`: Activation has been acknowledged and normal command delivery resumes
   at the destination.

Ownership checks at the head and in the worker journal fence stale commands and
reports. A refusal before adoption goes through `rolling_back` to `failed`:
source routing remains authoritative, but commands remain gated until a
possibly frozen source acknowledges abort. Cleanup of transfer staging is
retryable and can wait for an offline worker. It does not delete retained
workspaces or a reusable repository cache.

### What transfers

- **Workspace:** The manifest and checksummed chunks travel through the head
  over the existing worker link. Chunks are at most 256 KiB; the whole artifact
  (files, session metadata and history, plus any repository or native transcript
  data) is capped at 200 MB (200,000,000 bytes), with separate entry and metadata
  bounds. A scratch or ordinary folder snapshot goes into a fresh managed
  directory. Traversal, symlinks, special files, and conflicting paths are
  refused rather than followed or overwritten.
- **Git:** Preflight matches the normalized source remote against known folders
  from the destination journal and its managed repository cache; it does not
  scan the filesystem for repositories. Ambiguous matches are refused. If
  nothing matches, the Worker dialog offers an explicit clone opt-in. An
  authorized clone with a supported URL populates a reusable bare seed under
  the destination worker's state directory. The move itself never pushes.
  It carries the current branch and its reachable commits (including unpushed
  commits) in a bundle, separate staged and unstaged patches, and non-ignored
  untracked files. The destination verifies the bundle and expected commit and
  checks patch applicability in a fresh standalone checkout, not a linked
  worktree. It does not reset an existing checkout. Ignored files and other
  local branches are not copied. Detached HEADs, unsupported or missing
  remotes, custom Git filters, conflicts, tracked binary changes, Git symlinks,
  submodules, and assume-unchanged or skip-worktree index entries are not
  supported. Refusing the latter prevents Git from silently hiding local edits.
- **Conversation:** The same session ID, history, and checkpoints follow the
  move. Carbon6's own loop resumes from the durable journal. For native Claude
  Code, the worker copies the exact JSONL for the journal-selected resume ID
  into the project directory encoded from the new cwd and continues with
  `--resume` and that same ID. It honors the worker's `CLAUDE_CONFIG_DIR`
  (default `~/.claude`); tests must use an isolated config directory. No scan
  of other projects, overwrite of an unrelated transcript, or summary fallback
  substitutes for a missing or unusable native transcript.
  Config/project directories and transcripts must have trusted ownership and
  must not grant write access to other users; shared writable paths and
  symlinked config/project directories are refused (apart from macOS's standard
  `/tmp` and `/var` aliases). These checks cover POSIX permissions, not extended
  filesystem ACLs.

**Move back across workers creates a fresh snapshot on the previous worker
from the session's current files**, subject to the same transfer limits. The
recorded previous folder can help match the repo, but it is not reused as the
workspace. Both earlier directories—the old folder on that worker and the
just-departed source—are retained. Back does not authorize a new clone.

Rewind changes conversation history, not placement or files, and reports that
files stayed in place. Supporting SSH at creation does not support adding,
removing, or migrating an existing SSH placement.

## Agent consent

The realm-checked `sessions.place.change` catalog operation requires one-time
consent for the exact request. The agent supplies the destination, the current
placement sequence, and optionally a reason. Allow submits the approved request
once through the same checked path as a direct user move; it does not override
idle or placement checks. Not now or expiry changes nothing, and a changed
proposal requires a new approval. The requesting agent receives the outcome as
a notice. Guidance belongs in editable prompts, not hard-coded instructions.

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

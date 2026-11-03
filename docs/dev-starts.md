# How dev work starts — Design

Status: built (2026-11-03; §8 is what was built). Decided with you:
- **Nothing of your own setup is built in.** No branch prefix, no `master`, no submodules, no VM manager: every one of them is a value in a template you edit. carbon6's defaults stay as they were: a worktree from the current checkout (`HEAD`) on a `c6/{{id}}` branch, no setup commands, no cleanup.
- **A template says how a worktree is made:** its branch name, what it starts from, whether it tracks that, and the commands that prepare it.
- **A template may name a pool of workers and pick one:** named workers or workers with a label, the least busy online one.
- **A project, or a work item, names the template its work starts with.** Every way of starting work there uses it, and the form opens filled from it.
- **A work item remembers where its work lives** (the worker, the worktree, the branch), so everything later for it goes back there.
- **When the work ends, the worktree and branch can go:** off, ask (the default once turned on) or automatic, and only when nothing would be lost.
- **A hook runs when work ends,** for whatever else is yours to do.

## 1. The problem

You do dev work on a few machines (VMs, say), each with an enrolled worker. Each task gets its own git worktree and branch, cut from the latest main line, with its submodules fetched; when the task is pushed, the worktree and branch go. carbon6 can start a session in a worktree, but only one way (`c6/<id>` from `HEAD`), and every other step was a message you typed at each start: "Create a branch called vinayakb/kafka_ingest off of latest master. Do not track a remote branch. Fetch all submodules." Starting from a to-do, a task card or a project's + Work each began from a different session's settings, and a worktree stayed behind after the work was done.

## 2. A template's worktree

A template with **New git worktree** gets more settings, each shown (and editable) in the session form it fills:

| Setting | | Default |
|---|---|---|
| **Branch** | a pattern: `{{name}}` is the work item's title in snake_case (with no work item, the first message's first line), `{{id}}` the session's short id. Checked as a git branch name | `c6/{{id}}` |
| **Base** | what the branch starts from, e.g. `origin/master` | `HEAD` (the folder's checkout) |
| **Fetch first** | fetch the base from its remote before branching, so it is the latest | off |
| **Track the base** | the new branch tracks the base (`--track`); off creates it with `--no-track` | off |
| **Setup commands** | lines run in the new worktree before the first turn, e.g. `git submodule update --init --recursive`. Their output is shown in the transcript; one that fails stops the start, with its output | none |
| **When the work ends** | off, ask or automatic (§5) | off |

So the message you used to type becomes a template: branch `vinayakb/{{name}}`, base `origin/master`, fetch first, no tracking, setup `git submodule update --init --recursive`.

## 3. Where: a pool, pick one

A template's **workers** still work as before (empty: the online one; a list or `*`: one session on each). A template may instead **pick one**: from named workers or from **workers with a label** (labels you give workers on the Workers page), it takes the online one with the fewest running sessions. The form says which and why ("rust-dev-03, the least busy of dev-vm").

## 4. Start with, and the work's place

- **A project names its "start with" template,** and a work item may name its own (it wins). Every way of starting work in it uses that template: the project's + Work and quick sessions, a to-do's ▶ Start, Plan the next phase, Pick up, and a task card's **Start it** for a card raised in that work's sessions. The form opens filled from it; you can change anything.
- **The first session started for a work item through such a template** records the work's **place** on the work item: the worker, the worktree's folder, its branch and the template's "when the work ends".
- **Later starts for that work go back there:** the same worker, in that worktree, with no new worktree or branch. The form says so ("rust-dev-02, where this work's worktree is"). If that worker is offline, the form says so too: you may wait, or pick another worker, which makes a fresh worktree there (from the branch, if it was pushed).

## 5. When the work ends

When a work item with a place is closed (done or dropped), or a session with a worktree in no work is archived:

- **off:** nothing is removed.
- **ask:** a decision, "Remove the worktree and branch?", with **Remove** and **Keep**.
- **automatic:** it is removed at once.

Removing is done by the worker, and only when nothing would be lost: the worktree has no uncommitted changes, and the branch's commits are on a remote or merged into the base. Otherwise nothing is removed, and you are told what's left ("3 commits on vinayakb/kafka_ingest are on no remote"). Either way, the work item then forgets its place.

## 6. The "work ended" hook

A hook for the new event **WorkEnded** (Settings → Hooks) runs on the worker after that, in the worktree's repository, with `C6_WORKTREE`, `C6_BRANCH`, `C6_REPO` and `C6_REMOVED` (`1` or `0`) set, so anything beyond removing the worktree (closing a ticket, stopping a VM) is yours to script. It runs even when the setting is off, as long as the work had a place.

## 7. Build order

1. Template settings and the worker's worktree: branch pattern, base, fetch, tracking, setup commands.
2. Worker labels and pick-one pools.
3. Start with on projects and work, the start plan, and the form.
4. The work's place, and going back to it.
5. When the work ends: the decision, the worker's safe removal, the WorkEnded hook.

## 8. As built (2026-11-03)

- **API** (`c6_api::starts`): `WorktreeOptions` (branch pattern, base, fetch, track, setup, cleanup `off`/`ask`/`auto`), `branch_name`/`snake_name`/`check_branch`/`check_options`, `WorkPlace`, `StartQuery`/`StartPlan`, `WorktreeEnd`/`WorktreeEnded`. `Template` gained `worktree_options`, `pick` and `worker_labels`; `CreateSession` gained `worktree_options`; `Worker` gained `labels`; `WorkFields` gained `start_with` and `place`, `ProjectFields` `start_with` (both patchable; `WorkPatch.forget_place` forgets a place). `HOOK_EVENTS` has `WorkEnded` (`CARBON6_HOOK_EVENTS`: left out of Claude Code's own hook settings). Proto: `CreateSession.worktree_spec` (`WorktreeSpec`).
- **Worker** (`c6-worker/src/worktree.rs`): checks the branch with `git check-ref-format --branch`; with Fetch first, `git fetch <remote>` of the base's remote (else `--all`); checks out a branch that exists locally again, starts one that is on a remote (and not here) from there, else `worktree add -b <branch> --track|--no-track <path> <base>`. Setup lines run with `sh -c` in the worktree's top folder, one after another, with `C6_WORKTREE`, `C6_BRANCH`, `C6_REPO`, up to 30 minutes each; their output is a quiet notice "Worktree set up" in the transcript; one that fails takes the worktree (and a branch it made) away and fails the start with its exit code and output. They run like hooks and the shell tool do on the worker: as the worker's user, unsandboxed; tool-call policies don't apply (they are your commands, not the agent's). RPC `worktree.end` removes a worktree only when `git status --porcelain` is empty and `git rev-list <branch> --not --exclude=<branch> --branches --remotes` is empty (pushed, or merged into another branch), then `git branch -D`; then runs the realm's WorkEnded hooks (label selectors apply, by the first session's labels) with `C6_WORKTREE`, `C6_BRANCH`, `C6_REPO`, `C6_REMOVED` and the JSON on stdin.
- **Head** (`c6-head/src/starts.rs`): `GET /api/v1/starts` (catalog `sessions.start_plan`) gives the plan: the template asked for, else the work item's, else its project's; back to the work's place when it has one (offline: says so; `worker=` another: a fresh worktree of the place's branch there); else the template's worker: pick-one takes the online worker of its pool with the fewest starting or running sessions (ties by name), named workers the first named one, else the first online one. `PUT /api/v1/workers/{id}/labels` (catalog `workers.labels`, console). Template runs and remote subagents pick from the pool too (`templates::targets`). A session created with `worktree_options` is remembered in `worktree_starts` (migration 0061, also `workers.labels`); when it has started and joined work with no place, the work records its place. Closing work (done or dropped) with a place, or archiving a remembered session in no work: off runs the WorkEnded hooks only, ask opens a decision (`worktree:<owner>:<nonce>`, group Worktrees, Remove / Keep), auto removes at once; the result (removed, or what's left and why) is a `work` notification with the hooks' lines, and the work item forgets its place. A task card's Start it, for a card raised in a session of some work, starts as that work's plan says (its worktree, or its template) and joins that work; without a plan it starts as before.
- **UI:** Settings → Templates has Pick one worker, Worker labels, Branch, Base, Fetch the base first, Track the base, When the work ends, Setup commands (one per line; nested `a.b` fields in the list editor), and a "Dev work in its own worktree" example. The console's Workers page edits labels (realm pages show them). Projects and work items have a **Start with** select; a work item shows its worktree. The new-session form, whenever it is opened for some work (▶ Start, Pick up, Plan the next phase, a project's quick session, the Work field), asks for the plan and fills worker, folder, worktree, model, effort and labels from it, with a line saying the template and why this worker, the new branch (editable) and its base, or "in its worktree"; picking another worker asks again.
- **Verified:** unit tests (branch patterns and names; pool pick: least busy, online only, ties, labels and names), worker tests on real git in temp repos with a submodule (fetch from the moved-on base, no-track/track, setup and submodule checkout, failing setup and its cleanup, a pushed branch elsewhere; removal refused when dirty or unpushed, done when pushed or merged, the hook's env), and an integration test with a head and two labelled workers (plan, first start, place, setup output in the transcript, second start reusing the worktree, another worker's fresh worktree, close → ask → Remove with the hook, auto on archive, failing setup). In a browser on a scratch head with two labelled scratch workers: the template editor, a project's Start with, ▶ Start picking the least busy worker and showing the branch, a second start going back to the worktree, and closing the work → the decision → Remove.
- **Not built:** a place started from the form doesn't record the template's name (it's informational only); Pick up and Plan the next phase use the plan only through the form (the API endpoints that return their first message don't); no UI for `forget_place`; the "ask" decision isn't re-raised if the worker is offline when you answer (you're told nothing was removed, and the place is forgotten).

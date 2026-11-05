# Tests that fail only when something is wrong

Status: rules in force (2026-10-19); `make check` enforces what a pattern can catch (`scripts/test-lint.sh`).

A test is a claim: *this invariant holds*. It must fail **if and only if** the invariant is broken. A slow machine, a busy CI runner, another test running beside it, or the order tests run in must never fail it. The test suite runs on GitHub's runners (`ubuntu-24.04`, `macos-15`), which are shared, small and sometimes slow, and it must hold there.

## Rules

1. **Wait for a condition, never for a time.**
   - Wait with `c6_testkit::eventually(what, condition)`, or for a future with `c6_testkit::within(what, fut)`.
   - Their only deadline catches hangs: 5 minutes (`C6_TEST_DEADLINE_SECS`). That's far longer than anything takes while it works, so a slow machine makes a test slower, never failed.
   - No `tokio::time::timeout(Duration::…)`, no local deadline loops, and no copies of `eventually` with their own 10, 20 or 30 seconds.
2. **Assert once the thing is finished.**
   - Count events, read files or compare states after an explicit sign that the work is done, never during it. Signs: the turn ended (`turn_ended`), the session is idle, the state is `done`, the mock got its request, the text streamed.
   - A session is also briefly idle after `started`, before its queued prompt starts. For a first turn, require `turn_ended` (or idle **and** `end_reason`), not idle or a quick project's `ended` alone.
   - Example: the imports test counted a turn's events when its reply appeared, before the turn's usage and end arrived (0.3.5).
3. **Act on a signal, not after a pause.**
   - To interrupt a stream, interrupt once its first text has arrived, not 300 ms after starting.
   - To restart a head during a turn, gate a mock response and wait for its request before stopping the head. Release it while the head is down and check offline progress in the worker's journal, then restart and wait for the new attachment. This places the restart inside the turn even when setup or reconnect is slow, without losing offline-progress coverage.
   - A pause that a slow machine could outlast is a race.
4. **"Nothing happens" checks may sleep.**
   - A pause before asserting that something did *not* happen can't fail falsely. A slow machine only makes it a weaker check.
   - Prefer waiting for a sign that the thing was considered and declined.
5. **Tests share nothing they don't own.**
   - **Environment:** a test in a shared process (the `it` binaries, any crate's unit tests) never changes the environment. Other tests read it concurrently, which is undefined behaviour on Linux. Set what the code under test reads on that code instead: the worker's `Overrides`, `SshHost::with_program`, the head's `HeadConfig`. A standalone test binary with one test may set the environment before it starts anything.
   - **Ports:** bind port 0. A server that must come back on the same port (a head restarting while its workers keep its address) takes `c6_testkit::port()`. Those ports come from a range below every OS's ephemeral range, so no test binding port 0 is ever handed one, and each is held by a file lock for the life of its test process, so tests in other processes (nextest runs each test as its own) never share one. Never pick a free port, release it, and bind it later: another test can take it in between.
   - **Files:** each test works in its own temporary folders, never `$HOME` or the repository.
   - **Programs it writes:** a script a test writes and then runs can fail with "Text file busy" while a process another test just started still holds it open for writing. Wait with `c6_testkit::runnable(path, args)` before using it.
   - **Child processes:** own and clean up the whole process group, including on a panic. Killing just a shell leaves its children holding nextest's output handles open (`LEAK`), even after the test returns.
6. **What a test needs from the machine, CI has.**
   - A test that needs Chrome, git or a shell says so with `c6_testkit::need(what, present)` or `have(tool)`. On a laptop without it, the test skips. In CI (`GITHUB_ACTIONS`) a missing tool fails, so coverage never disappears quietly.
   - Tests that build container images (`C6_TEST_KITS`) are opt-in and don't run in CI.
7. **Only loopback.** Providers, GitHub and OAuth are fakes on `127.0.0.1` (`c6_testkit::MockLlm`, small axum servers). No test reaches the internet.
8. **The product's own time limits must not fail a test either.** Where the code under test waits (a browser starting, a credential, an RPC), its limit is generous enough for a loaded machine. For example, the browser now gets 60 seconds and says why it didn't start.

## Enforced

`scripts/test-lint.sh`, run by `make check` (locally and in CI), rejects:

- a local `fn eventually` or `fn until`;
- `tokio::time::timeout(` in tests;
- a deadline of `Instant::now() + Duration::from_…` in tests;
- `std::env::set_var` or `remove_var` in a shared test process;
- `fn free_port`;
- a code block in a doc comment that would be a doctest (untagged or `rust`). CI and `make test` run the tests with nextest, which never runs doctests, and the workspace has none, so examples in doc comments name their language (`text`, `sh`, `json`…).

A line that must break a rule says why: `// test-lint: allow (<reason>)`. Today only the opt-in kit builds do.

## Checked under load

The suite is also run with every CPU core kept busy by other processes, to surface timing assumptions a lint can't see (rule 2). A failure there is a bug in the test or the product, never "just load".

Use `NEXTEST=1 make test-under-load RUNS=5` for repeated workspace runs. Do not rebuild test binaries in the same target directory while nextest is running: Cargo can remove binaries nextest has listed but not yet started.

SQLite contention regressions use a real temporary WAL database and simultaneous operations on its connection pool, not just an in-memory database or serial calls. A transaction that reads before writing must take the writer with `BEGIN IMMEDIATE` before its first read: `busy_timeout` cannot repair a stale read snapshot's upgrade to a write. Related rows must commit together too (for example, a claim and all its evidence); fault injection should verify that a failed insert leaves none of a partial operation behind. Neither a longer timeout nor a retry is a fix for these atomicity bugs.

## Tile focus and first click: manual browser check

Focus and active-pane selection run on pointerdown. The pane header and its conditional buttons must stay mounted when only focus/active changes: detaching the original target between pointerdown and click loses its click. Use cached boolean conditions (`Show`) for their presence, not a closure that rebuilds them on every position change. Growing a tile still waits until click bubbles, after the action. The native `c6-ui` tests don't exercise browser event dispatch, so check this in a browser:

1. Open two session tiles side by side, with the left focused. Click **×** in the right tile's bar: it closes on the first click. Reopen it and refocus the left tile before each check below.
2. In the unfocused right tile, click **⋯**, **Changes**, **Terminal**, **Minimize**, and **Focus** (maximize). Each acts immediately; actions that leave the tile open focus it too. Bring it back or defocus as needed. Check any view tabs in its content the same way.
3. Open a second pane inside the right tile (a list and item, or a session and Changes). With the left tile focused, click **×** in either of the right tile's *pane* headers. It closes that pane on the first click. Also try its pin button and clicking a control in the inactive pane of an already-focused tile.
4. Click a transcript link in an unfocused tile: its destination opens on the first click. Type a draft in that tile, refocus the other tile, and click **Send**: it sends once and clears the draft. Try keyboard activation (Tab, then Enter/Space) and a touch tap as well.
5. Repeat with enough tiles for three to share a column (nine tiles): the clicked tile may grow only after its action, never move the target between press and release. Clicking plain transcript content should focus its tile, and shortcuts should then act there. A press canceled by dragging or scrolling should not activate a control.
6. Run the transcript-follow checks below too: focusing by clicking content must not interfere with wheel/touch/keyboard scroll intent or the transcript's tabindex.

For a browser-automation regression, keep the target element's DOM handle, send pointerdown, allow an animation frame, then send pointerup. Assert that the target stays connected through pointerdown and the action happens after one click. In particular, a split tile's pane-header close reproduces the old replacement reliably; an atomic synthetic `click()` can hide the interval where rendering occurs.

## Transcript follow: manual browser check

The `c6-ui` workspace unit tests cover input direction and stop/resume decisions. In a browser, use a long session with a streaming reply:

1. Open it normally: it starts at the bottom and follows new text. While there, roll the wheel up one notch (also try a tiny trackpad movement). It must stay up as tokens arrive or the window resizes, even if less than 120px from the end. Repeat during a smooth glide.
2. Focus the transcript and try Up, Page Up, Home and Shift+Space. Each stops following immediately. Down/Page Down/End and scrolling down resume only at the very bottom (less than 4px); being merely near the bottom does not resume. Check the scrollbar too.
3. Scroll more than a screen away, click **↓ Latest** (or its new-event count), and confirm that streaming follows again. Fold/show the tile while following, and while just a few pixels above the bottom with following off; each keeps its prior choice and position.
4. Open a search hit near the end. Its anchor must stay put through streaming and resize, not start following because it is near the end. Click Latest to resume.
5. On a phone, tap without dragging: following stays on. Drag toward older content by a small distance: it stops immediately and stays off through touch inertia. Drag back to the bottom or use Latest: it resumes. Try reversing direction within one gesture.
6. Check that horizontal scrolling and typing/navigation in text fields do not change transcript following. With reduced motion enabled, repeat the small upward scroll and Latest checks.

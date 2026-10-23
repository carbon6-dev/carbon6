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
   - Example: the imports test counted a turn's events when its reply appeared, before the turn's usage and end arrived (0.3.5).
3. **Act on a signal, not after a pause.**
   - To interrupt a stream, interrupt once its first text has arrived, not 300 ms after starting.
   - A pause that a slow machine could outlast is a race.
4. **"Nothing happens" checks may sleep.**
   - A pause before asserting that something did *not* happen can't fail falsely. A slow machine only makes it a weaker check.
   - Prefer waiting for a sign that the thing was considered and declined.
5. **Tests share nothing they don't own.**
   - **Environment:** a test in a shared process (the `it` binaries, any crate's unit tests) never changes the environment. Other tests read it concurrently, which is undefined behaviour on Linux. Set what the code under test reads on that code instead: the worker's `Overrides`, `SshHost::with_program`, the head's `HeadConfig`. A standalone test binary with one test may set the environment before it starts anything.
   - **Ports:** bind port 0. A server that must come back on the same port (a head restarting while its workers keep its address) takes `c6_testkit::port()`. Those ports come from a range below every OS's ephemeral range, so no test binding port 0 is ever handed one, and each is held by a file lock for the life of its test process, so tests in other processes (nextest runs each test as its own) never share one. Never pick a free port, release it, and bind it later: another test can take it in between.
   - **Files:** each test works in its own temporary folders, never `$HOME` or the repository.
6. **What a test needs from the machine, CI has.**
   - A test that needs Chrome, git or a shell says so with `c6_testkit::need(what, present)` or `have(tool)`. On a laptop without it, the test skips. In CI (`GITHUB_ACTIONS`) a missing tool fails, so coverage never disappears quietly.
   - Tests that build container images (`C6_TEST_KITS`, `C6_TEST_DECKS_RENDER`) are opt-in and don't run in CI.
7. **Only loopback.** Providers, GitHub and OAuth are fakes on `127.0.0.1` (`c6_testkit::MockLlm`, small axum servers). No test reaches the internet.
8. **The product's own time limits must not fail a test either.** Where the code under test waits (a browser starting, a credential, an RPC), its limit is generous enough for a loaded machine. For example, the browser now gets 60 seconds and says why it didn't start.

## Enforced

`scripts/test-lint.sh`, run by `make check` (locally and in CI), rejects:

- a local `fn eventually` or `fn until`;
- `tokio::time::timeout(` in tests;
- a deadline of `Instant::now() + Duration::from_…` in tests;
- `std::env::set_var` or `remove_var` in a shared test process;
- `fn free_port`.

A line that must break a rule says why: `// test-lint: allow (<reason>)`. Today only the opt-in kit builds do.

## Checked under load

The suite is also run with every CPU core kept busy by other processes, to surface timing assumptions a lint can't see (rule 2). A failure there is a bug in the test or the product, never "just load".

# Tests that fail only when something is wrong

Status: rules in force (2026-10-19); `make check` enforces what a pattern can catch (`scripts/test-lint.sh`).

## Narrow desktop tiles

Tile-bar mouse slots follow the tile's content width: 36px at 40rem and
above, 32px at 30–40rem, and 28px below 30rem. Phone/coarse-pointer slots
remain 44px. Optional marks yield below 40rem, before Changes, Terminal and
Pin move into Actions below 30rem. Icons and the trailing Actions, Maximize,
Close order are unchanged. `tests/tile-marks.cjs` checks resize boundaries,
retained controls, title room and both themes at 240–960px; `BEFORE=1` uses
the old fixed 28px assertions against a pinned baseline `ROOT`.
`SCREENSHOTS=/tmp/q1-screenshots/after` selects its screenshot directory.

`crates/c6-ui/tests/narrow-tiles.cjs` reuses the loopback production-browser
fixture below. It keeps the desktop window at 1600×1000 and resizes the actual
tile to 240, 320, 400, 480 and 640px, in both themes. It records screenshots
and element geometry in `/tmp/p3-screenshots/`, checks radio/text overlap and
elements escaping the tile, and checks 320px, 390px and landscape phones.
Run after building, with the same browser overrides as `motion.cjs`:

```sh
/home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/narrow-tiles.cjs
```

`BEFORE=1 ROOT=/path/to/pinned/dist` records baseline screenshots without
enforcing the new layout invariants; `VIEWS` selects comma-separated view
paths. This deliberately tests pane width independently of window width.
`ISOLATE_BROWSER=1` uses a fresh browser process for each desktop, split,
phone or coarse-pointer fixture group. As with the motion harness, report
shared-process failures alongside isolated results; isolation is not a retry.
The workspace gives each pane a named `@container/view`; view layout uses
named container breakpoints, while global phone navigation still follows the
viewport and pointer. Settings adds `@container/settings` to its content
column, so cards also adapt when its wide-screen side menu is present.
The populated fixtures cover list and detail views, cleanup decisions,
comments and open forms. Real two-pane layouts check each pane independently
and the retained single-pane/back navigation below 40rem. Coarse-pointer bars
are also checked down to 132px; natural four-column layouts verify there is
no horizontal canvas scroll at 1600, 1024 and 768px window widths.

Tables, code and other deliberately scrollable data regions are not a
workspace scrollbar. Calendar's seven-column month and the Workers table
still have their existing dense/data-table designs; don't mistake overflow
containment for a redesigned narrow version of those views.

## Same-tile session panes

`node crates/c6-ui/tests/session-panes.cjs` uses the same production fixture
and browser overrides as `motion.cjs`. It clicks Changes, Terminal, Files
and Outputs at 1200, 800, 640, 480, 320 and 240px tile widths, plus 320/390px
phones, in both themes. It asserts unchanged tile count, retained transcript,
single-pane Back/reopening, the phone sheet route and explicit ⌘-click to
a separate tile. Screenshots default to `/tmp/q1-screenshots/panes/`.

## Browser-local themes

`crates/c6-ui/tests/theme.cjs` checks System, Light, unchanged Dark and Soft:

```sh
node --test crates/c6-ui/tests/theme.cjs
SCREENSHOTS=/tmp/p3-screenshots node crates/c6-ui/tests/theme.cjs --browser
ENGINE=webkit THEME_DELAY_MS=1000 node crates/c6-ui/tests/theme.cjs --browser
```

Use Node 24 and the same browser overrides as the motion harness. The browser
test delays the real theme bootstrap fetch, holds WASM, and checks the first
body frames before the application renders. It also tests four usable radio
cards at 240/320/400/480/640px tile and phone widths, persistence, OS changes,
and real cross-tab palette and selected-card updates.

Soft keeps the requested greys and existing accents, not a blanket contrast
guarantee. The sample document's red `#f87171` on chip `#33343a` is 4.484:1:
it rounds to 4.5 but is below the unrounded AA threshold. Current Tailwind 4
red-400 resolves to sRGB8 `#ff6467`, measuring 4.294:1 on that chip. The test
prints both the sample swatches and the actual built tokens rather than
silently changing accents or treating rounded figures as a pass.

## Motion: production browser regression

`ONLY=sessionsPicker` checks the phone Sessions header's visual alignment,
retained SVG icons, accessible close handle, and tap-outside/handle/swipe/Escape
dismissal. Include `VIEWPORT=narrow,phone,landscape THEME=light,dark` for the
320px, 390px and short-height phone layouts.

`ONLY=modelPicker` checks the composer chip's bottom sheet on those phone
layouts and its unchanged desktop popover: arbitrary model ids, the last item
in a long offered list with an overlay keyboard, effort, all dismissal paths,
and one prefetched model-list request across repeated openings. It also checks
that the phone label, field and list span the sheet's normal padding, with a
close handle but no redundant top X. Phone model
screenshots end in `-model-picker.png` and `-model-picker-keyboard.png`.
`ONLY=modelForms` checks the other inline `ModelPicker` users in Ask and the
new-session form on both warm navigation and cold page loads, including
arbitrary text, retained input/focus during list loading, offered-model/effort
selection and cache reuse.

`crates/c6-ui/tests/motion.cjs` serves the built production UI on an owned
loopback port, with fake HTTP and WebSocket head responses. It does not connect
to your head, use your browser profile, or require an npm install. Build first
(coordinate the build when sharing a worktree), then run with Node 24:

```sh
env -u NO_COLOR make ui
/home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/motion.cjs --unit
ENGINE=both /home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/motion.cjs
ENGINE=both /home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/motion.cjs --helpers
```

The local defaults use `playwright-core` at
`/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1`, Chromium at
`/tmp/c7-chromium/chrome-linux/chrome`, and the WebKit wrapper
`/tmp/c7-webkit.sh`. Elsewhere set `PLAYWRIGHT_CORE`, `CHROMIUM`, and
`WEBKIT_EXECUTABLE`. `ROOT` overrides `crates/c6-ui/dist`. Missing browsers or
motion hooks fail explicitly; this standalone opt-in harness does not silently
skip coverage. `--unit` needs only Node and reads the actual `js/motion.js` into
isolated VM contexts: default/invalid choices, OS changes, explicit Full,
cross-tab notification, cancellation, and denied/quota-failed storage.
VM checks also distinguish JavaScript/transition cancellation from preserved CSS keyframes;
a source regression requires Off to hide outgoing count/notice decorations.
The notice source guard also keeps the retained last content, immediate inert
closed state, Undo action and non-ticking timeout (no visual countdown).
Production list-FLIP VM checks exercise inserted keys, inert removal ghosts,
interrupted frame cancellation, Full/Reduced/Off keyframes, and absence of
per-animation-frame geometry reads. They do not mount the comments UI.
`--helpers` additionally loads the built production helper into a real DOM in
fresh browser processes: Full/Reduced/Off in each selected engine (six cases
with `ENGINE=both`). A read-only adapter observes the helper's private WeakMap.
Native completion must both detach the inert removal ghost and empty the
state's retained-ghost array; DOM removal alone is not sufficient. Off must
create neither a ghost nor a timed effect. This still does not mount comments.
Bounded resize-FLIP checks cover growth interrupted mid-scale, replacement
animation ownership, and fade-only Reduced/Off behavior without mounting catch-up.
The glide VM regression checks its shared 200ms deadline, same-target
deduplication, interruption from displayed position, immediate input/snap
cancellation, preference changes, detached nodes, and three geometry reads per
invocation with none in animation frames.
Chromium also uses `/tmp/c7-browser-libs/usr/lib/x86_64-linux-gnu` when present
for its local shared libraries; `CHROMIUM_LIBS` overrides that directory and
the inherited `LD_LIBRARY_PATH` is preserved. The WebKit wrapper owns its own
library environment.
`ISOLATE_BROWSER=1` additionally owns a fresh browser process per case, not
just a fresh context. This is useful when diagnosing old WPE WebKit process
reuse: it does not retry or suppress an error, and shared-process failures
should still be reported with the isolated result rather than discarded.

By default each engine runs 1440×1000 desktop and 390×844 phone, light and dark, with
System (OS normal/reduced), Full (OS normal/reduced), Reduced and Off. For a
focused rerun, filters accept comma-separated values:

```sh
ENGINE=webkit VIEWPORT=phone THEME=dark MOTION_CASE=full-reduce ONLY=menu,sheet,tool,sendStop \
  /home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/motion.cjs

# Opt-in corners: 16 cases total with both engines and both themes.
ISOLATE_BROWSER=1 ENGINE=both VIEWPORT=narrow,landscape MOTION_CASE=full,reduced \
  ONLY=menu,sheet,phone,tool,sendStop SCREENSHOTS=/tmp/m1-screenshots/corners \
  /home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/motion.cjs
```

`VIEWPORT=narrow` selects 320×568; `VIEWPORT=landscape` selects 900×480.
Neither is included in the default matrix. Both use phone checks, including
the drawer: the fixture uses the product's width < 768 **or** height < 512
condition, rather than a width-only phone assumption. Labels include the view
name so corner artifacts do not overwrite the ordinary 390px phone cases.

Checks cover pre-hydration policy and persistence, cross-tab/realm preferences,
first-frame menu focus/hit-testing/action, phone sheets and scrims, interrupted
exits and presence disposal, overlay/offset and resized keyboards, desktop tile
open/close/reorder, phone-row composer position and transcript following,
native tool disclosure, D2 live/result output height and node identity, and
first-frame Send/Stop interactivity. Off audits actual computed CSS durations
and captured effects; Reduced rejects travel, spin/pulse and long effects.
Native details height interpolation is checked at 160ms when the engine
supports it; otherwise Full requires a captured 160ms body-reveal fallback.
The actual open/closed geometry is checked in both cases. The harness prints
the engine capability, captured timing and whether an intermediate frame was
observed (the last is diagnostic, not a frame-rate-dependent assertion).
The Actions menu's real Catch-up switch is toggled off and back on. Its fake
server holds each response while the test checks the exact PUT payload and
unchanged `aria-checked`, then acknowledges success and requires the new state.
Phone checks keep soft-wrapped and explicitly multiline drafts focused across working/idle cycles and
require a settled absolute end gap below one CSS pixel. They sample after
additional animation frames, not just the first successful ResizeObserver
snap: redundant textarea `height:auto` measurement can briefly expand the
transcript, clamp its end, then leave a gap when the measured height returns.
Separate checks retain real text-growth and width-change autosizing and verify
that an empty multiline suggestion grows the box and disappears on working.
Both draft inputs must reach and retain the true transcript end without a
test-authored scroll snap before the working/idle cycles begin.

Waits use DOM state, request completion, animation completion, or animation
frames—not fixed real-time pauses. The browser clock is explicitly advanced
across the product's 299/301ms Full presence boundary (79/81ms for Reduced),
and to interrupt an exit before disposal; host load cannot accidentally miss
the interruption. `C6_TEST_DEADLINE_SECS` controls the generous hang deadline,
not a performance threshold. FLIP guards record production root geometry
reads by frame and reject layout-property keyframes; they are not a substitute
for an engine performance trace proving compositor-only rendering. Synthetic
native drag events exercise production handlers, not OS drag gesture fidelity;
emulated visual viewports do not replace a physical keyboard/accessibility
review. Visual spring quality and interrupted-animation velocity still need
human review.

Tile contents never scale, including opening/closing, maximize/restore, pane
changes and neighbours growing back. Size changes commit the destination at
once: an empty surface-coloured veil fades away in 100ms (80ms Reduced), while
the actual tile and background remain opaque. No outgoing text/form clone is
inserted over the destination. Only same-size moves translate; Off is instant.
`node crates/c6-ui/tests/tile-motion.cjs` exercises six long-content tiles in
three- and four-column arrangements with System/full, Reduced and Off. It
pauses native WAAPI effects at 0/25/50/75/100% for frame captures under
`/tmp/m2-screenshots/` (`SCREENSHOTS` overrides), asserts scale remains 1 and
root opacity remains 1, and records LayoutShift sources in a separate unpaused
normal-speed maximize/restore run. `ROOT` selects another production build;
`BEFORE=1` records baseline frames without enforcing the fixed invariants.
Notice rise/update/dismiss/Undo, catch-up pending-to-summary growth, and the
mounted comments list are not browser-covered here; notice source guards and
the shared list helper's VM/real-DOM tests are narrower checks, not integration coverage.

Screenshots (including `-actions-open`, `-menu-open`, `-sheet-open`, `-status-working` and open
tools) and failure JSON containing effects,
requests, errors and DOM are saved to `/tmp/m1-screenshots`, overridden with
`SCREENSHOTS`. These are diagnostic artifacts, not pixel baselines.
For a before/after bundle comparison, build a clean `git archive` of the base
commit outside the shared worktree with the same toolchain/flags, then run
`ROOT=/path/to/base/crates/c6-ui/dist node crates/c6-ui/tests/motion.cjs --sizes`
and the same command with the current `ROOT`. This emits a **directory
inventory** of per-file and per-extension HTML/JS/CSS/WASM byte totals, raw and
reproducible gzip level 9. Both before/after dist directories must be clean:
Trunk can leave orphan snippet files that inflate an inventory. This command
does not trace reachable imports or measure network transfer; gzip totals are
compression estimates, not measured head transfer sizes.

A test is a claim: *this invariant holds*. It must fail **if and only if** the invariant is broken. A slow machine, a busy CI runner, another test running beside it, or the order tests run in must never fail it. The test suite runs on GitHub's runners (`ubuntu-24.04`, `macos-15`), which are shared, small and sometimes slow, and it must hold there.

## Failure diagnostics

`make test` enables panic backtraces (`RUST_BACKTRACE=1` unless already set, for both nextest and its `cargo test` fallback). Use `RUST_BACKTRACE=full make test` for every stack frame.

`.config/nextest.toml` shows each failure's complete captured stdout and stderr both immediately and at the end of the run (`failure-output = "immediate-final"`, `final-status-level = "fail"`). The final status list alone does not include that output: nextest's default only prints it immediately, where a long parallel run can bury it. Keep capture enabled so concurrent tests' diagnostics stay together.

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

## App-owned icons: native UI regression

`cargo test -p c6-ui` renders representative session notices, memory events,
published outputs, file attachments, approvals, questions and secret cards, plus
task/subagent states, document chips and mentions, and explorer diagram labels. Their
fixture content is plain text, so emoji or symbol-icons in the resulting markup
are a regression in the app, not a restriction on user content. A source guard
also scans app-owned Rust, HTML and JS (excluding test modules and comments).
Use drawn SVG icons instead of emoji; don't drop icons or replace them with
words alone. Rendered-markup regressions assert that restored icons produce
`<svg>` elements, not just that glyphs are absent. Keep trailing action arrows
as drawn SVGs too, in their original position beside the original label.
Intentional typography and keyboard/prose directions are allowed. Slack's emoji table and vendored terminal
Unicode data remain content, not app icons.

After building with `env -u NO_COLOR make ui`, check arbitrary Tailwind utilities
used by changed markup against the generated CSS, not merely the source class
strings. The shared SVGs use ordinary size/alignment utilities.

## Icons and components: browser check

Native tests in `icons.rs` keep the chrome-glyph and no-browser-SSR guards,
check every vendored icon at 14/16/20px (one outer SVG, 1.5px absolute stroke),
and compare the markup helpers with the same path source. `components.rs`
checks accessible names and trailing switches, bans local switch markup/SELECT
constants, and requires the shared class on native checkboxes. The ad hoc
button-class guard waits for the deferred button migration; existing buttons
are not silently exempted by a growing allow-list.

After `make ui`, open the signed-in `/dev/components` specimen. Repeat in light
and dark, at desktop and phone widths, with fine and coarse pointer media:

1. Icons are visible, inherit their colour and keep their weight at all three
   sizes. There are no nested SVGs; filled stars and Stop stay filled.
2. Check primary/secondary/quiet/danger buttons, small/default sizes, disabled
   and keyboard-focus states. On coarse pointers buttons are at least 40px;
   inputs/selects are at least 40px with 16px text. The select has one chevron.
3. Toggle the specimen's switch by label, track, Space and Enter. The disabled
   one never changes. Inspect Settings → Appearance, Suggestions and Work:
   labels/descriptions remain left, tracks align right, saving/error/disabled
   behaviour is unchanged. Form choices that need Save remain checkboxes.
4. In a session's ⋯ menu, toggle Catch-up (and Fold finished turns when the
   timeline feature is shown). The menu stays open; switch rows have the same
   shape as Settings and at least 44px touch targets. Find's key hint is a keycap.
5. Open Settings → Realms → Explore. Kind marks retain their original colours,
   with the approved Lucide shapes. Follow an in-app link and inspect a new-tab
   link: their trailing 14px arrows differ. Notifications has only its bell,
   with on/filtered/off state in its title and accessible name.
6. Watch the browser console and crash reporter through all of these: no
   browser `.to_html()` calls, disposed-signal panics or horizontal overflow.
   Check newly added arbitrary Tailwind selectors in the generated CSS too.

## Doc comments: placement browser check

`cargo test -p c6-ui` checks the pure popover placement with offset tile bounds,
above/below anchors, oversized threads and 320px/390px/desktop widths. In a
browser, repeat at those widths (with touch on phones):

1. Open a long doc, also with its comments panel beside it on a wide desktop
   tile. Start a comment on a block near the bottom of the visible tile. The
   popover flips above; the text box, Cancel and Comment remain reachable.
2. Type a long draft, scroll the doc, resize the tile/window and open an existing
   long thread. The box stays inside the visible tile, scrolling its contents
   when necessary, without losing the draft or focus. Cancel, posting, outside
   press and Escape still work.
3. On phones, open the keyboard in a block's sheet and in the comments panel
   (also on a deck). The sheet and its scrolling contents remain above the
   keyboard, including when only `visualViewport` shrinks or pans. Close the
   keyboard and check that the sheet returns to the screen bottom.
4. Check the floating comments panel in a short desktop tile too. Chip editors
   and comment ellipsis actions remain in normal flow, not clipped floating
   menus; ref chips still open their destination.

## Session preview marks: manual browser check

On desktop, hover a sidebar session or activate its drawn **Session details**
button with Tab and Enter/Space. On a touch phone, open **Sessions** and tap
the row's **Session details** button (the list uses the same preview card).
The card initially has one swatch, not the palette. Open it and pick a plain
colour and a pattern; the card must stay open and the saved mark must update.
Repeat with a session whose own colour is empty: its id-derived swatch must
still show. Move the pointer away with keyboard focus inside the card; it must
remain usable. Escape and the close button dismiss it. Open near the bottom
and right viewport edges, also at 320px and 390px phone widths: expanding the
palette must not push the card or its controls off screen, or require horizontal
scrolling. Tall cards scroll within the viewport.

## Archived session tiles: manual browser check

On desktop and a touch phone, type a draft in a session tile, then archive the
session from another client or its own actions menu. The tile must remain open,
with its real title/status/mark and a visible **Archived** marker and drawn
**Unarchive** action. The composer must explain why it cannot send, retain the
draft and disable sending. Reload, then open the archived session by its direct
URL in a fresh workspace: both must resolve the same metadata without needing
the non-archived list. Activate Unarchive by keyboard and touch: it must not
send the draft; sending afterward works normally. Check an Unarchive failure
is visible and leaves the session archived. New sends from a stale client are
also rejected clearly by the head, without queueing input.

## Initial tile loading: manual browser check

On a restored workspace, only the focused tile starts its pane loads first.
Other visible tiles start after the focused panes have drawn (for a session,
after its detail response and either the feed's initial `Synced` or a stream's
reconnecting notice, then a paint).
Non-session panes use their mounted shell as readiness, not completion of all
their background requests. Minimized tiles, tiles hidden by focus mode, and
phone tabs/panes not currently shown wait for their first reveal. Once a pane
has mounted, hiding it keeps its DOM, stream, draft and scroll state alive.
Restored headers share this initial budget and join a pending pane detail read.
After the focused paint, headers missing from the live list (including archived
or minimized sessions) may resolve by id without mounting their hidden panes;
an old cached answer must not suppress a fresh omission check or failed-read retry.

A failed detail response releases loading after drawing the error. A stream
that closes or fails to open before `Synced` draws a reconnecting notice and
also releases loading, while continuing its normal retries. A successful
replay clears the notice. There is no timer that declares an undrawn transcript
ready: a connection that remains open but never sends `Synced` still waits
until it syncs, disconnects, or the user changes focus/closes its tile.

The workspace native regressions cover mount eligibility, sticky mounts,
split-pane readiness and changes of focus while loading. In a browser, use
request gates rather than sleeps:

1. Restore several session tiles with a non-first tile focused. Hold that
   session's detail response and feed replay independently. Neither releasing
   detail alone nor replay alone may start another session tile's requests.
   Release both: confirm the focused transcript is in the DOM before another
   tile's first request (observe animation frames and the request log).
2. Restore a split focused tile with a list beside a session. Drawing the list
   must not release other tiles before the session's replay. Repeat with an
   empty session and a failed detail response (the error also releases loading).
3. Minimize a never-loaded tile or restore in focus mode: it makes no pane
   requests until revealed. On a phone, repeat for inactive tabs and a hidden
   list pane; resize to desktop and check newly visible panes load.
4. Hide an already-loaded session, stream a reply, and show it again. Keep DOM
   handles for its transcript and draft textarea: both remain connected and
   retain their contents/scroll choices. Switching away while the initial feed
   is gated, or closing its tile, must let the newly focused tile load and draw.
5. Run the first-click checks below, including a nonfocused split-pane close.
   Keep all icons; this scheduling must not recreate header controls on focus.
6. Resolve the focused session's detail, but close its stream before `Synced`.
   Confirm its reconnecting notice is drawn before other visible tiles start
   loading. Observe a new stream connection without switching focus; finish
   that retry's replay and check the notice clears and its transcript appears.
   Repeat with detail still gated when the stream closes: other tiles must
   wait for detail too, rather than treating failure as an unconditional bypass.

## Tile focus and first click: manual browser check

Focus and active-pane selection run on pointerdown. The pane header and its conditional buttons must stay mounted when only focus/active changes: detaching the original target between pointerdown and click loses its click. Use cached boolean conditions (`Show`) for their presence, not a closure that rebuilds them on every position change. Growing a tile still waits until click bubbles, after the action. The native `c6-ui` tests don't exercise browser event dispatch, so check this in a browser:

1. Open two session tiles side by side, with the left focused. Click **×** in the right tile's bar: it closes on the first click. Reopen it and refocus the left tile before each check below.
2. In the unfocused right tile, click **⋯**, **Changes**, **Terminal**, **Minimize**, and **Focus** (maximize). Each acts immediately; actions that leave the tile open focus it too. Bring it back or defocus as needed. Check any view tabs in its content the same way.
3. Open a second pane inside the right tile (a list and item, or a session and Changes). With the left tile focused, click **×** in either of the right tile's *pane* headers. It closes that pane on the first click. Also try its pin button and clicking a control in the inactive pane of an already-focused tile.
4. Click a transcript link in an unfocused tile: its destination opens on the first click. Type a draft in that tile, refocus the other tile, and click **Send**: it sends once and clears the draft. Try keyboard activation (Tab, then Enter/Space) and a touch tap as well.
5. Repeat with enough tiles for three to share a column (nine tiles): the clicked tile may grow only after its action, never move the target between press and release. Clicking plain transcript content should focus its tile, and shortcuts should then act there. A press canceled by dragging or scrolling should not activate a control.
6. Run the transcript-follow checks below too: focusing by clicking content must not interfere with wheel/touch/keyboard scroll intent or the transcript's tabindex.

For a browser-automation regression, keep the target element's DOM handle, send pointerdown, allow an animation frame, then send pointerup. Assert that the target stays connected through pointerdown and the action happens after one click. In particular, a split tile's pane-header close reproduces the old replacement reliably; an atomic synthetic `click()` can hide the interval where rendering occurs.

## Tile drops: layout regressions and browser check

The `workspace` unit tests exercise the same `check_drop` rules used by the
preview and the move, including all source/target/direction combinations for
2–16 tiles, minimized tiles, missing endpoints, and unchanged positions. There
is a four-column limit, not a per-column tile limit. A rejected drop must leave
the entire layout unchanged; every blue preview must change the visible layout.

In a desktop browser (also check touch-capable desktop/tablet native dragging):

1. Open sixteen tiles (four columns, four tiles each). Drag a tile from a shared
   column over another tile's left and right fifths. Neither edge turns blue
   and no text covers the tile's bar. Releasing leaves the layout unchanged.
2. Drop the same tile in the middle of another tile's top or bottom half. It
   joins that column at the highlighted position, even with many tiles there.
   Repeat in the padding between stacked tiles; the gap accepts the drop too.
3. Move over a valid destination, then back over the source or out of the grid.
   No old highlight remains. Moving between child elements inside a destination
   must keep the preview. Dropping onto the source, immediately before/after its
   current position, or beside its singleton column shows no highlight and does nothing.
4. With fewer than four occupied columns, move a tile into a new column. With
   four columns, a tile alone in its column can still move that column to a
   different place. Minimize/close a different tile during a drag: the drop uses
   stable tile keys, not stale indices. A source/destination hidden or closed
   during the drag must no longer advertise an accepted drop; hiding/closing
   the source cancels its drag state even if its bar never receives dragend.
   Also minimize every tile in the leftmost column, then drag a tile from a
   shared column beside another: it makes the promised fourth visible column,
   rather than merging with the rightmost one. A minimized-only slot is free
   for reuse, not a fifth saved column that squeezes two visible ones together.
5. Start a drag on a tile-bar button as well as its title; both native drags
   must follow the same rules. Dropping a tile over a message box still moves
   the tile; dropping a file still attaches it, not moves a tile. End/cancel a
   drag and check that all feedback disappears.
6. On a phone there is no reorder footer. Open **Sessions**, check one flat
   reading-order list including hidden tiles, switch and close each kind of
   page, and check its current highlight and positive count. Desktop dragging
   above remains the reordering route.

## Phone session header and desktop ring: browser check

Use Chromium and WebKit, light and dark. Test 320px and 390px portraits and a
short landscape viewport, with the keyboard closed, a resized viewport and an
overlay/offset visual viewport. Check one 44px top bar and no footer. Open
the app menu (realm, search, Activity, notifications, connection/version),
Sessions (Ask, bell, close/switch/new), Actions (Find/details/history), and the
plain context ring (the only phone route to Changes/Terminal/Files/Outputs).
Sheets and dialogs must stay within the visible viewport and scroll.

Drive idle, main-working, waiting, last-turn failure and background-only
states. No idle row or Background 0; working must not follow aggregate child
status. Deduplicate linked task/subagent records and exclude foreground native
work and mirrors. Show must expose cards, errors, Inspect/Stop and history.
Hold a draft and focused composer element across these transitions: it must
stay mounted and at the same bottom position; the transcript loses/gains 44px.
Following remains at the bottom, while a scrolled-up reader stays put.

At desktop height, exercise actual 240/320/400/480/640px tiles, not just the
bar width. The transcript and bar must share the same actual tile width.
Fine-pointer controls use 28px slots; coarse pointers keep 44px targets.
Verify narrow bars move secondary controls into Actions with their icons,
truncate the title, and never require a horizontally scrolling workspace.
Also test a list and item in a split tile. Inspect all four rim states,
neutral context arc, dashed waiting, failed cross, positive-only badge, worded
accessible name/tooltip, and stripe-default/Dot-only Appearance preference.
Keep screenshots for both themes; validate new arbitrary utilities in built
CSS as well as geometry. Emulated keyboard tests do not replace a physical
phone keyboard/accessibility review.

### Opt-in tile aspect marks

After `env -u NO_COLOR make ui`, run the loopback production regression:

```sh
ENGINE=both SCREENSHOTS=/tmp/z2-screenshots \
  /home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/tile-marks.cjs
```

Like `motion.cjs`, it uses the local Playwright/browser paths (or their environment
overrides), an owned ephemeral port, fake HTTP/WebSockets and fresh contexts.
Check both themes with several wide tiles: old-head/default off,
opt-in on, the selected shapes, 400ms resting spotlight and cancellation,
clicking a card and its palette, setting save/reload, and live off/on changes
without replacing controls or composers. Fixed controls keep their width-dependent
28/32/36px fine-pointer or 44px coarse-pointer slots and coordinates. Marks sit
8px after the intrinsic title with 8px gaps and clearance from controls; long
titles shrink rather than pushing marks against controls. `MARKS_BEFORE=1`
checks the old right-aligned geometry against a pre-fix dist. Resize actual
tile content through 720/640/639/480/479/320/240px: marks
yield below 640px, before secondary controls move into Actions below 480px;
the ring, Actions, Focus and Close stay ordered and reachable. Widening restores
the same nodes and geometry. Resize the viewport to 320/390px and short
landscape: no strips, pending dimming or popovers, even with the setting on.
Keep off/on/card/phone screenshots and reject console/crash-reporter errors.

## Composer latency and draft persistence

Textareas managed by `autosize` use CSS `field-sizing: content` when available.
The fallback coalesces text, width and visibility changes into one pending
animation-frame fit, canceled on unmount. Composer input handlers never fit
synchronously. Native tests cover completion parsing and text-save coalescing,
including forget-before-teardown; browser checks exercise actual layout/storage:

1. In Chromium (native sizing) and WebKit (fallback when unsupported), type,
   paste, clear and restore a multiline draft, resize/hide/reveal the tile and
   show a long suggested placeholder. It grows up to its cap then scrolls,
   shrinks when cleared, and remains usable at 320px, 390px and desktop widths.
   Instrument textarea geometry reads: none on the native path, none during
   input dispatch on the fallback; multiple edits in one frame yield one fit.
2. Instrument fetch signals and hold mention responses behind an explicit
   gate. Type `@ab`, wait for its request, then type `@abc`: the first request
   must abort, not merely have its result ignored. File, memex and Ask reference
   requests share the 120ms debounce and cancellation. Release the old response:
   it must never replace the newer suggestions. Moving/clicking the caret
   without changing a query must not issue requests or reset selection. Escape,
   completion, removing `@`, and closing the box cancel pending/in-flight work.
3. Observe localStorage writes while dispatching a burst of edits in one task:
   there is no synchronous write per edit. The latest text is saved after the
   250ms debounce; navigate away before it fires and it is flushed on teardown.
   `pagehide` and becoming hidden flush too. Reload immediately after typing
   (in the same JS task, within the debounce window), including with restored
   attachments; the last edit and attachments must return. Send/start a session during that window and return:
   the forgotten draft must stay gone, not be resurrected by a late save.
4. Restore a draft with ordered image/file attachments from IndexedDB. Edit text
   and immediately navigate away; reopen and check text, attachment metadata and
   ordering. Send and reopen again: neither returns. The attachment transaction
   queue and restore-ready gate remain unchanged; typing doesn't rewrite payloads.
   Repeat a failed send (draft stays), storage denial (quiet note), and pasting
   an attachment before the database opens (restored and new files both stay).

## Transcript follow: manual browser check

The native `child_lists_invalidate_on_spawn_not_tool_results_or_task_reports`
checks the shared membership memo's downstream invalidation. In a browser,
count `/children` and `/children?tree=1` requests while ordinary tool calls finish:
they must not refetch for each result. Spawn a subagent: both lists refresh.
Existing live working-count, attention/reconnect and explicit side-chat creation
refreshes remain intact; verify a child finishing and a newly created side chat.
Usage statistics are shared per feed between the header/details/transcript, not
incrementally summarized. The shown event window already borrows with `.with`
and must not be changed to `.get` (which would clone the whole retained log).

The `c6-ui` workspace unit tests cover input direction and stop/resume decisions. In a browser, use a long session with a streaming reply:

1. Open it normally: it starts at the bottom and follows new text. While there, roll the wheel up one notch (also try a tiny trackpad movement). It must stay up as tokens arrive or the window resizes, even if less than 120px from the end. Repeat during a smooth glide.
2. Focus the transcript and try Up, Page Up, Home and Shift+Space. Each stops following immediately. Down/Page Down/End and scrolling down resume only at the very bottom (less than 4px); being merely near the bottom does not resume. Check the scrollbar too.
3. Scroll more than a screen away, click **Latest** with its drawn down-arrow (or its new-event count), and confirm that streaming follows again. Fold/show the tile while following, and while just a few pixels above the bottom with following off; each keeps its prior choice and position.
4. Open a search hit near the end. Its anchor must stay put through streaming and resize, not start following because it is near the end. Click Latest to resume.
5. On a phone, tap without dragging: following stays on. Drag toward older content by a small distance: it stops immediately and stays off through touch inertia. Drag back to the bottom or use Latest: it resumes. Try reversing direction within one gesture.
6. Check that horizontal scrolling and typing/navigation in text fields do not change transcript following. With reduced motion enabled, repeat the small upward scroll and Latest checks.

### Tool-card arrival regression

The native `session::tests::tool_output_keeps_one_bounded_area_before_and_after_the_result` renders the production output area before output, during streaming, and after success or an error (including truncation and images). Its height and single output box must be the same in every state. Native tests deliberately render this small component, not the recursive session/subagent transcript.

In a browser, repeat at 320px, 390px and desktop widths:

1. Start a shell call that emits several lines before finishing. Its summary appears immediately; it stays collapsed throughout. Open it while running: the output scrolls within a fixed-height area, which remains the same height when the result replaces the live text. An error must behave the same way. Plans start open and retain that choice.
2. Before first opening, the collapsed card has no output `pre` or detail body. Open it and retain DOM handles for the card, its `details`, and its output `pre`. Deliver output chunks, its result, and another assistant event: all three handles must remain connected and refer to the same nodes. A user-opened card stays open; a user-closed card stays closed. Closing unmounts the body; reopening shows the latest result immediately in the same bounded area, not a growing fill-in box.
3. Create a session and a document with long titles. Their pending link areas must already occupy the completed card's height, with the input title when known. The result activates the destination without adding a row or horizontal overflow. Keep all status, document/deck and navigation icons.
4. With a long transcript following the bottom, repeat through completion: it stays at the bottom without a transient expansion/collapse. Scroll up deliberately before a result arrives: the viewport stays at that position. Repeat on a touch viewport and with reduced motion.

### Off-screen transcript rows and lazy bodies

`collapsed_tool_body_does_not_read_or_render_output` checks that closed bodies
neither render nor read their output; the bounded-output regression above still
covers pending, live, completed and error states. Thinking starts expanded when
enabled, but collapsing it removes its Markdown body until reopened.

In Chromium and WebKit, use a transcript spanning several screens (include tool
results and thinking). Inspect `.c6-transcript-row`: browsers supporting
`content-visibility: auto` skip off-screen layout and remember measured sizes.
Scroll slowly and fling in both directions; Latest must still glide/follow to
the real end as rows materialize. Open Find and jump to a hit in an off-screen
tool body, a folded turn, and an event outside the loaded window. Each unfolds,
mounts the body before highlighting/measuring, and centers the actual hit.
Repeat while streaming, with following off and reduced motion enabled. No
result-arrival fill-in jump, missing icons, or empty first jump is acceptable.

### Observer lifetime regression

The transcript-follow and glance-width ResizeObservers retain their Rust
callbacks alongside the observer, disconnect before dropping either on replacement
or unmount, and never `forget()` the closure. The message box uses the same
ownership pattern. The transcript's on-sight IntersectionObserver retains its
callback until cleanup too (as the glance outline already does).

Transcript-follow resize delivery must not measure or scroll synchronously:
deliver its callback twice in one task and assert that only one animation frame
is queued. Scroll away before that frame runs: it must recheck following and
leave the reading position alone. Queue another frame and close the tile in the
same task: cleanup must disconnect the observer and cancel the queued frame, with
no late reads or dropped-closure errors. With normal-motion WebKit and many tall
contained rows, stream and resize at phone and desktop widths; no ResizeObserver
loop warning is acceptable (also repeat with reduced motion and in Chromium).

In browser automation, wrap the native observers before loading the app and
record each callback and its observed elements. Open and close session tiles
repeatedly. Every observer attached to a removed textarea, transcript, glance or
on-sight marker must disconnect. Retaining a JS reference to its callback and
invoking it after unmount should throw the wasm-bindgen dropped-closure error,
not call a leaked Rust closure. While mounted, resize the tile, append a streamed
reply and scroll: width measurement, transcript following and visibility still
work, without callbacks or reactive reads after disposal.

## Shared page data: connection and session panes

`cargo test -p c6-ui resources::tests` checks that connection consumers use
one page-owned resource, sessions share their detail request, closing a tile
cannot dispose another tile's data, and a new page does not inherit another
page's cache. These tests inspect resource identities, not rendered HTML.

In a browser, filter Network to `connections` and open several session tiles,
their model pickers, New session, Usage and Settings. There should be one
connections-list GET per signed-in page, not one per picker or tile. Rename,
add or remove a connection, sign in with ChatGPT, and change a connection's
realm availability: after the successful save the shared list refreshes and
every picker reflects the confirmed result. Failed saves must not change it.

Open a transcript's Changes, Files and Terminal. They share its session-detail
request and use the actual folder from Started (including a worktree), not
the requested folder. With the transcript already loaded they need no first-
event fetch. Opening a pane directly still loads detail and the first event
in parallel; starting sessions retain the unavailable note until Started.
SSH sessions still refuse local Changes/Files and open remote terminals.
Close the transcript while keeping a pane open, then reopen it: the pane
must keep working and the transcript must refresh its detail.

### Shared model lists

`cargo test -p c6-head --lib models::tests` checks one cold request per
connection, ten-minute freshness, stale-while-refresh, refresh failures,
bounded eviction, and invalidation racing a provider reply or an old
connection-row read. Time is supplied to cache lookups; tests do not wait
ten minutes. The loopback HTTP test checks that connection edits and deletion
invalidate lists, while **Test connection** (including the saved connection's
`GET /models?fresh=true` and pool members) always contacts the provider and
does not populate the cache. Pools share their members' cached lists and
compute the union directly, avoiding a second ten-minute freshness window.
Editing any connection clears cached member lists too.
The head keeps at most 128 entries; failed refreshes retain the last successful
list and back off for 30 seconds. It never caches a cold failure.

`resources::tests` also checks the UI's shared pending model resources,
refresh coalescing and 128-entry bound. An evicted or invalidated entry cannot
dispose a list still used by an open picker. A confirmed connections reload
clears the page cache; late replies cannot reinsert old entries.

In Network, open several message boxes before opening any model dropdown:
their mount should warm each connection's list only once per page. Open the
all-connections picker: requests run in parallel, grouped in the same order
as before, with per-connection errors and free-text model entry unchanged.
Open a second picker or close/reopen a box: it shares both pending and ready
lists. Save a connection edit and repeat; lists must refresh only after the
save succeeds. Test a saved connection while its list is cached: the provider
must still be contacted. Repeat with an unavailable provider after the head's
ten-minute freshness window: the previous successful list remains usable
while one refresh runs. All pickers retain native controls and drawn icons.

## Task card actions: manual phone check

Check both a session transcript and Decisions, with a coarse pointer at 320px and 390px widths, and on desktop. Use a realm with several open work items and a closed one.

1. The actions read **Start session**, **Do it here**, **Save for later**, **Not now** in that order. Only Start session is filled; the next two have identical visible outlines and heights. They share one wrapping row, with no split arrow or stray divider. Coarse-pointer targets are at least 44px tall; desktop buttons remain compact.
2. Save for later's tooltip names the proposing session's work. Clicking it sends `Add to this work`, without a picker. With no work, it creates new work. Do it here's tooltip names the proposing session.
3. **Save elsewhere…** opens over the workspace, not inside the card. Scroll the transcript so the card is directly above the message box, then open it again: the whole dialog remains visible, with a scrolling list rather than content clipped by the composer. Try light/dark mode and the phone keyboard.
4. The chooser offers **New work** once and open work only in the card's realm. Search is case-insensitive. A nonmatching search shows an empty-state message but keeps New work available. The New work choice sends `Add as new work`; an existing work item sends `Add to work:<id>` and receives the card's title, prompt and origin as a to-do.
5. Close, outside click and Escape dismiss the chooser; Escape must not stop the session. Focus returns to Save elsewhere. A failed save leaves the card open with an error; all action buttons disable while a save is pending. Check the chooser with a long list and a long work title.
6. With only task cards open, the transcript shows **Suggested tasks (N)** without an empty **Waiting for you** heading. Add a question or approval: Waiting for you returns. Answer the last non-task decision: its heading disappears while Suggested tasks stays.

## Workspace destination reuse and explicit copies

Ordinary opens (`Beside`, `OwnTile`, `Instead`) first look for a live matching
destination in either pane, including minimized tiles. They focus that exact
pane, restore its tile, and reveal it even if another tile fills the workspace.
If explicit live copies exist, the origin tile wins, then the focused tile,
then the first matching tile in saved order. `NewTile` (⌘/Ctrl-click) always
makes another live copy, including sessions and documents. Ordinary “New tile”
menu actions use `OwnTile`, so they focus an existing destination. Opening a
destination found only in history does not steal or rewrite that history entry.

Saved-layout restore now collapses current document copies (not session copies
or Docs indexes), retaining the focused copy/active pane or the first in saved
order, with its exact block target and key. Only duplicate document panes are
removed; their companion panes and history stacks stay. A duplicate-only tile
without history disappears. One with history traverses Back, then Forward, to
its nearest useful page, preserving skipped snapshots and the opposite stack.
If all its history is already-open docs, a fresh Home pane retains both stacks
and the outgoing page in Forward. This may retain an extra navigation tile:
deleting it or grafting its independent history onto the winner would lose or
change navigation. Restore is idempotent and makes no rendering-to-HTML calls.

Back/Forward collisions also reveal/navigate the existing document (including
minimized tiles) rather than recreating a copy. The source panes and both
stacks remain untouched, so even a mixed historical snapshot remains available
instead of being destructively rewritten. Its other panes are not restored on
that collision click. Old same-tile copies in a history snapshot collapse only
when that snapshot becomes current; stored history is not pruned.

Fixed pages (Inbox, Decisions, Sessions, Docs list, Work list, Projects list,
Ask, workers list, Brief, Usage, Activity, Memex, Mail, Chat, Calendar, Contacts,
Photos, Analytics, Mission, Realms) reuse one matching live view. Documents,
sessions, workers, work, projects and collections match by their identifier;
lists remain separate destinations from their items. Settings is one page:
opening a different section navigates that same pane. Session Outputs likewise
navigates output selection in its existing pane. Searches match the complete
query/kind/worker, and explorer/app routes match their section/object/record,
so different investigations can stay side by side. Selected-parent detail
opens (`open_after`, including Glance's worker and started sessions) also reuse
existing destinations; new details keep the selected parent rather than its
list. Session companion actions (Changes/Files/Terminal/Outputs) retain their
deliberate same-tile placement contract rather than searching globally for a
companion elsewhere in `open_after`.

Session turn targets and document block targets reuse the destination and
record target changes in the destination tile's Back/Forward history, keeping
pane keys. Untargeted links only focus: they do not reset the reading target,
add history, or discard Forward. Documents accept `/docs/<id>#<block>` and
`/docs/<id>?block=<block>` (a nonempty hash wins); the address bar uses the hash.
The same targets work on `doc:<id>` Markdown links. In-app links retain their
query and hash, including inside document blocks. A document block jump uses
the existing comment-reveal callback to select its tab/slide and flash/scroll
to it without remounting DocPage or losing its block/comment drafts. Clicking
the same target again explicitly revisits it; focus alone does not.

Run `cargo test -p c6-ui workspace::tests` for the real pure `Layout::open`
path (every view and opening mode, either pane, minimize/maximize, duplicates,
targets, history and legacy saved Docs). `cargo test -p c6-ui` also covers
Markdown target preservation and document tab/slide reveal selection.

After building the final combined UI **once**, run:

```sh
ROOT=/path/to/dist ENGINE=chromium,webkit node crates/c6-ui/tests/workspace-open.cjs
```

The standalone harness reuses motion.cjs's private loopback server, Playwright
launcher, session/live fixtures and bounded condition waits. It checks desktop
and 320×568: actual doc links/mentions, links inside a document, query-targeted
tab selection, incoming session-created documents, pane key/DOM/editor identity,
target Back/Forward, minimized docs under a maximized session, explicit doc and
session copies, focused/first document-copy collapse on restore, retained
companion panes and history-only navigation shells, and Back routing without
recreating copies. Set
`VIEWPORT=desktop` or `VIEWPORT=narrow` to focus one case. The draft-retention
case dispatches a real link click without pointer blur, since normal editor
blur intentionally saves/closes it; it does not call workspace internals or
replace production navigation helpers.

`ONLY=autoDocs` isolates two real mounted session fixtures and one shared doc.
Session-authored `created` events use `OwnTile`, including repeated notifications
from either writer. Real `changed` events (head edits emit this value) refresh
the existing document, retaining DOM/editor drafts without opening or focusing
it, changing pane/tile keys, or adding/clearing history. Repeated edits by both
sessions are checked while focus stays on the chosen session. This deliberately
preserves the existing creation-only auto-open policy: streaming block edits
must not steal focus. Comments/deletes/other authors do not auto-open either.

Opener audit: `Go` (`own`/outside-column included), Markdown/mentions, doc chips,
Search, and attention all reach the shared destination policy. “Used in” opens
the referring Work/Project through `Go`, not another doc. The sessions hover
“New tile”, new-session dialog, and work resume now use `OwnTile`; genuine
modifier clicks and session companion modifiers retain `NewTile`. The workspace
blank-tile command intentionally creates a fresh Home. Workspace drag/drop only
moves existing tiles; there is no document `text/uri-list` drop opener to patch.

`ONLY=sessionTarget` isolates session query/turn targets, target Back/Forward,
and an identical repeated target (also with the tile Actions menu open) before
any Docs are mounted. These operations
must be panic-free: session targets need a keyed content replacement with a
fresh owner and DOM, not an unkeyed rebuild against disposed reactive attributes.
Doc targets deliberately keep the same content key to retain editors. The full
test prints each phase and checks browser errors both before and after the
session/doc boundary; its live fixture accepts the live socket's query string
and waits for readiness rather than depending on connection timing.

## Global shortcuts: platform modifier and terminal focus

`cargo test -p c6-ui util::tests::app_shortcuts` checks browser platform
classification and every Meta/Ctrl/Alt/Shift combination without rendering
app views. Workspace 1–9 and [ / ], sidebar \, Search K and Ask J use Meta
only on macOS (`navigator.platform` starts with `Mac`), Ctrl only elsewhere.
The opposite modifier, both together, Alt/Shift variants, composing events
and events already prevented by a child must not trigger an app command.

Run `ENGINE=both node crates/c6-ui/tests/keyboard-shortcuts.cjs` against the
coordinator's final dist (`ROOT`, `PLAYWRIGHT_CORE`, `CHROMIUM` and
`WEBKIT_EXECUTABLE` work as in `motion.cjs`; this script does not build the UI).
It overrides `navigator.platform` before app startup to `MacIntel`, then
`Linux x86_64` and `Win32`. With multiple tiles, send cancelable bubbling
keydown events using each platform's modifier: 1–9 focus the numbered tile,
[ / ] go back/forward in its history, \ toggles the sidebar, K focuses Search
and J opens Ask. Verify the other modifier and the variants above leave
the workspace and focus unchanged and are not prevented by these handlers.
Repeat with focus on `.xterm-helper-textarea` inside `.xterm`: none of those
commands may move tiles, toggle the sidebar, steal focus or prevent the event;
terminal Ctrl+J, Ctrl+K, Ctrl+[ and Ctrl+\ must remain terminal input. Test both
an event targeted at the textarea and one dispatched on `window` while the
terminal stays focused, especially Ctrl+[ on macOS. The fixture uses a real
focusable terminal-shaped DOM subtree without xterm's own key handlers, so the
events actually bubble to the production window listeners. After leaving the
terminal, shortcuts must work again. Normal Search and message inputs still
allow these explicit app commands. Retained but hidden Search inputs must not
claim K before the visible header input; the fixture checks actual visible focus.

### Phone app drawer: left swipe from inside (fix6)

`node crates/c6-ui/tests/motion.cjs --unit` exercises the production
`c6DrawerSwipe` helper without a UI build: touch-only direction locking,
tap slop and click preservation/suppression, short reversal after crossing
the threshold, long release, cancellation, motion/viewport changes and
listener cleanup. No app views are rendered to HTML.

After the coordinator's **combined** `make ui` (do not build each worktree),
run against that production dist:

```sh
ROOT=/path/to/coordinator/crates/c6-ui/dist ENGINE=chromium \
  ONLY=drawer THEME=light VIEWPORT=phone,narrow,landscape \
  MOTION_CASE=full,full-reduce,reduced,off \
  node crates/c6-ui/tests/motion.cjs
```

The existing fake-head fixture uses Chromium CDP **emulated touch**, not a
physical Android device. It checks actual native touch/pointer cancellation
and compatibility clicks: a normal nested-menu tap; a drag starting well
inside the drawer (no Android Back-edge dependency); finger tracking; a long
pull reversed to a short release with snap-back; long release with inert
retained exit; and real nested vertical scrolling without dismissal. Outside
tap and Escape still close. It checks the shared 280/200ms Full tokens,
80/80ms Reduced tokens, 0/0ms Off tokens and zero travel outside Full, including
explicit Full overriding an OS reduced-motion preference. Gesture recognition
is identical in each setting; only Full translates during the finger drag.
Physical Android Back-edge interaction remains a manual follow-up.

The drawer opener still uses **one** native Playwright tap, then waits for
the retained panel's `data-open="true"`, settles animations and asserts it
remains open. No sleeps, retries or JS clicks: a missed tap must time out and
an unintended subsequent close must fail. The nested scroll test registers
`scrollend` before its gesture and awaits native completion before removing
the fixture. The long-close gesture ends with slowing native touch moves,
testing a deliberate distance-based pull/release rather than a 115px
one-step flick. Fast-release diagnostic traces showed native opener down/up
with no subsequent click; native scroll completion and a one-frame trusted
press did not cure it. Those failures must remain reported separately, not
be described as an app-side dismissal or as solved by the state wait.

## Composer model picker typography across portals

Run `cargo test -p c6-ui composer_model_portals_keep_their_own_typography` for
the native source guard (no app-view rendering). The desktop model/subagent
popover must carry `text-xs` and its light/dark text colors across the portal;
the phone BottomSheet keeps its own `text-sm`. Commit `8c7a4307` moved only
this desktop popover: its text colors were already explicit, and the phone
portal/sheet was unchanged. No other newly portaled popovers need correction.
Against the coordinator's final combined production build, run
`ROOT=/path/to/dist ENGINE=both node crates/c6-ui/tests/model-typography.cjs`.
This standalone helper reuses `motion.cjs` without changing it or building the
UI. It asserts computed 12px desktop headings/helper copy, 14px phone sheet
copy, 16px phone input fields (the anti-iOS-zoom override), 12px offered-model
rows, retained colors/icons, model/effort selection, and keyboard-bounded phone
geometry at 390×844 and 320×568 in light/dark Chromium and WebKit. PNGs and
computed-style JSON go to `/tmp/q1-screenshots/model-after` (`SCREENSHOTS`
overrides it). Capture the regressed snapshot with
`BEFORE=1 ROOT=/tmp/q1-four-fixes-dist ENGINE=both node crates/c6-ui/tests/model-typography.cjs`;
that phase asserts desktop text is larger than 12px and saves the same views
under `/tmp/q1-screenshots/model-before`. `VIEWPORT=desktop,phone,narrow`,
`THEME=light,dark`, and browser path overrides work as in `motion.cjs`.

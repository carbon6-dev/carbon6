# Tests that fail only when something is wrong

Status: rules in force (2026-10-19); `make check` enforces what a pattern can catch (`scripts/test-lint.sh`).

## Motion: production browser regression

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

At desktop height, exercise real columns of 360/400/440/480px tiles before
applying the chosen floor (temporarily override the canvas floor, not just the
bar width). The transcript and bar must share the same actual tile width.
All seven controls must remain 44px, with identity separate;
record title room and overflow rather than shrinking targets. Verify the
480px floor and horizontally scrolling canvas. Inspect all four rim states,
neutral context arc, dashed waiting, failed cross, positive-only badge, worded
accessible name/tooltip, and stripe-default/Dot-only Appearance preference.
Keep screenshots for both themes; validate new arbitrary utilities in built
CSS as well as geometry. Emulated keyboard tests do not replace a physical
phone keyboard/accessibility review.

## Transcript follow: manual browser check

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
2. Retain DOM handles for the card, its `details`, and its output `pre`. Deliver output chunks, its result, and another assistant event: all three handles must remain connected and refer to the same nodes. A user-opened card stays open; a user-closed card stays closed.
3. Create a session and a document with long titles. Their pending link areas must already occupy the completed card's height, with the input title when known. The result activates the destination without adding a row or horizontal overflow. Keep all status, document/deck and navigation icons.
4. With a long transcript following the bottom, repeat through completion: it stays at the bottom without a transient expansion/collapse. Scroll up deliberately before a result arrives: the viewport stays at that position. Repeat on a touch viewport and with reduced motion.

### Observer lifetime regression

The transcript-follow and glance-width ResizeObservers retain their Rust
callbacks alongside the observer, disconnect before dropping either on replacement
or unmount, and never `forget()` the closure. The message box uses the same
ownership pattern. The transcript's on-sight IntersectionObserver retains its
callback until cleanup too (as the glance outline already does).

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

## Task card actions: manual phone check

Check both a session transcript and Decisions, with a coarse pointer at 320px and 390px widths, and on desktop. Use a realm with several open work items and a closed one.

1. The actions read **Start session**, **Do it here**, **Save for later**, **Not now** in that order. Only Start session is filled; the next two have identical visible outlines and heights. They share one wrapping row, with no split arrow or stray divider. Coarse-pointer targets are at least 44px tall; desktop buttons remain compact.
2. Save for later's tooltip names the proposing session's work. Clicking it sends `Add to this work`, without a picker. With no work, it creates new work. Do it here's tooltip names the proposing session.
3. **Save elsewhere…** opens over the workspace, not inside the card. Scroll the transcript so the card is directly above the message box, then open it again: the whole dialog remains visible, with a scrolling list rather than content clipped by the composer. Try light/dark mode and the phone keyboard.
4. The chooser offers **New work** once and open work only in the card's realm. Search is case-insensitive. A nonmatching search shows an empty-state message but keeps New work available. The New work choice sends `Add as new work`; an existing work item sends `Add to work:<id>` and receives the card's title, prompt and origin as a to-do.
5. Close, outside click and Escape dismiss the chooser; Escape must not stop the session. Focus returns to Save elsewhere. A failed save leaves the card open with an error; all action buttons disable while a save is pending. Check the chooser with a long list and a long work title.
6. With only task cards open, the transcript shows **Suggested tasks (N)** without an empty **Waiting for you** heading. Add a question or approval: Waiting for you returns. Answer the last non-task decision: its heading disappears while Suggested tasks stays.

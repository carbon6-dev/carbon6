# Tests that fail only when something is wrong

Status: rules in force (2026-10-19); `make check` enforces what a pattern can catch (`scripts/test-lint.sh`).

## Browser installation and pinned engines (S2)

Use Node 24 and npm on PATH. The browser suites have a private npm manifest and
lockfile in `crates/c6-ui/tests`, pinning **playwright-core 1.65.0**. Install once
per checkout (and again after changing the lockfile):

```sh
make ui-browser-install
```

This runs `npm ci --prefix crates/c6-ui/tests` and the **installed package's**
`cli.js install --with-deps chromium webkit`. On Linux the latter installs OS
libraries through apt (sudo required); on macOS no Linux library wrapper is
needed. It downloads the matching browsers to Playwright's normal user cache
(`~/.cache/ms-playwright` on Linux), or `PLAYWRIGHT_BROWSERS_PATH` if set for both
installation and execution. Do not use an unpinned `npx playwright` download.
The package pins WebKit **2388 (27.2)** and Chromium/headless-shell **1254
(158.0.8127.18)** on this Linux host. Playwright's platform overrides still
apply (for example WebKit on macOS 14); these are Playwright builds, not branded
Safari. No test dependency, browser binary or library defaults to `/tmp` or a
hard-coded developer-specific home path. Screenshot/evidence output may still use `/tmp`.

```sh
set -euo pipefail
ulimit -n 65536
env -u NO_COLOR make ui
make ui-browser-test
# Three complete passes, no retries or assertion changes:
UI_BROWSER_PASSES=3 make ui-browser-test
```

The runner covers workspace-open, motion, comment-sheet, phone-picker and
theme (`--browser`) in each engine, plus ring-render in Chromium. **ring-render
is Chromium-only**, because its layer/paint/blocked-main-thread assertions
require CDP; `ENGINE=webkit` now fails explicitly instead of silently launching
Chromium. Its full default three-run/two-theme workload remains enabled.
The default motion matrix has 24 contexts per engine; phone-picker has 24.
Each suite keeps its normal shared-process/context policy. `ISOLATE_BROWSER`
is not enabled. `BROWSER_ENGINES=webkit` selects just that engine for a complete
pass; `BROWSER_TEST_OUTPUT` changes the default `dist/browser-tests` output.
Logs/screenshots/traces are separated by pass, engine and suite. A failed
command stops the runner, keeps the evidence, and is never automatically retried.

Existing CI workflows are unchanged: these standalone browser suites were not
previously CI jobs. A browser runner on Ubuntu 24.04 needs Node 24, npm, the
existing Rust/Trunk/Brotli setup, `make ui-browser-install` (including sudo apt),
`env -u NO_COLOR make ui`, then `make ui-browser-test`; preserve
`dist/browser-tests` on failure. Do not enable a required browser job while the
independent Motion fields-panel failure recorded below is unresolved.
Downloads/apt are installation steps, not network access by the tests: the
fixtures still serve only private loopback origins. Rust test jobs are unchanged.

### Why change WebKit?

The approved N7 cold-load comparison alternated the same production WASM for
60 rounds per arm. The old Playwright 1.50.1/WebKit 2123 failed **12/231 loads**
(10 out-of-bounds, one unreachable, one disposed-value panic); Playwright
1.65.0/WebKit 2388 failed **0/240** (Fisher p≈0.0002). Original commands are
`/tmp/n7-webkit/run.sh` and `repeat-any.cjs`; raw results are under
`/tmp/n5-webkit/n7/`. Those are historical evidence locations, **not runtime
dependencies**. This supports replacing the old test engine, not a claim that
a particular WebKit source bug was identified, all application lifetime errors
were repaired, or real Safari was tested. The S2 verification results below
record the new full-suite cohorts separately from that cold-load experiment.

### S2 verification: November 7, 2026

Branch base: `c6/next` at `5384d8e2`, Linux x86-64/Debian 12, Node 24.16.0.
The installer and both smoke launches succeeded using the standard cache and
OS libraries, with no browser/core/library overrides for qualification.
Production WASM: **10,713,719 bytes**, Brotli **2,665,834 bytes**, SHA-256
`8dc5d6b0d6d7b2f526aa0a802e1b556ee4ec99bd2ad11bd450f934a743fb3001`.
The production source, optimization and size ratchet were not changed.

The required command sequence ran with `set -euo pipefail; ulimit -n 65536`:
`env -u NO_COLOR make ui; make check; make test`. All succeeded: clippy had no
warnings, test lint and size checks passed, and nextest reported **1,167 passed,
2 skipped** (two slow tests passed). A final `make check` also passed.

Three full invocations per engine are recorded for workspace-open and the
comment-sheet/phone-picker/theme matrices, and three full default Chromium
ring-render invocations. Motion qualification remains blocked, as explained
below; this is **not three green passes of the combined browser runner**.

| Suite | WebKit 2388 | Chromium 1254 / 158.0.8127.18 |
| --- | --- | --- |
| workspace-open | 3 complete passes, 12 viewport/platform scenarios | 3 complete passes, 12 scenarios |
| comment-sheet | 3 complete passes, 24 cases | 3 complete passes, 24 cases |
| phone-picker | 3 complete passes, 72 contexts | 3 complete passes, 72 contexts |
| theme `--browser` | 3 complete passes, 276 reported PASS checks | 3 complete passes, 276 reported PASS checks |
| ring-render | Not applicable: requires CDP; no WebKit result claimed | 3 complete passes, 18 default theme/workload runs, including tracing, blocked-main-thread and raster assertions |
| motion | **0/3 complete passes**: failed after 6, 9 and 1 passing contexts respectively; 3 failing contexts retained | **0/3 complete passes**: each stopped on the first context's fields-panel invariant |

**Observed WASM traps: zero** in these repeated cohorts and in the failed
Motion/initial harness runs. This is an observed count, not a proof of universal
engine reliability or a passing result for the independent failed assertions.

The first runs exposed two stale harness assumptions, retained in the logs:
workspace-open matched the secondary Sessions row's title as its entire
accessible name, omitting its status/worker subtitle; motion dereferenced a
lazy tool body before its native toggle mounted it. The exact row name now
includes the subtitle (the count must still be one), and the geometry wait
waits for the body before applying its unchanged expanded-height predicate.
The tool failure reproduced alone; the corrected check passed in both engines.

Additional tile-marks coverage initially timed out twice because Actions was
looked up beneath the tile after its popover had moved into a body portal.
The test now identifies exactly one newly mounted `data-pop` identity after
clicking the intended tile's Actions, keeps that identity for Escape, and
retains all availability/icon/resize/geometry/retained-node assertions. The
complete four-case Chromium/WebKit light/dark matrix then passed.

**Unresolved Motion interaction, not a WASM trap:** choosing an offered model
can hide New Session's where/how fields before effort can be used. A diagnostic
trace records native pointerdown/mousedown/mouseup/click on the intended
`fixture-model-32` button, nested in both `data-pop` regions, followed by the
fields becoming hidden and their opener changing from expanded to collapsed.
The saved failure diagnostics have empty browser-error arrays. The same
panel-open failure reproduced with the old **Playwright 1.50.1 / Chromium
133.0.6943.16** as an explicitly labelled control against the same production
WASM; it is not introduced solely by Chromium 158. WebKit also recorded
fields/list-reopen and effort-selection hangs in the full runs. Isolated
modelForms reruns of all three failed WebKit cases (dark/System,
dark/Full-reduce and light/System-reduce) passed; those diagnostic reruns do
not erase the failed full cohorts. No production fix or claim of a
source-level cause is made here.

The harness now explicitly requires the fields to remain expanded after both
model and effort selection, rather than concealing dismissal behind a
five-minute hidden-control wait. It never reopens the panel for the test,
selects a hidden control, forces a click, retries a failed case, or filters
modelForms out of the normal matrix. The requested three-green-full-pass
qualification cannot be claimed while that independent failure remains.

Supporting checks: motion VM/launcher **13 passed**, theme VM **12 passed**,
crash VM **8 passed, 1 browser-only test skipped**, real-DOM motion helpers
**6 passed**, plus syntax checks for all CJS files and the Bash runner. A
stubbed runner check verified 33 suite invocations for three two-engine passes,
the theme browser flag, Chromium-only ring selection, retained failure logs,
and immediate propagation of a simulated exit 7 without retry.

Raw evidence is local to the S2 worktree under `dist/s2-verification/`
(installation, provenance, required/final checks, initial failures, isolated
reruns and event diagnostic), `dist/s2-workspace/`,
`dist/s2-{chromium,webkit}-additional/`, and `dist/s2-motion-blocker/`.
Pass/engine/suite subdirectories preserve distinct cohorts. The earlier
`dist/s2-{chromium,webkit}*` fail-fast runs are retained too, not overwritten
or counted as clean passes. Explicit old-engine paths occur only in that
labelled control command, never in the checked-in default setup.

## Browser crashes: preserve the first failure (N5, updated S2)

The shared production-browser fixture identifies a WebKit/WASM trap as a fatal
failure and notes the old/new comparison above, not a proven source defect. It still
rejects its crash promise, records every browser error, and fails every existing
assertion. There is no expected-failure allowance or automatic successful retry.
An `unreachable` trap can follow a Rust panic; inspect earlier console messages
and `/api/v1/ui/error` reports before attributing it to WebKit. A later disposed
value or null DOM error after an initial memory trap is not independent proof of
its cause. A clean isolated rerun is evidence about intermittency, not a pass for
the failed run. Preserve both logs and the exact WASM SHA-256.

The panic hook now captures its JavaScript/WASM stack synchronously, rather than
letting the subsequent abort trap replace it. Crash reports prefix that stack
with bounded mounted tile/pane keys, static view kinds, and browser user agent.
The snapshot reads neither view payloads, message text, titles, nor saved layout.
`pending` distinguishes an unmounted pane shell; retained hidden panes still
count as mounted. The existing head endpoint's 2,000-character stack cap applies.

```sh
node --test crates/c6-ui/tests/crash.cjs
node crates/c6-ui/tests/motion.cjs --unit
# Actual restore/dedupe, target visits, tile closure, menus, and draft retention:
ENGINE=both node crates/c6-ui/tests/workspace-open.cjs
```

Browser executable and `ROOT` overrides are the same as `motion.cjs`. On Linux,
setting `navigator.platform` to `MacIntel` in a test does **not** run Safari or
macOS WebKit. Disabling BBQ/LLInt is an investigation control, not the production
test configuration or an application workaround. See the N5 investigation in
`docs/ui-responsiveness.md` for observations and remaining limits.

## Working ring and transcript rendering

### Background-notice descriptions

After building the UI, run the production-WASM regression (Node and browser
overrides are the same as `motion.cjs`):

```sh
ENGINE=both SCREENSHOTS=/tmp/q5-told node crates/c6-ui/tests/task-notices.cjs
```

It checks light/dark themes on desktop, four narrow tiles, and 390/320px phones.
Closed notices stay one line and retain their icon; subagent names/statuses and
command descriptions come from metadata. Expanded reports render Markdown and
link to their session, with the exact model-facing text folded separately. Old
events without metadata keep a neutral fallback and their original text. Browser
errors fail the run. `BEFORE=1 ROOT=/path/to/base/dist` captures the old closed and
expanded rows with identical fixtures, without asserting the new presentation.
Screenshot names begin `before-`/`after-`, so both sets coexist.

After `env -u NO_COLOR make ui`, run the production Chromium regression with
the same Node/Playwright/browser overrides as `motion.cjs`:

```sh
/home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/ring-render.cjs
PARAGRAPHS=180 LABEL=after-heavy /home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/ring-render.cjs
```

The private loopback fixture has three tiles and 93 historical replies. At 4×
CPU throttling it streams eight chunks before each of twelve new Markdown
items, in both themes. It checks retained rim/context nodes and both running
animation identities across item arrivals and changing session snapshots. The
1.4s spin belongs to the HTML wrapper; a clipped outer SVG sweeps over 2.8s to
grow/shrink the arc from 8 to 30 units. Both must have their own active-transform
compositor layers, transform-only keyframes, and no motion on the independently
drawn context. All four states and the unchanged static Reduced/Off working rim
are checked too. Run with `SCALE=2 LABEL=after-retina` for Retina-sized output;
the default scale is 1. Use distinct labels to preserve both sets of evidence.
The first run also saves a paused nine-phase full-cycle comparison against the
original dashed circle: 16/20/24px, integer/quarter/half-pixel placement, both
growth and shrink, and the cycle boundary. It checks arc length/easing, weighted
blue ink and angular continuity, and saves all-state Full/Reduced/Off galleries.
DSF-2 gallery PNGs have twice the physical resolution. CDP screencasts can
instead return CSS-pixel images; their own viewport metadata determines crops.

Evidence goes to `/tmp/q3-ring/` (`EVIDENCE` overrides it): JSON timing and
main-thread rAF gaps, long tasks, screenshots, and a trace/CPU profile on the
first of three runs. Runs 1 and 2 are unprofiled. Timings and frame counts have
**no pass/fail budget**. A separate 600ms deliberately blocked-main-thread
screencast follows the first run's natural workload. Its blue-pixel area checks
arc-length change independently of the centroid's angular movement: spinning a
fixed arc alone cannot satisfy the breathing check. The natural trace window
must contain zero ring-specific Paint records. The separate blocked-main-thread
challenge is not a measured transcript long task or a global FPS benchmark.

`BASELINE=1 ROOT=/path/to/pinned/dist` records the old implementation without
enforcing the new compositor/retention invariants. `LABEL`, `THEME`, `RUNS`,
`CPU`, `SCALE`, and `PARAGRAPHS` select evidence names/workloads. Compare identical
workloads and separate traced from untraced runs. The recorded DOM-update and
next-rAF latencies are **not presentation times**; use the trace's layout,
paint and compositor events for attribution. See the Q3 section of
`docs/ui-responsiveness.md` for measured findings and limitations.

For the shared phone/desktop header ring, run:

```sh
PHONE=1 EVIDENCE=/tmp/q11-ring/row-link /home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/ring-render.cjs
```

This mode checks 320/360/390/430px, both themes, Full/Reduced/Off and DSF 1/2.
It uses twelve open pages, a long session title, and real background-task events
for counts 0/1/2/12/123. It checks 44px bar/targets, title ellipsis, no badge
collision or header movement throughout count transitions, retained icons,
ring-to-sheet touch navigation, state cues and the two Full compositor layers.
It also checks all seven visible composer-status variants and quiet, the single
full-width 44px opener, full task wording, chevron and visible keyboard focus,
Enter/Space/Escape and taps on each part of the row. Geometry and native PageUp
checks preserve the draft, composer, transcript viewport and reader position
between visible states. Real approval/question cards keep their Allow/Deny and
Send/Cancel actions and send the expected mocked API payloads.
Screenshots and geometry JSON go to `/tmp/q11-ring/row-link/` by default. `THEME`,
`MOTION`, `WIDTHS` and `SCALE` can select a smaller diagnostic matrix.
The normal desktop mode also checks singular/plural task and server labels,
including mixed counts, after its measured workload; its spinner and chevron
must remain mounted while the wording changes.

## Published files: root replay, loading and versions

After building, `node crates/c6-ui/tests/outputs.cjs` checks independent
transcript/Outputs feeds of the same root session in light/dark desktop,
narrow-tile and phone layouts. It withholds the Outputs feed's `Synced` while
the transcript already has its Published row: pending replay must say loading,
not “Nothing published yet”. After replay, republishing an old filename must
bring it to the top and make it the default preview; older titles and content
remain selectable as versions. Long version titles must not widen the control
past its pane; the two-column layout follows the pane's container width, not
the desktop window. A live root publication must also appear.
`BEFORE=1 ROOT=/path/to/base/dist` reproduces the old false-empty message and
first-publication ordering. `ENGINE=webkit`, `THEMES`, `VIEWS` and `SCREENSHOTS`
select the browser, cases and private evidence directory.

The worker integration test
`subagents::normal_subagent_publication_replays_to_independent_root_subscribers`
uses a real task/publish flow and two head WebSockets to check that a normal
subagent publishes into the root log, not its child log, and that old Output
events survive tail summaries. No child aggregation is needed. These are
confirmed UI replay/order cases, not a claim to have diagnosed a particular
live session's missing deliverable; that original incident remains unconfirmed.

## Usage navigation naming

`node crates/c6-ui/tests/usage-name.cjs` checks that the console header and
console/realm menus all say **Usage**, including wide and 320px desktop tiles
and 320/390px phones in both themes. It follows the real menu link and checks
the console's Prices help uses the same name. Clipboard, session-details Usage
and the model quota popup are not changed by this naming pass. Screenshots and
control geometry go to `/tmp/n3-ui/item7`; `SCREENSHOTS` and `ENGINE` use the
shared production browser harness's overrides.

`BEFORE=1 ROOT=/tmp/n3-ui/item2/pristine-235fae5c-dist` captures the same
matrix with the old console/realm names and Prices help. Filenames have
`before-`/`after-` prefixes so neither phase overwrites the other.

The naming pass does not fix two inherited desktop-menu behaviours: its
opener is hidden when all measured header items fit (the fixture resizes to
1024px to open it), and `max-w-full` constrains its drawer to the narrow opener
wrapper. Geometry JSON and logs record that existing desktop link overflow;
phone link containment remains an assertion. Do not report those screenshots
as proof that the unrelated desktop drawer layout was repaired.

## Content-fitted desktop header

The desktop header uses measured item widths, not an `xl` navigation cutoff.
Sessions/Inbox, search, Decisions, Work and Docs take precedence over Projects,
analytics, apps/collections/workers, saved views, tile creation, administration,
connection text and the build tag. The realm and quick attention/Ask controls
have reserved space. Exceptionally long realm names are bounded to 36vw/28rem,
with their full name on hover, rather than being squeezed by navigation.
Overflowed links remain inert and measurable offscreen; the existing menu
contains their live links and badges. An amber dot on the menu, with a numeric
screen-reader description, reports counts on overflowed links. Notifications
remain icon-only, with state in matching title and aria-label.

`header-fit.cjs` uses the production browser fixture below, both engines and
themes, short labels/zero counts and a long realm/build tag with five-digit
Decisions and three-digit Projects/Work counts. It resizes **the same mounted
header** through 768, 1024, 1200, 1280, 1600 and 2000px, asserts pairwise control
rectangles never intersect or escape the header, and repeats with focused
search. It also checks readable realm space, menu badge preservation, its
accessible count indication, ample-space restoration, and no settled attribute
feedback loop. ResizeObserver watches intrinsic widths; MutationObserver only
watches content (including equal-width count changes), not its own attribute
outputs. There is no polling, per-frame loop, DOM reparenting or duplicate
measurement tree. Initial fitting runs before the header is made visible.

```sh
set -euo pipefail
env -u NO_COLOR make ui
ENGINE=both SCREENSHOTS=/tmp/q2-screenshots \
  /home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/header-fit.cjs
```

`BEFORE=1 ROOT=/path/to/pinned/dist` captures the baseline without enforcing the
new invariants. PNGs and per-case geometry JSON go to `SCREENSHOTS` (default
`/tmp/q2-screenshots`). `THEME=light,dark` and `COUNTS=short|large` select a
diagnostic subset; browser paths and `ROOT` use the motion harness overrides.
Every scenario owns a fresh browser process. Missing browsers fail explicitly.

For the full parallel Rust suite, raise a low per-shell descriptor limit first
(`ulimit -n 65536` on this Linux worker; the inherited soft limit is 1024), then
run `make check` and `make test` with `set -euo pipefail`. This affects only the
verification shell, not services or persisted machine settings.

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

### Session identity

`node crates/c6-ui/tests/session-identity.cjs` checks the production session
glance, details and shared desktop/phone Actions menu in both themes, wide and
320px desktop tiles, and 320/390px phones. The ID shows eight characters with
the full value on hover; clicking copies the full ID. Actions offers **Copy
session ID** and **Copy session link** (including the realm). The fixture
records clipboard writes and checks denied writes never report success.
Screenshots go to `/tmp/n3-ui/item6` (`SCREENSHOTS` overrides it). It uses the
motion harness's browser paths and `ENGINE` override, with no live head.

`BEFORE=1 ROOT=/tmp/n3-ui/item2/pristine-235fae5c-dist` captures the same
glance/menu/details matrix without expecting the new ID controls. It records
their absence explicitly in JSON and prefixes screenshots with `before-`.

### Sticky document ownership

`node crates/c6-ui/tests/doc-owner.cjs` uses the production browser fixture
with real docs and decks, long titles and enough content to scroll. Both
themes cover 1200/320px desktop tiles and 320/390px phones. It asserts the
creator's dot stays beside the single-line title and remains in the sticky
header, names the session on hover and for screen readers, follows live
name/pattern changes, and opens the same retained destination as **⋯ → Open
the session that made it**. It also checks a creator outside the live feed
and the absence of ownership controls for user/token-created documents even
when a session last edited them. Existing title/action SVGs remain present.

Screenshots and geometry go to `/tmp/n3-ui/item8` (`SCREENSHOTS` overrides it).
`BEFORE=1 ROOT=/tmp/n3-ui/item2/pristine-235fae5c-dist` records the same matrix
and explicitly records absent header marks/menu entries, without applying the
new assertions to the pristine build. `ENGINE=webkit` selects WebKit; cases
own separate browser processes; `CASE` selects a diagnostic name fragment.
The pristine 390px phone deck emits one startup ResizeObserver delivery error,
also reproduced after the change. Both modes record it explicitly in JSON and
the log while checking the layout/navigation; AFTER rejects repeated delivery
errors, errors after startup and all other browser errors. Neither mode changes
a live head or settings.

`crates/c6-ui/tests/theme.cjs` checks System, Light and Dark, including migration
of the former browser-local Soft preview (and legacy Dim) to Dark before paint:

```sh
node --test crates/c6-ui/tests/theme.cjs
SCREENSHOTS=/tmp/q6-dark/after node crates/c6-ui/tests/theme.cjs --browser
ENGINE=webkit THEME_DELAY_MS=1000 node crates/c6-ui/tests/theme.cjs --browser
```

Use Node 24 and the same browser overrides as the motion harness. The browser
test delays the real theme bootstrap fetch, holds WASM, and checks the first
body frames before the application renders. It also tests three usable radio
cards at 240/320/400/480/640px tile and phone widths, persistence, OS changes,
and real cross-tab palette and selected-card updates.

`theme.cjs --browser --workspace-radios` additionally checks one/five/six-tile
workspaces and restored duplicate Settings tiles collapsing to one Appearance
view with its selection intact: actual native `.checked`
properties, selected-card paint, Dark/System persistence through maximizing
another tile, restore and reload, and same-document/cross-tab motion-choice
synchronization. Navigating away from Appearance and back verifies that Dark
and the selected motion survive a remount, with fresh, distinct native group
names. Native group names are per mounted instance; hidden views
still participate in document-wide groups. Inert cloned radios participate
too, which is another reason tile motion must not clone outgoing content.
The saved-duplicate cases respect Settings' single-copy restore policy rather
than waiting for an intentionally removed second Appearance tile.
The suite also exercises both custom `Choices` callers (session questions and
decision forms), optional clearing, and the actual session aspect palette
through that lifecycle. `--workspace-choices` runs the custom-control audit
independently of the native-radio cases.
Unsent forms reconstruct their existing defaults after reload; saved aspect
marks persist. These checks do not add persistence to unsent forms.

Dark now uses the former Soft page/panel/chip greys `#1c1d21`, `#25262b`,
`#33343a`, with `#4a4b52` for stronger borders. Dark-only red-400 `#ff7476`
replaces `#ff6467`: the latter measured only 4.294:1 on the lighter chip.
Light's palette is unchanged. The contrast tests assert the unrounded 4.5:1
threshold for muted, red, white, blue, amber and green text on all three
backgrounds, and print measured ratios from both exact swatches and the built
Tailwind tokens (resolved by the browser to sRGB8). SVG text fills and the
muted utility are checked too; shape/border colours are not text colours.
Saved `soft` is rewritten to `dark` by the parser-blocking bootstrap, including
cross-tab updates, so there is no obsolete fourth radio or unselected choice.

Dark contrast reference (sRGB8, page / panel / chip; rounded here, never for
the assertions):

| Text | Colour | Page | Panel | Chip |
| --- | --- | ---: | ---: | ---: |
| Muted / zinc-400 | `#9f9fa9` | 6.420 | 5.757 | 4.729 |
| Red-400 | `#ff7476` | 6.424 | 5.761 | 4.732 |
| White | `#ffffff` | 16.839 | 15.099 | 12.404 |
| Blue-400 | `#51a2ff` | 6.385 | 5.725 | 4.703 |
| Sky-400 | `#00bcff` | 7.728 | 6.929 | 5.692 |
| Amber-400 | `#ffb900` | 9.776 | 8.766 | 7.201 |
| Emerald-400 | `#00d492` | 8.686 | 7.789 | 6.398 |

The accompanying grey-text audit adds dark-only zinc-400 text to Ask and
aspect popovers and readable fills to the grant graph and two-hop diagram
labels; icons keep their colours. Zinc-500 is too weak for dark text, but
remains a shape/border colour. Document charts' zinc-300 axis labels at 0.6
opacity still pass on their actual page/panel backgrounds (4.94 / 4.65).
Authored SVG diagrams are isolated images with their own palettes, not CSS
text that the parent theme can universally recolour.

This is not a blanket claim about every inherited text treatment: bare
`text-red-600` errors/removal actions (for example `settings.rs`, `push.rs`,
`memex.rs` and `session.rs`) remain below 4.5 on Dark page/panel/chip
(3.53 / 3.17 / 2.60). They need per-text-site dark variants, not a global
red-600 token lift: white on solid red-600 buttons still passes at 4.77.
Selected autocomplete notes in `chat/composer.rs` and `chat/mod.rs` also have
a pre-existing opacity-60 shortfall (~3.12 on sky-700). These are separate
text-site accessibility follow-ups, not a reason to lighten button backgrounds.
The browser contrast check additionally asserts red-200/300 on red-950,
rose-300 on zinc-800 and rose-900/40 status chips, and white on red-600.

## Motion: production browser regression

`crates/c6-ui/tests/composer-popovers.cjs` checks the running-task, model and
Send menus in both themes on desktop, in a narrow/short tile, and on a phone
with an overlay keyboard. It asserts visible tile/viewport bounds and real
pointer hit-testing, scrolls to the last Stop/Inspect/model choice, opens the
inspector, sends a message, and resizes a retained model picker. The `+` control
opens the native file chooser, not a web popover. Run after `make ui`, with the
same Node/browser overrides as the motion harness. `BEFORE=1 ROOT=...` records
the old build without enforcing the corrected geometry; `SCREENSHOTS` defaults
to `/tmp/n3-ui/item1`. Every fixture uses its own loopback head and browser.

### Transcript pointer and keyboard focus

```sh
ENGINE=both SCREENSHOTS=/tmp/n3-ui/item5 node crates/c6-ui/tests/transcript-focus.cjs
```

Build the production UI first (`env -u NO_COLOR make ui`); Node 24, browser
paths and `ROOT` use the motion harness overrides below. The owned loopback
fixture checks both themes on desktop, a four-tile workspace (under 400px per
tile), and 390/320px DSF-2 phones. Mouse/touch on transcript text and returning
from the composer must not create a transcript outline. Clicking an already
keyboard-focused transcript can retain the browser's native keyboard cue;
that mixed-input state is captured separately. Moving pointer focus away and
back must clear it. Phone status open/close (Close, outside tap and Escape)
and session-sheet open/close must not focus-outline the transcript either.
Native Shift+Tab traversal, Home, PageDown, PageUp and End retain a painted
keyboard cue and working keyboard scrolling, including across background-task
updates.
Screenshots and computed focus/outline JSON distinguish these input modes;
`ENGINE`, `THEME` and `VIEWPORT` can select a diagnostic subset.
Each scenario owns a fresh browser process as well as its context; startup
errors fail rather than being retried or suppressed.

The white outline in the Q11 `phone-dark-reduced-390-dsf2-background-12.png`
capture is inherited keyboard-test focus, not a touch/status interaction:
`ring-render.cjs`'s `phoneStatusRow` ends by explicitly focusing its transcript
and pressing End, then the same page is resized for the next width's captures.
Do not remove the scroll container's native keyboard focus indicator to hide
that test state. No production focus or styling change is needed for it.

### Transcript layout steps

After `env -u NO_COLOR make ui`, run the owned loopback production fixture:

```sh
/home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/transcript-steps.cjs
```

It sends through the real composer, presents a long transcript, starts a turn,
updates usage, runs a tool call/result, starts a background task, queues a second
message, streams a reply, and finishes the reply/turn/task/queue in a burst.
Both themes, a 400px desktop tile and a phone are checked in Full/Reduced/Off.
Per-frame scrollTop, scrollHeight, viewport, unchanged-message position and
estimated native thumb geometry are saved alongside DOM mutations and row
ResizeObserver records. It counts distinct scroll corrections for the whole
send-and-finish scenario, requires no usage/task-status height change, checks
bounded translation-only Full effects (none in Reduced/Off), and asserts that
upward input prevents later messages moving the reader. The narrow tile also
exercises rows leaving the live tail after a width change; their measured
height must not revert to an older intrinsic-size cache, including after an
upward pause, a 400→520px width change and native End to resume. Composer focus must
not pan the fixed app shell. It does not treat smoothly translated pixels as
additional logical scroll corrections. Thumb geometry is estimated from the
scroll range, not a measurement of a platform's native scrollbar raster.
The light desktop Off case also holds a pending frame, hides the document,
and requires the whole wire-ordered queue to flush and cancel that handle.
Hidden delivery and a failed visible frame registration must both make progress
without a subsequent animation frame.

`EVIDENCE` chooses the output directory (default `/tmp/q8-steps/after`);
`ROOT` selects an independently built baseline; `BEFORE=1` records that baseline
without asserting the new layout/animation invariants. `THEMES`, `VIEWS`
(`desktop,tile,phone`) and `MOTIONS` select cases. Chromium-only diagnostics
`CPU_RATE=4 TRACE=1` save a DevTools trace and active reading-column compositor
reasons. These are measurements, not machine-speed pass/fail thresholds.
`BLOCK_MS=300` diagnostic saves timestamped compositor frames during a busy
main-thread interval and the effect's entry clock; `BLOCK_PHASE=text` selects
overlapping queue/text corrections instead of the default tool-call phase.
The shared fixture's per-frame animation scanner is deliberately disabled here:
this harness observes production effects at creation and samples only its
unchanged anchor, rather than materializing every off-screen event. No live
head, settings, services or browser profile are used.

`ONLY=sessionsPicker` smoke-checks the phone Sessions right drawer's header
alignment, retained SVG icons, absence of a redundant top X/handle, and
tap-outside/Escape dismissal. Include
`VIEWPORT=narrow,phone,landscape THEME=light,dark` for the 320px, 390px and
short-height phone layouts. The dedicated `phone-picker.cjs` regression below
adds native right swipes, search, keyboard/focus containment and message actions.

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
to your head or use your browser profile. Install the pinned browsers above and build first
(coordinate the build when sharing a worktree), then run with Node 24:

```sh
env -u NO_COLOR make ui
/home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/motion.cjs --unit
ENGINE=both /home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/motion.cjs
ENGINE=both /home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/motion.cjs --helpers
```

Defaults resolve the locked test-local `playwright-core` and its matching
installed browsers. `PLAYWRIGHT_CORE`, `CHROMIUM`, `WEBKIT_EXECUTABLE`, and
`CHROMIUM_LIBS` remain explicit **diagnostic overrides**; do not use them for
the pinned-engine qualification or CI. `ROOT` overrides `crates/c6-ui/dist`. Missing browsers or
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
`CHROMIUM_LIBS`, if explicitly set, augments the inherited `LD_LIBRARY_PATH`;
the default uses the OS libraries installed by Playwright, with no wrapper.
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

## Production image-preview browser regression

`crates/c6-ui/tests/image-preview.cjs` runs the actual production WASM, not
recreated markup. It reuses `motion.cjs`'s private, port-0 loopback HTTP/WS
fixture and browser launcher. It never starts a head/worker or changes a live
account. All API replies, transcript attachment bytes, pending upload bytes,
and the document's SVG drawing are local fixtures. Each case gets private
browser storage and a fresh browser process.

Build in the worktree being tested, then run with Node 24 and Playwright Core
1.65.0 from the locked install above. No npm install or UI rebuild occurs in
the harness. The build requires Trunk, the WASM Rust target, Tailwind and
`brotli` on PATH. Do not pipe `make ui` through `tail` without `pipefail`: a
failed build must not silently test an older `dist`.

```sh
set -euo pipefail
env -u NO_COLOR make ui
ENGINE=both node crates/c6-ui/tests/image-preview.cjs
```

On thalassa, Node 24 is at
`/home/zheilbron/.nvm/versions/node/v24.16.0/bin/node`. Browser/library defaults
come from `motion.cjs`; other machines can set `PLAYWRIGHT_CORE`, `CHROMIUM`,
`CHROMIUM_LIBS`, and `WEBKIT_EXECUTABLE`. `ROOT` selects an existing production
dist, `ENGINE=chromium|webkit|both` selects the browser (default Chromium),
`VIEWPORT=desktop-wide,desktop-narrow,phone,phone-narrow` filters cases, and
`THEME=light,dark` filters themes. The desktop cases use a 1440×1000 window
with one or three real workspace tiles; phones use 390×844 and 320×568.

The three real entry points are exercised in every case:

- a received transcript image (`TranscriptImage` → `FullImage`);
- a file added through the composer's real file input, then its thumbnail;
- a document `diagram` block (`Expandable`, with `fill=true` → `FullImage`).

Checks cover full-window scrim bounds and four-corner hit testing, absence of
tile ancestors, image fit, explicit 14px captions/12px actions, image/caption
clicks staying open, Escape/Close/outside dismissal, opener focus return, and
attachment removal's composer-focus fallback. Drawing preview must not enter
document editing and also opens with Enter/Space. Geometry, decoded-image
readiness and DOM state are the completion conditions; there are no fixed
sleeps or external requests.

Screenshots and JSON ancestry/geometry measurements go to
`/tmp/q2-screenshots/` (`SCREENSHOTS` overrides it), named
`after-<engine>-<viewport>-<theme>-<caller>.png`. For baseline diagnosis:

```sh
ROOT=/tmp/q2-baseline-dist BEFORE=1 ENGINE=both \
  node crates/c6-ui/tests/image-preview.cjs
```

`BEFORE=1` records `before-*` screenshots and proves the baseline transcript
preview is row-constrained. It temporarily resets one ancestor CSS property
at a time, waits for rendering, records the geometry change and restores the
original styles. Against `8d0079b9`, removing only the transcript row's
`content-visibility:auto` makes Chromium's fixed preview fill the window;
WebKit changes its origin but retains the measured row size in this live-style
experiment. Both reproduce row-constrained previews in wide tiles too.
Composer and document previews fill the window at rest in the tested
browsers, but all three previously remained under tile/pane ancestors, which
can also contain fixed children during transforms. The fix portals the
complete shared preview, not just its image or an individual caller, without
removing transcript virtualization or tile clipping.

The accompanying source guard retains the model picker's typography checks
and additionally guards the image portal, all three callers, explicit image
action sizing and the audited viewport-overlay boundaries. The same escape
is applied to the shared workspace/session popovers, session overview/catch-up
phone sheets, session-preview cards, row menus,
person cards, session details, file/folder pickers and Add Worker dialog.
Existing task/decision/archive/Ask/needs-you dialogs already escape via a
component or caller Portal. Absolute document comment popovers and anchored
composer menus remain tile-local. The responsive new-session fields panel
(static on desktop) and stop confirmation (absolute dialog with a fixed click
catcher) are mixed anchored layouts, not blindly moved to `body` as a pair.
The browser harness also opens tile Actions, Session details and phone document
Comments as representative audited surfaces and checks their ancestry and
bounds. Document Comments and Sessions' Filter, sort and group now deliberately
portal to the owning tile's surface, with both sheet and scrim contained there
(see the sheet regressions below); image previews and the other viewport
overlays still escape.

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

## Sessions filters: placement browser check

Build once with source unchanged during the build, then run the production
loopback fixture with Node 24 (the browser/`ROOT` overrides match `motion.cjs`):

```sh
set -euo pipefail
ulimit -n 65536
env -u NO_COLOR make ui
scripts/ui-size.sh
node crates/c6-ui/tests/sessions-sheet.cjs
ENGINE=webkit node crates/c6-ui/tests/sessions-sheet.cjs
```

Both themes cover the desktop Sort popover, a narrow desktop tile's Filter,
sort and group sheet, and a phone with an overlay keyboard. The sheet and scrim
must belong to their own tile, stay within its bounds and above the keyboard,
and close through Done, Escape and the scrim. The body remains independently
scrollable. Browser errors fail the run. `SCREENSHOTS` defaults to
`/tmp/n3-ui/item4`; `BEFORE=1 ROOT=/path/to/baseline/dist` captures baseline
geometry/screenshots without enforcing the corrected bounds or dismissal checks.
The native `viewport_portals_escape_tiles_and_keep_their_own_typography` guard
keeps the document and Sessions sheets tile-local without weakening the guards
for viewport overlays.

## Doc comments: placement browser check

`reply-recovery.cjs` holds reply A's request until draft B is typed, then
fails A. It checks that B is unchanged, A is separately recoverable after a
real reload, and Restore reply cannot replace a nonempty draft. Desktop,
narrow desktop tiles and phones run in both themes in Chromium:

```sh
env -u NO_COLOR make ui
/home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/reply-recovery.cjs
```

Screenshots go to `/tmp/n3-ui/item3` (`EVIDENCE` overrides it).
`BEFORE=1 ROOT=/path/to/baseline/dist` asserts the original overwrite and
captures before evidence. `ENGINES=chromium,webkit` additionally runs WebKit;
startup traps fail explicitly rather than being retried or hidden.

`reply-recovery.cjs --responses` checks the shared HTTP decoder against
malformed successful JSON, a structured 503, non-JSON status-text fallback,
a rejected response-body read, and a 401 arriving after the workspace mounts
(which must replace it with sign-in). Every case owns its mocked responses
and browser context. Use `ENGINES=chromium,webkit` for both engines; no live
head or settings are touched. The normal mode above still covers deferred
mutation failure and draft preservation.

After `env -u NO_COLOR make ui`, run both production regressions:

```sh
ENGINE=both /home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/comments-design.cjs
ENGINE=both /home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/comment-sheet.cjs
```

`comments-design.cjs` checks margin-fit and tight wide tiles, four narrow
tiles and 390px/320px phones, in both themes and engines. Geometry asserts
unchanged block positions/sizes across opening, containment, placement beside
the reading column when its margin fits, and no anchor overlap whenever a
usable clear rectangle fits the measured box width and control minimum.
Cases use Motion Off and assert settled geometry, not animated enter/exit
stability. It checks one thread at a time,
the separate thread/message menus and their SVGs, multiline replies with
menus open, nearby Reply/Resolve and the quiet agent footer. Full-panel checks
cover recursive document order, independent and bulk collapse, per-document
browser persistence, reply and own-edit drafts, quote jumps, collapsed new
replies and one scrolling list with no nested thread scrollers. In margin-fit
light, deferred POST/GET responses check that a successful reply retains its
thread and textarea, and a late edit-save cannot erase a newer draft after
collapse/remount/reload. Unread updates are tested via reload with changed
mocked data; this does not claim coverage of the document's `/live` transport.
Before/after PNGs and JSON go to `/tmp/q9-comments/`; `BEFORE=1 ROOT=...`
records the original design without enforcing the new assertions. `VIEWPORT`,
`THEME`, `ENGINE`, `EVIDENCE` and the motion harness's browser overrides apply.
Failures, including WebKit startup traps, fail the run; there is no retry loop
or browser-tier override.

`comment-sheet.cjs` uses `motion.cjs`'s owned loopback fixture and launcher, without a head,
services, live settings or external requests. Four narrow tiles in a wide
window (also a short window), and 390px/320px phones, exercise new comments,
long existing threads and the comments panel in both themes. It checks sheet
and scrim containment, a real click/typing/Escape in another tile, independent
sheets in two documents, final-block visibility, scrollable short-tile controls,
focus return (including switching blocks without unmounting), draft retention,
the All comments handoff, resize, keyboard maximize/restore, tile removal and
a position-only move of the retained tile. Phone
sheets keep the full width and follow a visual-viewport-only keyboard change.
Evidence is saved under `/tmp/q7-sheet/`; `EVIDENCE`, `THEME`, `VIEWPORT`,
`ENGINE` and the motion harness's browser/`ROOT` overrides are supported.
`BEFORE=1 ROOT=/path/to/baseline/dist` records unchanged baseline screenshots
and geometry without enforcing the new tile-local assertions.

`cargo test -p c6-ui` checks margin and four-strip thread placement, reduced
widths, offscreen/hidden tiles, containment, recursive thread ordering, and
the generic panel popover's offset bounds and 320px/390px/desktop widths. In a
browser, repeat at those widths (with touch on phones):

1. Open a long doc and start a comment near the bottom of a wide tile. The
   box uses its margin, or shrinks and avoids the block above/below when
   possible, without moving the document. Reply, Resolve, Cancel and Comment
   remain reachable in their respective forms.
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
shared state/context ring (the only phone route to Changes/Terminal/Files/Outputs).
Sheets and dialogs must stay within the visible viewport and scroll.

Drive idle, main-working, waiting, last-turn failure and background-only
states. No idle row or “0 background tasks”; working must not follow aggregate child
status. Deduplicate linked task/subagent records and exclude foreground native
work and mirrors. The whole 44px status row, with its chevron and visible inset
focus, must expose cards, errors, Inspect/Stop and history. Its task count reads
“1 background task” / “2 background tasks”, matching desktop without “running”.
Waiting-card Show navigation and real approval/answer actions stay intact.
Every visible status variant, including combined two-line statuses, stays 44px
high without resizing the transcript viewport when switching between them.
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

Transcript-follow resize delivery commits the real end synchronously before
paint, with one native inverse-translation trajectory rather than a queued
scroll-animation frame. Deliver its callback twice without changing geometry:
the second invocation must not write scrollTop or restart the native effect.
Scroll away and deliver again: it must recheck following and leave the reading
position alone. Close the tile: cleanup must disconnect the observer before
dropping its callback, with no late reads or dropped-closure errors. The helper's
row-height observers also disconnect when following stops or the root detaches.
With normal-motion WebKit and many tall contained rows, stream and resize at
phone and desktop widths; no ResizeObserver loop warning is acceptable (also
repeat with reduced motion and in Chromium).

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

`task-work-name.cjs` is the production browser regression for Save elsewhere's
New work confirmation. It checks the selected title seed, safe structured
submission of quotes/backslashes/Unicode/markup-like text, Enter and button
confirmation, empty-name prevention, Escape cancellation, reseeding on reopen,
and unchanged existing-work actions. Run after `env -u NO_COLOR make ui`:

```sh
/home/zheilbron/.nvm/versions/node/v24.16.0/bin/node crates/c6-ui/tests/task-work-name.cjs
```

Both themes cover desktop, a narrow desktop tile and a phone. Evidence goes to
`/tmp/n3-ui/item9` (`EVIDENCE` overrides it). `BEFORE=1 ROOT=...` asserts the
old immediate submission and captures before screenshots; `ENGINES=webkit`
selects the additional WebKit check. The head regression
`work::tests::task_card_new_work_name_is_structured_validated_and_optional`
checks the HTTP answer, validation without creating work, unchanged to-do
contents, and compatibility with old unnamed answers.

Check both a session transcript and Decisions, with a coarse pointer at 320px and 390px widths, and on desktop. Use a realm with several open work items and a closed one.

1. The actions read **Start session**, **Do it here**, **Save for later**, **Not now** in that order. Only Start session is filled; the next two have identical visible outlines and heights. They share one wrapping row, with no split arrow or stray divider. Coarse-pointer targets are at least 44px tall; desktop buttons remain compact.
2. Save for later's tooltip names the proposing session's work. Clicking it sends `Add to this work`, without a picker. With no work, it creates new work. Do it here's tooltip names the proposing session.
3. **Save elsewhere…** opens over the workspace, not inside the card. Scroll the transcript so the card is directly above the message box, then open it again: the whole dialog remains visible, with a scrolling list rather than content clipped by the composer. Try light/dark mode and the phone keyboard.
4. The chooser offers **New work** once and open work only in the card's realm. Search is case-insensitive. A nonmatching search shows an empty-state message but keeps New work available. New work opens a focused, editable name seeded from the selected card title, without submitting. Enter or **Create work** sends `Add as new work` with `values.work_title`; Cancel or Escape sends nothing. An existing work item sends `Add to work:<id>` and receives the card's title, prompt and origin as a to-do.
5. Close, outside click and Escape dismiss the chooser; Escape must not stop the session. Focus returns to Save elsewhere. A failed save leaves the card open with an error; all action buttons disable while a save is pending. Check the chooser with a long list and a long work title.
6. With only task cards open, the transcript shows **Suggested tasks (N)** without an empty **Waiting for you** heading. Add a question or approval: Waiting for you returns. Answer the last non-task decision: its heading disappears while Suggested tasks stays.

## Workspace destination reuse and explicit copies

All opens (`Beside`, `OwnTile`, `Instead`, `NewTile`) first look for a live matching
destination in either pane, including minimized tiles. They focus that exact
pane, restore its tile, and reveal it even if another tile fills the workspace.
If explicit live copies exist, the origin tile wins, then the focused tile,
then the first matching tile in saved order. `NewTile` (⌘-click on Mac,
Ctrl-click elsewhere) creates a new tile only for a destination not already
open, except explicit session companion panes, which retain copies. Sessions
themselves always reveal their existing transcript, including from mentions,
lists and the phone picker; companion destinations remain distinct.
Ordinary “New tile” link actions use `OwnTile`; the pane's “Into its own tile”
action detaches its existing pane, without copying it. Home's New tile command
requests the Sessions starter and reveals it if already live. Sessions and
Realms starters each have only one live copy, also from an empty workspace. Opening a
destination found only in history does not steal or rewrite that history entry.

Saved-layout restore collapses current single-copy page duplicates (including
Docs indexes, sessions, Sessions and Realms starters, but not companion copies),
retaining the focused copy/active pane or the first in saved order, with its
exact reading target and key. Only duplicate single-copy panes are
removed; their companion panes and history stacks stay. A duplicate-only tile
without history disappears. One with history traverses Back, then Forward, to
its nearest useful page, preserving skipped snapshots and the opposite stack.
If all its history is already-open pages, Home retains both stacks and the
outgoing page in Forward, provided Home is not already live. Otherwise a
labelled history-only tile retains those stacks without mounting any page.
Back/Forward reveals a live destination or mounts it after its owner closes.
This may retain an extra navigation tile:
deleting it or grafting its independent history onto the winner would lose or
change navigation. Restore is idempotent and makes no rendering-to-HTML calls.

Back/Forward collisions also reveal/navigate the existing single-copy page (including
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
so different investigations can stay side by side. In-place `replace` also
reveals an existing single-copy destination rather than duplicating it.
Selected-parent detail
opens (`open_after`, including Glance's worker and started sessions) also reuse
existing destinations; new details keep the selected parent rather than its
list. Session companion actions (Changes/Files/Terminal/Outputs) retain their
deliberate same-tile placement contract rather than searching globally for a
companion elsewhere in `open_after`.

Session turn targets and document block targets reuse the destination and
record target changes in the destination tile's Back/Forward history, keeping
pane keys. A session target deliberately remounts its transcript owner to
revisit the turn; header controls and menus are keyed to that registration,
and queued old layout effects tolerate its disposal. Untargeted links only focus: they do not reset the reading target,
add history, or discard Forward. Documents accept `/docs/<id>#<block>` and
`/docs/<id>?block=<block>` (a nonempty hash wins); the address bar uses the hash.
The same targets work on `doc:<id>` Markdown links. In-app links retain their
query and hash, including inside document blocks. A document block jump uses
the existing comment-reveal callback to select its tab/slide and flash/scroll
to it without remounting DocPage or losing its block/comment drafts. Clicking
the same target again explicitly revisits it; focus alone does not.

Run `cargo test -p c6-ui workspace::tests` for the real pure `Layout::open`
path (every view and opening mode, either pane, minimize/maximize, duplicates,
targets, history and legacy saved documents, sessions and starters). `cargo test -p c6-ui` also covers
Markdown target preservation and document tab/slide reveal selection.

After building the final combined UI **once**, run:

```sh
ROOT=/path/to/dist ENGINE=chromium,webkit node crates/c6-ui/tests/workspace-open.cjs
```

The standalone harness reuses motion.cjs's private loopback server, Playwright
launcher, session/live fixtures and bounded condition waits. It checks desktop
and 320×568: actual doc links/mentions, links inside a document, query-targeted
tab selection, incoming session-created documents, pane key/DOM/editor identity,
target Back/Forward, minimized docs under a maximized session, modifier reuse
of open documents/sessions/pages and new tiles for unopened destinations,
explicit companion copies, focused/first duplicate collapse on restore, retained companion
panes and history-only navigation shells, pane detaching, Back/Forward and
in-place route collisions, Home's New tile, and real drag/drop without copying
panes. Session cases check independent per-session composer drafts, stream
cleanup and no transient duplicate transcript mount during restore. Pop-up
bookkeeping holds weak owners: a closed dismiss effect or an open menu must
not retain a departed transcript's stream, timers or draft listeners. A focused
history-only tile also releases initial loading after reload. Both
`MacIntel` (⌘-click) and `Linux x86_64` (Ctrl-click) browser platform
identities are tested; native Mac Ctrl-click remains a context click.
Set `VIEWPORT=desktop|narrow` or `PLATFORM=MacIntel|Linux x86_64` to focus a case.
The draft-retention
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
blank-tile command opens or reveals its one Home starter (Sessions, or Realms
in the console). Workspace drag/drop only
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

## Phone Sessions right drawer and message actions (N4)

Use the coordinator's **combined production UI build**; this script never
builds the UI. It reuses `serve`, `fixture`, `settle`, `within`, `launchBrowser`
and the fake session factory from `motion.cjs`, with an owned loopback origin,
fake HTTP/WebSocket responses and a fresh browser context for every case. No
live settings, services, credentials, browser profile or npm install are used.

```sh
NODE=/home/zheilbron/.nvm/versions/node/v24.16.0/bin/node
# Preserve/copy the baseline dist before the coordinator replaces it.
BASELINE=1 ROOT=/tmp/n4-picker/before-dist MOTION_CASE=full "$NODE" crates/c6-ui/tests/phone-picker.cjs
# Run only after the coordinator reports the combined build is ready.
ROOT=/path/to/coordinator/crates/c6-ui/dist "$NODE" crates/c6-ui/tests/phone-picker.cjs
# Optional additional WebKit modal/search/menu coverage; native swipes remain Chromium-only.
ENGINE=both ROOT=/path/to/coordinator/crates/c6-ui/dist "$NODE" crates/c6-ui/tests/phone-picker.cjs
```

The default matrix is 320×568, 390×844, 430×932 and 1440×1000, light/dark,
Full/Reduced/Off: 24 Chromium contexts, or 48 with `ENGINE=both`.
`VIEWPORT=narrow,phone,wide,desktop`, `THEME=light,dark`, and
`MOTION_CASE=full,reduced,off` select cases without changing their assertions.
`PLAYWRIGHT_CORE`, `CHROMIUM`, `CHROMIUM_LIBS` and `WEBKIT_EXECUTABLE` use the
same installed-browser overrides as `motion.cjs`; Node and Playwright must
already be available. `C6_TEST_DEADLINE_SECS` changes the hang guard, not a
rendering-speed threshold. Screenshots default to `/tmp/n4-picker/`
(`SCREENSHOTS` overrides it), with `before-`/`after-` prefixes so evidence
coexists. `BASELINE=1` records the old Sessions sheet and hovered inline message
actions, rather than asserting the new phone presentation.
`PICKER_DIAGNOSTICS=1` additionally saves desktop message/action bounds,
hover/display/opacity JSON and phone Ask focus/DOM evidence. Assertions do not
change; hover screenshots always await animation completion before capture.
Each engine normally reuses one browser process with a fresh context per
case. To investigate a runtime trap, select one viewport/theme/motion case
per invocation for a fresh browser process. Preserve the original failed
run separately from any isolated-case cohort; a passing rerun does not make
the shared-process run a clean pass or establish the cause of a WASM trap.

The fixed-build checks require the named list opener and `Sessions` dialog,
right-side shared `phone-drawer`, no top X/handle, keyboard-bounded geometry,
an exactly 44px header row with actual 44px Ask/bell/row-close touch targets,
and centered 20px visual Ask/bell pills (also under coarse-pointer CSS),
no Search autofocus, native inert background, a complete forward/backward Tab
cycle, and focus return after Escape, scrim and swipe close. Search keeps local
title results available while the fake saved-history response is held, uses a
200ms debounce, blurs on Enter without losing the query, separates open
sessions/open pages/history, rejects a late older response and retries an
explicit failed request. A dedicated opt-in recent-session seed leaves the
shared fixture's defaults unchanged: empty search must order Open sessions →
Other open pages → Recently viewed; typing must put all matching session
groups (including saved history) before matching pages. Real saved hits retain highlighted snippets,
surrounding context, entry-kind and worker metadata. A short local query must
label an on-head session as `head`; a saved on-head hit with an empty worker
name must use the same identity fallback. An `open_in` side-chat
result routes to its parent session, retaining the query, anchor and expansion
chain in the workspace target and URL. Response gates and animation completion
are awaited; there are no sleeps or action retries to hide missed taps.
The nested bell loads two real fake-attention links plus Mark all seen. It
must contain forward/backward Tab, make the parent picker inert, reject focus
back into its Search, and consume the first Escape without closing Sessions.
Escape returns focus to the actual tapped bell and restores only the picker,
not the still-modal app background. The retained Ask button must open its
existing usable assistant form. Resizing an open picker to desktop must close
it and release native inert, without resurrecting the modal on returning to phone.

Chromium CDP supplies **native emulated touch**, including cancellation and
compatibility-click behavior. The fixture retains one CDP session until its
browser context closes: detaching mid-case resets Chromium touch emulation.
Gesture helpers assert coarse pointer capability and nonzero touch points
afterward, so later form checks still exercise the phone policy.
Small/wrong-way/vertical starts and cancellation
must not dismiss; an outward pull reversed to a short release must snap back;
a long right release must close and restore focus. Full alone finger-tracks;
Reduced/Off retain gesture semantics without travel and their phone overlays
open/close instantly (zero duration, not the generic 80ms Reduced token).
Full Chromium also pauses the actual entrance at its `transitionrun` signal,
requires a real percentage `translate`, then uses native touch to pick it up:
35px finger movement must add exactly 35px to the displayed position without
a jump. The shared `motion.cjs --unit` swipe test separately checks percentage
and pixel pickup on both drawer sides at two widths, and ensures an existing
entrance offset does not count toward the finger-release threshold.
WebKit has no equivalent
Playwright native drag API, so its run explicitly reports no swipe proof; it
still runs every other modal, search, keyboard and action-menu assertion.
Keyboard cases synthesize visual-viewport insets; this is not physical-device
keyboard coverage or proof of Android Back-edge gestures.

Own messages expose Edit / Ask in a side chat / Rewind…; agent replies expose
Ask only. Both have always-visible 44px phone more targets and named
`Message actions` dialogs with inert/Tab containment and focus restoration
after Escape, Close and scrim dismissal. Own metadata has its own 44px row
below the full-width bubble; the agent target stays in its reserved text
gutter. Opening the portalled sheet must retain the row and its exact layout,
and must not send, rewind or branch. Edit and Rewind open their existing
confirmation panels, leave file restoration unchecked, and are cancelled
without mutations. Own and agent Ask forms each submit a real fake-API
`side-chats` request with the selected anchor; their existing autofocus must
reach the textarea before any test `fill()` or focus action. A controlled JSON error
acknowledges that route without inventing a running side chat. Desktop keeps
its existing inline Edit/Rewind/Ask actions and has no phone more button or
Sessions drawer. Browser crashes, UI error reports, external requests, and
unexpected settings mutations fail the run.

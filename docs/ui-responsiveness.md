# UI responsiveness investigation — November 6, 2026

Branch: `c6/p2-responsiveness`, explicitly based on `c6/next` at `d6f9787b`.
The two changes are limited to browser rendering configuration and live
connection-state publication. No icons, motion policy, drawer, Sessions sheet
header, model picker, head code, or database migrations are changed.

## Findings and decisions

1. **Remove forced `erase_components`; retain opt-z.** With the locked renderer,
   component erasure makes routine live updates clear/rebuild dynamic attributes
   and SVG inner HTML. A same-source, one-factor comparison consistently removed
   about 90% of snapshot mutation records. Changing LLVM opt-z to s or 2 alone
   did not remove this work. Keep fat LTO, one codegen unit, aborting browser
   panics, and wasm-opt z. Native release settings are unchanged.
2. **Publish connection state only on transitions.** Same-value `connected=true`
   writes on ordinary WebSocket frames unnecessarily invalidated resources that
   depend on reconnects. Keep first-connect, disconnect and real-reconnect
   notifications; retain all workers/sessions/attention/document publications.
3. **Do not change compression or SQLite code on this evidence.** The backend
   has an expensive recurring embedding-discovery scan, but it is not a newly
   introduced query, nor a demonstrated lock dependency of ordinary Send.

This fixes a demonstrated source of main-thread contention; it is not a claim
that every Mac/Android keystroke or every slow production write is explained.

## Method and provenance

Production distributions were served behind private loopback HTTP/WebSocket
mocks with no real head, credentials, provider calls, or production writes.
Chromium used Playwright-core 1.50.1; Node was 24.16.0. Machine: Linux x86-64,
Ryzen 9 9950X, 32 logical CPUs, approximately 91 GiB RAM. Builds had independent
target directories. This was a **shared, heavily loaded machine**, with load
varying from 17 to over 120. Use adjacent pairs and structural work counts;
small wall-time differences are not reliable speed rankings. Chromium 4x CPU
throttling is an emulation, not a calibrated Mac or Android measurement.

Measured revisions: `9b93195f` (older baseline), `a187e9a4` (motion baseline),
`d6f9787b` (preview), plus same-source compiler/erasure controls. Preview and
opt-s distributions were reused from the prior isolated opt-level experiment;
their hashes were verified. Other variants were built separately. A source
manifest verifies 747 tracked files in the opt-z/no-erasure control match
`d6f9787b`; only `.cargo/config.toml` differs.

Artifacts are under `/tmp/p2-responsiveness/`; builds are under
`/home/zheilbron/.local/state/c6w/p2-responsiveness-builds/`. These local harnesses
record their absolute tool/build paths; they are not installed test dependencies.

## Periodic work on a large document

Final document fixture (protocol 3): 48 blocks; eight decoded SVG diagram images
with 20 nodes each; 24 threads with five long comments each; open comments panel;
256 sessions, only ten busy sessions changing sequence/timestamp metadata; one
document tile; 1440×1000 viewport; Motion Off. Workers/Sessions/unchanged Marks
snapshot bundles arrive every two seconds. No transcript or document-change
events are required to reproduce the bursts. The harness asserts all eight
diagram images have decoded before measuring.

Three adjacent opt-z/no-erasure versus exact-preview pairs, 4x CPU, no sampling
profiler or tracing during timing. ScriptDuration is measured in 700 ms windows
following dispatch, not as a complete interaction-to-paint measurement:

| Build | Mean script ms/snapshot, rounds 1 / 2 / 3 | Median across nine windows | Mutation records across three snapshots |
|---|---:|---:|---:|
| Preview: z + erasure | 469 / 562 / 184 | 442 ms | 50,116–50,182 |
| Same source: z, typed | 142 / 168 / 129 | 157 ms | 4,888 |

Typed rendering was faster in each pair. Host contention, GC, and fixed windows
make an exact speedup claim inappropriate. The roughly **90.3% lower mutation
count** is consistent. Quiet controls have no document mutations and no repeating
long-task loop. Sampled rail and document nodes remain connected: this is broad
attribute/view-update churn, **not replacement of every DOM node**.

Supplementary opt-2/no-erasure measurements also produce 4,888 records, with
mean script windows of 17.7 ms at 1x and 132 ms at 4x. Erased z/s/2 all retain
approximately 50,182 records. These are separate runs, not paired proof that
opt-2 is faster. Its much larger download is not justified by these timings.

The final combined production build was then checked against preview in two
additional rounds at each CPU setting, on the decoded-diagram fixture:

| CPU | Preview mean script ms/window, rounds 1 / 2 | Final mean script ms/window, rounds 1 / 2 |
|---|---:|---:|
| 1x | 39.2 / 37.0 | 16.9 / 16.7 |
| 4x | 181.9 / 170.5 | 66.1 / 72.2 |

Each final round records 4,888 mutations versus preview's 50,182, and 27 HTTP
requests versus 36 per three snapshots. All application-error arrays are empty.
These final runs validate the combined changes; the earlier one-factor control
isolates the erasure cause.

### Mechanism and trigger isolation

In Tachys 0.2.19, `src/html/attribute/any_attribute.rs:375–397`, the erased
`Vec<AnyAttribute>::rebuild` takes the old states, removes named attributes,
clears inner HTML, resets properties, and builds the new vector. Typed attribute
rebuilds can avoid doing that work when their values have not changed.

The app's `sessions.rs` SessionsRail derives from the full Session vector and
`grouped()` uses positional `map(row).collect_view()`. Ordinary sequence/time
updates therefore rebuild broad row subtrees. `activity.rs` DockActivity is
another broad positional boundary. `aspects.rs` glyphs and `icons.rs` static SVG
paths amplify the cost under erased attribute rebuilding. **All icons remain
present**; no app view is converted to HTML to reduce the work.

Independent controls (preliminary non-SVG fixture) isolate 42,202 records from
session publication and 7,980 from worker publication, totaling 50,182 per three
bundles. Folding the Sessions rail reduces that total to 2,572 while leaving the
document and Activity section open. Attention alone does not rebuild the rail.
This supports the rail attribution, not a document-resource polling theory.

`docs.rs::reload_on_change` tracks a matching document ID, not every snapshot.
Document/threads/work-of/projects fetches are not triggered by raw session
snapshots. Matching-document events refetch those resources; unrelated document
IDs do not. Presence timers are bounded, viewport observation is event driven,
and staged header reads are one-shot. No autonomous quiet-page full-core loop
was demonstrated.

The named Performance trace contains 1,602 ParseHTML events, named Tachys
attribute-rebuild frames, and removeAttribute/setAttribute/innerHTML work.
It has snapshot markers, CPU-profile chunks, and screenshots. The diagnostic
named build is not used for production timing or size comparisons. Its temporary
DWARF-stripped copy preserves executable/name sections byte for byte.

## Live request invalidation

`api.rs::LiveFeed::connect` previously set connected=true on every valid frame.
Reactive same-value writes notify subscribers, so waiting/descendants/global
attention resources interpreted normal traffic as reconnect invalidation.
The transition-only helper reads untracked and writes only when the boolean
changes. A native reactive test covers first connect, 100 normal live frames,
disconnect, repeated failed retry, and real reconnect.

The connection-only control removes nine attention requests per three ordinary
snapshot bundles. In the transcript fixture, waiting/children/decisions each
drop from roughly five or six reads to three in the short live window; explicit
attention events still invalidate legitimately. This change does not remove
rail mutation amplification, and no standalone CPU speedup is claimed for it.
Other /elsewhere and /decisions/count dependencies and responsive copies remain.

Actual 60-second request counts on the final decoded-diagram document fixture,
one normal-CPU run per historical build (not extrapolated six-second samples):

| Build / phase | Attention | Elsewhere | Decisions/count | Projects | Work | Version | Quotas |
|---|---:|---:|---:|---:|---:|---:|---:|
| 9b93195f quiet | 0 | 2 | 4 | 2 | 6 | 2 | 1 |
| 9b93195f snapshots | 30 | 32 | 64 | 10 | 14 | 2 | 1 |
| a187e9a4 quiet | 0 | 8 | 6 | 3 | 9 | 2 | 1 |
| a187e9a4 snapshots | 90 | 128 | 96 | 15 | 21 | 2 | 1 |
| d6f9787b quiet | 0 | 8 | 6 | 3 | 9 | 2 | 1 |
| d6f9787b snapshots | 90 | 128 | 96 | 15 | 21 | 2 | 1 |
| Final quiet | 0 | 8 | 6 | 3 | 9 | 2 | 1 |
| Final snapshots | 0 | 128 | 96 | 15 | 21 | 2 | 1 |

Quiet does not mean zero requests indefinitely: version/quotas/work timers exist.
Old `9b93195f` does not support Motion Off, so its animated quiet TaskDuration
must not be compared as though it were wasm scripting. Its quiet/snapshot script
totals are 50/585 ms per minute; motion baseline 48/1,919 ms; preview 56/1,540 ms.
Those nonadjacent totals are context, not a compiler speed ranking.

## Transcript interactions

The corrected protocol-3 matrix completed 20 cases without application errors:
older/motion/preview/no-erasure/final × 1x/4x × Full/Off. One heavy parent has
600 turns / 2,401 events, and two busy tiles each have 60 turns / 241 events:
2,883 initial events and 720 tool calls total. Both busy streams receive text
updates every two seconds during the interaction sequence. Close targets the
heavy parent. The final/preview DOM has about 33,254 elements.

All figures below are **single observations**, not percentile guarantees. Send
is event timestamp to first RAF observing empty draft / queued bubble. The
mock deliberately waits 80 ms before publishing the queue. Close/open is event
to the first RAF observing geometry/removal/ghost-opacity change, not actual
presented pixels or animation completion. Scroll uses 24 wheel inputs and
includes automation delivery time. Raw data also retain the no-erasure control.

| Build | Preference | CPU | Draft clear / bubble ms | Close first RAF ms | Open first RAF ms | Scroll long tasks / max ms | Scroll max RAF gap ms |
|---|---|---:|---:|---:|---:|---:|---:|
| 9b93195f | Full (unsupported) | 1x | 11.5 / 98.0 | 77.4 | 31.2 | 0 / 0 | 16.8 |
| a187e9a4 | Full | 1x | 11.8 / 97.0 | 1,773.3 | 41.4 | 0 / 0 | 16.8 |
| Preview | Full | 1x | 11.7 / 94.7 | 124.6 | 17.1 | 0 / 0 | 16.8 |
| Final | Full | 1x | 12.2 / 97.2 | 95.7 | 15.8 | 0 / 0 | 16.8 |
| 9b93195f | Off (unsupported) | 1x | 13.3 / 99.7 | 66.9 | 31.7 | 0 / 0 | 16.8 |
| a187e9a4 | Off | 1x | 13.2 / 99.6 | 220.0 | 46.8 | 0 / 0 | 16.8 |
| Preview | Off | 1x | 14.3 / 99.5 | 65.5 | 18.4 | 0 / 0 | 16.8 |
| Final | Off | 1x | 16.5 / 115.3 | 89.0 | 19.2 | 0 / 0 | 16.8 |
| 9b93195f | Full (unsupported) | 4x | 43.9 / 132.0 | 303.9 | 198.4 | 0 / 0 | 50.0 |
| a187e9a4 | Full | 4x | 99.4 / 194.1 | 10,888.9 | 151.9 | 4 / 70 | 83.4 |
| Preview | Full | 4x | 47.3 / 134.1 | 494.1 | 67.3 | 3 / 63 | 83.3 |
| Final | Full | 4x | 31.3 / 128.4 | 422.1 | 64.4 | 5 / 84 | 100.0 |
| 9b93195f | Off (unsupported) | 4x | 38.9 / 131.4 | 278.1 | 96.1 | 0 / 0 | 33.4 |
| a187e9a4 | Off | 4x | 47.2 / 146.9 | 927.8 | 187.6 | 1 / 50 | 33.4 |
| Preview | Off | 4x | 44.7 / 132.9 | 340.4 | 90.6 | 10 / 96 | 100.1 |
| Final | Off | 4x | 30.6 / 140.9 | 237.9 | 66.3 | 0 / 0 | 66.7 |

This matrix **does not establish uniformly faster actions or scrolling**: normal
CPU Off observations even vary in the other direction, and 4x Full still has
long scroll tasks. It does verify draft clearing does not wait for the mocked
ack. The heavy close remains nontrivial; the motion baseline's multi-second
outlier is preserved rather than discarded. The settings fix is justified by
the repeated, independently isolated document/live-rail work, not a claim that
all interaction numbers improved. See `interaction-results-v3.md`,
`runs-v3.jsonl`, and `interaction-phases-v3.jsonl` for all cases and phase work.

A separate instrumented repeat of the worst historical case is saved as
`trace-v3-motion-full-4x-0-close-live.json`: 32,333 events, named JS/native frames,
525 CPU-profile chunks and 450 screenshots. The click runs through
`tileMotionPrepare → tileMotionGhost`; its inclusive sampled stack is about
10,988 ms, with 655 Layout events totaling about 6,979 ms. These categories
overlap and must not be added. `cloneNode` itself samples only about 68 ms;
calling the entire delay “DOM cloning” would be inaccurate. The exit-picture
path appends a ghost and restores scrollers, doing substantial nested layout
work in this historical build. It is **not a final-build trace**, nor proof of
a newly introduced preview regression. No additional motion change is made
from that historical result; current heavy-close cost remains a limitation.

## Download budget

Exact bytes of the built primary wasm; Brotli uses the build's q11 static
precompression (unrelated to dynamic JSON compression):

| Build | Raw bytes | Brotli bytes |
|---|---:|---:|
| Older baseline 9b93195f | 19,241,224 | 3,644,967 |
| Motion baseline a187e9a4 | 19,813,576 | 3,734,990 |
| Preview d6f9787b, z + erasure | 7,582,584 | 1,834,487 |
| Same-source s + erasure | 10,016,083 | 2,181,061 |
| Same-source 2 + erasure | 15,410,034 | 2,624,044 |
| Same-source z, typed | 10,412,306 | 2,589,947 |
| Same-source 2, typed | 23,475,740 | 3,918,122 |
| Final z, typed + connection guard | 10,412,407 | 2,589,638 |

Final versus preview: **+2,829,823 raw bytes (+37.3%) and +755,151 Brotli bytes
(+41.2%)**. This is an explicit responsiveness/download tradeoff; cold download
and compilation may be worse. The size ratchet keeps its 3% tolerance and now
uses the measured final bytes. The 8 MB raw / 2 MB Brotli targets remain goals,
not grounds to restore a demonstrated runtime regression.

Final wasm SHA256:
`209975722177a57552d9dc439890f87bba6d813f6648b7ce35f3a5acf43349cc`.

## Backend evidence, kept separate

The user supplied production GET TTFB of 20–72 ms, 657 missing-vector queries
among 669 slow-SQL warnings (mean logged 1.84 s), and one 13.04 s transaction
start. These are not measurements against production by this investigation.

Locked tower-http 0.7.1 dynamic Brotli defaults to **quality 4**, not 11.
An isolated benchmark of the production MIME predicate measured median 8.7 µs
for a short send acknowledgement, 127 µs for 52 KB JSON, and 1.30 ms for 423 KB.
Default and explicit q4 produced identical bytes. Compressing tiny acks is not
ideal in principle, but this is not evidence for a compression fix here.

The missing-vector anti-join in `c6-memex/src/store.rs` scans already-indexed
passages: LIMIT 32 bounds returned work, not examined rows. An 800k-passage,
all-indexed scratch fixture took 721 ms median; oldest 32 missing took 677 ms,
newest 32 missing 0.176 ms. Adding the already-covered vector key did not help.
Empty discovery sleeps two seconds and repeats. Query/polling are unchanged
since `9b93195f`; migration 36 only adds archive metadata and an active-doc index.

Ordinary Send inserts into **c6h.db**, separate from memex databases. Same-memex
BEGIN during the scan stayed below 0.5 ms in the WAL fixture. A send-shaped
synthetic POST measured 26.6 ms idle versus 29.4 ms under continuous memex scans;
a held head writer delayed it, while a held memex writer did not. This is not
a full-head integration benchmark and does not explain the production BEGIN.

The composer clears its draft before spawning the asynchronous send. Visible
queue text comes from WebSocket publication, not the returned HTTP ack. Ordinary
Send does not wait for worker/model generation; `@memex` expansion is a distinct
additional dependency. Do not infer a browser stall from ack timing alone.

Before a backend fix, identify the slow transaction's database/realm, outcome
and overlapping writer, and inspect discovery `rows_returned`. Repeated empty
full scans warrant a separate pending-work/backoff design, but no speculative
index, transaction or poll-loop change is included here.

## Verification, artifacts and limitations

- `env -u NO_COLOR make check`: formatting, native/all-target and wasm clippy,
  test lint, and the built size ratchet pass without warnings.
- `env -u NO_COLOR make ui`: production build and static precompression pass.
- `env -u NO_COLOR make test`: 1,084 workspace tests pass, two intentionally
  skipped; the two slow integration tests also pass. No failed-test rerun was
  necessary.
- Motion JS unit suite: 9/9 pass. Production Chromium tiles/sendStop checks pass
  in four desktop/phone Full/Off contexts (tiles are desktop-only in that suite).

Key evidence: `doc-results.jsonl`, `doc-final-noerase-pairs.log`,
`doc-final-combined.log`, `doc-minute-counts.log`, `doc-final-minute-counts.log`,
`doc-builds.jsonl`, `doc-z-noerase-source-proof.json`, and
`doc-trace-named-trace-256s-4x-r1-sessions.json`. Backend details and reproducible
scratch benchmarks are in `compression-audit.md` and `memex-audit.md`.

Two harness corrections are retained, not silently discarded. Early document
protocols 1/2 put raw SVG in sanitized text, so **they had no decoded diagrams**;
use them only for rail/compiler/trigger controls. Final protocol 3 asserts real
diagram embeds. Initial transcript interactions scanned all descendant geometry,
forcing off-screen content materialization; that entire interaction cohort is
excluded. Its earlier quiet/live windows and the separate document harness do
not perform that scan. A fresh protocol uses the production ScrollBox selector
and narrow CSS button selectors rather than global visibility/accessibility scans.

No physical Android/Mac browser, thermal/battery behavior, cold-network budget,
or production write trace was validated. Event-to-next-rAF style/geometry checks
are not filmstrip presentation or field INP. Remaining broad positional rail
updates should eventually be presentation-gated or keyed with reactive row data;
keying a row by ID alone would leave its plain Session props stale. That larger
refactor is deliberately not mixed into this demonstrated settings regression.

## Q3: fixed-arc working ring and new transcript items — November 6, 2026

Branch `c6/q3-ring-render`, explicitly based on `c6/next` at `8d0079b9`, not
upstream/main. This follow-up changes only the shared desktop SessionRing and
its CSS, plus tests/documentation. No transcript algorithm, icons, header,
image preview, settings, dependencies, size budgets or migrations change.

### Change and hypothesis results

- **Fixed arc, whole-SVG rotation.** The 24px outer-rim SVG rotates as one
  composited piece at the existing 1.4s period. Its arc is fixed at `20 46`;
  `c6-ring-arc` and its per-frame dash morph are removed. The independently
  named context SVG, failed cross, badge and all state colors remain. Reduced
  and Off retain the original stationary full working rim and release the
  working layer's `will-change` hint. The browser checks an active accelerated
  transform layer on this exact SVG, not merely a transform in its stylesheet.
- **A was only partly supported.** Baseline rotation already continues during
  a blocked main thread in both installed Chromium versions. The baseline's
  dash morph does repeatedly require Paint. The demonstrated improvement is
  removal of this paint-bound animation, **not newly enabling baseline angular
  motion, eliminating all frame drops, or a measured global FPS increase**.
- **B did not reproduce.** The original rim/context nodes and running spin
  animation survive new items, changing session snapshots, and
  Starting-to-Running updates. The new implementation retains them too. There
  is no speculative remount fix.
- **C remains a measured no-fix.** Large items miss frame budgets, but no
  specific small transcript change is justified by the profile below. Other
  `.animate-spin` sites already rotate HTML boxes around their icons; they do
  not share this inner-SVG/dash-morph structure and are unchanged.

`SessionRing` is shared by the workspace session controls; the separate plain
phone context ring is unchanged. All states are captured in light/dark, narrow
tile controls are checked down to 240px, and phone Full/Reduced/Off controls are
exercised without changing their design.

### Production measurement

The standalone `tests/ring-render.cjs` serves the real `make ui` WASM/CSS behind
an owned loopback fake head: three populated tiles, 93 historical replies, eight
short streamed chunks before each of twelve new assistant Markdown items.
Ordinary items have twelve paragraphs (about 1.7KB); the stress variant has
180 (about 23KB). Both include formatting, links, code and a table. Browser:
Chromium 158.0.8127.18 on shared Linux x86-64, 1600×1000, 4× CPU throttle, both
themes. Chromium 133.0.6943.16 independently checks compositor/retention
behavior. Neither is a physical Mac/Chrome measurement.

Each canonical case has one traced/profiled run and two unprofiled timing runs.
The following DOM medians pool 24 item arrivals from the two **unprofiled**
runs; frame gaps/estimated missed 60Hz rAF slots remain per-run observations,
not pass/fail limits or ring-specific compositor FPS:

| Workload / theme | DOM median before → after | Largest rAF gap before → after | Missed rAF slots before → after |
|---|---:|---:|---:|
| Ordinary / light | 4.75 → 4.50 ms | 16.8 → 16.8 ms | 0, 0 → 0, 0 |
| Ordinary / dark | 4.55 → 4.00 ms | 16.8 → 16.8 ms | 0, 0 → 0, 0 |
| Large item / light | 11.35 → 11.10 ms | 33.5 → 33.4 ms | 5, 11 → 7, 5 |
| Large item / dark | 11.35 → 11.35 ms | 33.4 → 33.4 ms | 12, 9 → 10, 8 |

There are **zero >50ms long tasks** in these natural-workload cases, before or
after. This does not mean all items fit a 16.7ms frame: main-thread rendering
can miss that budget without qualifying as a Long Task. Small timing movements
on this shared machine are not evidence of a transcript speedup.

In each traced natural window, ring-span Paint records fall from **217 to 0**
for ordinary items (each theme), and **228/230 to 0** for large items
(light/dark). Large-item partial-update `DroppedFrame` trace records are
12/12 before versus 17/16 after, while compositor DrawFrames continue in all
cases. These records must not be presented as an FPS win or as proof that the
ring stopped moving. A separate 600ms deliberately blocked-main-thread
screencast has distinct moving blue-ring pixels in every captured frame before
and after (34–36 frames); this challenge is outside all natural timing windows.

### What one new item costs

Single **traced** trials, median receipt-to-completed-main-thread-paint time:

| Workload | Light before → after | Dark before → after |
|---|---:|---:|
| Ordinary | 17.49 → 19.33 ms | 20.13 → 17.88 ms |
| Large item | 39.33 → 39.77 ms | 40.93 → 39.99 ms |

Receipt is the harness marker after its JSON parse, not network/provider
latency. Completed paint is main-thread paint recording, not physical display
presentation. The earlier next-rAF marker (about 14ms for large items) misses
most subsequent rendering work and must not be called render completion.

Downstream presentation timing is unresolved, not omitted: the canonical traced
large-item feedback medians are 43.47/45.39ms before versus 53.54/56.64ms after
(light/dark), despite nearly unchanged main-thread paint and activated-draw
times. Three additional adjacent light-theme pairs under greater host load
reverse that direction: feedback medians 74.86/75.07/61.18ms before versus
58.70/65.41/53.74ms after. Those later traced runs also record 2/2/2 natural
long tasks before and 3/1/0 after; they are not the unprofiled trials above.
The analysis cross-checks activated surface IDs through Viz aggregation and
buffer-swap completion: feedback tracks a downstream display wait, not merely
late callback delivery. Its scheduling cause and physical-pixel relevance are
not established. Ordered pairs on a shared host are not randomized causal
evidence; **neither a presentation-latency improvement nor a confirmed
regression is claimed**. Preserve these adverse and opposite-direction results
in `paired-summary.json`/`.md` for a controlled device follow-up.

For a large baseline item, mean work through completed paint is approximately
9.6–9.8ms exclusive FunctionCall work, 2.3–2.4ms ParseHTML, 9.3–9.9ms style,
8.6–8.7ms layout, 3.5–4.0ms prepaint, and 3.6ms paint. Nested durations are
interval-unioned rather than double-counted. After numbers are very similar.
The largest Layout records dirty 2,206 objects and take about 8ms. The main
increment is browser style/layout of the large new Markdown subtree, not an
attributed expensive Rust function.

Committed Markdown insertion is `session.rs:3827–3835`; complete Markdown and
mention serialization is `markdown.rs:117–180`. Existing keyed event rows are
retained, with per-row `content-visibility:auto`. Anonymous production WASM
indices are not assigned names from an unrelated diagnostic build. Consequently
this profile does not establish event/result cloning or Markdown parsing as the
dominant scripting cause, and does not justify an incremental-log/cache rewrite.

There is a concrete, smaller forced-layout edge: scroll dispatch calls a
wasm-bindgen `clientHeight` import, matching ScrollBox's entry visibility read
at `workspace.rs:2187–2193`, followed by follow/end/far bookkeeping. Forced
layout inside each item's receipt-to-paint interval averages only 0.62–1.09ms;
it is not the dominant large-item cost. Deferring that first visibility read or
changing follow scheduling has user-intent/scroll-restoration risks. A separate
one-factor experiment with retained follow/selection/anchor tests is the next
option, not an unmeasured fix here.

The inherited motion harness calls `document.getAnimations()` on every rAF;
its own forced rendering is attributed separately. Trace/profile overhead and
headless presentation feedback are not field INP. Trace counts are bounded by
`q3-start`/`q3-end`, excluding stale initial layer-tree events and the later
deliberate block. rAF intervals spanning the start boundary are excluded too.

### Size, verification and evidence

| Production WASM | Before | After | Delta |
|---|---:|---:|---:|
| Raw | 10,513,126 | 10,514,419 | +1,293 bytes |
| Brotli | 2,613,011 | 2,613,554 | +543 bytes |

The accepted size ceilings are unchanged; both builds pass `scripts/ui-size.sh`.
The fixed-arc and motion overrides are verified through computed production CSS,
and the animating SVG has a 24×24 compositor layer with the active-transform
reason. All eight native ring tests and eleven motion JS unit tests pass.
`env -u NO_COLOR make ui`, `make check`, and `make test` pass: **1,113 workspace
tests pass, two intentionally skipped**. The two slow tests finish successfully
(132s fixture upgrade and 339s Slack integration); no failed native-test rerun
is needed. Logs are in `/tmp/q3-ring/`, with the file limit raised to 16,384.

The dedicated ring regression passes both themes in Chromium 133 and 158,
including startup state changes, streamed text visibility, all state looks,
node/animation identity and Reduced/Off. Narrow tile-marks checks pass in both
themes. The existing sheet/sendStop matrix passes all twelve Chromium contexts.
Shared-process WebKit passes five desktop contexts before an out-of-bounds
hydration crash on desktop/dark/Off; that case passes alone and all twelve
WebKit contexts pass with isolated browser processes. The shared-process failure
is retained, not silently replaced by isolated results. This is historical
WebKit 2123 evidence; S2 below replaces the test dependency with WebKit 2388
after an alternating same-WASM cold-load comparison, without retries.

An extra `ONLY=tool` check fails because its saved `__pre` is null. It fails
again alone and identically against the pinned baseline distribution. This
pre-existing tool-output harness failure is not fixed or weakened in this change.

Evidence is under `/tmp/q3-ring/`: pinned `before-dist`/`after-dist`, build and
size logs, SHA-256 manifests, `comparison.json`, `comparison-attribution.json`
and its Markdown/analysis script, raw traces/CPU profiles, screenshots and
`blocked-ring-pixels.json` with cropped-ring hashes/blue-arc angles. A separate
set of adjacent traced pairs investigates presentation-feedback uncertainty;
these must not be mixed into the unprofiled comparison above.

## Q3 follow-up: restore breathing with transforms — November 6, 2026

The fixed arc above removed paint work, but those measurements did **not**
establish that the old breathing arc slowed the application. This follow-up
preserves its appearance instead of treating repainting alone as a reason to
remove it. The baseline for this change is the fixed-arc commit `348fd67b` on
`c6/q3-ring-render`; the original dash morph at `8d0079b9` remains a separate
visual reference and positive Paint control. No transcript code changes.

### Small transform-only construction

The radius is 10.5, so 8–30 SVG units span approximately 43.65–163.70 degrees,
always less than a semicircle. A single lower semicircle rotates from
−136.35 to −16.3 degrees behind a lower-half clip fixed within the spin wrapper.
Their intersection is exactly the desired clockwise arc starting at three o'clock. An outer HTML
wrapper supplies the original 1.4s linear spin; the **whole outer SVG**, not its
path, supplies the original 2.8s, per-half-cycle `ease-in-out` sweep. Both
animations change only `transform`, with independently verified active-transform
compositor layers. There are no joined half-rings, extra caps, masks, animated
dash attributes, JavaScript animation loops, or new dependencies.

This adds four decorative DOM nodes (two spans, one SVG, one path). The existing
rim remains for Idle/Waiting/Failed and the stationary full Working rim under
Reduced/Off. Context, accessible descriptions, failed cross and background badge
stay independent. Reduced/Off hide the new overlay, stop both animations and
release both layer hints. Ordinary live snapshots, new items and
Starting-to-Running updates retain both moving nodes and both CSS Animation
objects. The separate plain phone context ring is still unchanged.

### Verification and limits

`tests/ring-render.cjs` now checks both layers, transform-only keyframes, exact
periods/easing/offsets, clip/SVG geometry, stationary context, retained animation
identities, state cues and Full/Reduced/Off restoration. Paint attribution
includes the exact 24px ring parent and its subtree: the original dash morph
paints that parent span. Broader document/tile/button paints are not counted as
ring-specific paints. A fresh original-implementation positive control records
**260 ring Paints**, so a zero result is not merely an empty attribution filter.

The same production fixture uses three tiles, 93 historical replies, eight
streamed chunks before each of twelve ordinary or large new Markdown items,
4× CPU throttle, both themes, and DSF 1/2. Chromium 158.0.8127.18 checks ordinary
and 180-paragraph workloads; Chromium 133.0.6943.16 independently checks ordinary
workloads at both scales. The saved three-run series still separates traced
run 0 from unprofiled runs 1/2. Additional final single-run cases exercise the
completed regression, including its stricter ring-parent attribution.

Final Chromium 158 results (each paired value is **light / dark**):

| Workload / scale | Natural ring Paints | Interior blocked frames | Blue area, last third / first third |
|---|---:|---:|---:|
| Ordinary / DSF 1 | 0 / 0 | 29 / 30 | 1.635× / 1.665× |
| Ordinary / DSF 2 | 0 / 0 | 30 / 30 | 1.667× / 1.696× |
| Large items / DSF 1 | 0 / 0 | 30 / 30 | 1.640× / 1.689× |
| Large items / DSF 2 | 0 / 0 | 30 / 30 | 1.640× / 1.689× |

The fixed-arc before cases also have **zero** ring Paints at both scales/themes,
but blue area stays approximately constant (0.969–0.975×) during the block.
Chromium 133's four theme/scale cases likewise have **zero** ring Paints and
30 interior frames each, with 1.633–1.668× blue-area growth. Both moving boxes
have active-transform compositor layers, all retained-node/animation checks
pass, and candidate centroid displacement is 138.0–143.8 degrees. The original
dash-morph control has 0.972× blue area during the block: its rotation already
continues, but its breathing does not.

The separate 600ms blocked-main-thread diagnostic seeks the sweep early in its
growth phase, then commits that phase before blocking. It saves interior
compositor frames, rather than relying on rAF, and measures blue area separately
from centroid movement. A spinning fixed arc cannot pass the breathing check.
Centroid displacement includes half the arc-angle change and is **not** reported
as a measurement of pure spin. CDP can output CSS-pixel screencasts even at
DSF 2; cropping uses its own captured-viewport metadata, not layout height or an
assumed device-pixel ratio. The separate DSF-2 visual PNGs are truly 3200×2000
for a 1600×1000 viewport.

Paused comparisons use the actual production drawing beside an independently
animated reproduction of the original dashed circle, at nine times from 0 to
2800ms, including growth, shrink and cycle wrap. Each theme/scale has 81 working
comparisons: 16/20/24px at whole, quarter and half-pixel placement. Arc lengths
match the original easing to under 0.001 SVG unit. Weighted blue-ink checks avoid
treating a barely antialiased endpoint pixel as a whole pixel at 16px. Angular
occupancy checks find no detached piece or interior gap at their two-physical-
pixel endpoint tolerance. All-state galleries also cover each size under Full,
Reduced and Off. There is no internal join at which two separately rasterized
pieces could open a seam.

This establishes the requested paint/compositor and bounded raster invariants,
not a global FPS or transcript-latency improvement. Shared-host timing remains
variable: one saved heavy unprofiled run has an 849.9ms main-thread rAF gap and
34 long tasks. It is retained, not silently discarded or called a ring-induced
slowdown. The original rotation already continued during a blocked main thread;
the new result is that **arc length changes there too**, without ring paints.
No physical Retina Mac, Android device, thermal/battery test or field INP was
validated. Native Chromium headless evidence is not a substitute for those.

`env -u NO_COLOR make ui`, `make check`, and `make test` pass. The native suite
reports **1,113 passed, two intentionally skipped**; the eight native ring tests
pass. The production WASM is **10,515,173 raw / 2,614,072 Brotli bytes**, a delta
of **+754 / +518 bytes** from the fixed-arc baseline. The size ratchet and
8MB/2MB goals are unchanged. SHA-256:
`ec22ef43850575d387e4427743d806159336f8619b294f7a7a2b5fb69b5e9299`.

Follow-up evidence stays under `/tmp/q3-ring/breath/`: pinned `fixed-dist` and
`breath-dist`, build/check/test logs, JSON, traces/profiles, before/after blocked
frames, original positive-control frames, nine-phase reference comparisons and
all-state galleries. `frames.html` is a self-contained before/after frame viewer;
`final-summary.json` and `final-summary.md` contain the complete per-case results.
Historical measurements above are not rewritten to imply
that the fixed arc proved an overall speedup.

## N5: browser traps and disposed values — November 7, 2026

Investigation base: `c6/next` at `235fae5c`, not `upstream/main`. Evidence,
commands, pinned artifacts, and read-only ops questions are under
`/tmp/n5-webkit/`. This investigation does not change production optimization,
panic strategy, or the WASM size ratchet; a clean rerun is not evidence that an
intermittent crash has been repaired. The later S2 test-engine comparison below
supersedes treating the old WebKit 2123 trap as a current accepted test limitation.

### Keep the two fault classes separate

The older symbolized `/tmp/q10-symbols-browser.log` contains a **Chromium**
disposed `StoredValue<String>` read in `SessionChrome::pane_open`, called from
tile-header render effects after the transcript owner was replaced. This is
an application lifetime error, not a WebKit-specific diagnosis. Commit
`ef4d715b`, already an ancestor of the investigation base, makes that read
fallible, keys header controls by chrome registration token, and prevents old
transcript cleanup from removing its replacement's registration. The composer
commands resource already captures an owned session ID before its deferred
future (`16c8f5b3`, also an ancestor). Neither old expression is present at the
requested base. No additional fallible getters are added speculatively.

The separately recorded November 6 DecisionLink stack maps a disposed resource
read to `count.get()` in `decisions.rs`. It identifies the failing renderer,
**not who disposed its owner**. Its old optimized artifact is not interchangeable
with today's WASM function indices. Duplicate layout normalization operates on
plain layout data before creating reactive owners; a variable called `owners`
in that normalization is not a collection of Leptos owners. Runtime target
navigation and tile closure remain necessary lifetime regressions.

An out-of-bounds trap occurs *before* the later disposed-value report in
`/tmp/q10-workspace-verified-webkit-desktop-linux.log`. Subsequent failures on
an already broken page cannot establish a shared cause. Conversely, a Rust
panic followed by `unreachable` is the expected abort path and must not be
counted as an unexplained engine trap. A validator accepts WASM's structural
and type rules, not the runtime correctness of its pointers or application
lifetimes; validation alone cannot prove an engine defect.

### Diagnostics, without conversation contents

The existing crash report's stack field now includes a synchronous JS/WASM
stack for Rust panics, a bounded snapshot of actually mounted view kinds and
numeric tile/pane keys, and the browser user agent. The marker shares the
keyed content's mount lifetime and reuses its existing wrapper: no extra DOM
node, title, session/document ID, saved-layout read, or message text. Hidden
retained content remains mounted; a deferred shell is `pending`. The first
report still wins over later fallout, and the reload banner works without
Leptos or successful reporting. No telemetry endpoint or server behavior is
changed. Browser tests **still fail** on every WebKit trap; their diagnostic now
references the old/new engine comparison below. All existing assertions remain
in force, and panic evidence is still distinguished from a subsequent abort.

The live head was not accessed. Before this patch the endpoint logs only
`page`, `message`, and `stack`, not the User-Agent header. Ask ops for the
deployed asset hash, timestamped first reports, and any existing access-log
metadata that can distinguish real Safari from synthetic WPE's Safari-shaped
UA; absent metadata is not evidence of absent Safari crashes. Detailed,
privacy-preserving read-only requests are in `ops-questions.md` in the evidence
directory. No operational action has been taken.

## S2: pinned browser-test dependency — November 7, 2026

S2 branches from `c6/next` at `5384d8e2`. The approved N7 experiment alternated the same
production WASM for 60 rounds per arm: Playwright-core 1.50.1 / WebKit 2123
failed 12 of 231 loads (10 out-of-bounds, one unreachable, one disposed-value
panic); Playwright-core 1.65.0 / WebKit 2388 failed 0 of 240 (Fisher p≈0.0002).
Commands are preserved at `/tmp/n7-webkit/run.sh` and `repeat-any.cjs`, with raw
results in `/tmp/n5-webkit/n7/`. This is evidence to replace the old test engine,
not source-level proof of a specific WebKit defect or coverage of real Safari.

Browser tests now resolve a test-local npm lockfile pinning Playwright-core
1.65.0, with browsers and OS dependencies installed by that package's own CLI.
On Linux it pins WebKit 2388 (27.2) and Chromium/headless-shell 1254
(158.0.8127.18). The old ad hoc `/tmp` wrappers, binaries and extracted libraries
are no longer defaults. Explicit binary/core/library overrides remain for
controlled comparisons, not normal qualification. Production Rust, WASM,
optimization, panic behavior, settings and services are unchanged. No runtime
retry, expected trap or relaxed assertion was added. See `docs/testing.md` for
installation, repeated suite results, remaining blockers, and CI prerequisites.

`ring-render.cjs` is still Chromium-only: compositor layers, exact paint records
and the blocked-main-thread screencast require CDP. It now rejects a WebKit
engine request instead of silently counting a Chromium launch as WebKit proof.
The matching Chromium is tested as well; old Chromium 133 profiling results
above remain historical measurements, not performance claims for version 158.

S2 does **not** claim a fully green combined browser qualification. The required
UI build, check and native suite pass (1,167 tests, two skipped), but Motion
records separate New Session fields-panel dismissal/list-reopen/effort-selection
failures: all three unfiltered invocations fail in each engine. The other four
cross-engine suites pass three full invocations each, as does Chromium's ring
suite. **Zero WASM traps were observed**, including in the failed cohorts.
Native event tracing hits the intended offered-model button before the panel
becomes hidden; the same panel-open invariant fails in an old Playwright
1.50.1/Chromium 133 control against the same WASM. Isolated modelForms reruns of
all three failed WebKit cases pass without erasing their full-run failures. The test now
explicitly requires the fields to stay expanded after model/effort selection;
it does not reopen them, use hidden controls, force clicks, or retry. No
production fix is made speculatively. Existing CI remains unchanged rather
than adding a required job known to fail. Full cohort counts and evidence
locations are recorded in `docs/testing.md`.

## S1: typed icon boundaries and WASM headroom — November 7, 2026

Size work starts at `c6/next` **5384d8e2**, not main. A names-kept
wasm-bindgen/Binaryen build and twiggy top/dominators/monos identify large
Settings/workspace render functions and generic component/drop machinery.
Diagnostic names are excluded from production download measurements.

Icons now return `AnyView` at the shared SVG boundary; callers use it directly
instead of boxing it again. The SVG's attributes remain a **typed tuple**.
This does not enable `erase_components` or its erased-attribute rebuild path:
same-type updates still delegate to the typed SVG diff. All 108 vendored
icons, three sizes, strokes, fills, accessible attributes and markup helpers
are unchanged. No new CSS classes, dependency changes or browser SSR calls.

Production `make ui` measurements, with the existing Brotli quality-11 hook:

| Build | Raw bytes | Brotli bytes | Saving, raw / Brotli |
|---|---:|---:|---:|
| 5384d8e2 baseline | 10,713,719 | 2,665,834 | — |
| Shared typed icon boundary | 10,692,720 | 2,656,541 | 20,999 / 9,293 |
| Shared response-decoding futures | 10,676,549 | 2,637,469 | 16,171 / 19,072 |

The size ceilings stay **10,724,780 raw / 2,667,328 Brotli**. These savings
leave 29,859 Brotli bytes of room, so more savings are needed.
The existing native icon tests cover every path at every size; production
browser checks include the component gallery in both themes, desktop/phone
and Chromium/WebKit, plus live snapshot/transcript and retained-node checks
in `tests/ring-render.cjs`. Size reduction is not a claim of faster rendering.

Response decoding and empty-response checking now return boxed local futures
from non-inlined factories. This shares their poll machinery instead of
embedding it into every endpoint/resource future. Each decode adds one small
allocation, not a task or scheduler yield. The original lazy bodies, gloo JSON
decoder, status/error handling, 401 sign-out and drop/cancellation behavior
are retained; request serialization still happens inside the original async
wrapper. `reply-recovery.cjs --responses` checks malformed JSON, structured and
fallback errors, body-read rejection and post-mount sign-out in real browsers;
its normal mode still checks delayed mutation failure and draft recovery.

## S8: whole-module WASM savings — November 7, 2026

The exact starting point is `c6/next` **7035ec6b**, not main. Production
`make ui` initially fails the unchanged **10,724,780 raw / 2,667,328 Brotli**
ceilings: raw is 64,545 bytes over and Brotli has just 2,560 bytes of room.
Measurements use the locked UI profile, Binaryen 123 and Brotli quality 11;
names-kept diagnostic files are never counted as production downloads.

| Build | Raw bytes | Brotli bytes | Saving, raw / Brotli |
|---|---:|---:|---:|
| 7035ec6b baseline | 10,789,325 | 2,664,768 | — |
| Shared popup listener factories | 10,750,115 | 2,659,154 | 39,210 / 5,614 |
| Shared checked response-body polling | 10,691,725 | 2,651,304 | 58,390 / 7,850 |
| Shared list-editor view boundary | 10,688,242 | 2,649,938 | 3,483 / 1,366 |
| Shared stable activity ordering | 10,680,340 | 2,650,316 | 7,902 / -378 |
| Shared fresh-setting construction | 10,664,451 | 2,647,125 | 15,889 / 3,191 |
| Shared resource construction by result type | 10,584,246 | 2,638,163 | 80,205 / 8,962 |

The whole-module profile repeats S1's names-kept bindgen/`wasm-opt -Oz -g`
pipeline, twiggy top/dominators/monos, and all-symbol per-crate/per-module
shallow totals. An isolated exact **235fae5c** archive build provides the
growth comparison: executable code grows **159,700 bytes**, data **11,544**.
Its larger diagnostic name section is not an executable-code saving.
New session overview/jobs, picker and message-editing work is visible in
the growth; inline deserialization moving into `api::json` must not be
mistaken for an independent 294 KB API regression.

The final code section is **9,769,458 bytes**, down **204,593** from this
baseline and **44,893** below exact 235fae5c. Data changes from **699,291** to
**698,919**. These code/data byte counts match production, independently of
diagnostic names. Full old/baseline/final attribution checks cover all **50**
leading-crate buckets and **214** lexical crate/UI-path rows. Largest lexical crate
changes are c6_ui **-94,708**, reactive_graph **-66,483**, leptos **-22,219**,
core **-11,565**, alloc **-5,172**, and tachys **+1,692**. They are not complete
source ownership or per-download allocations; generic UI machinery appears
under framework labels, and module-reference views overlap the crate view.
An additional source-inventory rollup covers all **63** crate-root modules,
their **7** production children, and crate root, including zero-retained rows.
It folds root functions/types into `(root)` rather than inventing modules
from their symbol names, and checks both top-level and nested partition sums.

At 7035ec6b, the largest individual renderers are Settings **212,459**, workspace
construction **160,872**, App **73,556**, and ProjectPage **47,013** raw
diagnostic bytes. EventKind's largest flattened deserializer is **65,922**.
Drop/component/For families span modules and overlap those renderers.
New job/detail component types, repeated navigation/popover/form types, and
shared HTTP/body polling are additional candidates, not just list editors.

Popup dismissal now shares one non-generic effect/listener factory behind
boxed `shown` and reference-counted `close` callbacks. Tracking stays inside
the same effect; capture/passive flags, weak ownership, outside-press policy,
nested Escape ordering, focus restoration and cleanup are unchanged. All
current close captures are shared reactive handles, not clone-sensitive
owned state. No views, DOM attributes, icons or overlay lifecycles are erased
or merged by this change. Native UI tests pass **305/305**.

Successful JSON responses now share a non-generic boxed future for HTTP
checking and reading the body as text, before typed serde decoding. This
is the locked gloo-net 0.7 `Response::json` sequence: the same text reader
and serde parser, including transparent serde error strings. `check`'s
structured-error/status-text fallback and 401 sign-out remain unchanged.
The extra future allocation per JSON response does not spawn a task or add
a scheduler yield. Checking/reading remain lazy; dropping the outer future
drops the nested body future without promising stronger network cancellation.
Request sending, serialization and AbortSignal handling are unchanged.
The response-body change also passes **305/305** native UI tests.

ListSetting keeps its original typed section, fields, keyed rows, event handlers
and resources, returning one AnyView after construction instead of exposing the
whole editor type to every Settings branch. There is no HTML-string rendering
or global component erasure. The focused `settings-lists.cjs` regression passes
**8/8** contexts on both the exact baseline and this build (Chromium/WebKit,
light/dark, desktop/phone), including live-update DOM/focus retention, every
field kind, rejected/successful saves, reload, same-page section navigation and
root/realm switching. Native UI tests also pass **305/305**. Forcing the
constructor not to inline saved no additional raw bytes and increased Brotli
by 956 bytes, so that attribute was not retained.

The identical newest-activity Session sorts in Sessions, Ask conversations,
Mission's fallback and a View's session feed now share one non-inlined concrete
stable-sort function. Equal timestamps retain their original input order,
including the minimum/maximum i64 values; Viewed and Title policies are
unchanged. No dynamic comparator, new allocation, tie-breaker or unstable sort
is introduced. Native UI tests pass **306/306**. This saves raw bytes while
costing **378 Brotli bytes**; both measurements are retained in the table.

Ten plain setting readers first share one non-inlined resource factory. Each
call still creates a distinct caller-owned resource: no cache, coalescing or
changed dependencies. This narrow step adds no boxing; the next boundary
below does. Realm-tracked readers keep their explicit tracking. Native UI
tests pass **309/309**, including same-name
independence and parent/child cleanup; the expanded Settings browser suite
passes **8/8** on both the original and candidate builds. It also verifies
whole-JSON validation, exact save payloads and a fresh read after remount.
The fixture waits for the actual loaded value before editing, rather than
mistaking an initially rendered empty textarea for a completed resource.

Thirty resource-constructor positions now share construction by three result
types: JSON Value (18), WorkItem lists (8) and ProjectItem lists (4). Every
original fetch closure is unchanged, including synchronous dependency reads,
conditional empty results and steady-list wrappers. The helper invokes each
fetcher synchronously under the original owner/observer, then boxes its
returned future. It does not add an owner, cache, task or scheduler yield;
LocalResource retains its scheduling, refetch and cleanup machinery. Costs
are one boxed callback per resource and one additional boxed future per fetch,
plus indirect calls/polling; SSR also constructs these boxes without polling
the API body. Native UI tests pass **311/311**, including independent owners
and synchronous factory invocation with an unpolled body.
The Settings regression passes **8/8** on this build. The new resource lifecycle
suite passes **16/16** on both the exact baseline and this build (both engines,
light/dark), covering tracked Saved queries, tick refetch, errors/recovery,
remounts and conditional Work/Projects lists. Held responses preserve the
baseline's serial same-owner fetching; a disposed owner's late error cannot
replace fresh remounted content. No stronger cancellation guarantee is claimed.

Combined production savings are **205,079 raw / 26,605 Brotli bytes**, leaving
**140,534 raw / 29,165 Brotli bytes** under the unchanged ceilings. Both
requested headroom goals are met, with more than 100 KiB raw and 20 KiB Brotli
remaining. A separate pending approximately 25 KB raw change is not included
in these measurements. No further size trials are needed for this target.

Final verification runs `env -u NO_COLOR make ui`, `make check`, then
`make test`, with the documented 65,536 file-descriptor limit. All three pass;
the workspace run passes **1,262 tests**, with two deliberately ignored
upgrade-fixture generators and no failures. Motion VM tests pass **13/13**,
theme tests **12/12**, and crash VM tests **8/8** (the separate opt-in mounted
crash browser test is not enabled). The rebuilt production files match the
immutable browser snapshot byte-for-byte; CSS also matches the exact baseline.

The broader pinned-browser run is **not wholly green**. Its original
workspace-open Changes/backdrop failures reproduce in Chromium and WebKit,
including isolated final-artifact reruns and baseline controls. Full Motion's
first desktop/light/system context fails its model/effort assertion on both
engines; baseline controls reproduce that assertion. The standalone WebKit
modelForms check passes on both
artifacts, while the exact full first-context rerun fails: this is an
interaction-sensitive baseline failure, not a passing full suite. Original
cohorts remain failed; final-run assertions and normal deadlines are unchanged.
The extra WebKit full-context rerun failed later, after effort selection. Two
fixed baseline controls both stopped at model selection, so attribution of
that later effort-selection variant remains unverified.

Independent final coverage passes phone-picker **48/48**, comment-sheet
**16/16**, Settings **8/8**, resource lifecycle **16/16**, response failures
and sign-out **10/10**, reply recovery **12/12**, and session-sheet **36/36**.
Theme passes in both engines and ring-render in Chromium. Motion's explicitly
labelled subset excluding modelForms passes **24/24 per engine**; it is not a
full Motion pass. These are Linux Playwright 1.65.0 engines, not physical Safari.

The broader audit found no accidental release inclusion of native tests,
SSR, regex, ICU or a second gloo-net implementation. Markdown defaults are
already disabled; its supported tables/strikethrough/task lists remain.
Unused Debug derives do not explain significant retained code. Panic/crash
diagnostics, static help/icon tables and stable sorting remain intact.
Offline second-pass convergence saves only **434 Brotli bytes** (Os: 1,289),
so no optimizer/linker/profile changes are retained on that evidence.

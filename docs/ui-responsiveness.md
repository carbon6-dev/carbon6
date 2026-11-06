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

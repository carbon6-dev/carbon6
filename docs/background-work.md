# Background work and typing (S15)

Audit baseline: `8a5ac826` on `c6/next`, November 8, 2026. Paths in the inventory
are baseline locations, not moving references to the final implementation.
`ui/` below means `crates/c6-ui/src/`; JS means `crates/c6-ui/js/`.
This inventory distinguishes local presentation clocks, HTTP polling, push
invalidation, retry, and bounded interaction scheduling. A timeout is not
necessarily a poll. CSS hiding does not unmount a component.

## Final release disposition: audit only, no runtime changes

The bounded screen isolates each change against exact `8a5ac826`, rather than
treating the all-background build as qualified. Visibility-aware polling and
push-driven realm/decision counters survived the short screen, but **their
five-pair combination failed the predeclared quiet endpoint**. Therefore **no
runtime change qualifies for this release**. Production Rust, JS, styles,
assets and existing test files are restored to the exact baseline. Only the
audit and standalone benchmark are retained; neither promises faster typing.
Held-notice/turn-stat memoization and Work-over-events failed the short screen;
both stream-presentation policies had already been rejected.
The earlier five-commit series is no longer the release branch history: original
commit hashes below identify historical experiments, not commits to merge.
The five-pair confirmation and final baseline verification are recorded under
“Final confirmation, evidence, and historical series” below.

The four isolated builds use the unchanged production profile, Chromium,
fixture, 15 s warm-up, 200 keys, 4× CPU throttle and 200 live chunks. To bound
the release investigation to about an hour, the predeclared **exploratory
screen** uses two balanced-order A/B pairs per change and scenario: 32 fresh,
serial browser trials, **32 passed, zero failures, no retries or exclusions**.
This is a conservative filter, **not proof of individual non-regression**.
The rule was fixed before timing: discard a candidate if its paired p50 median
change is positive in either quiet or live typing (1e-6 ms floating tolerance).
Task cost and p95 are reported, not substituted for that endpoint after seeing
the results. Centers below are medians of two run statistics; brackets retain
the full run range, not pooled-key confidence intervals.

| Isolated change / scenario | p50 base → candidate, ms [run range] | p95 base → candidate, ms [run range] | Paired p50 Δ, ms [pair range] |
|---|---|---|---|
| Visibility / quiet | 7.30 [6.60–8.00] → 7.25 [6.80–7.70] | 18.20 [18.00–18.40] → 18.55 [18.20–18.90] | −0.05 [−0.30,+0.20] |
| Visibility / live | 10.10 [9.00–11.20] → 8.90 [8.50–9.30] | 159.30 [141.70–176.90] → 148.45 [142.50–154.40] | −1.20 [−2.70,+0.30] |
| Held notices/stats / quiet | 6.85 [6.30–7.40] → 7.20 [6.80–7.60] | 17.70 [17.50–17.90] → 18.45 [18.30–18.60] | +0.35 [+0.20,+0.50] |
| Held notices/stats / live | 11.80 [10.70–12.90] → 9.00 [8.60–9.40] | 208.45 [206.80–210.10] → 169.05 [168.80–169.30] | −2.80 [−4.30,−1.30] |
| Counters / quiet | 7.40 [7.10–7.70] → 7.20 [6.40–8.00] | 18.35 [18.20–18.50] → 18.15 [17.70–18.60] | −0.20 [−0.70,+0.30] |
| Counters / live | 11.05 [8.70–13.40] → 9.65 [8.30–11.00] | 207.80 [138.70–276.90] → 130.85 [90.20–171.50] | −1.40 [−2.40,−0.40] |
| Work events / quiet | 6.70 [5.70–7.70] → 7.05 [7.00–7.10] | 17.80 [17.50–18.10] → 18.15 [18.10–18.20] | +0.35 [−0.70,+1.40] |
| Work events / live | 8.50 [8.10–8.90] → 10.10 [8.70–11.50] | 132.00 [88.60–175.40] → 173.65 [142.10–205.20] | +1.60 [−0.20,+3.40] |

Visibility's quiet p95 and task total worsen in this short screen; its task
median is 825.03→882.75 ms (+57.72 ms), while counters' live task median rises
10,039.64→10,107.24 ms (+67.61 ms). These trade-offs are not hidden. Full run
and pair values, build/source/harness hashes, order and loads are in
`/tmp/s15-bg/ablation/screen-{manifest,summary}.json`; raw traces/profiles are
under `ablation/results/`. Rejected implementations and all their measurements
remain separate, never pooled into this screen or the final confirmation.

### Pre-trial gate amendment

Before any ablation trial, the coordinator amended the runnable ceiling from
two to **four**, retaining **at least 95% idle in every one of 30 one-second
samples**, and no compiler, test or browser in any user's process snapshot.
Eight complete original-gate windows failed despite mostly 95–99% idle and
normal host-service run-queue peaks of three/four; those windows and subsequent
original-gate attempts remain rejected. The first amended window passed at
96–99% idle, queue 1–3 (`ablation/gates/amended-screen-1`). Load was
16.84/17.14/18.09 before and 17.11/17.13/18.06 after. See
`ablation/gate-amendment.md` and both gate scripts for the pre-trial reason and
unchanged screening rule. Interleaved A/B order reduces temporal confounding;
it does not prove that every shared-host disturbance cancels. Earlier cohorts
retain their original stricter gate classification.

### Explicitly deferred work

- Held-notice/turn-stat changes fail the screen's quiet endpoint. They are not
  retained despite better live numbers; patch and omitted native tests remain
  under `ablation/held-source.patch` and `held-native-tests.patch`.
- Work-over-events/shared open queries fail quiet and live endpoints. The
  implementation, semantic head producers, editor-race fix/tests and measured
  one-quota/zero-Work minute are **experimental evidence, not shipping code**.
  The final baseline still has duplicate Work resources, broad snapshot
  invalidation and visible fallback polls. Do not claim a retained 39→1/min
  request reduction. Patch/build/trace evidence is under `ablation/work-*`;
  remaining event-coverage/projection limits are described historically below.
- The original fixed150 and adaptive/frame500 stream policies remain rejected.
  Even restoring direct streams with all other background changes left live
  p50 8.50→10.30 ms (six-pair Δ +1.75 [−0.20,+3.30]) and p95
  162.25→199.55 ms. No remaining source cause was established by attribution;
  see `followup/selected/cohort-attribution.{md,json}`. The bounded screen is
  selection evidence, not an attribution of those whole-build results to one
  timer, observer, function or engine.

## Background inventory

At the audit baseline the global socket has Workers, Sessions, Doc, Marks and Attention
(`crates/c6-api/src/lib.rs:144-156`). Registry snapshots are coalesced at 250 ms
(`crates/c6-head/src/api.rs:423-473`); worker heartbeats can therefore invalidate
Sessions-dependent resources even without a session change. Attention is a
separate 500 ms coalesced hint (`crates/c6-head/src/attention.rs:29,43-73`).
Decision mutations and timed snooze/deadline handling already emit it
(`crates/c6-head/src/decisions.rs:105,130,254,316,661-672`). `/elsewhere` reads realm
names and decision counts (`crates/c6-head/src/attention.rs:379-391`), not general
model/session progress. Those existing semantics make Attention appropriate for
the shared counters, but not a replacement for every data source below.

| Activity and baseline source | Trigger / frequency | Push coverage and disposition |
|---|---|---|
| Live fleet/session feed, `ui/api.rs:50-86` | One page WebSocket; reconnect 500 ms doubling to 10 s | The push source. Retained, including hidden tabs for notifications. Ordinary frames do not count as reconnects. |
| Per-session feed, `ui/api.rs:124-155`, `ui/session.rs:394-451` | Socket per followed session; retry 500 ms–10 s; visible updates frame-batched, hidden updates flushed | Already push. Preserve replay, wire order, late-delta suppression, and cancellation. |
| Quotas, `ui/quota.rs:25-35` | Initial GET and every 60 s | No quota event; Usage tokens do not contain subscription windows. Retain visible-only fallback. |
| Build version, `ui/main.rs:662-686` | Initial GET, 30 s, reconnect rising edge | Executable replacement breaks socket; reconnect covers normal replacement. |
| Phone waiting count, `ui/main.rs:163-170` | Initial `/elsewhere`, realm changes, attention, every sessions snapshot, 30 s | Global Attention covers decision counts including timed snooze wake. Duplicate consumer of realm counters. |
| Place menus, `ui/main.rs:506-512` | Independent `/elsewhere` initial reads, realm changes, snapshots, 30 s | Same data as phone count; menus include retained closed drawers. |
| Place links / missing realm, `ui/main.rs:590,619` | Independent initial `/elsewhere` reads | Same data; realm metadata changes need invalidation too. |
| Decision badges, `ui/decisions.rs:19-49` | Initial count, all sessions snapshots, 30 s, per mounted badge | Attention already covers create/update/answer, deadline expiry, snooze wake. |
| Decisions page, `ui/decisions.rs:182-191` | Initial/filter, coalesced sessions/doc changes | Attention is the appropriate invalidator. Existing quiet reload policy protects editing. |
| Session decisions, `ui/decisions.rs:215-245` | Initial, 30 s, attention/reconnect plus generic sessions/doc reload | Attention and reconnect suffice. |
| Work lists/badges, `ui/work.rs:43-60` | Initial, 30 s, 10 s coalesced sessions/doc updates; typing defer | Follow-up: one shared page query, semantic Work hints and a five-visible-minute time/external fallback. No per-navigation polls. |
| Work item, `ui/work.rs:559-566` | Initial and coalesced sessions/doc updates | Follow-up: semantic Work invalidation; quiet publication and stale own-mutation GET fencing. |
| Work template save, `ui/work.rs:790-792` | One 5 s refresh after explicit action | Bounded completion refresh, not polling. |
| Projects lists/badges/detail, `ui/projects.rs:40-76,177-205` | Initial/realm plus 10 s coalesced sessions/doc changes; optional closed list | No project timer of its own. No complete Project event; retain existing invalidation. Detail can reload three resources. |
| Needs You, `ui/needs.rs:130-179` | Initial, Attention, reconnect rising edge; 300 ms coalescing | Already event-driven. Retained drawer instances have their own resources. |
| Activity badges, `ui/monitor.rs:371-389` | Initial and 3 s; HTTP is console-only | No complete global model/sync Activity event; retain visible-only poll. |
| Activity page, `ui/monitor.rs:33-49` | Initial and 1.5 s | No matching push event; retain visible-only poll. |
| Model call detail, `ui/monitor.rs:244-263` | Initial, 1 s while live; HTTP stops when ended | No model-call-detail event. Pause hidden and stop interval when finished. |
| Brief overview, `ui/brief.rs:162-183,218-231` | Five RPCs every 30 s, unless request open/row unfolded | Attention is incomplete for FYI/triage/commitments. Retain visible-only poll. |
| Brief drafting, `ui/brief.rs:569-609` | Initial; 2 s timer until closed; HTTP only during generation/first draft pending | Explicitly no draft change event. Retain visible-only timer with busy/generation guards. |
| Mail outbox, `ui/mail.rs:354-364` | Initial and 3 s, even empty | No outgoing-mail event; empty state cannot stop polling (remote mail can arrive). Pause hidden. |
| Chat sync, `ui/chat/mod.rs:276-283` | Slack sync and refresh every 30 s; metadata refresh includes channels/sections/threads and periodically people | No Slack change event in global feed. Preserve visible cadence, pause hidden; no draft change. |
| Update settings, `ui/settings.rs:1636-1653` | Workers snapshots | Event-driven but heartbeats over-invalidate. No dedicated update-progress event; unchanged. |
| Workflow runs, `ui/settings.rs:2385-2407` | Initial; 3 s timer, HTTP only while any run is running/waiting | Decisions do not cover every workflow transition. Retain visible-only timer; it still wakes without HTTP while idle and visible. |
| Fleet last-seen clock, `ui/fleet.rs:138-149` | 5 s local clock; worker data pushed | No HTTP. Pause hidden. |
| Relative clocks, `ui/util.rs:197-206` | 5 s per caller: sessions list/hover, tasks list/detail, deck presenter | No HTTP. Pause hidden; text dates still refresh on return. |
| Login-code countdown, `ui/auth.rs:445-456` | 1 s local clock | No HTTP. Pause hidden; still ticks while visible without an active code. |
| Missing session metadata, `ui/workspace.rs:1759-1805` | By-ID read for eligible missing row; 2 s failed-read retry | Archived rows intentionally absent from live list. Retry remains; not an ordinary idle poll. |
| Seen/catch-up, `ui/workspace.rs:1843-1896` | Visibility/layout/session changes | Already visible gated; explicit reads/writes, not per-key. Preserve. |
| Docs list/detail, `ui/docs.rs:117-125,633,1289` | Local actions and matching LiveEvent::Doc | Already push, detail filtered by document ID. |
| Terminal, `ui/term.rs:58-136` | One PTY WebSocket on mount, streamed output | No HTTP poll or reconnect loop; retain connection while hidden. |
| Session timeline, `ui/timeline.rs:263-272` | Start/end markers when folding/watching | Not every token. Preserve guard. |
| Git detail, `ui/details.rs:63-77` | Turn-end marker | Not every token. Preserve guard. |
| Transcript clock, `ui/session.rs:2261-2268,2552-2554` | 1 s while mounted; formats running elapsed title/aria/text | Local clock, no server event needed. Idle held-notice consumer was broader (below). |
| Phone status clock, `ui/session.rs:2726-2739` | 1 s, including CSS-hidden desktop alternative | Local turn string; remains a surface-local unconditional interval, not an HTTP poll. |
| Subagent clock, `ui/session.rs:4723-4734` | 1 s; sums child turns for elapsed header | Local presentation; complete/open children need no live clock. |
| Held notices, `ui/session.rs:2907-2931` | Full retained-history scan each 1 s / details 5 s tick while idle | Durable NoticeQueued/User events cover membership. Only grace deadline requires time, not history polling. |
| Event reducers, `ui/session.rs:102-139,2254-2255,2339-2358` | Durable events: usage, decisions, results, rewinds, keyed pieces | Not composer text or ordinary text deltas. Retained-history scans remain on real log changes. |
| Streamed Markdown, `ui/session.rs:2373-2375,2533,4509` | Text/thinking lane change, at most one visible frame flush | Full growing prefix parsed and innerHTML replaced; a distinct live rendering cost, not draft-triggered transcript rerender. |

## Per-keystroke and bounded interaction inventory

| Activity and baseline source | Trigger / frequency | Disposition / existing event |
|---|---|---|
| Session text, `ui/session.rs:6266-6270` | Each input reads DOM value, sets draft, updates caret completion | Local; no network, usage estimate or model validation per key. |
| Mention/command discovery, `ui/session.rs:5639-5642,5670-5675,5899-5914,6327` | Input/click and caret-navigation keyup; two prefix parsers allocate | Local; not currently measured as the dominant 200-character draft cost. |
| Completion list, `ui/session.rs:5878-5898,6201-6204,6279-6295` | Query/results and keydown; catalog clone/filter/sort | Local; plain closure can redo work. Retain behavior. |
| Mention requests, `ui/session.rs:5807-5863` | Changed query/place, 120 ms trailing debounce; abort old browser requests | Already debounced/query-deduplicated. Up to filesystem + memex + Ask references. Not an unconditional per-key request. |
| Grey suggestion, `ui/session.rs:5726-5742,5782-5797` | Fetch initially idle / running→idle; seen acknowledgement; acceptance fills draft | Fetch has no text dependency. Display reads empty draft/running/disabled; accepting never sends. |
| Draft text storage, `ui/util/drafts.rs:13,78-109` | 250 ms trailing save; flush on hide/pagehide/disposal; forget cancels | Already debounced. Preserve immediate lifecycle persistence and sent-draft protection. |
| Chat draft, `ui/chat/composer.rs:105` | localStorage on each reactive text change | Different composer; remains a follow-up candidate, not blamed for session typing. |
| Autosize, `ui/util.rs:280-380` | CSS field-sizing when supported; otherwise input/rAF fallback plus hint/visibility/resize refit | No polling. Preserve native Chrome behavior and fallback correctness. |
| Attachment work, `ui/session.rs:5974-5988`, `ui/util/drafts.rs:306-335` | File picker/drop/paste, attachment changes; file validation/IDB persistence | Not text-triggered. |
| Send/model validation, `ui/session.rs:5990-5995,6054-6078` | Explicit send/send now | No per-key provider/model/usage fetch. |
| Model catalog, `ui/resources.rs:17-19,104-132`, `ui/models.rs` | Lazy picker/prefetch, page cache 10 min, retry 30 s on demand, max 128 | Freshness thresholds are not intervals. Connections edits invalidate. |
| Document autosave, `ui/docs.rs:2595-2607` | Dirty edits, 1.5 s debounce/lifecycle flush | Interaction scheduling, not idle polling; retain. |
| Menus/presence/animations, `ui/util.rs:30-129`, `ui/activity.rs:61,650` | Bounded 80–300 ms presence, count transitions, hover delay | Event-triggered local timers; cleanup owned. No replacement server event. |
| Mention popup/search reveal, `ui/mentions.rs:173`, `ui/session.rs:3851-3889` | Bounded delayed reveal/focus after action/render | Not periodic refetch. |
| Spotlight/contact batching, `ui/aspects.rs:192-213`, `ui/contacts.rs:72-85` | 400 ms pointer dwell / 800 ms queued contact-click batch | Local affordance / explicit persistence, not a background poll. |
| Notice dismissal / session hover/start, `ui/util.rs:500,525`, `ui/sessions.rs:1015,1835` | 6 s dismissal, 160 ms transition, 250 ms hover exit, 0 ms post-start callback | Bounded action/lifecycle scheduling, not polling. |
| Chat completion/formatting/jump/undo, `ui/chat/composer.rs:166,188,217`, `ui/chat/message.rs:173` | 0 ms caret restoration, 50 ms jump reveal, bounded undo-send delay | Explicit interaction scheduling; no recurring fetch. |
| Session double-Esc arm, `ui/session.rs:947-954` | 1.5 s reset after first Escape | Explicit keyboard affordance, not polling. |

Usage (`ui/usage.rs:45-50`) and Analytics (`ui/analytics.rs:45-65`) load on
mount/days/filter/group changes, not a periodic or focus refresh. Analytics uses
live sessions for group choices only. Settings lists, auth/push/backups/watchers,
calendar/contacts/people/photos/files/collections/custom views otherwise use
mount, filter, explicit-action or lazy-pagination reads. There is no general
window-focus refetch policy. Model input focus (`ui/models.rs:106`) consults the
same bounded on-access cache rather than unconditionally requesting models.

## JavaScript and observer inventory

First-party JS has no general periodic HTTP poll. Observer delivery and bounded
animation frames must not be confused with setInterval polling. The baseline
has 19 observer-construction sites: 11 ResizeObserver, four MutationObserver and
four IntersectionObserver sites (some create multiple instances). S15 adds one
owned IntersectionObserver site for pane visibility.

| Source | Trigger and work | Event/poll |
|---|---|---|
| `js/motion.js:83` | Popover ResizeObserver watches menu/anchor/tile; scroll/viewport listeners schedule one rAF fit | Event-driven; disposer clears listeners/observer/frame. |
| `js/motion.js:243` | Resize FLIP observer watches opted-in element | Bounded WAAPI animation, not a frame poll. |
| `js/motion.js:384,399` | Scroll row ResizeObserver remembers intrinsic row sizes; subtree childList MutationObserver registers/unregisters row descendants | Event-driven; streamed Markdown replacement also traverses added/removed descendants. No HTTP. |
| `js/motion.js:312,448-511` | Pointer/wheel/scroll, scroll-end and bounded follow/glide frames | Input/animation, not recurring HTTP. |
| `js/header.js:72,76,80` | Header ResizeObserver; subtree childList/characterData observer; drawer aria-expanded observer | Event-driven whole-header fit; ignores feedback-only mutations and writes no observed attributes. |
| `js/search.js:35-58` | MutationObserver resets 200 ms search highlight timer; rescans registered roots | Event/debounce; broad all-root rescan remains a candidate during active search. |
| `js/terminal.js:60` | ResizeObserver invokes terminal fit; PTY output arrives over socket | Event-driven, no idle HTTP loop in first-party helpers. |
| `js/files.js`, `js/passkey.js`, `js/push.js`, `js/crash.js:62`, service worker | Explicit file/auth/notification operations, error-only crash POST, notification-click focus/open | Event-driven, no recurring poll. |
| `ui/deck.rs:140-148` | ResizeObserver, deck scaling | Event-driven. |
| `ui/docs.rs:1161,1559-1591` | ResizeObservers for document/editor geometry | Event-driven. |
| `ui/mail.rs:49-58` | IntersectionObserver lazy near-viewport work | Event-driven, not polling. |
| `ui/chat/message.rs:156`, `ui/timeline.rs:415` | IntersectionObservers for older-message pagination and visible outline detail | Event-driven, bounded loading/done guards. |
| `ui/session.rs:1216,2982` | Session-width ResizeObserver; top/bottom sentinel IntersectionObservers | Event-driven breakpoint update / older/later log loading. |
| `ui/util.rs:327-350`, `ui/workspace.rs:2152-2176` | Fallback textarea/scroll ResizeObservers | Event-driven. Resize must still preserve transcript follow when multiline draft changes its viewport. |
| Inline tile motion, `ui/workspace.rs:1330` | One queued rAF per workspace layout action; before/after rect reads and bounded WAAPI moves | Not an idle frame loop or HTTP poll; cancelled by motion policy/cleanup. |
| Pane restoration/loading, `ui/workspace.rs:2109,2363-2364` | One scroll-restore frame on reveal; two frames to release other tiles' initial loading | Mount/visibility work, not recurrent polling. |
| Session paging/arrival/details, `ui/session.rs:487,1772,2384` | One frame after paging, details restoration or Synced | Scroll/focus/arrival presentation, not per-key history rendering. |
| Live tool-output follow, `ui/session.rs:5137` | Output/result changes schedule scroll-to-end rAF only while following | Event-driven geometry read/write; no independent interval or HTTP. Not identified as a specific forced-layout culprit by the captured profiles. |
| Session completion caret, `ui/session.rs:5932,5949,5967` | One focus/selection frame after explicit command/mention acceptance | Not ordinary-key polling. |
| Document placement/reveal/follow, `ui/docs.rs:1153,1166,1421,1540,2149,2177,3463` | Observer delivery, block/comment reveal, popup reservation/placement, comment-list changes | Queued bounded geometry/focus work, not idle scanning. |
| Drawer/label/hover/aspect mounting, `ui/main.rs:369`, `ui/labels.rs:186`, `ui/sessions.rs:1125`, `ui/aspects.rs:143` | One frame after opening/pinning or keyed aspect-bar changes | Focus/static-slot mounting; not recurring HTTP or a general focus refetch policy. |
| S15 pane surface observer, `ui/workspace.rs::ColumnView` | One owned IntersectionObserver plus reactive layout visibility | Pauses hidden/minimized/inactive pane polls without unmounting drafts. |
| Rejected pre-merge stream experiment, `ui/session.rs::frame_stream` | One owned rAF after a long-stream cadence deadline per live lane | Measured separately below, then removed with the original presentation throttle; not a final background activity. |

## Initial implementation (historical; release disposition above takes precedence)

The visibility scheduler retains initial fetches and each visible cadence, clears
intervals while the document or owning pane is hidden, catches up once on return,
and clears owned handles/listeners on disposal. It does not close sockets or
discard drafts. Initial reads, genuine push invalidations, and transient-error
retry timeouts are separate work and are not all blocked by this scheduler.

Held notices now derive their unconsumed candidates only when durable events
change. A single owned timeout targets the earliest three-second grace deadline,
rearms for later candidates, and is cancelled while the page is hidden or the
component is disposed. Returning catches up against current wall time. Empty or
already-mature candidates require no clock. Existing consumption, ordering and
quota-delivery timestamps are preserved. Running turn formatting is memoized per
status surface and shared by its text, title and accessible label; it does not
subscribe the transcript row list to time.

The initial long live reply/thinking fix (at least 8 KiB) published growing prefixes
at most once per 150 ms, with one cancellable trailing read of the latest lane.
Short text retained frame cadence. Resets, non-prefix replacements and durable
messages rendered immediately. This was a presentation limit, not a receive limit:
no tokens, events, icons, or authoritative state were dropped. Continuous input
did not keep postponing the trailing deadline. Each live subtree owned cleanup,
so a durable replacement or closed view could not resurrect old streamed text.
The pre-merge follow-up below rejects both this policy and a frame-aligned
adaptive replacement because their median-typing trade-offs do not meet the
requested acceptance rule. The selected implementation restores the existing
feed-frame/equality-memo presentation instead of keeping dead throttle code.

Realm/decision counters now share one page-owned pair of resources across all
navigation copies. Attention changes and successful reconnects invalidate them;
ordinary worker/session snapshots and independent 30-second timers do not. Realm
create/update/delete also ping the existing global Attention hint so names and
membership remain correct across browsers. Session decisions use the same
attention/reconnect policy, preserving editing deferral. The head already emits
Attention for deadline expiry and snooze wakeup. Version state is page-owned and
checked initially and on successful reconnect, rather than polled: replacing the
head breaks that socket. This counter step needs no wire-event variant or
migration; the Work follow-up below adds a separate semantic hint.

Three one-second presentation intervals remain in `session.rs` (Transcript,
PhoneStatus and SubagentLive), including CSS-hidden alternatives. They no longer
scan history for held notices; this change does **not** claim to eliminate every
background clock. Global navigation fallbacks without a `ColumnCtx` are gated by
document visibility, not by whether their retained drawer is open.

## Original single-run production benchmark

Artifacts are local under `/tmp/s15-bg/`; nothing is published. Original pairs
are `before-verified` / `after-verified` and `before-live-verified` /
`after-live-verified`. Other runs are diagnostics, not interchangeable samples
in these tables. Earlier `*-final` controls preceded the mutation-counter
positive-control correction and synchronized producer start; their timing data
is retained separately, not silently mixed into the final comparison.

The independently built baseline is exactly `8a5ac826`. Both builds use the same
private fake-head HTTP/WebSocket fixture and production WASM in Chromium
158.0.8127.18, a 1800 × 1000 dark/full-motion workspace with session, Work and
Projects tiles, and CDP 4× CPU throttling. There is a fixed 15-second warmup before
each **actual 60-second idle window** to drain initial ten-second coalesced
sidebar reads. Each typing window enters 200 characters at a requested 40 ms
pace; actual wall time is recorded, not assumed.

The fixture replays 20,965 events including 950 historical turns, 95 image
records, and approximately 5,023,286 bytes of additional event envelopes. It is
not the user's private session or an exact 8,800-event / 2.2 MB capture. Normal
transcript tail trimming leaves only 14 historical Markdown nodes and one image
mounted (3,372 total DOM nodes quiet; 5,009 nodes / 15 Markdown nodes live).
Thus this tests a long retained log, **not 950 simultaneously rendered turns**.

The live control starts with a roughly 30 KiB rich-Markdown prefix and delivers
exactly 200 additional chunks at 20 Hz while typing. Both controls wait for all
200 keys and all 200 chunks, and for the final chunk to appear. Different wall
times therefore do not hide dropped live updates. The producer starts inside
the measured typing window. Live Markdown mutations are also a positive control
for the observer used to check that quiet input does not rewrite history; it
must observe both innerHTML-root mutations and replaced Markdown subtrees.

Latency is keydown → rAF → posted task, an approximation after the next rendering
opportunity, **not hardware INP or proof of a presented pixel**. Quantized
EventTiming entries, long tasks, interval stacks/callbacks, request paths, CDP
TaskDuration/ScriptDuration, CPU profiles and DevTools traces are saved. Browser
and benchmark instrumentation are present in both cases. These are single paired
runs, not confidence intervals or physical Mac/Android measurements.

| 200-character typing window | Quiet before | Quiet after | Long live reply before | Long live reply after |
|---|---:|---:|---:|---:|
| Latency p50, ms | 7.1 | 10.9 | 8.9 | 14.7 |
| Latency p95, ms | 17.8 | 20.2 | 90.9 | 71.6 |
| Latency max, ms | 20.7 | 24.7 | 205.2 | 86.1 |
| Long tasks ≥50 ms | 0 | 0 | 122 | 17 |
| Long-task duration sum / max, ms | 0 / 0 | 0 / 0 | 8,315 / 104 | 1,004 / 67 |
| Actual window, ms | 8,457 | 8,679 | 13,493 | 10,840 |
| Main-thread TaskDuration delta, ms | 818.7 | 1,313.7 | 10,409.9 | 5,919.8 |
| ScriptDuration delta, ms | 79.0 | 156.5 | 2,355.5 | 1,113.0 |
| HTTP request starts | 0 | 0 | 32 | 12 |
| Transcript Markdown mutation records | 0 | 0 | 122 | 66 |

Long-stream p95 fell 21.2%, max 58.0%, long-task count 86.1%, and measured task
time 43.1% in this pair. Live **p50 increased 5.8 ms**: the bounded cadence trades
some median timing for less tail contention. The earlier diagnostic pair also
reduced live long tasks (115 → 23) and max (179.4 → 86.4 ms), but had a larger p95
change (89.0 → 37.5 ms). This variability precludes promising one fixed p95 gain.

Quiet typing **worsened** by 3.8 / 2.4 / 4.0 ms at p50 / p95 / max in the verified
pair, and measured task time increased. The earlier diagnostic quiet pair had
the opposite timing direction. Neither quiet pair had long tasks, HTTP requests
or transcript Markdown mutations. There is no consistent measured quiet-typing
speedup, nor evidence that ordinary draft keys alone were repeatedly rendering
history. Do not hide the quiet result behind the live-tail improvement.

| Actual idle minute (no live chunks) | Quiet control before → after | Live-control setup before → after |
|---|---:|---:|
| Wall time, ms | 60,007 → 60,003 | 60,007 → 60,002 |
| Main-thread TaskDuration, ms/min | 176.1 → 144.0 | 174.3 → 140.0 |
| ScriptDuration, ms/min | 53.4 → 23.5 | 49.0 → 30.1 |
| HTTP starts/min | 39 → 13 | 39 → 13 |

TaskDuration is browser main-thread task time, not whole-machine CPU usage or
server CPU. Both idle controls sent 39 requests before: 12 `/elsewhere`, two
`/version`, ten `/decisions/count`, 12 `/work`, two session-decision reads and one
quota read. Afterward only 12 `/work` and one quota read remain. The visible
duplicate Work navigation reads are explicitly not solved here. Live typing
still sends six Projects and six Work reads through genuine/coalesced existing
invalidation; the 20 other request starts disappear. These are requests **during**
typing, not proof that draft input triggered them. Typing totals cover unequal
durations and timer boundaries; use the actual idle-minute windows for request
rate comparisons.

### Profile attribution

The verified live traces resolve repeated generated-WASM callbacks writing
`innerHTML`, with substantial native browser rendering after replacement:

| Trace scope | Before | After |
|---|---:|---:|
| ParseHTML count / total ms | 246 / 505.6 | 134 / 234.7 |
| Style (`UpdateLayoutTree`), ms | 1,824.3 | 376.2 |
| Layout, ms | 1,797.8 | 1,445.1 |
| Paint same-name union, ms | 1,380.7 | 747.6 |
| PrePaint, ms | 633.9 | 339.4 |
| Layerize, ms | 772.7 | 441.9 |
| Row-registration MutationObserver count / total ms | 122 / 147.3 | 66 / 56.3 |
| Native intersection computation, ms | 151.2 | 141.2 |

These scopes overlap and **must not be added** into one CPU total. Paint scopes
nest: 2,616.4 / 1,371.5 ms inclusive before/after becomes 1,380.7 / 747.6 ms
same-name union. The production `js/motion.js:399` observer traverses added and
removed descendants; it is distinct from the injected benchmark observer and
smaller than native rendering. Its maximum callback drops 15.5 → 1.5 ms.
ResizeObserver JavaScript cost is not separately resolved; this is not evidence
it is free. ParseHTML scope count is not an exact publication or token count.

The shorter window matters: layout total falls 19.6%, but its per-second cost is
essentially unchanged (133.24 → 133.31 ms/s). Native intersection cost per second
rises 11.21 → 13.03 ms/s. Style, parsing and Paint union rates fall 74.3%, 42.2%
and 32.6% respectively. Residual long tasks remain multi-stage; neither the
observer nor JavaScript alone explains them. Quiet layout and paint cost also
worsen in the verified pair despite nearly identical operation counts; no
specific source regression or dismissal of that result is justified by this
single sample.

The source-backed conclusion is that long growing Markdown prefixes cause DOM
replacement plus native parsing/style/layout/paint, not that all this time is
Rust Markdown parsing or pure JavaScript. The production WASM is stripped;
numeric WASM functions cannot reliably be assigned to Rust function names.
`(program)` in CPU samples mostly represents unresolved/native browser execution.
Raw profiles contain negative `timeDeltas`; the attribution script reconstructs
absolute sample timestamps, stable-sorts/weights them, and clips to that run's
own markers rather than naively summing positive deltas. Trace scopes and sampled
weights overlap and must not be added into one CPU decomposition.

See `/tmp/s15-bg/profile-attribution.md`, `attribute-profile.py`, and paired trace
and `.cpuprofile` files for the complete attribution and after comparison.

## Pre-merge controlled repeat follow-up

The original quiet regression was not dismissed because older runs reversed
direction. A new cohort runs **six pairs each for quiet and live**, alternating
build order (three first/three second), on thalassa with fresh serial Chromium
processes and immutable production roots. Our team ran no builds, tests or other
browsers alongside its timed trials; recorded host load was elevated, not an
unloaded-host claim. All 24 trials pass. Its manifest, raw keys, native
EventTiming, CPU profiles and traces are under `/tmp/s15-bg/followup/current/`.

The fixture, 15-second warmup, 4× CPU throttle, 200 characters and 200 live
chunks are unchanged. These repeats **omit the preceding 60-second idle phase**;
idle is measured separately over full minutes, never extrapolated. They are not
pooled with the original single trials. Playwright's requested 40 ms key hold is
acknowledgement-paced, not independent-clock input: measured live key intervals
and window lengths differ between builds. Latency remains a key→rAF→posted-task
proxy, not hardware INP. Each **run/pair**, not each key, is the replication unit.

Initial cohort, exact `8a5ac826` versus pre-follow-up `c89b96d3`; values are
medians of six per-run statistics, with run min–max in brackets:

| Metric | Quiet base | Quiet current | Live base | Live current |
|---|---:|---:|---:|---:|
| p50 ms | 7.80 [7.1–10.7] | 7.65 [6.9–10.7] | 9.55 [8.8–10.9] | 14.45 [13.6–15.7] |
| p95 ms | 18.45 [17.8–19.5] | 18.55 [17.7–24.5] | 141.10 [105.2–174.6] | 53.10 [33.8–74.2] |
| Maximum ms | 23.80 [19.2–26.0] | 21.85 [21.0–35.1] | 275.75 [217.7–406.2] | 87.00 [84.5–91.4] |
| Task ms | 930.73 [799–1,212] | 824.96 [809–1,475] | 10,128 [10,004–10,820] | 5,870 [5,617–6,333] |
| Script ms | 110.80 [91.6–130.3] | 92.97 [87.4–145.4] | 2,152 [2,069–2,246] | 1,175 [1,131–1,245] |
| Long tasks | 0 | 0 | 130.5 [119–139] | 25 [17–35] |

Quiet paired p50 Δ is **−0.65 ms [−2.7,+3.0]**, lower in four pairs and higher
in two; paired task Δ is −119 ms [−375,+675]. The substantial pair-three current
outlier is retained. This does not establish a consistent quiet slowdown and
does not prove every quiet input is faster. There is no evidence here for an
IO→RO rewrite: median quiet intersection cost is 55.5→50.3 ms; layout, paint and
function-call costs also decrease at the cohort median. Existing visibility
semantics are retained rather than changed on a source hypothesis.

**Live regression is repeatable:** paired proxy p50 Δ is **+4.90 ms
[+3.4,+6.9]**, worse in six of six pairs, despite p95, maxima and task totals
improving in all six. Native main-thread input-queue p95 also rises
1.70→25.13 ms (paired Δ +23.51 ms [+21.13,+24.40], worse six of six).
Native EventTiming median rises 28.60→30.32 ms (five of six), while its p95 and
mean improve. The original stream cadence cannot be described as an unqualified
typing improvement.

The complete cohort attribution is
`/tmp/s15-bg/followup/current/cohort-attribution.{md,json}`. It validates all 200
raw keys and native timing records per run, uses each run's own marker window
and timestamp-reconstructed CPU weights, and unions nested Paint intervals.
Live ParseHTML falls 497→249 ms, style 2,048→375 ms and Paint union 1,452→796 ms;
layout total is approximately unchanged (1,647→1,639 ms) and its rate rises in
the shorter final window. No captured FunctionCall encloses Layout/Paint, and
generated WASM function numbers do not identify a Rust caller. These combined
results motivate a stream-policy ablation, not an unsupported attribution to
visibility, stats or counters.

### Frame-aligned adaptive candidate: measured and rejected

An uncommitted replacement kept the ordinary 150 ms long-stream cadence but
used 500 ms during recent composer input, with a nonreactive input timestamp.
A cadence deadline joined an owned animation frame rather than publishing from
an off-frame timeout. This was a **cadence-plus-phase package**, not an isolated
test of each line. Short/reset/durable text stayed immediate and the final tail
was checked. No application-view HTML serialization was introduced.

Its fully qualified cohort, deliberately retained at
`/tmp/s15-bg/followup/final/` despite rejection, has six paired quiet and six
paired live trials, alternating three first/three second; **24/24 pass**.
The immutable candidate WASM SHA-256 is
`6b935ff4b97abb6a6831cbe5d1d9e56dd7f8aefc4872ab3fa3987447fdd7f8a6`
in `followup/final-dist`. Those paths must not be overwritten with the selected
implementation. Centers below are medians of six run statistics:

| Metric | Quiet base | Quiet candidate | Live base | Live candidate |
|---|---:|---:|---:|---:|
| Proxy p50 ms | 7.25 [5.8–8.0] | 7.70 [6.9–8.9] | 9.95 [8.4–11.7] | 11.20 [10.8–11.6] |
| Proxy p95 ms | 17.95 [17.1–18.6] | 18.30 [17.7–18.8] | 175.30 [140.3–266.7] | 23.60 [20.2–75.3] |
| Task ms | 866.13 [814–881] | 870.51 [804–934] | 10,055 [9,906–10,265] | 3,138 [3,029–3,382] |

Live paired proxy p50 is still **+1.35 ms [−0.9,+2.7], higher 5/6**; quiet
paired p50 is **+0.70 ms [−1.1,+3.1]**, higher four, lower one, tied one.
The large native queue-tail penalty is gone: live native queue p95 is
1.545→1.156 ms, paired −0.410 ms [−0.523,+0.017]. Native EventTiming p50
29.770→13.200 ms, p95 81.734→33.059 ms and mean 37.684→16.213 ms all improve
in six of six pairs. Layout total/rate also fall 1,654→432 ms / 128.3→41.9 ms/s;
style 2,015→443 ms, Paint union 1,458→376 ms and parsing 503→94 ms. All 200 raw
keys, native records and recovered chains per trial are validated. See its
`cohort-attribution.{md,json}` and `native-primary.json`.

Those native/tail improvements do **not** erase the requested primary median
regression. The candidate is rejected, not relabelled a universal improvement by
switching metrics. The responsible long-stream presentation policy is reverted;
the other background fixes and Work push/shared ownership are retained.

Before this cohort, the coordinator arranged an exclusive slot. The accepted
30-second gate (`followup/gates/final-3`) has CPU idle **96–99%**, run queue
**1–2**, no other compiler/test/browser processes in the all-user snapshot.
Load averages before/after are 14.66/15.39/16.81 and 16.78/15.81/16.90;
per-trial one-minute load ranges **14.45–22.15**. Load average is recorded, not
called low. Rejected gate windows 1/2 remain (run queue up to five; then 92%
idle/up to three). An earlier single frame500 screen was started before a
passing gate, is explicitly unqualified, and is not pooled. The first gate
parser mishandled repeated `vmstat` headers; its recovered sample is retained
and rejected. A frame500 UI build interrupted by another helper was rerun
successfully; no interrupted artifact was timed.

### Work push and shared ownership (rejected experiment, not retained)

`LiveEvent::Work` is an independent coalesced invalidation hint (at most one per
500 ms), not a registry heartbeat and not a row payload or authorization grant.
Committed central Work/project writes, background note/guess/place completion,
seen markers, relevant settings, session lifecycle/title/archive, decisions,
documents and normal external request changes emit it. The client combines
hints/local completion/resume/reconnect in a short 200 ms window.

One lazy page-owned open-Work query now serves all navigation copies, the Work
list, Brief, merge/session/new-session pickers and ProjectPage's open-Work
candidates. Closing or hiding the first consumer cannot dispose the query or
create another polling owner. Closed/detail/reference and explicitly scoped
task queries retain their distinct semantics and ownership. Work editing views
hold changed remote publication while a marked editor has focus, including
replies requested before editing; own successful detail mutations publish
immediately and fence older in-flight detail GETs.
Local completion handlers capture the page's invalidator during component
setup; they do not look up reactive context after an HTTP await. The integrated
editor regression exposed a real `expected context ... Page` panic in the
initial uncommitted implementation, missed by helper-only tests. Its failed
run and isolated repeat are retained, rather than classifying the resulting
`unreachable` trap as an engine failure.

One **five-visible-minute page fallback** remains for time-derived snooze/quiet
transitions and uncovered/best-effort external changes. Hidden hints retain
dirty state without starting Work HTTP; visible resume catches up once. This
does not promise instantaneous time-state freshness, a durable event outbox,
or every token's attached timestamp. In particular, memex bulk pitch marking
still lacks a request event. Existing request/decision snooze projection
predicates and the ignored explicit task-realm query are unchanged; polling
cannot repair an incorrect read predicate. Projects' own broad resource
architecture is unchanged apart from its shared open-Work candidates.

## Rejected all-background/direct-stream candidate: correctness and build identity

The selected follow-up restores `session.rs` to `32613ce8`, apart from two
`LiveFeed.work` test-literal fields. This is an exact removal of the original
long-stream presentation policy, not an unused-helper workaround. The adaptive
500 ms/rAF experiment is retained only as rejected evidence under
`/tmp/s15-bg/followup/final/` and `frame500-source.patch`; those artifacts must
not be confused with the selected build.

Selected production root: `/tmp/s15-bg/followup/revert-dist`. WASM SHA-256:
`1315ddd939189ebbf7761f4280f1b45c58da362fb5cb838a2929e22177ec42dc`.
Exact original baseline root: `/tmp/s15-bg/before-dist`, WASM SHA-256:
`ac73f4588225a27d4b06bcb0d4d3cc87207912b24c6eece3ee4bb48c67656cff`.

The corrected Work editor race passed alone in Chromium/dark, then the full
Work/Projects light/dark subset passed **4/4**. It holds real detail GETs and
Work PATCHes for project assignment and title: a pre-focus reply cannot rebuild the focused input or erase
its draft; an own successful mutation publishes immediately and clears pending
remote state; an older-generation GET cannot replace that success, including
after blur and the real quiet timeout. All later reads are held so they cannot
mask a stale-response bug. This regression first reproduced a genuine missing
`Page` context panic twice on the old artifact; capturing the page-owned
invalidator before the async boundary fixes the cause, without reordering
publication. Failed-artifact logs remain under `followup/resource-*`.

The selected Chromium workspace matrix passed **4/4** (desktop/narrow ×
Linux/Mac shortcut emulation), status-row geometry **12/12** (light/dark × six
widths), and motion helper units **13/13**. Node syntax checks and
`git diff --check` passed. Evidence: `followup/resource-selected-isolated.log`,
`resource-selected-matrix.log`, `workspace-selected.log`, `status-selected.log`
and `motion-unit-selected.log`. These do not qualify WebKit, a physical phone,
hardware INP, or the forced autosize fallback.

The focused workspace nextest selection passed **549 tests** (all c6-ui and
c6-head tests plus the real-loopback Work WebSocket integration). It includes
page ownership/sharing, hidden dirty-state/coalescing, quiet publication,
mutation-generation fencing, captured callback lifetime, committed head writes,
external semantic producers, and absence of Work hints on registry snapshots.
An integrated reconnect and hide-before-an-already-armed-200-ms-timer browser
case remains an explicit coverage limit; source review found no corresponding
defect, and the state/cleanup paths are covered separately.

Selected required checks passed with `ulimit -n 65536` in each shell:
`env -u NO_COLOR make ui`, `make check` (warning-free, test lint and UI size
ratchet passed), and `make test` (**1,294 passed, two skipped**, 219.143 s test
runtime). A collapsible-if warning first failed check and its isolated rerun;
the equivalent head-only condition was collapsed, without changing the selected
UI artifact. See `followup/build-selected.log`, `check-final-fixed.log` and
`test-final.log`. The subsequent six-pair direct-stream cohort failed its live
endpoint; see `followup/selected/` and the final disposition above. These checks
and sizes describe that rejected experiment, not the final baseline.

| Selected production WASM | Exact baseline bytes | Selected bytes | Delta | Remaining ceiling headroom |
|---|---:|---:|---:|---:|
| Raw | 10,594,434 | 10,626,832 | +32,398 | 97,948 |
| Brotli | 2,643,871 | 2,654,634 | +10,763 | 12,694 |

Ceilings remain 10,724,780 raw / 2,667,328 Brotli. No size budget was raised,
and no icons, classes or application features were removed to fit it.

## Original verification and size (before pre-merge follow-up)

Required sequence, in one resource-configured shell, passed:

```sh
set -euo pipefail
ulimit -n 65536
env -u NO_COLOR make ui
make check
make test
```

`make check`: no warnings, test lint and UI size checks pass. `make test`: **1,280
passed, two skipped**, no failures; Slack and fixture-upgrade tests completed
slowly (404 s / 194 s). Focused workspace nextest previously passed all 329
selected tests, including the new real-head realm create/rename/delete Attention
invalidation test. UI native tests cover visibility scheduler transitions,
held-notice deadlines/consumption/order, stream policy/trailing cleanup and
attention/reconnect transitions. No application view `.to_html()` calls added.

Both verified post-change production benchmark runs passed: ordinary draft keys
make no mention/suggestion/model/message calls; quiet input does not mutate
transcript Markdown; unrelated Sessions snapshots do not refresh realm/decision
counts; one Attention hint refreshes one shared count. Document-hidden fallback
intervals clear and resume once. Actual minimize/reveal, focus/defocus and
inactive/active 390 px pane transitions pause/resume the Work interval without
duplicates or loss of the 200-character session draft. Document visibility is
injected for deterministic scheduling; pane controls and viewport changes are
real workspace operations, not physical Android backgrounding.

The existing workspace browser assertion initially failed and failed again in
an isolated run: it counted **all** visibility listeners as “draft listeners.”
Captured callback identities showed both old draft callbacks removed. A second
tile adds two navigation owners while the source-row hover clock disappears,
explaining the net +1 without a retained composer callback. The test now scopes
the synchronous draft/autosize registration pairs, normalizes hover topology,
and also checks every old session callback disappears on detach and every moved
session/tile callback disappears on close. It does not simply increase an
expected listener count. The ownership baseline now truly starts without a
session (rather than an eagerly mounted inactive desktop companion). A later
Changes interaction also needed a harness correction: click the real desktop
toolbar, or the phone sheet, instead of clicking through a popover scrim. No
force clicks or relaxed ownership/copy assertions were used.

The corrected isolated Chromium desktop/Linux workspace run passed, then the
full **4/4** desktop/narrow × Linux/Mac shortcut-platform matrix passed. The
Chromium status-row geometry matrix passed **12/12** (light/dark × six viewport
or tile widths). Motion helper unit tests passed **13/13**; Node syntax and
`git diff --check` passed. Evidence: `/tmp/s15-bg/workspace-ownership.md`,
`workspace-cold-isolated-fixed.log`, `workspace-cold-chromium-matrix.log`,
`workspace-status-row-chromium.log`, `status-row/`, and `motion-unit.log`.
These are Chromium/emulated-platform results, not physical Mac/Safari/Android
proof. No WebKit or forced-autosize-fallback browser cohort was run for S15.

| Production WASM | Baseline bytes | Final bytes | Delta | Remaining ceiling headroom |
|---|---:|---:|---:|---:|
| Raw | 10,594,434 | 10,628,749 | +34,315 | 96,031 |
| Brotli | 2,643,871 | 2,648,890 | +5,019 | 18,438 |

The change fits within the starting approximately 130 KB raw / 23 KB Brotli
headroom. Final ceilings remain 10,724,780 raw / 2,667,328 Brotli. No icons or
Tailwind classes were removed to make the budget fit.

## Final confirmation, evidence, and historical series

The original local series below was based on `c6/next` at `8a5ac826`, authored
as Zachary Heilbron <zheilbron@gmail.com> without attribution trailers. It was
removed from the release branch for the bounded ablation. These hashes identify
historical evidence only; do not merge the original series to obtain the final
implementation. The final branch is exact baseline plus one audit/benchmark
commit; no runtime commits are retained.

### Five-pair confirmation: visibility plus counters rejected

All **20 trials passed** their 200-key/final-tail/error checks, alternating A/B
order across five quiet and five live pairs. No failed browser trial, outlier or
pair was excluded. The accepted amended gate had 96–99% idle CPU and queue 1–4;
per-trial one-minute load ranged 15.57–20.60. Repeats remain acknowledgement-paced
key→rAF→posted-task proxies, not hardware INP or independent-clock arrivals.

| Metric | Quiet base → candidate [run range] | Quiet paired Δ [pair range] | Live base → candidate [run range] | Live paired Δ [pair range] |
|---|---|---|---|---|
| p50 ms | 7.40 [6.10–7.90] → 7.50 [7.10–8.20] | **+0.10 [−0.50,+1.80]** | 9.60 [8.70–12.40] → 8.90 [8.00–12.80] | −0.30 [−3.50,+0.90] |
| p95 ms | 18.10 [17.90–18.40] → 18.60 [17.80–18.80] | +0.60 [−0.60,+0.90] | 199.50 [140.80–207.10] → 153.70 [139.40–278.80] | +9.60 [−60.10,+73.00] |
| Maximum ms | 20.00 [19.20–31.70] → 20.80 [19.30–23.60] | +1.20 [−12.40,+1.60] | 354.90 [272.10–474.40] → 339.90 [272.60–545.10] | +6.90 [−65.30,+70.70] |
| Task ms | 832.64 [779.69–915.06] → 865.03 [844.76–911.65] | +69.56 [−70.30,+118.18] | 10,038.39 [9,886.32–10,127.95] → 10,108.48 [10,030.42–10,190.27] | +131.90 [−58.82,+216.57] |

The +0.10 ms quiet result is small relative to its spread and is **not proof of
a real source-level slowdown**. It nevertheless fails the strict nonpositive
paired-median rule; it is not rounded away, replaced with a different endpoint,
or retried until favorable. Live p95's difference of build medians and median
paired difference point in opposite directions; both are reported instead of
choosing the favorable one. All raw values, source/build/harness hashes, order,
loads and browser logs remain under `/tmp/s15-bg/ablation/retained-results/`.
The word `retained` in that artifact path names the provisionally selected
candidate, **not code retained in the final release**. Its source is preserved
by the isolated patches and intermediate index trees, without release commits.

That candidate passed warning-free `make check`, **1,273 native tests, two
skipped**, Chromium workspace **4/4**, status-row **12/12**, and Work/Projects
resource lifecycle **4/4**. Correctness success does not waive the performance
criterion. Fresh idle-minute controls were deliberately not started after the
confirmation failed; the slot ended and the phone helper was released. No more
timing cohorts were run to rescue a change before the bounded release deadline.

### Idle evidence and remaining work

Existing real-minute observations remain labelled by implementation: original
baseline **39 HTTP requests/min**, the earlier counter/visibility-containing
build **13/min (12 Work + one quota)**, and the rejected Work-event build
**one quota/zero Work**. The latter's CPU work was not a clean exclusive control.
None of the experimental request reductions is a final release improvement:
the final runtime is unchanged baseline, and duplicate Work polling remains.
The prior selected cohort's attempted fresh idle controls also never started
because its original gate failed twenty windows. No idle count is extrapolated
from typing-only repeats or from a rejected gate window.

Follow-ups are evidence-backed patches, not new task cards: repeat the
visibility/counter confirmation with stronger replication and characterize
variance; separately
revisit held-notice/stat work and semantic Work invalidation/shared resources.
The earlier profiles show large full-prefix live Markdown/layout costs, but do
not establish which remaining source change caused a whole-build regression.
No IO→RO rewrite or new stream policy is smuggled into this audit-only result.

### Final baseline verification and size

After discarding every runtime change, the release branch is directly based on
`8a5ac826`, with only this audit, its standalone benchmark and documentation
links added. Production Rust/JS/styles/assets and pre-existing native/browser
tests have no diff against the base. The earlier implementation and browser
fixture corrections are not retained. There is no added application-view
`.to_html()` or removal of features, icons, classes or drafts.

Final checks used `set -euo pipefail; ulimit -n 65536` in each shell:

- Fresh `env -u NO_COLOR make ui` passed with the unchanged production profile;
  see `ablation/final-base-build.log`. The isolated build cache was selected
  with `CARGO_TARGET_DIR`, not profile or optimization changes.
- Fresh `make check` after that UI build passed warning-free, including test
  lint and the actual final UI size ratchet; see
  `ablation/final-confirmed-check.log`. The earlier overlapping check saw the
  rejected candidate's old dist; its size output is not the final size.
- Final baseline `make test`: **1,268 passed, two skipped**, 439.350 s test
  runtime; see `ablation/final-base-test.log`. The two slow tests passed.
- The standalone benchmark's default behavioral controls passed **2/2**
  quiet/live typing-only smoke runs against baseline, with all 200 keys,
  complete drafts, final live tail and no browser errors. These overlapped
  builds/tests and are **functional checks, not qualified timings**; see
  `ablation/final-bench-smoke-{quiet,live}.log`.
- Final Chromium status-row **12/12** and Work/Projects resource lifecycle
  **4/4** passed without retries, against the fresh baseline dist; see
  `ablation/final-base-status.log` and `final-base-resource.log`.
  The unchanged motion helper's unit suite passed **13/13**; see
  `ablation/motion-unit.log`. Node syntax and `git diff --check` also passed.
- **Unresolved baseline workspace harness failure:** the unchanged
  `workspace-open.cjs` failed in its first Chromium desktop/Linux case at
  “Changes companions keep explicit-copy behavior without copying the
  transcript.” Its click on `Changes` is intercepted by the open
  `.c6-presence fixed inset-0 z-40` scrim until the 300 s wrapper closes the
  browser. The exact case was rerun alone against the original baseline dist
  and reproduced the same interception and deadline. See
  `ablation/final-base-workspace.log` and
  `final-base-workspace-isolated.log`. No force click, timeout increase or
  fixture fix is retained; the full final baseline workspace matrix is not
  verified. The status/resource suites above were subsequently run separately.

Final WASM SHA-256 is
`ac73f4588225a27d4b06bcb0d4d3cc87207912b24c6eece3ee4bb48c67656cff`,
**byte-identical to the original baseline WASM**. All JS, CSS and other assets
also match; the rebuilt `index.html` (and its Brotli form) differs only in the
order of two modulepreload links. Thus the whole dist is not claimed to be
byte-identical; see `ablation/final-dist-comparison.txt`.

| Final baseline WASM | Baseline bytes | Final bytes | Delta | Ceiling | Headroom |
|---|---:|---:|---:|---:|---:|
| Raw | 10,594,434 | 10,594,434 | **0** | 10,724,780 | **130,346** |
| Brotli | 2,643,871 | 2,643,871 | **0** | 2,667,328 | **23,457** |

No budget increase was needed. There is no new WebKit, physical-device,
forced-autosize, hardware-INP or independent-clock-arrival evidence. Final
verification ran after the exclusive measurement slot ended, not as a new
performance cohort. All evidence remains local under `/tmp/s15-bg/`.

### Historical commit identifiers

| Commit | Change |
|---|---|
| `b0768d18` | Owned visible-only fallback intervals, relative clocks and real pane visibility. |
| `32613ce8` | Event-derived held notices with one grace-deadline timeout; shared formatted turn stats. |
| `4ca21f91` | Bounded long-stream Markdown presentation; rejected and not retained. |
| `d1632c2e` | Shared push-driven realm/decision counts, Attention metadata invalidation, reconnect-only version refresh. |
| `c89b96d3` | Historical benchmark/ownership tests and report; current evidence is a separate new commit. |

Unchanged baseline work / follow-up limits (not new task cards):

- Quotas, Brief generation, mail outbox, workflow runs, model-call detail and
  activity still need visible fallbacks because existing global push is
  incomplete. Removing their timers requires corresponding server events first.
- Work's duplicate navigation resources and broad snapshot invalidation were
  replaced only in the rejected Work experiment. They remain in the baseline
  source, with visible fallback polling. Projects' own reads still have broad
  registry/doc invalidation. Work's push/shared patch needs further investigation
  before it can be retained; its one-slow-fallback behavior is not shipping.
- Three local session presentation clocks remain unconditional. The held-notice
  history scan and repeated turn formatting also remain: their replacement was
  discarded after the short screen failed the quiet endpoint. No claim of their
  removal applies to the final baseline source.
- Search highlighting still uses a broad 200 ms all-root rescan, and the separate
  Chat composer still saves text synchronously per key. Neither was active in
  this session-composer benchmark; their cost is not established by it.
- Keep healthy sockets alive while hidden for notifications/current state.
  Initial reads, push-triggered reloads and bounded error retries remain. This
  is not a claim of zero background activity or zero hidden-page network I/O.
- More aggressive streaming optimization (incremental Markdown/DOM patching)
  could reduce remaining rendering cost, but risks Markdown semantics and needs
  separate correctness and profile evidence. The presentation-cadence fix was
  measured and reverted: the final baseline preserves direct feed-frame/equality
  Memos, with the original full-prefix Markdown rendering cost. Neither the
  original throttle nor the adaptive/rAF experiment remains in the final
  source, and no authoritative text was dropped to improve a benchmark.
- The six-pair follow-up above replaces speculation from the old quiet direction
  reversal. Its spread and all outliers remain reported; neither one trial nor
  six pairs prove every possible typing workload is faster.

No push, PR, deployment, service restart, live settings change, file publication,
decision or task card was performed.

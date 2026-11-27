# UI clipboard inventory

Audit: 2026-11-27, `crates/c6-ui/src` (including `chat/`, `util/`), plus the
file-viewer and terminal JS bridges. “Before” is `origin/main` at `e6bcb026`.
The table records **what is copied**, not just whether text can be selected.
Native browser selection remains available throughout; normal input/textarea
editing already supports Select All / Copy and is not given a duplicate button
for every field. Navigation lists deliberately do not grow a button on every
title, date, badge, icon, or counter: copy the underlying item in its detail view.

## Contract and implementation

`components::CopyButton` and `components::copy_button_html` are the two rendering
entry points for **one control** (`.c6-copy`), sharing `viewer-controls.js`'s
clipboard handler and the same icon, labels, status, and CSS. The HTML entry point
is needed for sanitized Markdown/file-viewer HTML, not a second clipboard
implementation. All previous Rust clipboard writes and the viewer-specific copy
handler have been consolidated here. No new dependencies.

* Default is the stored source, not `innerText`, a summary, a filename, or cleaned
  citation display. User mentions retain their source spelling. Markdown retains
  formatting, links and fences. JSON encoding protects controls, CR, NUL, Unicode,
  quotes and HTML through attributes. Hidden viewer source also uses JSON rather
  than HTML text parsing, which normalizes CR and NUL.
* Text publications/attachments fetch the **specific version URL**, including its
  realm, only when copying. Binary files offer explicitly named Copy link/path.
  Worker file viewers load uncapped full source on explicit Copy, retaining URL
  validation/cancellation; their 2 MiB display cap and ordinary 40 MiB preview-read
  ceiling do not silently truncate clipboard content. Pathless code fences retain
  full copy source even when display is capped. Network/clipboard failures say
  “Couldn't copy”, never “Copied”; there is no success claim before the promise
  resolves. Already-truncated stored data cannot be reconstructed by the UI.
* Controls appear on hover/focus within their containing block on fine pointers,
  remain visible on coarse pointers, use native keyboard buttons and aria-labels,
  and keep their icon alongside a live “Copied” status. Captioned session-ID/menu
  controls remain visible. Capture-phase handling prevents Copy from also opening
  a row, selecting a doc block, or toggling a message action.
* Tables explicitly copy the **currently displayed** headings/rows as TSV, not
  unloaded pages or unrounded underlying analytics values. No general Copy as
  plain text menu was added; source Markdown is the unambiguous reply default.

## Surfaces

| Page / component (source under `crates/c6-ui/src`) | Before: action and payload | Action / payload after this change (or deliberate existing behavior) |
| --- | --- | --- |
| Transcript user bubbles, including delegated/card-origin messages (`session.rs`) | No Copy; visible text can differ from source mentions | Copy message: original `EventKind::User.text`, not expanded `prompt` or rendered mentions |
| Task-card-origin prompt (`session.rs::CardSource`) | Subject/rest split, no Copy | Copy message: entire original text, including hidden rest |
| Queued messages (`session.rs::Queued`) | Take back / send, no Copy | Copy queued message: queued text |
| Expanded hook/command input (`session.rs`) | Disclosure and `<pre>`, selection only | Copy expanded prompt: full `prompt`, separately from original user message |
| Assistant replies (`session.rs`) | Rendered Markdown, no Copy | Copy as Markdown: original text block, prior to rendering and citation cleanup |
| Streaming replies (`session.rs`) | Live rendered text | Reply Copy appears on persisted reply; live tool-output Copy uses the current buffer until its result arrives |
| Thinking / summarized updates (`session.rs`) | Disclosure / rendered Markdown | Copy thinking as Markdown when expanded; Copy summarized update: original text |
| Tool invocation (`session.rs::ToolBody`) | Summary / shortened readable input, no Copy | Copy tool input: original `input_json`; pretty JSON fallback only for callers without original JSON |
| Tool results (`session.rs::ToolOutput`) | Bounded scrolling display, no content Copy | Copy tool output: full stored result `output`, preferring it over live/stale output; no display clipping |
| Side chats (`session.rs::side_chat`) | Nested transcript | Inherits all transcript message/tool/error copy actions |
| Subagents (`session.rs::subagent_thread`, `SubagentPeek`) | Last-three-step shortened peek or nested transcript | Open subagent transcript for full message/report Copy; do not mislabel the shortened peek as complete content |
| Task reports / notices (`session.rs`) | Metadata and raw-agent-text disclosure, no Copy | Copy notice text; Copy raw notice copies entire delivered agent-facing text; nested reports also inherit transcript Copy |
| Session start / runtime handoff (`session.rs`) | System-prompt disclosure, no Copy | Copy system prompt: full stored prompt |
| Worktree setup (`session.rs`) | Setup-log disclosure, no Copy | Copy setup log: full stored log |
| Compaction (`session.rs`) | Rendered summary, no Copy | Copy compaction summary as Markdown: original summary |
| Approval card (`session.rs`) | Readable command/JSON in `<pre>` | Copy approval input: entire shown command/JSON, not summary label |
| Transcript errors / other errors (`util.rs::ErrorBox`) | Full text, no Copy | Copy error: full error string, multiline causes included |
| Text transcript attachments (`session.rs::user_attachments`) | Preview/open/download by name, no content Copy | Copy attachment content: fetch actual attachment URL, never attachment name |
| Binary transcript attachments (`session.rs`) | Thumbnail / named file link | Copy attachment link: absolute origin plus attachment URL |
| Published outputs panel and transcript publication row (`outputs.rs`) | Versions, previews, Download; no content Copy | Copy output content: text/Markdown/HTML/JSON/XML/SVG version bytes decoded as text; binary Copy output link |
| File viewer shared header (`file_viewers.rs`, `files.rs`) | **Only Copy path**, passed `Input.path` (often the output's name) | Separate Copy content and Copy path; binary Copy file link/path. A name/path is never used as content |
| Markdown code fences (`markdown.rs` → `file_viewers.rs::fence`) | Shared viewer; pathless fences had no Copy | Copy content: complete code text, without language/fences/line numbers |
| JSON tree nodes (`file_viewers.rs`) | Small copy icon copied JSON Pointer | Same pointer, now shared control; full JSON Copy content is separate |
| CSV / TSV / JSONL viewers (`file_viewers.rs`) | Sort/filter/paging, path copy only | Copy content copies original complete source, not sorted/filtered/truncated rows |
| Logs / Mermaid / diffs (`file_viewers_extra.rs`) | Shared file viewer, path copy only | Copy content copies original log/diagram/diff source, not rendered chart or visible window |
| Session diff (`files.rs::Diff`) | Per-file collapse / bounded diff, no Copy | Copy diff: full available diff; when backend truncates, label explicitly says truncated at 4 MB |
| Changed-file paths and branch (`files.rs`) | Displayed values, selection only | Copy path and Copy branch: complete value |
| Markdown worker path links (`markdown.rs`, viewer JS) | Open/popover file action | Link navigation unchanged; opened shared viewer offers original path plus actual file content; no nested button in anchor |
| File chooser (`files.rs`) | Editable/selectable path fields and browse/open/download | Keep native field Copy; actual picked content/path copies in viewer/transcript |
| Session IDs / session links (`details.rs`, session menu/header/info) | `CopySession` copied full ID / realm-qualified absolute URL, custom feedback | Same payloads and accessible labels, shared control; short display never becomes payload |
| Worker IDs / location / branch (`details.rs`) | Short/link labels, selection only | Copy worker ID, folder path and branch: full values beside navigation actions |
| Fleet worker rows (`fleet.rs`) | Hostname and worker navigation, selection only | Full worker-ID and hostname copy controls |
| Enrollment (`fleet.rs`) | Custom Copy command; claimed success without awaiting; insecure fallback selected input | Shared Copy enrollment command awaits success; readonly field still selects on focus for manual insecure-context copy |
| Machines (`machines.rs`) | Resource charts / host/version/path fields, no Copy | Full hostname, machine ID, version, data-directory Copy; charts remain visual and selectable labels are unchanged |
| Ask (`assistant.rs`) | Editable question starts an assistant session | Question editing retains native Copy; opened conversation inherits user/reply/tool Copy |
| Chat messages / threads (`chat/message.rs`, `chats.rs`) | Message text selection; ad-hoc Copy link | Copy message: stored message body without sync's speaker prefix; Copy link: original URL through shared control |
| Chat attachments (`chat/message.rs`, `mail.rs::FileChip`) | Images / Slack/open links | Shared file chip copies same-origin stored text attachment content, else explicit attachment link (external Slack files are not fetched as text) |
| Chat settings (`chat/settings.rs`) | Ad-hoc manifest Copy with optimistic success note | Shared Copy manifest: full manifest text |
| Chat composer/settings fields (`chat/composer.rs`, `chat/views.rs`, `chat/mod.rs`) | Editable drafts/config values | Native selection/Copy retained; sent messages use message controls |
| Mail threads (`mail.rs`) | Text or sandboxed HTML mail; selection only | Copy email text: full stored plain body; HTML mail additionally offers Copy email HTML source |
| Mail attachments (`mail.rs::FileChip`) | Icon/name/size/open link | Copy attachment content for same-origin text; Copy attachment link for binary/external attachments |
| Mail settings (`mail.rs`) | Selectable OAuth redirect URI | Copy redirect URI: full URI |
| Mail compose (`compose.rs`) | Editable draft body/addresses | Keep native editor Copy; no duplicate action for each draft field |
| Document reading (`docs.rs`) | Rendered blocks, export/download; selection only | Copy block as Markdown: original block text; export remains the whole-document route |
| Deck reading (`docs.rs`, `deck.rs`) | Rendered slides, notes, presentation/export | Copy slide as Markdown: ordered child text joined with blank lines; separate Copy slide notes; export for structured whole deck |
| Document/deck editing (`docs.rs`, `deck.rs`, `word_diff.rs`) | Native text/editor selection / rendered revision diffs | Keep native editor selection; source Copy on reading view, not a lossy reconstruction of rich chip attributes |
| Discuss on docs/cards/decisions (`docs.rs`, `decisions.rs`) | User text / agent Markdown / unsent text, no Copy | Copy comment/message source (including unsent comment); agent Markdown copied before rendering |
| Decisions (`decisions.rs`, `understanding.rs`) | Document and choices / Discuss | Copy decision as Markdown: original `document`; choices are action/navigation controls, not duplicated clipboard buttons |
| Task-card “What the new session is told” (`decisions.rs`) | Collapsed prompt, no Copy | Copy task prompt: complete `p`, not task title |
| Work detail (`work.rs`) | Title / done-when display | Copy work item: title and done-when; to-do Copy uses full prompt (or text fallback), not truncated row title |
| Work lists/projects (`projects.rs`, `goal.rs`, `work.rs`) | Navigation cards / editable fields | Keep navigation/native fields; underlying item detail supplies source Copy |
| Session follow-ups (`capture.rs`) | Editable/displayed saved text and quote | Copy follow-up: whole saved text; source quotes retain their jump links |
| Task processes/captured streams (`tasks.rs`) | Command and stdout/stderr in `<pre>` | Copy command / Copy captured stream: complete stored command/stream, not preview/size label |
| Call monitor (`monitor.rs`) | Prompt/reply/thinking/details, cleaned display | Copy each part: original field before citation display cleanup |
| Auth API tokens shown once (`auth.rs`) | Select-all token and CLI login command | Copy API token / Copy login command: complete value; no retrieval of expired/hidden tokens |
| Private secret cards (`session.rs::SecretCardView`) | Ad-hoc Copy, optimistic success, secret dismissal | Shared Copy secret: exact secret; dismissal unchanged |
| Settings prompts (`settings.rs::PromptSections`) | Editable text / reset/save | Copy prompt: entire current draft, including edits |
| Settings connections / webhooks (`settings.rs`) | Connection base URL / select-all webhook URL | Copy connection URL / Copy webhook URL: exact URL |
| Settings secret store (`settings.rs`) | Write-only replacement inputs, no readback | Native input Copy only; never invent a way to copy hidden encrypted credentials |
| Other settings (`settings.rs`, `models.rs`, `aspects.rs`, `quota.rs`, `labels.rs`) | Native inputs/selects / policy descriptions | Keep native field Copy; source prompts/URLs/tokens have explicit controls above |
| Backup key (`backups.rs`) | Select-all key | Copy backup key: full shown key |
| Terminal (`term.rs`, `js/terminal.js`, vendored xterm) | Native terminal selection + browser copy event; no auto-copy merely on selection | Keep xterm's selection/keyboard Copy. Do not treat Ctrl-C without a selection as Copy: it is a terminal interrupt |
| Usage (`usage.rs`) | Rendered per-model table, no export/copy control | Copy usage table (TSV): visible headings/cells, current filters |
| Analytics (`analytics.rs`) | Summary/waits/providers/sessions tables, no Copy | Copy analytics table (TSV) on each table; displayed values, not unseen raw series |
| Collections (`collections.rs`) | Record table / JSON detail / schema editor, no Copy | Copy records table (TSV); Copy record JSON: full fields object; schema/native editors keep standard Copy |
| Custom data views (`views.rs`, `itemviews.rs`) | Query/card/table views and Markdown templates | Copy view table (TSV); record navigation and template editors remain native; code fences inherit shared content Copy |
| Memex items (`memex.rs`) | Source text and original links, no Copy | Copy item text: full stored text, not title |
| Explorer object fields (`explorer.rs`) | Detail field `<dd>` text, no Copy | Copy named field: entire field value |
| Calendar (`calendar.rs`) | Description in bounded pre, selection only | Copy event description: complete description |
| Photos (`photos.rs`) | Image + stored extracted text, selection only | Copy photo text: whole stored text; images are not misrepresented as clipboard text |
| People/contacts (`people.rs`, `contacts.rs`) | Navigable people/identity cards and editable fields | Retain native link/field selection; no change to identity navigation |
| Search/activity/catchup/brief/needs (`search.rs`, `activity.rs`, `catchup.rs`, `brief.rs`, `needs.rs`) | Summaries and links to underlying items/sessions | Copy underlying full content in its detail/transcript, rather than labeling shortened snippets as full text |
| Adoption/watchers/notifications (`adopt.rs`, `watchers.rs`, `notify.rs`, `push.rs`) | Forms, status summaries, action/navigation controls | Native editable Copy; shared ErrorBox handles errors, underlying sessions hold full content |
| Session lists/archive/timeline (`sessions.rs`, `session_archive.rs`, `timeline.rs`) | Navigation summaries, no source-copy buttons | Source copying lives in session detail/transcript; session IDs use existing shared detail/menu route |
| Navigation/infrastructure (`main.rs`, `workspace.rs`, `panes.rs`, `place.rs`, `resources.rs`, `api.rs`, `marks.rs`, `icons.rs`, `when.rs`, `session_ring.rs`, `completion.rs`, `mentions.rs`, `mail_html.rs`, `file_formats.rs`, `voice.rs`, `util/*`) | Routing/rendering/formatting/transport helpers, not independent content pages | No new clipboard action on routing, timing, decorative marks, model pickers, audio plumbing or utility internals; consumers above own copy semantics |

## The “copies the name” cause

The shared viewer's only copy button passed **`Input.path`** as `data-fv-copy`.
Output/file previews supply their filename as this path, so that action could
only copy a name/path, never the displayed content. Pathless fences did not even
get that button. Transcript tool results and the publication panel had no
dedicated content-copy action. There was no existing content-copy implementation
to correct by merely swapping a label. The fix separates content from path/link
at the common renderer and adds version-content copying at publication surfaces.

## Verification handoff

Implementing-agent process: **no cargo, make, trunk, Docker builds, test suites,
provider calls or evals run**. Direct `rustfmt`, `node --check` and `git diff
--check` are syntax/format checks only. Integration must build, run the tests and
check `scripts/ui-size.sh`; no claim is made about a measured size budget.

Written tests:

* Rust: original user text/mentions vs expanded prompt; raw assistant Markdown;
  large stored tool result vs stale live buffer/name; original tool JSON; full
  error; JSON-safe control characters/HTML; code fence source, separate filename
  copy, full >2 MiB pathless source and downloadable-source fetch marker.
* `tests/viewer-controls.test.cjs`: retained icon/success, permission rejection,
  complete file source vs capped display/path, publication version-content URL.
* `tests/copy-actions.cjs`: actual production WASM in Chromium, private loopback
  fixtures with clipboard-read/write permissions; keyboard-click prompt/reply,
  code, tool input/full output, publication and error Copy, then read clipboard.
  The full browser runner includes this Chromium-only suite and the
  `viewer-controls.test.cjs` unit checks. It runs fresh desktop and 390px touch
  contexts, asserts hover/focus/touch visibility and viewport fit, and retains
  `copy-1440.png` / `copy-390.png` screenshots in its evidence directory.

At the implementation handoff, remaining verification was Rust type checking/clippy, all unit/browser suites,
clipboard policies on non-Chromium/insecure origins, touch/visual placement, and
compressed UI budget. Native terminal selection behavior was inspected in the
bridge/vendor, not exercised against a live shell. No live head/workers touched.

## Integration verification

The final Linux Docker release build measures **11,646,584 raw / 2,907,978 Brotli
bytes**, within both existing size allowances; no budget increase is needed.
Native and WASM Clippy and `make check` pass after correcting the slide-copy
view expression and using a consistent `&str` for JSON source encoding.
All 95 JavaScript unit tests and 14 motion VM checks pass. Production-WASM
clipboard reads pass in fresh 1440px desktop and 390px touch contexts for every
payload listed above. Both screenshots were inspected: desktop controls reveal
on hover/focus, phone controls remain visible, and neither layout overflows or
adds a competing action cluster. The motion tile fixture now uses the real
platform modifier (`ControlOrMeta`), since native macOS Control-click opens a
context menu rather than dispatching the production new-tile click.

The first full browser cohort exposed pre-existing failures, retained with its
evidence rather than discarded. Each representative failure reproduced twice
against the exact `e6bcb026` release assets as well as the copy branch:

* Transcript-flow assertions must skip `hidden` recovery notices, just as layout
  does; they still require the preceding visible reply and the exact flex gap.
* Ask uses the platform Ctrl/Cmd modifier; macOS textarea end-of-document uses
  Cmd+Down. Native typing now exercises those real gestures, not Linux shortcuts.
* New-session tests follow the current Worker/Model/More chips, with effort under
  More, retaining model-input identity, shared model caching, nested Escape,
  settings persistence and passive-versus-pinned preview checks.
* Native WebKit's scrollbar intercepts the last pixel of a right-aligned
  queue action at the old 16px gutter. An isolated 24px-gutter experiment passes
  the unchanged desktop four-edge hit tests; touch needs a 32px gutter so the
  additional transparent 8px target halo also clears the native scrollbar.
  Waiting for scrollbar fade
  did not resolve it and was not retained. No forced clicks, retries, fixed
  delays or relaxed edge assertions were added.

The first full Rust run retained five failures (2,092 passed); each also failed
twice in isolation before correction. Markdown tests now distinguish the
rendered body from the deliberately raw JSON copy attribute and assert both
independently. Icon tests enumerate the new Copy control explicitly, including
the path-only binary viewer without a download URL. Binary attachment/output
links use a shared origin helper that does not call browser bindings in native
rendering; native tests assert those payloads and the real browser clipboard
suite covers their absolute addresses.

The full native cohort also exposed two literal Ctrl tile-switch shortcuts in
session-sheet isolation; these now use the same real platform modifier as the
app. The app already maps either Meta or Control to `How::NewTile`, and the
workspace-open Mac cases exercise actual Meta-clicks, not only dispatched
events. Theme, message-edit and session-sheet now shard existing independent
case groups instead of racing whole accumulated suites against one worker/case
deadline. The scheduler tests retain all groups and explicit selector behavior;
no deadline was enlarged. Final full native and Linux runs use four browser
workers, with no compilation or Rust-test run competing for the machine.

The next native cohort passed 128/130 shards and exposed a real modal bug in
WebKit, reproduced twice with both fresh-process and connected browsers. Safari
can skip native button Tab stops; a boundary-only trap then lets Tab escape
from an interior search input to the body. `c6Modal` now owns the complete
Tab/Shift-Tab cycle, retaining its disabled/hidden/inert filtering and opener
restoration. The new deterministic VM regression fails on the old handler and
passes on the corrected one; phone-picker retains its original containment
assertion and now includes active-element/eligible-control diagnostics.

The preserved settings-opener focus assertion then exposed Safari's separate
button-click behavior: clicking Model does not natively focus it, so the shared
panel's fallback opener was Worker. This reproduced twice in isolation. Start
settings chips now focus the actual clicked button before opening, allowing
Escape to restore the correct opener without changing the assertion.

Session-target navigation then exposed a disposed-reactive-value panic in all
four Chromium workspace cases. It reproduced twice on this branch and twice on
the exact pre-copy baseline. An exact-byte reproduction of the production
optimized WASM with its symbol map located the queued render effect in
`SessionLocation`'s model chip: the persistent tile bar borrowed signals from a
transcript that navigation had already disposed. Keying the bar alone cannot
order two separately queued render effects. Borrowed controls, location and
menu now have explicit reactive owners which are synchronously paused (including
nested render effects) by transcript cleanup, before its signals are removed.
The keyed bar still owns DOM removal; no transcript or bar owner is retained by
the other. A deterministic lifecycle regression checks recursive pausing before
signal disposal, an unaffected persistent bar and weak ownership after unmount.
The browser's original no-panic and navigation-history assertions are unchanged.

The next full cohort reached later workspace phases and exposed physical
Ctrl-clicks in its Linux-platform emulation on a macOS host. Both failures
reproduced twice alone. `navigator.platform` does not change the host's native
context-menu gesture: native Mac cases retain real Meta-clicks, native Linux
cases retain real Ctrl-clicks, and only Linux-on-Mac emulation dispatches an
explicit Ctrl-click event. Every destination, deduplication, draft and history
assertion remains intact, as does the separate Mac context-menu check.

That cohort also exposed the baseline's location strip adding 67px below the
single phone bar; this reproduced twice on each tree. On phones, Worker,
Folder/Worktree and Model now live in the ring's Session details page instead of
adding another fixed header. Desktop location controls stay in the tile bar.
The unchanged phone-bar suite still requires actual content within 44px of the
top; the session-sheet suite additionally requires the phone location summary
and model control to be reachable in Details, with no strip in the tile.

The narration browser assertion predates shared Copy and prohibited every
button inside a summarized update. Its two failures reproduced alone. It now
still prohibits disclosure elements and unimplemented full-text actions, while
requiring the exact complete set of Copy buttons and their original per-item
Markdown payloads (including the long wrapping summary), rather than merely
allowing arbitrary new buttons.

Native press cancellation/multitouch then reproduced twice on each tree. The
fixture described cumulative reply snapshots but sent them verbatim as
`StreamMsg::Text` deltas; repeated prefixes inflated the stream enough to scroll
the original recovery control above the viewport. Its helper now emits only the
appended suffix and waits for the exact rendered snapshot. A deterministic unit
test checks the wire deltas and rejects non-append snapshots. All existing
native-input, >=22px layout-growth, end-following, full-visibility and hit-test
assertions remain unchanged; neither payload snapshots nor viewport shrank.

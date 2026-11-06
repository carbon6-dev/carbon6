# A session's tile bar, panes and glance — Design

Status: phone revision and first desktop combined ring implemented (2026-11-06). The **Revised direction** tab of doc:01a19e63-08ac-765f-8d88-21bb8c7a27ff is the design source. Phones have one navigation bar, a conditional status row above the composer, and no footer. Desktop keeps panes beside the transcript and the read-only glance.

## 1. The tile bar

Desktop, left to right: the session's identity dot, title (double-click to rename), optional aspect shapes, then **combined ring, Changes, Terminal, Pin, Actions, Focus, Close**. The identity dot is independent of agent state. Back/Forward, detach and minimize are in Actions, retaining their icons and keyboard shortcuts. Desktop dragging still reorders tiles. Tiles share the available workspace width without a minimum tile width or horizontal canvas scrolling; their contents must adapt to the tile rather than the window.

Fine-pointer desktop slots use the tile's own width: 36px at 40rem and above, 32px at 30–40rem, and compact 28px below 30rem. Phones and coarse pointers keep 44px targets. Below 30rem of tile width, Changes, Terminal and Pin move into Actions with the same icons. The combined ring, Actions, Focus (Maximize) and Close remain in the bar, leaving the title room to truncate even in a 240px tile. Focus is immediately left of Close, after Actions, at every width where they appear. Below 14rem Focus and Close also move into Actions, in that order, so even four columns at the smallest desktop breakpoint retain usable chrome. No action is removed.

Optional aspect shapes yield first: below 40rem of tile content width they are hidden, before secondary controls collapse below 30rem. The title and core controls keep their room; the aspect preference and phone behaviour are unchanged.

A two-pane tile below 40rem uses the existing phone navigation: one pane at
a time, with **Back to the list** in the item's slim header. Both panes stay
mounted, preserving drafts and scroll. Above that width, both appear beside
each other and each view responds to its own pane's content container—not
the window width or even the whole tile's width.

- **Combined ring:** a standalone 24px SVG with a neutral context-used arc inside a complete state rim: grey idle, blue working, amber waiting, red failed. Waiting is dashed and failure has a cross, so colour is not the only cue. A badge appears only for a positive count of distinct background jobs. Its accessible name and tooltip spell out state, context and jobs. The own main-agent state does not become working just because background work runs. Clicking opens the session overview, even with a glance column visible.
- **Identity colour:** Settings → Appearance → **Session bar colour** uses the shared immediate-setting **Coloured stripe** switch: on (default) keeps the stripe; off is **Dot only**. The identity dot remains in either case. Saving disables the switch, and a failed save restores the previous choice and reports the error. This is independent of aspect marks.
- **Optional aspects:** Settings → Appearance → **Session aspects → Show on tile bars**, using the shared Switch, is **off by default** so the default bar remains as in PR29. On desktop only, the chosen aspects appear after the title as shapes in their own colours/patterns, never named chips. They share the title's flexible room, not the ring or fixed control slots. Resting on a value for 400ms dims tiles without it; leaving cancels it, clicking opens its card (related sessions, destination and palette). The choice is `session_facets.tile_bars`, stored per realm alongside `shown`; missing fields from old saved settings/heads mean off, with no migration. One workspace-level gate unmounts the entire feature off/on phone: no per-tile mark signals, scans, listeners or timers when off. Enabling/disabling does not remount the bar's controls or the transcript.
- **Buttons:** tile-bar and session-strip controls use 28px slots on fine-pointer desktops and 44px targets on phones or coarse pointers. Changes and Terminal are filled only while their pane is on screen. Over SSH, Changes remains available but disabled. Pane headers retain their existing sizing.
- **⋯:** Rename… · Find in session (⌘F) · Files · Outputs · Session details… (usage, and what applies: labels, slash commands, agents, skills, hooks) · New like this… · Save as template… · the **Catch-up** and **Fold finished turns** switches (docs/catchup.md) · Archive.
- With another view beside the session in its tile (a doc), the bar speaks for that view; the session's icons and ⋯ are in a slim strip at the top of the session. A pane of its own beside it leaves the bar the session's.
- **Gone by default:** the second bar and its tabs, the drawer, ⚙, the ◆ pill, the aspect marks (and their hover dimming). Only aspect marks can be explicitly restored; phones remain unchanged.

### Phone

One 44px bar: **app menu, title, plain context ring, Actions, Sessions picker**. There is no app header above it and no footer/bottom tabs. The context ring's used arc is neutral grey, with no animation, state rim or background badge on phones. It opens the session sheet; **Changes, Terminal, Files and Outputs** live there, with the existing glance and background work. Phone pane buttons are not repeated in the header or Actions.

- **App menu:** positive app-wide waiting count on the hamburger; realm selection (current row highlighted without a check), search, app navigation, Activity, notifications, connection and version. A disconnected indicator appears on the hamburger. Realm and Activity are no longer separate phone-header rows.
- **Sessions:** one flat list of open sessions/pages, including hidden ones, with a current highlight and close controls; recently viewed, All sessions and New session or page follow it. Ask and the Needs you bell are in this sheet, centred on one header line with matching 20px visual pills inside 44px touch targets. The bell occupies the rows' close-control column; there is no header X. Tap outside, tap or swipe down on the handle, or press Escape to close. The handle is a native button named **Close Sessions**, including for screen readers. The old long-press/reorder footer is removed.
- **Status:** immediately above the still-mounted composer, outside the transcript scroll area, a 44px row exists only for **Working…** (calmer spinning loader-circle), **Waiting on you N** (triangle-alert), **Last turn failed** (circle-x), or positive **Background N** (hourglass). All icons are drawn SVG. Background work is independent of main-agent work. **Show** opens waiting cards, failure details and task/subagent controls; appearing/updating the row never takes focus from the composer. Its keyboard focus treatment stays inside the smaller visual button within the 44px target. Linked child/task records count once; foreground native work and read-only mirrors do not inflate the count. Completed history and held notices remain in the session overview when idle, without an idle row. Desktop retains its existing transcript task/waiting affordances.
- Row insertion/removal consumes transcript height, not composer position. Following stays bottom-anchored; reading older content stays put. The visual viewport bounds the app, sheets and dialogs with an overlay keyboard. Landscape uses this same layout when height is below 512px.
- **Motion:** the status row yields its height once, immediately, with a compositor-only rise/fade; its exiting visual stays out of flow and inert. The existing ResizeObserver still snaps following readers to the end without gliding. Session/status sheets and the glance retain their bodies through the exit and reverse if reopened; closed trees are inert immediately, and heavy bodies unmount after presence ends. Phone sheets use the shared 280ms spring enter/200ms exit and 30% scrim; desktop glance/panels use the shared 40px slide/fade (220ms enter/150ms exit). These effects never animate streaming text or delay input; the shared motion preference controls reduced/off behavior.
- **App drawer:** on phones the ☰ drawer enters from and exits to the left, with the same sheet timings and uninterrupted reversal. It accepts input from its first frame. System follows the OS; Reduced fades without travel and Off is immediate. Its keyboard-aware height and 288px width (capped to the viewport) stay unchanged.
- **Composer sizing:** working/suggestion changes refit only an empty draft's hint. A nonempty draft still resizes for text and width changes, but not for a status-only update: temporarily measuring it at `height:auto` can make WebKit clamp the transcript to a transient end without a net ResizeObserver size change. Text, resize and width notifications share one queued fit before paint, with no duplicate synchronous input fit. Avoiding repeated measurements preserves true end-following on small phones without moving or recreating the composer.
- **Model chip:** on phones (including short landscape) its model, effort and subagent choices use the existing keyboard-aware bottom sheet; desktop keeps the anchored popover. Phone sheet controls stack at the normal sheet padding: the Model label sits directly above its full-width field and offered list, with no reserved connection column. Tap outside, tap or swipe the named close handle, or Escape to close; the model sheet has no redundant top X (other shared sheets retain theirs). The sheet retains its body through exit and can reverse without remounting. The model text field still accepts arbitrary ids and lists every offered model, not filtered suggestions; its list flows inside the sheet's scroll area so the keyboard cannot clip it. Selection still takes effect on the next send. New-session, side-chat and Ask forms keep their inline fields; their phone model lists flow in their existing scroll area too, rather than being clipped above or below it. The Sessions page retains its new-session form during subsequent model-list loading, so typing does not detach the focused input. Its Where and how sheet uses the shared keyboard inset and 80% visible-height cap. Desktop form dropdowns are unchanged. All use the existing per-connection model cache warmed by composer prefetch.

## Icons throughout the app

App-owned chrome uses drawn SVG icons instead of emoji or Unicode pictograms
(including checkmarks, crosses, stars and disclosure triangles). Don't drop
icons or replace them with words alone: keep each icon in its original position
and keep its accompanying text. This includes trailing action arrows and icons
mentioned in help text. Use `crates/c6-ui/src/icons.rs`; new icons use its shared
helper and follow the same 24-unit viewBox, rounded
`currentColor` strokes and 16px size; compact wrappers constrain them to the
surrounding text. Decorative SVGs are hidden from accessibility APIs; icon-only
controls retain a title or accessible label.

Notices use a drawn bell, files a drawn page, secrets a drawn key, and published
outputs the existing package icon. Memory events keep their brain, task and
subagent states keep their status marks, and labelled actions keep their icons. Mail
attachments share a file icon; the filename and type distinguish formats.
Chat actions and document comments use drawn message, bookmark, link and
reaction icons, not emoji. The same rule applies to explorer badges and menus.

Keep intentional typography (`› ‹ … · ×`, quotes, dashes and list bullets),
keyboard symbols and directions within prose (action arrows are drawn SVGs), and
the worker/SSH relationship separator. Imported/user-authored emoji in messages, reactions, status text,
documents and terminal output remain content, including Slack's emoji picker
and the terminal vendor's Unicode-width data. The native UI tests render
representative session cards and scan app-owned Rust/HTML/JS to guard this rule.

### Archived sessions stay readable

An open session tile resolves its own metadata by id when the live session list
omits it. Archiving never closes that tile, including from its own actions menu.
Its real title, status and mark remain, with a visible **Archived** marker and a
drawn **Unarchive** button in a slim archive header below the bar (or at the
session's own strip when the bar represents another view). This leaves the
existing bar's icons in place and keeps the title and archive action reachable
on a narrow phone. Restored
workspace layouts and direct session links behave the same way. Archived
sessions still do not appear in the non-archived Sessions list or sidebar.

The message box explains **“This session is archived. Unarchive it to send
messages.”** and disables sending, preserving its draft text and attachments.
Unarchive is explicit and does not itself send anything; then the draft can be
sent normally. The head rejects new sends to an archived session with a clear
archive/unarchive error, covering stale clients too. Previously those sends ran
the agent while leaving it archived and hidden. Archiving is still not Stop:
an already-running turn or already-queued message is not cancelled by this UI
change.

`Session.archived` is a boolean returned by the session API (default false when
reading older responses). The existing by-id endpoint includes archived
sessions; the live/list endpoints continue to omit them. No storage or database
migration is needed.

## 2. Panes

Changes, Terminal, Files and Outputs are views of their own (`/sessions/{id}/changes`, `/terminal`, `/files`, `/outputs`), each with its name and the session's title. Clicking an icon opens its pane, or closes it if open; ⌘/Ctrl-click opens it in a new tile. Opened beside the session, the session stays and a list beside it goes back in the tile's history; the two share the tile evenly.

An ordinary click always keeps the pane in its session's tile. Below 40rem
of tile width, or on a phone, the retained single-pane navigation shows the
chosen pane and Back returns to the transcript. Opening it again reveals
the existing pane. Files and Outputs use the same route, including from
the phone session sheet. Only an explicit ⌘/Ctrl-click separates it.
This replaces the old 1100px automatic-new-tile fallback from `5a453ed0`,
which predates both the phone-header revision and narrow-tile navigation.

## 3. The glance

One line each, read-only: **Runs on** (worker, online dot, model and effort, runtime), **Worktree** (folder and repository) or **Folder**, **Branch**, **Changes** (files), **Work**, **Labels**, **Context** (meter, %, window, compacts at 85%); then **Newest outputs** (three). First, only while applicable, **Waiting on you** links approvals/questions to their cards. Changes and the folder open their panes, Runs on opens the worker, Work and Labels their editors, Context its breakdown and usage, and an output the Outputs pane. The session overview contains this existing glance plus pane routes and background work. The Timeline implementation/slot is retained but remains gated by `timeline::SHOWN = false`, as before this change.

## 4. Defaults

- **Thinking:** Settings → Appearance → Session transcripts → **Show thinking**, off by default. The explanation is “Show model reasoning expanded, labelled Thinking; otherwise hide it from transcripts.” Off removes reasoning blocks entirely, including their label; a thinking-only reply leaves no empty event or branching controls. Text, tool calls, results and interruption notices are unchanged. On shows both streamed and saved reasoning expanded, under the existing **Thinking** label (you can still fold an individual block). This applies to every transcript: sessions, inline subagent threads, side chats, expanded timeline turns and the phone layout. Catch-up/timeline summaries already omit reasoning. Model-call diagnostics and explicit thinking search results are not transcripts and are unchanged.
- **Times** (2026-11-09). Settings → Appearance → Session transcripts → **Times**:
  - **On your messages and what arrives on its own** (the default) puts a small time under your messages, notices, other sessions' messages and background tasks that ended.
  - **On every entry** puts one under every entry.
  - **Off** puts none.

  Unless it's off, a thin divider marks where the day changes ("Today", "Yesterday", "Monday, November 3") and pauses of 30 minutes or more ("2 h later"). Any entry without a time shows its full date and time when you point at it, in its top corner. Times are local clock times (`when::time`, `when::full`); nothing counts up in the transcript, so nothing redraws while you read. A tool call over a minute shows its duration as minutes and seconds ("4m 10s"). The choice is `TranscriptSettings.times` (`exchanges`, `all`, `off`), kept with Show thinking across realms and devices.
- **Thinking preference storage and API:** one user per head, one preference across realms and devices, saved in the existing `settings` table as `transcript_display`; no migration. It is loaded when the app opens and updates all mounted transcripts when changed in Settings. Other open devices pick it up on reload; it is not a live cross-device broadcast. `GET /api/v1/settings/transcript` (`settings.transcript.get`) returns `{"show_thinking": false}` until saved; `PUT` on that path (`settings.transcript.put`) accepts the same object and returns 204. A missing field defaults to false. Available from either a realm or the console. This is display only: recorded events, streamed data, search indexing and model input are unchanged. The generated Settings API reference includes the request schema and default.
- Icons: Lucide 1.66.0 (ISC and inherited Feather MIT), inline at 14/16/20px with an absolute 1.5px stroke; the bar's icons stay 16px. The desktop combined ring remains a 24px data visualization. Tile controls keep 44px phone/coarse targets and use the width-dependent mouse slots above. Settings and the ⋯ menu share `components::Switch`: label left, track at the row's end, 44px touch rows. Menu keyboard hints use `Kbd`.
- A pane opens beside the session in the same tile at every width; below 40rem and on phones it fills the tile, with Back to the retained transcript. ⌘/Ctrl-click explicitly opens a separate tile.
- The glance is a column 24px to the right of the transcript, 288px wide where it starts and growing with the room to 380px (`clamp(18rem, 100% − 59.5rem, 23.75rem)`), its values wrapping rather than cut short, which keeps its reading width (56rem); the two are centred together in the session's column, the glance stays in sight as the transcript scrolls, and the message box and the find bar line up with the transcript. That needs the session's column to be at least 1240px wide (896 + 24 + 288 and 16px either side); narrower, the glance is behind the context ring: a popover, a sheet from the bottom on a phone. ⌘I opens and closes it there; Esc closes it.
- The desktop ring's badge is positive background work only; its rim is the session's own state. The phone status row spells both out separately.
- The branch has no "based on" yet.
- **Transcript scrolling:** opening a session follows the latest reply; opening a search hit stays at its anchor. Any upward wheel/trackpad movement, vertical touch drag toward older content, or Up/Page Up/Home (Shift+Space too) immediately stops following, even a pixel from the bottom, and cancels a running glide. New text and resizes do not re-enable it. Scrolling down to the very bottom (less than 4px away), or clicking **Latest** (with its trailing drawn down-arrow), resumes following. Folding and showing the tile preserves that choice and position. Touching without dragging and editing a text field do not stop following.

## 5. Not yet

- Terminal below the transcript (the workspace's panes sit side by side).
- Lines added and removed in Changes (the git status the worker gives has files only).
- The Outputs pane grouped into documents, new files and kept copies (docs/rich-content.md).

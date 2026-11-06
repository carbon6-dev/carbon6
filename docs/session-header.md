# A session's tile bar, panes and glance — Design

Status: second step built (2026-11-02), for review live. The first step (2026-11-01: a second bar of tabs under the tile bar, a details drawer under it) was too busy and hard to read: its pills scrolled sideways on a phone, ⚙ and the ◆ pill said the same thing and grew with done, interrupted and failed counts, its task lists were long, and it had scrollbars. Decided with you after a design round (the options and mockups are in doc:01a19636-a09e-70ae-a84d-628864fe7dc7, "Reworking the session header and ⋯ popover"):
- **one row** above the transcript, the tile bar; nothing else but the find bar when it's open;
- **panes beside**, never in place of the transcript;
- a read-only **glance** instead of the drawer.

## 1. The tile bar

Left to right: back and forward, the session's mark, its title (double-click to rename), its own turn's state as a dot and a word (idle, running, starting, failed, needs you; what runs in its background doesn't count), **◌ N** while any task or subagent of it runs, the icons, a divider, then **⋯**, ☆ pin, ⧉ detach, minimize, focus, ✕ (on a phone the state is its dot alone).

- **The state** is a button: with something waiting on you (an approval, a question) it goes to its card in the transcript; else, while something runs in the background, it opens ◌ N's list; else the glance (nothing when the glance is a column). Its hover says which.
- **◌ N** (a spinner and the count, blue) is the only count in the bar: nothing when nothing runs, no done or failed counts, no ⚙ or ◆. Its hover says "N running in the background:" and names each ("Subagent: …", "Task: …"), a line each; its accessible name says the same. A click lists them in a popover (a sheet from the bottom on a phone): the subagents and tasks running, then the last three of each that failed or were stopped, each with its state, name or command and how long it ran; a task has **Inspect** and, while it runs, **Stop**, and a subagent goes to its thread. **Show all** lists every one. The context ring, not ◌ N, opens the glance.
- **Buttons:** every button in the bar, the session's strip and a pane's bar is a 16px Lucide icon (‹ › ⋯ ☆ ⧉ − focus ✕ too) in a 28px target (24px in a pane's 28px bar), 40px on a touch screen, where those bars grow to 44px. The session's icons are filled only while their pane is on screen (not when a phone has gone back to the transcript, or its tile is minimized: a click then brings it forward); hover shows only with a mouse, and focus only as a keyboard's ring. **Changes** (with how many files changed), **Terminal**, and the **context ring** (filling with the context used, amber from 80%, the figure in its title) while the glance isn't a column. Not over SSH: no Changes.
- **⋯:** Rename… · Find in session (⌘F) · Files · Outputs · Session details… (usage, and what applies: labels, slash commands, agents, skills, hooks) · New like this… · Save as template… · the **Catch-up** and **Fold finished turns** switches (docs/catchup.md) · Archive.
- With another view beside the session in its tile (a doc), the bar speaks for that view; the session's icons and ⋯ are in a slim strip at the top of the session. A pane of its own beside it leaves the bar the session's.
- **Gone:** the second bar and its tabs, the drawer, ⚙, the ◆ pill, the aspect marks (and the dimming of other tiles when pointing at one).

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

## 2. Panes

Changes, Terminal, Files and Outputs are views of their own (`/sessions/{id}/changes`, `/terminal`, `/files`, `/outputs`), each with its name and the session's title. Clicking an icon opens its pane, or closes it if open; ⌘/Ctrl-click opens it in a new tile. Opened beside the session, the session stays and a list beside it goes back in the tile's history; the two share the tile evenly.

## 3. The glance

One line each, read-only: **Runs on** (worker, online dot, model and effort, runtime), **Worktree** (folder and the repository it's of) or **Folder**, **Branch**, **Changes** (files), **Work**, **Labels**, **Context** (meter, %, window, compacts at 85%); then **Newest outputs** (three); last, the **Timeline**: its turns by hour, newest first, the latest hour open, a line per turn (newest first) going to it in the transcript (docs/catchup.md §3). First of all, only while there is any, **Waiting on you**: each approval or question the session waits on ("approval: cargo publish", "question: which branch?"), going to its card in the transcript. Tasks and subagents aren't in the glance: ◌ N lists them, failures included, and so does the catch-up. A line acts where you would: Changes and the folder open their panes, Runs on the worker, Work and Labels their editors in a popover, Context its breakdown (Compact now) and usage, an output the Outputs pane on it.

## 4. Defaults

- **Thinking:** Settings → Appearance → Session transcripts → **Show thinking**, off by default. The explanation is “Show model reasoning expanded, labelled Thinking; otherwise hide it from transcripts.” Off removes reasoning blocks entirely, including their label; a thinking-only reply leaves no empty event or branching controls. Text, tool calls, results and interruption notices are unchanged. On shows both streamed and saved reasoning expanded, under the existing **Thinking** label (you can still fold an individual block). This applies to every transcript: sessions, inline subagent threads, side chats, expanded timeline turns and the phone layout. Catch-up/timeline summaries already omit reasoning. Model-call diagnostics and explicit thinking search results are not transcripts and are unchanged.
- **Thinking preference storage and API:** one user per head, one preference across realms and devices, saved in the existing `settings` table as `transcript_display`; no migration. It is loaded when the app opens and updates all mounted transcripts when changed in Settings. Other open devices pick it up on reload; it is not a live cross-device broadcast. `GET /api/v1/settings/transcript` (`settings.transcript.get`) returns `{"show_thinking": false}` until saved; `PUT` on that path (`settings.transcript.put`) accepts the same object and returns 204. A missing field defaults to false. Available from either a realm or the console. This is display only: recorded events, streamed data, search indexing and model input are unchanged. The generated Settings API reference includes the request schema and default.
- Icons: Lucide (ISC), inline, 16px; hit targets 28px with a mouse (24px in a pane's bar), 40px on a touch screen.
- A pane opens beside the session when its tile is at least ~1100px wide, else in a tile of its own; on a phone beside (it fills the screen, ‹ goes back).
- The glance is a column 24px to the right of the transcript, 288px wide where it starts and growing with the room to 380px (`clamp(18rem, 100% − 59.5rem, 23.75rem)`), its values wrapping rather than cut short, which keeps its reading width (56rem); the two are centred together in the session's column, the glance stays in sight as the transcript scrolls, and the message box and the find bar line up with the transcript. That needs the session's column to be at least 1240px wide (896 + 24 + 288 and 16px either side); narrower, the glance is behind the context ring: a popover, a sheet from the bottom on a phone. ⌘I opens and closes it there; Esc closes it.
- The bar's only count is *◌ N*; the state word is the session's own turn.
- The branch has no "based on" yet.
- **Transcript scrolling:** opening a session follows the latest reply; opening a search hit stays at its anchor. Any upward wheel/trackpad movement, vertical touch drag toward older content, or Up/Page Up/Home (Shift+Space too) immediately stops following, even a pixel from the bottom, and cancels a running glide. New text and resizes do not re-enable it. Scrolling down to the very bottom (less than 4px away), or clicking **↓ Latest**, resumes following. Folding and showing the tile preserves that choice and position. Touching without dragging and editing a text field do not stop following.

## 5. Not yet

- Terminal below the transcript (the workspace's panes sit side by side).
- Lines added and removed in Changes (the git status the worker gives has files only).
- The Outputs pane grouped into documents, new files and kept copies (docs/rich-content.md).

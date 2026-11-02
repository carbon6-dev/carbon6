# A session's tile bar, panes and glance — Design

Status: second step built (2026-11-02), for review live. The first step (2026-11-01: a second bar of tabs under the tile bar, a details drawer under it) was too busy and hard to read: its pills scrolled sideways on a phone, ⚙ and the ◆ pill said the same thing and grew with done, interrupted and failed counts, its task lists were long, and it had scrollbars. Decided with you after a design round (the options and mockups are in doc:01a19636-a09e-70ae-a84d-628864fe7dc7, "Reworking the session header and ⋯ popover"):
- **one row** above the transcript, the tile bar; nothing else but the find bar when it's open;
- **panes beside**, never in place of the transcript;
- a read-only **glance** instead of the drawer.

## 1. The tile bar

Left to right: back and forward, the session's mark, its title (double-click to rename), its state as a dot and a word (idle, running, starting, failed, needs you), **◌ N running** while any background task or subagent of it runs, the icons, a divider, then **⋯**, ☆ pin, ⧉ detach, minimize, focus, ✕.

- **N running** is the only count in the bar: nothing when nothing runs, no done or failed counts, no ⚙ or ◆. It opens the glance (scrolled to *Needs a look* when the glance is a column).
- **The icons:** 24px squares, filled while their pane is open. **Changes** (with how many files changed), **Terminal**, and the **context ring** (filling with the context used, amber from 80%, the figure in its title) while the glance isn't a column. Not over SSH: no Changes.
- **⋯:** Rename… · Find in session (⌘F) · Files · Outputs · Session details… (usage, and what applies: labels, slash commands, agents, skills, hooks) · New like this… · Save as template… · Archive.
- With another view beside the session in its tile (a doc), the bar speaks for that view; the session's icons and ⋯ are in a slim strip at the top of the session. A pane of its own beside it leaves the bar the session's.
- **Gone:** the second bar and its tabs, the drawer, ⚙, the ◆ pill, the aspect marks (and the dimming of other tiles when pointing at one).

## 2. Panes

Changes, Terminal, Files and Outputs are views of their own (`/sessions/{id}/changes`, `/terminal`, `/files`, `/outputs`), each with its name and the session's title. Clicking an icon opens its pane, or closes it if open; ⌘/Ctrl-click opens it in a new tile. Opened beside the session, the session stays and a list beside it goes back in the tile's history; the two share the tile evenly.

## 3. The glance

One line each, read-only: **Runs on** (worker, online dot, model and effort, runtime), **Worktree** (folder and the repository it's of) or **Folder**, **Branch**, **Changes** (files), **Work**, **Labels**, **Context** (meter, %, window, compacts at 85%); then **Needs a look** (up to three tasks or subagents running or failed, failures first, then the newest) and **Newest outputs** (three). A line acts where you would: Changes and the folder open their panes, Runs on the worker, Work and Labels their editors in a popover, Context its breakdown (Compact now) and usage, a task its Inspect and Stop, a subagent its thread, an output the Outputs pane on it.

## 4. Defaults

- Icons: Lucide (ISC), inline.
- A pane opens beside the session when its tile is at least ~1100px wide, else in a tile of its own; on a phone beside (it fills the screen, ‹ goes back).
- The glance is a column (288px) 24px to the right of the transcript, which keeps its reading width (56rem); the two are centred together in the session's column, the glance stays in sight as the transcript scrolls, and the message box and the find bar line up with the transcript. That needs the session's column to be at least 1240px wide (896 + 24 + 288 and 16px either side); narrower, the glance is behind the context ring: a popover, a sheet from the bottom on a phone. ⌘I opens and closes it there; Esc closes it.
- The bar's only count is *N running*.
- The branch has no "based on" yet.

## 5. Not yet

- Terminal below the transcript (the workspace's panes sit side by side).
- Lines added and removed in Changes (the git status the worker gives has files only).
- The Outputs pane grouped into documents, new files and kept copies (docs/rich-content.md).

# A session's tile bar, second bar and details drawer — Design

Status: first step built (2026-11-01), for review live. Decided with you (the options and mockups are in doc:01a19636-a09e-70ae-a84d-628864fe7dc7, "Reworking the session header and ⋯ popover", tab *Chosen direction, in detail*):
- a session's controls each have **one place and one look**: tabs switch views, the drawer shows facts, the ⋯ menu does things;
- **no view icons in the tile bar**; the views are word tabs in a second bar;
- the details panel opens **below** the second bar first; a placement on the **right** (and a setting with Auto) may follow after review.

## 1. The problem

Three places showed a session's controls: the tile bar, the header block at the top of the transcript, and the ⋯ popover (that same header). The header's one row mixed tabs, a Details toggle, Find, dialogs (New like this, Save as template), Archive and read-only pills, and five things appeared twice (title and rename, status, tasks, where it runs, context). Clicking a tab in the popover switched the tile; other buttons opened dialogs or popovers.

## 2. The three layers

| Layer | Says | Holds |
|---|---|---|
| **Tile bar** | which session this is | back and forward, mark, title (double-click to rename), aspects, status, **⋯ actions menu**, pin, detach, focus, close |
| **Second bar** | how it's doing, and what you're looking at | tabs: Transcript, Changes ±n, Files, Terminal, Outputs n, Config; then ⚙ tasks, ◆ subagents, Context n%, the drawer button |
| **Details drawer** | everything else | where it runs, labels and Work, context and usage, tasks, the newest outputs |

- **⋯** holds only actions: Rename…, Find in session (⌘F), New like this…, Save as template…, Archive. When another view is open beside the session in its tile (so the tile bar shows that view), the session's own ⋯ is at the end of its second bar.
- **The second bar** stays put while the transcript scrolls. Outputs is always a tab (it used to appear only after a publish). On a narrow tile the tabs scroll sideways and Context drops its word.
- **The drawer** opens under the second bar and pushes the transcript down. ⚙ and Context open it; ◆ keeps its own list of subagents. ⌘I opens and closes it, Esc closes it.
- **Gone:** the header block at the top of the transcript, the context and usage row, the Details card, and ⚙ in the tile bar.

## 3. Not yet

- The drawer **on the right**, and Settings → Appearance → *Details panel* (Auto, below, right).
- ⌥1–6 to switch views.
- The Outputs tab grouped into documents, new files and kept copies (docs/rich-content.md).

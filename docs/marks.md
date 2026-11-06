# Marks: telling sessions apart at a glance — Design

Status: built (2026-10-30), description updated to the current UI (2026-11-05; see [session-header.md](session-header.md)). Decided with you:
- a session has more to tell apart than its own colour: **its worker, its work, its project, its realm, its repo, its model**;
- each **value** of each aspect (this worker, that piece of work) has a **mark**, the same everywhere it shows;
- twelve colours are too few: marks are **20 colours, plain or in one of 4 patterns** (horizontal stripes, vertical stripes, diagonal stripes, dots): 100 marks;
- the aspect is told by **shape**, the value by mark, so two dots in a row never need a legend.

## 1. Marks

A mark is a colour (`sky`, `navy`…) or a colour in a pattern (`navy/dots`). The colours: sky, emerald, amber, rose, violet, teal, orange, indigo, lime, pink, cyan, fuchsia (the original twelve), then red, yellow, blue, brown, navy, olive, maroon and slate. The patterns: `hstripes`, `vstripes`, `diagonal`, `dots`, drawn in the colour on a pale tint of it. New things take marks in order, all plain colours before any pattern (`c6_api::marks::all`), the one least used first.

A session's own mark is its `colour` (any mark now; old sessions keep theirs). A desktop tile bar representing the session has a circular identity button before the title and, by default, a stripe along its top (the bar keeps the panel's own colours, so its text reads whatever the mark). The session's rail row and dock line carry a short stripe; its list row has a dot. Identity is independent of agent state: desktop tile bars use a separate combined state/context ring, other rows retain their state dot, and phones spell status out above the composer rather than colouring the context ring.

## 2. Aspects and shapes

| Aspect | Shape | Key | Mark when you've picked none |
|---|---|---|---|
| the session itself | ● circle | (its `colour`) | picked when it starts: least used among the realm's recent sessions |
| worker | ■ square | `worker:<id>` | picked when first seen: least used among workers |
| work | ◆ diamond | `work:<id>` | picked when first seen: least used among open work |
| project | ⬢ hexagon | `project:<id>` | picked when first seen: least used among active and paused projects |
| realm | ▲ triangle | `realm:<id>` | home sky, commons amber, others from the name (as before) |
| repo | ⬟ pentagon | `repo:<name>` | from the name |
| model | ⬭ pill | `model:<name>` | from the name |

Marks you or the head picked live in the head (`marks` table) and reach every browser with the live feed (`LiveEvent::Marks`), so a change shows everywhere at once. Taking away a repo's, model's or realm's mark goes back to the one from its name; a worker, work item or project gets one picked again. Sessions in the live list carry the open work they serve and its project (`Session.work`; subagents and side chats their tree's). A project's quick session can carry its project without a work item, so it has a project aspect but no work aspect (docs/projects.md). Sessions run on the head have no worker aspect.

## 3. Where they show

- **A desktop tile's bar**: the session's optional top stripe and circular identity mark button, then its title and combined state/context ring. Clicking the mark button opens the session's palette. Settings → Appearance → Session bar colour defaults to **Coloured stripe**; **Dot only** removes only the stripe. Aspect marks are **off by default**. Settings → Appearance → Session aspects → **Show on tile bars** adds the chosen aspects as coloured shapes after the title, taking only the title's flexible room; the ring and seven fixed 44px controls never move or shrink. There are no named chips in the bar. Resting the pointer on a shape for 400ms dims tiles without that value; leaving cancels the pending spotlight and undims them. Clicking opens its card: related sessions, its page when applicable, and its shared palette. When the bar represents another view beside the session, it does not show the session's marks; the session's own slim strip has its controls and actions, not a second set of identity or aspect marks.
- **The phone header**: one 44px bar, without a separate identity/aspect row or stripe. Its context ring is plain, without a state rim; status is worded above the composer. **Session mark** in Actions opens the identity palette. The app menu and Sessions picker replace the old extra header and footer.
- **The rail and dock**: the session's short mark stripe, then its title and its shown aspects as small shapes, named on hover. A separate state dot says how it is doing. The rail's hover card shows named aspect chips and just the session's current swatch (including its pattern, or the mark hashed from its id for older sessions). Click the swatch to expand the palette inside the card; colour storage is unchanged. The card and palette stay within the viewport, scrolling if needed. The SVG **Session details** button in rail and Sessions-page rows opens the same card by click, keyboard or touch; on a phone, go to **Sessions** and tap that button. Explicitly opened cards stay until closed (close button or Escape), rather than depending on hover. Keyboard focus inside a hover-opened card keeps it open too.
- **The sessions list**: the session's mark dot, with its shown aspects as named shape-and-label chips below its title and location. These chips and the rail/dock aspect shapes describe the values; they are not buttons opening aspect palettes or related-session popovers.
- **Workers** (list and page), **Work** (list, item page, the Brief's "Left open", a session's work line), **Projects** (list, page, its work, and project links in Work): the value's own mark, clickable to change on the worker, work and project pages and in the workers list.
- **The realm switcher**: each realm's triangle.

Which aspects show in these rows and cards is a per-realm setting (`session_facets`, Settings → Appearance): worker, work, project and realm by default. The same setting holds `tile_bars`, false by default, to opt desktop bars into those choices. Missing fields in older stored settings or older head responses mean off. Phones (including short landscape) never mount tile marks, even when opted in. A realm aspect shows only when it isn't home and the UI is not scoped to a realm.

The off path keeps the current appearance and no per-tile mark signals, aspect scans, timers or listeners. One workspace-level gate mounts the controller only while enabled on desktop; it fills static empty bar slots without remounting tiles, their controls or composers. Turning it off or resizing to phone removes the strips, popovers and pending dwell timers and clears dimming. The existing settings table stores the choice per realm; no migration. Another device reads it on reload, not by a live preference broadcast.

Appearance responds to its pane, not the browser window: below 56rem the
Settings menu is a compact selector above the content. Theme, Motion and
Session aspects stack until the content column has room for readable cards.
Shared switches put their hints below the label and switch, using the full
row width instead of squeezing descriptions alongside the control.

### Session bar colour

**Settings → Appearance → Session bar colour** chooses how much of a session's tile bar carries its identity colour:

- **Coloured stripe** (default) keeps the session's colour along the bar's top edge.
- **Dot only** opts into a neutral bar: only the session's identity dot carries its colour, not the bar's background or edge.

This changes session identity styling only, independently of **Show on tile bars**. Aspect marks keep their own colours and patterns, and the opt-in hover/click behaviour is the same with either identity style. It does not recolour state dots, rail/dock/list marks, or the marks stored in the head. Like transcript display, bar colour is one preference for the head's user across realms and devices, not a per-realm facet choice. It is loaded when the UI opens; another device sees a saved change on reload. Existing heads with no saved preference keep coloured stripes, with no migration.

## 4. API

- `GET /api/v1/marks`: every mark given, by key (`marks.list`). A realm's caller sees its own work's.
- `PUT /api/v1/marks` `{key, mark}`: gives a value its mark, `mark: ""` takes it away (`marks.set`).
- `PUT /api/v1/sessions/{id}/colour` `{colour}`: a session's mark (`sessions.colour.set`).
- `GET/PUT /api/v1/settings/session_facets` `{shown: [...], tile_bars: false}` (`sessions.facets.get`/`put`): per realm; omitted `tile_bars` means false, including writes from older clients. `shown` remains ordered and deduplicated by the head.
- `GET/PUT /api/v1/settings/session_bar` `{colour: "tinted" | "dot_only"}` (`settings.session_bar.get`/`put`): user-wide display preference, stored in the existing settings table as `session_bar_display`. An omitted `colour` defaults to `tinted`; unsupported values are rejected without changing the saved preference.

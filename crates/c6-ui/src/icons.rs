//! Line icons from Lucide (lucide.dev, ISC licence), their path data copied inline.

use leptos::prelude::*;

/// A 16px icon in the text's colour: Lucide's 24-unit grid, stroked to
/// about 1.5px like the app's own.
fn icon(paths: impl IntoView + 'static) -> impl IntoView {
    view! {
        <svg viewBox="0 0 24 24" class="size-4 shrink-0" fill="none" stroke="currentColor" stroke-width="2.25"
            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            {paths}
        </svg>
    }
}

/// Lucide `file-diff`: a session's changes.
pub fn file_diff() -> impl IntoView {
    icon(view! {
        <path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z" />
        <path d="M9 10h6" />
        <path d="M12 13V7" />
        <path d="M9 17h6" />
    })
}

/// Lucide `square-terminal`: a session's terminal.
pub fn square_terminal() -> impl IntoView {
    icon(view! {
        <path d="m7 11 2-2-2-2" />
        <path d="M11 13h4" />
        <rect width="18" height="18" x="3" y="3" rx="2" ry="2" />
    })
}

/// Lucide `folder`: a session's files.
pub fn folder() -> impl IntoView {
    icon(view! {
        <path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" />
    })
}

/// Lucide `package`: a session's outputs.
pub fn package() -> impl IntoView {
    icon(view! {
        <path d="M11 21.73a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73z" />
        <path d="M12 22V12" />
        <path d="m3.3 7 7.703 4.734a2 2 0 0 0 1.994 0L20.7 7" />
        <path d="m7.5 4.27 9 5.15" />
    })
}

/// Lucide `chevron-left`: back.
pub fn chevron_left() -> impl IntoView {
    icon(view! { <path d="m15 18-6-6 6-6" /> })
}

/// Lucide `chevron-right`: forward.
pub fn chevron_right() -> impl IntoView {
    icon(view! { <path d="m9 18 6-6-6-6" /> })
}

/// Lucide `x`: close.
pub fn x() -> impl IntoView {
    icon(view! {
        <path d="M18 6 6 18" />
        <path d="m6 6 12 12" />
    })
}

/// Lucide `ellipsis`: a menu of actions.
pub fn ellipsis() -> impl IntoView {
    icon(view! {
        <circle cx="12" cy="12" r="1" />
        <circle cx="19" cy="12" r="1" />
        <circle cx="5" cy="12" r="1" />
    })
}

/// Lucide `star`, filled when `on` (pinned).
pub fn star(on: bool) -> impl IntoView {
    view! {
        <svg viewBox="0 0 24 24" class="size-4 shrink-0" fill=if on { "currentColor" } else { "none" } stroke="currentColor" stroke-width="2.25"
            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2" />
        </svg>
    }
}

/// Lucide `square-arrow-out-up-right`: into a tile of its own.
pub fn square_arrow_out() -> impl IntoView {
    icon(view! {
        <path d="M21 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h6" />
        <path d="m21 3-9 9" />
        <path d="M15 3h6v6" />
    })
}

/// Lucide `minus`: minimize.
pub fn minus() -> impl IntoView {
    icon(view! { <path d="M5 12h14" /> })
}

/// Lucide `maximize`: focus (fill the screen).
pub fn maximize() -> impl IntoView {
    icon(view! {
        <path d="M8 3H5a2 2 0 0 0-2 2v3" />
        <path d="M21 8V5a2 2 0 0 0-2-2h-3" />
        <path d="M3 16v3a2 2 0 0 0 2 2h3" />
        <path d="M16 21h3a2 2 0 0 0 2-2v-3" />
    })
}

/// Lucide `minimize`: out of focus, back to the tiles.
pub fn minimize() -> impl IntoView {
    icon(view! {
        <path d="M8 3v3a2 2 0 0 1-2 2H3" />
        <path d="M21 8h-3a2 2 0 0 1-2-2V3" />
        <path d="M3 16h3a2 2 0 0 1 2 2v3" />
        <path d="M16 21v-3a2 2 0 0 1 2-2h3" />
    })
}

/// Lucide `search`: a search field's magnifier.
pub fn search() -> impl IntoView {
    icon(view! {
        <circle cx="11" cy="11" r="8" />
        <path d="m21 21-4.3-4.3" />
    })
}

/// Lucide `sliders-horizontal`: filter, sort and group.
pub fn sliders() -> impl IntoView {
    icon(view! {
        <path d="M21 4h-7M10 4H3M21 12h-9M8 12H3M21 20h-5M12 20H3" />
        <path d="M14 2v4M8 10v4M16 18v4" />
    })
}

/// Lucide `square-check` when `on`, else `square`: a toggle chip's box.
pub fn check_box(on: bool) -> impl IntoView {
    view! {
        <svg viewBox="0 0 24 24" class="size-3.5 shrink-0" fill="none" stroke="currentColor" stroke-width="2.25"
            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <rect width="18" height="18" x="3" y="3" rx="2" />
            {on.then(|| view! { <path d="m9 12 2 2 4-4" /> })}
        </svg>
    }
}

/// An icon button in a bar (a tile's, a pane's, a session's strip): 28px
/// round a 16px glyph, 40px on a touch screen. Hover only with a mouse (a
/// tapped button on a phone keeps no look); a ring for the keyboard's focus.
pub const BAR_BUTTON: &str = "flex size-7 shrink-0 items-center justify-center rounded text-zinc-600 outline-none focus-visible:ring-2 focus-visible:ring-sky-500/60 disabled:opacity-30 pointer-fine:hover:bg-black/10 pointer-fine:hover:text-zinc-900 pointer-fine:disabled:hover:bg-transparent pointer-coarse:size-10 dark:text-zinc-400 dark:pointer-fine:hover:bg-white/10 dark:pointer-fine:hover:text-zinc-100";

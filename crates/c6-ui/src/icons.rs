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

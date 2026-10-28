//! Small shared view helpers.

use c6_api::SessionStatus;
use leptos::prelude::*;

pub fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

pub fn ago(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

/// A signal that ticks every few seconds, for relative times.
pub fn clock() -> RwSignal<i64> {
    let now = RwSignal::new(now_ms());
    set_interval(move || now.set(now_ms()), std::time::Duration::from_secs(5));
    now
}

#[component]
pub fn StatusPill(status: SessionStatus) -> impl IntoView {
    let (cls, label) = match status {
        SessionStatus::Running => ("bg-blue-100 text-blue-800 dark:bg-blue-950 dark:text-blue-300", "running"),
        SessionStatus::Idle => ("bg-zinc-100 text-zinc-700 dark:bg-zinc-800 dark:text-zinc-300", "idle"),
        SessionStatus::Starting => ("bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-300", "starting"),
        SessionStatus::Failed => ("bg-red-100 text-red-800 dark:bg-red-950 dark:text-red-300", "failed"),
    };
    view! { <span class=format!("inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium {cls}")>{label}</span> }
}

/// This browser's saved state, kept per place (docs/realms-ux.md §4):
/// `c6.x` is `c6.<realm>.x` (or `c6.admin.x`). Home falls back to what was
/// saved before realms had their own pages.
pub fn storage_get(key: &str) -> Option<String> {
    let placed = placed_key(key);
    storage_get_global(&placed).or_else(|| (crate::place::realm().as_deref() == Some("home")).then(|| storage_get_global(key)).flatten())
}

pub fn storage_set(key: &str, value: &str) {
    storage_set_global(&placed_key(key), value);
}

fn placed_key(key: &str) -> String {
    match (crate::place::header(), key.strip_prefix("c6.")) {
        (Some(p), Some(rest)) => format!("c6.{p}.{rest}"),
        _ => key.to_owned(),
    }
}

/// Saved state shared by every place in this browser.
pub fn storage_get_global(key: &str) -> Option<String> {
    window().local_storage().ok()??.get_item(key).ok()?
}

pub fn storage_set_global(key: &str, value: &str) {
    if let Ok(Some(s)) = window().local_storage() {
        let _ = s.set_item(key, value);
    }
}

pub const INPUT: &str = "w-full rounded-md border border-zinc-300 bg-white px-2 py-1.5 text-sm dark:border-zinc-700 dark:bg-zinc-900";
pub const BUTTON: &str = "rounded-md bg-zinc-900 px-3 py-1.5 text-sm font-medium text-white hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300";
pub const BUTTON_QUIET: &str = "rounded-md px-3 py-1.5 text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800";

/// "Open in Gmail": a conversation on its service's own web, in a new tab.
#[component]
pub fn OpenIn(link: c6_api::Link) -> impl IntoView {
    view! {
        <a class="inline-flex items-center rounded-md px-3 py-1.5 text-sm text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-800"
            href=link.url target="_blank" rel="noopener noreferrer">{format!("Open in {} ↗", link.service)}</a>
    }
}

#[component]
pub fn ErrorBox(message: String) -> impl IntoView {
    view! {
        <div class="rounded-md border border-red-300 bg-red-50 px-3 py-2 text-sm text-red-800 dark:border-red-900 dark:bg-red-950 dark:text-red-200">
            {message}
        </div>
    }
}

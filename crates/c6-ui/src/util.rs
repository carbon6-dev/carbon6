//! Small shared view helpers.

use c6_api::SessionStatus;
use leptos::prelude::*;

pub fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

/// How long ago (crate::when, where every page's dates come from).
pub use crate::when::ago;

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

/// A short notice at the bottom of the screen, with Undo for what can be
/// undone (archiving), instead of asking first.
#[derive(Clone, Copy)]
pub struct Notice {
    shown: RwSignal<Option<Shown>>,
    next: StoredValue<u32>,
}

/// A notice shown: its number, its text, and its Undo.
type Shown = (u32, String, Option<Callback<()>>);

impl Notice {
    pub fn provide() {
        provide_context(Notice { shown: RwSignal::new(None), next: StoredValue::new(0) });
    }

    pub fn get() -> Self {
        expect_context()
    }

    /// Shows `text` (with Undo, if given) for 6 seconds.
    pub fn show(self, text: impl Into<String>, undo: Option<Callback<()>>) {
        let n = self.next.get_value();
        self.next.set_value(n + 1);
        self.shown.set(Some((n, text.into(), undo)));
        set_timeout(
            move || {
                if self.shown.with_untracked(|s| s.as_ref().is_some_and(|(m, ..)| *m == n)) {
                    self.shown.set(None);
                }
            },
            std::time::Duration::from_secs(6),
        );
    }
}

#[component]
pub fn NoticeHost() -> impl IntoView {
    let notice = Notice::get();
    move || {
        notice.shown.get().map(|(_, text, undo)| {
            view! {
                <div class="notice-safe pointer-events-none fixed inset-x-0 bottom-0 z-50 flex justify-center px-4" role="status">
                    <div class="pointer-events-auto flex min-h-11 items-center gap-1 rounded-full bg-zinc-900 pl-4 pr-1 text-sm text-white shadow-lg dark:bg-zinc-100 dark:text-zinc-900">
                        <span class=if undo.is_some() { "" } else { "pr-3" }>{text}</span>
                        {undo.map(|u| view! {
                            " · "
                            <button class="min-h-11 rounded-full px-3 font-medium underline" on:click=move |_| {
                                notice.shown.set(None);
                                u.run(());
                            }>"Undo"</button>
                        })}
                    </div>
                </div>
            }
        })
    }
}

/// A "⋯" button and its menu. With `row`, it's a row's: on touch screens
/// always there, elsewhere on hover or keyboard focus (style/input.css).
/// The menu is fixed to the screen, so a scrolling list doesn't clip it,
/// and a tap anywhere else closes it.
#[component]
pub fn Menu(items: Vec<(&'static str, Callback<()>)>, #[prop(optional)] row: bool, #[prop(optional, into)] label: String) -> impl IntoView {
    let open = RwSignal::new(None::<(f64, f64)>);
    let items = StoredValue::new(items);
    let trigger = if row {
        "row-action flex h-8 w-8 shrink-0 items-center justify-center rounded-md text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-800"
    } else {
        "flex h-11 w-11 shrink-0 items-center justify-center rounded-md text-zinc-500 hover:bg-zinc-100 dark:hover:bg-zinc-800"
    };
    let label = if label.is_empty() { "More".to_owned() } else { label };
    view! {
        <button class=trigger title=label.clone() aria-label=label aria-haspopup="menu" aria-expanded=move || open.get().is_some().to_string()
            on:click=move |ev| {
                ev.prevent_default();
                ev.stop_propagation();
                if open.get_untracked().is_some() {
                    open.set(None);
                    return;
                }
                // Under the button, its right edge on the button's.
                let at = ev.current_target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok()).map(|el| {
                    let r = el.get_bounding_client_rect();
                    (r.bottom(), window().inner_width().ok().and_then(|w| w.as_f64()).unwrap_or(0.0) - r.right())
                });
                open.set(at.or(Some((0.0, 0.0))));
            }>"⋯"</button>
        {move || open.get().map(|(top, right)| view! {
            <div class="fixed inset-0 z-40" on:click=move |ev| {
                ev.prevent_default();
                ev.stop_propagation();
                open.set(None);
            }></div>
            <div role="menu" class="fixed z-50 min-w-40 rounded-md border border-zinc-200 bg-white py-1 text-sm shadow-lg dark:border-zinc-700 dark:bg-zinc-900"
                style=format!("top: {}px; right: {}px", top + 4.0, right.max(8.0))>
                {items.get_value().into_iter().map(|(name, run)| view! {
                    <button role="menuitem" class="flex min-h-11 w-full items-center px-4 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800"
                        on:click=move |ev| {
                            ev.prevent_default();
                            ev.stop_propagation();
                            open.set(None);
                            run.run(());
                        }>{name}</button>
                }).collect_view()}
            </div>
        })}
    }
}

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

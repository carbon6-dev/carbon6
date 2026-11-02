//! Small shared view helpers.

use c6_api::SessionStatus;
use leptos::prelude::*;

pub fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

/// How long ago (crate::when, where every page's dates come from).
pub use crate::when::ago;

/// Today in this browser's time zone, as YYYY-MM-DD.
pub fn today() -> String {
    let d = js_sys::Date::new_0();
    format!("{:04}-{:02}-{:02}", d.get_full_year(), d.get_month() + 1, d.get_date())
}

/// A signal that ticks every few seconds, for relative times.
pub fn clock() -> RwSignal<i64> {
    let now = RwSignal::new(now_ms());
    set_interval(move || now.set(now_ms()), std::time::Duration::from_secs(5));
    now
}

/// A session state's pill colours and word.
pub fn status_look(status: SessionStatus) -> (&'static str, &'static str) {
    match status {
        SessionStatus::Running => ("bg-blue-100 text-blue-800 dark:bg-blue-950 dark:text-blue-300", "running"),
        SessionStatus::Idle => ("bg-zinc-100 text-zinc-700 dark:bg-zinc-800 dark:text-zinc-300", "idle"),
        SessionStatus::Starting => ("bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-300", "starting"),
        SessionStatus::Failed => ("bg-red-100 text-red-800 dark:bg-red-950 dark:text-red-300", "failed"),
    }
}

#[component]
pub fn StatusPill(status: SessionStatus) -> impl IntoView {
    let (cls, label) = status_look(status);
    view! { <span class=format!("inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium {cls}")>{label}</span> }
}

/// Sizes a message box to what is in it; its CSS `max-height` caps it, and
/// past that it scrolls.
pub fn fit(el: &web_sys::HtmlTextAreaElement) {
    // Empty, it keeps its own rows: a long hint doesn't size it.
    if el.value().is_empty() {
        let _ = el.set_attribute("style", "overflow-y:hidden");
        return;
    }
    let _ = el.set_attribute("style", "height:auto;overflow-y:hidden");
    let border = el.offset_height() - el.client_height();
    let height = el.scroll_height() + border;
    let _ = el.set_attribute("style", &format!("height:{height}px;overflow-y:hidden"));
    // Capped by its max-height: only then does it scroll (and show a bar).
    if el.scroll_height() > el.client_height() + 1 {
        let _ = el.set_attribute("style", &format!("height:{height}px;overflow-y:auto"));
    }
}

/// Keeps a message box sized to its text as `track` changes it (typed, sent,
/// put back to edit).
pub fn autosize(node: NodeRef<leptos::html::Textarea>, track: impl Fn() + 'static) {
    let refit = move || {
        request_animation_frame(move || {
            if let Some(el) = node.try_get_untracked().flatten() {
                fit(&el);
            }
        })
    };
    Effect::new(move || {
        track();
        refit();
    });
    // Narrower (a phone turned, a tile resized), its text takes more lines.
    let resized = window_event_listener(leptos::ev::resize, move |_| refit());
    on_cleanup(move || resized.remove());
}

/// The class of a message box that grows with its text up to 40% of the
/// window, then scrolls.
pub const GROWS: &str = "max-h-[40dvh] resize-none overflow-y-auto";

/// A touch screen is the main way in (a phone or tablet): there, Return
/// in a message box is a new line, as on a phone's keyboard.
pub fn touch_first() -> bool {
    window().match_media("(pointer: coarse)").ok().flatten().is_some_and(|m| m.matches())
}

/// The key that sends a message box: Enter (Shift+Enter for a new line);
/// on a touch screen only ⌘/Ctrl+Enter, Return being a new line. ⌘/Ctrl+Enter
/// sends everywhere.
pub fn sends(ev: &web_sys::KeyboardEvent) -> bool {
    ev.key() == "Enter" && !ev.is_composing() && (ev.meta_key() || ev.ctrl_key() || (!ev.shift_key() && !touch_first()))
}

/// The key for Send now while the agent works (stop its current step, then
/// send): ⌘/Ctrl+Shift+Enter, on every device (on a touch screen ⌘/Ctrl+Enter
/// is the only plain Send).
pub fn sends_now(ev: &web_sys::KeyboardEvent) -> bool {
    ev.key() == "Enter" && !ev.is_composing() && (ev.meta_key() || ev.ctrl_key()) && ev.shift_key()
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

pub fn storage_remove(key: &str) {
    if let Ok(Some(s)) = window().local_storage() {
        let _ = s.remove_item(&placed_key(key));
    }
}

/// A message box's text, kept in this browser until it's sent
/// (`c6.draft.<key>`): a panel that shows something else for a moment, or a
/// popup closed by mistake, gives it back as it was.
pub fn draft(key: &str) -> RwSignal<String> {
    let key = format!("c6.draft.{key}");
    let text = RwSignal::new(storage_get(&key).unwrap_or_default());
    Effect::new(move || text.with(|t| if t.is_empty() { storage_remove(&key) } else { storage_set(&key, t) }));
    text
}

/// Drops a draft that was sent, from code that may outlive its box.
pub fn forget_draft(key: &str) {
    storage_remove(&format!("c6.draft.{key}"));
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

/// How something that pops up goes away.
type PutAway = std::rc::Rc<dyn Fn()>;
type Hook = wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>;

thread_local! {
    static POPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// What pops up now, and how it goes away: one at a time.
    static SHOWN: std::cell::RefCell<Option<(u64, PutAway)>> = const { std::cell::RefCell::new(None) };
}

/// The document's listeners while something pops up; dropped, they go.
struct Dismiss {
    id: u64,
    hooks: Vec<(&'static str, Hook)>,
}

impl Drop for Dismiss {
    fn drop(&mut self) {
        for (kind, f) in &self.hooks {
            let _ = document().remove_event_listener_with_callback_and_bool(kind, wasm_bindgen::JsCast::unchecked_ref(f.as_ref()), true);
        }
        SHOWN.with(|s| {
            let mut s = s.borrow_mut();
            if s.as_ref().is_some_and(|(i, _)| *i == self.id) {
                *s = None;
            }
        });
    }
}

/// Puts away what `shown` says is open (`close`) as phones and desktops
/// expect: a press anywhere else (`pointerdown`, a tap and a click alike;
/// a touch scroll starts with one), a wheel scrolling anywhere else, focus
/// moving elsewhere, Escape (which goes no further, so it doesn't also stop
/// a running turn), and another one opening. Scrolling the page by itself
/// (a transcript following its turn) leaves it open. Nothing covers the
/// page, so scrolling stays the page's. What belongs to it (its button and
/// what it shows) carries `data-pop`, the value returned.
pub fn dismissable(shown: impl Fn() -> bool + 'static, close: impl Fn() + Clone + 'static) -> String {
    use wasm_bindgen::JsCast;
    let id = POPS.with(|n| {
        n.set(n.get() + 1);
        n.get()
    });
    let key = format!("pop{id}");
    let mine = format!("[data-pop=\"{key}\"]");
    let hooks = StoredValue::new_local(None::<Dismiss>);
    Effect::new(move || {
        if !shown() {
            hooks.set_value(None);
            return;
        }
        if hooks.with_value(Option::is_some) {
            return;
        }
        let put_away: PutAway = std::rc::Rc::new(close.clone());
        let before = SHOWN.with(|s| s.borrow_mut().replace((id, put_away)));
        if let Some((other, put_away)) = before
            && other != id
        {
            put_away();
        }
        let outside = {
            let mine = mine.clone();
            move |ev: &web_sys::Event| {
                !ev.target()
                    .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                    .is_some_and(|el| el.closest(&mine).ok().flatten().is_some())
            }
        };
        let mut d = Dismiss { id, hooks: Vec::new() };
        for kind in ["pointerdown", "wheel", "focusin"] {
            let (outside, close) = (outside.clone(), close.clone());
            d.hooks.push((
                kind,
                wasm_bindgen::closure::Closure::new(move |ev: web_sys::Event| {
                    if outside(&ev) {
                        close();
                    }
                }),
            ));
        }
        let close = close.clone();
        d.hooks.push((
            "keydown",
            wasm_bindgen::closure::Closure::new(move |ev: web_sys::Event| {
                if ev.dyn_ref::<web_sys::KeyboardEvent>().is_some_and(|k| k.key() == "Escape") {
                    ev.stop_propagation();
                    close();
                }
            }),
        ));
        let opts = web_sys::AddEventListenerOptions::new();
        opts.set_capture(true);
        opts.set_passive(true);
        for (kind, f) in &d.hooks {
            let _ = document().add_event_listener_with_callback_and_add_event_listener_options(kind, f.as_ref().unchecked_ref(), &opts);
        }
        hooks.set_value(Some(d));
    });
    key
}

/// A "⋯" button and its menu. With `row`, it's a row's: on touch screens
/// always there, elsewhere on hover or keyboard focus (style/input.css).
/// The menu is fixed to the screen, so a scrolling list doesn't clip it,
/// and goes away as `dismissable` says.
#[component]
pub fn Menu(items: Vec<(&'static str, Callback<()>)>, #[prop(optional)] row: bool, #[prop(optional, into)] label: String) -> impl IntoView {
    let open = RwSignal::new(None::<(f64, f64)>);
    let pop = dismissable(
        move || open.get().is_some(),
        move || {
            open.try_set(None);
        },
    );
    let items = StoredValue::new(items);
    let trigger = if row {
        "row-action flex h-8 w-8 shrink-0 items-center justify-center rounded-md text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-800"
    } else {
        "flex h-11 w-11 shrink-0 items-center justify-center rounded-md text-zinc-500 hover:bg-zinc-100 dark:hover:bg-zinc-800"
    };
    let label = if label.is_empty() { "More".to_owned() } else { label };
    view! {
        <button class=trigger title=label.clone() aria-label=label aria-haspopup="menu" aria-expanded=move || open.get().is_some().to_string()
            data-pop=pop.clone() on:click=move |ev| {
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
            <div role="menu" data-pop=pop.clone() class="fixed z-50 min-w-40 rounded-md border border-zinc-200 bg-white py-1 text-sm shadow-lg dark:border-zinc-700 dark:bg-zinc-900"
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

/// One of a few answers, each shown whole as a row to tap: a drop-down
/// shows only the chosen one and cuts long ones off. More than 8 fall back
/// to a drop-down. `optional`: tapping the chosen row again clears it.
/// Rows aren't buttons, so a `<label>` around them doesn't pick the first.
#[component]
pub fn Choices(choices: Vec<(String, String)>, value: RwSignal<String>, #[prop(optional)] optional: bool) -> impl IntoView {
    if choices.len() > 8 {
        return view! {
            <select class=format!("{INPUT} py-1 text-sm") prop:value=move || value.get() on:change=move |ev| value.set(event_target_value(&ev))>
                {optional.then(|| view! { <option value="">"—"</option> })}
                {choices.into_iter().map(|(v, l)| view! { <option value=v>{l}</option> }).collect_view()}
            </select>
        }
        .into_any();
    }
    view! {
        <div role="radiogroup" class="space-y-1">
            {choices.into_iter().map(|(v, l)| {
                let v = StoredValue::new(v);
                let on = Memo::new(move |_| value.with(|x| v.with_value(|v| x == v)));
                let pick = move || value.update(|x| *x = if optional && v.with_value(|v| x == v) { String::new() } else { v.get_value() });
                view! {
                    <div role="radio" tabindex="0" aria-checked=move || on.get().to_string()
                        class=move || format!(
                            "flex w-full cursor-pointer items-start gap-2 rounded-md border px-2.5 py-1.5 text-sm outline-none focus-visible:ring-2 focus-visible:ring-sky-500 {}",
                            if on.get() { "border-sky-500 bg-sky-100 dark:bg-sky-900/40" } else { "border-zinc-300 bg-white hover:border-zinc-400 dark:border-zinc-700 dark:bg-zinc-900 dark:hover:border-zinc-500" },
                        )
                        on:click=move |ev| { ev.prevent_default(); pick() }
                        on:keydown=move |ev| if ev.key() == "Enter" || ev.key() == " " { ev.prevent_default(); pick() }>
                        <span class=move || format!("mt-1 h-3 w-3 shrink-0 rounded-full border {}", if on.get() { "border-sky-600 bg-sky-600" } else { "border-zinc-400 dark:border-zinc-500" })></span>
                        <span class="min-w-0 flex-1">{l}</span>
                    </div>
                }
            }).collect_view()}
        </div>
    }
    .into_any()
}

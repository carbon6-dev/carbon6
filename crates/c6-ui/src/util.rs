//! Small shared view helpers.

use c6_api::SessionStatus;
use leptos::prelude::*;

mod drafts;
pub use drafts::attachments as draft_attachments;

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

/// A signal that ticks every few seconds, for relative times, while its
/// view lives (each hover card has one: never cleared, they piled up).
pub fn clock() -> RwSignal<i64> {
    let now = RwSignal::new(now_ms());
    let tick = set_interval_with_handle(move || now.set(now_ms()), std::time::Duration::from_secs(5)).ok();
    on_cleanup(move || {
        if let Some(t) = tick {
            t.clear();
        }
    });
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
/// As `fit`, but an empty box with a long `hint` (a suggested message in
/// its placeholder) is sized to show all of it: measured as if typed, then
/// emptied again. Without one, as `fit`.
pub fn fit_hint(el: &web_sys::HtmlTextAreaElement, hint: Option<&str>) {
    if el.client_width() == 0 {
        return;
    }
    match hint.filter(|h| !h.is_empty()) {
        Some(h) if el.value().is_empty() => {
            el.set_value(h);
            fit(el);
            let style = el.get_attribute("style").unwrap_or_default();
            el.set_value("");
            let _ = el.set_attribute("style", &style);
        }
        _ => fit(el),
    }
}

pub fn fit(el: &web_sys::HtmlTextAreaElement) {
    // Hidden tiles have no layout: scrollHeight would turn a restored draft
    // into a zero-height box. Leave even its existing inline style alone.
    if el.client_width() == 0 {
        return;
    }
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
    autosize_hint(node, track, || None);
}

/// Like `autosize`, preserving the placeholder's size on reveal and resize.
pub fn autosize_hint(node: NodeRef<leptos::html::Textarea>, track: impl Fn() + 'static, hint: impl Fn() -> Option<String> + 'static) {
    use wasm_bindgen::JsCast;
    let hint = StoredValue::new_local(hint);
    let refit = move || {
        request_animation_frame(move || {
            if let Some(el) = node.try_get_untracked().flatten() {
                let hint = hint.try_with_value(|h| untrack(h)).flatten();
                fit_hint(&el, hint.as_deref());
            }
        })
    };
    Effect::new(move || {
        track();
        refit();
    });
    // Narrower (a phone turned, a tile resized), its text takes more lines.
    let resized = window_event_listener(leptos::ev::resize, move |_| refit());
    let visible = window_event_listener_untyped("visibilitychange", move |_| refit());
    on_cleanup(move || {
        resized.remove();
        visible.remove();
    });
    // Observe width and hidden -> visible transitions, not our own height
    // writes. Queue writes outside the observer's layout delivery loop.
    type ResizeWatch = (web_sys::ResizeObserver, wasm_bindgen::closure::Closure<dyn FnMut(js_sys::Array)>);
    let observer = StoredValue::new_local(None::<ResizeWatch>);
    Effect::new(move || {
        let Some(el) = node.get() else { return };
        observer.update_value(|o| {
            if let Some((ro, _)) = o.take() {
                ro.disconnect();
            }
        });
        let mut width = -1;
        let callback = wasm_bindgen::closure::Closure::<dyn FnMut(js_sys::Array)>::new(move |_| {
            if let Some(el) = node.try_get_untracked().flatten() {
                let next = el.client_width();
                if next != width {
                    width = next;
                    refit();
                }
            }
        });
        if let Ok(ro) = web_sys::ResizeObserver::new(callback.as_ref().unchecked_ref()) {
            ro.observe(&el);
            observer.set_value(Some((ro, callback)));
        }
    });
    on_cleanup(move || {
        observer.update_value(|o| {
            if let Some((ro, _)) = o.take() {
                ro.disconnect();
            }
        });
    });
}

/// The class of a message box that grows with its text up to 40% of the
/// window, then scrolls.
pub const GROWS: &str = "grows max-h-[40dvh] resize-none overflow-y-auto";

/// A touch screen is the main way in (a phone or tablet): there, Return
/// in a message box is a new line, as on a phone's keyboard.
pub fn touch_first() -> bool {
    window().match_media("(pointer: coarse)").ok().flatten().is_some_and(|m| m.matches())
}

/// The phone's layout: one tile and one pane at a time. The desktop one
/// needs 48rem of width and 32rem of height (`md` in the style).
pub fn phone() -> bool {
    !window().match_media("(min-width: 48rem) and (min-height: 32rem)").ok().flatten().is_some_and(|m| m.matches())
}

/// The key that sends a message box: Enter (Shift+Enter for a new line);
/// on a touch screen only ⌘/Ctrl+Enter, Return being a new line. ⌘/Ctrl+Enter
/// sends everywhere.
pub fn sends(ev: &web_sys::KeyboardEvent) -> bool {
    ev.key() == "Enter" && !ev.is_composing() && (ev.meta_key() || ev.ctrl_key() || (!ev.shift_key() && !touch_first()))
}

/// The key for Send now while the agent works (the agent gets it at once,
/// not at its next step): ⌘/Ctrl+Shift+Enter, on every device (on a touch screen ⌘/Ctrl+Enter
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
    drafts::forget(key);
}

fn placed_key(key: &str) -> String {
    placed_key_in(crate::place::header().as_deref(), key)
}

fn placed_key_in(place: Option<&str>, key: &str) -> String {
    match (place, key.strip_prefix("c6.")) {
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

fn storage_remove_global(key: &str) {
    if let Ok(Some(s)) = window().local_storage() {
        let _ = s.remove_item(key);
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

/// Something popped up: which one, the component that popped it up (one
/// that a component within that one pops up is within it), and how it goes
/// away.
struct Up {
    id: u64,
    owner: Option<Owner>,
    put_away: PutAway,
}

thread_local! {
    static POPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// What pops up now, outermost first: one at a time, or one within
    /// another (a drop-down in a menu).
    static SHOWN: std::cell::RefCell<Vec<Up>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Of what's up (outermost first) when another pops up: what stays (what
/// it pops up within, `holds`) and what goes.
fn opening<T>(shown: Vec<T>, holds: impl Fn(&T) -> bool) -> (Vec<T>, Vec<T>) {
    shown.into_iter().partition(holds)
}

/// Whether an Escape is this one's: only the innermost goes.
fn innermost(shown: &[u64], id: u64) -> bool {
    shown.last() == Some(&id)
}

/// What opens a pop-up: what carries its `data-pop` and takes focus, or a
/// `<details>`' summary.
fn opener_selector(key: &str) -> String {
    format!(":is(button, summary, input, [role=button])[data-pop=\"{key}\"], [data-pop=\"{key}\"] > summary")
}

/// Whether `owner` is a component within `of` (not `of` itself: two that
/// one component pops up are side by side).
fn within(owner: &Owner, of: &Owner) -> bool {
    let mut at = owner.parent();
    while let Some(o) = at {
        if &o == of {
            return true;
        }
        at = o.parent();
    }
    false
}

/// Whether what carries `data-pop` `key` sits inside the pop-up `outer`'s
/// element on the page.
fn inside(key: &str, outer: u64) -> bool {
    document()
        .query_selector(&format!("[data-pop=\"{key}\"]"))
        .ok()
        .flatten()
        .and_then(|el| el.parent_element())
        .and_then(|p| p.closest(&format!("[data-pop=\"pop{outer}\"]")).ok().flatten())
        .is_some()
}

/// Where Escape puts focus back: what opened it (focused as it opened, else
/// the first of its openers on screen); for a pop-up whose opener isn't
/// marked, what had focus outside it.
fn opener_of(mine: &str, key: &str) -> Option<web_sys::HtmlElement> {
    use wasm_bindgen::JsCast;
    let doc = document();
    let openers = opener_selector(key);
    let body: Option<web_sys::Element> = doc.body().map(Into::into);
    let active = doc.active_element().filter(|a| body.as_ref() != Some(a));
    if let Some(a) = active.as_ref().filter(|a| a.matches(&openers).unwrap_or(false)) {
        return a.clone().dyn_into().ok();
    }
    let all = doc.query_selector_all(&openers).ok()?;
    let on_screen = (0..all.length())
        .filter_map(|i| all.item(i)?.dyn_into::<web_sys::HtmlElement>().ok())
        .find(|e| e.offset_width() > 0 || e.offset_height() > 0);
    on_screen.or_else(|| active.filter(|a| a.closest(mine).ok().flatten().is_none()).and_then(|a| a.dyn_into().ok()))
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
        SHOWN.with(|s| s.borrow_mut().retain(|x| x.id != self.id));
    }
}

/// Puts away what `shown` says is open (`close`) as phones and desktops
/// expect: a press anywhere else (`pointerdown`, a tap and a click alike;
/// a touch scroll starts with one), a wheel scrolling anywhere else, focus
/// moving elsewhere, Escape (which goes no further, so it doesn't also stop
/// a running turn, and puts focus back on what opened it), and another one
/// opening (but not one opening within it, which goes first on Escape).
/// Scrolling the page by itself (a transcript following its turn) leaves
/// it open. Nothing covers the page, so scrolling stays the page's. What
/// belongs to it (its button and what it shows) carries `data-pop`, the
/// value returned: a press on its button is no press outside, so the button
/// alone toggles it. `close` may run once its component is gone: `try_set`.
pub fn dismissable(shown: impl Fn() -> bool + 'static, close: impl Fn() + Clone + 'static) -> String {
    pop_up(shown, close, true)
}

/// As `dismissable`, for what covers the page to catch a press outside it
/// (`Popover`): Escape, one at a time and focus going back, but presses,
/// wheels and focus left to what covers the page. What it shows carries
/// `data-pop` (and its button, if one opens it, for focus to go back to).
pub fn escapable(shown: impl Fn() -> bool + 'static, close: impl Fn() + Clone + 'static) -> String {
    pop_up(shown, close, false)
}

fn pop_up(shown: impl Fn() -> bool + 'static, close: impl Fn() + Clone + 'static, outside_closes: bool) -> String {
    use wasm_bindgen::JsCast;
    let id = POPS.with(|n| {
        n.set(n.get() + 1);
        n.get()
    });
    let key = format!("pop{id}");
    let mine = format!("[data-pop=\"{key}\"]");
    let marked = key.clone();
    let owner = Owner::current();
    let hooks = StoredValue::new_local(None::<Dismiss>);
    Effect::new(move || {
        if !shown() {
            hooks.set_value(None);
            return;
        }
        if hooks.with_value(Option::is_some) {
            return;
        }
        // What it doesn't pop up within goes (put away once the list is
        // let go of).
        let gone = SHOWN.with(|s| {
            let mut s = s.borrow_mut();
            // Within it as components go, or on the page (a drop-down in a
            // panel the same component pops up).
            let holds = |x: &Up| owner.as_ref().zip(x.owner.as_ref()).is_some_and(|(o, of)| within(o, of)) || inside(&marked, x.id);
            let (stay, gone) = opening(std::mem::take(&mut *s), holds);
            *s = stay;
            s.push(Up { id, owner: owner.clone(), put_away: std::rc::Rc::new(close.clone()) });
            gone
        });
        for x in gone {
            (x.put_away)();
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
        if outside_closes {
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
        }
        let opener = opener_of(&mine, &marked);
        let close = close.clone();
        d.hooks.push((
            "keydown",
            wasm_bindgen::closure::Closure::new(move |ev: web_sys::Event| {
                let escape = ev.dyn_ref::<web_sys::KeyboardEvent>().is_some_and(|k| k.key() == "Escape" && !k.is_composing());
                if !escape || !SHOWN.with(|s| innermost(&s.borrow().iter().map(|x| x.id).collect::<Vec<_>>(), id)) {
                    return;
                }
                ev.stop_propagation();
                close();
                if let Some(el) = &opener {
                    let _ = el.focus();
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

/// A `<details>` drop-down that goes away as `dismissable` says: give the
/// `<details>` the `data-pop` returned and the handler for its `toggle`.
pub fn details_pop(menu: NodeRef<leptos::html::Details>) -> (String, impl Fn(web_sys::Event) + Copy + 'static) {
    let open = RwSignal::new(false);
    let pop = dismissable(
        move || open.get(),
        move || {
            if let Some(d) = menu.try_get_untracked().flatten() {
                d.set_open(false);
            }
            // Its `toggle` says so too, but not if it was drawn again.
            open.try_set(false);
        },
    );
    let toggled = move |_: web_sys::Event| {
        open.try_set(menu.try_get_untracked().flatten().is_some_and(|d| d.open()));
    };
    (pop, toggled)
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
        "row-action flex h-8 w-8 shrink-0 items-center justify-center rounded-md text-muted hover:bg-zinc-200 dark:hover:bg-zinc-800"
    } else {
        "flex h-11 w-11 shrink-0 items-center justify-center rounded-md text-muted hover:bg-zinc-100 dark:hover:bg-zinc-800"
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

thread_local! {
    /// What [`kept`] resources last loaded, by key.
    static KEPT: std::cell::RefCell<std::collections::HashMap<String, std::rc::Rc<dyn std::any::Any>>> = Default::default();
}

/// A resource whose last value outlives its component: one rebuilt by a
/// reload starts with what it showed last, not empty, and the signal
/// changes only when a fetch brings something different. `key` names the
/// query; components asking the same thing share it.
pub fn kept<T, Fut>(key: impl Into<String>, fetch: impl Fn() -> Fut + 'static) -> Signal<Option<T>>
where
    T: Clone + PartialEq + Send + Sync + 'static,
    Fut: Future<Output = T> + 'static,
{
    let key = StoredValue::new(key.into());
    let value = RwSignal::new(KEPT.with(|c| c.borrow().get(&key.get_value()).and_then(|v| v.downcast_ref::<T>().cloned())));
    let fetched = LocalResource::new(fetch);
    Effect::new(move || {
        if let Some(v) = fetched.get() {
            KEPT.with(|c| c.borrow_mut().insert(key.get_value(), std::rc::Rc::new(v.clone())));
            if value.try_get_untracked().is_some_and(|cur| cur.as_ref() != Some(&v)) {
                let _ = value.try_set(Some(v));
            }
        }
    });
    value.into()
}

/// A resource's value that holds while it reloads and changes only when
/// the reload brings something different, so a list drawn from it isn't
/// redrawn by every reload.
pub fn steady<T: Clone + PartialEq + Send + Sync + 'static>(r: LocalResource<T>) -> Memo<Option<T>> {
    Memo::new(move |prev: Option<&Option<T>>| r.get().or_else(|| prev.cloned().flatten()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_drafts_use_the_text_drafts_place_and_key() {
        for key in ["new", "ask", "session-123"] {
            let key = format!("c6.draft.{key}");
            assert_eq!(placed_key_in(None, &key), key);
            assert_eq!(placed_key_in(Some("home"), &key), key.replacen("c6.", "c6.home.", 1));
            assert_ne!(placed_key_in(Some("admin"), &key), placed_key_in(Some("work"), &key));
        }
    }

    #[test]
    fn what_pops_up_within_another_keeps_it_open() {
        // A menu (1) holding a drop-down (2), and another opening within
        // the drop-down: both stay.
        assert_eq!(opening(vec![1, 2], |_| true), (vec![1, 2], vec![]));
        // One opening elsewhere puts both away.
        assert_eq!(opening(vec![1, 2], |_| false), (vec![], vec![1, 2]));
        // One opening in the menu but not in its drop-down: the drop-down goes.
        assert_eq!(opening(vec![1, 2], |x| *x == 1), (vec![1], vec![2]));
    }

    #[test]
    fn escape_puts_away_the_innermost_only() {
        assert!(innermost(&[1, 2], 2));
        assert!(!innermost(&[1, 2], 1));
        assert!(!innermost(&[], 1));
    }

    #[test]
    fn its_opener_carries_its_mark_or_is_its_summary() {
        assert_eq!(opener_selector("pop3"), ":is(button, summary, input, [role=button])[data-pop=\"pop3\"], [data-pop=\"pop3\"] > summary");
    }

    #[test]
    fn a_component_is_within_its_parents() {
        let root = Owner::new();
        let (child, grandchild) = root.with(|| {
            let child = Owner::new();
            let grandchild = child.with(Owner::new);
            (child, grandchild)
        });
        assert!(within(&grandchild, &root));
        assert!(within(&grandchild, &child));
        assert!(!within(&child, &child));
        assert!(!within(&root, &child));
        assert!(!within(&grandchild, &Owner::new()));
    }
}

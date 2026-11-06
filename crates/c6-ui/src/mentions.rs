//! Mentions of carbon6 things in rendered Markdown (`markdown.rs`): what
//! they show, how a click opens them in the app, and the card that
//! hovering one (on a touch screen, a long press) shows.

use std::cell::Cell;

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::api;
use crate::markdown::{Kind, Known};
use crate::util::{ago, now_ms, status_look};
use crate::workspace::{ColumnCtx, How, View, Workspace, use_column, use_workspace};

/// What is known of a thing as a message renders: a session's title and
/// mark from the live list, a document's title as last seen here, work's
/// and a project's from the sessions that serve them. Untracked.
pub fn known(kind: Kind, id: &str) -> Known {
    let Some(live) = use_context::<api::LiveFeed>() else { return Known::default() };
    untrack(|| match kind {
        Kind::Session => live
            .sessions
            .with_untracked(|ss| {
                ss.iter().find(|s| s.id == id).map(|s| Known { title: Some(s.title.clone()), mark: Some(crate::marks::of(s)) })
            })
            .unwrap_or_default(),
        Kind::Doc => Known { title: crate::docs::title_of(id), mark: None },
        Kind::Work => Known {
            title: live
                .sessions
                .with_untracked(|ss| ss.iter().find_map(|s| s.work.as_ref().filter(|w| w.id == id).map(|w| w.title.clone()))),
            mark: Some(live.marks.with_untracked(|m| crate::marks::mark_of(m, &format!("work:{id}")))),
        },
        Kind::Project => Known {
            title: live
                .sessions
                .with_untracked(|ss| ss.iter().find_map(|s| s.work.as_ref().filter(|w| w.project == id).map(|w| w.project_title.clone()))),
            mark: Some(live.marks.with_untracked(|m| crate::marks::mark_of(m, &format!("project:{id}")))),
        },
    })
}

fn nothing(_: Kind, _: &str) -> Known {
    Known::default()
}

/// The view an in-app link in rendered Markdown goes to: a mention (or any
/// link to a session, document, work or project, however written), or
/// another of the app's own pages. None for what the browser should open
/// (the web, files, another realm).
fn in_app(href: &str) -> Option<View> {
    let origin = window().location().origin().unwrap_or_default();
    let here = crate::markdown::Here { origin: origin.clone(), realm: crate::place::realm(), base: crate::place::base(), known: &nothing };
    if let Some((kind, id, rest)) = crate::markdown::thing_of(href, &here) {
        let query = rest.split('#').next().unwrap_or_default();
        return Some(View::from_path(&format!("{}{query}", kind.path(&id))));
    }
    let path = href.strip_prefix(origin.as_str()).filter(|_| !origin.is_empty()).unwrap_or(href);
    if !path.starts_with('/') || path.starts_with("//") {
        return None;
    }
    let path = crate::place::strip(path);
    let path = path.split('#').next().unwrap_or_default();
    // Another place's (a realm's, the console's) loads that place.
    if path.starts_with("/r/") || path.starts_with(&format!("/{}", crate::place::CONSOLE)) {
        return None;
    }
    let to = View::from_path(path);
    (to != View::home() || path == "/").then_some(to)
}

/// Opens an in-app link clicked in rendered Markdown in the app, not with
/// a page load: beside, or on ⌘/Ctrl-click in a new tile. Middle- and
/// shift-clicks are the browser's. Whether it did.
pub fn follow_link(ev: &web_sys::MouseEvent, col: Option<ColumnCtx>, ws: Workspace) -> bool {
    if ev.button() != 0 || ev.shift_key() || ev.alt_key() || ev.default_prevented() {
        return false;
    }
    let Some(a) = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()).and_then(|e| e.closest("a[href]").ok().flatten()) else {
        return false;
    };
    let Some(to) = in_app(&a.get_attribute("href").unwrap_or_default()) else { return false };
    ev.prevent_default();
    hide_peek();
    let how = How::from_mouse(ev);
    match col {
        Some(c) => c.open(to, how),
        None => ws.open(None, to, how),
    }
    true
}

/// A click handler for a block of rendered Markdown (its links open in the
/// app; see [`follow_link`]).
pub fn link_clicks() -> impl Fn(web_sys::MouseEvent) + Clone + 'static {
    let (col, ws) = (use_column(), use_workspace());
    move |ev| {
        follow_link(&ev, col, ws);
    }
}

thread_local! {
    /// A long press just showed a card: the click its release makes opens
    /// nothing.
    static PRESSED_LONG: Cell<bool> = const { Cell::new(false) };
    static CLICKS_WATCHED: Cell<bool> = const { Cell::new(false) };
    static PEEK: Cell<Option<RwSignal<Option<Peeked>>>> = const { Cell::new(None) };
}

fn hide_peek() {
    if let Some(shown) = PEEK.with(Cell::get)
        && shown.try_with_untracked(Option::is_some) == Some(true)
    {
        shown.set(None);
    }
}

/// The mention an event happened on.
fn mention_of(target: Option<web_sys::EventTarget>) -> Option<web_sys::Element> {
    target?.dyn_into::<web_sys::Element>().ok()?.closest("a.c6-mention[data-ref]").ok().flatten()
}

fn in_card(target: Option<web_sys::EventTarget>) -> bool {
    target.and_then(|t| t.dyn_into::<web_sys::Element>().ok()).is_some_and(|e| e.closest("[data-peek]").ok().flatten().is_some())
}

/// A card shown for a mention: its thing, and where on screen.
#[derive(Clone, Debug, PartialEq)]
struct Peeked {
    reference: String,
    kind: Kind,
    id: String,
    style: String,
}

const CARD_WIDTH: f64 = 288.0;

/// The card for the mention `el`, under it (over it near the bottom of the
/// screen), kept on screen.
fn peek_of(el: &web_sys::Element) -> Option<Peeked> {
    let reference = el.get_attribute("data-ref")?;
    let (name, id) = reference.split_once(':')?;
    let kind = Kind::parse(name)?;
    let r = el.get_bounding_client_rect();
    let vw = window().inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1200.0);
    let vh = window().inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(800.0);
    let x = r.left().min(vw - CARD_WIDTH - 8.0).max(8.0);
    let style = if r.bottom() + 150.0 > vh {
        format!("left:{x}px;bottom:{}px", vh - r.top() + 4.0)
    } else {
        format!("left:{x}px;top:{}px", r.bottom() + 4.0)
    };
    Some(Peeked { reference: reference.clone(), kind, id: id.to_owned(), style })
}

/// The one card for mentions, wherever they are: shown ~400ms after the
/// pointer comes onto one and until it has left both a moment; on a touch
/// screen after a ~500ms press (a tap opens), until a tap elsewhere.
#[component]
pub fn Peek() -> impl IntoView {
    let shown = RwSignal::new(None::<Peeked>);
    PEEK.with(|p| p.set(Some(shown)));
    // Each change of plan (show, keep, hide) outdates the timers before it.
    let generation = StoredValue::new(0u64);
    // The mention under the mouse (its ref), and a touch press under way.
    let over = StoredValue::new(String::new());
    let pressing = StoredValue::new(None::<(f64, f64)>);
    let bump = move || {
        generation.update_value(|g| *g += 1);
        generation.get_value()
    };
    let later = move |ms: u64, f: Box<dyn FnOnce()>| {
        let me = bump();
        set_timeout(
            move || {
                if generation.try_get_value() == Some(me) {
                    f()
                }
            },
            std::time::Duration::from_millis(ms),
        );
    };
    let hide_now = move || {
        bump();
        over.set_value(String::new());
        if shown.with_untracked(Option::is_some) {
            shown.set(None);
        }
    };
    let hide_soon = move || later(250, Box::new(move || shown.set(None)));
    let keep = move || {
        bump();
    };
    watch_clicks();
    let hooks = vec![
        window_event_listener(leptos::ev::pointerover, move |ev| {
            if ev.pointer_type() == "touch" {
                return;
            }
            let Some(el) = mention_of(ev.target()) else { return };
            let reference = el.get_attribute("data-ref").unwrap_or_default();
            if over.with_value(|o| *o == reference) {
                return;
            }
            over.set_value(reference.clone());
            if shown.with_untracked(|s| s.as_ref().is_some_and(|p| p.reference == reference)) {
                keep();
                return;
            }
            later(400, Box::new(move || shown.set(peek_of(&el))));
        }),
        window_event_listener(leptos::ev::pointerout, move |ev| {
            if ev.pointer_type() == "touch" {
                return;
            }
            let Some(el) = mention_of(ev.target()) else { return };
            let into = ev.related_target().and_then(|t| t.dyn_into::<web_sys::Node>().ok());
            if into.is_some_and(|n| el.contains(Some(&n))) {
                return;
            }
            over.set_value(String::new());
            hide_soon();
        }),
        window_event_listener(leptos::ev::pointerdown, move |ev| {
            PRESSED_LONG.with(|p| p.set(false));
            if in_card(ev.target()) {
                return;
            }
            match mention_of(ev.target()).filter(|_| ev.pointer_type() == "touch") {
                Some(el) => {
                    hide_now();
                    pressing.set_value(Some((ev.client_x() as f64, ev.client_y() as f64)));
                    later(
                        500,
                        Box::new(move || {
                            if pressing.get_value().is_some() {
                                PRESSED_LONG.with(|p| p.set(true));
                                shown.set(peek_of(&el));
                            }
                        }),
                    );
                }
                None => hide_now(),
            }
        }),
        window_event_listener(leptos::ev::pointermove, move |ev| {
            if let Some((x, y)) = pressing.get_value()
                && ev.pointer_type() == "touch"
                && (ev.client_x() as f64 - x).hypot(ev.client_y() as f64 - y) > 10.0
            {
                pressing.set_value(None);
                bump();
            }
        }),
        window_event_listener(leptos::ev::pointerup, move |ev| {
            if ev.pointer_type() == "touch" && pressing.get_value().is_some() {
                pressing.set_value(None);
                if !PRESSED_LONG.with(Cell::get) {
                    bump();
                }
            }
        }),
        window_event_listener(leptos::ev::pointercancel, move |_| {
            pressing.set_value(None);
        }),
        // A long press is ours, not the browser's menu.
        window_event_listener(leptos::ev::contextmenu, move |ev| {
            if mention_of(ev.target()).is_some() && (pressing.get_value().is_some() || PRESSED_LONG.with(Cell::get)) {
                ev.prevent_default();
            }
        }),
        window_event_listener(leptos::ev::wheel, move |ev| {
            if !in_card(ev.target()) {
                hide_now();
            }
        }),
        window_event_listener(leptos::ev::keydown, move |ev| {
            if ev.key() == "Escape" {
                hide_now();
            }
        }),
    ];
    let hooks = StoredValue::new_local(hooks);
    on_cleanup(move || {
        for h in hooks.try_update_value(std::mem::take).unwrap_or_default() {
            h.remove();
        }
    });
    move || {
        shown.get().map(|p| {
            view! {
                <div data-peek class="fixed z-50 w-72 space-y-1.5 rounded-lg border border-zinc-200 bg-white p-3 text-sm shadow-xl dark:border-zinc-700 dark:bg-zinc-900"
                    style=p.style.clone()
                    on:pointerenter=move |ev| if ev.pointer_type() != "touch" { keep() }
                    on:pointerleave=move |ev| if ev.pointer_type() != "touch" { hide_soon() }>
                    {match p.kind {
                        Kind::Session => view! { <SessionPeek id=p.id.clone() /> }.into_any(),
                        Kind::Doc => view! { <DocPeek id=p.id.clone() /> }.into_any(),
                        Kind::Work | Kind::Project => view! { <ItemPeek kind=p.kind id=p.id.clone() /> }.into_any(),
                    }}
                </div>
            }
        })
    }
}

/// The click a long press's release makes opens nothing (a capture on the
/// window, ahead of every handler).
fn watch_clicks() {
    if CLICKS_WATCHED.with(|w| w.replace(true)) {
        return;
    }
    let f = wasm_bindgen::closure::Closure::<dyn Fn(web_sys::Event)>::new(move |ev: web_sys::Event| {
        if PRESSED_LONG.with(|p| p.replace(false)) {
            ev.prevent_default();
            ev.stop_propagation();
        }
    });
    let _ = window().add_event_listener_with_callback_and_bool("click", f.as_ref().unchecked_ref(), true);
    f.forget();
}

/// The title row: its mark (or glyph), the title, and how it stands.
fn title_row(mark: Option<String>, glyph: AnyView, title: String, state: Option<(String, &'static str)>) -> impl IntoView {
    view! {
        <div class="flex items-start gap-2">
            {match mark {
                Some(m) => view! { <span class="mt-1.5 h-2 w-2 shrink-0 rounded-full" style=crate::marks::fill(&m)></span> }.into_any(),
                None => view! { <span class="shrink-0 text-muted">{glyph}</span> }.into_any(),
            }}
            <div class="min-w-0 flex-1 break-words font-medium">{title}</div>
            {state.map(|(word, cls)| view! { <span class=format!("shrink-0 rounded-full px-2 py-0.5 text-xs font-medium {cls}")>{word}</span> })}
        </div>
    }
}

/// A session: its title, its state in its colour, where it runs and when
/// it was last active.
#[component]
fn SessionPeek(id: String) -> impl IntoView {
    let live = api::live();
    let short = Kind::Session.short(&id);
    let s = Memo::new(move |_| live.sessions.with(|ss| ss.iter().find(|s| s.id == id).cloned()));
    move || match s.get() {
        Some(s) => {
            let (cls, word) = if s.awaiting_approval {
                ("bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-300", "needs you")
            } else {
                status_look(s.display_status())
            };
            let title = if s.title.trim().is_empty() { "Untitled session".to_owned() } else { s.title.clone() };
            let worker = live.session_worker(&s);
            view! {
                {title_row(Some(crate::marks::of(&s)), ().into_any(), title, Some((word.to_owned(), cls)))}
                <div class="truncate text-xs text-muted">
                    <span title=s.on_head.then_some(api::HEAD_TITLE)>{worker}</span>
                    {format!(" · active {}", ago(now_ms() - s.updated_at))}
                </div>
            }
            .into_any()
        }
        None => view! {
            {title_row(None, crate::icons::circle().into_any(), short.clone(), None)}
            <div class="text-xs text-muted">"Not among this realm's sessions"</div>
        }
        .into_any(),
    }
}

/// Who changed a document, in words.
fn by_whom(by: &str) -> String {
    match by {
        "" => "someone".into(),
        "you" => "you".into(),
        "assistant" => "Ask".into(),
        "console" => "Ask in the console".into(),
        _ => match by.split_once(':') {
            Some(("session", id)) => known(Kind::Session, id).title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "a session".into()),
            Some((_, name)) => name.to_owned(),
            None => by.to_owned(),
        },
    }
}

/// A document: its title, and when and by whom it last changed.
#[component]
fn DocPeek(id: String) -> impl IntoView {
    let title = crate::docs::title_of(&id).unwrap_or_else(|| Kind::Doc.short(&id));
    let doc = {
        let id = id.clone();
        LocalResource::new(move || api::doc_get(id.clone()))
    };
    move || {
        let (title, line) = match doc.get() {
            None => (title.clone(), "…".to_owned()),
            Some(Ok(d)) => {
                let s = d.summary;
                (s.title, format!("changed {} by {}", ago(now_ms() - s.updated_at), by_whom(&s.updated_by)))
            }
            Some(Err(e)) => (title.clone(), e),
        };
        view! {
            {title_row(None, crate::icons::file().into_any(), title, None)}
            <div class="truncate text-xs text-muted">{line}</div>
        }
    }
}

/// Work or a project: its title and state, as the head says now.
#[component]
fn ItemPeek(kind: Kind, id: String) -> impl IntoView {
    let reference = format!("{}:{id}", kind.name());
    let title = known(kind, &id).title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| kind.short(&id));
    let mark = live_mark_of(&reference);
    let now = {
        let reference = reference.clone();
        LocalResource::new(move || api::doc_refs(vec![reference.clone()]))
    };
    move || {
        let l = now.get().and_then(Result::ok).and_then(|v| v.into_iter().next());
        let (title, state, at) = match l {
            Some(l) if l.state == "gone" => (title.clone(), Some(("gone".to_owned(), STATE_QUIET)), 0),
            Some(l) => (l.title, Some((l.state.replace('_', " "), state_look(&l.state))), l.at),
            None => (title.clone(), None, 0),
        };
        view! {
            {title_row(Some(mark.clone()), ().into_any(), title, state)}
            <div class="text-xs text-muted">{format!("{}{}", kind.word(), if at > 0 { format!(" · moved {}", ago(now_ms() - at)) } else { String::new() })}</div>
        }
    }
}

fn live_mark_of(reference: &str) -> String {
    api::live().marks.with_untracked(|m| crate::marks::mark_of(m, reference))
}

const STATE_QUIET: &str = "bg-zinc-100 text-zinc-700 dark:bg-zinc-800 dark:text-zinc-300";

/// A work item's or project's state's pill colours.
fn state_look(state: &str) -> &'static str {
    match state {
        "needs_you" => "bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-300",
        "running" | "active" => "bg-blue-100 text-blue-800 dark:bg-blue-950 dark:text-blue-300",
        "done" => "bg-emerald-100 text-emerald-800 dark:bg-emerald-950 dark:text-emerald-300",
        "dropped" | "failed" => "bg-red-100 text-red-800 dark:bg-red-950 dark:text-red-300",
        _ => STATE_QUIET,
    }
}

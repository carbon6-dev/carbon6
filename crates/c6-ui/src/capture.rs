//! Noting thoughts while a session works (docs/capture.md): a session's
//! Up next (your follow-ups, held until you send them), quotes linking a
//! to-do or follow-up to the transcript entry that prompted it, and
//! starting several task cards or to-dos together (docs/batch-starts.md).

use c6_api::Decision;
use c6_api::work::{Followup, Quote, Todo};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT};
use crate::workspace::{Go, How, Target, View, use_column, use_workspace};

/// How many start together at most (the head's limit).
const MAX_BATCH: usize = 10;

thread_local! {
    /// Follow-ups were noted on this page (the message box, the transcript):
    /// Up next reads them again at once.
    static NOTED: ArcTrigger = ArcTrigger::new();
}

/// Says a follow-up was noted here.
pub fn noted() {
    NOTED.with(|t| t.notify());
}

/// The session, at the entry a quote came from, its words found there.
pub fn quote_view(q: &Quote) -> View {
    let words: String = q.text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(80).collect();
    View::Session { id: q.session.clone(), target: Some(Target { q: words, anchor: None, open: Vec::new(), at: Some(q.seq) }) }
}

fn clip(t: &str, n: usize) -> String {
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > n { format!("{}…", t.chars().take(n).collect::<String>()) } else { t }
}

/// What prompted a to-do or follow-up, as a link to it in the transcript.
#[component]
pub fn QuoteLine(quote: Quote) -> impl IntoView {
    let text = clip(&quote.text, 140);
    view! {
        <Go to=quote_view(&quote) class="block min-w-0 truncate text-xs italic text-muted hover:underline"
            title="Open the session at what prompted it">{format!("“{text}” ")}{crate::icons::icon(crate::icons::Icon::ArrowUpRight, crate::icons::Size::S14)}</Go>
    }
}

/// A session's follow-ups, above its message box (docs/capture.md §4):
/// collapsed while it works, open and marked when its turn ends. Nothing
/// is sent until you send it.
#[component]
pub fn UpNext(session: String, running: Signal<bool>) -> impl IntoView {
    let sid = StoredValue::new(session);
    let live = use_context::<api::LiveFeed>();
    // The live list's count says when to read them again.
    let count = Memo::new(move |_| {
        live.map(|l| l.sessions.with(|ss| ss.iter().find(|s| s.id == sid.get_value()).map_or(0, |s| s.followups))).unwrap_or(0)
    });
    let items = RwSignal::new(Vec::<Followup>::new());
    let error = RwSignal::new(None::<String>);
    let reload = move || {
        spawn_local(async move {
            match api::followups(sid.get_value()).await {
                Ok(f) => {
                    items.try_set(f);
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
        })
    };
    Effect::new(move || {
        count.track();
        NOTED.with(|t| t.track());
        reload();
    });
    let open = RwSignal::new(false);
    // Open when it's done: when a turn ends, or shown while idle.
    Effect::new(move |was: Option<bool>| {
        let now = running.get();
        if was.is_none_or(|w| w) && !now {
            open.set(true);
        }
        now
    });
    let editing = RwSignal::new(None::<(String, String)>);
    let act = move |f: std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>>>>| {
        spawn_local(async move {
            match f.await {
                Ok(()) => {
                    error.try_set(None);
                    reload();
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
        })
    };
    let send = move |ids: Vec<String>| act(Box::pin(async move { api::followups_send(sid.get_value(), ids).await }));
    move || {
        let list = items.get();
        if list.is_empty() {
            return error.get().map(|e| view! { <ErrorBox message=e /> }.into_any()).unwrap_or_else(|| ().into_any());
        }
        let n = list.len();
        let waiting = !running.get();
        let frame = if waiting {
            "rounded-lg border border-amber-300 bg-amber-50 px-2 py-1.5 text-sm dark:border-amber-800 dark:bg-amber-950/40"
        } else {
            "rounded-lg border border-zinc-200 bg-white px-2 py-1 text-sm dark:border-zinc-800 dark:bg-zinc-900"
        };
        view! {
            <div class=frame data-up-next="">
                <div class="flex items-center gap-2">
                    <button class="text-xs font-medium" aria-expanded=move || open.get().to_string()
                        title="What you meant to ask or tell it once it's done (/next)"
                        on:click=move |_| open.update(|o| *o = !*o)>
                        {format!("Up next · {n}")}
                    </button>
                    {waiting.then(|| view! { <span class="text-xs text-muted">"it's done: send what you noted"</span> })}
                    <span class="flex-1"></span>
                    {(n > 1).then(|| view! {
                        <button class=format!("{BUTTON_QUIET} py-0.5 text-xs") title="Send them all as one message, numbered"
                            on:click=move |_| send(Vec::new())>"Send all"</button>
                    })}
                </div>
                {error.get().map(|e| view! { <ErrorBox message=e /> })}
                <Show when=move || open.get()>
                    <ul class="mt-1 space-y-1">
                        {items.get().into_iter().map(|f| {
                            let (fid, fid2, fid3, fid4) = (f.id.clone(), f.id.clone(), f.id.clone(), f.id.clone());
                            let text = f.text.clone();
                            let this = f.id.clone();
                            view! {
                                <li class="rounded px-1 py-0.5 hover:bg-white/60 dark:hover:bg-zinc-900/60">
                                    {move || match editing.get().filter(|(id, _)| *id == this) {
                                        Some((id, draft)) => view! {
                                            <input class=format!("{INPUT} w-full text-sm") prop:value=draft
                                                on:input=move |ev| editing.set(Some((id.clone(), event_target_value(&ev))))
                                                on:keydown=move |ev| match ev.key().as_str() {
                                                    "Enter" => {
                                                        if let Some((id, t)) = editing.get_untracked() {
                                                            editing.set(None);
                                                            act(Box::pin(async move { api::followup_edit(sid.get_value(), id, t).await.map(|_| ()) }));
                                                        }
                                                    }
                                                    "Escape" => editing.set(None),
                                                    _ => {}
                                                } />
                                        }.into_any(),
                                        None => view! { <p class="whitespace-pre-wrap break-words">{text.clone()}</p> }.into_any(),
                                    }}
                                    {f.quote.clone().map(|q| view! { <QuoteLine quote=q /> })}
                                    <div class="flex flex-wrap gap-1 pt-0.5">
                                        <button class=format!("{BUTTON} py-0.5 text-xs") title="Send it now, as your message"
                                            on:click=move |_| send(vec![fid.clone()])>"Send"</button>
                                        <button class=format!("{BUTTON_QUIET} py-0.5 text-xs")
                                            on:click={
                                                let t = f.text.clone();
                                                move |_| editing.set(Some((fid2.clone(), t.clone())))
                                            }>"Edit"</button>
                                        <button class=format!("{BUTTON_QUIET} py-0.5 text-xs") title="Keep it as a to-do on this session's work instead"
                                            on:click=move |_| { let id = fid3.clone(); act(Box::pin(async move { api::followup_to_todo(sid.get_value(), id).await.map(|_| ()) })) }>"To-do"</button>
                                        <button class="text-xs text-muted hover:text-red-600" title="Drop it" aria-label="Drop it"
                                            on:click=move |_| { let id = fid4.clone(); act(Box::pin(async move { api::followup_remove(sid.get_value(), id).await })) }>
                                            {crate::icons::x()}
                                        </button>
                                    </div>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                </Show>
            </div>
        }
        .into_any()
    }
}

/// Where a task card would run: what must match to start cards together.
fn card_place(d: &Decision) -> (String, String, String, String) {
    let s = |k: &str| d.context[k].as_str().unwrap_or("").to_owned();
    (d.realm.clone(), s("worker"), s("cwd"), s("ssh"))
}

/// The folder a card runs in, briefly.
fn card_where(d: &Decision) -> String {
    let cwd = d.context["cwd"].as_str().unwrap_or("");
    cwd.trim_end_matches('/').rsplit('/').next().unwrap_or(cwd).to_owned()
}

/// A dialog's frame over the workspace.
fn dialog(label: &'static str, close: Callback<()>, body: impl Fn() -> AnyView + Send + Sync + 'static) -> impl IntoView {
    let body = std::sync::Arc::new(body);
    view! {
        <leptos::portal::Portal>
            <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4" on:click=move |_| close.run(())
                on:keydown=move |ev| if ev.key() == "Escape" { ev.stop_propagation(); close.run(()); }>
                <div role="dialog" aria-modal="true" aria-label=label
                    class="flex max-h-[80dvh] w-full max-w-lg flex-col gap-3 overflow-hidden rounded-lg bg-white p-4 text-left shadow-xl dark:bg-zinc-900"
                    on:click=|ev| ev.stop_propagation()>
                    <div class="flex shrink-0 items-center gap-2">
                        <h2 class="flex-1 font-medium">{label}</h2>
                        <button class=format!("{BUTTON_QUIET} pointer-coarse:min-h-11") aria-label="Close" on:click=move |_| close.run(())>{crate::icons::x()}</button>
                    </div>
                    // Drawn again as you pick: what's allowed follows the picks.
                    {
                        let body = body.clone();
                        move || body()
                    }
                </div>
            </div>
        </leptos::portal::Portal>
    }
}

/// "Start together…" over a group of task cards (a session's Suggested
/// tasks, or Decisions'): pick some, then one session does them, or the
/// session that proposed them all does (docs/batch-starts.md §2).
#[component]
pub fn CardsTogether(#[prop(into)] cards: Signal<Vec<Decision>>, refresh: Callback<()>) -> impl IntoView {
    let shown = RwSignal::new(false);
    let picked = RwSignal::new(Vec::<i64>::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let (ws, col) = (use_workspace(), StoredValue::new(use_column()));
    let close = Callback::new(move |()| {
        shown.set(false);
        picked.set(Vec::new());
        error.set(None);
    });
    let chosen = move || cards.with(|cs| cs.iter().filter(|d| picked.with(|p| p.contains(&d.id))).cloned().collect::<Vec<_>>());
    let go = move |how: &'static str| {
        let ids = picked.get_untracked();
        busy.set(true);
        spawn_local(async move {
            match api::task_batch(ids, how).await {
                Ok(session) => {
                    close.run(());
                    refresh.run(());
                    if let Some(id) = session {
                        match col.get_value() {
                            Some(c) => c.open(View::session(id), How::Beside),
                            None => ws.open(None, View::session(id), How::OwnTile),
                        }
                    }
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    view! {
        <Show when=move || cards.with(|c| c.len() >= 2)>
            <button class="text-xs text-muted hover:underline" title="Pick several to do in one session"
                on:click=move |_| shown.set(true)>"Start together…"</button>
        </Show>
        <Show when=move || shown.get()>
            {dialog("Start together", close, move || {
                let all = cards.get();
                let place = chosen().first().map(card_place);
                let one_session = {
                    let c = chosen();
                    c.len() >= 2 && c.iter().all(|d| d.context["session"] == c[0].context["session"])
                };
                let n = picked.with(Vec::len);
                view! {
                    <p class="shrink-0 text-sm text-muted">"Pick the tasks one session should do, in this order. Only tasks in the same folder go together."</p>
                    <ul class="min-h-0 space-y-1 overflow-y-auto">
                        {all.into_iter().map(|d| {
                            let id = d.id;
                            let fits = place.as_ref().is_none_or(|p| *p == card_place(&d));
                            let why = (!fits).then(|| format!("In {}, not with the others", card_where(&d)));
                            view! {
                                <li>
                                    <label class=if fits { "flex items-center gap-2 rounded px-2 py-1 text-sm hover:bg-zinc-50 dark:hover:bg-zinc-800" } else { "flex items-center gap-2 rounded px-2 py-1 text-sm opacity-50" } title=why>
                                        <input type="checkbox" class=crate::components::CHECKBOX disabled=!fits
                                            prop:checked=move || picked.with(|p| p.contains(&id))
                                            on:change=move |_| picked.update(|p| if let Some(i) = p.iter().position(|x| *x == id) { p.remove(i); } else if p.len() < MAX_BATCH { p.push(id); }) />
                                        <span class="min-w-0 flex-1 truncate">{d.title.clone()}</span>
                                        <span class="shrink-0 text-xs text-muted">{card_where(&d)}</span>
                                    </label>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                    {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
                    <div class="flex shrink-0 flex-wrap gap-2">
                        <button class=format!("{BUTTON} pointer-coarse:min-h-11") disabled=move || busy.get() || n < 2
                            on:click=move |_| go("start")>{if n >= 2 { format!("Start {n} in one session") } else { "Start in one session".into() }}</button>
                        {one_session.then(|| view! {
                            <button class=format!("{BUTTON_QUIET} pointer-coarse:min-h-11") disabled=move || busy.get()
                                title="The session that proposed them does them, after what it's doing now"
                                on:click=move |_| go("here")>"Do them here"</button>
                        })}
                        <button class=format!("{BUTTON_QUIET} pointer-coarse:min-h-11") on:click=move |_| close.run(())>"Cancel"</button>
                    </div>
                }
                .into_any()
            })}
        </Show>
    }
}

/// One of the things a work item's picker offers.
#[derive(Clone, Debug, PartialEq)]
enum Pick {
    Todo(String),
    Card(i64),
}

/// "Start together…" on a work item's To do (docs/batch-starts.md §2):
/// its open to-dos and the open task cards of its sessions. Picked cards
/// become its to-dos first; `start` then opens the new-session form for
/// all of them (their ids, in order).
#[component]
pub fn WorkTogether(work: String, #[prop(into)] todos: Signal<Vec<Todo>>, start: Callback<Vec<String>>) -> impl IntoView {
    let work = StoredValue::new(work);
    let cards = LocalResource::new(move || api::work_cards(work.get_value()));
    let open_todos = Memo::new(move |_| todos.with(|ts| ts.iter().filter(|t| t.done_at.is_none()).cloned().collect::<Vec<_>>()));
    let card_list = Memo::new(move |_| cards.get().and_then(Result::ok).unwrap_or_default());
    let enough = Memo::new(move |_| open_todos.with(Vec::len) + card_list.with(Vec::len) >= 2);
    let shown = RwSignal::new(false);
    let picked = RwSignal::new(Vec::<Pick>::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let close = Callback::new(move |()| {
        shown.set(false);
        picked.set(Vec::new());
        error.set(None);
    });
    let toggle = move |p: Pick| {
        picked.update(|ps| {
            if let Some(i) = ps.iter().position(|x| *x == p) {
                ps.remove(i);
            } else if ps.len() < MAX_BATCH {
                ps.push(p);
            }
        })
    };
    let go = move || {
        // To-dos first, then cards, each in the order shown.
        let ps = picked.get_untracked();
        let order_t: Vec<String> =
            open_todos.get_untracked().into_iter().map(|t| t.id).filter(|id| ps.contains(&Pick::Todo(id.clone()))).collect();
        let order_c: Vec<i64> = card_list.get_untracked().into_iter().map(|d| d.id).filter(|id| ps.contains(&Pick::Card(*id))).collect();
        busy.set(true);
        spawn_local(async move {
            let mut ids = order_t;
            if !order_c.is_empty() {
                match api::work_todos_from_cards(work.get_value(), order_c).await {
                    Ok(kept) => ids.extend(kept),
                    Err(e) => {
                        error.set(Some(e));
                        busy.set(false);
                        return;
                    }
                }
            }
            busy.set(false);
            close.run(());
            start.run(ids);
        });
    };
    view! {
        <Show when=move || enough.get()>
            <button class="text-xs text-muted hover:underline" title="Pick several to do in one session"
                on:click=move |_| shown.set(true)>"Start together…"</button>
        </Show>
        <Show when=move || shown.get()>
            {dialog("Start together", close, move || {
                let n = picked.with(Vec::len);
                let row = move |p: Pick, title: String, mark: Option<&'static str>, quote: Option<Quote>| {
                    let (p1, p2) = (p.clone(), p);
                    view! {
                        <li>
                            <label class="flex items-center gap-2 rounded px-2 py-1 text-sm hover:bg-zinc-50 dark:hover:bg-zinc-800">
                                <input type="checkbox" class=crate::components::CHECKBOX
                                    prop:checked=move || picked.with(|ps| ps.contains(&p1))
                                    on:change=move |_| toggle(p2.clone()) />
                                <span class="min-w-0 flex-1">
                                    <span class="block truncate">{title}</span>
                                    {quote.map(|q| view! { <span class="block truncate text-xs italic text-muted">{format!("“{}”", clip(&q.text, 100))}</span> })}
                                </span>
                                {mark.map(|m| view! { <span class="shrink-0 text-[11px] text-muted">{m}</span> })}
                            </label>
                        </li>
                    }
                };
                view! {
                    <p class="shrink-0 text-sm text-muted">"Pick what one session should do: to-dos first, then task cards, in this order. Task cards you pick become to-dos of this work."</p>
                    <ul class="min-h-0 space-y-1 overflow-y-auto">
                        {open_todos.get().into_iter().map(|t| row(Pick::Todo(t.id.clone()), t.text.clone(), None, t.quote.clone())).collect_view()}
                        {card_list.get().into_iter().map(|d| row(Pick::Card(d.id), d.title.clone(), Some("task card"), None)).collect_view()}
                    </ul>
                    {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
                    <div class="flex shrink-0 flex-wrap gap-2">
                        <button class=format!("{BUTTON} pointer-coarse:min-h-11") disabled=move || busy.get() || n < 2
                            on:click=move |_| go()>{if n >= 2 { format!("Start {n} in one session") } else { "Start in one session".into() }}</button>
                        <button class=format!("{BUTTON_QUIET} pointer-coarse:min-h-11") on:click=move |_| close.run(())>"Cancel"</button>
                    </div>
                }
                .into_any()
            })}
        </Show>
    }
}

/// What you picked in the transcript: the entry, the words, and where to
/// show the popover (the selection's top, in the viewport).
#[derive(Clone, Debug, PartialEq)]
struct Picked {
    seq: u64,
    text: String,
    x: f64,
    y: f64,
}

/// The entry (`seq`) a node sits in, from its row's id (`<session>-e<seq>`).
fn entry_of(node: Option<web_sys::Node>, session: &str) -> Option<u64> {
    use wasm_bindgen::JsCast;
    let mut el = node?;
    let prefix = format!("{session}-e");
    loop {
        if let Some(e) = el.dyn_ref::<web_sys::Element>()
            && let Some(seq) = e.id().strip_prefix(&prefix).and_then(|s| s.parse().ok())
        {
            return Some(seq);
        }
        el = el.parent_node()?;
    }
}

/// A selection in this session's transcript, if any.
fn picked(session: &str) -> Option<Picked> {
    let sel = window().get_selection().ok()??;
    if sel.is_collapsed() || sel.range_count() == 0 {
        return None;
    }
    let text = sel.to_string().as_string()?.trim().to_owned();
    if text.is_empty() {
        return None;
    }
    let seq = entry_of(sel.anchor_node(), session)?;
    // Within one entry only: the link goes to one place.
    if entry_of(sel.focus_node(), session) != Some(seq) {
        return None;
    }
    let rect = sel.get_range_at(0).ok()?.get_bounding_client_rect();
    Some(Picked { seq, text, x: rect.left() + rect.width() / 2.0, y: rect.top() })
}

/// Select words in a transcript entry: a small popover offers Quote (into
/// the message box), To-do and Ask next, each keeping a link to the entry
/// (docs/capture.md §5).
#[component]
pub fn SelectionCapture(session: String, on_quote: Callback<String>) -> impl IntoView {
    let sid = StoredValue::new(session);
    let at = RwSignal::new(None::<Picked>);
    // "todo" or "next", while its line is open.
    let noting = RwSignal::new(None::<&'static str>);
    let line = RwSignal::new(String::new());
    let note = RwSignal::new(None::<Result<String, String>>);
    let check = move || {
        if noting.get_untracked().is_some() {
            return;
        }
        at.set(picked(&sid.get_value()));
        note.set(None);
    };
    let up = window_event_listener(leptos::ev::mouseup, move |_| check());
    let keys = window_event_listener(leptos::ev::keyup, move |ev| {
        if ev.key() == "Escape" {
            at.set(None);
            noting.set(None);
        } else if ev.shift_key() {
            check();
        }
    });
    on_cleanup(move || {
        up.remove();
        keys.remove();
    });
    let save = move || {
        let (Some(p), Some(kind)) = (at.get_untracked(), noting.get_untracked()) else { return };
        let text = line.get_untracked().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let quote = Some(Quote { session: sid.get_value(), seq: p.seq, text: p.text });
        spawn_local(async move {
            let done = if kind == "todo" {
                api::session_todo(sid.get_value(), text, quote).await.map(|w| format!("To-do added to “{}”.", w.title))
            } else {
                api::followup_add(sid.get_value(), text, quote).await.map(|_| {
                    noted();
                    "Held in Up next.".to_owned()
                })
            };
            if done.is_ok() {
                line.set(String::new());
                noting.set(None);
            }
            note.set(Some(done));
        });
    };
    // Keep the selection while pressing a button.
    let keep = |ev: web_sys::MouseEvent| ev.prevent_default();
    move || {
        let p = at.get()?;
        let style = format!("left: {}px; top: {}px", p.x.max(8.0), (p.y - 8.0).max(8.0));
        Some(view! {
            <div class="fixed z-40 -translate-x-1/2 -translate-y-full rounded-lg border border-zinc-200 bg-white p-1 text-xs shadow-lg dark:border-zinc-700 dark:bg-zinc-900"
                style=style data-selection-capture="">
                {move || match noting.get() {
                    None => view! {
                        <div class="flex gap-1">
                            <button class=format!("{BUTTON_QUIET} py-0.5 text-xs") on:mousedown=keep title="Quote it in your message"
                                on:click=move |_| { if let Some(p) = at.get_untracked() { on_quote.run(p.text); } at.set(None); }>"Quote"</button>
                            <button class=format!("{BUTTON_QUIET} py-0.5 text-xs") on:mousedown=keep title="A to-do on this session's work, linked to this"
                                on:click=move |_| noting.set(Some("todo"))>"To-do"</button>
                            <button class=format!("{BUTTON_QUIET} py-0.5 text-xs") on:mousedown=keep title="A follow-up to send once it's done, linked to this"
                                on:click=move |_| noting.set(Some("next"))>"Ask next"</button>
                        </div>
                    }.into_any(),
                    Some(kind) => view! {
                        <input class=format!("{INPUT} w-72 text-sm") autofocus=true
                            placeholder=if kind == "todo" { "What to do (Enter to add)" } else { "What to ask or tell it (Enter to hold)" }
                            prop:value=move || line.get() on:input=move |ev| line.set(event_target_value(&ev))
                            on:keydown=move |ev| match ev.key().as_str() {
                                "Enter" => { ev.prevent_default(); save(); }
                                "Escape" => { noting.set(None); at.set(None); }
                                _ => {}
                            } />
                    }.into_any(),
                }}
                {move || note.get().map(|r| match r {
                    Ok(t) => view! { <p class="px-1 pt-1 text-muted" role="status">{t}</p> }.into_any(),
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                })}
            </div>
        })
    }
}

/// The phone's way in (no reliable selection menu): the whole entry, from
/// its Message actions.
#[component]
pub fn EntryCapture(
    session: String,
    seq: Signal<Option<u64>>,
    text: Signal<String>,
    on_quote: Callback<String>,
    done: Callback<()>,
) -> impl IntoView {
    let sid = StoredValue::new(session);
    let noting = RwSignal::new(None::<&'static str>);
    let line = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let save = move || {
        let (Some(seq), Some(kind)) = (seq.get_untracked(), noting.get_untracked()) else { return };
        let what = line.get_untracked().trim().to_owned();
        if what.is_empty() {
            return;
        }
        let quote = Some(Quote { session: sid.get_value(), seq, text: text.get_untracked() });
        spawn_local(async move {
            let r = if kind == "todo" {
                api::session_todo(sid.get_value(), what, quote).await.map(|_| ())
            } else {
                api::followup_add(sid.get_value(), what, quote).await.map(|_| noted())
            };
            match r {
                Ok(()) => {
                    line.set(String::new());
                    noting.set(None);
                    done.run(());
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let row = "flex min-h-12 w-full items-center gap-3 rounded-md px-3 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800";
    view! {
        {move || match noting.get() {
            None => view! {
                <button type="button" class=row on:click=move |_| { on_quote.run(text.get_untracked()); done.run(()); }>"Quote in your message"</button>
                <button type="button" class=row on:click=move |_| noting.set(Some("todo"))>"To-do…"</button>
                <button type="button" class=row on:click=move |_| noting.set(Some("next"))>"Ask next…"</button>
            }.into_any(),
            Some(kind) => view! {
                <form class="flex gap-2 px-3 py-2" on:submit=move |ev| { ev.prevent_default(); save(); }>
                    <input class=format!("{INPUT} min-w-0 flex-1") autofocus=true
                        placeholder=if kind == "todo" { "What to do" } else { "What to ask or tell it" }
                        prop:value=move || line.get() on:input=move |ev| line.set(event_target_value(&ev)) />
                    <button type="submit" class=BUTTON>{if kind == "todo" { "Add" } else { "Hold" }}</button>
                </form>
            }.into_any(),
        }}
        {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quote opens its session at its entry, finding its first words, and
    /// that survives the address bar.
    #[test]
    fn a_quote_links_to_its_entry() {
        let q = Quote { session: "s1".into(), seq: 42, text: "\n  Retries are disabled here\nbecause…".into() };
        let v = quote_view(&q);
        let View::Session { id, target: Some(t) } = &v else { panic!("{v:?}") };
        assert_eq!((id.as_str(), t.at, t.q.as_str()), ("s1", Some(42), "Retries are disabled here"));
        assert_eq!(View::from_path(&v.path()), v, "{}", v.path());
    }
}

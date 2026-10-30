//! Work (docs/work.md): what you've left open. A panel of its own lists
//! your work by state, what needs you first; a piece of work opens beside
//! the list with where it stands, what belongs to it, and the way back in
//! (Resume, Pick up). Each session's header shows the work it serves.

use c6_api::Session;
use c6_api::work::{LiveAttached, WorkItem, WorkPatch, WorkProposal, state_label};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, ago, now_ms};
use crate::workspace::{Go, How, View, use_column, use_workspace};

/// How long ago a time was.
fn since(at: i64) -> String {
    ago(now_ms() - at)
}

/// What needs a look: work that needs you or went quiet.
fn waiting(items: &[WorkItem]) -> usize {
    items.iter().filter(|w| matches!(w.state.as_str(), "needs_you" | "quiet")).count()
}

/// Loads open work again when sessions change (at most every 10 s, not
/// while you type) and every half minute.
fn open_work() -> (LocalResource<Result<Vec<WorkItem>, String>>, RwSignal<u32>) {
    let tick = RwSignal::new(0u32);
    crate::decisions::reload_quietly(tick);
    let handle = set_interval_with_handle(move || tick.update(|t| *t += 1), std::time::Duration::from_secs(30)).ok();
    on_cleanup(move || {
        if let Some(h) = handle {
            h.clear();
        }
    });
    let list = LocalResource::new(move || {
        tick.track();
        api::realm().track();
        api::work_list(false)
    });
    (list, tick)
}

/// The header link, with how much needs a look.
#[component]
pub fn WorkLink(class: &'static str) -> impl IntoView {
    let (list, _) = open_work();
    view! {
        <Go to=View::Work { id: String::new() } class=class title="What you've left open: what needs you, what went quiet">
            "Work"
            {move || list.get().and_then(Result::ok).map(|l| waiting(&l)).filter(|n| *n > 0).map(|n| view! {
                <span class="ml-1 rounded-full bg-violet-600 px-1.5 text-[10px] font-semibold text-white">{n}</span>
            })}
        </Go>
    }
}

fn state_class(state: &str) -> &'static str {
    match state {
        "needs_you" => "bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300",
        "quiet" => "bg-rose-100 text-rose-800 dark:bg-rose-900/40 dark:text-rose-300",
        "moving" => "bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300",
        "waiting" => "bg-sky-100 text-sky-800 dark:bg-sky-900/40 dark:text-sky-300",
        _ => "bg-zinc-100 text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400",
    }
}

#[component]
fn StatePill(state: String) -> impl IntoView {
    view! { <span class=format!("shrink-0 rounded px-1.5 py-0.5 text-[11px] font-medium {}", state_class(&state))>{state_label(&state)}</span> }
}

/// When it last moved: the latest of what's attached, else its own.
fn moved(w: &WorkItem) -> i64 {
    w.live.iter().map(|l| l.at).max().unwrap_or(0).max(w.fields.moved_at)
}

/// The kind of thing a ref points at, as a mark.
fn kind_mark(reference: &str) -> &'static str {
    match reference.split(':').next().unwrap_or("") {
        "session" => "◆",
        "request" => "✉",
        "thread" => "✉",
        "chat" => "💬",
        "decision" => "?",
        _ => "↗",
    }
}

/// Snooze until tomorrow at 9.
fn tomorrow() -> i64 {
    crate::brief::at_nine(1)
}

/// Sets a piece of work's status or snooze, then `after`.
fn settle(id: String, p: WorkPatch, after: impl Fn(Result<WorkItem, String>) + 'static) {
    spawn_local(async move { after(api::work_patch(id, p).await) });
}

/// The latest session attached, to resume.
fn last_session(w: &WorkItem) -> Option<String> {
    w.live
        .iter()
        .filter(|l| l.reference.starts_with("session:") && !matches!(l.state.as_str(), "gone" | "archived"))
        .max_by_key(|l| l.at)
        .and_then(|l| l.reference.strip_prefix("session:"))
        .map(str::to_owned)
}

/// Opens the work's latest session: where it's shown, else a tile of its own.
fn resume(w: &WorkItem) -> bool {
    match last_session(w) {
        Some(id) => {
            use_workspace().open(None, View::session(id), How::NewTile);
            true
        }
        None => false,
    }
}

#[component]
pub fn WorkPage() -> impl IntoView {
    let (list, tick) = open_work();
    let show_closed = RwSignal::new(false);
    let closed = LocalResource::new(move || {
        let on = show_closed.get();
        tick.track();
        async move { if on { api::work_list(true).await } else { Ok(Vec::new()) } }
    });
    let picked = RwSignal::new(0usize);
    let error = RwSignal::new(None::<String>);
    let adding = RwSignal::new(false);
    let (title, done_when) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let proposals = RwSignal::new(None::<Vec<WorkProposal>>);
    let finding = RwSignal::new(false);
    let col = use_column();
    let active = Signal::derive(move || col.as_ref().is_none_or(|c| c.active.get()));
    let items = Memo::new(move |_| list.get().and_then(Result::ok).unwrap_or_default());
    let open_item = move |id: String, how: How| match use_column() {
        Some(c) => c.open(View::Work { id }, how),
        None => use_workspace().open(None, View::Work { id }, how),
    };
    let open_item = StoredValue::new(open_item);
    let reload = move || tick.update(|t| *t += 1);
    let done = move |r: Result<WorkItem, String>| match r {
        Ok(_) => reload(),
        Err(e) => error.set(Some(e)),
    };
    let add = move || {
        let t = title.get_untracked();
        if t.trim().is_empty() {
            return;
        }
        let d = done_when.get_untracked();
        spawn_local(async move {
            match api::work_add(t, d, Vec::new()).await {
                Ok(_) => {
                    title.set(String::new());
                    done_when.set(String::new());
                    adding.set(false);
                    reload();
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let find = move |_| {
        finding.set(true);
        error.set(None);
        spawn_local(async move {
            match api::work_proposals().await {
                Ok(p) => proposals.set(Some(p)),
                Err(e) => error.set(Some(e)),
            }
            finding.set(false);
        });
    };
    // The keys: J/K move, Enter opens, E done, H tomorrow, # drop, R resume.
    let keys = window_event_listener(leptos::ev::keydown, move |ev| {
        let in_field = ev
            .target()
            .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
            .is_some_and(|el| matches!(el.tag_name().as_str(), "TEXTAREA" | "INPUT" | "SELECT") || el.has_attribute("contenteditable"));
        if !active.get_untracked() || in_field || ev.meta_key() || ev.ctrl_key() || ev.alt_key() {
            return;
        }
        let all = items.get_untracked();
        let Some(w) = all.get(picked.get_untracked()).cloned() else { return };
        match ev.key().as_str() {
            "j" | "ArrowDown" => picked.update(|p| *p = (*p + 1).min(all.len().saturating_sub(1))),
            "k" | "ArrowUp" => picked.update(|p| *p = p.saturating_sub(1)),
            "Enter" => open_item.with_value(|f| f(w.id.clone(), How::Beside)),
            "e" => settle(w.id, WorkPatch { status: Some("done".into()), ..Default::default() }, done),
            "h" => settle(w.id, WorkPatch { snooze_until: Some(tomorrow()), ..Default::default() }, done),
            "#" => settle(w.id, WorkPatch { status: Some("dropped".into()), ..Default::default() }, done),
            "r" => {
                if !resume(&w) {
                    error.set(Some("Nothing to resume: no session belongs to it yet. Pick it up instead.".into()));
                }
            }
            _ => return,
        }
        ev.prevent_default();
    });
    on_cleanup(move || keys.remove());
    view! {
        <main class="mx-auto max-w-4xl space-y-4 px-4 py-5">
            <header class="flex flex-wrap items-baseline gap-x-3 gap-y-1">
                <h1 class="text-xl font-semibold">"Work"</h1>
                <span class="text-sm text-zinc-500">{move || {
                    let all = items.get();
                    match (all.len(), waiting(&all)) {
                        (0, _) => "Nothing left open.".to_owned(),
                        (n, 0) => format!("{n} open"),
                        (n, w) => format!("{n} open · {w} need a look"),
                    }
                }}</span>
                <span class="flex-1"></span>
                <span class="hidden text-xs text-zinc-400 sm:inline">"J/K move · Enter open · E done · H tomorrow · # drop · R resume"</span>
            </header>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="flex flex-wrap gap-2">
                <button class=BUTTON on:click=move |_| adding.update(|a| *a = !*a)>"+ Add"</button>
                <button class=BUTTON_QUIET disabled=move || finding.get() on:click=find
                    title="An agent groups your open sessions and requests that belong to no work yet; you accept or skip each">
                    {move || if finding.get() { "Looking…" } else { "Find work in what's open" }}
                </button>
            </div>
            {move || adding.get().then(|| view! {
                <div class="space-y-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
                    <input class=INPUT placeholder="What you mean to get done" prop:value=move || title.get()
                        on:input=move |ev| title.set(event_target_value(&ev)) on:keydown=move |ev| if ev.key() == "Enter" { add() } />
                    <input class=INPUT placeholder="Done when… (optional)" prop:value=move || done_when.get()
                        on:input=move |ev| done_when.set(event_target_value(&ev)) on:keydown=move |ev| if ev.key() == "Enter" { add() } />
                    <div class="flex justify-end gap-2">
                        <button class=BUTTON_QUIET on:click=move |_| adding.set(false)>"Cancel"</button>
                        <button class=BUTTON on:click=move |_| add()>"Add"</button>
                    </div>
                </div>
            })}
            {move || proposals.get().map(|ps| view! { <Proposals ps proposals reload=Callback::new(move |()| reload()) /> })}
            {move || match list.get() {
                None => view! { <p class="text-sm text-zinc-500">"Loading…"</p> }.into_any(),
                Some(Err(e)) => view! { <ErrorBox message=e /> }.into_any(),
                Some(Ok(_)) if items.get().is_empty() => view! {
                    <p class="text-sm text-zinc-500">
                        "Nothing left open. Sessions you start join work an agent picks (or you pick in the form); "
                        "add a to-do above, or let an agent find work in what's open."
                    </p>
                }.into_any(),
                Some(Ok(_)) => view! {
                    <div class="space-y-4">
                        {c6_api::work::STATES.iter().filter(|s| !matches!(**s, "done" | "dropped")).map(|state| {
                            let state = (*state).to_owned();
                            let s2 = state.clone();
                            let these = move || items.get().into_iter().enumerate().filter(|(_, w)| w.state == s2).collect::<Vec<_>>();
                            let these = Memo::new(move |_| these());
                            view! {
                                <section class="space-y-1" class:hidden=move || these.get().is_empty()>
                                    <h2 class="text-xs font-medium uppercase tracking-wide text-zinc-500">
                                        {move || format!("{} ({})", state_label(&state), these.get().len())}
                                    </h2>
                                    <For each=move || these.get() key=|(i, w)| (*i, w.id.clone(), w.version, w.state.clone(), w.why.clone()) let:row>
                                        <Row w=row.1.clone() index=row.0 picked open_item=Callback::new(move |(id, how): (String, How)| open_item.with_value(|f| f(id, how))) />
                                    </For>
                                </section>
                            }
                        }).collect_view()}
                    </div>
                }.into_any(),
            }}
            <div class="pt-2">
                <label class="flex items-center gap-1.5 text-sm text-zinc-500">
                    <input type="checkbox" prop:checked=move || show_closed.get() on:change=move |ev| show_closed.set(event_target_checked(&ev)) />
                    "Show closed work"
                </label>
                {move || show_closed.get().then(|| view! {
                    <div class="mt-2 space-y-1">
                        {move || closed.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|w| {
                            let id = w.id.clone();
                            view! {
                                <button class="flex w-full items-center gap-2 rounded px-2 py-1 text-left text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800"
                                    on:click=move |ev| open_item.with_value(|f| f(id.clone(), How::from_mouse(&ev)))>
                                    <StatePill state=w.state.clone() />
                                    <span class="min-w-0 flex-1 truncate">{w.fields.title.clone()}</span>
                                    <span class="text-xs text-zinc-400">{w.fields.closed_at.map(since).unwrap_or_default()}</span>
                                </button>
                            }
                        }).collect_view()}
                    </div>
                })}
            </div>
        </main>
    }
}

/// One piece of work in the list.
#[component]
fn Row(w: WorkItem, index: usize, picked: RwSignal<usize>, open_item: Callback<(String, How)>) -> impl IntoView {
    let id = StoredValue::new(w.id.clone());
    let next = w.fields.note.as_ref().map(|n| n.next.clone()).filter(|n| !n.is_empty());
    let when = moved(&w);
    view! {
        <button class="block w-full rounded-md border px-3 py-2 text-left hover:bg-zinc-50 dark:hover:bg-zinc-900"
            class=("border-violet-400", move || picked.get() == index)
            class=("border-zinc-200", move || picked.get() != index)
            class=("dark:border-zinc-800", move || picked.get() != index)
            on:click=move |ev| { picked.set(index); open_item.run((id.get_value(), How::from_mouse(&ev))); }>
            <div class="flex items-center gap-2">
                <span class="min-w-0 flex-1 truncate font-medium">{w.fields.title.clone()}</span>
                <span class="shrink-0 text-xs text-zinc-400">{(when > 0).then(|| since(when))}</span>
            </div>
            {(!w.why.is_empty()).then(|| view! { <p class="truncate text-xs text-zinc-600 dark:text-zinc-400">{w.why.clone()}</p> })}
            {next.map(|n| view! { <p class="truncate text-xs text-zinc-500">{format!("Next: {n}")}</p> })}
            <div class="mt-1 flex flex-wrap gap-1">
                {w.live.iter().take(6).map(|l| view! {
                    <span class="max-w-48 truncate rounded bg-zinc-100 px-1.5 text-[11px] text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400" title=l.title.clone()>
                        {format!("{} {}", kind_mark(&l.reference), l.title)}
                    </span>
                }).collect_view()}
                {(w.live.len() > 6).then(|| view! { <span class="text-[11px] text-zinc-400">{format!("+{}", w.live.len() - 6)}</span> })}
            </div>
        </button>
    }
}

/// What an agent found in what's open: accept or skip each.
#[component]
fn Proposals(ps: Vec<WorkProposal>, proposals: RwSignal<Option<Vec<WorkProposal>>>, reload: Callback<()>) -> impl IntoView {
    let left = RwSignal::new(ps);
    let error = RwSignal::new(None::<String>);
    let drop_at = move |i: usize| {
        left.update(|l| {
            l.remove(i);
        });
        if left.get_untracked().is_empty() {
            proposals.set(None);
        }
    };
    view! {
        <section class="space-y-2 rounded-lg border border-violet-200 bg-violet-50/50 p-3 dark:border-violet-900 dark:bg-violet-950/20">
            <div class="flex items-center gap-2">
                <h2 class="flex-1 text-sm font-medium">"Work found in what's open"</h2>
                <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| proposals.set(None)>"Close"</button>
            </div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || {
                let l = left.get();
                if l.is_empty() {
                    return view! { <p class="text-sm text-zinc-500">"Nothing open belongs to no work: all filed."</p> }.into_any();
                }
                l.into_iter().enumerate().map(|(i, p)| {
                    let p2 = p.clone();
                    let accept = move |_| {
                        let p = p2.clone();
                        spawn_local(async move {
                            match api::work_add(p.title, p.done_when, p.attach).await {
                                Ok(_) => {
                                    drop_at(i);
                                    reload.run(());
                                }
                                Err(e) => error.set(Some(e)),
                            }
                        });
                    };
                    view! {
                        <div class="rounded-md border border-zinc-200 bg-white p-2 dark:border-zinc-800 dark:bg-zinc-900">
                            <div class="flex items-center gap-2">
                                <span class="min-w-0 flex-1 font-medium">{p.title.clone()}</span>
                                <button class=format!("{BUTTON} text-xs") on:click=accept>"Accept"</button>
                                <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| drop_at(i)>"Skip"</button>
                            </div>
                            {(!p.done_when.is_empty()).then(|| view! { <p class="text-xs text-zinc-500">{format!("Done when: {}", p.done_when)}</p> })}
                            <ul class="mt-1 text-xs text-zinc-600 dark:text-zinc-400">
                                {p.attach.iter().map(|a| view! { <li class="truncate">{format!("{} {}", kind_mark(&a.reference), a.title)}</li> }).collect_view()}
                            </ul>
                        </div>
                    }
                }).collect_view().into_any()
            }}
        </section>
    }
}

/// Where an attached thing opens.
#[component]
fn AttachedLink(l: LiveAttached) -> impl IntoView {
    let class = "min-w-0 flex-1 truncate text-sky-700 hover:underline dark:text-sky-400";
    let r = l.reference.clone();
    let text = format!("{} {}", kind_mark(&r), l.title);
    if let Some(id) = r.strip_prefix("session:") {
        view! { <Go to=View::session(id) class=class>{text}</Go> }.into_any()
    } else if let Some(id) = r.strip_prefix("request:").and_then(|i| i.parse::<i64>().ok()) {
        view! { <Go to=View::Brief own=true brief_request=id class=class>{text}</Go> }.into_any()
    } else if r.starts_with("decision:") {
        view! { <Go to=View::Decisions class=class>{text}</Go> }.into_any()
    } else if r.starts_with("thread:") {
        view! { <Go to=View::Mail own=true class=class>{text}</Go> }.into_any()
    } else if r.starts_with("chat:") {
        view! { <Go to=View::Chat own=true class=class>{text}</Go> }.into_any()
    } else {
        view! { <a href=r target="_blank" rel="noopener noreferrer" class=class>{text}</a> }.into_any()
    }
}

/// A piece of work: where it stands, what belongs to it, and the ways back in.
#[component]
pub fn WorkItemPage(id: String) -> impl IntoView {
    let id = StoredValue::new(id);
    let tick = RwSignal::new(0u32);
    crate::decisions::reload_quietly(tick);
    let item = LocalResource::new(move || {
        tick.track();
        api::work_get(id.get_value())
    });
    let shown = RwSignal::new(None::<Result<WorkItem, String>>);
    Effect::new(move || {
        if let Some(r) = item.get() {
            shown.set(Some(r));
        }
    });
    let error = RwSignal::new(None::<String>);
    let reload = move || tick.update(|t| *t += 1);
    let apply = move |r: Result<WorkItem, String>| match r {
        Ok(w) => shown.set(Some(Ok(w))),
        Err(e) => error.set(Some(e)),
    };
    let patch = move |p: WorkPatch| settle(id.get_value(), p, apply);
    let editing = RwSignal::new(false);
    let (title, done_when) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let attach_ref = RwSignal::new(String::new());
    let attach_title = RwSignal::new(String::new());
    let pickup = RwSignal::new(None::<(String, Option<Session>)>);
    let live = api::live();
    let others = LocalResource::new(|| api::work_list(false));
    let merging = RwSignal::new(false);
    let pick_up = move |w: WorkItem| {
        let seed = last_session(&w).and_then(|s| live.sessions.with_untracked(|ss| ss.iter().find(|x| x.id == s).cloned()));
        spawn_local(async move {
            match api::work_pickup(w.id.clone()).await {
                Ok(text) => pickup.set(Some((text, seed))),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let save = move || {
        let p = WorkPatch { title: Some(title.get_untracked()), done_when: Some(done_when.get_untracked()), ..Default::default() };
        editing.set(false);
        patch(p);
    };
    let attach = move || {
        let (r, t) = (attach_ref.get_untracked().trim().to_owned(), attach_title.get_untracked());
        if r.is_empty() {
            return;
        }
        spawn_local(async move {
            match api::work_attach(id.get_value(), r, t).await {
                Ok(w) => {
                    attach_ref.set(String::new());
                    attach_title.set(String::new());
                    shown.set(Some(Ok(w)));
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <main class="mx-auto max-w-3xl space-y-4 px-4 py-5">
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || match shown.get() {
                None => view! { <p class="text-sm text-zinc-500">"Loading…"</p> }.into_any(),
                Some(Err(e)) => view! { <ErrorBox message=e /> }.into_any(),
                Some(Ok(w)) => {
                    let open = w.fields.status == "open";
                    let w1 = w.clone();
                    let w2 = w.clone();
                    let w3 = w.clone();
                    view! {
                        <header class="space-y-1">
                            {move || if editing.get() {
                                view! {
                                    <div class="space-y-2">
                                        <input class=INPUT prop:value=move || title.get() on:input=move |ev| title.set(event_target_value(&ev))
                                            on:keydown=move |ev| if ev.key() == "Enter" { save() } />
                                        <input class=INPUT placeholder="Done when…" prop:value=move || done_when.get()
                                            on:input=move |ev| done_when.set(event_target_value(&ev)) on:keydown=move |ev| if ev.key() == "Enter" { save() } />
                                        <div class="flex justify-end gap-2">
                                            <button class=BUTTON_QUIET on:click=move |_| editing.set(false)>"Cancel"</button>
                                            <button class=BUTTON on:click=move |_| save()>"Save"</button>
                                        </div>
                                    </div>
                                }.into_any()
                            } else {
                                let (t, d) = (w1.fields.title.clone(), w1.fields.done_when.clone());
                                let (t2, d2) = (t.clone(), d.clone());
                                view! {
                                    <div class="flex items-start gap-2">
                                        <h1 class="min-w-0 flex-1 text-xl font-semibold">{t}</h1>
                                        <button class=format!("{BUTTON_QUIET} text-xs") title="Rename, or say what done means"
                                            on:click=move |_| { title.set(t2.clone()); done_when.set(d2.clone()); editing.set(true); }>"Edit"</button>
                                    </div>
                                    <p class="text-sm text-zinc-500">{if d.is_empty() { "Done when: not said".to_owned() } else { format!("Done when: {d}") }}</p>
                                }.into_any()
                            }}
                            <div class="flex flex-wrap items-center gap-2 text-sm">
                                <StatePill state=w.state.clone() />
                                <span class="text-zinc-600 dark:text-zinc-400">{w.why.clone()}</span>
                            </div>
                        </header>
                        {w.fields.offer.clone().filter(|_| open).map(|o| view! {
                            <div class="flex flex-wrap items-center gap-2 rounded-lg border border-emerald-300 bg-emerald-50 p-3 text-sm dark:border-emerald-800 dark:bg-emerald-950/30">
                                <span class="min-w-0 flex-1">{format!("Looks done: {}", o.evidence)}</span>
                                <button class=BUTTON on:click=move |_| patch(WorkPatch { status: Some("done".into()), ..Default::default() })>"Close it"</button>
                                <button class=BUTTON_QUIET on:click=move |_| patch(WorkPatch { dismiss_offer: true, ..Default::default() })>"Not yet"</button>
                            </div>
                        })}
                        <div class="flex flex-wrap gap-2">
                            {open.then(|| {
                                let (wa, wb) = (w2.clone(), w2.clone());
                                let can_resume = last_session(&wa).is_some();
                                view! {
                                    <button class=BUTTON disabled=!can_resume title="Open its latest session (R)"
                                        on:click=move |_| { resume(&wa); }>"Resume"</button>
                                    <button class=BUTTON_QUIET title="A new session that starts from where it stood"
                                        on:click=move |_| pick_up(wb.clone())>"Pick up in a new session"</button>
                                    <button class=BUTTON_QUIET on:click=move |_| patch(WorkPatch { status: Some("done".into()), ..Default::default() })>"Done"</button>
                                    <button class=BUTTON_QUIET on:click=move |_| patch(WorkPatch { snooze_until: Some(tomorrow()), ..Default::default() })>"Tomorrow"</button>
                                    <button class=BUTTON_QUIET on:click=move |_| patch(WorkPatch { status: Some("dropped".into()), ..Default::default() })>"Drop"</button>
                                    <button class=BUTTON_QUIET on:click=move |_| merging.update(|m| *m = !*m)>"Merge…"</button>
                                }
                            })}
                            {(!open).then(|| view! {
                                <button class=BUTTON on:click=move |_| patch(WorkPatch { status: Some("open".into()), ..Default::default() })>"Reopen"</button>
                            })}
                            {w.fields.snoozed_until.filter(|_| w.state == "snoozed").map(|_| view! {
                                <button class=BUTTON_QUIET on:click=move |_| patch(WorkPatch { snooze_until: Some(0), ..Default::default() })>"Wake it"</button>
                            })}
                        </div>
                        {move || merging.get().then(|| {
                            let me = w3.id.clone();
                            view! {
                                <div class="flex items-center gap-2 text-sm">
                                    <span class="text-zinc-500">"Merge into this:"</span>
                                    <select class=format!("{INPUT} w-auto!") on:change=move |ev| {
                                        let from = event_target_value(&ev);
                                        if from.is_empty() { return; }
                                        merging.set(false);
                                        spawn_local(async move { apply(api::work_merge(id.get_value(), from).await) });
                                    }>
                                        <option value="">"Choose work…"</option>
                                        {others.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|o| o.id != me).map(|o| view! {
                                            <option value=o.id.clone()>{o.fields.title.clone()}</option>
                                        }).collect_view()}
                                    </select>
                                </div>
                            }
                        })}
                        <section class="space-y-1 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
                            <div class="flex items-center gap-2">
                                <h2 class="flex-1 text-xs font-medium uppercase tracking-wide text-zinc-500">"Where was I"</h2>
                                {w.fields.note.as_ref().map(|n| view! { <span class="text-xs text-zinc-400">{since(n.at)}</span> })}
                                <button class="text-xs text-zinc-500 hover:underline" title="Write it again now"
                                    on:click=move |_| spawn_local(async move {
                                        match api::work_refresh(id.get_value()).await {
                                            Ok(()) => { let _ = set_timeout_with_handle(reload, std::time::Duration::from_secs(5)); }
                                            Err(e) => error.set(Some(e)),
                                        }
                                    })>"↻"</button>
                            </div>
                            {match w.fields.note.clone() {
                                Some(n) => view! {
                                    <dl class="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
                                        <dt class="text-zinc-500">"Done"</dt><dd>{n.done}</dd>
                                        <dt class="text-zinc-500">"Left"</dt><dd>{n.left}</dd>
                                        <dt class="text-zinc-500">"Next"</dt><dd class="font-medium">{n.next}</dd>
                                    </dl>
                                }.into_any(),
                                None => view! {
                                    <p class="text-sm text-zinc-500">{if w.fields.attached.is_empty() {
                                        "Nothing belongs to it yet: start a session on it (Pick up), or attach something below."
                                    } else {
                                        "No note yet: an agent writes one when something here moves."
                                    }}</p>
                                }.into_any(),
                            }}
                        </section>
                        <section class="space-y-1">
                            <h2 class="text-xs font-medium uppercase tracking-wide text-zinc-500">"What belongs to it"</h2>
                            {w.live.iter().map(|l| {
                                let r = l.reference.clone();
                                let by = w.fields.attached.iter().find(|a| a.reference == r).map(|a| a.by.clone()).unwrap_or_default();
                                let detail = w.fields.attached.iter().find(|a| a.reference == r).map(|a| a.detail.clone()).unwrap_or_default();
                                view! {
                                    <div class="flex items-center gap-2 rounded px-2 py-1 text-sm hover:bg-zinc-50 dark:hover:bg-zinc-900">
                                        <AttachedLink l=l.clone() />
                                        {(!l.state.is_empty()).then(|| view! { <span class="shrink-0 text-xs text-zinc-500">{l.state.replace('_', " ")}</span> })}
                                        {(!detail.is_empty()).then(|| view! { <span class="max-w-48 shrink truncate text-xs text-zinc-400" title=detail.clone()>{detail.clone()}</span> })}
                                        {(by == "guess").then(|| view! { <span class="shrink-0 text-[11px] text-zinc-400" title="An agent put it here; take it off if it doesn't belong">"guessed"</span> })}
                                        {(l.at > 0).then(|| view! { <span class="shrink-0 text-xs text-zinc-400">{since(l.at)}</span> })}
                                        <button class="shrink-0 text-xs text-zinc-400 hover:text-red-600" title="Take it off this work"
                                            on:click=move |_| { let r = r.clone(); spawn_local(async move { apply(api::work_detach(id.get_value(), r).await) }); }>"✕"</button>
                                    </div>
                                }
                            }).collect_view()}
                            <div class="flex flex-wrap gap-2 pt-1">
                                <input class=format!("{INPUT} min-w-0 flex-[2]") placeholder="Attach a link (a pull request, a doc…) or session:<id>"
                                    prop:value=move || attach_ref.get() on:input=move |ev| attach_ref.set(event_target_value(&ev))
                                    on:keydown=move |ev| if ev.key() == "Enter" { attach() } />
                                <input class=format!("{INPUT} min-w-0 flex-1") placeholder="Its name (optional)"
                                    prop:value=move || attach_title.get() on:input=move |ev| attach_title.set(event_target_value(&ev))
                                    on:keydown=move |ev| if ev.key() == "Enter" { attach() } />
                                <button class=BUTTON_QUIET on:click=move |_| attach()>"Attach"</button>
                            </div>
                        </section>
                    }.into_any()
                }
            }}
            {move || pickup.get().map(|(first, seed)| {
                let (title, note) = ("Pick this work up in a new session", "Starts where it stood: the note and what belongs to it. It joins this work.");
                let close = move |()| { pickup.set(None); reload(); };
                // Where its latest session ran, with its model and labels.
                match seed.as_ref().map(crate::sessions::SessionSeed::from_session) {
                    Some(seed) => view! {
                        <crate::sessions::SessionDialog title note first_message=first seed work=id.get_value() on_close=close />
                    }.into_any(),
                    None => view! {
                        <crate::sessions::SessionDialog title note first_message=first work=id.get_value() on_close=close />
                    }.into_any(),
                }
            })}
        </main>
    }
}

/// A session's work, in its header: which work it serves, and the way to
/// change that.
#[component]
pub fn SessionWork(session_id: String, title: Signal<String>) -> impl IntoView {
    let reference = StoredValue::new(format!("session:{session_id}"));
    let tick = RwSignal::new(0u32);
    let mine = LocalResource::new(move || {
        tick.track();
        api::work_of(reference.get_value())
    });
    let all = LocalResource::new(move || {
        tick.track();
        api::work_list(false)
    });
    let error = RwSignal::new(None::<String>);
    let current = move || mine.get().and_then(Result::ok).and_then(|w| w.into_iter().next());
    let change = move |to: String| {
        let from = current().map(|w| w.id);
        spawn_local(async move {
            let r = reference.get_value();
            let result = async {
                if from.as_deref() == Some(to.as_str()) {
                    return Ok(());
                }
                if let Some(old) = &from {
                    api::work_detach(old.clone(), r.clone()).await?;
                }
                match to.as_str() {
                    "none" => {}
                    "new" => {
                        let t = title.get_untracked();
                        let t = if t.trim().is_empty() { "New work".to_owned() } else { t };
                        api::work_add(t, String::new(), vec![c6_api::work::WorkRef { reference: r, title: String::new() }]).await?;
                    }
                    id => {
                        api::work_attach(id.to_owned(), r, String::new()).await?;
                    }
                }
                Ok::<(), String>(())
            }
            .await;
            if let Err(e) = result {
                error.set(Some(e));
            }
            tick.update(|t| *t += 1);
        });
    };
    view! {
        <span class="inline-flex min-w-0 items-center gap-1 text-xs text-zinc-500">
            <span>"Work:"</span>
            {move || current().map(|w| view! {
                <Go to=View::Work { id: w.id.clone() } class="max-w-56 truncate text-violet-700 hover:underline dark:text-violet-400" title=w.why.clone()>
                    {w.fields.title.clone()}
                </Go>
                <StatePill state=w.state.clone() />
            })}
            <select class="max-w-40 cursor-pointer rounded bg-transparent px-1 text-zinc-500 hover:bg-zinc-200/60 dark:hover:bg-zinc-800"
                title="The work this session serves (docs/work.md)"
                prop:value=move || { all.track(); mine.track(); String::new() }
                on:change=move |ev| { let to = event_target_value(&ev); if !to.is_empty() { change(to) } }>
                <option value="">{move || if current().is_some() { "change…" } else { "none · add to work…" }}</option>
                {move || current().is_some().then(|| view! { <option value="none">"Not part of any work"</option> })}
                <option value="new">"New work from this session"</option>
                {move || {
                    let me = current().map(|w| w.id).unwrap_or_default();
                    all.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|w| w.id != me).map(|w| view! {
                        <option value=w.id.clone()>{w.fields.title.clone()}</option>
                    }).collect_view()
                }}
            </select>
            {move || error.get().map(|e| view! { <span class="text-red-600">{e}</span> })}
        </span>
    }
}

/// The Brief's "Left open": work that needs you or went quiet.
#[component]
pub fn LeftOpen() -> impl IntoView {
    let (list, _) = open_work();
    move || {
        let items: Vec<WorkItem> = list
            .get()
            .and_then(Result::ok)
            .unwrap_or_default()
            .into_iter()
            .filter(|w| matches!(w.state.as_str(), "needs_you" | "quiet"))
            .collect();
        (!items.is_empty()).then(|| {
            let n = items.len();
            view! {
                <section class="space-y-1">
                    <h2 class="flex items-baseline gap-2 text-xs font-medium uppercase tracking-wide text-zinc-500">
                        {format!("Left open ({n})")}
                        <Go to=View::Work { id: String::new() } class="normal-case tracking-normal text-zinc-400 hover:underline">"all work"</Go>
                    </h2>
                    {items.into_iter().take(6).map(|w| view! {
                        <Go to=View::Work { id: w.id.clone() } class="flex items-center gap-2 rounded px-2 py-1 text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800">
                            <StatePill state=w.state.clone() />
                            <span class="min-w-0 flex-1 truncate">{w.fields.title.clone()}</span>
                            <span class="hidden max-w-[50%] truncate text-xs text-zinc-500 sm:inline">{w.why.clone()}</span>
                        </Go>
                    }).collect_view()}
                </section>
            }
        })
    }
}

/// A request in the Brief: the work it belongs to, or "Keep as work".
#[component]
pub fn RequestWork(id: i64, #[prop(into)] title: String) -> impl IntoView {
    let reference = StoredValue::new(format!("request:{id}"));
    let title = StoredValue::new(title);
    let tick = RwSignal::new(0u32);
    let mine = LocalResource::new(move || {
        tick.track();
        api::work_of(reference.get_value())
    });
    let error = RwSignal::new(None::<String>);
    let keep = move |_| {
        spawn_local(async move {
            let r = c6_api::work::WorkRef { reference: reference.get_value(), title: title.get_value() };
            match api::work_add(title.get_value(), String::new(), vec![r]).await {
                Ok(_) => tick.update(|t| *t += 1),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <span class="inline-flex items-center gap-1 text-xs">
            {move || match mine.get().and_then(Result::ok).and_then(|w| w.into_iter().next()) {
                Some(w) => view! {
                    <span class="text-zinc-500">"Work:"</span>
                    <Go to=View::Work { id: w.id.clone() } class="max-w-56 truncate text-violet-700 hover:underline dark:text-violet-400">{w.fields.title.clone()}</Go>
                }.into_any(),
                None => view! {
                    <button type="button" class=format!("{BUTTON_QUIET} text-xs") on:click=keep
                        title="Keep it in your work, so it isn't lost when you've answered and are waiting">"Keep as work"</button>
                }.into_any(),
            }}
            {move || error.get().map(|e| view! { <span class="text-red-600">{e}</span> })}
        </span>
    }
}

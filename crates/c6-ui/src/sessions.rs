//! Sessions list and the new-session form.

use c6_api::{Connection, CreateSession, Session, SessionStatus, WorkerStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::models::ModelInput;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, StatusPill, ago, clock, storage_get, storage_set};
use crate::workspace::{Go, How, View, use_column, use_workspace};

#[component]
pub fn SessionsPage() -> impl IntoView {
    let live = api::live();
    let now = clock();
    let connections = LocalResource::new(api::connections);
    let query = RwSignal::new(String::new());
    let status_filter = RwSignal::new(String::new());
    let label_filter = crate::labels::use_filter().0;
    let grouping = use_grouping();
    let folded = Folded::load(Folded::PAGE);

    view! {
        <main class="mx-auto max-w-6xl space-y-6 px-4 py-6">
            <Suspense fallback=|| ()>
                {move || connections.get().map(|r| match r {
                    Ok(conns) if conns.is_empty() => view! {
                        <div class="rounded-lg border border-dashed border-zinc-300 p-6 text-center text-sm text-zinc-600 dark:border-zinc-700 dark:text-zinc-400">
                            "Add an LLM connection in " <Go to=View::Settings { note: None, section: "models".into() } class="underline">"Settings"</Go> " to start a session."
                        </div>
                    }.into_any(),
                    Ok(conns) => view! { <NewSession connections=conns /> <RunTemplate /> }.into_any(),
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                })}
            </Suspense>

            <section class="space-y-2">
                <div class="flex flex-wrap items-center gap-2">
                    <h2 class="flex-1 text-sm font-medium text-zinc-500">"Sessions"</h2>
                    <input class=format!("{INPUT} w-56") placeholder="Search titles, labels, workers, paths" prop:value=move || query.get()
                        on:input=move |ev| query.set(event_target_value(&ev)) />
                    <select class=format!("{INPUT} w-32") prop:value=move || status_filter.get() on:change=move |ev| status_filter.set(event_target_value(&ev))>
                        <option value="">"All"</option>
                        <option value="running">"Running"</option>
                        <option value="idle">"Idle"</option>
                        <option value="failed">"Failed"</option>
                    </select>
                </div>
                <crate::labels::LabelFilterBar />
                <GroupBar grouping />
                <div class="divide-y divide-zinc-200 overflow-hidden rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                    {move || {
                        let q = query.get().to_lowercase();
                        let st = status_filter.get();
                        let wanted = label_filter.get();
                        let realm = crate::api::realm().get();
                        let sessions: Vec<Session> = live
                            .sessions
                            .get()
                            .into_iter()
                            .filter(|s| st.is_empty() || serde_json::to_value(s.status).ok().and_then(|v| v.as_str().map(str::to_owned)).as_deref() == Some(st.as_str()))
                            .filter(|s| crate::labels::matches(s, &wanted))
                            .filter(|s| realm.is_empty() || s.realm == realm)
                            .filter(|s| {
                                q.is_empty()
                                    || s.labels.iter().chain(s.auto_labels.iter()).any(|l| l.to_lowercase().contains(&q))
                                    || s.title.to_lowercase().contains(&q)
                                    || s.cwd.to_lowercase().contains(&q)
                                    || live.worker_name(&s.worker_id.to_string()).to_lowercase().contains(&q)
                            })
                            .collect();
                        if sessions.is_empty() {
                            let msg = if wanted.is_empty() && q.is_empty() { "No sessions yet." } else { "No sessions match." };
                            view! { <p class="px-4 py-8 text-center text-sm text-zinc-500">{msg}</p> }.into_any()
                        } else {
                            grouped(sessions, &grouping.levels.get(), String::new(), folded, &|s| view! { <SessionRow s now /> }.into_any(), false)
                        }
                    }}
                </div>
            </section>
        </main>
    }
}

/// How sessions are grouped: label keys, outermost first, kept in the head
/// (`session_grouping`) and shared by the sessions list and the rail.
#[derive(Clone, Copy)]
pub struct Grouping {
    levels: RwSignal<Vec<String>>,
}

impl Grouping {
    /// Loads it from the head (once, for the page: see [`use_grouping`]).
    pub fn load() -> Self {
        let levels = RwSignal::new(Vec::new());
        spawn_local(async move {
            if let Ok(v) = api::get_setting("session_grouping").await
                && let Ok(g) = serde_json::from_value::<c6_api::SessionGrouping>(v)
            {
                levels.set(g.levels);
            }
        });
        Grouping { levels }
    }

    fn save(&self, levels: Vec<String>) -> impl std::future::Future<Output = Result<(), String>> + use<> {
        self.levels.set(levels.clone());
        api::put_setting("session_grouping", serde_json::json!({ "levels": levels }))
    }
}

pub fn use_grouping() -> Grouping {
    expect_context::<Grouping>()
}

/// The groups you folded in one view of the list (in this browser).
#[derive(Clone, Copy)]
struct Folded {
    key: &'static str,
    paths: RwSignal<Vec<String>>,
}

impl Folded {
    const PAGE: &str = "c6.sessions.folded";
    const RAIL: &str = "c6.rail.folded";

    fn load(key: &'static str) -> Self {
        let folded = storage_get(key).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Folded { key, paths: RwSignal::new(folded) }
    }

    fn toggle(&self, path: &str) {
        self.paths.update(|c| {
            if let Some(i) = c.iter().position(|p| p == path) {
                c.remove(i);
            } else {
                c.push(path.to_owned());
            }
        });
        storage_set(self.key, &serde_json::to_string(&self.paths.get_untracked()).unwrap_or_default());
    }
}

/// A session's value for a label key: its first `key:` label (folders in
/// their `~/` form).
fn group_value(s: &Session, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    c6_api::labels::display_labels(&s.auto_labels).iter().chain(s.labels.iter()).find_map(|l| l.strip_prefix(&prefix).map(str::to_owned))
}

/// The sessions in nested groups, one level per key: most recently active
/// group first, sessions without the label last. `compact`: the rail's.
fn grouped(
    sessions: Vec<Session>,
    levels: &[String],
    path: String,
    folded: Folded,
    row: &dyn Fn(Session) -> AnyView,
    compact: bool,
) -> AnyView {
    let Some((key, rest)) = levels.split_first() else {
        return sessions.into_iter().map(row).collect_view().into_any();
    };
    let mut groups: Vec<(Option<String>, Vec<Session>)> = Vec::new();
    for s in sessions {
        let v = group_value(&s, key);
        match groups.iter_mut().find(|(g, _)| *g == v) {
            Some((_, list)) => list.push(s),
            None => groups.push((v, vec![s])),
        }
    }
    let latest = |list: &[Session]| list.iter().map(|s| s.updated_at).max().unwrap_or_default();
    groups.sort_by_key(|(v, list)| (v.is_none(), std::cmp::Reverse(latest(list))));
    groups
        .into_iter()
        .map(|(value, list)| {
            let here = format!("{path}{key}:{}\u{1f}", value.as_deref().unwrap_or(""));
            let (count, running) = (list.len(), list.iter().filter(|s| s.status == SessionStatus::Running).count());
            let name = match &value {
                Some(v) => view! { <span class="font-mono">{v.clone()}</span> }.into_any(),
                None => view! { <span class="italic">{format!("no {key}")}</span> }.into_any(),
            };
            let is_folded = {
                let here = here.clone();
                Memo::new(move |_| folded.paths.with(|c| c.contains(&here)))
            };
            let toggle = {
                let here = here.clone();
                move |_| folded.toggle(&here)
            };
            let rest = rest.to_vec();
            // Drawn once; folding hides them.
            let rows = grouped(list, &rest, here.clone(), folded, row, compact);
            let (button, body) = if compact {
                (
                    "flex w-full items-center gap-1 rounded px-1 py-0.5 text-left text-[11px] text-zinc-500 hover:bg-zinc-200/60 dark:hover:bg-zinc-800/60",
                    "ml-1.5 border-l border-zinc-200 pl-1 dark:border-zinc-800",
                )
            } else {
                (
                    "flex w-full items-center gap-2 bg-zinc-50 py-1.5 pl-3 pr-4 text-left text-xs text-zinc-600 hover:bg-zinc-100 dark:bg-zinc-900/60 dark:text-zinc-400 dark:hover:bg-zinc-900",
                    "divide-y divide-zinc-200 border-t border-zinc-200 pl-5 dark:divide-zinc-800 dark:border-zinc-800",
                )
            };
            view! {
                <div>
                    <button class=button on:click=toggle title=format!("{key}: {}", value.clone().unwrap_or_else(|| format!("no {key}")))>
                        <span class="w-3 shrink-0 text-sm text-zinc-500">{move || if is_folded.get() { "▸" } else { "▾" }}</span>
                        {(!compact).then(|| view! { <span class="shrink-0 text-zinc-400">{format!("{key}:")}</span> })}
                        <span class="min-w-0 truncate">{name}</span>
                        <span class="ml-auto shrink-0 tabular-nums">{count}</span>
                        {(running > 0).then(|| view! {
                            <span class="shrink-0 text-blue-600 dark:text-blue-400">{if compact { format!("· {running}▶") } else { format!("{running} running") }}</span>
                        })}
                    </button>
                    <div class=body class:hidden=move || is_folded.get()>
                        {rows}
                    </div>
                </div>
            }
        })
        .collect_view()
        .into_any()
}

/// "Group by": the levels (remove, or move one out a level) and a key to
/// add, from the labels your sessions have.
#[component]
pub fn GroupBar(grouping: Grouping) -> impl IntoView {
    let live = api::live();
    let error = RwSignal::new(None::<String>);
    let save = move |levels: Vec<String>| {
        let done = grouping.save(levels);
        spawn_local(async move { error.set(done.await.err()) });
    };
    let keys = Memo::new(move |_| {
        let mut keys: Vec<String> = ["repo", "host", "worker", "folder", "model"].map(str::to_owned).to_vec();
        live.sessions.with(|ss| {
            for s in ss {
                for l in &s.labels {
                    if let Some((k, _)) = l.split_once(':')
                        && !keys.iter().any(|x| x == k)
                    {
                        keys.push(k.to_owned());
                    }
                }
            }
        });
        keys
    });
    view! {
        <div class="flex flex-wrap items-center gap-2 text-xs text-zinc-500">
            <span>"Group by"</span>
            {move || {
                let levels = grouping.levels.get();
                levels.iter().enumerate().map(|(i, k)| {
                    let (outer, gone) = (levels.clone(), levels.clone());
                    view! {
                        {(i > 0).then(|| view! { <span class="text-zinc-400">"›"</span> })}
                        <span class="inline-flex items-center gap-1 rounded bg-violet-100 px-1.5 py-0.5 font-mono text-[11px] text-violet-800 dark:bg-violet-950 dark:text-violet-300">
                            {(i > 0).then(|| view! {
                                <button title="Group by this one level further out" on:click=move |_| {
                                    let mut l = outer.clone();
                                    l.swap(i - 1, i);
                                    save(l);
                                }>"‹"</button>
                            })}
                            {k.clone()}
                            <button title="Stop grouping by this" on:click=move |_| {
                                let mut l = gone.clone();
                                l.remove(i);
                                save(l);
                            }>"×"</button>
                        </span>
                    }
                }).collect_view()
            }}
            <select class=format!("{INPUT} w-auto! py-0.5 text-xs")
                prop:value=""
                on:change=move |ev| {
                    let k = event_target_value(&ev);
                    if !k.is_empty() {
                        let mut l = grouping.levels.get_untracked();
                        l.push(k);
                        save(l);
                    }
                }>
                <option value="">{move || if grouping.levels.with(Vec::is_empty) { "none" } else { "then by…" }}</option>
                {move || {
                    let chosen = grouping.levels.get();
                    keys.get().into_iter().filter(|k| !chosen.contains(k)).map(|k| { let v = k.clone(); view! { <option value=v>{k}</option> } }).collect_view()
                }}
            </select>
            {move || error.get().map(|e| view! { <span class="text-red-600">{e}</span> })}
        </div>
    }
}

/// A session's "⋯" in a list (its row, a card, the dock): archive it
/// without opening it, with Undo rather than a question; and what the list
/// adds (`dismiss`).
#[component]
pub(crate) fn SessionMenu(id: String, #[prop(optional)] dismiss: Option<Callback<()>>) -> impl IntoView {
    let notice = crate::util::Notice::get();
    let id = StoredValue::new(id);
    let archive = Callback::new(move |()| {
        spawn_local(async move {
            let sid = id.get_value();
            match api::set_archived(sid.clone(), true).await {
                Ok(()) => notice.show(
                    "Archived",
                    Some(Callback::new(move |()| {
                        let sid = sid.clone();
                        spawn_local(async move {
                            if let Err(e) = api::set_archived(sid, false).await {
                                notice.show(format!("Couldn't bring it back: {e}"), None);
                            }
                        });
                    })),
                ),
                Err(e) => notice.show(format!("Couldn't archive it: {e}"), None),
            }
        });
    });
    let mut items = Vec::new();
    if let Some(d) = dismiss {
        items.push(("Dismiss", d));
    }
    items.push(("Archive", archive));
    view! { <crate::util::Menu items row=true label="Session actions" /> }
}

#[component]
fn SessionRow(s: Session, now: RwSignal<i64>) -> impl IntoView {
    let live = api::live();
    let worker = live.worker_name(&s.worker_id.to_string());
    let updated = s.updated_at;
    view! {
        <div class="group flex items-center gap-1 pr-2 hover:bg-zinc-100 dark:hover:bg-zinc-900">
        <Go to=View::session(s.id.clone()) class="flex min-w-0 flex-1 items-center gap-3 py-3 pl-4" title="Open beside this list (⌘-click: new tile)">
            <div class="min-w-0 flex-1">
                <div class="truncate font-medium">{s.title.clone()}</div>
                <div class="truncate text-xs text-zinc-500">
                    {worker} " · " <span class="font-mono">{match (&s.ssh, s.cwd.is_empty()) {
                        (Some(t), _) => format!("⇄ {t}:{}", s.cwd),
                        (None, true) => "scratch workspace".to_owned(),
                        (None, false) => s.cwd.clone(),
                    }}</span> " · " {s.model.clone()}
                </div>
                {(!s.labels.is_empty() || !crate::labels::shown_auto(&s.auto_labels).is_empty()).then(|| view! {
                    <div class="mt-1 flex flex-wrap gap-1">
                        {crate::labels::shown_auto(&s.auto_labels).into_iter().map(|l| view! { <crate::labels::LabelChip label=l auto=true stay=true /> }).collect_view()}
                        {s.labels.iter().cloned().map(|l| view! { <crate::labels::LabelChip label=l stay=true /> }).collect_view()}
                    </div>
                })}
            </div>
            <StatusPill status=s.status />
            <span class="w-16 shrink-0 text-right text-xs text-zinc-500">{move || ago(now.get() - updated)}</span>
        </Go>
        <SessionMenu id=s.id.clone() />
        </div>
    }
}

/// The rail's sessions: every session (in this realm, with the label
/// filter's labels), grouped as the sessions list is, one narrow row each.
/// Hovering a row pops out its details, with its colour and "New like
/// this"; clicking opens it (⌘-click: a new tile).
#[component]
pub fn SessionsRail(open: RwSignal<bool>) -> impl IntoView {
    let live = api::live();
    let grouping = use_grouping();
    let folded = Folded::load(Folded::RAIL);
    let label_filter = crate::labels::use_filter().0;
    let query = RwSignal::new(String::new());
    let hover = Hover::new();
    let group_at = RwSignal::new(None::<(f64, f64)>);
    let like_this = RwSignal::new(None::<SessionSeed>);
    let sessions = Memo::new(move |_| {
        let (q, wanted, realm) = (query.get().to_lowercase(), label_filter.get(), api::realm().get());
        live.sessions
            .get()
            .into_iter()
            .filter(|s| crate::labels::matches(s, &wanted) && (realm.is_empty() || s.realm == realm))
            .filter(|s| {
                q.is_empty()
                    || s.title.to_lowercase().contains(&q)
                    || s.labels.iter().chain(s.auto_labels.iter()).any(|l| l.to_lowercase().contains(&q))
            })
            .collect::<Vec<_>>()
    });
    let small = "rounded px-1 font-normal normal-case tracking-normal hover:bg-zinc-200/60 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100";
    view! {
        <div class="flex items-center gap-1 pb-1 pl-1.5 pr-3 pt-2 text-[11px] font-semibold uppercase tracking-wide text-zinc-500">
            <button class=crate::activity::FOLD title=move || if open.get() { "Fold" } else { "Unfold" } on:click=move |_| open.update(|o| *o = !*o)>
                {move || if open.get() { "▾" } else { "▸" }}
            </button>
            <span class="flex-1 pl-0.5">"Sessions"
                {move || (!open.get()).then(|| view! { <span class="ml-1.5 rounded-full bg-zinc-200 px-1.5 font-normal tabular-nums dark:bg-zinc-800">{sessions.with(Vec::len)}</span> })}
            </span>
            <button class=small title="Group by label keys (shared with the sessions list)"
                on:click=move |ev| group_at.set(Some(crate::workspace::below(&ev, 288.0)))>
                {move || match grouping.levels.get().as_slice() {
                    [] => "group ▾".to_owned(),
                    levels => format!("{} ▾", levels.join("›")),
                }}
            </button>
            <Go to=View::Sessions class=small title="All sessions, and a new one">"＋"</Go>
        </div>
        <crate::workspace::Popover at=group_at class="w-72">
            <GroupBar grouping />
        </crate::workspace::Popover>
        <Show when=move || open.get()>
        <div class="px-1.5 pb-1">
            <input class="w-full rounded border border-zinc-200 bg-white px-1.5 py-0.5 text-xs dark:border-zinc-800 dark:bg-zinc-900"
                placeholder="Filter" prop:value=move || query.get() on:input=move |ev| query.set(event_target_value(&ev)) />
        </div>
        <div class="min-h-0 flex-1 overflow-y-auto px-1.5 pb-3" on:scroll=move |_| hover.hide_now()>
            {move || {
                let list = sessions.get();
                if list.is_empty() {
                    return view! { <p class="px-2 py-2 text-xs text-zinc-500">"No sessions match."</p> }.into_any();
                }
                grouped(list, &grouping.levels.get(), String::new(), folded, &move |s| view! { <RailRow s hover /> }.into_any(), true)
            }}
        </div>
        </Show>
        {move || hover.shown.get().and_then(|(id, x, y)| live.sessions.with(|ss| ss.iter().find(|s| s.id == id).cloned()).map(|s| view! {
            <RailCard s x y hover like_this />
        }))}
        {move || like_this.get().map(|seed| view! { <NewSessionDialog seed on_close=move |()| like_this.set(None) /> })}
    }
}

/// Which rail row's details pop out, and where; it stays while the pointer
/// is on the row or the card, and goes a moment after it leaves both.
#[derive(Clone, Copy)]
struct Hover {
    shown: RwSignal<Option<(String, f64, f64)>>,
    generation: StoredValue<u64>,
}

impl Hover {
    fn new() -> Self {
        Hover { shown: RwSignal::new(None), generation: StoredValue::new(0) }
    }

    fn show(&self, id: String, row: &web_sys::Element) {
        self.keep();
        let r = row.get_bounding_client_rect();
        let vh = window().inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(800.0);
        // Beside the rail, level with the row, kept on screen.
        let y = r.top().min(vh - 340.0).max(8.0);
        let rail = row.closest("aside").ok().flatten().map(|a| a.get_bounding_client_rect().right()).unwrap_or(r.right());
        self.shown.set(Some((id, rail + 6.0, y)));
    }

    /// The pointer came back: don't hide.
    fn keep(&self) {
        self.generation.update_value(|g| *g += 1);
    }

    fn hide_soon(&self) {
        self.generation.update_value(|g| *g += 1);
        let (me, this) = (self.generation.get_value(), *self);
        set_timeout(
            move || {
                if this.generation.try_get_value() == Some(me) {
                    this.shown.set(None);
                }
            },
            std::time::Duration::from_millis(250),
        );
    }

    fn hide_now(&self) {
        self.keep();
        if self.shown.with_untracked(Option::is_some) {
            self.shown.set(None);
        }
    }
}

#[component]
fn RailRow(s: Session, hover: Hover) -> impl IntoView {
    let id = s.id.clone();
    let colour = crate::colour::of(&s);
    let state = if s.awaiting_approval {
        "h-1.5 w-1.5 shrink-0 animate-pulse rounded-full bg-amber-500"
    } else {
        match s.status {
            SessionStatus::Running => "h-1.5 w-1.5 shrink-0 animate-pulse rounded-full bg-blue-500",
            SessionStatus::Failed => "h-1.5 w-1.5 shrink-0 rounded-full bg-red-500",
            _ => "hidden",
        }
    };
    view! {
        <div
            on:mouseenter=move |ev| {
                if let Some(el) = ev.current_target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok()) {
                    hover.show(id.clone(), &el);
                }
            }
            on:mouseleave=move |_| hover.hide_soon()
        >
            <Go to=View::session(s.id.clone()) class="flex items-center gap-1.5 rounded px-1 py-0.5 text-[13px] hover:bg-zinc-200/60 dark:hover:bg-zinc-800/60">
                <span class=format!("h-2.5 w-1 shrink-0 rounded-sm {}", crate::colour::solid(colour))></span>
                <span class="min-w-0 flex-1 truncate">{s.title.clone()}</span>
                <span class=state></span>
            </Go>
        </div>
    }
}

/// A rail row's details: all of its title, its state, where it runs, its
/// labels and colour, and what you can do with it.
#[component]
fn RailCard(s: Session, x: f64, y: f64, hover: Hover, like_this: RwSignal<Option<SessionSeed>>) -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let now = clock();
    let worker = live.worker_name(&s.worker_id.to_string());
    let updated = s.updated_at;
    let (open_id, tile_id) = (s.id.clone(), s.id.clone());
    let seed = SessionSeed::from_session(&s);
    let place = match (&s.ssh, s.cwd.is_empty()) {
        (Some(t), _) => format!("⇄ {t}:{}", s.cwd),
        (None, true) => "scratch workspace".to_owned(),
        (None, false) => c6_api::labels::display_labels(&s.auto_labels)
            .iter()
            .find_map(|l| l.strip_prefix("folder:").map(str::to_owned))
            .unwrap_or_else(|| s.cwd.clone()),
    };
    let act = "rounded-md border border-zinc-200 px-2 py-1 text-xs hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800";
    view! {
        <div class="fixed z-40 w-72 space-y-2 rounded-lg border border-zinc-200 bg-white p-3 text-sm shadow-xl dark:border-zinc-700 dark:bg-zinc-900"
            style=format!("left:{x}px;top:{y}px")
            on:mouseenter=move |_| hover.keep()
            on:mouseleave=move |_| hover.hide_soon()>
            <div class="flex items-start gap-2">
                <div class="min-w-0 flex-1 break-words font-medium">{s.title.clone()}</div>
                <StatusPill status=s.status />
            </div>
            {s.awaiting_approval.then(|| view! { <p class="text-xs font-medium text-amber-600">"A tool call waits for your approval"</p> })}
            <div class="space-y-0.5 text-xs text-zinc-500">
                <div class="truncate">{worker} " · " {s.model.clone()}</div>
                <div class="break-all font-mono">{place}</div>
                <div>{move || format!("active {}", ago(now.get() - updated))}</div>
            </div>
            {(!s.labels.is_empty() || !crate::labels::shown_auto(&s.auto_labels).is_empty()).then(|| view! {
                <div class="flex flex-wrap gap-1">
                    {crate::labels::shown_auto(&s.auto_labels).into_iter().map(|l| view! { <crate::labels::LabelChip label=l auto=true stay=true /> }).collect_view()}
                    {s.labels.iter().cloned().map(|l| view! { <crate::labels::LabelChip label=l stay=true /> }).collect_view()}
                </div>
            })}
            <crate::colour::Palette session=s.id.clone() />
            <div class="flex flex-wrap gap-1.5 pt-1">
                <button class=act on:click=move |_| { hover.hide_now(); ws.open(None, View::session(open_id.clone()), How::Beside) }>"Open"</button>
                <button class=act on:click=move |_| { hover.hide_now(); ws.open(None, View::session(tile_id.clone()), How::NewTile) }>"New tile"</button>
                {(!s.observed && s.instance.is_empty()).then(|| view! {
                    <button class=act title="Start a new, empty session with this one's worker, folder, model and labels (you can change them first)"
                        on:click=move |_| { hover.hide_now(); like_this.set(Some(seed.clone())) }>"New like this"</button>
                })}
            </div>
        </div>
    }
}

/// What a new session starts from when it is made like another one: where
/// it runs, its model and its labels (the agents, skills, hooks and
/// commands that apply follow from those).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionSeed {
    pub worker_id: String,
    /// The folder on the worker, or on the SSH host.
    pub cwd: String,
    pub ssh: Option<String>,
    pub connection_id: String,
    pub model: String,
    pub effort: String,
    pub labels: Vec<String>,
    /// It ran in a git worktree of `cwd`: the new one gets its own.
    pub worktree: bool,
    pub realm: String,
}

impl SessionSeed {
    pub fn from_session(s: &Session) -> Self {
        SessionSeed {
            worker_id: s.worker_id.to_string(),
            cwd: s.cwd.clone(),
            ssh: s.ssh.clone(),
            connection_id: s.connection_id.clone(),
            model: s.model.clone(),
            effort: s.effort.clone().unwrap_or_default(),
            labels: s.labels.clone(),
            worktree: false,
            realm: s.realm.clone(),
        }
    }
}

#[component]
fn NewSession(connections: Vec<Connection>) -> impl IntoView {
    view! { <SessionForm connections /> }
}

/// A popup with the new-session form, filled in from `seed`: change what
/// you like and start it. The new session opens beside the current one.
#[component]
pub fn NewSessionDialog(seed: SessionSeed, #[prop(into)] on_close: Callback<()>) -> impl IntoView {
    let connections = LocalResource::new(api::connections);
    let seed = StoredValue::new(seed);
    view! {
        <div class="fixed inset-0 z-30 flex items-start justify-center overflow-y-auto bg-black/40 px-4 py-16" on:click=move |_| on_close.run(())
            on:keydown=move |ev| {
                // Not the session's Esc, which stops its turn.
                if ev.key() == "Escape" {
                    ev.stop_propagation();
                    on_close.run(());
                }
            }>
            <div class="w-full max-w-3xl space-y-3 rounded-lg bg-white p-4 shadow-xl outline-none dark:bg-zinc-900" tabindex="-1" on:click=|ev| ev.stop_propagation()>
                <div class="flex items-center gap-2">
                    <h2 class="flex-1 font-medium">"New session like this one"</h2>
                    <button class=format!("{BUTTON_QUIET} text-xs text-zinc-500") title="Close (Esc)" on:click=move |_| on_close.run(())>"✕"</button>
                </div>
                <p class="text-xs text-zinc-500">"A new, empty session with this one's worker, folder, model and labels (so the same agents, skills and hooks). Change anything before starting it."</p>
                <Suspense fallback=|| view! { <p class="text-sm text-zinc-500">"Loading…"</p> }>
                    {move || connections.get().map(|r| match r {
                        Ok(conns) if conns.is_empty() => view! { <p class="text-sm text-zinc-500">"Add an LLM connection in Settings to start a session."</p> }.into_any(),
                        Ok(conns) => view! { <SessionForm connections=conns seed=seed.get_value() on_started=on_close on_cancel=on_close /> }.into_any(),
                        Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    })}
                </Suspense>
            </div>
        </div>
    }
}

/// The new-session form. With a `seed` it starts from those values instead
/// of the ones remembered in this browser, and remembers none of its own.
#[component]
fn SessionForm(
    connections: Vec<Connection>,
    #[prop(optional)] seed: Option<SessionSeed>,
    /// After the session started (and opened).
    #[prop(optional, into)]
    on_started: Option<Callback<()>>,
    /// Shows a Cancel button.
    #[prop(optional, into)]
    on_cancel: Option<Callback<()>>,
) -> impl IntoView {
    let live = api::live();
    let (ws, col) = (use_workspace(), use_column());
    let seeded = seed.is_some();
    let seed = seed.unwrap_or_default();
    // Radio groups and datalists are per document: each form its own names.
    static FORMS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = FORMS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (where_name, recent_id) = (format!("c6-where-{n}"), format!("c6-ssh-recent-{n}"));

    let worker = RwSignal::new(if seeded { seed.worker_id.clone() } else { storage_get("c6.worker").unwrap_or_default() });
    let cwd = RwSignal::new(if seeded && seed.ssh.is_none() { seed.cwd.clone() } else { String::new() });
    let picking = RwSignal::new(false);
    // Or on an SSH host, through the worker: the target and a folder there.
    let remote = RwSignal::new(if seeded { seed.ssh.is_some() } else { storage_get("c6.where").as_deref() == Some("ssh") });
    let ssh = RwSignal::new(seed.ssh.clone().unwrap_or_default());
    let remote_dir = RwSignal::new(if seed.ssh.is_some() { seed.cwd.clone() } else { String::new() });
    // Hosts from the inventory, offered by name.
    let inventory = LocalResource::new(api::hosts);
    let recent_ssh = move || -> Vec<String> {
        storage_get(&format!("c6.ssh.{}", worker.get())).map(|s| c6_api::labels::parse_labels(&s)).unwrap_or_default()
    };
    let worker_online = move || {
        let w = worker.get();
        live.workers.with(|ws| ws.iter().any(|x| x.id.to_string() == w && x.status == WorkerStatus::Online))
    };
    // Empty: "Auto", your routing rules choose the connection and model.
    // A seed's connection, if it is still there.
    let seeded_connection = (seeded && connections.iter().any(|x| x.id == seed.connection_id)).then(|| seed.connection_id.clone());
    let remembered_connection = seeded_connection
        .clone()
        .or_else(|| storage_get("c6.connection").filter(|c| c.is_empty() || connections.iter().any(|x| &x.id == c)));
    let connection = RwSignal::new(remembered_connection.clone().unwrap_or_else(|| connections[0].id.clone()));
    let has_routes = RwSignal::new(false);
    spawn_local(async move {
        let routes = api::get_setting("routes").await.ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
        let any = routes.iter().any(|r| r["enabled"].as_bool() != Some(false));
        has_routes.set(any);
        // First time here with rules set up: start on Auto.
        if any && remembered_connection.is_none() {
            connection.set(String::new());
        }
    });
    let auto = move || connection.get().is_empty();
    let model = RwSignal::new(if seeded_connection.is_some() { seed.model.clone() } else { String::new() });
    let effort = RwSignal::new(if seeded { seed.effort.clone() } else { storage_get("c6.effort").unwrap_or_default() });
    let worktree =
        RwSignal::new(if seeded { seed.worktree && seed.ssh.is_none() } else { storage_get("c6.worktree").as_deref() == Some("1") });
    let prompt = RwSignal::new(String::new());
    // Your labels for the session (remembered per folder).
    let labels = RwSignal::new(seed.labels.join(", "));
    let seed_realm = StoredValue::new(seed.realm.clone());
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let conns = StoredValue::new(connections);
    // Effort only where the connection passes it on to the model.
    // On Auto, the rule sets it.
    let effort_supported = move || conns.with_value(|cs| cs.iter().find(|c| c.id == connection.get()).is_some_and(|c| c.supports_effort));

    // Pick a worker once the fleet loads: the last one used if it is online,
    // otherwise an online one. A worker the user picks by hand is kept, and
    // so is a seed's.
    let chosen_by_hand = RwSignal::new(seeded);
    Effect::new(move || {
        // The memex is a node, not a worker: it runs no sessions.
        let ws: Vec<c6_api::Worker> = live.workers.get().into_iter().filter(|w| w.role != "memex").collect();
        if chosen_by_hand.get_untracked() {
            return;
        }
        let current = worker.get_untracked();
        let status = |id: &str| ws.iter().find(|w| w.id.to_string() == id).map(|w| w.status);
        if status(&current) == Some(WorkerStatus::Online) {
            return;
        }
        let pick = ws
            .iter()
            .find(|w| w.status == WorkerStatus::Online)
            .or_else(|| (status(&current).is_none()).then(|| ws.iter().find(|w| w.status != WorkerStatus::Revoked)).flatten());
        if let Some(w) = pick {
            worker.set(w.id.to_string());
        }
    }); // Remember the last directory per worker (a seed's folder first).
    Effect::new(move |first: Option<()>| {
        let w = worker.get();
        if first.is_none() && seeded {
            return;
        }
        let remembered = storage_get(&format!("c6.cwd.{w}")).unwrap_or_default();
        cwd.set(remembered.clone());
        // Forget a remembered folder that no longer exists there.
        if !remembered.is_empty() {
            spawn_local(async move {
                if api::fs_list(w.clone(), remembered.clone(), false).await.is_err() && cwd.get_untracked() == remembered {
                    cwd.set(String::new());
                }
            });
        }
    });
    // A seed's labels stay whatever the folder.
    if !seeded {
        Effect::new(move || {
            let (w, c) = (worker.get(), cwd.get());
            labels.set(storage_get(&format!("c6.labels.{w}.{c}")).unwrap_or_default());
        });
    }
    // Default model per connection (a seed's model for its connection).
    let keep_model = seeded_connection.is_some();
    Effect::new(move |first: Option<()>| {
        let c = connection.get();
        if first.is_none() && keep_model {
            return;
        }
        if c.is_empty() {
            model.set(String::new());
            return;
        }
        let default = conns.with_value(|cs| cs.iter().find(|x| x.id == c).and_then(|x| x.default_model.clone()));
        model.set(storage_get(&format!("c6.model.{c}")).or(default).unwrap_or_default());
    });

    let submit = move || {
        if pending.get_untracked() {
            return;
        }
        let on_ssh = remote.get_untracked();
        let target = ssh.get_untracked().trim().to_owned();
        if on_ssh && target.is_empty() {
            error.set(Some("Enter the SSH host (user@host, or an alias from the worker's ~/.ssh/config)".into()));
            return;
        }
        if !on_ssh && cwd.get_untracked().is_empty() && worktree.get_untracked() {
            error.set(Some("A git worktree needs a folder".into()));
            return;
        }
        let Ok(worker_id) = worker.get_untracked().parse() else {
            error.set(Some("Pick a worker".into()));
            return;
        };
        let req = CreateSession {
            worker_id,
            cwd: if on_ssh { remote_dir.get_untracked().trim().to_owned() } else { cwd.get_untracked() },
            ssh: on_ssh.then(|| target.clone()),
            connection_id: connection.get_untracked(),
            model: model.get_untracked(),
            title: None,
            prompt: Some(prompt.get_untracked()).filter(|p| !p.trim().is_empty()),
            attachments: Vec::new(),
            labels: c6_api::labels::parse_labels(&labels.get_untracked()),
            effort: Some(effort.get_untracked()).filter(|e| !e.is_empty() && effort_supported()),
            worktree: worktree.get_untracked() && !on_ssh,
            // A seed's realm; else the realm you are looking at (else home).
            realm: if seeded { Some(seed_realm.get_value()) } else { Some(crate::api::realm().get_untracked()) }.filter(|r| !r.is_empty()),
        };
        if on_ssh {
            // Recent targets of this worker, newest first.
            let mut recent = recent_ssh();
            recent.retain(|t| *t != target);
            recent.insert(0, target.clone());
            recent.truncate(10);
            storage_set(&format!("c6.ssh.{}", req.worker_id), &recent.join(","));
        }
        // A seeded form leaves the main form's defaults alone.
        if !seeded {
            storage_set("c6.where", if on_ssh { "ssh" } else { "worker" });
            storage_set("c6.worktree", if worktree.get_untracked() { "1" } else { "0" });
            storage_set("c6.effort", &effort.get_untracked());
            storage_set("c6.worker", &req.worker_id.to_string());
            storage_set(&format!("c6.cwd.{}", req.worker_id), &req.cwd);
            storage_set(&format!("c6.labels.{}.{}", req.worker_id, req.cwd), &req.labels.join(", "));
            storage_set("c6.connection", &req.connection_id);
            if !req.connection_id.is_empty() {
                storage_set(&format!("c6.model.{}", req.connection_id), &req.model);
            }
        }
        pending.set(true);
        spawn_local(async move {
            match api::create_session(req).await {
                // Beside the form, so another can start right away.
                Ok(s) => {
                    match col {
                        Some(c) => c.open(View::session(s.id), How::Beside),
                        None => ws.open(None, View::session(s.id), How::Beside),
                    }
                    if let Some(f) = on_started {
                        f.run(());
                    }
                }
                Err(e) => error.set(Some(e)),
            }
            pending.set(false);
        });
    };
    let submit_click = submit;
    let prompt_box = NodeRef::<leptos::html::Textarea>::new();
    // In a popup, start typing the first message right away.
    if on_cancel.is_some() {
        Effect::new(move || {
            if let Some(el) = prompt_box.get() {
                let _ = el.focus();
            }
        });
    }

    view! {
        {move || picking.get().then(|| view! {
            <crate::files::FolderPicker
                worker_id=worker.get_untracked()
                start=cwd.get_untracked()
                on_select=Callback::new(move |p: String| { cwd.set(p); picking.set(false); })
                on_cancel=Callback::new(move |_| picking.set(false))
            />
        })}
        <section class=if seeded { "space-y-3" } else { "space-y-3 rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-800 dark:bg-zinc-900/50" }>
            <div class="grid gap-3 sm:grid-cols-[1fr_2fr_auto]">
                <label class="block text-xs text-zinc-500">
                    "Worker"
                    <select class=format!("mt-1 {INPUT}") on:change=move |ev| { chosen_by_hand.set(true); worker.set(event_target_value(&ev)) } prop:value=move || worker.get()>
                        {move || live.workers.get().into_iter().filter(|w| w.status != WorkerStatus::Revoked && w.role != "memex").map(|w| {
                            let label = if w.status == WorkerStatus::Online { w.name.clone() } else { format!("{} (offline)", w.name) };
                            view! { <option value=w.id.to_string()>{label}</option> }
                        }).collect_view()}
                    </select>
                </label>
                <div class="block text-xs text-zinc-500 sm:col-span-2">
                    <div class="flex items-center gap-3">
                        <label class="flex items-center gap-1"><input type="radio" name=where_name.clone() prop:checked=move || !remote.get() on:change=move |_| remote.set(false) />"Folder on the worker"</label>
                        <label class="flex items-center gap-1" title="Work on another machine over SSH, using the worker's keys and ~/.ssh/config">
                            <input type="radio" name=where_name prop:checked=move || remote.get() on:change=move |_| remote.set(true) />"SSH host, through the worker"
                        </label>
                    </div>
                    <div class="mt-1 flex gap-2" class:hidden=move || !remote.get()>
                        <input class=format!("{INPUT} min-w-0 flex-1 font-mono") list=recent_id.clone() placeholder="user@host or an ssh_config alias"
                            prop:value=move || ssh.get() on:input=move |ev| ssh.set(event_target_value(&ev)) />
                        <datalist id=recent_id>
                            {move || inventory.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|h| h.host.enabled).map(|h| {
                                let label = format!("{} · {}", h.host.target, h.host.labels.join(", "));
                                view! { <option value=h.host.name.clone() label=label></option> }
                            }).collect_view()}
                            {move || recent_ssh().into_iter().map(|t| view! { <option value=t></option> }).collect_view()}
                        </datalist>
                        <input class=format!("{INPUT} w-40! font-mono") placeholder="folder there (~)"
                            prop:value=move || remote_dir.get() on:input=move |ev| remote_dir.set(event_target_value(&ev)) />
                    </div>
                    <div class="mt-1 flex gap-2" class:hidden=move || remote.get()>
                        <button class=format!("{INPUT} min-w-0 flex-1 truncate text-left font-mono disabled:opacity-60")
                            disabled=move || !worker_online()
                            title=move || if worker_online() { "Choose a folder on the worker" } else { "The worker is offline" }
                            on:click=move |_| picking.set(true)>
                            {move || if cwd.get().is_empty() { "No folder: a scratch workspace (choose one…)".to_owned() } else { cwd.get() }}
                        </button>
                        {move || (!cwd.get().is_empty()).then(|| view! {
                            <button class="shrink-0 rounded-md px-2 text-xs text-zinc-500 hover:bg-zinc-200/60 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                                title="Work in a scratch workspace instead of a folder" on:click=move |_| cwd.set(String::new())>"✕"</button>
                        })}
                    </div>
                </div>
                <label class="block text-xs text-zinc-500">
                    "Connection"
                    <select class=format!("mt-1 {INPUT}") on:change=move |ev| connection.set(event_target_value(&ev)) prop:value=move || connection.get()>
                        {move || (has_routes.get() || auto()).then(|| view! { <option value="">"Auto (your routing rules)"</option> })}
                        {conns.get_value().into_iter().map(|c| view! { <option value=c.id.clone()>{c.name.clone()}</option> }).collect_view()}
                    </select>
                </label>
                {move || auto().then(|| view! {
                    <p class="text-xs text-zinc-500">
                        "The connection, model and effort come from the first rule in Settings → Model routing that matches this session's labels."
                    </p>
                })}
                <label class="block text-xs text-zinc-500" class:hidden=auto>
                    "Model"
                    <div class="mt-1"><ModelInput connection=connection value=model on_set=move |v| model.set(v) class=INPUT.to_owned() placeholder="model id (empty: routing rules, else the default)" /></div>
                </label>
                <label class="block text-xs text-zinc-500" class:hidden=move || !effort_supported()>
                    "Effort"
                    <select class=format!("mt-1 {INPUT}") prop:value=move || effort.get() on:change=move |ev| effort.set(event_target_value(&ev))>
                        <option value="">"default"</option>
                        <option value="low">"low"</option>
                        <option value="medium">"medium"</option>
                        <option value="high">"high"</option>
                        <option value="xhigh">"xhigh"</option>
                        <option value="max">"max"</option>
                    </select>
                </label>
            </div>
            <label class="block text-xs text-zinc-500">
                "Labels"
                <input class=format!("mt-1 font-mono {INPUT}") placeholder="optional, e.g. client:acme, spike (repo, folder, worker and model are added automatically)"
                    prop:value=move || labels.get() on:input=move |ev| labels.set(event_target_value(&ev)) />
            </label>
            <textarea node_ref=prompt_box class=format!("{INPUT} min-h-24 resize-y")
                placeholder=if seeded { "First message (optional: leave it empty to start an empty session; ⌘/Ctrl+Enter to start)" } else { "What should the agent do? (⌘/Ctrl+Enter to start)" }
                prop:value=move || prompt.get()
                on:input=move |ev| prompt.set(event_target_value(&ev))
                on:keydown=move |ev| {
                    if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key()) { ev.prevent_default(); submit(); }
                }
            ></textarea>
            {move || {
                let is_cc = conns.with_value(|cs| cs.iter().any(|c| c.id == connection.get() && c.kind == "claude_code"));
                let w = worker.get();
                let missing = live.workers.with(|ws| ws.iter().any(|x| x.id.to_string() == w && x.status == WorkerStatus::Online && !x.capabilities.iter().any(|c| c == "claude_code")));
                (is_cc && missing).then(|| view! {
                    <p class="text-sm text-amber-700 dark:text-amber-400">"This worker doesn't have the claude CLI installed (or it isn't on the worker's PATH; set C6_CLAUDE_BIN)."</p>
                })
            }}
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="flex items-center justify-end gap-4">
                <label class="flex items-center gap-1.5 text-sm text-zinc-600 dark:text-zinc-400" class:hidden=move || remote.get() title="Create a c6/… branch and a separate checkout for this session">
                    <input type="checkbox" prop:checked=move || worktree.get() on:change=move |ev| worktree.set(event_target_checked(&ev)) />
                    "New git worktree"
                </label>
                {on_cancel.map(|c| view! { <button class=BUTTON_QUIET on:click=move |_| c.run(())>"Cancel"</button> })}
                <button class=BUTTON disabled=move || pending.get() on:click=move |_| submit_click()>"Start session"</button>
            </div>
        </section>
    }
}

/// Live status for a session id, from the session list feed.
pub fn live_status(id: &str) -> Option<SessionStatus> {
    api::live().sessions.with(|ss| ss.iter().find(|s| s.id == id).map(|s| s.status))
}

/// Runs a template: one session, or a fleet run (then mission control,
/// filtered to the run).
#[component]
fn RunTemplate() -> impl IntoView {
    let (ws, col) = (use_workspace(), use_column());
    let filter = crate::labels::use_filter().0;
    // Every realm's templates (an app's are in its realm).
    let templates = LocalResource::new(api::all_templates);
    // "realm/name".
    let chosen = RwSignal::new(String::new());
    let input = RwSignal::new(String::new());
    let pending = RwSignal::new(false);
    let result = RwSignal::new(None::<Result<String, String>>);
    let run = move || {
        let key = chosen.get_untracked();
        let Some((realm, name)) = key.split_once('/').map(|(r, n)| (r.to_owned(), n.to_owned())) else { return };
        if pending.get_untracked() {
            return;
        }
        pending.set(true);
        spawn_local(async move {
            match api::run_template(name, input.get_untracked(), Some(realm)).await {
                Ok(r) => {
                    let mut note = format!("Started {} session{}", r.sessions.len(), if r.sessions.len() == 1 { "" } else { "s" });
                    if !r.errors.is_empty() {
                        note.push_str(&format!(" ({} could not start: {})", r.errors.len(), r.errors.join("; ")));
                    }
                    result.set(Some(Ok(note)));
                    input.set(String::new());
                    if let [one] = r.sessions.as_slice() {
                        match col {
                            Some(c) => c.open(View::session(one.id.clone()), How::Beside),
                            None => ws.open(None, View::session(one.id.clone()), How::Beside),
                        }
                    } else {
                        filter.set(vec![format!("run:{}", r.run_id)]);
                        ws.open(None, View::Mission, How::Beside);
                    }
                }
                Err(e) => result.set(Some(Err(e))),
            }
            pending.set(false);
        });
    };
    move || {
        let list: Vec<c6_api::RealmTemplate> =
            templates.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|t| t.template.enabled).collect();
        (!list.is_empty()).then(|| {
            // (key, label, description): another realm's templates say whose.
            let descriptions: Vec<(String, String, String)> = list
                .iter()
                .map(|t| {
                    let label = if t.realm == "home" { t.template.name.clone() } else { format!("{} ({})", t.template.name, t.realm) };
                    (format!("{}/{}", t.realm, t.template.name), label, t.template.description.clone())
                })
                .collect();
            let takes_input: Vec<String> =
                list.iter().filter(|t| t.template.prompt.contains("{{input}}")).map(|t| format!("{}/{}", t.realm, t.template.name)).collect();
            let takes_input = StoredValue::new(takes_input);
            let wants_input = move || takes_input.with_value(|v| v.contains(&chosen.get()));
            view! {
                <section class="space-y-2 rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-800 dark:bg-zinc-900/50">
                    <div class="flex flex-wrap items-center gap-2">
                        <span class="text-sm font-medium">"Run a template"</span>
                        <select class=format!("{INPUT} w-auto!") prop:value=move || chosen.get() on:change=move |ev| chosen.set(event_target_value(&ev))>
                            <option value="">"Choose…"</option>
                            {descriptions.iter().map(|(k, l, d)| view! { <option value=k.clone() title=d.clone()>{l.clone()}</option> }).collect_view()}
                        </select>
                        <input class=format!("{INPUT} min-w-48 flex-1") placeholder=move || if wants_input() { "input for {{input}}" } else { "(this template takes no input)" }
                            prop:disabled=move || !wants_input() prop:value=move || input.get() on:input=move |ev| input.set(event_target_value(&ev))
                            on:keydown=move |ev| if ev.key() == "Enter" { run() } />
                        <button class=BUTTON prop:disabled=move || chosen.get().is_empty() || pending.get() on:click=move |_| run()>"Run"</button>
                    </div>
                    {move || result.get().map(|r| match r {
                        Ok(m) => view! { <p class="text-sm text-emerald-600">{m}</p> }.into_any(),
                        Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    })}
                </section>
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_takes_where_and_how_a_session_runs() {
        let s: Session = serde_json::from_value(serde_json::json!({
            "id": "s1", "worker_id": "00000000-0000-0000-0000-000000000001", "title": "t", "cwd": "/src/app",
            "connection_id": "anthropic", "model": "claude-x", "status": "idle", "error": null, "effort": "high",
            "created_at": 0, "updated_at": 0, "last_seq": 0, "labels": ["client:acme"], "auto_labels": ["repo:app"],
            "ssh": "admin@web1", "realm": "work"
        }))
        .unwrap();
        let seed = SessionSeed::from_session(&s);
        assert_eq!(seed.worker_id, "00000000-0000-0000-0000-000000000001");
        assert_eq!((seed.cwd.as_str(), seed.ssh.as_deref()), ("/src/app", Some("admin@web1")));
        assert_eq!((seed.connection_id.as_str(), seed.model.as_str(), seed.effort.as_str()), ("anthropic", "claude-x", "high"));
        // Only your labels: the automatic ones are made again for the new session.
        assert_eq!(seed.labels, vec!["client:acme".to_owned()]);
        assert_eq!(seed.realm, "work");
        assert!(!seed.worktree);
    }
}

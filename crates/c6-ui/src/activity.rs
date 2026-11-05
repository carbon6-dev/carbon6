//! Keeping track of many sessions: the dock (desktop), the inbox, mission
//! control, and the phone's bottom bar.

use c6_api::{ContentBlock, EventKind, Session, SessionStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{ErrorBox, ago, now_ms};
use crate::workspace::{Go, How, Tracked, View, use_workspace};

fn dot(status: SessionStatus, tracked: Tracked) -> &'static str {
    match (status, tracked) {
        (_, Tracked::Approval) => "h-2 w-2 shrink-0 animate-pulse rounded-full bg-amber-500",
        (SessionStatus::Running, _) => "h-2 w-2 shrink-0 animate-pulse rounded-full bg-blue-500",
        (SessionStatus::Failed, _) => "h-2 w-2 shrink-0 rounded-full bg-red-500",
        // Amber is for what waits on you; a finished turn is just done.
        (_, Tracked::Attention) => "h-2 w-2 shrink-0 rounded-full bg-emerald-500",
        _ => "h-2 w-2 shrink-0 rounded-full bg-zinc-400",
    }
}

fn what(status: SessionStatus, tracked: Tracked) -> &'static str {
    match (status, tracked) {
        (_, Tracked::Approval) => "needs your approval",
        (SessionStatus::Running, _) => "running",
        (SessionStatus::Failed, _) => "failed",
        (_, Tracked::Attention) => "finished, not opened yet",
        (SessionStatus::Starting, _) => "starting",
        _ => "idle",
    }
}

/// A rail section's ▾/▸.
pub const FOLD: &str = "w-3 shrink-0 text-left text-muted hover:text-zinc-900 dark:hover:text-zinc-100";

/// A rail section folded or open, remembered in this browser.
fn section_open(key: &'static str) -> RwSignal<bool> {
    let open = RwSignal::new(crate::util::storage_get(key).as_deref() != Some("0"));
    Effect::new(move || crate::util::storage_set(key, if open.get() { "1" } else { "0" }));
    open
}

/// The rail (desktop): Activity, the running, finished-unseen and pinned
/// sessions, and below it every session, grouped; each section folds.
/// Both narrowed by the shared label filter (shown, with what it hides).
#[component]
pub fn Dock() -> impl IntoView {
    let activity = section_open("c6.dock.activity");
    let sessions = section_open("c6.dock.sessions");
    let fold = FOLD;
    // Hidden, the dock leaves a strip at the left edge: pointing at it slides
    // the dock out over the tiles, and it goes again once you move away.
    let kept = section_open("c6.dock.kept");
    let peek = RwSignal::new(false);
    let timer = StoredValue::new(None::<TimeoutHandle>);
    let after = move |ms: u64, show: bool| {
        if let Some(t) = timer.get_value() {
            t.clear();
        }
        let t = set_timeout_with_handle(move || peek.set(show), std::time::Duration::from_millis(ms)).ok();
        timer.set_value(t);
    };
    let toggle = Callback::new(move |()| {
        kept.update(|k| *k = !*k);
        peek.set(false);
    });
    // ⌘\ (Ctrl+\) hides it or keeps it open.
    let keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if (ev.meta_key() || ev.ctrl_key()) && !ev.alt_key() && ev.key() == "\\" {
            ev.prevent_default();
            toggle.try_run(());
        }
    });
    on_cleanup(move || keys.remove());
    view! {
        <div class=move || if kept.get() { "hidden w-60 shrink-0 lg:flex" } else { "relative z-30 hidden w-2 shrink-0 lg:block" }
            on:mouseenter=move |_| if !kept.get_untracked() { after(80, true) }
            on:mouseleave=move |_| if !kept.get_untracked() { after(250, false) }>
            {move || (!kept.get()).then(|| view! {
                <button class="absolute inset-0 border-r border-zinc-200 bg-zinc-50 hover:bg-zinc-200 dark:border-zinc-800 dark:bg-zinc-950 dark:hover:bg-zinc-800"
                    title="Show the sidebar (⌘\\ keeps it open)" aria-label="Show the sidebar" on:click=move |_| toggle.run(())></button>
            })}
            <aside class=move || {
                let at = if kept.get() {
                    "w-full"
                } else if peek.get() {
                    "absolute inset-y-0 left-0 w-60 translate-x-0 shadow-xl"
                } else {
                    "pointer-events-none invisible absolute inset-y-0 left-0 w-60 -translate-x-full"
                };
                format!("flex flex-col border-r border-zinc-200 bg-zinc-50 transition-[translate,visibility] duration-150 ease-out motion-reduce:transition-none dark:border-zinc-800 dark:bg-zinc-950 {at}")
            }>
                <div class=move || if !activity.get() { "flex shrink-0 flex-col" } else if sessions.get() { "flex max-h-[45%] min-h-0 shrink-0 flex-col" } else { "flex min-h-0 flex-1 flex-col" }>
                    <DockActivity open=activity fold kept toggle />
                </div>
                <div class=move || if sessions.get() { "flex min-h-0 flex-1 flex-col border-t border-zinc-200 dark:border-zinc-800" } else { "flex shrink-0 flex-col border-t border-zinc-200 dark:border-zinc-800" }>
                    <crate::sessions::SessionsRail open=sessions />
                </div>
            </aside>
        </div>
    }
}

#[component]
fn DockActivity(open: RwSignal<bool>, fold: &'static str, kept: RwSignal<bool>, toggle: Callback<()>) -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let filter = crate::labels::use_filter().0;
    let all = Memo::new(move |_| ws.tracked(&live));
    let hidden = Memo::new(move |_| ws.minimized());
    // A minimized session shows in Minimized, not again below.
    let minimized_sessions = Memo::new(move |_| {
        hidden.with(|h| {
            h.iter()
                .flat_map(|(_, _, views)| views.iter())
                .filter_map(|v| match v {
                    View::Session { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect::<std::collections::HashSet<_>>()
        })
    });
    let items = Memo::new(move |_| {
        let f = filter.get();
        all.get()
            .into_iter()
            .filter(|(s, _)| crate::labels::matches(s, &f) && !minimized_sessions.with(|m| m.contains(&s.id)))
            .collect::<Vec<_>>()
    });
    view! {
            <div class="flex items-center justify-between pb-1 pl-1.5 pr-3 pt-3 text-[11px] font-semibold uppercase tracking-wide text-muted">
                <span class="flex items-center gap-1.5">
                    <button class=fold title=move || if open.get() { "Fold" } else { "Unfold" } on:click=move |_| open.update(|o| *o = !*o)>
                        {move || if open.get() { crate::icons::chevron_down().into_any() } else { crate::icons::chevron_right().into_any() }}
                    </button>
                    "Activity"
                    {move || (!open.get() && !items.with(Vec::is_empty)).then(|| view! {
                        <span class="rounded-full bg-zinc-200 px-1.5 font-normal tabular-nums dark:bg-zinc-800">{items.with(Vec::len)}</span>
                    })}
                </span>
                <span class="flex items-center gap-2">
                    <Go to=View::Mission class="inline-flex items-center gap-1 font-normal normal-case tracking-normal hover:text-zinc-900 dark:hover:text-zinc-100" title="Mission control">{crate::icons::grid_plus()}"all"</Go>
                    <button class="font-normal normal-case tracking-normal hover:text-zinc-900 dark:hover:text-zinc-100"
                        title=move || if kept.get() { "Hide the sidebar: it slides out when you point at the left edge (⌘\\)" } else { "Keep the sidebar open (⌘\\)" }
                        on:click=move |_| toggle.run(())>
                        {move || if kept.get() { "«" } else { "»" }}
                    </button>
                </span>
            </div>
            <Show when=move || open.get()>
            {move || (!filter.with(Vec::is_empty)).then(|| {
                let hidden = all.with(Vec::len) - items.with(Vec::len);
                view! {
                    <div class="mx-1.5 mb-1 space-y-1 rounded-md bg-sky-50 px-2 py-1.5 text-[11px] text-sky-900 dark:bg-sky-950 dark:text-sky-200">
                        <div class="flex flex-wrap items-center gap-1">
                            {filter.get().into_iter().map(|l| view! { <span class="rounded bg-sky-100 px-1 font-mono dark:bg-sky-900">{l}</span> }).collect_view()}
                            <button class="ml-auto underline" title="Show all sessions again" on:click=move |_| filter.set(Vec::new())>"clear"</button>
                        </div>
                        {(hidden > 0).then(|| view! { <div class="text-sky-700 dark:text-sky-400">{format!("{hidden} hidden by the filter")}</div> })}
                    </div>
                }
            })}
            <div class="min-h-0 flex-1 overflow-y-auto px-1.5 pb-3">
                {move || hidden.with(|h| (!h.is_empty()).then(|| view! {
                    <div class="px-2 pb-0.5 pt-2 text-[10px] font-semibold uppercase tracking-wide text-muted">"Minimized"</div>
                    {h.iter().map(|(key, n, views)| view! { <MinimizedItem key=*key n=*n views=views.clone() /> }).collect_view()}
                }))}
                {move || {
                    let items = items.get();
                    if items.is_empty() && hidden.with(Vec::is_empty) {
                        let msg = if filter.with(Vec::is_empty) {
                            view! { "Running sessions, ones that finish while you're elsewhere, and ones you pin ("<span class="inline-flex align-text-bottom">{crate::icons::star(false)}</span>") show here." }.into_any()
                        } else {
                            view! { "Nothing with these labels is running, finished or pinned." }.into_any()
                        };
                        return view! { <p class="px-2 py-2 text-xs text-muted">{msg}</p> }.into_any();
                    }
                    let group = |which: Tracked, title: &'static str| {
                        let rows: Vec<_> = items.iter().filter(|(_, t)| *t == which).cloned().collect();
                        (!rows.is_empty()).then(|| {
                            let clear = (which == Tracked::Attention).then(|| view! {
                                <button class="font-normal normal-case tracking-normal hover:text-zinc-900 dark:hover:text-zinc-100" title="Mark them all seen"
                                    on:click=move |_| ws.attention.set(Vec::new())>"clear"</button>
                            });
                            view! {
                                <div class="flex items-center justify-between px-2 pb-0.5 pt-2 text-[10px] font-semibold uppercase tracking-wide text-muted">
                                    {title} {clear}
                                </div>
                                {rows.into_iter().map(|(s, t)| view! { <DockItem s t /> }).collect_view()}
                            }
                        })
                    };
                    view! {
                        {group(Tracked::Approval, "Waiting for you")}
                        {group(Tracked::Running, "Running")}
                        {group(Tracked::Attention, "Finished")}
                        {group(Tracked::Pinned, "Pinned")}
                    }.into_any()
                }}
            </div>
            </Show>
    }
}

/// A minimized tile: what it shows (a session's colour and state), and
/// bringing it back or closing it.
#[component]
fn MinimizedItem(key: u64, n: usize, views: Vec<View>) -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let views = StoredValue::new(views);
    let label = move || views.with_value(|vs| vs.iter().map(|v| v.label(&live)).collect::<Vec<_>>().join("  ›  "));
    let session = Memo::new(move |_| {
        views.with_value(|vs| match vs.last() {
            Some(View::Session { id, .. }) => live.sessions.with(|ss| ss.iter().find(|s| &s.id == id).cloned()),
            _ => None,
        })
    });
    let state = move || {
        session.get().map(|s| {
            let seen = !ws.attention.with(|a| a.contains(&s.id));
            let t = if s.awaiting_approval {
                Tracked::Approval
            } else if seen {
                Tracked::Pinned
            } else {
                Tracked::Attention
            };
            (crate::marks::fill(&crate::marks::of(&s)), dot(s.display_status(), t), what(s.display_status(), t))
        })
    };
    view! {
        <div class="group flex items-center gap-2 rounded-md px-2 py-1.5 hover:bg-zinc-200/60 dark:hover:bg-zinc-800/60">
            <span class="h-6 w-1 shrink-0 rounded-sm" style=move || state().map(|(fill, _, _)| fill).unwrap_or_default()></span>
            <span class=move || state().map_or("h-2 w-2 shrink-0", |(_, d, _)| d)></span>
            <button class="min-w-0 flex-1 text-left" title=tile_title(n, "Bring it back") on:click=move |_| ws.show_tile(key)>
                <div class="truncate text-sm">{label}</div>
                <div class="truncate text-[11px] text-muted">{move || state().map_or("minimized", |(_, _, w)| w)}</div>
            </button>
            {tile_badge(n)}
            <button class="row-action flex h-8 w-6 shrink-0 items-center justify-center text-xs text-muted hover:text-zinc-900 dark:hover:text-zinc-100"
                title="Close this tile" aria-label="Close" on:click=move |_| ws.close_tile(key)>{crate::icons::x()}</button>
        </div>
    }
}

/// What a tile number means, for its title (⌘ and the number reach the
/// first nine).
fn tile_title(n: usize, what: &str) -> String {
    if n <= 9 { format!("{what}: tile {n} (⌘{n})") } else { format!("{what}: tile {n}") }
}

/// The number of the tile a session is open in.
pub(crate) fn tile_badge(n: usize) -> impl IntoView {
    view! {
        <span class="shrink-0 rounded bg-zinc-200 px-1 font-mono text-[10px] tabular-nums text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400"
            title=tile_title(n, "Open")>{n}</span>
    }
}

#[component]
fn DockItem(s: Session, t: Tracked) -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let worker = live.session_worker(&s);
    let id = s.id.clone();
    let number = {
        let id = id.clone();
        Memo::new(move |_| ws.tile_number(&id))
    };
    let dismissable = t == Tracked::Attention;
    view! {
        <div class="group flex items-center gap-2 rounded-md px-2 py-1.5 hover:bg-zinc-200/60 dark:hover:bg-zinc-800/60">
            <span class="h-6 w-1 shrink-0 rounded-sm" style=crate::marks::fill(&crate::marks::of(&s)) title="Its mark"></span>
            <span class=dot(s.display_status(), t)></span>
            <Go to=View::session(s.id.clone()) class="min-w-0 flex-1" title="Open (⌘-click: new tile)">
                <div class="flex items-center gap-1.5">
                    <span class="min-w-0 flex-1 truncate text-sm">{s.title.clone()}</span>
                    <span class="flex shrink-0 items-center gap-0.5"><crate::aspects::AspectGlyphs s=s.clone() /></span>
                </div>
                <div title=s.on_head.then_some(api::HEAD_TITLE) class="truncate text-[11px] text-muted">{format!("{} · {}", what(s.display_status(), t), worker)}</div>
            </Go>
            {move || number.get().map(tile_badge)}
            {dismissable.then(|| {
                let id = id.clone();
                view! { <button class="row-action flex h-8 w-6 shrink-0 items-center justify-center text-xs text-muted hover:text-zinc-900 dark:hover:text-zinc-100" title="Dismiss" aria-label="Dismiss" on:click=move |_| ws.dismiss(&id)>{crate::icons::x()}</button> }
            })}
            <crate::sessions::SessionMenu id=id.clone() />
        </div>
    }
}

/// What needs attention, what's running, and what's pinned.
#[component]
pub fn InboxPage() -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let filter = crate::labels::use_filter().0;
    let items = Memo::new(move |_| {
        let f = filter.get();
        ws.tracked(&live).into_iter().filter(|(s, _)| crate::labels::matches(s, &f)).collect::<Vec<_>>()
    });
    let section = move |title: &'static str, which: Tracked, empty: fn() -> AnyView| {
        view! {
            <section class="space-y-2">
                <h2 class="text-sm font-semibold text-muted">{title}</h2>
                {move || {
                    let rows: Vec<_> = items.get().into_iter().filter(|(_, t)| *t == which).collect();
                    if rows.is_empty() {
                        view! { <p class="text-sm text-muted">{empty()}</p> }.into_any()
                    } else {
                        view! {
                            <div class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                                {rows.into_iter().map(|(s, t)| view! { <InboxRow s t /> }).collect_view()}
                            </div>
                        }.into_any()
                    }
                }}
            </section>
        }
    };
    view! {
        <main class="mx-auto max-w-3xl space-y-6 px-4 py-6">
            <crate::labels::LabelFilterBar />
            <MailNeedsReply />
            {move || {
                // Sessions the filter hides, so an empty inbox is not misread.
                let f = filter.get();
                let hidden = if f.is_empty() { 0 } else { ws.tracked(&live).into_iter().filter(|(s, _)| !crate::labels::matches(s, &f)).count() };
                (hidden > 0).then(|| view! {
                    <p class="-mt-4 text-xs text-muted">{format!("{hidden} more without these labels.")}</p>
                })
            }}
            {move || items.with(|i| i.iter().any(|(_, t)| *t == Tracked::Approval)).then(|| section("Needs your approval", Tracked::Approval, || ().into_any()))}
            {section("Finished, not opened yet", Tracked::Attention, || "Nothing finished while you were elsewhere.".into_any())}
            {move || items.with(|i| i.iter().any(|(_, t)| *t == Tracked::Attention)).then(|| view! {
                <button class="-mt-4 text-xs text-muted underline" on:click=move |_| ws.attention.set(Vec::new())>"Mark all seen"</button>
            })}
            {section("Running", Tracked::Running, || "No sessions are running.".into_any())}
            {section("Pinned", Tracked::Pinned, || view! { "Pin a session with "<span class="inline-flex align-text-bottom">{crate::icons::star(false)}</span>" in its header to keep it here." }.into_any())}
        </main>
    }
}

#[component]
fn InboxRow(s: Session, t: Tracked) -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let worker = live.session_worker(&s);
    let (dismiss_id, pin_id) = (s.id.clone(), s.id.clone());
    view! {
        <div class="flex items-center gap-3 px-4 py-3">
            <span class=dot(s.display_status(), t)></span>
            <Go to=View::session(s.id.clone()) class="min-w-0 flex-1">
                <div class="truncate font-medium">{s.title.clone()}</div>
                <div title=s.on_head.then_some(api::HEAD_TITLE) class="truncate text-xs text-muted">{format!("{} · {} · {}", what(s.display_status(), t), worker, ago(now_ms() - s.updated_at))}</div>
                {(!s.labels.is_empty() || !crate::labels::shown_auto(&s.auto_labels).is_empty()).then(|| view! {
                    <div class="mt-1 flex flex-wrap gap-1">
                        {crate::labels::shown_auto(&s.auto_labels).into_iter().map(|l| view! { <crate::labels::LabelChip label=l auto=true stay=true /> }).collect_view()}
                        {s.labels.iter().cloned().map(|l| view! { <crate::labels::LabelChip label=l stay=true /> }).collect_view()}
                    </div>
                })}
            </Go>
            {(t == Tracked::Attention).then(|| view! {
                <button class="rounded-md px-2 py-1 text-xs text-muted hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=move |_| ws.dismiss(&dismiss_id)>"Dismiss"</button>
            })}
            {(t == Tracked::Pinned).then(|| view! {
                <button class="rounded-md px-2 py-1 text-xs text-muted hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=move |_| ws.toggle_pin(&pin_id)>"Unpin"</button>
            })}
        </div>
    }
}

/// Live cards for every tracked session (or the most recent ones).
#[component]
pub fn MissionPage() -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let filter = crate::labels::use_filter().0;
    let ids = Memo::new(move |_| {
        let f = filter.get();
        let tracked: Vec<String> =
            ws.tracked(&live).into_iter().filter(|(s, _)| crate::labels::matches(s, &f)).map(|(s, _)| s.id).collect();
        if !tracked.is_empty() {
            return tracked;
        }
        let mut recent: Vec<Session> = live.sessions.get().into_iter().filter(|s| crate::labels::matches(s, &f)).collect();
        recent.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        recent.into_iter().take(6).map(|s| s.id).collect()
    });
    view! {
        <main class="space-y-3 px-4 py-4">
            <p class="text-xs text-muted">"Running, finished-unseen and pinned sessions (or your most recent), live. Reply from a card, or open it beside this view."</p>
            <crate::labels::LabelFilterBar />
            {move || ids.with(Vec::is_empty).then(|| view! { <p class="text-sm text-muted">"No sessions with these labels."</p> })}
            <div class="grid gap-3 [grid-template-columns:repeat(auto-fill,minmax(20rem,1fr))]">
                <For each=move || ids.get() key=|id| id.clone() let:id>
                    <MissionCard id />
                </For>
            </div>
        </main>
    }
}

#[component]
fn MissionCard(id: String) -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let feed = crate::session::Feed::open(id.clone(), crate::session::Window::Tail);
    let sid = id.clone();
    let session = Memo::new(move |_| live.sessions.with(|ss| ss.iter().find(|s| s.id == sid).cloned()));
    let tracked = {
        let id = id.clone();
        move || {
            if ws.attention.with(|a| a.contains(&id)) { Tracked::Attention } else { Tracked::Pinned }
        }
    };
    // The stream replays the whole log first; show the card once it has,
    // so it doesn't scroll through it.
    let caught_up = feed.synced;
    // The latest words: streaming text, else the last reply or message.
    let tail = Memo::new(move |_| {
        if !caught_up.get() {
            return String::new();
        }
        let live_text = feed.streaming.with(|s| s.text.clone());
        if !live_text.trim().is_empty() {
            return c6_api::citations::clean(&live_text).into_owned();
        }
        feed.events.with(|evs| {
            evs.iter()
                .rev()
                .find_map(|e| match &e.kind {
                    EventKind::Assistant { blocks, .. } => blocks.iter().rev().find_map(|b| match b {
                        ContentBlock::Text { text, .. } if !text.trim().is_empty() => Some(c6_api::citations::clean(text).into_owned()),
                        _ => None,
                    }),
                    // From another session's agent, not you.
                    EventKind::User { text, from_title, from_session_id, .. } if !from_session_id.is_empty() => {
                        Some(format!("From {from_title}: {text}"))
                    }
                    EventKind::User { text, .. } => Some(format!("› {text}")),
                    EventKind::Error { message } => Some(format!("Error: {message}")),
                    _ => None,
                })
                .unwrap_or_default()
        })
    });
    // A tool call still waiting for its result.
    let tool = Memo::new(move |_| {
        if !caught_up.get() {
            return None;
        }
        feed.events.with(|evs| {
            let done: std::collections::HashSet<&str> = evs
                .iter()
                .filter_map(|e| match &e.kind {
                    EventKind::ToolResult { call_id, .. } => Some(call_id.as_str()),
                    _ => None,
                })
                .collect();
            evs.iter().rev().find_map(|e| match &e.kind {
                EventKind::Assistant { blocks, .. } => blocks.iter().find_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input_json } if !done.contains(id.as_str()) => {
                        let arg = serde_json::from_str::<serde_json::Value>(input_json)
                            .ok()
                            .and_then(|v| {
                                ["command", "path", "pattern", "url", "description"]
                                    .iter()
                                    .find_map(|k| v.get(*k)?.as_str().map(str::to_owned))
                            })
                            .unwrap_or_default();
                        Some(format!("{name} {arg}"))
                    }
                    _ => None,
                }),
                _ => None,
            })
        })
    });
    let text = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let send_id = StoredValue::new(id.clone());
    let send = move || {
        let t = text.get_untracked();
        if t.trim().is_empty() {
            return;
        }
        text.set(String::new());
        // Replying is attending to it.
        ws.dismiss(&send_id.get_value());
        spawn_local(async move {
            if let Err(e) = api::send_message(send_id.get_value(), t.clone(), None, Vec::new()).await {
                error.set(Some(e));
                text.set(t);
            }
        });
    };
    let open_id = id.clone();
    view! {
        <div class="flex h-48 flex-col rounded-lg border sm:h-64 border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-900">
            <div class="group flex items-center gap-2 border-b border-zinc-100 px-3 py-2 dark:border-zinc-800">
                {move || session.get().map(|s| view! { <span class=dot(s.display_status(), tracked())></span> })}
                <Go to=View::session(open_id.clone()) class="min-w-0 flex-1 truncate text-sm font-medium hover:underline" title="Open (⌘-click: new tile)">
                    {move || session.get().map(|s| s.title).unwrap_or_else(|| "Session".into())}
                </Go>
                <span class="shrink-0 text-[11px] text-muted">
                    {move || session.get().map(|s| format!("{} · {}", live.session_worker(&s), ago(now_ms() - s.updated_at)))}
                </span>
                <crate::sessions::SessionMenu id=open_id.clone() />
            </div>
            <div class="relative min-h-0 flex-1 overflow-hidden px-3 py-2">
                {move || tool.get().map(|t| view! { <div class="mb-1 truncate font-mono text-[11px] text-blue-600 dark:text-blue-400">{crate::icons::play()}" "{t}</div> })}
                // Bottom-anchored, so the newest words show.
                <div class="absolute inset-x-3 bottom-2 top-7 flex flex-col justify-end overflow-hidden">
                    <p class="whitespace-pre-wrap break-words text-xs leading-relaxed text-zinc-600 dark:text-zinc-400">{move || {
                        let t = tail.get();
                        let n = t.chars().count();
                        if n > 700 { format!("…{}", t.chars().skip(n - 700).collect::<String>()) } else { t }
                    }}</p>
                </div>
            </div>
            {move || error.get().map(|e| view! { <div class="px-3"><ErrorBox message=e /></div> })}
            <div class="border-t border-zinc-100 p-2 dark:border-zinc-800">
                <input
                    class="w-full rounded-md border border-zinc-300 bg-white px-2 py-1 text-xs dark:border-zinc-700 dark:bg-zinc-950"
                    placeholder="Reply…"
                    prop:value=move || text.get()
                    on:input=move |ev| text.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Enter" { send() }
                />
            </div>
        </div>
    }
}

/// How long a press on a phone's tab lifts it (to drag, or for its menu).
const LONG_PRESS: std::time::Duration = std::time::Duration::from_millis(500);
/// How far a press may move and still be one (further, it's a swipe).
const PRESS_SLOP: f64 = 8.0;
/// How near the strip's ends a lifted tab scrolls it, and how fast.
const EDGE: f64 = 32.0;
const EDGE_STEP: i32 = 6;

/// Whether a press moved off where it began (then it's no long press, or a
/// lifted tab is being dragged).
fn moved_off(from: (f64, f64), to: (f64, f64)) -> bool {
    (to.0 - from.0).hypot(to.1 - from.1) > PRESS_SLOP
}

/// The gap a dragged tab would land in, with the pointer at `x` (`mids`:
/// the tabs' middles, in order): before the first tab whose middle is past
/// it (0: before them all), else after the last.
fn drop_slot(x: f64, mids: &[f64]) -> usize {
    mids.iter().position(|&m| x < m).unwrap_or(mids.len())
}

/// How many places the tab at `i` moves to land in gap `slot`: none for
/// the gaps either side of it.
fn slot_step(i: usize, slot: usize) -> isize {
    if slot > i { (slot - i - 1) as isize } else { slot as isize - i as isize }
}

/// How far to scroll the strip (`left` to `right`) with a lifted tab at
/// `x`: towards an end it is near.
fn edge_scroll(x: f64, left: f64, right: f64) -> i32 {
    if x < left + EDGE {
        -EDGE_STEP
    } else if x > right - EDGE {
        EDGE_STEP
    } else {
        0
    }
}

/// A press on a tab, from pointer down to up.
#[derive(Clone, Copy)]
struct Press {
    key: u64,
    pointer: i32,
    from: (f64, f64),
    /// Where the pointer is now.
    at: (f64, f64),
    /// Until it lifts the tab.
    timer: Option<TimeoutHandle>,
    /// Lifted (held long enough), and dragged since.
    lifted: bool,
    dragged: bool,
}

/// Phones: switch tiles, add one, and every tile in a sheet (▸, with the
/// sessions you viewed lately). The current tab's × closes
/// its tile. A long press lifts a tab: drag it and a line marks where it
/// will land (it stays put until let go), or let go where it is for its
/// menu.
#[component]
pub fn MobileBar() -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    // The tiles shown, in reading order, as ⌘1–9 and the dock number them.
    let tiles = Memo::new(move |_| {
        ws.layout.with(|l| {
            l.reading_order()
                .into_iter()
                .filter(|&i| !l.tiles[i].minimized)
                .map(|i| {
                    let t = &l.tiles[i];
                    (t.key, t.columns.get(t.cursor).map(|c| c.view.label(&live)).unwrap_or_default(), i == l.focus)
                })
                .collect::<Vec<_>>()
        })
    });
    // Only the tabs scroll, and their edges fade where more is hidden that
    // way, so it reads as a strip to swipe; the current tab is brought
    // into view whenever it changes.
    let strip = NodeRef::<leptos::html::Div>::new();
    let edges = RwSignal::new((false, false));
    let measure = move || {
        if let Some(el) = strip.try_get_untracked().flatten() {
            let (left, max) = (el.scroll_left(), el.scroll_width() - el.client_width());
            let _ = edges.try_set((left > 2, left < max - 2));
        }
    };
    Effect::new(move || {
        let focused = tiles.with(|t| t.iter().position(|(_, _, f)| *f));
        request_animation_frame(move || {
            if let (Some(el), Some(i)) = (strip.try_get_untracked().flatten(), focused)
                && let Some(tab) = el.children().item(i as u32)
            {
                let (box_, tab) = (el.get_bounding_client_rect(), tab.get_bounding_client_rect());
                if tab.left() < box_.left() {
                    el.set_scroll_left(el.scroll_left() - (box_.left() - tab.left()) as i32 - 16);
                } else if tab.right() > box_.right() {
                    el.set_scroll_left(el.scroll_left() + (tab.right() - box_.right()) as i32 + 16);
                }
            }
            measure();
        });
    });
    let fade = move || match edges.get() {
        (true, true) => "[mask-image:linear-gradient(to_right,transparent,black_24px,black_calc(100%-24px),transparent)]",
        (true, false) => "[mask-image:linear-gradient(to_right,transparent,black_24px)]",
        (false, true) => "[mask-image:linear-gradient(to_left,transparent,black_24px)]",
        (false, false) => "",
    };
    // The press under way, the tab it lifted (for its look, and so the
    // strip stops scrolling under the finger), and whether a press lifted
    // a tab (the click that ends it is no tap).
    let press = StoredValue::new(None::<Press>);
    let lifted = RwSignal::new(None::<u64>);
    // The gap a lifted tab would land in, when that would move it.
    let line = RwSignal::new(None::<usize>);
    let pressed = StoredValue::new(false);
    // While a tab is lifted: what keeps the strip scrolling near its ends.
    let scrolling = StoredValue::new(None::<IntervalHandle>);
    // The tab whose menu is open, and where (its left, its top).
    let menu = RwSignal::new(None::<(u64, f64, f64)>);
    let tab_of = move |key: u64| -> Option<web_sys::Element> {
        let el = strip.try_get_untracked().flatten()?;
        let i = tiles.try_with_untracked(|t| t.iter().position(|(k, ..)| *k == key)).flatten()?;
        el.children().item(i as u32)
    };
    // Ends a press (and a lift); what it was.
    let end = move || -> Option<Press> {
        let p = press.try_update_value(Option::take).flatten()?;
        if let Some(t) = p.timer {
            t.clear();
        }
        if p.lifted {
            if let Some(h) = scrolling.try_update_value(Option::take).flatten() {
                h.clear();
            }
            if let Some(el) = strip.try_get_untracked().flatten() {
                let _ = el.release_pointer_capture(p.pointer);
            }
            let _ = lifted.try_set(None);
            let _ = line.try_set(None);
        }
        Some(p)
    };
    // A lifted tab stays put while the line follows the pointer, to the gap
    // nearest it; near an end of the strip, the strip scrolls.
    let follow = move || {
        let (Some(p), Some(el)) = (press.try_get_value().flatten(), strip.try_get_untracked().flatten()) else { return };
        if !p.lifted {
            return;
        }
        let r = el.get_bounding_client_rect();
        let by = edge_scroll(p.at.0, r.left(), r.right());
        if by != 0 {
            el.set_scroll_left(el.scroll_left() + by);
        }
        let Some(i) = tiles.try_with_untracked(|t| t.iter().position(|(k, ..)| *k == p.key)).flatten() else { return };
        let tabs = el.children();
        let mids: Vec<f64> = (0..tabs.length())
            .filter_map(|j| tabs.item(j))
            .map(|t| {
                let r = t.get_bounding_client_rect();
                r.left() + r.width() / 2.0
            })
            .collect();
        let slot = drop_slot(p.at.0, &mids);
        let at = (p.dragged && slot_step(i, slot) != 0).then_some(slot);
        if line.get_untracked() != at {
            let _ = line.try_set(at);
        }
    };
    let down = move |ev: web_sys::PointerEvent| {
        end();
        pressed.set_value(false);
        if ev.button() != 0 {
            return;
        }
        // Which tab (the × keeps its press to itself).
        let key = ev
            .target()
            .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
            .and_then(|t| t.closest("[data-tile]").ok().flatten())
            .and_then(|t| t.get_attribute("data-tile"))
            .and_then(|k| k.parse::<u64>().ok());
        let Some(key) = key else { return };
        let (pointer, from) = (ev.pointer_id(), (f64::from(ev.client_x()), f64::from(ev.client_y())));
        let lift = move || {
            let Some(Some(p)) = press.try_update_value(|p| {
                p.as_mut().filter(|p| p.key == key && !p.lifted).map(|p| {
                    p.lifted = true;
                    p.timer = None;
                    *p
                })
            }) else {
                return;
            };
            let _ = pressed.try_set_value(true);
            let _ = lifted.try_set(Some(key));
            // Every move of this pointer comes here now, wherever it goes.
            if let Some(el) = strip.try_get_untracked().flatten() {
                let _ = el.set_pointer_capture(p.pointer);
            }
            let nav = window().navigator();
            if js_sys::Reflect::has(&nav, &"vibrate".into()).unwrap_or(false) {
                let _ = nav.vibrate_with_duration(10);
            }
            let h = set_interval_with_handle(follow, std::time::Duration::from_millis(30)).ok();
            let _ = scrolling.try_set_value(h);
        };
        let timer = set_timeout_with_handle(lift, LONG_PRESS).ok();
        press.set_value(Some(Press { key, pointer, from, at: from, timer, lifted: false, dragged: false }));
    };
    let moving = move |ev: web_sys::PointerEvent| {
        let at = (f64::from(ev.client_x()), f64::from(ev.client_y()));
        let Some(p) = press.get_value() else { return };
        if p.pointer != ev.pointer_id() {
            return;
        }
        if !p.lifted {
            // Moving before it lifts: a swipe of the strip.
            if moved_off(p.from, at) {
                end();
            }
            return;
        }
        press.update_value(|p| {
            if let Some(p) = p {
                p.at = at;
                p.dragged |= moved_off(p.from, at);
            }
        });
        follow();
    };
    // Let go: a tab lifted and not dragged shows its menu; a dragged one
    // moves to the line, if there is one.
    let up = move |ev: web_sys::PointerEvent| {
        if press.with_value(|p| p.is_none_or(|p| p.pointer != ev.pointer_id())) {
            return;
        }
        let slot = line.get_untracked();
        let Some(p) = end() else { return };
        if !p.lifted {
            return;
        }
        if p.dragged {
            let i = tiles.with_untracked(|t| t.iter().position(|(k, ..)| *k == p.key));
            if let (Some(i), Some(slot)) = (i, slot) {
                ws.step_tile(p.key, slot_step(i, slot));
            }
        } else if let Some(tab) = tab_of(p.key) {
            let r = tab.get_bounding_client_rect();
            menu.set(Some((p.key, r.left(), r.top())));
        }
    };
    // The sheet of tiles and recently viewed sessions.
    let sheet = RwSignal::new(false);
    let keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" && menu.get_untracked().is_some() {
            menu.set(None);
        } else if ev.key() == "Escape" && sheet.get_untracked() {
            sheet.set(false);
        }
    });
    on_cleanup(move || {
        keys.remove();
        end();
    });
    view! {
        <nav class="relative flex shrink-0 items-center gap-1 border-t border-zinc-200 bg-zinc-50 px-2 py-1 md:hidden dark:border-zinc-800 dark:bg-zinc-950">
            // Padded so the current tab's × (taller than the tabs, for a
            // finger) fits without the strip scrolling up and down. While a
            // tab is lifted, the finger drags it rather than the strip.
            <div node_ref=strip
                class=move || format!("flex min-w-0 flex-1 gap-1 overflow-x-auto py-0.5 [scrollbar-width:none] {} {}", fade(), if lifted.get().is_some() { "touch-none" } else { "" })
                on:scroll=move |_| measure()
                on:pointerdown=down
                on:pointermove=moving
                on:pointerup=up
                on:pointercancel=move |_| { end(); }
                on:pointerleave=move |_| if press.with_value(|p| p.is_some_and(|p| !p.lifted)) { end(); }
                on:touchmove=move |ev| if lifted.get_untracked().is_some() { ev.prevent_default() }
                on:contextmenu=move |ev| ev.prevent_default()>
                {move || {
                    let tabs = tiles.get();
                    let n = tabs.len();
                    tabs.into_iter().enumerate().map(|(j, (key, label, focused))| view! {
                    <div data-tile=key.to_string()
                        class=if focused { "relative flex max-w-44 shrink-0 select-none items-center rounded-md bg-zinc-200 [-webkit-touch-callout:none] dark:bg-zinc-800" } else { "relative flex max-w-36 shrink-0 select-none items-center rounded-md [-webkit-touch-callout:none]" }
                        class=("opacity-50", move || lifted.get() == Some(key))
                        class=("outline-dashed", move || lifted.get() == Some(key))
                        class=("outline-1", move || lifted.get() == Some(key))
                        class=("outline-sky-400", move || lifted.get() == Some(key))
                    >
                        // Where the lifted tab will land: the gap before this
                        // tab, or after it if it is the last.
                        {move || {
                            let side = match line.get() {
                                Some(s) if s == j && j == 0 => "left-0",
                                Some(s) if s == j => "-left-[3px]",
                                Some(s) if s == n && j + 1 == n => "right-0",
                                _ => return None,
                            };
                            Some(view! { <span class=format!("pointer-events-none absolute inset-y-1 w-0.5 rounded-full bg-sky-400 {side}")></span> })
                        }}
                        <button
                            class=if focused { "min-w-0 truncate py-1.5 pl-2.5 text-xs font-medium" } else { "min-w-0 truncate px-2.5 py-1.5 text-xs text-zinc-600 dark:text-zinc-400" }
                            on:click=move |_| if !pressed.try_update_value(std::mem::take).unwrap_or(false) { ws.show_tile(key) }
                        >{label}</button>
                        {focused.then(|| view! {
                            <button class="-my-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-md text-sm text-muted hover:text-zinc-900 dark:hover:text-zinc-100"
                                title="Close this tile" aria-label="Close this tile"
                                on:pointerdown=move |ev| ev.stop_propagation()
                                on:click=move |_| ws.close_tile(key)>"×"</button>
                        })}
                    </div>
                }).collect_view()
                }}
            </div>
            {move || lifted.get().is_some().then(|| view! {
                <div class="pointer-events-none absolute inset-x-0 bottom-full flex justify-center pb-1">
                    <span class="rounded-full bg-zinc-50/90 px-2 py-0.5 text-[11px] text-muted dark:bg-zinc-950/90">"Drag to move · let go to drop"</span>
                </div>
            })}
            <button class="shrink-0 rounded-md px-2.5 py-1.5 text-xs text-muted" title="New tile" on:click=move |_| ws.new_tile()>"+"</button>
            <button class="shrink-0 rounded-md px-2.5 py-1.5 text-xs tabular-nums text-zinc-600 dark:text-zinc-400"
                title="Your tiles, and the sessions you viewed lately" aria-haspopup="dialog" aria-expanded=move || sheet.get().to_string()
                on:click=move |_| sheet.set(true)>{crate::icons::chevron_right()}" "{move || sheet_label(tiles.with(Vec::len))}</button>
            <Show when=move || sheet.get()>
                <TabSheet close=Callback::new(move |()| sheet.set(false)) />
            </Show>
            {move || menu.get().map(|(key, left, top)| {
                let alone = tiles.with(Vec::len) <= 1;
                let close = Callback::new(move |()| menu.set(None));
                let pick = Callback::new(move |what: TabAction| {
                    menu.set(None);
                    match what {
                        TabAction::Close => ws.close_tile(key),
                        TabAction::Minimize => ws.minimize(key),
                    }
                });
                view! { <TabMenu at={(left, top)} alone pick close pressed /> }
            })}
        </nav>
    }
}

/// What a tab's menu does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabAction {
    Close,
    Minimize,
}

/// A tab's menu, just above it (`at`: the tab's left and top): close its
/// tile, or minimize it (not the last one shown). A tap outside or Esc
/// closes it.
#[component]
fn TabMenu(at: (f64, f64), alone: bool, pick: Callback<TabAction>, close: Callback<()>, pressed: StoredValue<bool>) -> impl IntoView {
    let size = |v: Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>| v.ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
    let (width, height) = (size(window().inner_width()), size(window().inner_height()));
    // On screen, however near the edge the tab is.
    let style = format!("left:{}px;bottom:{}px", at.0.min(width - 168.0).max(8.0), height - at.1 + 6.0);
    let item = move |what: TabAction, label: &'static str, off: bool| {
        view! {
            <button role="menuitem" class="block w-full rounded-md px-3 py-2 text-left hover:bg-zinc-100 disabled:text-zinc-400 disabled:hover:bg-transparent dark:hover:bg-zinc-800 dark:disabled:text-zinc-600"
                disabled=off on:click=move |_| pick.run(what)>{label}</button>
        }
    };
    view! {
        // The press that opened it ends over this: that's no tap outside.
        <div class="fixed inset-0 z-40" on:pointerdown=move |_| pressed.set_value(false)
            on:click=move |_| if !pressed.try_update_value(std::mem::take).unwrap_or(false) { close.run(()) }></div>
        <div role="menu" style=style class="fixed z-50 w-40 rounded-lg border border-zinc-200 bg-white p-1 text-sm shadow-lg dark:border-zinc-700 dark:bg-zinc-900">
            {item(TabAction::Close, "Close", false)}
            {item(TabAction::Minimize, "Minimize", alone)}
        </div>
    }
}

/// The phone bar's sheet button: how many tiles are shown, beside its icon.
fn sheet_label(open: usize) -> String {
    open.to_string()
}

/// How far down a sheet's handle is swiped to close it.
const SWIPE_CLOSE: f64 = 40.0;

/// Whether a press on a sheet's handle went far enough down to close it.
fn swiped_down(from: f64, to: f64) -> bool {
    to - from > SWIPE_CLOSE
}

/// A tile, as the phone's sheet lists it.
#[derive(Clone, Debug, PartialEq)]
struct SheetTile {
    key: u64,
    /// In reading order, as ⌘1–9 and the dock number it.
    number: usize,
    /// The session it shows, if it shows one.
    session: Option<String>,
    label: String,
    focused: bool,
    minimized: bool,
}

/// A phone's tiles, from the bottom (as what needs you is): those shown in
/// reading order (the current one marked), each to show or close; the
/// minimized; the sessions you viewed lately that no tile holds; then all
/// sessions, and a new tile. Swipe the handle down, tap outside or Esc to
/// close it.
#[component]
fn TabSheet(close: Callback<()>) -> impl IntoView {
    let (ws, live) = (use_workspace(), api::live());
    let now = RwSignal::new(now_ms());
    let tiles = Memo::new(move |_| {
        ws.layout.with(|l| {
            l.reading_order()
                .into_iter()
                .enumerate()
                .map(|(n, i)| {
                    let t = &l.tiles[i];
                    let view = t.columns.get(t.cursor).map(|c| &c.view);
                    SheetTile {
                        key: t.key,
                        number: n + 1,
                        session: view.and_then(|v| match v {
                            View::Session { id, .. } => Some(id.clone()),
                            _ => None,
                        }),
                        label: view.map(|v| v.label(&live)).unwrap_or_default(),
                        focused: i == l.focus,
                        minimized: t.minimized,
                    }
                })
                .collect::<Vec<_>>()
        })
    });
    let recent = crate::sessions::use_recent(6, move |id| ws.tile_number(id).is_some());
    let swipe = StoredValue::new(None::<f64>);
    let row = move |t: SheetTile| {
        let fill = t.session.as_deref().and_then(crate::marks::of_id).map(|m| crate::marks::fill(&m)).unwrap_or_default();
        let key = t.key;
        view! {
            <div class=if t.focused { "flex items-center gap-2 rounded-md bg-zinc-100 pl-2 dark:bg-zinc-800" } else { "flex items-center gap-2 rounded-md pl-2" }>
                <span class="w-4 shrink-0 text-right font-mono text-[11px] tabular-nums text-muted">{t.number}</span>
                <span class="h-2.5 w-2.5 shrink-0 rounded-full" style=fill></span>
                <button class="min-w-0 flex-1 truncate py-2 text-left" class:font-medium=t.focused
                    on:click=move |_| { ws.show_tile(key); close.run(()) }>{t.label}</button>
                <button class="flex h-9 w-9 shrink-0 items-center justify-center text-muted hover:text-zinc-900 dark:hover:text-zinc-100"
                    title="Close this tile" aria-label="Close this tile" on:click=move |_| ws.close_tile(key)>"×"</button>
            </div>
        }
    };
    let link = "block w-full rounded-md px-2 py-2 text-left text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-800";
    view! {
        <div class="fixed inset-0 z-40 bg-black/30" on:click=move |_| close.run(())></div>
        <div role="dialog" aria-label="Tiles"
            class="fixed inset-x-0 bottom-0 z-50 flex max-h-[80dvh] flex-col rounded-t-xl border border-zinc-200 bg-white text-sm shadow-lg dark:border-zinc-700 dark:bg-zinc-900">
            // Its handle: swiped down, the sheet goes.
            <div class="flex shrink-0 touch-none justify-center pb-1 pt-2"
                on:pointerdown=move |ev: web_sys::PointerEvent| {
                    swipe.set_value(Some(f64::from(ev.client_y())));
                    if let Some(el) = ev.current_target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok()) {
                        let _ = el.set_pointer_capture(ev.pointer_id());
                    }
                }
                on:pointermove=move |ev: web_sys::PointerEvent| {
                    if swipe.get_value().is_some_and(|from| swiped_down(from, f64::from(ev.client_y()))) {
                        swipe.set_value(None);
                        close.run(());
                    }
                }
                on:pointerup=move |_| swipe.set_value(None)
                on:pointercancel=move |_| swipe.set_value(None)>
                <span class="h-1 w-10 rounded-full bg-zinc-300 dark:bg-zinc-600"></span>
            </div>
            <div class="min-h-0 overflow-y-auto p-1 pb-4">
                {move || {
                    let (shown, hidden): (Vec<SheetTile>, Vec<SheetTile>) = tiles.get().into_iter().partition(|t| !t.minimized);
                    view! {
                        <crate::needs::Heading text=format!("Open tiles · {}", shown.len()) />
                        {shown.into_iter().map(row).collect_view()}
                        {(!hidden.is_empty()).then(|| view! {
                            <crate::needs::Heading text="Minimized".to_owned() />
                            {hidden.into_iter().map(row).collect_view()}
                        })}
                    }
                }}
                {move || {
                    let rows = recent.get();
                    (!rows.is_empty()).then(|| view! {
                        <crate::needs::Heading text="Recently viewed".to_owned() />
                        {rows.into_iter().map(|(s, at)| view! { <crate::sessions::RecentRow s at now close /> }).collect_view()}
                    })
                }}
                <div class="my-1 border-t border-zinc-200 dark:border-zinc-800"></div>
                <button class=link on:click=move |_| { ws.open(None, View::Sessions, How::OwnTile); close.run(()) }>"All sessions "{crate::icons::arrow_right()}</button>
                <button class=link on:click=move |_| { ws.new_tile(); close.run(()) }>"+ New tile"</button>
            </div>
        </div>
    }
}

/// Mail that needs a reply (when a memex with Google accounts is there).
#[component]
fn MailNeedsReply() -> impl IntoView {
    let threads = LocalResource::new(|| {
        api::rpc_as::<Vec<c6_api::MailThread>>("memex.mail.threads", serde_json::json!({"view": "needs_reply", "limit": 6}))
    });
    view! {
        {move || threads.get().and_then(Result::ok).filter(|t| !t.is_empty()).map(|t| view! {
            <section class="space-y-2">
                <h2 class="text-sm font-semibold text-muted">"Mail that needs a reply"</h2>
                <div class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                    {t.into_iter().map(|x| view! {
                        <crate::workspace::Go to=crate::workspace::View::Mail class="block px-3 py-2 text-sm hover:bg-zinc-50 dark:hover:bg-zinc-900">
                            <span class="font-medium">{x.people.join(", ")}</span>
                            <span class="ml-2 text-zinc-600 dark:text-zinc-400">{x.subject.clone()}</span>
                        </crate::workspace::Go>
                    }).collect_view()}
                </div>
            </section>
        })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_press_that_moves_is_a_swipe() {
        assert!(!moved_off((10.0, 10.0), (15.0, 15.0)), "a finger's wobble");
        assert!(moved_off((10.0, 10.0), (19.0, 10.0)));
        assert!(moved_off((10.0, 10.0), (10.0, 1.0)));
    }

    #[test]
    fn a_dragged_tab_lands_in_the_gap_nearest_the_finger() {
        let mids = [40.0, 90.0, 140.0];
        assert_eq!(drop_slot(10.0, &mids), 0, "before them all");
        assert_eq!(drop_slot(60.0, &mids), 1);
        assert_eq!(drop_slot(200.0, &mids), 3, "after them all");
        assert_eq!(slot_step(1, 1), 0, "the gap before it: where it is");
        assert_eq!(slot_step(1, 2), 0, "the gap after it: where it is");
        assert_eq!(slot_step(1, 0), -1);
        assert_eq!(slot_step(1, 3), 1);
        assert_eq!(slot_step(0, 3), 2);
        assert_eq!(slot_step(2, 0), -2);
    }

    #[test]
    fn a_lifted_tab_near_an_end_scrolls_the_strip() {
        assert_eq!(edge_scroll(10.0, 0.0, 300.0), -EDGE_STEP);
        assert_eq!(edge_scroll(150.0, 0.0, 300.0), 0);
        assert_eq!(edge_scroll(290.0, 0.0, 300.0), EDGE_STEP);
    }

    #[test]
    fn the_sheet_says_how_many_tiles_and_goes_when_swiped_down() {
        assert_eq!(sheet_label(3), "3");
        assert!(swiped_down(100.0, 150.0));
        assert!(!swiped_down(100.0, 120.0), "a nudge");
        assert!(!swiped_down(100.0, 20.0), "up");
    }
}

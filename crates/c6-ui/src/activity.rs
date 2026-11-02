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
pub const FOLD: &str = "w-3 shrink-0 text-left text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100";

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
    let _keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if (ev.meta_key() || ev.ctrl_key()) && !ev.alt_key() && ev.key() == "\\" {
            ev.prevent_default();
            toggle.run(());
        }
    });
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
            <div class="flex items-center justify-between pb-1 pl-1.5 pr-3 pt-3 text-[11px] font-semibold uppercase tracking-wide text-zinc-500">
                <span class="flex items-center gap-1.5">
                    <button class=fold title=move || if open.get() { "Fold" } else { "Unfold" } on:click=move |_| open.update(|o| *o = !*o)>
                        {move || if open.get() { "▾" } else { "▸" }}
                    </button>
                    "Activity"
                    {move || (!open.get() && !items.with(Vec::is_empty)).then(|| view! {
                        <span class="rounded-full bg-zinc-200 px-1.5 font-normal tabular-nums dark:bg-zinc-800">{items.with(Vec::len)}</span>
                    })}
                </span>
                <span class="flex items-center gap-2">
                    <Go to=View::Mission class="font-normal normal-case tracking-normal hover:text-zinc-900 dark:hover:text-zinc-100" title="Mission control">"⊞ all"</Go>
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
                    <div class="px-2 pb-0.5 pt-2 text-[10px] font-semibold uppercase tracking-wide text-zinc-400">"Minimized"</div>
                    {h.iter().map(|(key, n, views)| view! { <MinimizedItem key=*key n=*n views=views.clone() /> }).collect_view()}
                }))}
                {move || {
                    let items = items.get();
                    if items.is_empty() && hidden.with(Vec::is_empty) {
                        let msg = if filter.with(Vec::is_empty) {
                            "Running sessions, ones that finish while you're elsewhere, and ones you pin (☆) show here."
                        } else {
                            "Nothing with these labels is running, finished or pinned."
                        };
                        return view! { <p class="px-2 py-2 text-xs text-zinc-500">{msg}</p> }.into_any();
                    }
                    let group = |which: Tracked, title: &'static str| {
                        let rows: Vec<_> = items.iter().filter(|(_, t)| *t == which).cloned().collect();
                        (!rows.is_empty()).then(|| {
                            let clear = (which == Tracked::Attention).then(|| view! {
                                <button class="font-normal normal-case tracking-normal hover:text-zinc-900 dark:hover:text-zinc-100" title="Mark them all seen"
                                    on:click=move |_| ws.attention.set(Vec::new())>"clear"</button>
                            });
                            view! {
                                <div class="flex items-center justify-between px-2 pb-0.5 pt-2 text-[10px] font-semibold uppercase tracking-wide text-zinc-400">
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
                <div class="truncate text-[11px] text-zinc-500">{move || state().map_or("minimized", |(_, _, w)| w)}</div>
            </button>
            {tile_badge(n)}
            <button class="row-action flex h-8 w-6 shrink-0 items-center justify-center text-xs text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100"
                title="Close this tile" aria-label="Close" on:click=move |_| ws.close_tile(key)>"✕"</button>
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
                <div title=s.on_head.then_some(api::HEAD_TITLE) class="truncate text-[11px] text-zinc-500">{format!("{} · {}", what(s.display_status(), t), worker)}</div>
            </Go>
            {move || number.get().map(tile_badge)}
            {dismissable.then(|| {
                let id = id.clone();
                view! { <button class="row-action flex h-8 w-6 shrink-0 items-center justify-center text-xs text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100" title="Dismiss" aria-label="Dismiss" on:click=move |_| ws.dismiss(&id)>"✕"</button> }
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
    let section = move |title: &'static str, which: Tracked, empty: &'static str| {
        view! {
            <section class="space-y-2">
                <h2 class="text-sm font-semibold text-zinc-500">{title}</h2>
                {move || {
                    let rows: Vec<_> = items.get().into_iter().filter(|(_, t)| *t == which).collect();
                    if rows.is_empty() {
                        view! { <p class="text-sm text-zinc-500">{empty}</p> }.into_any()
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
                    <p class="-mt-4 text-xs text-zinc-500">{format!("{hidden} more without these labels.")}</p>
                })
            }}
            {move || items.with(|i| i.iter().any(|(_, t)| *t == Tracked::Approval)).then(|| section("Needs your approval", Tracked::Approval, ""))}
            {section("Finished, not opened yet", Tracked::Attention, "Nothing finished while you were elsewhere.")}
            {move || items.with(|i| i.iter().any(|(_, t)| *t == Tracked::Attention)).then(|| view! {
                <button class="-mt-4 text-xs text-zinc-500 underline" on:click=move |_| ws.attention.set(Vec::new())>"Mark all seen"</button>
            })}
            {section("Running", Tracked::Running, "No sessions are running.")}
            {section("Pinned", Tracked::Pinned, "Pin a session with ☆ in its header to keep it here.")}
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
                <div title=s.on_head.then_some(api::HEAD_TITLE) class="truncate text-xs text-zinc-500">{format!("{} · {} · {}", what(s.display_status(), t), worker, ago(now_ms() - s.updated_at))}</div>
                {(!s.labels.is_empty() || !crate::labels::shown_auto(&s.auto_labels).is_empty()).then(|| view! {
                    <div class="mt-1 flex flex-wrap gap-1">
                        {crate::labels::shown_auto(&s.auto_labels).into_iter().map(|l| view! { <crate::labels::LabelChip label=l auto=true stay=true /> }).collect_view()}
                        {s.labels.iter().cloned().map(|l| view! { <crate::labels::LabelChip label=l stay=true /> }).collect_view()}
                    </div>
                })}
            </Go>
            {(t == Tracked::Attention).then(|| view! {
                <button class="rounded-md px-2 py-1 text-xs text-zinc-500 hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=move |_| ws.dismiss(&dismiss_id)>"Dismiss"</button>
            })}
            {(t == Tracked::Pinned).then(|| view! {
                <button class="rounded-md px-2 py-1 text-xs text-zinc-500 hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=move |_| ws.toggle_pin(&pin_id)>"Unpin"</button>
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
            <p class="text-xs text-zinc-500">"Running, finished-unseen and pinned sessions (or your most recent), live. Reply from a card, or open it beside this view."</p>
            <crate::labels::LabelFilterBar />
            {move || ids.with(Vec::is_empty).then(|| view! { <p class="text-sm text-zinc-500">"No sessions with these labels."</p> })}
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
            return live_text;
        }
        feed.events.with(|evs| {
            evs.iter()
                .rev()
                .find_map(|e| match &e.kind {
                    EventKind::Assistant { blocks, .. } => blocks.iter().rev().find_map(|b| match b {
                        ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.clone()),
                        _ => None,
                    }),
                    // From another session's agent, not you.
                    EventKind::User { text, from_title, from_session_id, .. } if !from_session_id.is_empty() => {
                        Some(format!("✉ {from_title}: {text}"))
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
                <span class="shrink-0 text-[11px] text-zinc-500">
                    {move || session.get().map(|s| format!("{} · {}", live.session_worker(&s), ago(now_ms() - s.updated_at)))}
                </span>
                <crate::sessions::SessionMenu id=open_id.clone() />
            </div>
            <div class="relative min-h-0 flex-1 overflow-hidden px-3 py-2">
                {move || tool.get().map(|t| view! { <div class="mb-1 truncate font-mono text-[11px] text-blue-600 dark:text-blue-400">{format!("▶ {t}")}</div> })}
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

/// Phones: switch tiles, add one, and the inbox.
#[component]
pub fn MobileBar() -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    // In reading order, as ⌘1–9 and the dock number them.
    let tiles = Memo::new(move |_| {
        ws.layout.with(|l| {
            l.reading_order()
                .into_iter()
                .map(|i| {
                    let t = &l.tiles[i];
                    (t.key, t.columns.get(t.cursor).map(|c| c.view.label(&live)).unwrap_or_default(), i == l.focus)
                })
                .collect::<Vec<_>>()
        })
    });
    let filter = crate::labels::use_filter().0;
    let counts = Memo::new(move |_| crate::labels::attention_counts(ws, live, &filter.get()));
    let count = move || counts.get().0;
    view! {
        <nav class="flex shrink-0 items-center gap-1 overflow-x-auto [scrollbar-width:none] border-t border-zinc-200 bg-zinc-50 px-2 py-1.5 md:hidden dark:border-zinc-800 dark:bg-zinc-950">
            {move || tiles.get().into_iter().map(|(key, label, focused)| view! {
                <button
                    class=if focused { "max-w-36 shrink-0 truncate rounded-md bg-zinc-200 px-2.5 py-1.5 text-xs font-medium dark:bg-zinc-800" } else { "max-w-36 shrink-0 truncate rounded-md px-2.5 py-1.5 text-xs text-zinc-600 dark:text-zinc-400" }
                    on:click=move |_| ws.show_tile(key)
                >{label}</button>
            }).collect_view()}
            <button class="shrink-0 rounded-md px-2.5 py-1.5 text-xs text-zinc-500" title="New tile" on:click=move |_| ws.new_tile()>"+"</button>
            <span class="flex-1"></span>
            <button class="relative shrink-0 rounded-md px-2.5 py-1.5 text-xs text-zinc-600 dark:text-zinc-400"
                title=move || { let (s, h) = counts.get(); crate::labels::attention_title(s, h) }
                on:click=move |_| ws.open(None, View::Inbox, How::Beside)>
                "Inbox"
                {move || (count() > 0).then(|| view! { <span class="ml-1 rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white">{count()}</span> })}
            </button>
        </nav>
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
                <h2 class="text-sm font-semibold text-zinc-500">"Mail that needs a reply"</h2>
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

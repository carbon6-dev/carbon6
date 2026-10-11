//! Keeping track of many sessions: the dock (desktop), the inbox, mission
//! control, and the phone's bottom bar.

use c6_api::{ContentBlock, EventKind, Session, SessionStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{ErrorBox, ago, now_ms};
use crate::workspace::{Go, How, MAX_TILES, Tracked, View, use_workspace};

fn dot(status: SessionStatus, tracked: Tracked) -> &'static str {
    match (status, tracked) {
        (_, Tracked::Approval) => "h-2 w-2 shrink-0 animate-pulse rounded-full bg-amber-500",
        (SessionStatus::Running, _) => "h-2 w-2 shrink-0 animate-pulse rounded-full bg-blue-500",
        (SessionStatus::Failed, _) => "h-2 w-2 shrink-0 rounded-full bg-red-500",
        (_, Tracked::Attention) => "h-2 w-2 shrink-0 rounded-full bg-amber-500",
        _ => "h-2 w-2 shrink-0 rounded-full bg-zinc-400",
    }
}

fn what(status: SessionStatus, tracked: Tracked) -> &'static str {
    match (status, tracked) {
        (_, Tracked::Approval) => "needs your approval",
        (SessionStatus::Running, _) => "running",
        (SessionStatus::Failed, _) => "failed",
        (_, Tracked::Attention) => "finished",
        (SessionStatus::Starting, _) => "starting",
        _ => "idle",
    }
}

/// Running, finished-unseen and pinned sessions, always in view (desktop),
/// narrowed by the shared label filter (which it shows, with what it hides).
#[component]
pub fn Dock() -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let filter = crate::labels::use_filter().0;
    let all = Memo::new(move |_| ws.tracked(&live));
    let items = Memo::new(move |_| {
        let f = filter.get();
        all.get().into_iter().filter(|(s, _)| crate::labels::matches(s, &f)).collect::<Vec<_>>()
    });
    view! {
        <aside class="hidden w-56 shrink-0 flex-col border-r border-zinc-200 bg-zinc-50 lg:flex dark:border-zinc-800 dark:bg-zinc-950">
            <div class="flex items-center justify-between px-3 pb-1 pt-3 text-[11px] font-semibold uppercase tracking-wide text-zinc-500">
                "Activity"
                <Go to=View::Mission class="font-normal normal-case tracking-normal hover:text-zinc-900 dark:hover:text-zinc-100" title="Mission control">"⊞ all"</Go>
            </div>
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
                {move || {
                    let items = items.get();
                    if items.is_empty() {
                        let msg = if filter.with(Vec::is_empty) {
                            "Running sessions, ones that finish while you're elsewhere, and ones you pin (☆) show here."
                        } else {
                            "Nothing with these labels is running, finished or pinned."
                        };
                        return view! { <p class="px-2 py-2 text-xs text-zinc-500">{msg}</p> }.into_any();
                    }
                    items.into_iter().map(|(s, t)| view! { <DockItem s t /> }).collect_view().into_any()
                }}
            </div>
        </aside>
    }
}

#[component]
fn DockItem(s: Session, t: Tracked) -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let worker = live.worker_name(&s.worker_id.to_string());
    let id = s.id.clone();
    let dismissable = t == Tracked::Attention;
    view! {
        <div class="group flex items-center gap-2 rounded-md px-2 py-1.5 hover:bg-zinc-200/60 dark:hover:bg-zinc-800/60">
            <span class=dot(s.status, t)></span>
            <Go to=View::session(s.id.clone()) class="min-w-0 flex-1" title="Open (⌘-click: new tile)">
                <div class="truncate text-sm">{s.title.clone()}</div>
                <div class="truncate text-[11px] text-zinc-500">{format!("{} · {}", what(s.status, t), worker)}</div>
            </Go>
            {dismissable.then(|| {
                let id = id.clone();
                view! { <button class="hidden text-xs text-zinc-400 hover:text-zinc-900 group-hover:inline dark:hover:text-zinc-100" title="Dismiss" on:click=move |_| ws.dismiss(&id)>"✕"</button> }
            })}
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
            {section("Needs attention", Tracked::Attention, "Nothing finished while you were elsewhere.")}
            {section("Running", Tracked::Running, "No sessions are running.")}
            {section("Pinned", Tracked::Pinned, "Pin a session with ☆ in its header to keep it here.")}
        </main>
    }
}

#[component]
fn InboxRow(s: Session, t: Tracked) -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let worker = live.worker_name(&s.worker_id.to_string());
    let (dismiss_id, pin_id) = (s.id.clone(), s.id.clone());
    view! {
        <div class="flex items-center gap-3 px-4 py-3">
            <span class=dot(s.status, t)></span>
            <Go to=View::session(s.id.clone()) class="min-w-0 flex-1">
                <div class="truncate font-medium">{s.title.clone()}</div>
                <div class="truncate text-xs text-zinc-500">{format!("{} · {} · {}", what(s.status, t), worker, ago(now_ms() - s.updated_at))}</div>
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
    let feed = crate::session::Feed::open(id.clone());
    let sid = id.clone();
    let session = Memo::new(move |_| live.sessions.with(|ss| ss.iter().find(|s| s.id == sid).cloned()));
    let tracked = {
        let id = id.clone();
        move || {
            if ws.attention.with(|a| a.contains(&id)) { Tracked::Attention } else { Tracked::Pinned }
        }
    };
    // The latest words: streaming text, else the last reply or message.
    let tail = Memo::new(move |_| {
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
                    EventKind::User { text, .. } => Some(format!("› {text}")),
                    EventKind::Error { message } => Some(format!("Error: {message}")),
                    _ => None,
                })
                .unwrap_or_default()
        })
    });
    // A tool call still waiting for its result.
    let tool = Memo::new(move |_| {
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
            <div class="flex items-center gap-2 border-b border-zinc-100 px-3 py-2 dark:border-zinc-800">
                {move || session.get().map(|s| view! { <span class=dot(s.status, tracked())></span> })}
                <Go to=View::session(open_id.clone()) class="min-w-0 flex-1 truncate text-sm font-medium hover:underline" title="Open (⌘-click: new tile)">
                    {move || session.get().map(|s| s.title).unwrap_or_else(|| "Session".into())}
                </Go>
                <span class="shrink-0 text-[11px] text-zinc-500">
                    {move || session.get().map(|s| format!("{} · {}", live.worker_name(&s.worker_id.to_string()), ago(now_ms() - s.updated_at)))}
                </span>
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
    let tiles = Memo::new(move |_| {
        ws.layout.with(|l| {
            l.tiles
                .iter()
                .enumerate()
                .map(|(i, t)| (i, t.columns.get(t.cursor).map(|c| c.view.label(&live)).unwrap_or_default(), i == l.focus))
                .collect::<Vec<_>>()
        })
    });
    let filter = crate::labels::use_filter().0;
    let counts = Memo::new(move |_| crate::labels::attention_counts(ws, live, &filter.get()));
    let count = move || counts.get().0;
    view! {
        <nav class="flex shrink-0 items-center gap-1 overflow-x-auto border-t border-zinc-200 bg-zinc-50 px-2 py-1.5 md:hidden dark:border-zinc-800 dark:bg-zinc-950">
            {move || tiles.get().into_iter().map(|(i, label, focused)| view! {
                <button
                    class=if focused { "max-w-36 shrink-0 truncate rounded-md bg-zinc-200 px-2.5 py-1.5 text-xs font-medium dark:bg-zinc-800" } else { "max-w-36 shrink-0 truncate rounded-md px-2.5 py-1.5 text-xs text-zinc-600 dark:text-zinc-400" }
                    on:click=move |_| ws.focus_tile(i)
                >{label}</button>
            }).collect_view()}
            {move || (ws.layout.with(|l| l.tiles.len()) < MAX_TILES).then(|| view! {
                <button class="shrink-0 rounded-md px-2.5 py-1.5 text-xs text-zinc-500" title="New tile" on:click=move |_| ws.new_tile()>"+"</button>
            })}
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

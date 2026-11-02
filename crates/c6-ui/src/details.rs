//! A session's details drawer, under its bar: where it runs, its labels
//! and work, how full its context is and what it has used, its tasks and
//! subagents, and what it published.

use c6_api::{Event, EventKind, Session};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::session::{SubagentList, SubagentTree, UsageStats, tokens};
use crate::util::BUTTON_QUIET;

const OPEN_KEY: &str = "c6.session.details";

/// Whether the drawer is open, remembered in this browser.
pub fn remembered() -> RwSignal<bool> {
    let open = RwSignal::new(crate::util::storage_get(OPEN_KEY).as_deref() == Some("1"));
    Effect::new(move || crate::util::storage_set(OPEN_KEY, if open.get() { "1" } else { "0" }));
    open
}

/// The git state of a session's folder, (files changed, current branch):
/// looked at when `at` (worker, folder) is known, and again after each
/// turn. `None`: not a repository, or it couldn't be asked.
pub fn git_state(at: Memo<Option<(String, String)>>, events: RwSignal<Vec<Event>>) -> LocalResource<Option<(usize, String)>> {
    // The last turn's end: a new one may have changed the folder.
    let ended =
        Memo::new(move |_| events.with(|evs| evs.iter().rev().find(|e| matches!(e.kind, EventKind::TurnEnded { .. })).map(|e| e.seq)));
    LocalResource::new(move || {
        let (at, _) = (at.get(), ended.get());
        async move {
            let (w, cwd) = at?;
            let status = api::git_status(w, cwd).await.ok()?;
            (status["repo"] != false)
                .then(|| (status["files"].as_array().map_or(0, Vec::len), status["branch"].as_str().unwrap_or_default().to_owned()))
        }
    })
}

fn row(icon: &'static str, main: impl IntoView, side: impl IntoView) -> impl IntoView {
    view! {
        <div class="flex items-start gap-2 py-0.5">
            <span class="w-4 shrink-0 text-center text-muted">{icon}</span>
            <span class="min-w-0 flex-1">{main}</span>
            <span class="shrink-0 text-xs text-muted">{side}</span>
        </div>
    }
}

fn section(title: &'static str, body: impl IntoView) -> impl IntoView {
    view! {
        <section class="min-w-0 space-y-1">
            <h3 class="text-[11px] font-semibold uppercase tracking-wide text-muted">{title}</h3>
            {body}
        </section>
    }
}

/// Where it runs: its worker (and SSH target), folder, branch, model and
/// runtime.
#[component]
pub fn Environment(
    session: Session,
    worker_name: String,
    online: Signal<bool>,
    started: Memo<Option<(String, String)>>,
    model_label: Memo<String>,
    /// The connection it uses now.
    connection: Signal<String>,
    git: LocalResource<Option<(usize, String)>>,
) -> impl IntoView {
    let s = StoredValue::new(session);
    let connections = LocalResource::new(api::connections);
    let branch = move || {
        let worktree = started.get().map(|(_, b)| b).filter(|b| !b.is_empty());
        let current = git.get().flatten().map(|g| g.1).filter(|b| !b.is_empty());
        (current.or(worktree.clone()), worktree.is_some())
    };
    // Claude Code or carbon6's own agent, and on which connection.
    let runtime = move || {
        let id = connection.get();
        let c = connections.get().and_then(Result::ok)?.into_iter().find(|c| c.id == id)?;
        let what = if c.kind == "claude_code" { "Claude Code" } else { "carbon6's agent" };
        Some(format!("{what} · {}", c.name))
    };
    view! {
        {row(
            "▣",
            if s.with_value(|s| s.on_head) {
                view! { <span>{crate::api::HEAD_TITLE}</span> }.into_any()
            } else {
                view! {
                    <span class="inline-flex items-center gap-1.5">
                        <span class=move || if online.get() { "h-1.5 w-1.5 rounded-full bg-emerald-500" } else { "h-1.5 w-1.5 rounded-full bg-zinc-400" }></span>
                        {worker_name}
                    </span>
                }.into_any()
            },
            (),
        )}
        {s.with_value(|s| s.ssh.clone()).map(|t| row(
            "⇄",
            view! { <span class="break-all font-mono text-xs text-sky-800 dark:text-sky-300" title="Working on this machine over SSH, through the worker">{t}</span> },
            (),
        ))}
        {row(
            "▤",
            view! {
                <span class="break-all font-mono text-xs">
                    {move || started.get().map(|(c, _)| c).or_else(|| s.with_value(|s| Some(s.cwd.clone()))).filter(|f| !f.is_empty()).unwrap_or_else(|| "scratch workspace".into())}
                </span>
            },
            (),
        )}
        {move || {
            let (b, worktree) = branch();
            b.map(|b| row("⑂", view! { <span class="break-all font-mono text-xs">{b}</span> }, worktree.then_some("worktree")))
        }}
        {row(
            "◆",
            view! { <span>{move || { let m = model_label.get(); if m.is_empty() { s.with_value(|s| s.model.clone()) } else { m } }}</span> },
            s.with_value(|s| s.realm.clone()),
        )}
        {move || runtime().map(|r| row("◇", view! { <span class="text-xs text-zinc-600 dark:text-zinc-400">{r}</span> }, ()))}
    }
}

/// How full its context is, what fills it, and Compact now.
#[component]
pub fn Context(session: String, events: RwSignal<Vec<Event>>, usage: Memo<UsageStats>, running: Signal<bool>) -> impl IntoView {
    let id = StoredValue::new(session);
    let system_chars = Memo::new(move |_| {
        events.with(|evs| match evs.first().map(|e| &e.kind) {
            Some(EventKind::Started { system_prompt, .. }) => system_prompt.len() as u64,
            _ => 0,
        })
    });
    let compact = move |_| {
        spawn_local(async move {
            let _ = api::send_message(id.get_value(), "/compact".into(), None, Vec::new()).await;
        });
    };
    move || {
        usage.with(|u| {
            let window = u.context_window;
            let Some(used) = u.context else {
                return view! { <p class="py-1 text-xs text-muted">"Known after the first model call."</p> }.into_any();
            };
            let system = (system_chars.get() / 4).min(used);
            let rest = used - system;
            let pct = |n: u64| if window == 0 { 0.0 } else { n as f64 * 100.0 / window as f64 };
            let free = window.saturating_sub(used);
            let bar = |n: u64, color: &'static str| view! { <span class=format!("block h-full {color}") style=format!("width:{:.2}%", pct(n).min(100.0))></span> };
            let line = |color: &'static str, label: &'static str, n: u64| {
                view! {
                    <div class="flex items-center gap-2 text-xs">
                        <span class=format!("size-2.5 shrink-0 rounded-sm {color}")></span>
                        <span class="min-w-0 flex-1 truncate text-zinc-600 dark:text-zinc-400">{label}</span>
                        <span class="tabular-nums">{tokens(n)}</span>
                        <span class="w-12 text-right tabular-nums text-muted">{if window == 0 { "–".into() } else { format!("{:.1}%", pct(n)) }}</span>
                    </div>
                }
            };
            view! {
                <div class="flex items-baseline justify-between gap-2 py-0.5 text-sm">
                    <span class="font-medium">{if window == 0 { tokens(used) } else { format!("{:.0}% used", pct(used)) }}</span>
                    <span class="text-xs tabular-nums text-muted">{if window == 0 { String::new() } else { format!("{} / {}", tokens(used), tokens(window)) }}</span>
                </div>
                <div class="relative flex h-2 gap-px overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-800">
                    {bar(rest, "bg-blue-500")}
                    {bar(system, "bg-amber-500")}
                    // Compaction starts here.
                    <span class="absolute inset-y-0 w-px bg-zinc-500" style="left:85%" title="Compacts at 85%"></span>
                </div>
                <div class="space-y-1 pt-1">
                    {line("bg-blue-500", "Conversation and tools", rest)}
                    {line("bg-amber-500", "System prompt (estimated)", system)}
                    {(window > 0).then(|| line("bg-zinc-200 dark:bg-zinc-700", "Free", free))}
                </div>
                <div class="flex items-center justify-between gap-2 pt-0.5 text-xs text-muted">
                    <span>"Compacts at 85%"</span>
                    <button class=format!("{BUTTON_QUIET} px-2 py-0.5 text-xs") disabled=running title="Summarize the conversation so far to free up context (/compact)" on:click=compact>"Compact now"</button>
                </div>
            }
            .into_any()
        })
    }
}

/// What the session has used, in all.
#[component]
pub fn Usage(usage: Memo<UsageStats>) -> impl IntoView {
    move || {
        usage.with(|u| {
            let turns = u.turns.len();
            view! {
                <p class="pt-1 text-xs text-zinc-600 dark:text-zinc-400">
                    {format!("{turns} turn{} · {} model call{}", if turns == 1 { "" } else { "s" }, u.total.calls, if u.total.calls == 1 { "" } else { "s" })}
                </p>
                <p class="font-mono text-[11px] text-muted">{u.total.summary()}</p>
            }
        })
    }
}

/// The drawer itself: five sections side by side when the pane is wide,
/// wrapping when it's narrow, scrolling within itself.
#[component]
pub fn DetailsDrawer(
    session: Session,
    worker_name: String,
    online: Signal<bool>,
    events: RwSignal<Vec<Event>>,
    usage: Memo<UsageStats>,
    started: Memo<Option<(String, String)>>,
    model_label: Memo<String>,
    connection: Signal<String>,
    git: LocalResource<Option<(usize, String)>>,
    running: Signal<bool>,
    subagents: SubagentTree,
    tab: RwSignal<&'static str>,
    /// The output the Outputs tab shows.
    output: RwSignal<Option<String>>,
    open: RwSignal<bool>,
) -> impl IntoView {
    let live = api::live();
    let id = session.id.clone();
    // The work it serves (docs/work.md §7): not a subagent's, an observed
    // session's or an instance's.
    let work = (session.parent_id.is_none() && !session.observed && session.instance.is_empty()).then(|| {
        let (sid, fallback) = (session.id.clone(), session.title.clone());
        let title = Signal::derive(move || {
            live.sessions.with(|ss| ss.iter().find(|x| x.id == sid).map(|x| x.title.clone())).unwrap_or_else(|| fallback.clone())
        });
        view! { <crate::work::SessionWork session_id=session.id.clone() title /> }
    });
    let labels = view! { <crate::labels::SessionLabels session=session.id.clone() labels=session.labels.clone() auto_labels=session.auto_labels.clone() /> };
    // An output picked (or all of them) opens in the Outputs tab.
    let show_output = move |name: Option<String>| {
        if name.is_some() {
            output.set(name);
        }
        tab.set("outputs");
    };
    let idle = Memo::new(move |_| events.with(|e| crate::tasks::count(e)) == 0 && subagents.is_empty());
    view! {
        <div class="relative shrink-0 border-b border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-950">
            <button class="absolute right-2 top-1.5 z-10 rounded px-1 text-xs text-muted hover:bg-zinc-100 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                title="Close (Esc)" on:click=move |_| open.set(false)>"✕"</button>
            <div class="max-h-[40vh] overflow-y-auto px-3 pb-3 pt-2 text-sm">
                <div class="grid grid-cols-1 gap-x-6 gap-y-4 @lg:grid-cols-2 @3xl:grid-cols-3 @5xl:grid-cols-5">
                    {section("Where it runs", view! { <Environment session worker_name online started model_label connection git /> })}
                    {section("Labels and work", view! { {labels}<div class="pt-1">{work}</div> })}
                    {section("Context and usage", view! { <Context session=id.clone() events usage running /><Usage usage /> })}
                    {section("Tasks", view! {
                        <crate::tasks::TaskHistory session=id.clone() events />
                        <SubagentList tree=subagents />
                        <Show when=move || idle.get()>
                            <p class="py-1 text-xs text-muted">"No background tasks or subagents."</p>
                        </Show>
                    })}
                    {section("Outputs", view! {
                        <crate::outputs::Recent events on_pick=show_output />
                    })}
                </div>
            </div>
        </div>
    }
}

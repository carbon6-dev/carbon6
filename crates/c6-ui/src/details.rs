//! A session's glance and its details. The glance: where it runs, its
//! worktree, branch and changes, its work and labels, how full its context
//! is, what waits on you and its newest outputs, a line each, and the
//! outline of its turns by hour (docs/catchup.md §3); a column
//! beside the transcript when the session's column is wide, else opened
//! from the context ring in its tile's bar. Its details (⋯ → Session
//! details…): what it has used, and what applies to it.

use c6_api::{Event, EventKind};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::panes::Pane;
use crate::session::{SessionChrome, UsageStats, tokens};
use crate::util::BUTTON_QUIET;
use crate::workspace::{Popover, View, below};

/// How wide the glance is, as a column or a popover.
pub const GLANCE_WIDTH: f64 = 288.0;

const LINE: &str = "flex w-full min-w-0 items-baseline gap-2 rounded px-1.5 py-1.5 text-left text-xs leading-snug";
const ACTS: &str = "hover:bg-zinc-100 dark:hover:bg-zinc-800";
const LABEL: &str = "w-[4.5rem] shrink-0 text-[10px] font-medium uppercase tracking-wide text-muted";
const HEADING: &str = "px-1.5 pt-2 text-[10px] font-medium uppercase tracking-wide text-muted";
const MUTED: &str = "text-muted";

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

/// A line of the glance: a small label, what it says (wrapping rather than
/// cut short), and what clicking it does.
fn line(label: &'static str, body: impl IntoView + 'static, act: Option<Callback<web_sys::MouseEvent>>) -> AnyView {
    let label = view! { <span class=LABEL>{label}</span> };
    let body = view! { <span class="min-w-0 flex-1 [overflow-wrap:anywhere]">{body}</span> };
    match act {
        Some(f) => view! { <button class=format!("{LINE} {ACTS}") on:click=move |ev| f.run(ev)>{label}{body}</button> }.into_any(),
        None => view! { <div class=LINE>{label}{body}</div> }.into_any(),
    }
}

/// A folder by its last two parts.
fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').filter(|p| !p.is_empty()).take(3).collect();
    match parts.as_slice() {
        [a, b, _] => format!("…/{b}/{a}"),
        _ => path.to_owned(),
    }
}

/// The glance: read-only, a line each; a line acts where you would
/// (Changes opens the Changes pane, Work and Labels their editors, Context
/// its breakdown…). `on_done`: after something it opened elsewhere (the
/// popover closes).
#[component]
pub fn Glance(chrome: SessionChrome, #[prop(optional, into)] on_done: Option<Callback<()>>) -> impl IntoView {
    let live = api::live();
    let SessionChrome { id, detail, events, usage, started, model_label, connection, git, running, col, .. } = chrome;
    // Its row in the live list (labels and work as they change), else as
    // loaded.
    let session = Memo::new(move |_| {
        let id = id.get_value();
        live.sessions.with(|ss| ss.iter().find(|s| s.id == id).cloned()).or_else(|| detail.get().and_then(Result::ok).map(|d| d.session))
    });
    let done = move || {
        if let Some(f) = on_done {
            f.run(());
        }
    };
    // A view opened from here, the transcript staying (⌘/Ctrl-click: a
    // new tile).
    let open = move |view: View, ev: &web_sys::MouseEvent| {
        if let Some(c) = col {
            crate::panes::open(c, view, ev.meta_key() || ev.ctrl_key());
        }
        done();
    };
    let pane = move |p: Pane, ev: &web_sys::MouseEvent| {
        if let Some(c) = col {
            crate::panes::show(c, p, &id.get_value(), ev.meta_key() || ev.ctrl_key());
        }
        done();
    };

    // Runs on: its worker (online or not), model and effort, runtime.
    let connections = LocalResource::new(api::connections);
    let runtime = move || {
        let id = connection.get();
        let c = connections.get().and_then(Result::ok)?.into_iter().find(|c| c.id == id)?;
        Some(if c.kind == "claude_code" { "Claude Code" } else { "carbon6's agent" })
    };
    let runs_on = move || {
        let s = session.get()?;
        let online = live.workers.with(|ws| !crate::session::worker_offline(&s, ws));
        let model = Some(model_label.get()).filter(|m| !m.is_empty()).unwrap_or_else(|| s.model.clone());
        let rest = [Some(model), runtime().map(str::to_owned)].into_iter().flatten().collect::<Vec<_>>().join(" · ");
        // The head runs it itself: no worker.
        let body = if s.on_head {
            view! { <span title=crate::api::HEAD_TITLE>{format!("carbon6's head · {rest}")}</span> }.into_any()
        } else {
            let worker =
                detail.get().and_then(Result::ok).map(|d| d.worker_name).unwrap_or_else(|| live.worker_name(&s.worker_id.to_string()));
            let worker = match &s.ssh {
                Some(t) => format!("{worker} ⇄ {t}"),
                None => worker,
            };
            let title = format!("{worker}{} · {rest}", if online { "" } else { " (offline)" });
            view! {
                <span class=format!("mr-1 inline-block size-1.5 rounded-full align-middle {}", if online { "bg-emerald-500" } else { "bg-zinc-400" })></span>
                <span title=title>{format!("{worker} · {rest}")}</span>
            }
            .into_any()
        };
        let wid = s.worker_id.to_string();
        let act = (!s.on_head).then(|| Callback::new(move |ev: web_sys::MouseEvent| open(View::Worker { id: wid.clone() }, &ev)));
        Some(line("Runs on", body, act))
    };

    // Its worktree and the repository it's of, or its folder.
    let place = move || {
        let s = session.get()?;
        let st = started.get();
        let cwd = st.as_ref().map_or_else(|| s.cwd.clone(), |st| st.cwd.clone());
        let worktree = st.as_ref().is_some_and(|st| !st.branch.is_empty());
        let repo = st.as_ref().map(|st| if st.repo.is_empty() { s.cwd.clone() } else { st.repo.clone() }).unwrap_or_default();
        let repo_name = repo.trim_end_matches('/').rsplit('/').next().unwrap_or_default().to_owned();
        // Not over SSH: the worker can't look at that machine's files.
        let act = (s.ssh.is_none() && st.is_some()).then(|| Callback::new(move |ev: web_sys::MouseEvent| pane(Pane::Files, &ev)));
        let body = if cwd.is_empty() {
            view! { <span class=MUTED>"scratch workspace"</span> }.into_any()
        } else {
            view! {
                <span class="font-mono" title=cwd.clone()>{short_path(&cwd)}</span>
                {worktree.then(|| view! { <span class=MUTED title=repo>{format!(" of {repo_name}")}</span> })}
            }
            .into_any()
        };
        Some(line(if worktree { "Worktree" } else { "Folder" }, body, act))
    };

    let branch = move || {
        let current = git.get().flatten().map(|g| g.1).filter(|b| !b.is_empty());
        let b = current.or_else(|| started.get().map(|s| s.branch).filter(|b| !b.is_empty()))?;
        Some(line("Branch", view! { <span class="font-mono" title=b.clone()>{b.clone()}</span> }, None))
    };

    let changes = move || {
        let (n, _) = git.get().flatten()?;
        let body = match n {
            0 => "none".to_owned(),
            1 => "1 file".to_owned(),
            n => format!("{n} files"),
        };
        Some(line("Changes", body, Some(Callback::new(move |ev: web_sys::MouseEvent| pane(Pane::Changes, &ev)))))
    };

    // The work it serves (docs/work.md §7): not a subagent's, an observed
    // session's or an instance's.
    let work_at = RwSignal::new(None::<(f64, f64)>);
    let serves = move || session.get().is_some_and(|s| s.parent_id.is_none() && !s.observed && s.instance.is_empty());
    let tag = Memo::new(move |_| session.get().and_then(|s| s.work).map(|w| w.id));
    let item = LocalResource::new(move || {
        tag.track();
        api::work_of(format!("session:{}", id.get_value()))
    });
    let work = move || {
        serves().then(|| {
            let body = move || match item.get().and_then(Result::ok).and_then(|w| w.into_iter().next()) {
                Some(w) => view! {
                    <span title=w.why.clone()>{format!("◆ {}", w.fields.title)}</span>
                    <span class=MUTED>{format!(" · {}", c6_api::work::state_label(&w.state))}</span>
                }
                .into_any(),
                None => view! { <span class=MUTED>"none"</span> }.into_any(),
            };
            line("Work", body, Some(Callback::new(move |ev: web_sys::MouseEvent| work_at.set(Some(below(&ev, 384.0))))))
        })
    };

    let labels_at = RwSignal::new(None::<(f64, f64)>);
    let labels = move || {
        let s = session.get()?;
        let all: Vec<String> = crate::labels::shown_auto(&s.auto_labels).into_iter().chain(s.labels).collect();
        let body = if all.is_empty() {
            view! { <span class=MUTED>"none"</span> }.into_any()
        } else {
            view! { <span class="font-mono text-[11px]" title=all.join(" ")>{all.join("  ")}</span> }.into_any()
        };
        Some(line("Labels", body, Some(Callback::new(move |ev: web_sys::MouseEvent| labels_at.set(Some(below(&ev, 384.0)))))))
    };

    let context_at = RwSignal::new(None::<(f64, f64)>);
    let context = move || {
        usage.with(|u| {
            let body = match u.context {
                None => view! { <span class=MUTED>"known after the first model call"</span> }.into_any(),
                Some(used) => {
                    let pct = (used * 100).checked_div(u.context_window);
                    let full = pct.is_some_and(|p| p >= 80);
                    view! {
                        {pct.map(|p| view! {
                            <span class="mr-1.5 inline-block h-1 w-10 overflow-hidden rounded-full bg-zinc-200 align-middle dark:bg-zinc-700">
                                <span class=format!("block h-full {}", if full { "bg-amber-500" } else { "bg-blue-500" }) style=format!("width:{}%", p.min(100))></span>
                            </span>
                        })}
                        <span class="tabular-nums">{match pct { Some(p) => format!("{p}% of {}", tokens(u.context_window)), None => tokens(used) }}</span>
                        <span class=MUTED>" · compacts at 85%"</span>
                    }
                    .into_any()
                }
            };
            line("Context", body, Some(Callback::new(move |ev: web_sys::MouseEvent| context_at.set(Some(below(&ev, 384.0))))))
        })
    };

    // What waits on you (an approval, a question), each going to its card.
    let waiting = move || {
        let list = chrome.waiting.get();
        (!list.is_empty()).then(|| {
            view! {
                <div class=HEADING>"Waiting on you"</div>
                {list.into_iter().map(|w| {
                    let seq = w.seq;
                    view! {
                        <button class=format!("{LINE} {ACTS}") title="Go to it in the transcript"
                            on:click=move |_| { chrome.go_waiting(seq); done(); }>
                            <span class="size-1.5 shrink-0 self-center rounded-full bg-amber-500"></span>
                            <span class="line-clamp-2 min-w-0 flex-1 text-amber-800 [overflow-wrap:anywhere] dark:text-amber-300">{w.what.clone()}</span>
                        </button>
                    }
                }).collect_view()}
                <div class="h-1"></div>
            }
        })
    };

    let published = Memo::new(move |_| events.with(|e| e.iter().any(|e| matches!(e.kind, EventKind::Output { .. }))));
    let show_output = move |name: Option<String>| {
        if let Some(c) = col {
            match name {
                Some(n) => crate::panes::show_output(c, &id.get_value(), n),
                None => crate::panes::show(c, Pane::Outputs, &id.get_value(), false),
            }
        }
        done();
    };

    view! {
        <div class="space-y-px">
            {waiting}
            {runs_on}
            {place}
            {branch}
            {changes}
            {work}
            {labels}
            {context}
            <Show when=move || published.get()>
                <div class=HEADING>"Newest outputs"</div>
                <crate::outputs::Recent events on_pick=show_output />
            </Show>
            // Its turns by hour, each going to its place in the transcript.
            <div class=HEADING>"Timeline"</div>
            <crate::timeline::Outline timeline=chrome.timeline on_go=move |t: c6_api::TimelineTurn| {
                chrome.go_turn(&t);
                done();
            } />
        </div>
        <Popover at=work_at class="w-[min(24rem,calc(100vw-1rem))] p-3">
            {move || session.get_untracked().map(|s| {
                let (sid, fallback) = (s.id.clone(), s.title.clone());
                let title = Signal::derive(move || {
                    live.sessions.with(|ss| ss.iter().find(|x| x.id == sid).map(|x| x.title.clone())).unwrap_or_else(|| fallback.clone())
                });
                view! { <crate::work::SessionWork session_id=s.id title /> }
            })}
        </Popover>
        <Popover at=labels_at class="w-[min(24rem,calc(100vw-1rem))] p-3">
            // Read once: a rebuilt editor loses what was typed.
            {move || session.get_untracked().map(|s| view! {
                <crate::labels::SessionLabels session=s.id labels=s.labels auto_labels=s.auto_labels />
            })}
        </Popover>
        <Popover at=context_at class="w-96 max-w-[calc(100vw-1rem)] space-y-1 p-3">
            <h3 class="text-[11px] font-semibold uppercase tracking-wide text-muted">"Context"</h3>
            <Context session=id.get_value() events usage running />
            <h3 class="pt-2 text-[11px] font-semibold uppercase tracking-wide text-muted">"Usage"</h3>
            <Usage usage />
        </Popover>
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

/// ⋯ → Session details…: what it has used, and what applies to it (its
/// labels, slash commands, agents, skills and hooks).
#[component]
pub fn SessionDetails(session: String, usage: Memo<UsageStats>, #[prop(into)] on_close: Callback<()>) -> impl IntoView {
    let panel = NodeRef::<leptos::html::Div>::new();
    // Focused, so Esc closes it (and doesn't stop the session's turn).
    Effect::new(move || {
        if let Some(el) = panel.get() {
            let _ = el.focus();
        }
    });
    view! {
        <div class="fixed inset-0 z-30 flex items-start justify-center overflow-y-auto bg-black/40 px-4 py-16" on:click=move |_| on_close.run(())
            on:keydown=move |ev| {
                if ev.key() == "Escape" {
                    ev.stop_propagation();
                    on_close.run(());
                }
            }>
            <div node_ref=panel role="dialog" aria-modal="true" aria-label="Session details" tabindex="-1"
                class="w-full max-w-2xl space-y-4 rounded-lg bg-white p-4 shadow-xl outline-none dark:bg-zinc-900" on:click=|ev| ev.stop_propagation()>
                <div class="flex items-center gap-2">
                    <h2 class="flex-1 font-medium">"Session details"</h2>
                    <button class=crate::icons::BAR_BUTTON title="Close (Esc)" aria-label="Close" on:click=move |_| on_close.run(())>{crate::icons::x()}</button>
                </div>
                <section class="space-y-1">
                    <h3 class="text-sm font-medium">"Usage"</h3>
                    <Usage usage />
                </section>
                <crate::labels::SessionConfig session />
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_is_shortened_to_its_last_two_parts() {
        assert_eq!(short_path("/home/me/src/carbon6"), "…/src/carbon6");
        assert_eq!(short_path("/home/me/wt/"), "…/me/wt");
        assert_eq!(short_path("/srv/app"), "/srv/app");
    }
}

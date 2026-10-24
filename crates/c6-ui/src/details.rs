//! A session's details card, at the top right of the session: where it
//! runs, how full its context is, what it has used, and what it changed.

use c6_api::{Event, EventKind, Session};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::session::{UsageStats, tokens};
use crate::util::BUTTON_QUIET;

const OPEN_KEY: &str = "c6.session.details";

/// Whether the card is open, remembered in this browser.
pub fn remembered() -> RwSignal<bool> {
    let open = RwSignal::new(crate::util::storage_get(OPEN_KEY).as_deref() == Some("1"));
    Effect::new(move || crate::util::storage_set(OPEN_KEY, if open.get() { "1" } else { "0" }));
    open
}

/// Lines added and removed in a unified diff.
fn diff_counts(diff: &str) -> (usize, usize) {
    diff.lines().fold((0, 0), |(a, d), l| {
        if l.starts_with('+') && !l.starts_with("+++") {
            (a + 1, d)
        } else if l.starts_with('-') && !l.starts_with("---") {
            (a, d + 1)
        } else {
            (a, d)
        }
    })
}

fn row(icon: &'static str, main: impl IntoView, side: impl IntoView) -> impl IntoView {
    view! {
        <div class="flex items-center gap-2 py-1">
            <span class="w-4 shrink-0 text-center text-zinc-400">{icon}</span>
            <span class="min-w-0 flex-1 truncate">{main}</span>
            <span class="shrink-0 text-xs text-zinc-500">{side}</span>
        </div>
    }
}

fn section(title: &'static str, body: impl IntoView) -> impl IntoView {
    view! {
        <section class="space-y-1 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
            <h3 class="text-xs font-semibold text-zinc-500">{title}</h3>
            {body}
        </section>
    }
}

#[component]
pub fn DetailsCard(
    session: Session,
    worker_name: String,
    online: Signal<bool>,
    events: RwSignal<Vec<Event>>,
    usage: Memo<UsageStats>,
    started: Memo<Option<(String, String)>>,
    model_label: Memo<String>,
    running: Signal<bool>,
    tab: RwSignal<&'static str>,
    open: RwSignal<bool>,
) -> impl IntoView {
    let s = StoredValue::new(session);
    // The folder as people know it (~/…), and the repository's name.
    let folder = s.with_value(|s| {
        let shown = c6_api::labels::display_labels(&s.auto_labels);
        shown.iter().find_map(|l| l.strip_prefix("folder:").map(str::to_owned))
    });
    let repo = s.with_value(|s| s.auto_labels.iter().find_map(|l| l.strip_prefix("repo:").map(str::to_owned)));
    let system_chars = Memo::new(move |_| {
        events.with(|evs| match evs.first().map(|e| &e.kind) {
            Some(EventKind::Started { system_prompt, .. }) => system_prompt.len() as u64,
            _ => 0,
        })
    });
    // Changes in the session's folder, looked at again after each turn.
    let ended = Memo::new(move |_| events.with(|evs| evs.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count()));
    let changes = LocalResource::new(move || {
        let (on, _) = (open.get(), ended.get());
        let at = s.with_value(|s| (s.ssh.is_none()).then(|| s.worker_id.to_string())).zip(started.get().map(|(c, _)| c));
        async move {
            let (w, cwd) = at.filter(|_| on)?;
            let status = api::git_status(w.clone(), cwd.clone()).await.ok()?;
            if status["repo"] == false {
                return Some(Err("Not a git repository"));
            }
            let diff = api::git_diff(w, cwd).await.ok()?;
            let (added, removed) = diff_counts(diff["diff"].as_str().unwrap_or_default());
            let files = status["files"].as_array().map_or(0, Vec::len);
            let branch = status["branch"].as_str().unwrap_or_default().to_owned();
            Some(Ok((added, removed, files, diff["truncated"] == true, branch)))
        }
    });
    let branch = move || {
        let worktree = started.get().map(|(_, b)| b).filter(|b| !b.is_empty());
        let current = changes.get().flatten().and_then(Result::ok).map(|c| c.4).filter(|b| !b.is_empty());
        (current.or(worktree.clone()), worktree.is_some())
    };
    let compact = move |_| {
        let id = s.with_value(|s| s.id.clone());
        spawn_local(async move {
            let _ = api::send_message(id, "/compact".into(), None, Vec::new()).await;
        });
    };

    let environment = view! {
        {row(
            "▣",
            view! {
                <span class="inline-flex items-center gap-1.5">
                    <span class=move || if online.get() { "h-1.5 w-1.5 rounded-full bg-emerald-500" } else { "h-1.5 w-1.5 rounded-full bg-zinc-400" }></span>
                    {worker_name}
                </span>
            },
            s.with_value(|s| s.ssh.clone().map(|t| format!("⇄ {t}"))),
        )}
        {row(
            "▤",
            view! { <span class="font-mono text-xs" title=move || started.get().map(|(c, _)| c)>{folder.or_else(|| s.with_value(|s| Some(s.cwd.clone()))).filter(|f| !f.is_empty()).unwrap_or_else(|| "scratch workspace".into())}</span> },
            repo.map(|r| r.rsplit('/').next().unwrap_or(&r).to_owned()),
        )}
        {move || {
            let (b, worktree) = branch();
            b.map(|b| row("⑂", view! { <span class="font-mono text-xs">{b}</span> }, worktree.then_some("Worktree")))
        }}
        {row("◆", view! { <span>{move || model_label.get()}</span> }, s.with_value(|s| s.realm.clone()))}
    };

    let context = move || {
        usage.with(|u| {
            let window = u.context_window;
            let Some(used) = u.context else {
                return view! { <p class="py-1 text-xs text-zinc-500">"Known after the first model call."</p> }.into_any();
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
                        <span class="flex-1 text-zinc-600 dark:text-zinc-400">{label}</span>
                        <span class="tabular-nums">{tokens(n)}</span>
                        <span class="w-12 text-right tabular-nums text-zinc-500">{if window == 0 { "–".into() } else { format!("{:.1}%", pct(n)) }}</span>
                    </div>
                }
            };
            view! {
                <div class="flex items-baseline justify-between py-1 text-sm">
                    <span class="font-medium">{if window == 0 { tokens(used) } else { format!("{:.0}% used", pct(used)) }}</span>
                    <span class="text-xs tabular-nums text-zinc-500">{if window == 0 { String::new() } else { format!("{} / {}", tokens(used), tokens(window)) }}</span>
                </div>
                <div class="relative flex h-2 gap-px overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-800">
                    {bar(rest, "bg-blue-500")}
                    {bar(system, "bg-amber-500")}
                    // Compaction starts here.
                    <span class="absolute inset-y-0 w-px bg-zinc-500" style="left:85%" title="Compacts at 85%"></span>
                </div>
                <div class="space-y-1 pt-2">
                    {line("bg-blue-500", "Conversation and tools", rest)}
                    {line("bg-amber-500", "System prompt (estimated)", system)}
                    {(window > 0).then(|| line("bg-zinc-200 dark:bg-zinc-700", "Free", free))}
                </div>
                <div class="flex items-center justify-between pt-1 text-xs text-zinc-500">
                    <span>"Compacts at 85%"</span>
                    <button class=format!("{BUTTON_QUIET} text-xs") disabled=running title="Summarize the conversation so far to free up context (/compact)" on:click=compact>"Compact now"</button>
                </div>
            }.into_any()
        })
    };

    let totals = move || {
        usage.with(|u| {
            let turns = u.turns.len();
            view! {
                <p class="py-1 text-xs text-zinc-600 dark:text-zinc-400">
                    {format!("{turns} turn{} · {} model call{}", if turns == 1 { "" } else { "s" }, u.total.calls, if u.total.calls == 1 { "" } else { "s" })}
                </p>
                <p class="font-mono text-[11px] text-zinc-500">{u.total.summary()}</p>
            }
        })
    };

    let changed = move || {
        match changes.get() {
        None => view! { <p class="py-1 text-xs text-zinc-500">"Looking…"</p> }.into_any(),
        Some(None) => {
            let why = if online.get() { "Not available for this session." } else { "Shown while its worker is online." };
            view! { <p class="py-1 text-xs text-zinc-500">{why}</p> }.into_any()
        }
        Some(Some(Err(e))) => view! { <p class="py-1 text-xs text-zinc-500">{e}</p> }.into_any(),
        Some(Some(Ok((added, removed, files, truncated, _)))) => view! {
            <div class="flex items-center gap-2 py-1 text-sm">
                <span class="tabular-nums text-emerald-600 dark:text-emerald-400">{format!("+{added}{}", if truncated { "+" } else { "" })}</span>
                <span class="tabular-nums text-red-600 dark:text-red-400">{format!("−{removed}")}</span>
                <span class="flex-1 text-xs text-zinc-500">{format!("{files} file{}", if files == 1 { "" } else { "s" })}</span>
                <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| tab.set("changes")>"Review"</button>
            </div>
        }.into_any(),
    }
    };

    view! {
        <div class="absolute right-3 top-3 z-20 w-80 max-w-[calc(100%-1.5rem)] space-y-2 rounded-xl border border-zinc-200 bg-white p-2 text-sm shadow-lg dark:border-zinc-800 dark:bg-zinc-900">
            <div class="flex items-center justify-between px-1">
                <span class="text-xs font-semibold text-zinc-500">"Session details"</span>
                <button class="text-xs text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100" title="Close" on:click=move |_| open.set(false)>"✕"</button>
            </div>
            {section("Environment", environment)}
            {section("Context", context)}
            {section("Usage", totals)}
            {s.with_value(|s| s.ssh.is_none()).then(|| section("Changes", changed))}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_diff_lines() {
        let d = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n+more\n same\n";
        assert_eq!(diff_counts(d), (2, 1));
    }
}

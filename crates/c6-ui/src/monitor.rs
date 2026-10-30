//! The activity panel (docs/activity.md): what's going on now. Model calls
//! as they stream, sessions running, and what each connector's sync is
//! doing; all kept in the head's memory, gone on a restart.

use leptos::prelude::*;
use serde_json::Value;

use crate::api;
use crate::util::{ErrorBox, ago, now_ms};

/// Seconds, as 4.2s or 1m 12s.
fn took(ms: i64) -> String {
    let s = ms.max(0) as f64 / 1000.0;
    if s < 60.0 { format!("{s:.1}s") } else { format!("{}m {:02}s", (s / 60.0) as i64, (s % 60.0) as i64) }
}

fn state_dot(state: &str) -> &'static str {
    match state {
        "waiting" => "bg-zinc-400 animate-pulse",
        "thinking" => "bg-violet-500 animate-pulse",
        "answering" => "bg-blue-500 animate-pulse",
        "failed" => "bg-red-500",
        "stopped" => "bg-amber-500",
        _ => "bg-emerald-500",
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

#[component]
pub fn ActivityPage() -> impl IntoView {
    let data = RwSignal::new(None::<Result<Value, String>>);
    let now = RwSignal::new(now_ms());
    let load = move || {
        leptos::task::spawn_local(async move {
            data.set(Some(api::activity().await));
            now.set(now_ms());
        });
    };
    load();
    // Live while you look: again every second and a half.
    let tick = set_interval_with_handle(load, std::time::Duration::from_millis(1500)).ok();
    on_cleanup(move || {
        if let Some(t) = tick {
            t.clear();
        }
    });
    let only_live = RwSignal::new(false);
    let open = RwSignal::new(None::<u64>);
    let get = move |k: &'static str| data.get().and_then(Result::ok).map(|d| d[k].clone()).unwrap_or(Value::Null);
    // The calls shown; each row stays as the list refreshes (an unfolded
    // one keeps what you opened in it).
    let calls = Memo::new(move |_| {
        let all = get("calls").as_array().cloned().unwrap_or_default();
        all.into_iter().filter(|c| !only_live.get() || c["ended_at"].is_null()).take(100).collect::<Vec<Value>>()
    });
    let ids = move || calls.get().into_iter().filter_map(|c| c["id"].as_u64()).collect::<Vec<u64>>();
    view! {
        <main class="mx-auto max-w-6xl space-y-6 px-4 py-5">
            <header class="space-y-1">
                <h1 class="text-xl font-semibold">"Activity"</h1>
                <p class="text-sm text-zinc-500">"What's going on now: model calls as they happen, sessions running, and each connector's sync. Kept in the head's memory only (the last 200 calls); a restart clears it."</p>
            </header>
            {move || data.get().and_then(Result::err).map(|e| view! { <ErrorBox message=e /> })}

            <section class="space-y-2">
                <div class="flex flex-wrap items-baseline gap-3">
                    <h2 class="text-[11px] font-semibold uppercase tracking-wide text-zinc-500">"Model calls"</h2>
                    // In flight and waiting, by connection: a local server answers one at a time.
                    {move || {
                        let calls = get("calls");
                        let mut by: std::collections::BTreeMap<String, (u32, u32)> = Default::default();
                        for c in calls.as_array().into_iter().flatten().filter(|c| c["ended_at"].is_null()) {
                            let e = by.entry(s(&c["connection_name"])).or_default();
                            if c["state"] == "waiting" { e.1 += 1 } else { e.0 += 1 }
                        }
                        by.into_iter().map(|(name, (going, waiting))| view! {
                            <span class="rounded bg-zinc-100 px-1.5 text-xs dark:bg-zinc-800">
                                {format!("{}: {going} answering{}", if name.is_empty() { "?".into() } else { name }, if waiting > 0 { format!(", {waiting} waiting for an answer") } else { String::new() })}
                            </span>
                        }).collect_view()
                    }}
                    <span class="flex-1"></span>
                    <label class="flex items-center gap-1 text-xs text-zinc-500">
                        <input type="checkbox" prop:checked=move || only_live.get() on:change=move |ev| only_live.set(event_target_checked(&ev)) />
                        "Only in flight"
                    </label>
                </div>
                <div class="divide-y divide-zinc-100 overflow-hidden rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                    {move || calls.with(Vec::is_empty).then(|| view! {
                        <p class="px-3 py-2 text-sm text-zinc-500">{if only_live.get() { "No model call in flight." } else { "No model calls since the head started." }}</p>
                    })}
                    <For each=ids key=|id| *id let:id>
                        <CallRow id calls now open />
                    </For>
                </div>
                <p class="text-xs text-zinc-500">"Calls the head makes: the brief's sorting and drafts, the memex's reading, vision, the assistant and suggestions. Agent sessions on workers are under Sessions running; embeddings aren't shown."</p>
            </section>

            <section class="space-y-2">
                <h2 class="text-[11px] font-semibold uppercase tracking-wide text-zinc-500">"Sessions running"</h2>
                <div class="divide-y divide-zinc-100 rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                    {move || {
                        let rows = get("sessions").as_array().cloned().unwrap_or_default();
                        if rows.is_empty() {
                            return view! { <p class="px-3 py-2 text-sm text-zinc-500">"None."</p> }.into_any();
                        }
                        rows.into_iter().map(|r| view! {
                            <a class="flex flex-wrap items-baseline gap-x-2 px-3 py-1.5 text-sm hover:bg-zinc-50 dark:hover:bg-zinc-900/50"
                                href=format!("/r/{}/sessions/{}", s(&r["realm"]), s(&r["id"]))>
                                <span class="h-2 w-2 shrink-0 animate-pulse rounded-full bg-blue-500"></span>
                                <span class="min-w-0 flex-1 truncate font-medium">{s(&r["title"])}</span>
                                <span class="text-xs text-zinc-500">{format!("{} · {} · {}", s(&r["realm"]), s(&r["connection_name"]), s(&r["model"]))}</span>
                                <span class="text-xs text-zinc-400">{s(&r["worker"])}</span>
                            </a>
                        }).collect_view().into_any()
                    }}
                </div>
            </section>

            <section class="space-y-2">
                <h2 class="text-[11px] font-semibold uppercase tracking-wide text-zinc-500">"Connectors"</h2>
                <div class="grid gap-3 md:grid-cols-2">
                    {move || {
                        let rows = get("connectors").as_array().cloned().unwrap_or_default();
                        if rows.is_empty() {
                            return view! { <p class="text-sm text-zinc-500">"No app syncs anything yet (Settings → Apps)."</p> }.into_any();
                        }
                        let t = now.get();
                        rows.into_iter().map(|c| view! { <Connector c now=t /> }).collect_view().into_any()
                    }}
                </div>
            </section>

            <section class="space-y-2">
                <h2 class="text-[11px] font-semibold uppercase tracking-wide text-zinc-500">"Memex"</h2>
                {move || {
                    let m = get("memex");
                    let st = &m["status"];
                    let mut lines = Vec::new();
                    if let (Some(n), Some(per)) = (st["calls_today"].as_u64(), st["calls_per_day"].as_u64()) {
                        lines.push(format!("Model calls in the last day: {n} of {per}"));
                    }
                    if let Some(n) = st["extracting"].as_u64().filter(|n| *n > 0) {
                        lines.push(format!("{n} files waiting to be read"));
                    }
                    if let Some(n) = st["outbox"].as_u64().filter(|n| *n > 0) {
                        lines.push(format!("{n} things from sessions waiting to go in"));
                    }
                    for r in m["sorting"].as_array().into_iter().flatten() {
                        let v = &r["status"];
                        let waiting = v["waiting"].as_u64().unwrap_or(0);
                        let why = if v["model"] != true {
                            " (no understanding model chosen)"
                        } else if v["budget_left"].as_u64() == Some(0) {
                            " (the day's model calls are used up)"
                        } else {
                            ""
                        };
                        lines.push(format!("{}: {waiting}{} conversations waiting to be sorted for the brief{why}", s(&r["realm"]), if waiting >= 20 { "+" } else { "" }));
                        // The walk back through the mail, and the cards it grows.
                        let w = &r["walk"];
                        if let Some(total) = w["threads"].as_i64().filter(|n| *n > 0) {
                            let read = w["threads_read"].as_i64().unwrap_or(0);
                            let back = w["oldest_read"].as_i64().map(crate::when::compact);
                            lines.push(format!(
                                "{}: read {read} of {total} mail threads ({}%){}{}; {} relationship cards{}; {} correspondents",
                                s(&r["realm"]),
                                read * 100 / total,
                                back.map(|b| format!(", back to {b}")).unwrap_or_default(),
                                w["threads_failed"].as_i64().filter(|n| *n > 0).map(|n| format!("; {n} couldn't be read")).unwrap_or_default(),
                                w["cards"].as_i64().unwrap_or(0),
                                w["cards_waiting"].as_i64().filter(|n| *n > 0).map(|n| format!(" ({n} waiting to take in new threads)")).unwrap_or_default(),
                                w["correspondents"].as_i64().unwrap_or(0)
                            ));
                        }
                    }
                    view! {
                        <div class="space-y-1 rounded-lg border border-zinc-200 p-3 text-sm dark:border-zinc-800">
                            {lines.into_iter().map(|l| view! { <p>{l}</p> }).collect_view()}
                            {st["last_error"].as_str().map(|e| view! { <p class="text-red-600">{e.to_owned()}</p> })}
                        </div>
                    }
                }}
            </section>
        </main>
    }
}

/// One model call; a click unfolds it: the prompt, thinking and answer,
/// live while it goes on.
#[component]
fn CallRow(id: u64, calls: Memo<Vec<Value>>, now: RwSignal<i64>, open: RwSignal<Option<u64>>) -> impl IntoView {
    let live = calls.with_untracked(|cs| cs.iter().find(|c| c["id"] == id).is_some_and(|c| c["ended_at"].is_null()));
    view! {
        <div>
            {move || calls.with(|cs| cs.iter().find(|c| c["id"] == id).cloned()).map(|c| view! { <CallLine c now=now.get() open /> })}
            {move || (open.get() == Some(id)).then(|| view! { <CallDetail id live /> })}
        </div>
    }
}

/// A call's line: what, about what, where, how it's going.
#[component]
fn CallLine(c: Value, now: i64, open: RwSignal<Option<u64>>) -> impl IntoView {
    let id = c["id"].as_u64().unwrap_or(0);
    let state = s(&c["state"]);
    let started = c["started_at"].as_i64().unwrap_or(now);
    let ended = c["ended_at"].as_i64();
    let first = c["first_at"].as_i64();
    let out = c["output_tokens"].as_u64().unwrap_or(0);
    let rate = match (first, ended) {
        (Some(f), Some(e)) if out > 0 && e > f => format!(" · {:.0} tok/s", out as f64 * 1000.0 / (e - f) as f64),
        _ => String::new(),
    };
    let timing = format!(
        "{}{}{}",
        took(ended.unwrap_or(now) - started),
        first.map(|f| format!(" · first token {}", took(f - started))).unwrap_or_default(),
        rate
    );
    let tokens = if c["input_tokens"].as_u64().unwrap_or(0) > 0 || out > 0 {
        format!("{} in · {out} out", c["input_tokens"].as_u64().unwrap_or(0))
    } else {
        String::new()
    };
    let when = ended.map(|e| ago(now - e)).unwrap_or_default();
    view! {
            <div class=move || format!("flex cursor-pointer flex-wrap items-baseline gap-x-2 px-3 py-1.5 text-sm {}", if open.get() == Some(id) { "bg-zinc-100 dark:bg-zinc-900" } else { "hover:bg-zinc-50 dark:hover:bg-zinc-900/50" })
                on:click=move |_| open.update(|o| *o = if *o == Some(id) { None } else { Some(id) })>
                <span class=format!("h-2 w-2 shrink-0 self-center rounded-full {}", state_dot(&state)) title=state.clone()></span>
                <span class="w-24 shrink-0 font-medium">{s(&c["what"])}</span>
                <span class="min-w-0 flex-1 truncate" title=s(&c["about"])>{s(&c["about"])}</span>
                // A pool's call: the member that took it, and the pool.
                <span class="shrink-0 text-xs text-zinc-500">{format!("{}{} · {}{}", s(&c["connection_name"]), if s(&c["via_name"]).is_empty() { String::new() } else { format!(" (pool {})", s(&c["via_name"])) }, s(&c["model"]), if c["effort"].as_str().is_some_and(|e| !e.is_empty()) { format!(" · {}", s(&c["effort"])) } else { String::new() })}</span>
                <span class="w-full truncate pl-4 text-xs text-zinc-500 sm:w-auto sm:pl-0">{format!("{state} · {timing}{}{}", if tokens.is_empty() { String::new() } else { format!(" · {tokens}") }, if when.is_empty() { String::new() } else { format!(" · {when}") })}</span>
                {(c["ended_at"].is_null() || !c["latest"].as_str().unwrap_or_default().is_empty()).then(|| view! {
                    <span class="w-full truncate pl-4 font-mono text-[11px] text-zinc-400">{s(&c["latest"])}</span>
                })}
                {c["error"].as_str().filter(|e| !e.is_empty()).map(|e| view! { <span class="w-full pl-4 text-xs text-red-600">{e.to_owned()}</span> })}
            </div>
    }
}

#[component]
fn CallDetail(id: u64, live: bool) -> impl IntoView {
    let call = RwSignal::new(None::<Result<Value, String>>);
    let load = move || {
        // Done calls don't change: fetched once.
        if call.with_untracked(|c| matches!(c, Some(Ok(v)) if !v["ended_at"].is_null())) {
            return;
        }
        leptos::task::spawn_local(async move {
            call.set(Some(api::activity_call(id).await));
        });
    };
    load();
    // Follows the answer as it streams.
    let tick = live.then(|| set_interval_with_handle(load, std::time::Duration::from_millis(1000)).ok()).flatten();
    on_cleanup(move || {
        if let Some(t) = tick {
            t.clear();
        }
    });
    let field =
        move |k: &'static str| move || call.with(|c| c.as_ref().and_then(|r| r.as_ref().ok()).map(|v| s(&v[k])).unwrap_or_default());
    // Each part stays put as its text grows, so what you opened stays open.
    let block = move |title: &'static str, k: &'static str| {
        let text = field(k);
        let shown = field(k);
        let size = field(k);
        view! {
            <details class="rounded border border-zinc-200 dark:border-zinc-800" class:hidden=move || shown().is_empty() open=title != "Prompt">
                <summary class="cursor-pointer px-2 py-1 text-xs text-zinc-500">{move || format!("{title} ({} characters)", size().chars().count())}</summary>
                <pre class="max-h-96 overflow-auto whitespace-pre-wrap break-words px-2 py-1 font-mono text-[11px]">{text}</pre>
            </details>
        }
    };
    view! {
        <div class="space-y-2 border-t border-zinc-100 bg-zinc-50/50 p-3 dark:border-zinc-800 dark:bg-zinc-900/30">
            {move || match call.get() {
                None => Some(view! { <p class="text-xs text-zinc-500">"…"</p> }.into_any()),
                Some(Err(e)) => Some(view! { <ErrorBox message=e /> }.into_any()),
                Some(Ok(_)) => None,
            }}
            {block("Prompt", "prompt")}
            {block("Thinking", "thinking")}
            {block("Answer", "text")}
        </div>
    }
}

/// An app's sync: going on now, or how its last round went; each account.
#[component]
fn Connector(c: Value, now: i64) -> impl IntoView {
    let round = &c["round"];
    let syncing = round.is_object() && round["ended_at"].is_null();
    let line = if syncing {
        format!(
            "Syncing now · {} · run {} · {} items, {} records so far",
            took(now - round["started_at"].as_i64().unwrap_or(now)),
            round["runs"].as_u64().unwrap_or(0) + 1,
            round["items"].as_u64().unwrap_or(0),
            round["records"].as_u64().unwrap_or(0)
        )
    } else if round.is_object() {
        format!(
            "Last round {} · took {} · {} run{} · {} items",
            ago(now - round["ended_at"].as_i64().unwrap_or(now)),
            took(round["ended_at"].as_i64().unwrap_or(0) - round["started_at"].as_i64().unwrap_or(0)),
            round["runs"].as_u64().unwrap_or(0),
            if round["runs"] == 1 { "" } else { "s" },
            round["items"].as_u64().unwrap_or(0)
        )
    } else if let Some(t) = c["last_run_at"].as_i64() {
        format!("Last ran {}", ago(now - t))
    } else {
        "Not run since the head started".into()
    };
    let error = [&round["error"], &c["last_error"]].into_iter().filter_map(|e| e.as_str()).find(|e| !e.is_empty()).map(str::to_owned);
    view! {
        <div class="space-y-2 rounded-lg border border-zinc-200 p-3 text-sm dark:border-zinc-800">
            <div class="flex flex-wrap items-baseline gap-2">
                <span class=format!("h-2 w-2 shrink-0 self-center rounded-full {}", if syncing { "animate-pulse bg-blue-500" } else if error.is_some() { "bg-red-500" } else if c["enabled"] == true { "bg-emerald-500" } else { "bg-zinc-400" })></span>
                <span class="font-medium">{s(&c["app"])}</span>
                <span class="text-xs text-zinc-500">{format!("{} · {} · {}", s(&c["realm"]), s(&c["name"]), s(&c["cron"]))}</span>
                {(c["enabled"] != true).then(|| view! { <span class="text-xs text-zinc-500">"(off)"</span> })}
            </div>
            <p class="text-xs">{line}</p>
            {error.map(|e| view! { <p class="text-xs text-red-600">{e}</p> })}
            {c["accounts"].as_array().filter(|a| !a.is_empty()).map(|accounts| view! {
                <ul class="space-y-0.5 text-xs text-zinc-600 dark:text-zinc-400">
                    {accounts.iter().map(|a| {
                        let what = if a["kind"] == "slack" {
                            let mut w = format!("{} of {} conversations imported", a["imported"].as_u64().unwrap_or(0), a["conversations"].as_u64().unwrap_or(0));
                            if let Some(n) = a["files_waiting"].as_u64().filter(|n| *n > 0) {
                                w.push_str(&format!(" · {n} files waiting"));
                            }
                            if let Some(t) = a["retry_after"].as_i64().filter(|t| *t > now) {
                                w.push_str(&format!(" · rate limited for {}", took(t - now)));
                            }
                            w
                        } else {
                            let phase = match a["phase"].as_str() { Some("import") => "importing", Some("live") => "up to date, following", Some(p) => p, None => "" };
                            let mut w = phase.to_owned();
                            if let Some(n) = a["done"].as_u64() {
                                w.push_str(&format!(" · {n} done"));
                            }
                            w
                        };
                        let who = a["email"].as_str().or(a["account"].as_str()).unwrap_or_default().to_owned();
                        view! {
                            <li class="flex flex-wrap gap-x-2">
                                <span class="font-medium">{who}</span>
                                <span>{s(&a["kind"])}</span>
                                <span class="flex-1">{what}</span>
                                {a["updated_at"].as_i64().map(|t| view! { <span class="text-zinc-400">{format!("synced {}", ago(now - t))}</span> })}
                                {a["error"].as_str().filter(|e| !e.is_empty()).map(|e| view! { <span class="w-full text-red-600">{e.to_owned()}</span> })}
                            </li>
                        }
                    }).collect_view()}
                </ul>
            })}
        </div>
    }
}

/// In the top bar: model calls in flight and syncs under way, linking to
/// the panel; nothing when it's quiet.
#[component]
pub fn ActivityBadge() -> impl IntoView {
    let counts = RwSignal::new(Value::Null);
    // Only the console may see it; elsewhere it stays empty, unasked.
    let console = crate::place::is_console();
    let load = move || {
        if !console {
            return;
        }
        leptos::task::spawn_local(async move {
            counts.set(api::activity_counts().await.unwrap_or(Value::Null));
        });
    };
    load();
    let tick = set_interval_with_handle(load, std::time::Duration::from_secs(3)).ok();
    on_cleanup(move || {
        if let Some(t) = tick {
            t.clear();
        }
    });
    view! {
        {move || {
            let c = counts.get();
            let (calls, syncing) = (c["calls"].as_u64().unwrap_or(0), c["syncing"].as_u64().unwrap_or(0));
            (calls + syncing > 0).then(|| {
                let mut parts = Vec::new();
                if calls > 0 {
                    parts.push(format!("{calls} model call{}", if calls == 1 { "" } else { "s" }));
                }
                if syncing > 0 {
                    parts.push(format!("{syncing} syncing"));
                }
                view! {
                    <a href="/admin/activity" title=format!("{} (Activity)", parts.join(" · ")).clone()
                        class="flex shrink-0 items-center gap-1.5 whitespace-nowrap rounded-md px-2 py-1 text-xs text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-800">
                        <span class="h-2 w-2 animate-pulse rounded-full bg-blue-500"></span>
                        <span class="hidden 2xl:inline">{parts.join(" · ")}</span>
                    </a>
                }
            })
        }}
    }
}

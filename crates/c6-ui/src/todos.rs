//! Action items (docs/follow-through.md §7): things to settle when you want
//! to, with their context; settle one inline or start a session for it.

use c6_api::Todo;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, ago, now_ms};
use crate::workspace::{Go, How, View, use_workspace};

/// The header link, with how many are open.
#[component]
pub fn TodoLink(class: &'static str) -> impl IntoView {
    let live = api::live();
    let tick = RwSignal::new(0u32);
    let handle = set_interval_with_handle(move || tick.update(|t| *t += 1), std::time::Duration::from_secs(30)).ok();
    on_cleanup(move || {
        if let Some(h) = handle {
            h.clear();
        }
    });
    let count = LocalResource::new(move || {
        tick.track();
        live.sessions.track();
        api::todo_count()
    });
    view! {
        <Go to=View::Todos class=class>
            "To do"
            {move || count.get().and_then(Result::ok).filter(|n| *n > 0).map(|n| view! {
                <span class="ml-1 rounded-full bg-sky-600 px-1.5 text-[10px] font-semibold text-white">{n}</span>
            })}
        </Go>
    }
}

#[component]
pub fn TodosPage() -> impl IntoView {
    let state = RwSignal::new("open".to_owned());
    let tick = RwSignal::new(0u32);
    let list = LocalResource::new(move || {
        tick.track();
        let s = state.get();
        async move { api::todos(s).await }
    });
    let refresh = Callback::new(move |()| tick.update(|t| *t += 1));
    view! {
        <main class="mx-auto max-w-3xl space-y-4 px-4 py-6">
            <div class="flex flex-wrap items-center gap-2">
                <h1 class="text-lg font-semibold">"To do"</h1>
                <span class="flex-1 text-sm text-zinc-500">"Things agents could not settle on their own. They carried on meanwhile; settle these when you like."</span>
                <select class=format!("{INPUT} w-auto py-1 text-sm") prop:value=move || state.get() on:change=move |ev| state.set(event_target_value(&ev))>
                    <option value="open">"Open"</option>
                    <option value="snoozed">"Snoozed"</option>
                    <option value="done">"Done"</option>
                    <option value="dismissed">"Dismissed"</option>
                </select>
            </div>
            <Suspense fallback=|| ()>
                {move || list.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(v) if v.is_empty() => view! { <p class="text-sm text-zinc-500">"Nothing here."</p> }.into_any(),
                    Ok(v) => v.into_iter().map(|t| view! { <TodoCard t refresh /> }).collect_view().into_any(),
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn TodoCard(t: Todo, refresh: Callback<()>) -> impl IntoView {
    let ws = use_workspace();
    let id = t.id;
    let resolution = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let set = move |body: serde_json::Value| {
        spawn_local(async move {
            match api::edit_todo(id, body).await {
                Ok(_) => refresh.run(()),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let start = move |_| {
        busy.set(true);
        spawn_local(async move {
            match api::todo_session(id).await {
                Ok(s) => {
                    ws.open(None, View::session(s.id), How::Beside);
                    refresh.run(());
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    let facts: Vec<String> =
        t.context["facts"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let things: Vec<String> =
        t.context["things"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let session = t.context["session"].as_str().map(str::to_owned);
    let open = matches!(t.state.as_str(), "open" | "snoozed");
    let tone = match t.priority.as_str() {
        "high" => "border-amber-300 dark:border-amber-900",
        _ => "border-zinc-200 dark:border-zinc-800",
    };
    view! {
        <section class=format!("space-y-2 rounded-lg border p-3 {tone}")>
            <div class="flex flex-wrap items-center gap-2">
                <span class="rounded bg-zinc-100 px-1.5 text-[11px] dark:bg-zinc-800">{t.kind.clone()}</span>
                <h2 class="min-w-0 flex-1 font-medium">{t.title.clone()}</h2>
                {(t.count > 1).then(|| view! { <span class="text-xs text-zinc-500">{format!("raised {}×", t.count)}</span> })}
                <span class="text-xs text-zinc-500">{format!("{} ago", ago(now_ms() - t.updated_at))}</span>
            </div>
            {(!t.detail.is_empty()).then(|| view! { <p class="whitespace-pre-wrap text-sm text-zinc-600 dark:text-zinc-400">{t.detail.clone()}</p> })}
            <div class="flex flex-wrap items-center gap-2 text-xs text-zinc-500">
                <span>{format!("from {}", t.source)}</span>
                {session.map(|s| view! { <Go to=View::session(s) class="text-sky-700 hover:underline dark:text-sky-400">"the session ↗"</Go> })}
                {things.into_iter().map(|k| view! { <span class="rounded bg-violet-100 px-1 font-mono text-violet-800 dark:bg-violet-950 dark:text-violet-300">{k}</span> }).collect_view()}
                {t.sessions.iter().map(|s| view! { <Go to=View::session(s.clone()) class="text-sky-700 hover:underline dark:text-sky-400">"working session ↗"</Go> }).collect_view()}
            </div>
            {(!facts.is_empty()).then(|| view! { <FactList ids=facts on_change=refresh /> })}
            {(!t.resolution.is_empty()).then(|| view! { <p class="text-sm text-emerald-700 dark:text-emerald-400">{format!("Resolved: {}", t.resolution)}</p> })}
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {open.then(|| view! {
                <div class="flex flex-wrap items-center gap-2">
                    <input class=format!("{INPUT} min-w-0 flex-1 py-1 text-sm") placeholder="How it was settled (optional)"
                        prop:value=move || resolution.get() on:input=move |ev| resolution.set(event_target_value(&ev)) />
                    <button class=BUTTON on:click=move |_| set(json!({"state": "done", "resolution": resolution.get_untracked()}))>"Done"</button>
                    <button class=BUTTON_QUIET disabled=move || busy.get() on:click=start title="A session that helps you settle it, with its context">"Work on it"</button>
                    <button class=BUTTON_QUIET on:click=move |_| set(json!({"state": "snoozed", "snooze_until": now_ms() + 86_400_000}))>"Tomorrow"</button>
                    <button class=BUTTON_QUIET on:click=move |_| set(json!({"state": "snoozed", "snooze_until": now_ms() + 7 * 86_400_000}))>"Next week"</button>
                    <button class=format!("{BUTTON_QUIET} text-zinc-500") on:click=move |_| set(json!({"state": "dismissed"}))>"Dismiss"</button>
                </div>
            })}
            {(!open).then(|| view! { <button class=BUTTON_QUIET on:click=move |_| set(json!({"state": "open"}))>"Reopen"</button> })}
        </section>
    }
}

/// Facts (by id) with what to do about each: confirm, retire, correct.
#[component]
pub fn FactList(ids: Vec<String>, #[prop(optional)] on_change: Option<Callback<()>>) -> impl IntoView {
    let ids = StoredValue::new(ids);
    let tick = RwSignal::new(0u32);
    let facts = LocalResource::new(move || {
        tick.track();
        api::rpc_as::<Vec<c6_api::Fact>>("memex.knowledge.get", json!({"ids": ids.get_value()}))
    });
    let changed = move || {
        tick.update(|t| *t += 1);
        if let Some(c) = on_change {
            c.run(());
        }
    };
    view! {
        <Suspense fallback=|| ()>
            {move || facts.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(fs) => view! {
                    <ul class="space-y-2">{fs.into_iter().map(|f| view! { <FactRow f on_change=Callback::new(move |()| changed()) /> }).collect_view()}</ul>
                }.into_any(),
            })}
        </Suspense>
    }
}

pub fn status_tone(status: &str) -> &'static str {
    match status {
        "reviewed" => "bg-emerald-100 text-emerald-800 dark:bg-emerald-950 dark:text-emerald-300",
        "verified" => "bg-sky-100 text-sky-800 dark:bg-sky-950 dark:text-sky-300",
        "observed" => "bg-zinc-100 text-zinc-700 dark:bg-zinc-800 dark:text-zinc-300",
        "inferred" => "bg-violet-100 text-violet-800 dark:bg-violet-950 dark:text-violet-300",
        "disputed" => "bg-amber-100 text-amber-900 dark:bg-amber-950 dark:text-amber-200",
        _ => "bg-zinc-100 text-zinc-400 line-through dark:bg-zinc-900",
    }
}

#[component]
pub fn FactRow(f: c6_api::Fact, on_change: Callback<()>) -> impl IntoView {
    let id = StoredValue::new(f.id.clone());
    let fixing = RwSignal::new(false);
    let text = RwSignal::new(f.claim.clone());
    let error = RwSignal::new(None::<String>);
    let send = move |body: serde_json::Value| {
        spawn_local(async move {
            let mut b = body;
            b["id"] = json!(id.get_value());
            match api::rpc("memex.knowledge.set", b).await {
                Ok(_) => on_change.run(()),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let current = f.valid_to.is_none();
    let last = f.evidence.last().cloned().unwrap_or_default();
    view! {
        <li class="space-y-1 rounded-md bg-zinc-50 p-2 text-sm dark:bg-zinc-900">
            <div class="flex flex-wrap items-center gap-2">
                <span class=format!("rounded px-1.5 text-[11px] {}", status_tone(&f.status))>{f.status.clone()}</span>
                <span class="text-[11px] text-zinc-500">{f.volatility.clone()}{f.due.then_some(" · due")}</span>
                {(!f.labels.is_empty()).then(|| view! { <span class="text-[11px] text-zinc-500">{format!("only {}", f.labels.join(", "))}</span> })}
            </div>
            <p>{f.negative.then_some("NOT: ")}{f.claim.clone()}</p>
            {(!f.conditions.is_empty()).then(|| view! { <p class="text-xs text-zinc-500">{format!("when: {}", f.conditions)}</p> })}
            {f.check.clone().map(|c| view! { <p class="font-mono text-[11px] text-zinc-500">{format!("check: {}", c.command)}</p> })}
            {(!last.is_null()).then(|| {
                let what = [last["command"].as_str(), last["source"].as_str()].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
                view! {
                    <details class="text-xs text-zinc-500">
                        <summary class="cursor-pointer">{format!("evidence ({}) {}", f.evidence.len(), last["by"].as_str().unwrap_or(""))}</summary>
                        <p class="font-mono">{what}</p>
                        <pre class="max-h-40 overflow-auto whitespace-pre-wrap">{last["excerpt"].as_str().unwrap_or("").to_owned()}</pre>
                    </details>
                }
            })}
            {move || error.get().map(|e| view! { <p class="text-xs text-red-600">{e}</p> })}
            {current.then(|| view! {
                <div class="flex flex-wrap gap-2 text-xs">
                    {(f.status != "reviewed").then(|| view! { <button class="text-emerald-700 hover:underline dark:text-emerald-400" on:click=move |_| send(json!({"status": "reviewed"}))>"Confirm"</button> })}
                    <button class="text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| fixing.update(|x| *x = !*x)>"Correct"</button>
                    <button class="text-red-600 hover:underline" on:click=move |_| send(json!({"status": "retired"}))>"No longer true"</button>
                </div>
            })}
            {move || fixing.get().then(|| view! {
                <div class="flex gap-2">
                    <input class=format!("{INPUT} min-w-0 flex-1 py-1 text-sm") prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev)) />
                    <button class=BUTTON on:click=move |_| send(json!({"claim": text.get_untracked()}))>"Save"</button>
                </div>
            })}
        </li>
    }
}

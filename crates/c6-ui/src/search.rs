//! Session search: the header box and the results page.

use c6_api::{SearchHit, SearchResult, SnippetPart};
use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_query_map};

use crate::api;
use crate::util::{ErrorBox, INPUT, ago, now_ms};

const SELECT: &str = "rounded-md border border-zinc-300 bg-white px-2 py-1.5 text-sm dark:border-zinc-700 dark:bg-zinc-900";

/// Entry kinds, with labels, for the filter and the hit badges.
const KINDS: [(&str, &str); 7] = [
    ("user", "your message"),
    ("assistant", "agent reply"),
    ("tool_call", "tool call"),
    ("tool_result", "tool output"),
    ("thinking", "thinking"),
    ("error", "error"),
    ("summary", "summary"),
];

fn kind_label(kind: &str) -> &str {
    KINDS.iter().find(|(k, _)| *k == kind).map_or(kind, |(_, l)| l)
}

fn enc(s: &str) -> String {
    js_sys::encode_uri_component(s).as_string().unwrap_or_default()
}

/// The search terms of a query, as the head reads them (for highlighting).
pub fn terms(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = q.trim();
    while !rest.is_empty() {
        let negate = rest.starts_with('-');
        if negate {
            rest = &rest[1..];
        }
        let (term, next) = match rest.strip_prefix('"') {
            Some(r) => r.split_once('"').unwrap_or((r, "")),
            None => rest.split_once(char::is_whitespace).unwrap_or((rest, "")),
        };
        if !negate && term.trim().chars().count() >= 3 {
            out.push(term.trim().to_owned());
        }
        rest = next.trim_start();
    }
    out
}

/// The search box in the header. ⌘K / Ctrl+K focuses it.
#[component]
pub fn SearchBox() -> impl IntoView {
    let navigate = use_navigate();
    let text = RwSignal::new(String::new());
    let input = NodeRef::<leptos::html::Input>::new();
    let _hotkey = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "k" && (ev.meta_key() || ev.ctrl_key()) {
            ev.prevent_default();
            if let Some(el) = input.get_untracked() {
                let _ = el.focus();
                el.select();
            }
        }
    });
    view! {
        <input
            node_ref=input
            type="search"
            class="w-44 rounded-md border border-zinc-300 bg-white px-2 py-1 text-xs focus:w-64 dark:border-zinc-700 dark:bg-zinc-900 transition-[width]"
            placeholder="Search sessions (⌘K)"
            prop:value=move || text.get()
            on:input=move |ev| text.set(event_target_value(&ev))
            on:keydown=move |ev| {
                if ev.key() == "Enter" {
                    let q = text.get_untracked();
                    if !q.trim().is_empty() {
                        navigate(&format!("/search?q={}", enc(q.trim())), Default::default());
                    }
                }
            }
        />
    }
}

/// `/search?q=…&kind=…&worker=…`
#[component]
pub fn SearchPage() -> impl IntoView {
    let query = use_query_map();
    let navigate = use_navigate();
    let live = api::live();
    let q = move || query.read().get("q").unwrap_or_default();
    let kind = move || query.read().get("kind").unwrap_or_default();
    let worker = move || query.read().get("worker").unwrap_or_default();
    let text = RwSignal::new(q());
    Effect::new(move || text.set(q()));
    let results = LocalResource::new(move || {
        let (q, kind, worker) = (q(), kind(), worker());
        async move {
            if q.trim().is_empty() {
                return Ok(None);
            }
            api::search(q, kind, worker).await.map(Some)
        }
    });
    let go = {
        let navigate = navigate.clone();
        move |q: String, kind: String, worker: String| {
            let mut url = format!("/search?q={}", enc(q.trim()));
            if !kind.is_empty() {
                url.push_str(&format!("&kind={}", enc(&kind)));
            }
            if !worker.is_empty() {
                url.push_str(&format!("&worker={}", enc(&worker)));
            }
            navigate(&url, Default::default());
        }
    };
    let (go1, go2, go3) = (go.clone(), go.clone(), go);
    view! {
        <main class="mx-auto max-w-4xl space-y-4 px-4 py-6">
            <div class="flex flex-wrap items-center gap-2">
                <input
                    type="search"
                    class=format!("{INPUT} min-w-64 flex-1")
                    placeholder="Words or \"a phrase\" (3+ characters each); -word excludes"
                    prop:value=move || text.get()
                    on:input=move |ev| text.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Enter" { go1(text.get_untracked(), kind(), worker()) }
                />
                <select class=SELECT on:change=move |ev| go2(q(), event_target_value(&ev), worker())>
                    <option value="" selected=move || kind().is_empty()>"Everything"</option>
                    {KINDS.iter().map(|&(k, l)| view! { <option value=k selected=move || kind() == k>{l}</option> }).collect_view()}
                </select>
                <select class=SELECT on:change=move |ev| go3(q(), kind(), event_target_value(&ev))>
                    <option value="" selected=move || worker().is_empty()>"All workers"</option>
                    {move || live.workers.get().into_iter().map(|w| {
                        let id = w.id.to_string();
                        let sel = id.clone();
                        view! { <option value=id selected=move || worker() == sel>{w.name}</option> }
                    }).collect_view()}
                </select>
            </div>
            <Suspense fallback=|| view! { <p class="text-sm text-zinc-500">"Searching…"</p> }>
                {move || results.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(None) => view! { <p class="text-sm text-zinc-500">"Search every message, tool call and tool output in your sessions."</p> }.into_any(),
                    Ok(Some(r)) if r.results.is_empty() => view! { <p class="text-sm text-zinc-500">"No matches."</p> }.into_any(),
                    Ok(Some(r)) => {
                        let q = q();
                        view! {
                            <div class="space-y-3">
                                {r.results.into_iter().map(|res| view! { <ResultCard res q=q.clone() /> }).collect_view()}
                                {r.truncated.then(|| view! { <p class="text-xs text-zinc-500">"Showing the best matches; add words to narrow the search."</p> })}
                            </div>
                        }.into_any()
                    }
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn ResultCard(res: SearchResult, q: String) -> impl IntoView {
    let s = res.session;
    let more = res.total_hits as usize - res.hits.len().min(res.total_hits as usize);
    let badge = if s.parent_call_id.is_some() {
        Some("subagent")
    } else if s.fork_seq.is_some() {
        Some("side chat")
    } else {
        None
    };
    // Side chats open inside their top-level session.
    let (page, open) = match &res.open_in {
        Some(o) => (o.session_id.clone(), Some(o.side_chats.join(","))),
        None => (s.id.clone(), None),
    };
    let base = format!("/sessions/{page}?q={}{}", enc(&q), open.as_ref().map(|o| format!("&open={}", enc(o))).unwrap_or_default());
    let title_href = base.clone();
    let parent = res.open_in.as_ref().map(|o| o.title.clone());
    view! {
        <div class="rounded-lg border border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-900">
            <div class="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-0.5 border-b border-zinc-100 px-4 py-2 dark:border-zinc-800">
                <div class="min-w-0 truncate">
                    <a class="font-medium hover:underline" href=title_href>{s.title.clone()}</a>
                    {parent.map(|p| view! { <span class="ml-2 text-xs text-zinc-500">{format!("in “{p}”")}</span> })}
                </div>
                <div class="flex shrink-0 items-center gap-2 text-xs text-zinc-500">
                    {badge.map(|b| view! { <span class="rounded bg-sky-100 px-1.5 text-sky-800 dark:bg-sky-950 dark:text-sky-300">{b}</span> })}
                    <span>{res.worker_name.clone()}</span>
                    <span class="font-mono">{s.model.clone()}</span>
                    <span>{ago(now_ms() - s.updated_at)}</span>
                </div>
            </div>
            <div class="divide-y divide-zinc-100 dark:divide-zinc-800">
                {res.hits.into_iter().map(|h| view! { <HitRow h base=base.clone() /> }).collect_view()}
            </div>
            {(more > 0).then(|| view! { <p class="px-4 py-1.5 text-xs text-zinc-500">{format!("{more} more match{} in this session", if more == 1 { "" } else { "es" })}</p> })}
        </div>
    }
}

#[component]
fn HitRow(h: SearchHit, base: String) -> impl IntoView {
    let href = format!("{base}#{}", h.anchor);
    view! {
        <a class="block px-4 py-2 text-sm hover:bg-zinc-50 dark:hover:bg-zinc-800/60" href=href>
            <span class="mr-2 inline-block rounded bg-zinc-100 px-1.5 text-[11px] text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400">{kind_label(&h.kind).to_owned()}</span>
            <span class=if matches!(h.kind.as_str(), "tool_call" | "tool_result") { "font-mono text-xs text-zinc-700 dark:text-zinc-300" } else { "text-zinc-700 dark:text-zinc-300" }>
                {snippet(h.snippet)}
            </span>
        </a>
    }
}

fn snippet(parts: Vec<SnippetPart>) -> impl IntoView {
    parts
        .into_iter()
        .map(|p| {
            if p.hit {
                view! { <mark class="rounded bg-amber-200 px-0.5 text-inherit dark:bg-amber-700/60">{p.text}</mark> }.into_any()
            } else {
                view! { <span>{p.text}</span> }.into_any()
            }
        })
        .collect_view()
}

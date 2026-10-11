//! Session search: the header box and the results page.

use c6_api::{SearchHit, SearchResult, SnippetPart};
use leptos::prelude::*;

use crate::api;
use crate::util::{ErrorBox, INPUT, ago, now_ms};
use crate::workspace::{Go, How, Target, View, use_column, use_workspace};

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
    let ws = use_workspace();
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
                        // In the focused tile (reusing its search column, if any).
                        ws.open(None, View::Search { q: q.trim().to_owned(), kind: String::new(), worker: String::new() }, How::Beside);
                    }
                }
            }
        />
    }
}

/// A search, in a workspace column; changing it updates the column.
#[component]
pub fn SearchPage(q: String, kind: String, worker: String) -> impl IntoView {
    let live = api::live();
    let col = use_column();
    let (q0, kind0, worker0) = (StoredValue::new(q), StoredValue::new(kind), StoredValue::new(worker));
    let q = move || q0.get_value();
    let kind = move || kind0.get_value();
    let worker = move || worker0.get_value();
    let text = RwSignal::new(q());
    // The shared label filter (as in the session list and inbox).
    let filter = crate::labels::use_filter().0;
    let results = LocalResource::new(move || {
        let (q, kind, worker, labels) = (q(), kind(), worker(), filter.get());
        async move {
            if q.trim().is_empty() {
                return Ok(None);
            }
            api::search(q, kind, worker, labels).await.map(Some)
        }
    });
    let go = move |q: String, kind: String, worker: String| {
        if let Some(c) = col {
            c.replace(View::Search { q: q.trim().to_owned(), kind, worker });
        }
    };
    let (go1, go2, go3) = (go, go, go);
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
            <crate::labels::LabelFilterBar />
            <Suspense fallback=|| view! { <p class="text-sm text-zinc-500">"Searching…"</p> }>
                {move || results.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(None) => view! { <p class="text-sm text-zinc-500">"Search every message, tool call and tool output in your sessions."</p> }.into_any(),
                    Ok(Some(r)) if r.results.is_empty() => view! {
                        <Ignored words=r.ignored.clone() />
                        <p class="text-sm text-zinc-500">{move || if filter.with(Vec::is_empty) { "No matches." } else { "No matches in sessions with these labels." }}</p>
                    }.into_any(),
                    Ok(Some(r)) => {
                        let q = q();
                        view! {
                            <div class="space-y-3">
                                <Ignored words=r.ignored.clone() />
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

/// A note about words too short to search for.
#[component]
fn Ignored(words: Vec<String>) -> impl IntoView {
    (!words.is_empty()).then(|| {
        let list = words.iter().map(|w| format!("“{w}”")).collect::<Vec<_>>().join(", ");
        let plural = words.len() > 1;
        view! {
            <p class="rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-sm text-amber-800 dark:border-amber-900 dark:bg-amber-950 dark:text-amber-200">
                {format!("Ignored {list}: {} shorter than 3 characters. To include {}, put {} in a \"quoted phrase\" with a longer word, like \"macOS 15\".",
                    if plural { "words are" } else { "it is" },
                    if plural { "them" } else { "it" },
                    if plural { "them" } else { "it" })}
            </p>
        }
    })
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
        Some(o) => (o.session_id.clone(), o.side_chats.clone()),
        None => (s.id.clone(), Vec::new()),
    };
    let at =
        move |anchor: Option<String>| View::Session { id: page.clone(), target: Some(Target { q: q.clone(), anchor, open: open.clone() }) };
    let title_view = at(None);
    let parent = res.open_in.as_ref().map(|o| o.title.clone());
    view! {
        <div class="rounded-lg border border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-900">
            <div class="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-0.5 border-b border-zinc-100 px-4 py-2 dark:border-zinc-800">
                <div class="min-w-0 truncate">
                    <Go to=title_view class="font-medium hover:underline" title="Open beside the results (⌘-click: new tile)">{s.title.clone()}</Go>
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
                {res.hits.into_iter().map(|h| { let to = at(Some(h.anchor.clone())); view! { <HitRow h to /> } }).collect_view()}
            </div>
            {(more > 0).then(|| view! { <p class="px-4 py-1.5 text-xs text-zinc-500">{format!("{more} more match{} in this session", if more == 1 { "" } else { "es" })}</p> })}
        </div>
    }
}

#[component]
fn HitRow(h: SearchHit, to: View) -> impl IntoView {
    view! {
        <Go to class="block px-4 py-2 text-sm hover:bg-zinc-50 dark:hover:bg-zinc-800/60" title="Open beside the results (⌘-click: new tile)">
            <span class="mr-2 inline-block rounded bg-zinc-100 px-1.5 text-[11px] text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400">{kind_label(&h.kind).to_owned()}</span>
            <span class=if matches!(h.kind.as_str(), "tool_call" | "tool_result") { "font-mono text-xs text-zinc-700 dark:text-zinc-300" } else { "text-zinc-700 dark:text-zinc-300" }>
                {snippet(h.snippet)}
            </span>
        </Go>
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

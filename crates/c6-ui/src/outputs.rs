//! A session's outputs: deliverables the agent published (publish_file),
//! previewed by type, with their versions.

use c6_api::{Event, EventKind};
use leptos::prelude::*;

use crate::util::BUTTON_QUIET;

#[derive(Clone, PartialEq)]
struct Version {
    seq: u64,
    title: String,
    mime: String,
    size: u64,
}

/// Outputs by name, in the order first published; versions oldest first.
fn outputs(events: &[Event]) -> Vec<(String, Vec<Version>)> {
    let mut out: Vec<(String, Vec<Version>)> = Vec::new();
    for e in events {
        if let EventKind::Output { name, title, mime, size } = &e.kind {
            let v = Version { seq: e.seq, title: title.clone(), mime: mime.clone(), size: *size };
            match out.iter_mut().find(|(n, _)| n == name) {
                Some((_, vs)) => vs.push(v),
                None => out.push((name.clone(), vec![v])),
            }
        }
    }
    out
}

pub fn count(events: &[Event]) -> usize {
    outputs(events).len()
}

fn url(session: &str, seq: u64) -> String {
    format!("/api/v1/sessions/{session}/outputs/{seq}")
}

fn size(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{} KB", n >> 10),
        n => format!("{n} B"),
    }
}

/// The Outputs tab.
#[component]
pub fn Outputs(session: String, events: RwSignal<Vec<Event>>) -> impl IntoView {
    let session = StoredValue::new(session);
    let list = Memo::new(move |_| events.with(|e| outputs(e)));
    // The chosen output (by name) and version (by seq); default the newest.
    let chosen = RwSignal::new(None::<String>);
    let version = RwSignal::new(None::<u64>);
    let current = Memo::new(move |_| {
        let l = list.get();
        let name = chosen.get().filter(|n| l.iter().any(|(x, _)| x == n)).or_else(|| l.last().map(|(n, _)| n.clone()))?;
        let versions = l.into_iter().find(|(n, _)| *n == name)?.1;
        let v = version.get().and_then(|s| versions.iter().find(|v| v.seq == s).cloned()).or_else(|| versions.last().cloned())?;
        Some((name, versions, v))
    });
    view! {
        <div class="grid gap-4 py-2 md:grid-cols-[14rem_1fr]">
            <div class="space-y-1">
                {move || {
                    let l = list.get();
                    if l.is_empty() {
                        return view! { <p class="text-sm text-zinc-500">"Nothing published yet. Deliverables the agent publishes (reports, decks, charts, documents) appear here."</p> }.into_any();
                    }
                    l.into_iter().rev().map(|(name, vs)| {
                        let latest = vs.last().cloned().expect("a version");
                        let n = name.clone();
                        let active = move || current.with(|c| c.as_ref().is_some_and(|(x, _, _)| *x == n));
                        let pick = name.clone();
                        view! {
                            <button class=move || format!("block w-full rounded-md px-2 py-1.5 text-left text-sm {}", if active() { "bg-zinc-200 dark:bg-zinc-800" } else { "hover:bg-zinc-100 dark:hover:bg-zinc-900" })
                                on:click=move |_| { chosen.set(Some(pick.clone())); version.set(None); }>
                                <div class="truncate font-medium">{latest.title.clone()}</div>
                                <div class="truncate font-mono text-[11px] text-zinc-500">{format!("{name} · v{} · {}", vs.len(), size(latest.size))}</div>
                            </button>
                        }
                    }).collect_view().into_any()
                }}
            </div>
            {move || current.get().map(|(name, versions, v)| {
                let u = url(&session.get_value(), v.seq);
                let n = versions.len();
                view! {
                    <div class="min-w-0 space-y-2">
                        <div class="flex flex-wrap items-center gap-2">
                            <h3 class="min-w-0 flex-1 truncate font-medium">{v.title.clone()}<span class="ml-2 font-mono text-xs text-zinc-500">{name.clone()}</span></h3>
                            {(n > 1).then(|| view! {
                                <select class="rounded-md border border-zinc-300 bg-white px-1 py-0.5 text-xs dark:border-zinc-700 dark:bg-zinc-900"
                                    prop:value=v.seq.to_string() on:change=move |ev| version.set(event_target_value(&ev).parse().ok())>
                                    {versions.iter().enumerate().rev().map(|(i, x)| view! { <option value=x.seq.to_string()>{format!("v{}{}", i + 1, if i + 1 == n { " (latest)" } else { "" })}</option> }).collect_view()}
                                </select>
                            })}
                            <a class=format!("{BUTTON_QUIET} text-xs") href=u.clone() target="_blank" rel="noopener">"Open ↗"</a>
                            <a class=format!("{BUTTON_QUIET} text-xs") href=format!("{u}?download=1")>"Download"</a>
                        </div>
                        <Preview url=u mime=v.mime.clone() />
                    </div>
                }
            })}
        </div>
    }
}

/// An output shown by its type.
#[component]
fn Preview(url: String, mime: String) -> impl IntoView {
    let frame = "h-[70vh] w-full rounded-md border border-zinc-200 bg-white dark:border-zinc-800";
    if mime == "text/html" || mime == "image/svg+xml" {
        // Its own origin (the head sends a sandbox policy too).
        view! { <iframe class=frame src=url sandbox="allow-scripts allow-popups allow-forms allow-modals"></iframe> }.into_any()
    } else if mime == "application/pdf" {
        view! { <iframe class=frame src=url></iframe> }.into_any()
    } else if mime.starts_with("image/") {
        view! { <img class="max-h-[70vh] max-w-full rounded-md border border-zinc-200 dark:border-zinc-800" src=url /> }.into_any()
    } else if mime.starts_with("text/") || mime == "application/json" {
        let text = LocalResource::new(move || {
            let url = url.clone();
            async move { gloo_net::http::Request::get(&url).send().await.map_err(|e| e.to_string())?.text().await.map_err(|e| e.to_string()) }
        });
        let markdown = mime == "text/markdown";
        view! {
            {move || text.get().map(|r| match r {
                Ok(t) if markdown => view! { <div class="md max-h-[70vh] overflow-auto rounded-md border border-zinc-200 p-4 dark:border-zinc-800" inner_html=crate::markdown::to_html(&t)></div> }.into_any(),
                Ok(t) => view! { <pre class="max-h-[70vh] overflow-auto rounded-md border border-zinc-200 p-3 font-mono text-xs dark:border-zinc-800">{t}</pre> }.into_any(),
                Err(e) => view! { <p class="text-sm text-red-600">{e}</p> }.into_any(),
            })}
        }
        .into_any()
    } else {
        view! { <p class="rounded-md border border-dashed border-zinc-300 p-6 text-center text-sm text-zinc-500 dark:border-zinc-700">"No preview for this kind of file: download it."</p> }.into_any()
    }
}

/// A publish, as a line in the transcript; an image is shown too.
pub fn published_line(session: &str, seq: u64, name: &str, title: &str, mime: &str, size_bytes: u64) -> AnyView {
    let u = url(session, seq);
    let image = mime.starts_with("image/");
    view! {
        <div class="space-y-1">
            <p class="flex flex-wrap items-center gap-2 text-xs text-zinc-500">
                <span>"📦"</span>
                <span>"Published"</span>
                <span class="font-medium text-zinc-700 dark:text-zinc-300">{title.to_owned()}</span>
                <span class="font-mono">{format!("{name} · {}", size(size_bytes))}</span>
                <a class="underline" href=u.clone() target="_blank" rel="noopener">"open"</a>
                <span>"· see Outputs"</span>
            </p>
            {image.then(|| view! {
                <a href=u.clone() target="_blank" rel="noopener" title="Open full size">
                    <img class="max-h-96 max-w-full rounded-md border border-zinc-200 dark:border-zinc-800" src=u.clone() alt=title.to_owned() loading="lazy" />
                </a>
            })}
        </div>
    }
    .into_any()
}

//! The Memex page: search your knowledge store, read items and things
//! (labels as nodes), write notes. And the memex's settings.

use c6_api::{MemexHit, MemexStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Value;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, ago, now_ms};
use crate::workspace::{Go, View};

const KINDS: [(&str, &str); 8] = [
    ("session", "Sessions"),
    ("note", "Notes"),
    ("web", "Web pages"),
    ("output", "Outputs"),
    ("document", "Documents"),
    ("paper", "Papers"),
    ("email", "Email"),
    ("photo", "Photos"),
];

fn kind_tone(kind: &str) -> &'static str {
    match kind {
        "session" => "bg-sky-100 text-sky-800 dark:bg-sky-950 dark:text-sky-300",
        "note" => "bg-amber-100 text-amber-900 dark:bg-amber-950 dark:text-amber-200",
        "web" => "bg-emerald-100 text-emerald-800 dark:bg-emerald-950 dark:text-emerald-300",
        "output" => "bg-violet-100 text-violet-800 dark:bg-violet-950 dark:text-violet-300",
        _ => "bg-zinc-100 text-zinc-700 dark:bg-zinc-800 dark:text-zinc-300",
    }
}

fn when(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    String::from(d.to_locale_date_string("default", &wasm_bindgen::JsValue::UNDEFINED))
}

#[component]
pub fn MemexPage() -> impl IntoView {
    let status = LocalResource::new(api::memex_status);
    let query = RwSignal::new(String::new());
    let kinds = RwSignal::new(Vec::<String>::new());
    // What was searched (on Enter or a filter change).
    let asked = RwSignal::new(None::<(String, Vec<String>)>);
    let results = LocalResource::new(move || {
        let a = asked.get();
        async move {
            match a {
                Some((q, k)) if !q.trim().is_empty() => api::memex_search(q, k).await.map(Some),
                _ => Ok(None),
            }
        }
    });
    // An item or thing open below the results.
    let open = RwSignal::new(None::<String>);
    let search = move || {
        open.set(None);
        asked.set(Some((query.get_untracked(), kinds.get_untracked())));
    };
    view! {
        <main class="mx-auto max-w-4xl space-y-4 px-4 py-4">
            <Suspense fallback=|| ()>
                {move || status.get().map(|r| match r {
                    Ok(s) => view! { <StatusLine s /> }.into_any(),
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                })}
            </Suspense>
            <div class="flex gap-2">
                <input class=format!("{INPUT} min-w-0 flex-1") placeholder="Search everything: past sessions, notes, pages read, outputs…"
                    prop:value=move || query.get() on:input=move |ev| query.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Enter" { search() } />
                <button class=BUTTON on:click=move |_| search()>"Search"</button>
            </div>
            <div class="flex flex-wrap gap-1 text-xs">
                {KINDS.iter().map(|(k, label)| {
                    let k = k.to_string();
                    let k2 = k.clone();
                    let on = move || kinds.with(|v| v.contains(&k));
                    view! {
                        <button class=move || format!("rounded-full border px-2 py-0.5 {}", if on() { "border-zinc-900 bg-zinc-900 text-white dark:border-zinc-100 dark:bg-zinc-100 dark:text-zinc-900" } else { "border-zinc-300 text-zinc-600 dark:border-zinc-700 dark:text-zinc-400" })
                            on:click=move |_| {
                                kinds.update(|v| if let Some(i) = v.iter().position(|x| *x == k2) { v.remove(i); } else { v.push(k2.clone()); });
                                if asked.with_untracked(Option::is_some) { search(); }
                            }>{*label}</button>
                    }
                }).collect_view()}
            </div>
            <Suspense fallback=|| view! { <p class="text-sm text-zinc-500">"Searching…"</p> }>
                {move || results.get().map(|r| match r {
                    Ok(None) => view! { <NoteBox /> }.into_any(),
                    Ok(Some(hits)) if hits.is_empty() => view! { <p class="text-sm text-zinc-500">"Nothing matches."</p> }.into_any(),
                    Ok(Some(hits)) => view! {
                        <div class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                            {hits.into_iter().map(|h| view! { <Hit h open /> }).collect_view()}
                        </div>
                    }.into_any(),
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                })}
            </Suspense>
            {move || open.get().map(|id| view! { <Detail id open /> })}
        </main>
    }
}

#[component]
fn StatusLine(s: MemexStatus) -> impl IntoView {
    if !s.connected {
        return view! {
            <div class="rounded-md border border-amber-300 bg-amber-50 p-3 text-sm text-amber-900 dark:border-amber-900 dark:bg-amber-950 dark:text-amber-200">
                "No memex is connected. Set one up in "
                <Go to=View::Settings { note: None } class="underline">"Settings → Memex"</Go>
                {(s.outbox > 0).then(|| format!(" ({} updates are waiting for it).", s.outbox))}
            </div>
        }
        .into_any();
    }
    let total: u64 = s.items.values().sum();
    let parts: Vec<String> = s.items.iter().map(|(k, n)| format!("{n} {k}")).collect();
    let meaning = if s.embedding_model.is_empty() {
        "keyword search only (choose an embedding model in Settings → Memex)".to_owned()
    } else {
        format!("{} of {} passages embedded with {}", s.embedded, s.passages, s.embedding_model)
    };
    view! {
        <div class="space-y-1 text-xs text-zinc-500">
            <p>{format!("{} · {total} items ({}) · {} things · {meaning}", s.node.clone().unwrap_or_default(), parts.join(", "), s.things)}</p>
            {s.last_error.clone().map(|e| view! { <p class="text-red-600">{e}</p> })}
        </div>
    }
    .into_any()
}

#[component]
fn Hit(h: MemexHit, open: RwSignal<Option<String>>) -> impl IntoView {
    let id = h.id.clone();
    let session = (h.kind == "session" && h.source == "carbon6").then(|| h.external_id.clone());
    view! {
        <div class="space-y-1 px-3 py-2">
            <div class="flex flex-wrap items-center gap-2">
                <span class=format!("rounded px-1.5 text-[11px] {}", kind_tone(&h.kind))>{h.kind.clone()}</span>
                <button class="text-left font-medium hover:underline" on:click=move |_| open.set(Some(id.clone()))>{h.title.clone()}</button>
                <span class="text-xs text-zinc-500">{when(h.created_at)}</span>
                {h.matched.iter().map(|m| view! { <span class="text-[10px] uppercase tracking-wide text-zinc-400">{m.clone()}</span> }).collect_view()}
                <span class="flex-1"></span>
                {session.map(|sid| view! { <Go to=View::session(sid) class="text-xs text-sky-700 hover:underline dark:text-sky-400">"open session ↗"</Go> })}
                {h.url.clone().map(|u| view! { <a href=u.clone() target="_blank" rel="noopener noreferrer" class="text-xs text-sky-700 hover:underline dark:text-sky-400">"page ↗"</a> })}
            </div>
            <p class="text-sm text-zinc-600 dark:text-zinc-400">{h.snippet.clone()}</p>
        </div>
    }
}

/// An item in full, or a thing with its notes, items and related things.
#[component]
fn Detail(id: String, open: RwSignal<Option<String>>) -> impl IntoView {
    let data = LocalResource::new(move || api::memex_get(id.clone()));
    let error = RwSignal::new(None::<String>);
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <div class="flex justify-end"><button class=BUTTON_QUIET on:click=move |_| open.set(None)>"Close"</button></div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || data.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) if v.get("item").is_some() => item_view(v["item"].clone(), open, error).into_any(),
                Ok(v) => thing_view(v["thing"].clone(), open).into_any(),
            })}
        </section>
    }
}

fn chips(about: Vec<Value>, open: RwSignal<Option<String>>) -> impl IntoView {
    about
        .into_iter()
        .map(|t| {
            let label = t["label"].as_str().unwrap_or_default().to_owned();
            let l2 = label.clone();
            view! { <button class="rounded bg-zinc-100 px-1.5 text-[11px] hover:underline dark:bg-zinc-800" on:click=move |_| open.set(Some(l2.clone()))>{label}</button> }
        })
        .collect_view()
}

fn item_view(it: Value, open: RwSignal<Option<String>>, error: RwSignal<Option<String>>) -> impl IntoView {
    let s = |k: &str| it[k].as_str().unwrap_or_default().to_owned();
    let id = s("id");
    let about = it["about"].as_array().cloned().unwrap_or_default();
    let remove = move |_| {
        if !window().confirm_with_message("Delete this from the memex? (The original, if any, is not touched.)").unwrap_or(false) {
            return;
        }
        let id = id.clone();
        spawn_local(async move {
            match api::memex_delete(id).await {
                Ok(()) => open.set(None),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <div class="flex flex-wrap items-center gap-2">
            <span class=format!("rounded px-1.5 text-[11px] {}", kind_tone(&s("kind")))>{s("kind")}</span>
            <h2 class="font-medium">{s("title")}</h2>
            <span class="text-xs text-zinc-500">{format!("{} · from {}", when(it["created_at"].as_i64().unwrap_or(0)), s("source"))}</span>
            <span class="flex-1"></span>
            <button class="text-xs text-red-600 hover:underline" on:click=remove>"Delete"</button>
        </div>
        <div class="flex flex-wrap gap-1">{chips(about, open)}</div>
        <pre class="max-h-[32rem] overflow-auto whitespace-pre-wrap rounded-md bg-zinc-50 p-3 text-sm dark:bg-zinc-900">{s("text")}</pre>
    }
}

fn thing_view(t: Value, open: RwSignal<Option<String>>) -> impl IntoView {
    let thing = &t["thing"];
    let list = |key: &str| -> Vec<MemexHit> { serde_json::from_value(t[key].clone()).unwrap_or_default() };
    let (notes, items) = (list("notes"), list("items"));
    let related = t["related"].as_array().cloned().unwrap_or_default();
    let rows = move |hits: Vec<MemexHit>| {
        hits.into_iter()
            .map(|h| {
                let id = h.id.clone();
                view! {
                    <li class="text-sm">
                        <span class=format!("mr-1 rounded px-1 text-[10px] {}", kind_tone(&h.kind))>{h.kind.clone()}</span>
                        <button class="hover:underline" on:click=move |_| open.set(Some(id.clone()))>{h.title.clone()}</button>
                        <span class="ml-1 text-xs text-zinc-500">{ago(now_ms() - h.created_at)}" ago"</span>
                    </li>
                }
            })
            .collect_view()
    };
    view! {
        <div class="flex items-center gap-2">
            <span class="rounded bg-violet-100 px-1.5 text-[11px] text-violet-800 dark:bg-violet-950 dark:text-violet-300">{thing["kind"].as_str().unwrap_or_default().to_owned()}</span>
            <h2 class="text-lg font-semibold">{thing["name"].as_str().unwrap_or_default().to_owned()}</h2>
            <span class="font-mono text-xs text-zinc-500">{thing["label"].as_str().unwrap_or_default().to_owned()}</span>
        </div>
        {(!notes.is_empty()).then(|| view! { <h3 class="text-xs font-medium uppercase text-zinc-500">"Notes"</h3><ul class="space-y-1">{rows(notes.clone())}</ul> })}
        {(!items.is_empty()).then(|| view! { <h3 class="text-xs font-medium uppercase text-zinc-500">"Items"</h3><ul class="space-y-1">{rows(items.clone())}</ul> })}
        {(!related.is_empty()).then(|| view! {
            <h3 class="text-xs font-medium uppercase text-zinc-500">"Related"</h3>
            <div class="flex flex-wrap gap-1">{chips(related.iter().map(|p| p[0].clone()).collect::<Vec<_>>(), open)}</div>
        })}
    }
}

/// Write a note (shown before any search).
#[component]
fn NoteBox() -> impl IntoView {
    let (text, about) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let status = RwSignal::new(None::<Result<String, String>>);
    let save = move |_| {
        let t = text.get_untracked();
        if t.trim().is_empty() {
            return;
        }
        let labels = c6_api::labels::parse_labels(&about.get_untracked());
        spawn_local(async move {
            match api::memex_note(t, labels).await {
                Ok(_) => {
                    text.set(String::new());
                    status.set(Some(Ok("Saved to the memex.".into())));
                }
                Err(e) => status.set(Some(Err(e))),
            }
        });
    };
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
            <h2 class="text-sm font-medium">"Add a note"</h2>
            <textarea class=format!("{INPUT} min-h-20") placeholder="A fact, decision or insight worth keeping…" prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))></textarea>
            <div class="flex flex-wrap items-center gap-2">
                <input class=format!("{INPUT} min-w-0 flex-1 font-mono text-xs") placeholder="about (labels): e.g. client:acme, host:web1 — empty: every session"
                    prop:value=move || about.get() on:input=move |ev| about.set(event_target_value(&ev)) />
                <button class=BUTTON on:click=save>"Save"</button>
            </div>
            {move || status.get().map(|r| match r {
                Ok(m) => view! { <p class="text-xs text-emerald-600">{m}</p> }.into_any(),
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
            })}
        </section>
    }
}

/// Settings → Memex: connecting one, its embedding model, the brief size.
#[component]
pub fn MemexSettings() -> impl IntoView {
    let status = LocalResource::new(api::memex_status);
    let loaded = LocalResource::new(move || api::get_setting("memex"));
    let connections = LocalResource::new(api::connections);
    let (conn, model, chars) = (RwSignal::new(String::new()), RwSignal::new(String::new()), RwSignal::new(String::new()));
    Effect::new(move || {
        if let Some(Ok(v)) = loaded.get() {
            conn.set(v["embedding"]["connection"].as_str().unwrap_or_default().to_owned());
            model.set(v["embedding"]["model"].as_str().unwrap_or_default().to_owned());
            chars.set(v["brief_chars"].as_u64().map(|n| n.to_string()).unwrap_or_default());
        }
    });
    let token = RwSignal::new(None::<String>);
    let note = RwSignal::new(None::<Result<String, String>>);
    let make_token = move |_| {
        spawn_local(async move {
            match api::create_enrollment(Some("memex".into())).await {
                Ok(e) => token.set(Some(e.token)),
                Err(e) => note.set(Some(Err(e))),
            }
        });
    };
    let save = move |_| {
        let embedding = (!conn.get_untracked().is_empty())
            .then(|| serde_json::json!({"connection": conn.get_untracked(), "model": model.get_untracked().trim()}));
        let mut v = serde_json::json!({});
        if let Some(e) = embedding {
            v["embedding"] = e;
        }
        if let Ok(n) = chars.get_untracked().trim().parse::<u32>() {
            v["brief_chars"] = n.into();
        }
        spawn_local(async move {
            note.set(Some(
                api::put_setting("memex", v)
                    .await
                    .map(|_| "Saved. The memex picks it up within a minute; new passages are embedded as they come.".to_owned()),
            ));
        });
    };
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <div>
                <h2 class="font-medium">"Memex"</h2>
                <p class="text-sm text-zinc-500">"Your knowledge store, a separate service (c6m) on the fabric that can run on any machine. carbon6 sends it what your sessions do (digests, outputs, pages read, memory notes); sessions start with a brief from it about their labels, and agents can search it and save notes. What a session may see is set by scope rules below."</p>
            </div>
            {move || status.get().and_then(Result::ok).map(|s| if s.connected {
                view! { <p class="text-sm text-emerald-700 dark:text-emerald-400">{format!("Connected: {} · {} items", s.node.clone().unwrap_or_default(), s.items.values().sum::<u64>())}</p> }.into_any()
            } else {
                view! {
                    <div class="space-y-2 text-sm">
                        <p>"Not connected. On the machine that should hold it (this one is fine), with c6m installed:"</p>
                        <button class=BUTTON_QUIET on:click=make_token>"Create a token"</button>
                        {move || token.get().map(|t| view! {
                            <pre class="select-all overflow-x-auto whitespace-pre-wrap break-all rounded-md bg-zinc-100 p-2 font-mono text-xs dark:bg-zinc-900">{format!("c6m enroll {t}\nc6m run")}</pre>
                        })}
                    </div>
                }.into_any()
            })}
            <div class="grid gap-2 sm:grid-cols-3">
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Embedding connection"</span>
                    <select class=format!("{INPUT} py-1 text-sm") prop:value=move || conn.get() on:change=move |ev| conn.set(event_target_value(&ev))>
                        <option value="">"None (keyword search only)"</option>
                        {move || connections.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|c| c.kind != "anthropic" && c.kind != "claude_code").map(|c| view! { <option value=c.name.clone()>{c.name.clone()}</option> }).collect_view()}
                    </select>
                </label>
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Embedding model"</span>
                    <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="e.g. nomic-embed-text, text-embedding-3-small" prop:value=move || model.get() on:input=move |ev| model.set(event_target_value(&ev)) />
                </label>
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Brief size (characters)"</span>
                    <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="4000; 0 for none" prop:value=move || chars.get() on:input=move |ev| chars.set(event_target_value(&ev)) />
                </label>
            </div>
            <div class="flex items-center justify-end gap-3">
                {move || note.get().map(|r| match r {
                    Ok(m) => view! { <span class="text-sm text-emerald-600">{m}</span> }.into_any(),
                    Err(e) => view! { <span class="text-sm text-red-600">{e}</span> }.into_any(),
                })}
                <button class=BUTTON on:click=save>"Save"</button>
            </div>
        </section>
    }
}

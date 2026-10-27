//! What the memex understands: suggestions to review, commitments, and a
//! thing's graph and timeline.

use c6_api::{Commitment, MemexGraph, MemexReview};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT};

fn date(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    String::from(d.to_locale_date_string("default", &wasm_bindgen::JsValue::UNDEFINED))
}

/// Suggestions: people who may be the same, faces to name.
#[component]
pub fn Review(open: RwSignal<Option<String>>) -> impl IntoView {
    let tick = RwSignal::new(0u32);
    let list = LocalResource::new(move || {
        tick.track();
        api::rpc_as::<Vec<MemexReview>>("memex.review", json!({"limit": 60}))
    });
    let error = RwSignal::new(None::<String>);
    let decide = move |id: i64, accept: bool, name: Option<String>| {
        spawn_local(async move {
            match api::rpc("memex.review_decide", json!({"id": id, "accept": accept, "name": name})).await {
                Ok(_) => tick.update(|t| *t += 1),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <section class="space-y-3">
            <p class="text-sm text-zinc-500">"The memex never merges people or names faces on its own. Same names (an author on a paper and someone you write to, say) and new faces wait here for you."</p>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <Suspense fallback=|| ()>
                {move || list.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(v) if v.is_empty() => view! { <p class="text-sm text-zinc-500">"Nothing to review."</p> }.into_any(),
                    Ok(v) => view! {
                        <ul class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                            {v.into_iter().map(|r| view! { <ReviewRow r open decide /> }).collect_view()}
                        </ul>
                    }.into_any(),
                })}
            </Suspense>
        </section>
    }
}

#[component]
fn ReviewRow(
    r: MemexReview,
    open: RwSignal<Option<String>>,
    decide: impl Fn(i64, bool, Option<String>) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let id = r.id;
    let thing = |t: &c6_api::MemexThingRef| {
        let key = t.id.clone();
        view! {
            <button class="font-medium hover:underline" on:click=move |_| open.set(Some(key.clone()))>{t.name.clone()}</button>
            <span class="ml-1 font-mono text-[11px] text-zinc-500">{t.label.clone()}</span>
        }
    };
    let name = RwSignal::new(String::new());
    if r.kind == "face" {
        let picture = r.picture.clone().map(|p| crate::api::url(&format!("/api/v1/memex/{p}")));
        let guess = r.b.clone();
        return view! {
            <li class="flex flex-wrap items-center gap-3 px-3 py-2 text-sm">
                {picture.map(|src| view! { <img src=src class="h-16 w-16 rounded-md object-cover" alt="a face" /> })}
                <div class="min-w-0 flex-1 space-y-1">
                    <p class="text-zinc-500">{r.reason.clone()}</p>
                    {guess.as_ref().map(|g| view! { <p>"Maybe: " {thing(g)}</p> })}
                    <input class=format!("{INPUT} py-1 text-sm") placeholder="Who is this? (a name)" prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
                </div>
                {guess.is_some().then(|| view! { <button class=BUTTON_QUIET on:click=move |_| decide(id, true, None)>"Yes, it's them"</button> })}
                <button class=BUTTON on:click=move |_| decide(id, true, Some(name.get_untracked()))>"Name"</button>
                <button class=BUTTON_QUIET on:click=move |_| decide(id, false, None)>"Skip"</button>
            </li>
        }
        .into_any();
    }
    let b = r.b.clone();
    view! {
        <li class="flex flex-wrap items-center gap-2 px-3 py-2 text-sm">
            <span>{thing(&r.a)}</span>
            <span class="text-zinc-400">"and"</span>
            {b.as_ref().map(|b| view! { <span>{thing(b)}</span> })}
            <span class="text-xs text-zinc-500">{format!("({})", r.reason)}</span>
            <span class="flex-1"></span>
            <button class=BUTTON on:click=move |_| decide(id, true, None)>"Same — merge"</button>
            <button class=BUTTON_QUIET on:click=move |_| decide(id, false, None)>"Different"</button>
        </li>
    }
    .into_any()
}

/// Who owes what, read from mail and sessions.
#[component]
pub fn Commitments(open: RwSignal<Option<String>>) -> impl IntoView {
    let status = RwSignal::new("open".to_owned());
    let tick = RwSignal::new(0u32);
    let list = LocalResource::new(move || {
        tick.track();
        let s = status.get();
        async move { api::rpc_as::<Vec<Commitment>>("memex.commitments", json!({"status": s})).await }
    });
    let set = move |id: i64, to: &'static str| {
        spawn_local(async move {
            let _ = api::rpc("memex.commitment", json!({"id": id, "status": to})).await;
            tick.update(|t| *t += 1);
        });
    };
    view! {
        <section class="space-y-3">
            <div class="flex items-center gap-2 text-sm">
                <p class="flex-1 text-zinc-500">"Promises and requests in your mail and sessions, as the understanding model read them."</p>
                <select class=format!("{INPUT} w-auto py-1 text-sm") prop:value=move || status.get() on:change=move |ev| status.set(event_target_value(&ev))>
                    <option value="open">"Open"</option>
                    <option value="done">"Done"</option>
                    <option value="dropped">"Dropped"</option>
                </select>
            </div>
            <Suspense fallback=|| ()>
                {move || list.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(v) if v.is_empty() => view! { <p class="text-sm text-zinc-500">"None. (Choose an understanding model in Settings → Memex to have mail and sessions read.)"</p> }.into_any(),
                    Ok(v) => {
                        let now = crate::util::now_ms();
                        view! {
                            <ul class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                                {v.into_iter().map(|c| {
                                    let item = c.item_id.clone();
                                    let late = c.due.is_some_and(|d| d < now) && c.status == "open";
                                    let id = c.id;
                                    let who = if c.who == "me" { "I".to_owned() } else { c.who.clone() };
                                    view! {
                                        <li class="flex flex-wrap items-center gap-2 px-3 py-2 text-sm">
                                            <span class=if c.who == "me" { "rounded bg-amber-100 px-1.5 text-[11px] text-amber-900 dark:bg-amber-950 dark:text-amber-200" } else { "rounded bg-zinc-100 px-1.5 text-[11px] dark:bg-zinc-800" }>{who}</span>
                                            <span class="min-w-0 flex-1">{c.what.clone()}</span>
                                            {c.due.map(|d| view! { <span class=if late { "text-xs text-red-600" } else { "text-xs text-zinc-500" }>{format!("by {}", date(d))}</span> })}
                                            <button class="max-w-48 truncate text-xs text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| open.set(Some(item.clone()))>{c.item_title.clone()}</button>
                                            {(c.status == "open").then(|| view! {
                                                <button class=BUTTON_QUIET on:click=move |_| set(id, "done")>"Done"</button>
                                                <button class=BUTTON_QUIET on:click=move |_| set(id, "dropped")>"Drop"</button>
                                            })}
                                            {(c.status != "open").then(|| view! { <button class=BUTTON_QUIET on:click=move |_| set(id, "open")>"Reopen"</button> })}
                                        </li>
                                    }
                                }).collect_view()}
                            </ul>
                        }.into_any()
                    }
                })}
            </Suspense>
        </section>
    }
}

fn kind_color(kind: &str) -> &'static str {
    match kind {
        "person" => "#0ea5e9",
        "org" => "#10b981",
        "topic" => "#8b5cf6",
        "paper" => "#f59e0b",
        "place" => "#ef4444",
        "event" => "#ec4899",
        _ => "#71717a",
    }
}

/// Who a person or organization is to you (docs/relationships.md): their
/// cards, as the model wrote them from your mail's history with them.
#[component]
pub fn Cards(#[prop(into)] params: serde_json::Value) -> impl IntoView {
    let cards = LocalResource::new(move || {
        let p = params.clone();
        async move { api::rpc("memex.relations.cards", p).await }
    });
    view! {
        {move || cards.get().and_then(Result::ok).and_then(|v| v.as_array().cloned()).filter(|a| !a.is_empty()).map(|a| view! {
            <div class="space-y-1.5">
                {a.into_iter().map(|c| {
                    let f = |k: &str| c["card"][k].as_str().unwrap_or("").to_owned();
                    view! {
                        <div class="space-y-0.5 rounded-md border border-zinc-200 p-2 text-sm dark:border-zinc-800">
                            <p>
                                <span class="font-medium">{c["name"].as_str().unwrap_or("").to_owned()}</span>
                                <span class="ml-1.5 rounded bg-sky-100 px-1.5 text-[11px] text-sky-800 dark:bg-sky-950 dark:text-sky-300">{f("relationship").replace('_', " ")}</span>
                                {(!f("role").is_empty()).then(|| view! { <span class="ml-1.5 text-xs text-zinc-500">{f("role")}</span> })}
                            </p>
                            {(!f("story").is_empty()).then(|| view! { <p>{f("story")}</p> })}
                            {(!f("open").is_empty()).then(|| view! { <p class="text-xs"><span class="text-zinc-500">"Open: "</span>{f("open")}</p> })}
                            {(!f("how").is_empty()).then(|| view! { <p class="text-xs"><span class="text-zinc-500">"You: "</span>{f("how")}</p> })}
                            <p class="text-[11px] text-zinc-400">{format!("From {} thread{} with you, the model's reading", c["threads"].as_i64().unwrap_or(0), if c["threads"] == 1 { "" } else { "s" })}</p>
                        </div>
                    }
                }).collect_view()}
            </div>
        })}
    }
}

/// A thing's graph, its timeline by month, and "summarize what I know".
#[component]
pub fn ThingMore(key: String, open: RwSignal<Option<String>>) -> impl IntoView {
    let cards_for = serde_json::json!({"id": key.clone()});
    let k1 = key.clone();
    let graph = LocalResource::new(move || {
        let k = k1.clone();
        async move { api::rpc_as::<MemexGraph>("memex.graph", json!({"id": k, "limit": 30})).await }
    });
    let k2 = key.clone();
    let months = LocalResource::new(move || {
        let k = k2.clone();
        async move { api::rpc("memex.months", json!({"id": k})).await }
    });
    let summary = RwSignal::new(None::<Result<String, String>>);
    let busy = RwSignal::new(false);
    let summarize = move |_| {
        let k = key.clone();
        busy.set(true);
        spawn_local(async move {
            summary.set(Some(
                api::rpc("memex.summarize", json!({"id": k})).await.map(|v| v["summary"].as_str().unwrap_or_default().to_owned()),
            ));
            busy.set(false);
        });
    };
    view! {
        <Cards params=cards_for />
        <div class="flex items-center gap-2">
            <button class=BUTTON_QUIET disabled=move || busy.get() on:click=summarize>{move || if busy.get() { "Summarizing…" } else { "Summarize what I know" }}</button>
            <span class="text-xs text-zinc-500">"(saved as a note, marked as the model's)"</span>
        </div>
        {move || summary.get().map(|r| match r {
            Ok(s) => view! { <div class="whitespace-pre-wrap rounded-md bg-amber-50 p-3 text-sm dark:bg-amber-950/40">{s}</div> }.into_any(),
            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
        })}
        {move || months.get().and_then(Result::ok).map(timeline)}
        {move || graph.get().map(|r| match r {
            Ok(g) if !g.nodes.is_empty() => radial(g, open).into_any(),
            Ok(_) => ().into_any(),
            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
        })}
    }
}

/// Items per month, a bar each.
fn timeline(v: Value) -> impl IntoView {
    let rows = v.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return ().into_any();
    }
    let mut months: Vec<(String, i64)> = Vec::new();
    for r in &rows {
        let m = r["month"].as_str().unwrap_or_default().to_owned();
        let n = r["n"].as_i64().unwrap_or(0);
        match months.last_mut() {
            Some((last, total)) if *last == m => *total += n,
            _ => months.push((m, n)),
        }
    }
    // Fill the gaps between the first and last month.
    let parse = |m: &str| -> Option<(i32, u32)> { Some((m.get(0..4)?.parse().ok()?, m.get(5..7)?.parse().ok()?)) };
    let (Some(first), Some(last)) = (parse(&months[0].0), parse(&months[months.len() - 1].0)) else { return ().into_any() };
    let mut all = Vec::new();
    let (mut y, mut mo) = first;
    while (y, mo) <= last && all.len() < 600 {
        let key = format!("{y:04}-{mo:02}");
        all.push((key.clone(), months.iter().find(|(m, _)| *m == key).map(|(_, n)| *n).unwrap_or(0)));
        mo += 1;
        if mo > 12 {
            mo = 1;
            y += 1;
        }
    }
    let max = all.iter().map(|(_, n)| *n).max().unwrap_or(1).max(1) as f64;
    let w = (all.len() as f64 * 6.0).max(120.0);
    view! {
        <div class="space-y-1">
            <h3 class="text-xs font-medium uppercase text-zinc-500">{format!("Timeline · {} – {}", all[0].0, all[all.len() - 1].0)}</h3>
            <svg viewBox=format!("0 0 {w} 40") class="h-12 w-full" preserveAspectRatio="none">
                {all.iter().enumerate().map(|(i, (m, n))| {
                    let h = (*n as f64 / max) * 38.0;
                    view! { <rect x=(i as f64 * 6.0).to_string() y=(40.0 - h).to_string() width="5" height=h.to_string() class="fill-sky-500"><title>{format!("{m}: {n}")}</title></rect> }
                }).collect_view()}
            </svg>
        </div>
    }
    .into_any()
}

/// The thing in the middle, its neighbours in a ring (closer: more shared).
fn radial(g: MemexGraph, open: RwSignal<Option<String>>) -> impl IntoView {
    let n = g.nodes.len();
    let max = g.nodes.iter().map(|x| x.shared).max().unwrap_or(1).max(1) as f64;
    let (cx, cy) = (300.0, 220.0);
    let pos: Vec<(String, f64, f64)> = g
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let a = i as f64 / n as f64 * std::f64::consts::TAU - std::f64::consts::FRAC_PI_2;
            let r = 200.0 - 90.0 * (node.shared as f64 / max);
            (node.thing.id.clone(), cx + r * a.cos() * 1.3, cy + r * a.sin())
        })
        .collect();
    let at = |id: &str| pos.iter().find(|(x, _, _)| x == id).map(|(_, x, y)| (*x, *y));
    let edges: Vec<_> = g
        .edges
        .iter()
        .filter_map(|(a, b, w)| Some((at(a)?, at(b)?, *w)))
        .map(|((x1, y1), (x2, y2), w)| {
            view! { <line x1=x1.to_string() y1=y1.to_string() x2=x2.to_string() y2=y2.to_string() stroke-width=(0.5 + (w as f64).ln()).to_string() class="stroke-zinc-300 dark:stroke-zinc-700" /> }
        })
        .collect();
    let center = g.center.name.clone();
    view! {
        <div class="space-y-1">
            <h3 class="text-xs font-medium uppercase text-zinc-500">"Graph"</h3>
            <svg viewBox="0 0 600 440" class="w-full rounded-md border border-zinc-200 dark:border-zinc-800">
                {edges}
                {g.nodes.iter().zip(&pos).map(|(node, (_, x, y))| view! {
                    <line x1=cx.to_string() y1=cy.to_string() x2=x.to_string() y2=y.to_string() stroke=kind_color(&node.thing.kind) stroke-opacity="0.35"
                        stroke-width=(0.6 + (node.shared.max(1) as f64).ln()).to_string() />
                }).collect_view()}
                {g.nodes.iter().zip(&pos).map(|(node, (_, x, y))| {
                    let key = node.thing.id.clone();
                    let tip = format!("{} ({}) · {} shared{}", node.thing.name, node.thing.kind, node.shared,
                        if node.rels.is_empty() { String::new() } else { format!(" · {}", node.rels.join(", ")) });
                    let name: String = node.thing.name.chars().take(22).collect();
                    view! {
                        <g class="cursor-pointer" on:click=move |_| open.set(Some(key.clone()))>
                            <circle cx=x.to_string() cy=y.to_string() r=(4.0 + 6.0 * (node.shared as f64 / max)).to_string() fill=kind_color(&node.thing.kind) />
                            <text x=x.to_string() y=(y + 16.0).to_string() text-anchor="middle" class="fill-zinc-700 text-[10px] dark:fill-zinc-300">{name}</text>
                            <title>{tip}</title>
                        </g>
                    }
                }).collect_view()}
                <circle cx=cx.to_string() cy=cy.to_string() r="12" class="fill-zinc-900 dark:fill-zinc-100" />
                <text x=cx.to_string() y=(cy + 26.0).to_string() text-anchor="middle" class="fill-zinc-900 text-xs font-semibold dark:fill-zinc-100">{center}</text>
            </svg>
            <p class="text-[11px] text-zinc-500">"Closer and larger: more items in common. Colour: person, org, topic, paper, place, event. Click to go there."</p>
        </div>
    }
}

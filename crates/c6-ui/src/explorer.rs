//! The realm explorer (docs/explorer.md): the map of realms, and for one
//! realm its inventory, overview, section lists and objects, each with
//! everything connected to it. It reads; the pages that own each thing
//! edit it.

use c6_api::{ExObject, ExRef, RealmSummary, View as ViewDef};
use leptos::prelude::*;
use serde_json::Value;

use crate::api;
use crate::util::{ErrorBox, INPUT, ago, now_ms};
use crate::workspace::{Go, View};

fn explore(realm: &str, section: &str, kind: &str, id: &str) -> View {
    View::Explore { realm: realm.into(), section: section.into(), kind: kind.into(), id: id.into() }
}

fn when(at: Option<i64>) -> String {
    at.map(|t| ago(now_ms() - t)).unwrap_or_default()
}

/// A kind's mark and color.
fn badge(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "collection" => ("▦", "text-sky-600"),
        "record" => ("▤", "text-sky-500"),
        "series" => ("∿", "text-cyan-600"),
        "thing" => ("◆", "text-violet-600"),
        "item" => ("✉", "text-violet-500"),
        "fact" => ("✓", "text-emerald-600"),
        "session" => ("▶", "text-amber-600"),
        "decision" => ("?", "text-rose-600"),
        "trigger" => ("⚡", "text-orange-600"),
        "grant" => ("⇄", "text-teal-600"),
        "view" => ("◱", "text-indigo-600"),
        "entry" => ("⚙", "text-zinc-500"),
        "secret" => ("🔒", "text-zinc-500"),
        "realm" => ("◎", "text-zinc-700"),
        "template" => ("✎", "text-amber-700"),
        "event" => ("◇", "text-zinc-500"),
        _ => ("•", "text-zinc-500"),
    }
}

/// One object as a line that opens it.
#[component]
fn RefLine(realm: String, section: String, r: ExRef) -> impl IntoView {
    let (mark, color) = badge(&r.kind);
    let to = if r.kind == "realm" { explore(&r.id, "", "", "") } else { explore(&realm, &section, &r.kind, &r.id) };
    view! {
        <Ex to=to class="flex min-w-0 items-baseline gap-2 rounded px-2 py-1 text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800">
            <span class=format!("w-4 shrink-0 text-center {color}") title=r.kind.clone()>{mark}</span>
            <span class="min-w-0 truncate">{r.title.clone()}</span>
            <span class="min-w-0 flex-1 truncate text-xs text-zinc-500">{r.sub.clone()}</span>
            <span class="shrink-0 text-xs text-zinc-400">{when(r.at)}</span>
        </Ex>
    }
}

/// A link inside the explorer: it moves this column (⌘-click: a new tile),
/// as browsing does, instead of opening a column beside it.
#[component]
fn Ex(to: View, #[prop(optional, into)] class: String, #[prop(optional, into)] title: String, children: Children) -> impl IntoView {
    let col = crate::workspace::use_column();
    let ws = crate::workspace::use_workspace();
    let href = to.path();
    let to = StoredValue::new(to);
    view! {
        <a href=href class=class title=title on:click=move |ev: web_sys::MouseEvent| {
            if ev.button() != 0 || ev.shift_key() || ev.alt_key() {
                return;
            }
            ev.prevent_default();
            let v = to.get_value();
            match &col {
                Some(c) if !(ev.meta_key() || ev.ctrl_key()) => c.replace(v),
                _ => ws.open(None, v, crate::workspace::How::from_mouse(&ev)),
            }
        }>{children()}</a>
    }
}

/// A click on a link drawn in a graph: as `Ex` does.
fn follow(ev: web_sys::MouseEvent, to: View, nav: &Nav) {
    if ev.button() != 0 || ev.shift_key() || ev.alt_key() {
        return;
    }
    ev.prevent_default();
    match &nav.0 {
        Some(c) if !(ev.meta_key() || ev.ctrl_key()) => c.replace(to),
        _ => nav.1.open(None, to, crate::workspace::How::from_mouse(&ev)),
    }
}

/// Where graph clicks go: this column (or the workspace).
#[derive(Clone)]
struct Nav(Option<crate::workspace::ColumnCtx>, crate::workspace::Workspace);

fn nav() -> Nav {
    Nav(crate::workspace::use_column(), crate::workspace::use_workspace())
}

// --- The map ---------------------------------------------------------------------

/// Every realm, what it holds, and the grants between them.
#[component]
pub fn RealmsMap() -> impl IntoView {
    let map = LocalResource::new(api::explorer_map);
    view! {
        <main class="mx-auto max-w-6xl space-y-4 px-4 py-6">
            <div>
                <h1 class="text-lg font-semibold">"Realms"</h1>
                <p class="text-sm text-zinc-500">"Each realm keeps an app or a project apart. Open one to explore what it holds; the arrows are grants, the only way data crosses."</p>
            </div>
            {move || map.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(m) => {
                    let realms = m["realms"].as_array().cloned().unwrap_or_default();
                    let grants = m["grants"].as_array().cloned().unwrap_or_default();
                    view! {
                        <GrantGraph realms=realms.clone() grants=grants.clone() />
                        <div class="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
                            {realms.into_iter().map(|c| {
                                let id = c["realm"]["id"].as_str().unwrap_or("").to_owned();
                                let n = |k: &str| c["counts"][k].as_i64().unwrap_or(0);
                                let chips: Vec<String> = [("collections", "collections"), ("records", "records"), ("things", "things"), ("items", "items"), ("sessions", "sessions"), ("triggers", "triggers"), ("open_decisions", "open decisions")]
                                    .iter().filter(|(k, _)| n(k) > 0).map(|(k, l)| format!("{} {l}", n(k))).collect();
                                let health = c["health"].as_i64().unwrap_or(0);
                                view! {
                                    <Ex to=explore(&id, "", "", "") class="block space-y-2 rounded-lg border border-zinc-200 p-3 hover:border-zinc-400 dark:border-zinc-800 dark:hover:border-zinc-600">
                                        <div class="flex items-baseline gap-2">
                                            <span class="font-medium">{c["realm"]["name"].as_str().unwrap_or("").to_owned()}</span>
                                            <span class="font-mono text-xs text-zinc-500">{id.clone()}</span>
                                            <span class="flex-1"></span>
                                            {(health > 0).then(|| view! { <span class="rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white" title="Needs attention">{health}</span> })}
                                        </div>
                                        <p class="line-clamp-2 text-xs text-zinc-500">{c["realm"]["description"].as_str().unwrap_or("").to_owned()}</p>
                                        {c["app"]["app"].as_str().map(|a| view! { <p class="text-xs">{format!("app {a} {}", c["app"]["version"].as_str().unwrap_or(""))}</p> })}
                                        <div class="flex flex-wrap gap-1">
                                            {chips.into_iter().map(|t| view! { <span class="rounded bg-zinc-100 px-1.5 text-xs dark:bg-zinc-800">{t}</span> }).collect_view()}
                                        </div>
                                        <p class="text-xs text-zinc-500">{format!("${:.2} this month{}", c["spend_month"].as_f64().unwrap_or(0.0), c["last_activity"].as_i64().map(|t| format!(" · active {}", ago(now_ms() - t))).unwrap_or_default())}</p>
                                    </Ex>
                                }
                            }).collect_view()}
                        </div>
                    }.into_any()
                }
            })}
        </main>
    }
}

/// Realms in a ring, grants as arrows between them.
#[component]
fn GrantGraph(realms: Vec<Value>, grants: Vec<Value>) -> impl IntoView {
    if grants.is_empty() {
        return view! { <p class="text-xs text-zinc-500">"No grants: every realm keeps to itself (commons is read by every realm that reads it)."</p> }.into_any();
    }
    let ids: Vec<String> = realms.iter().filter_map(|r| r["realm"]["id"].as_str().map(str::to_owned)).collect();
    let n = ids.len().max(1) as f64;
    let (w, h, rad) = (640.0, 260.0, 100.0);
    let pos = |i: usize| {
        let a = std::f64::consts::TAU * i as f64 / n - std::f64::consts::FRAC_PI_2;
        (w / 2.0 + rad * 2.2 * a.cos(), h / 2.0 + rad * a.sin())
    };
    let at = |id: &str| ids.iter().position(|x| x == id).map(pos);
    view! {
        <svg viewBox=format!("0 0 {w} {h}") class="h-64 w-full rounded-md border border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-950">
            <defs><marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 z" class="fill-teal-600" /></marker></defs>
            {grants.iter().filter_map(|g| {
                let (a, b) = (at(g["from_realm"].as_str()?)?, at(g["to_realm"].as_str()?)?);
                // Stop short of the node.
                let (dx, dy) = (b.0 - a.0, b.1 - a.1);
                let len = (dx * dx + dy * dy).sqrt().max(1.0);
                let (x2, y2) = (b.0 - dx / len * 30.0, b.1 - dy / len * 18.0);
                let label = format!("{} {}", g["mode"].as_str().unwrap_or(""), g["kind"].as_str().unwrap_or(""));
                let dashed = g["state"] == "requested";
                Some(view! {
                    <g>
                        <line x1=format!("{:.0}", a.0) y1=format!("{:.0}", a.1) x2=format!("{x2:.0}") y2=format!("{y2:.0}") class="stroke-teal-600" stroke-width="1.5" stroke-dasharray=if dashed { "4 3" } else { "" } marker-end="url(#arrow)" />
                        <text x=format!("{:.0}", (a.0 + b.0) / 2.0) y=format!("{:.0}", (a.1 + b.1) / 2.0 - 4.0) text-anchor="middle" class="fill-zinc-500 text-[10px]">{label}</text>
                    </g>
                })
            }).collect_view()}
            {ids.iter().enumerate().map(|(i, id)| {
                let (x, y) = pos(i);
                view! {
                    <g>
                        <rect x=format!("{:.0}", x - 34.0) y=format!("{:.0}", y - 12.0) width="68" height="24" rx="6" class="fill-zinc-100 stroke-zinc-300 dark:fill-zinc-800 dark:stroke-zinc-600" />
                        <text x=format!("{x:.0}") y=format!("{:.0}", y + 4.0) text-anchor="middle" class="fill-zinc-800 text-[11px] dark:fill-zinc-200">{id.clone()}</text>
                    </g>
                }
            }).collect_view()}
        </svg>
    }
    .into_any()
}

// --- One realm -------------------------------------------------------------------

const GROUPS: [(&str, &str); 6] = [
    ("data", "Data"),
    ("knowledge", "Knowledge"),
    ("work", "Work"),
    ("config", "Configuration"),
    ("flow", "Flow"),
    ("infrastructure", "Infrastructure"),
];

/// A realm: its inventory (left), a section or the overview (middle), and
/// an object (right).
#[component]
pub fn ExplorePage(realm: String, section: String, kind: String, id: String) -> impl IntoView {
    // Exploring a realm is being in it: the rest of the UI (charts, views)
    // follows.
    if realm != "root" && api::realm().get_untracked() != realm {
        api::realm().set(realm.clone());
    }
    let r1 = realm.clone();
    let summary = LocalResource::new(move || api::realm_summary(r1.clone()));
    let has_object = !kind.is_empty();
    let (rr, sec) = (realm.clone(), section.clone());
    view! {
        <main class="flex h-full min-h-0 flex-col gap-3 px-3 py-3 lg:flex-row">
            <aside class=format!("shrink-0 space-y-3 overflow-y-auto lg:w-64 {}", if has_object || !section.is_empty() { "hidden lg:block" } else { "" })>
                {move || summary.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(s) => view! { <Inventory summary=s realm=rr.clone() section=sec.clone() /> }.into_any(),
                })}
            </aside>
            <section class=format!("min-w-0 flex-1 overflow-y-auto {}", if has_object { "hidden lg:block" } else { "" })>
                {if section.is_empty() {
                    let r2 = realm.clone();
                    view! { {move || summary.get().map(|r| match r {
                        Err(_) => ().into_any(),
                        Ok(s) => view! { <Overview summary=s realm=r2.clone() /> }.into_any(),
                    })} }.into_any()
                } else {
                    view! { <SectionList realm=realm.clone() section=section.clone() selected=id.clone() /> }.into_any()
                }}
            </section>
            {has_object.then(|| view! {
                <section class="min-w-0 overflow-y-auto rounded-lg border border-zinc-200 p-3 lg:w-[30rem] lg:shrink-0 dark:border-zinc-800">
                    <ObjectPane realm=realm.clone() section=section.clone() kind=kind.clone() id=id.clone() />
                </section>
            })}
        </main>
    }
}

#[component]
fn Inventory(summary: RealmSummary, realm: String, section: String) -> impl IntoView {
    let fresh = |at: Option<i64>| at.is_some_and(|t| now_ms() - t < 86_400_000);
    let no_knowledge = !summary.knowledge && summary.realm.id != "root";
    let (name, rid) = (summary.realm.name.clone(), summary.realm.id.clone());
    view! {
        <Ex to=explore(&realm, "", "", "") class="block">
            <h1 class="text-lg font-semibold">{name}</h1>
            <p class="font-mono text-xs text-zinc-500">{rid}</p>
        </Ex>
        <Ex to=View::Realms class="text-xs text-zinc-500 underline">"All realms"</Ex>
        <SearchBox realm=realm.clone() current=section.strip_prefix("search:").unwrap_or("").to_owned() />
        {GROUPS.iter().map(|(g, title)| {
            let entries: Vec<c6_api::ExEntry> = summary.entries.iter().filter(|e| e.group == *g).cloned().collect();
            (!entries.is_empty()).then(|| view! {
                <div>
                    <h2 class="px-2 text-xs font-medium uppercase tracking-wide text-zinc-500">{*title}</h2>
                    {entries.into_iter().map(|e| {
                        let active = e.section == section;
                        let count = if e.inherited > 0 { format!("{} +{}", e.count, e.inherited) } else { e.count.to_string() };
                        let class = format!("flex items-center gap-2 rounded px-2 py-1 text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800 {}", if active { "bg-zinc-100 font-medium dark:bg-zinc-800" } else { "" });
                        let dot = fresh(e.changed_at).then(|| view! { <span class="h-1.5 w-1.5 rounded-full bg-sky-500" title="Changed in the last day"></span> });
                        let label = e.label.clone();
                        let tip = if e.inherited > 0 { "its own + from root" } else { "" };
                        let inner = view! {
                            <span class="min-w-0 flex-1 truncate">{label}</span>
                            {dot}
                            <span class="text-xs text-zinc-500" title=tip>{count}</span>
                        };
                        match &e.link {
                            Some(l) => view! { <Go to=View::from_path(l) class=class>{inner}<span class="text-xs text-zinc-400">"↗"</span></Go> }.into_any(),
                            None => view! { <Ex to=explore(&realm, &e.section, "", "") class=class>{inner}</Ex> }.into_any(),
                        }
                    }).collect_view()}
                </div>
            })
        }).collect_view()}
        {no_knowledge.then(|| view! { <p class="px-2 text-xs text-zinc-500">"Knowledge appears when a memex is connected."</p> })}
    }
}

#[component]
fn Overview(summary: RealmSummary, realm: String) -> impl IntoView {
    let r1 = realm.clone();
    let since = RwSignal::new(1i64);
    let activity = LocalResource::new(move || api::realm_activity(r1.clone(), now_ms() - since.get() * 86_400_000));
    let wiring = RwSignal::new(false);
    let tiles: Vec<c6_api::ExEntry> = summary.entries.iter().filter(|e| e.group == "data" || e.group == "knowledge").cloned().collect();
    let max = tiles.iter().map(|t| t.count).max().unwrap_or(1).max(1) as f64;
    let (rh, ra, rt, rw) = (realm.clone(), realm.clone(), realm.clone(), realm.clone());
    view! {
        <div class="space-y-5">
            <div class="space-y-1">
                <p class="text-sm">{summary.realm.description.clone()}</p>
                <p class="text-xs text-zinc-500">
                    {summary.app.as_ref().map(|a| format!("App {} {} · ", a.app, a.version))}
                    {format!("${:.2} model spend this month", summary.spend_month)}
                    {summary.last_activity.map(|t| format!(" · last activity {}", ago(now_ms() - t)))}
                    {(summary.realm.id != "root" && summary.realm.id != "commons").then_some(if summary.realm.reads_commons { " · reads commons" } else { " · does not read commons" })}
                </p>
            </div>
            {(!summary.health.is_empty()).then(|| view! {
                <div class="rounded-md border border-amber-300 bg-amber-50 p-2 dark:border-amber-800 dark:bg-amber-950">
                    <h2 class="px-2 text-sm font-medium">"Needs attention"</h2>
                    {summary.health.clone().into_iter().map(|h| view! { <RefLine realm=rh.clone() section=String::new() r=h /> }).collect_view()}
                </div>
            })}
            {(!tiles.is_empty()).then(|| view! {
                <div class="space-y-1">
                    <h2 class="text-sm font-medium">"What it holds"</h2>
                    <div class="flex flex-wrap gap-2">
                        {tiles.into_iter().map(|t| {
                            // Sized by how much it holds.
                            let size = 6.0 + 6.0 * (t.count as f64 / max).sqrt();
                            let class = format!("flex flex-col justify-between rounded-md border p-2 text-sm hover:border-zinc-500 {}", if t.group == "data" { "border-sky-200 bg-sky-50 dark:border-sky-900 dark:bg-sky-950" } else { "border-violet-200 bg-violet-50 dark:border-violet-900 dark:bg-violet-950" });
                            let style = format!("width: {size:.1}rem; height: {:.1}rem", size * 0.6);
                            let inner = view! { <span class="truncate">{t.label.clone()}</span><span class="text-lg font-semibold">{t.count}</span> };
                            match &t.link {
                                Some(l) => view! { <Go to=View::from_path(l) class=class><div style=style class="flex flex-col justify-between">{inner}</div></Go> }.into_any(),
                                None => view! { <Ex to=explore(&rt, &t.section, "", "") class=class><div style=style class="flex flex-col justify-between">{inner}</div></Ex> }.into_any(),
                            }
                        }).collect_view()}
                    </div>
                </div>
            })}
            <div class="space-y-1">
                <div class="flex items-center gap-2">
                    <h2 class="text-sm font-medium">"How it is wired"</h2>
                    <button class="text-xs text-zinc-500 underline" on:click=move |_| wiring.update(|w| *w = !*w)>{move || if wiring.get() { "hide" } else { "show" }}</button>
                </div>
                {move || wiring.get().then(|| view! { <Wiring realm=rw.clone() /> })}
            </div>
            <div class="space-y-1">
                <div class="flex items-center gap-2">
                    <h2 class="text-sm font-medium">"Activity"</h2>
                    <select class="rounded border border-zinc-300 bg-transparent px-1 text-xs dark:border-zinc-700" on:change=move |ev| since.set(event_target_value(&ev).parse().unwrap_or(1))>
                        <option value="1">"last day"</option>
                        <option value="7">"last week"</option>
                        <option value="30">"last month"</option>
                    </select>
                </div>
                {move || activity.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(v) if v.is_empty() => view! { <p class="text-sm text-zinc-500">"Nothing happened in this time."</p> }.into_any(),
                    Ok(v) => v.into_iter().map(|a| view! { <RefLine realm=ra.clone() section=String::new() r=a /> }).collect_view().into_any(),
                })}
            </div>
        </div>
    }
}

/// The realm's wiring: what triggers watch, what they write or start, and
/// what views show, as three columns.
#[component]
fn Wiring(realm: String) -> impl IntoView {
    let nav = nav();
    let data = LocalResource::new(move || async move {
        let triggers = api::automations().await?;
        let views = api::views().await.unwrap_or_default();
        Ok::<_, String>((triggers, views))
    });
    view! {
        {move || data.get().map(|r| match r {
            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
            Ok((triggers, views)) => {
                // Nodes: sources (watched), triggers, targets (written,
                // started, shown).
                let mut sources: Vec<(String, String)> = Vec::new();
                let mut targets: Vec<(String, String)> = Vec::new();
                let mut edges: Vec<(String, String, &'static str)> = Vec::new();
                let add = |v: &mut Vec<(String, String)>, kind: &str, name: &str| {
                    let k = (kind.to_owned(), name.to_owned());
                    if !v.contains(&k) {
                        v.push(k);
                    }
                };
                for t in &triggers {
                    let tid = format!("trigger:{}", t.id);
                    for (k, key) in [("collection", "collection"), ("series", "series")] {
                        if let Some(n) = t.event_params[key].as_str() {
                            add(&mut sources, k, n);
                            edges.push((format!("{k}:{n}"), tid.clone(), "watches"));
                        }
                    }
                    let event = if t.event_params["collection"].is_null() && t.event_params["series"].is_null() { Some(t.trigger.clone()) } else { None };
                    if let Some(e) = event {
                        add(&mut sources, "event", &e);
                        edges.push((format!("event:{e}"), tid.clone(), "on"));
                    }
                    for (k, key) in [("collection", "collection"), ("series", "series")] {
                        if let Some(n) = t.action_params[key].as_str() {
                            add(&mut targets, k, n);
                            edges.push((tid.clone(), format!("{k}:{n}"), "writes"));
                        }
                    }
                    if t.action == "start" {
                        add(&mut targets, "template", &t.template);
                        edges.push((tid.clone(), format!("template:{}", t.template), "starts"));
                    }
                    if t.action == "ask" || t.action == "notify" {
                        add(&mut targets, "event", &t.action);
                        edges.push((tid.clone(), format!("event:{}", t.action), if t.action == "ask" { "asks" } else { "notifies" }));
                    }
                }
                for v in &views {
                    for (k, key) in [("collection", "collection"), ("series", "series")] {
                        if let Some(n) = v.source[key].as_str() {
                            add(&mut targets, "view", &v.name);
                            edges.push((format!("{k}:{n}"), format!("view:{}", v.name), "shown by"));
                            add(&mut sources, k, n);
                        }
                    }
                }
                if triggers.is_empty() && views.is_empty() {
                    return view! { <p class="text-sm text-zinc-500">"No triggers or views yet: nothing happens here on its own."</p> }.into_any();
                }
                let row = 30.0;
                let height = (sources.len().max(triggers.len()).max(targets.len()).max(1) as f64) * row + 20.0;
                let ypos = |i: usize| 20.0 + i as f64 * row;
                let pos = |key: &str| -> Option<(f64, f64)> {
                    if let Some(i) = sources.iter().position(|(k, n)| format!("{k}:{n}") == key) {
                        return Some((150.0, ypos(i)));
                    }
                    if let Some(i) = triggers.iter().position(|t| format!("trigger:{}", t.id) == key) {
                        return Some((320.0, ypos(i)));
                    }
                    targets.iter().position(|(k, n)| format!("{k}:{n}") == key).map(|i| (490.0, ypos(i)))
                };
                let label = |kind: &str, name: &str| format!("{} {name}", badge(kind).0);
                let node = |x: f64, y: f64, text: String, to: Option<View>| {
                    let t = view! { <text x=format!("{x:.0}") y=format!("{:.0}", y + 4.0) text-anchor="middle" class="fill-zinc-800 text-[11px] dark:fill-zinc-200">{text}</text> };
                    match to {
                        Some(to) => { let nav = nav.clone(); view! { <a href=to.path() on:click=move |ev: web_sys::MouseEvent| follow(ev, to.clone(), &nav)>{t}</a> }.into_any() }
                        None => t.into_any(),
                    }
                };
                view! {
                    <svg viewBox=format!("0 0 640 {height:.0}") class="w-full rounded-md border border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-950" style=format!("height: {:.0}px", height.min(600.0))>
                        {edges.iter().filter_map(|(a, b, what)| {
                            let (p, q) = (pos(a)?, pos(b)?);
                            let (x1, x2) = if p.0 < q.0 { (p.0 + 60.0, q.0 - 60.0) } else { (p.0 - 60.0, q.0 + 60.0) };
                            Some(view! {
                                <g>
                                    <line x1=format!("{x1:.0}") y1=format!("{:.0}", p.1) x2=format!("{x2:.0}") y2=format!("{:.0}", q.1) class="stroke-zinc-400" stroke-width="1" />
                                    <text x=format!("{:.0}", (x1 + x2) / 2.0) y=format!("{:.0}", (p.1 + q.1) / 2.0 - 3.0) text-anchor="middle" class="fill-zinc-400 text-[9px]">{*what}</text>
                                </g>
                            })
                        }).collect_view()}
                        {sources.iter().enumerate().map(|(i, (k, n))| {
                            let to = matches!(k.as_str(), "collection" | "series").then(|| explore(&realm, "", k, n));
                            node(150.0, ypos(i), label(k, n), to)
                        }).collect_view()}
                        {triggers.iter().enumerate().map(|(i, t)| node(320.0, ypos(i), label("trigger", &t.name), Some(explore(&realm, "triggers", "trigger", &t.id)))).collect_view()}
                        {targets.iter().enumerate().map(|(i, (k, n))| {
                            let to = match k.as_str() {
                                "collection" | "series" | "view" => Some(explore(&realm, "", k, n)),
                                _ => None,
                            };
                            node(490.0, ypos(i), label(k, n), to)
                        }).collect_view()}
                    </svg>
                }.into_any()
            }
        })}
    }
}

/// Searches the realm: results in the middle pane.
#[component]
fn SearchBox(realm: String, current: String) -> impl IntoView {
    let q = RwSignal::new(current);
    let col = crate::workspace::use_column();
    let ws = crate::workspace::use_workspace();
    let go = move || {
        let text = q.get_untracked().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let v = explore(&realm, &format!("search:{text}"), "", "");
        match &col {
            Some(c) => c.replace(v),
            None => ws.open(None, v, crate::workspace::How::Beside),
        }
    };
    view! {
        <form class="px-2" on:submit=move |ev| { ev.prevent_default(); go(); }>
            <input class=format!("{INPUT} py-1 text-sm") type="search" placeholder="Search this realm" prop:value=move || q.get() on:input=move |ev| q.set(event_target_value(&ev)) />
        </form>
    }
}

/// What a search found, by kind.
#[component]
fn SearchResults(realm: String, q: String, selected: String) -> impl IntoView {
    let (r1, q1) = (realm.clone(), q.clone());
    let found = LocalResource::new(move || api::realm_search(r1.clone(), q1.clone()));
    let section = format!("search:{q}");
    view! {
        <div class="space-y-3">
            <div class="flex items-center gap-2">
                <Ex to=explore(&realm, "", "", "") class="text-sm text-zinc-500 hover:underline lg:hidden">"← Overview"</Ex>
                <h2 class="font-medium">{format!("“{q}”")}</h2>
            </div>
            {move || found.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(gs) if gs.is_empty() => view! { <p class="text-sm text-zinc-500">"Nothing in this realm matches."</p> }.into_any(),
                Ok(gs) => gs.into_iter().map(|g| {
                    let (rr, ss, sel) = (realm.clone(), section.clone(), selected.clone());
                    view! {
                        <div>
                            <h3 class="px-2 text-xs font-medium uppercase tracking-wide text-zinc-500">{format!("{} ({}{})", g.label, g.items.len(), if g.more > 0 { format!("+{}", g.more) } else { String::new() })}</h3>
                            {g.items.into_iter().map(|x| {
                                let on = x.id == sel;
                                view! { <div class=if on { "rounded bg-sky-50 dark:bg-sky-950" } else { "" }><RefLine realm=rr.clone() section=ss.clone() r=x /></div> }
                            }).collect_view()}
                        </div>
                    }
                }).collect_view().into_any(),
            })}
        </div>
    }
}

#[component]
fn SectionList(realm: String, section: String, selected: String) -> impl IntoView {
    if let Some(q) = section.strip_prefix("search:") {
        return view! { <SearchResults realm=realm.clone() q=q.to_owned() selected=selected.clone() /> }.into_any();
    }
    let q = RwSignal::new(String::new());
    let (r1, s1) = (realm.clone(), section.clone());
    let list = LocalResource::new(move || api::realm_list(r1.clone(), s1.clone(), q.get()));
    let title = section.replace(':', " · ");
    // A collection or series section opens the object itself too.
    let (what, arg) = section.split_once(':').map(|(a, b)| (a.to_owned(), b.to_owned())).unwrap_or((section.clone(), String::new()));
    let (rr, ss) = (realm.clone(), section.clone());
    view! {
        <div class="space-y-2">
            <div class="flex flex-wrap items-center gap-2">
                <Ex to=explore(&realm, "", "", "") class="text-sm text-zinc-500 hover:underline lg:hidden">"← Overview"</Ex>
                <h2 class="font-medium">{title}</h2>
                {(what == "collection").then(|| view! { <Ex to=explore(&realm, &section, "collection", &arg) class="text-xs text-zinc-500 underline">"about this collection"</Ex> })}
                <span class="flex-1"></span>
                <input class=format!("{INPUT} w-48 py-1 text-sm") placeholder="Filter" prop:value=move || q.get() on:input=move |ev| q.set(event_target_value(&ev)) />
            </div>
            {move || list.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) if v.is_empty() => view! { <p class="text-sm text-zinc-500">"Nothing here."</p> }.into_any(),
                Ok(v) => v.into_iter().map(|x| {
                    let on = x.id == selected;
                    view! { <div class=if on { "rounded bg-sky-50 dark:bg-sky-950" } else { "" }><RefLine realm=rr.clone() section=ss.clone() r=x /></div> }
                }).collect_view().into_any(),
            })}
        </div>
    }
    .into_any()
}

#[component]
fn ObjectPane(realm: String, section: String, kind: String, id: String) -> impl IntoView {
    let (r1, k1, i1) = (realm.clone(), kind.clone(), id.clone());
    let obj = LocalResource::new(move || api::realm_object(r1.clone(), k1.clone(), i1.clone()));
    let graph = RwSignal::new(false);
    let back = explore(&realm, &section, "", "");
    view! {
        {move || obj.get().map(|r| match r {
            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
            Ok(o) => {
                let (mark, color) = badge(&o.kind);
                let (rc, sc, rg, sg) = (realm.clone(), section.clone(), realm.clone(), section.clone());
                let chart = (o.kind == "series").then(|| ViewDef {
                    name: o.id.clone(),
                    kind: "chart".into(),
                    source: serde_json::json!({"series": o.id, "bucket": "day", "minutes": 43200}),
                    ..Default::default()
                });
                let o2 = o.clone();
                view! {
                    <div class="space-y-3">
                        <div class="flex items-start gap-2">
                            <Ex to=back.clone() class="text-sm text-zinc-500 hover:underline lg:hidden">"←"</Ex>
                            <span class=format!("text-lg {color}")>{mark}</span>
                            <div class="min-w-0 flex-1">
                                <h2 class="break-words font-medium">{o.title.clone()}</h2>
                                <p class="text-xs text-zinc-500">{format!("{} · {}", o.kind, o.sub)}</p>
                            </div>
                            {o.open.clone().map(|p| view! { <Go to=View::from_path(&p) class="shrink-0 text-xs text-zinc-500 underline" title="Edit it where it lives">"Open ↗"</Go> })}
                        </div>
                        {(!o.fields.is_empty()).then(|| view! {
                            <dl class="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-sm">
                                {o.fields.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| view! { <dt class="text-zinc-500">{k.clone()}</dt><dd class="whitespace-pre-wrap break-words">{v.clone()}</dd> }).collect_view()}
                            </dl>
                        })}
                        {chart.map(|v| view! { <crate::views::ViewBody view=v all=Vec::new() /> })}
                        {(!o.body.is_empty()).then(|| view! { <div class="md max-h-96 overflow-y-auto text-sm" inner_html=crate::markdown::to_html(&o.body)></div> })}
                        <div class="flex items-center gap-2 border-t border-zinc-200 pt-2 dark:border-zinc-800">
                            <h3 class="text-sm font-medium">"Connected"</h3>
                            <button class="text-xs text-zinc-500 underline" on:click=move |_| graph.update(|g| *g = !*g)>{move || if graph.get() { "as lists" } else { "as a graph" }}</button>
                        </div>
                        {move || if graph.get() {
                            view! { <Neighborhood realm=rg.clone() section=sg.clone() object=o2.clone() /> }.into_any()
                        } else if o2.connected.is_empty() {
                            view! { <p class="text-sm text-zinc-500">"Nothing connected yet."</p> }.into_any()
                        } else {
                            o2.connected.iter().map(|g| view! {
                                <div>
                                    <h4 class="px-2 text-xs font-medium text-zinc-500">{format!("{}{}", g.label, if g.more > 0 { format!(" (+{} more)", g.more) } else { String::new() })}</h4>
                                    {g.items.iter().cloned().map(|x| view! { <RefLine realm=rc.clone() section=sc.clone() r=x /> }).collect_view()}
                                </div>
                            }).collect_view().into_any()
                        }}
                    </div>
                }.into_any()
            }
        })}
    }
}

/// An object, what is connected to it, and what is connected to those:
/// two rings. Clicking a node opens it.
#[component]
fn Neighborhood(realm: String, section: String, object: ExObject) -> impl IntoView {
    let nav = nav();
    let (r1, k1, i1) = (realm.clone(), object.kind.clone(), object.id.clone());
    let data = LocalResource::new(move || api::realm_neighborhood(r1.clone(), k1.clone(), i1.clone()));
    let short = |s: &str| {
        let t: String = s.chars().take(16).collect();
        if s.chars().count() > 16 { format!("{t}…") } else { t }
    };
    view! {
        {move || data.get().map(|r| match r {
            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
            Ok(g) => {
                let nodes = g["nodes"].as_array().cloned().unwrap_or_default();
                let edges = g["edges"].as_array().cloned().unwrap_or_default();
                let (w, h) = (520.0, 440.0);
                let (cx, cy) = (w / 2.0, h / 2.0);
                // Ring 1 evenly; ring 2 fanned out around its parent.
                let ring1: Vec<usize> = (0..nodes.len()).filter(|&i| nodes[i]["depth"] == 1).collect();
                let mut angle = vec![0.0f64; nodes.len()];
                let mut pos = vec![(cx, cy); nodes.len()];
                let n1 = ring1.len().max(1) as f64;
                for (k, &i) in ring1.iter().enumerate() {
                    let a = std::f64::consts::TAU * k as f64 / n1 - std::f64::consts::FRAC_PI_2;
                    angle[i] = a;
                    pos[i] = (cx + 120.0 * a.cos(), cy + 100.0 * a.sin());
                }
                let spread = std::f64::consts::TAU / n1 * 0.8;
                for &p in &ring1 {
                    let kids: Vec<usize> = (0..nodes.len()).filter(|&i| nodes[i]["depth"] == 2 && nodes[i]["parent"].as_u64() == Some(p as u64)).collect();
                    let m = kids.len();
                    for (k, &i) in kids.iter().enumerate() {
                        let off = if m > 1 { spread * (k as f64 / (m - 1) as f64 - 0.5) } else { 0.0 };
                        let a = angle[p] + off;
                        pos[i] = (cx + 225.0 * a.cos(), cy + 195.0 * a.sin());
                    }
                }
                let refs: Vec<ExRef> = nodes.iter().map(|n| serde_json::from_value(n["ref"].clone()).unwrap_or_default()).collect();
                view! {
                    <svg viewBox=format!("0 0 {w} {h}") class="w-full rounded-md border border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-950">
                        {edges.iter().filter_map(|e| {
                            let (a, b) = (e["from"].as_u64()? as usize, e["to"].as_u64()? as usize);
                            let (p, q) = (*pos.get(a)?, *pos.get(b)?);
                            let far = nodes.get(a)?["depth"] == 2 || nodes.get(b)?["depth"] == 2;
                            Some(view! {
                                <line x1=format!("{:.0}", p.0) y1=format!("{:.0}", p.1) x2=format!("{:.0}", q.0) y2=format!("{:.0}", q.1)
                                    class=if far { "stroke-zinc-200 dark:stroke-zinc-800" } else { "stroke-zinc-300 dark:stroke-zinc-700" } stroke-width="1">
                                    <title>{e["label"].as_str().unwrap_or("").to_owned()}</title>
                                </line>
                            })
                        }).collect_view()}
                        {refs.iter().enumerate().map(|(i, r)| {
                            let (x, y) = pos[i];
                            let (mark, color) = badge(&r.kind);
                            let depth = nodes[i]["depth"].as_u64().unwrap_or(0);
                            let to = explore(&realm, &section, &r.kind, &r.id);
                            let size = match depth { 0 => "text-[12px] font-semibold", 1 => "text-[10px]", _ => "text-[9px] opacity-80" };
                            let href = to.path();
                            view! {
                                <a href=href on:click={ let nav = nav.clone(); move |ev: web_sys::MouseEvent| follow(ev, to.clone(), &nav) }>
                                    <text x=format!("{x:.0}") y=format!("{:.0}", y + 4.0) text-anchor="middle" class=format!("fill-current {color} {size}")>
                                        {format!("{mark} {}", short(&r.title))}
                                        <title>{format!("{} {}: {}", r.kind, r.title, r.sub)}</title>
                                    </text>
                                </a>
                            }
                        }).collect_view()}
                    </svg>
                    <p class="text-xs text-zinc-500">"Two hops: what is connected to it, and what is connected to those. Click a node to open it."</p>
                }.into_any()
            }
        })}
    }
}

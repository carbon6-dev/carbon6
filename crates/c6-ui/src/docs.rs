//! Documents (docs/docs.md §12): the Docs list, and a document's panel.
//! A document reads first: ✎ by a block (or a double-click, or Edit for
//! the whole doc) opens its Markdown in place; typing sends the change
//! when you pause or are done, guarded by the version you read, so
//! someone else's edit since is never overwritten. A document updates live
//! as others (and agents) write it, each block drawn again only when it
//! changes, so what you're typing (a block, a comment) keeps the cursor.

use c6_api::docs::{Doc, DocBatch, DocBlock, DocNew, DocOp, DocPatch, DocQuery, DocRestore, DocSummary, DocTab, DocThread, DocVersion};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Map, Value};

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, ago, now_ms};
use crate::workspace::{Go, How, View, use_column, use_workspace};

fn since(at: i64) -> String {
    ago(now_ms() - at)
}

/// Who wrote something, as you read it.
pub fn author(by: &str) -> String {
    match by {
        "you" => "you".into(),
        "assistant" => "the assistant".into(),
        b if b.starts_with("session:") => "a session".into(),
        b if b.starts_with("token:") => "an API token".into(),
        b => b.to_owned(),
    }
}

thread_local! {
    /// Documents' titles as last seen, kept in this browser: tile labels
    /// come from the live feed, which carries sessions but not documents.
    static TITLES: (Owner, RwSignal<std::collections::HashMap<String, String>>) = {
        let owner = Owner::new_root(None);
        let saved = crate::util::storage_get_global(TITLES_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let signal = owner.with(|| RwSignal::new(saved));
        (owner, signal)
    };
}

const TITLES_KEY: &str = "c6.doc_titles";

/// A document's title as last seen here, if it has been.
pub fn title_of(id: &str) -> Option<String> {
    TITLES.with(|t| t.1.with(|m| m.get(id).cloned())).filter(|t| !t.trim().is_empty())
}

fn remember_titles<'a>(docs: impl IntoIterator<Item = &'a DocSummary>) {
    TITLES.with(|t| {
        let titles = t.1;
        let changed: Vec<_> = titles.with_untracked(|m| {
            docs.into_iter().filter(|d| m.get(&d.id) != Some(&d.title)).map(|d| (d.id.clone(), d.title.clone())).collect()
        });
        if !changed.is_empty() {
            titles.update(|m| m.extend(changed));
            crate::util::storage_set_global(TITLES_KEY, &titles.with_untracked(|m| serde_json::to_string(m).unwrap_or_default()));
        }
    });
}

/// The session a document came from: its mark, and a link to it.
#[component]
fn FromSession(id: String) -> impl IntoView {
    let live = api::live();
    let session = {
        let id = id.clone();
        move || live.sessions.with(|ss| ss.iter().find(|s| s.id == id).cloned())
    };
    let s2 = session.clone();
    view! {
        <span class="inline-flex min-w-0 items-center gap-1">
            "from "
            {move || session().map(|s| view! { <span class="h-2 w-2 shrink-0 rounded-sm" style=crate::marks::fill(&crate::marks::of(&s))></span> })}
            <Go to=View::session(id.clone()) class="truncate hover:underline" stop=true>
                {move || s2().map(|s| s.title).filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "a session".into())}
            </Go>
        </span>
    }
}

/// A session's title for `session:<id>`, from the live feed.
fn author_named(by: &str) -> String {
    if let Some(id) = by.strip_prefix("session:") {
        // Untracked: called while the document draws, it must not make
        // the whole document redraw (dropping an open comment box and
        // what's typed in it) whenever any session changes.
        let live = api::live();
        let title = live.sessions.with_untracked(|ss| ss.iter().find(|s| s.id == id).map(|s| s.title.clone()));
        return match title.filter(|t| !t.trim().is_empty()) {
            Some(t) if t.chars().count() > 32 => format!("session {}…", t.chars().take(30).collect::<String>().trim_end()),
            Some(t) => format!("session {t}"),
            None => "a session".into(),
        };
    }
    author(by)
}

fn kind_mark(kind: &str) -> &'static str {
    if kind == "deck" { "▦" } else { "▤" }
}

/// Reloads `tick` whenever a document changes over the live feed (`id`:
/// only that one).
fn reload_on_change(tick: RwSignal<u32>, id: Option<String>) {
    let live = api::live();
    Effect::new(move |_| {
        if let Some(c) = live.doc.get()
            && id.as_ref().is_none_or(|i| *i == c.id)
        {
            tick.update(|t| *t += 1);
        }
    });
}

/// The header link.
#[component]
pub fn DocsLink(class: &'static str) -> impl IntoView {
    view! { <Go to=View::Docs { id: String::new() } class=class title="Docs and decks: what people and agents write together">"Docs"</Go> }
}

/// What chips and embeds in a document panel share: the document, the
/// things its ref chips name as they are now, and the chip being changed
/// (its block, key and value).
#[derive(Clone, Copy)]
struct ChipCtx {
    doc: StoredValue<String>,
    refs: RwSignal<std::collections::HashMap<String, c6_api::work::LiveAttached>>,
    editing: RwSignal<Option<(String, String, Value)>>,
    send: StoredValue<Box<dyn Fn(Vec<DocOp>) + Send + Sync>>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// A block's Markdown as HTML with its chips as small widgets (docs/docs.md
/// §3): a date and a dropdown you click to change, a mention, and a ref
/// with the thing's title and state as they are now.
fn chips_html(text: &str, attrs: &Value, refs: &std::collections::HashMap<String, c6_api::work::LiveAttached>) -> String {
    let chips = attrs.get("chips").and_then(Value::as_object);
    let mut md = String::with_capacity(text.len());
    let mut shown = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("<?c6 block ") {
        let after = &rest[i + 11..];
        let Some(j) = after.find("?>") else { break };
        let key = after[..j].trim();
        md.push_str(&rest[..i]);
        let v = chips.and_then(|c| c.get(key)).cloned().unwrap_or(Value::Null);
        let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
        const CHIP: &str = "mx-0.5 rounded px-1 py-px text-[0.92em]";
        let html = match v["type"].as_str().unwrap_or("") {
            "date" => format!(
                "<button type=\"button\" data-chip=\"{}\" title=\"A date: click to change it\" class=\"{CHIP} bg-sky-50 text-sky-800 hover:bg-sky-100 dark:bg-sky-950/40 dark:text-sky-300\">📅 {}</button>",
                esc(key),
                esc(&s("value"))
            ),
            "mention" => format!(
                "<span class=\"{CHIP} bg-violet-50 text-violet-800 dark:bg-violet-950/40 dark:text-violet-300\">@{}</span>",
                esc(&if s("name").is_empty() { s("user") } else { s("name") })
            ),
            "dropdown" => format!(
                "<button type=\"button\" data-chip=\"{}\" title=\"Pick another\" class=\"{CHIP} bg-zinc-100 text-zinc-800 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-200\">{} ▾</button>",
                esc(key),
                esc(&s("value"))
            ),
            "ref" => {
                let r = s("ref");
                let live = refs.get(&r);
                let title = live
                    .map(|l| l.title.clone())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| if s("title").is_empty() { r.clone() } else { s("title") });
                let state = live.map(|l| l.state.clone()).unwrap_or_default();
                format!(
                    "<button type=\"button\" data-ref=\"{}\" title=\"{}\" class=\"{CHIP} bg-emerald-50 text-emerald-800 hover:underline dark:bg-emerald-950/40 dark:text-emerald-300\">↗ {}{}</button>",
                    esc(&r),
                    esc(&r),
                    esc(&title),
                    if state.is_empty() {
                        String::new()
                    } else {
                        format!(" <span class=\"text-[10px] uppercase tracking-wide opacity-70\">{}</span>", esc(&state.replace('_', " ")))
                    }
                )
            }
            _ => format!("[{}]", esc(key)),
        };
        md.push_str(&format!("\u{E000}{}\u{E001}", shown.len()));
        shown.push(html);
        rest = &after[j + 2..];
    }
    md.push_str(rest);
    let mut html = crate::markdown::to_html(&md);
    for (i, h) in shown.iter().enumerate() {
        html = html.replace(&format!("\u{E000}{i}\u{E001}"), h);
    }
    html
}

/// The ref chips' references in a document.
fn ref_chips(d: &Doc) -> Vec<String> {
    fn walk(b: &DocBlock, out: &mut Vec<String>) {
        for v in b.attrs.get("chips").and_then(Value::as_object).into_iter().flat_map(|c| c.values()) {
            if v["type"] == "ref"
                && let Some(r) = v["ref"].as_str()
                && !out.iter().any(|x| x == r)
            {
                out.push(r.to_owned());
            }
        }
        for c in &b.children {
            walk(c, out);
        }
    }
    let mut out = Vec::new();
    for t in &d.tabs {
        for b in &t.blocks {
            walk(b, &mut out);
        }
    }
    out
}

/// Where a ref chip's thing opens.
fn ref_path(r: &str) -> Option<String> {
    let (kind, id) = r.split_once(':')?;
    Some(match kind {
        "session" => format!("/sessions/{id}"),
        "work" => format!("/work/{id}"),
        "project" => format!("/projects/{id}"),
        "doc" => format!("/docs/{id}"),
        "decision" => "/decisions".to_owned(),
        _ => return None,
    })
}

/// A chip or ref clicked in a block: handled here (and the block isn't
/// opened for editing).
fn chip_clicked(
    ev: &web_sys::MouseEvent,
    block: &DocBlock,
    ctx: ChipCtx,
    col: Option<crate::workspace::ColumnCtx>,
    ws: crate::workspace::Workspace,
) -> bool {
    use wasm_bindgen::JsCast;
    let Some(el) =
        ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()).and_then(|e| e.closest("[data-chip],[data-ref]").ok().flatten())
    else {
        return false;
    };
    // A mention (a link) middle- or shift-clicked: the browser's.
    if el.tag_name() == "A" && (ev.button() != 0 || ev.shift_key() || ev.alt_key()) {
        return true;
    }
    ev.prevent_default();
    ev.stop_propagation();
    if let Some(r) = el.get_attribute("data-ref") {
        if r.starts_with("http://") || r.starts_with("https://") {
            let _ = window().open_with_url_and_target(&r, "_blank");
        } else if let Some(path) = ref_path(&r) {
            // Beside, or on ⌘/Ctrl-click in a new tile.
            let (to, how) = (View::from_path(&path), How::from_mouse(ev));
            match col {
                Some(c) => c.open(to, how),
                None => ws.open(None, to, how),
            }
        }
        return true;
    }
    let key = el.get_attribute("data-chip").unwrap_or_default();
    fn find(b: &DocBlock, key: &str) -> Option<(String, Value)> {
        if let Some(v) = b.attrs.get("chips").and_then(|c| c.get(key)) {
            return Some((b.id.clone(), v.clone()));
        }
        b.children.iter().find_map(|c| find(c, key))
    }
    if let Some((b, v)) = find(block, &key) {
        ctx.editing.set(Some((b, key, v)));
    }
    true
}

/// Changing a date or picking a dropdown's option.
#[component]
fn ChipEditor(block: String, key: String, value: Value, ctx: ChipCtx) -> impl IntoView {
    let current = RwSignal::new(value["value"].as_str().unwrap_or("").to_owned());
    let kind = value["type"].as_str().unwrap_or("").to_owned();
    let options: Vec<String> = value["options"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    let (b, k, v) = (StoredValue::new(block), StoredValue::new(key), StoredValue::new(value));
    let save = move |to: String| {
        let mut nv = v.get_value();
        nv["value"] = Value::String(to);
        let op = DocOp::Set {
            title: None,
            theme: None,
            size: None,
            tab: None,
            tab_title: None,
            tabs: None,
            block: Some(b.get_value()),
            chip: Some(k.get_value()),
            value: Some(nv),
        };
        ctx.editing.set(None);
        ctx.send.with_value(|f| f(vec![op]));
    };
    view! {
        <div class="my-1 flex flex-wrap items-center gap-2 rounded border border-zinc-200 bg-white p-2 text-sm shadow-sm dark:border-zinc-800 dark:bg-zinc-900"
            on:click=|ev| ev.stop_propagation()>
            {if kind == "dropdown" {
                options.into_iter().map(|o| {
                    let picked = o == current.get_untracked();
                    let o2 = o.clone();
                    view! {
                        <button class=format!("rounded px-2 py-0.5 {}", if picked { "bg-violet-600 text-white" } else { "bg-zinc-100 hover:bg-zinc-200 dark:bg-zinc-800" })
                            on:click=move |_| save(o2.clone())>{o}</button>
                    }
                }).collect_view().into_any()
            } else if kind == "date" {
                view! {
                    <input type="date" class=format!("{INPUT} w-auto!") prop:value=move || current.get() on:input=move |ev| current.set(event_target_value(&ev)) />
                    <button class=format!("{BUTTON} text-xs") on:click=move |_| save(current.get_untracked())>"Set"</button>
                }.into_any()
            } else {
                view! {
                    <input class=format!("{INPUT} w-auto!") prop:value=move || current.get() on:input=move |ev| current.set(event_target_value(&ev)) />
                    <button class=format!("{BUTTON} text-xs") on:click=move |_| save(current.get_untracked())>"Set"</button>
                }.into_any()
            }}
            <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| ctx.editing.set(None)>"Cancel"</button>
        </div>
    }
}

/// A picture in a document: a click or tap shows it as big as the screen
/// allows (`fill`: past its own size too, for a drawing), and ⌘-, Ctrl-,
/// Shift- or middle-click opens `new_tab` (a file's address) as a link would.
#[component]
fn Expandable(src: String, name: String, fill: bool, new_tab: Option<String>, children: Children) -> impl IntoView {
    let open = RwSignal::new(false);
    let href = new_tab.clone();
    view! {
        // A block: a drawing has no size of its own, and in a box that
        // shrinks to fit its content it would shrink to nothing.
        <a href=href target="_blank" title="Show it bigger" class=if fill { "block w-full cursor-zoom-in" } else { "block w-fit max-w-full cursor-zoom-in" }
            on:click=move |ev: web_sys::MouseEvent| {
                ev.stop_propagation();
                if new_tab.is_none() || (ev.button() == 0 && !(ev.meta_key() || ev.ctrl_key() || ev.shift_key() || ev.alt_key())) {
                    ev.prevent_default();
                    open.set(true);
                }
            }>
            {children()}
        </a>
        // Its clicks stay here: the block around it would start editing.
        {move || open.get().then(|| view! {
            <div on:click=|ev| ev.stop_propagation()>
                <crate::session::FullImage src=src.clone() name=name.clone() fill=fill close=move |_| open.set(false) />
            </div>
        })}
    }
}

/// An embed (docs/docs.md §3): a picture, a file, a diagram, a chart drawn
/// from its rows, or a live view of the realm.
#[component]
fn Embed(v: Value) -> impl IntoView {
    let ctx = use_context::<ChipCtx>();
    let doc = ctx.map(|c| c.doc.get_value()).unwrap_or_default();
    let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
    let file_url = |blob: &str| api::url(&format!("/api/v1/docs/{}/files/{}", api::enc(&doc), api::enc(blob)));
    match v["type"].as_str().unwrap_or("") {
        "image" => {
            let (src, alt) = (file_url(&s("blob")), if s("alt").is_empty() { s("name") } else { s("alt") });
            view! {
                <figure class="my-2">
                    <Expandable src=src.clone() name=alt.clone() fill=false new_tab=Some(src.clone())>
                        <img src=src alt=alt class="max-h-[32rem] max-w-full rounded border border-zinc-200 dark:border-zinc-800" />
                    </Expandable>
                    {(!s("caption").is_empty()).then(|| view! { <figcaption class="mt-1 text-xs text-muted">{s("caption")}</figcaption> })}
                </figure>
            }.into_any()
        }
        "file" => view! {
            <a href=file_url(&s("blob")) class="my-1 inline-flex items-center gap-2 rounded border border-zinc-200 px-2 py-1 text-sm text-sky-700 hover:bg-zinc-50 dark:border-zinc-800 dark:text-sky-400 dark:hover:bg-zinc-900"
                on:click=|ev| ev.stop_propagation()>
                "📎 " {s("name")}
                <span class="text-xs text-muted">{v["size"].as_u64().map(|n| format!("{} KB", n.div_ceil(1024))).unwrap_or_default()}</span>
            </a>
        }.into_any(),
        // An agent's SVG, drawn as a picture: it can't run or restyle the page.
        "diagram" => {
            let svg = s("svg");
            let src = format!("data:image/svg+xml;base64,{}", base64_of(svg.as_bytes()));
            let title = s("title");
            view! {
                <figure class="my-2">
                    <Expandable src=src.clone() name=title.clone() fill=true new_tab=None>
                        <img src=src alt=title class="max-w-full" />
                    </Expandable>
                    {(!s("title").is_empty()).then(|| view! { <figcaption class="mt-1 text-xs text-muted">{s("title")}</figcaption> })}
                </figure>
            }.into_any()
        }
        "chart" => view! { <Chart v=v.clone() /> }.into_any(),
        "view" => view! { <EmbedView name=s("view") /> }.into_any(),
        other => view! {
            <div class="rounded border border-zinc-200 px-3 py-2 text-sm text-muted dark:border-zinc-800">{format!("{other} (not shown here)")}</div>
        }.into_any(),
    }
}

const SERIES: [&str; 6] = ["#7c3aed", "#0284c7", "#059669", "#d97706", "#db2777", "#475569"];

/// A chart drawn from the rows the document keeps: {chart: bar or line,
/// title, columns: [label, series…], rows: [[label, value…]]}. Hover a
/// value to read it; "Data" shows the rows.
#[component]
fn Chart(v: Value) -> impl IntoView {
    let kind = v["chart"].as_str().unwrap_or("bar").to_owned();
    let title = v["title"].as_str().unwrap_or("").to_owned();
    let columns: Vec<String> = v["columns"].as_array().into_iter().flatten().map(|c| c.as_str().unwrap_or("").to_owned()).collect();
    let rows: Vec<Vec<Value>> = v["rows"].as_array().into_iter().flatten().filter_map(|r| r.as_array().cloned()).collect();
    let num = |x: &Value| x.as_f64().or_else(|| x.as_str().and_then(|s| s.trim().parse().ok())).unwrap_or(0.0);
    let series = columns.len().saturating_sub(1).max(rows.first().map(|r| r.len().saturating_sub(1)).unwrap_or(0));
    let values: Vec<f64> = rows.iter().flat_map(|r| r.iter().skip(1).map(num)).collect();
    let max = values.iter().cloned().fold(0.0_f64, f64::max).max(1e-9);
    let min = values.iter().cloned().fold(0.0_f64, f64::min);
    let (w, h, left, bottom, top) = (640.0, 260.0, 44.0, 28.0, 10.0);
    let plot_h = h - bottom - top;
    let y = move |val: f64| top + plot_h * (1.0 - (val - min) / (max - min));
    let n = rows.len().max(1) as f64;
    let band = (w - left - 8.0) / n;
    let mut marks = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let label = r.first().map(|l| l.as_str().map(str::to_owned).unwrap_or_else(|| l.to_string())).unwrap_or_default();
        let x0 = left + band * i as f64;
        marks.push(format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" text-anchor=\"middle\" fill=\"currentColor\" opacity=\"0.7\">{}</text>",
            x0 + band / 2.0,
            h - 10.0,
            esc(&label)
        ));
        if kind != "line" {
            let bw = (band * 0.8) / series.max(1) as f64;
            for k in 0..series {
                let val = r.get(k + 1).map(num).unwrap_or(0.0);
                let (yv, y0) = (y(val), y(0.0));
                let name = columns.get(k + 1).cloned().unwrap_or_default();
                marks.push(format!(
                    "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"{}\" rx=\"2\"><title>{}{}: {}</title></rect>",
                    x0 + band * 0.1 + bw * k as f64,
                    yv.min(y0),
                    (bw - 2.0).max(1.0),
                    (y0 - yv).abs().max(0.5),
                    SERIES[k % SERIES.len()],
                    esc(&label),
                    if name.is_empty() { String::new() } else { format!(" · {}", esc(&name)) },
                    val
                ));
            }
        }
    }
    if kind == "line" {
        for k in 0..series {
            let pts: Vec<(f64, f64, f64)> = rows
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let val = r.get(k + 1).map(num).unwrap_or(0.0);
                    (left + band * i as f64 + band / 2.0, y(val), val)
                })
                .collect();
            let path = pts.iter().map(|(x, y, _)| format!("{x:.1},{y:.1}")).collect::<Vec<_>>().join(" ");
            marks.push(format!("<polyline points=\"{path}\" fill=\"none\" stroke=\"{}\" stroke-width=\"2\" />", SERIES[k % SERIES.len()]));
            for (i, (x, yv, val)) in pts.iter().enumerate() {
                let label = rows[i].first().map(|l| l.as_str().map(str::to_owned).unwrap_or_else(|| l.to_string())).unwrap_or_default();
                marks.push(format!(
                    "<circle cx=\"{x:.1}\" cy=\"{yv:.1}\" r=\"3.5\" fill=\"{}\"><title>{}: {val}</title></circle>",
                    SERIES[k % SERIES.len()],
                    esc(&label)
                ));
            }
        }
    }
    // The axis: zero, and the top value.
    marks.push(format!(
        "<line x1=\"{left}\" x2=\"{:.1}\" y1=\"{:.1}\" y2=\"{:.1}\" stroke=\"currentColor\" opacity=\"0.3\" />",
        w - 8.0,
        y(0.0),
        y(0.0)
    ));
    for val in [max, min].into_iter().filter(|v| *v != 0.0).chain([0.0]) {
        marks.push(format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" text-anchor=\"end\" fill=\"currentColor\" opacity=\"0.6\">{}</text>",
            left - 6.0,
            y(val) + 4.0,
            if val.fract() == 0.0 { format!("{val:.0}") } else { format!("{val:.1}") }
        ));
    }
    let svg = format!("<svg viewBox=\"0 0 {w} {h}\" class=\"w-full\" role=\"img\">{}</svg>", marks.join(""));
    let data = RwSignal::new(false);
    let legend: Vec<String> = columns.iter().skip(1).cloned().collect();
    view! {
        <figure class="my-2 rounded border border-zinc-200 p-2 dark:border-zinc-800" on:click=|ev| ev.stop_propagation()>
            <div class="flex items-center gap-2 text-sm">
                <span class="flex-1 font-medium">{title}</span>
                {legend.iter().enumerate().map(|(k, l)| view! {
                    <span class="flex items-center gap-1 text-xs text-muted"><span class="inline-block size-2 rounded-sm" style=format!("background:{}", SERIES[k % SERIES.len()])></span>{l.clone()}</span>
                }).collect_view()}
                <button class="text-xs text-muted hover:underline" on:click=move |_| data.update(|d| *d = !*d)>"Data"</button>
            </div>
            <div class="text-zinc-700 dark:text-zinc-300" inner_html=svg></div>
            {move || data.get().then(|| view! {
                <table class="mt-1 w-full text-xs">
                    <thead><tr>{columns.iter().map(|c| view! { <th class="px-1 text-left font-medium">{c.clone()}</th> }).collect_view()}</tr></thead>
                    <tbody>{rows.iter().map(|r| view! {
                        <tr>{r.iter().map(|c| view! { <td class="px-1">{c.as_str().map(str::to_owned).unwrap_or_else(|| c.to_string())}</td> }).collect_view()}</tr>
                    }).collect_view()}</tbody>
                </table>
            })}
        </figure>
    }
}

/// A view of the realm, live, inside a doc (docs/os.md §4.5).
#[component]
fn EmbedView(name: String) -> impl IntoView {
    let views = LocalResource::new(api::views);
    view! {
        <div class="my-2 rounded border border-zinc-200 p-2 dark:border-zinc-800" on:click=|ev| ev.stop_propagation()>
            {move || views.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(all) => match all.iter().find(|v| v.name == name).cloned() {
                    None => view! { <p class="text-sm text-muted">{format!("No view {name} in this realm.")}</p> }.into_any(),
                    Some(v) => view! {
                        <div class="mb-1 text-xs font-medium uppercase tracking-wide text-muted">{if v.title.is_empty() { v.name.clone() } else { v.title.clone() }}</div>
                        <crate::views::ViewBody view=v all=all />
                    }.into_any(),
                },
            })}
        </div>
    }
}

// --- The list ---------------------------------------------------------------

#[component]
pub fn DocsPage() -> impl IntoView {
    let tick = RwSignal::new(0u32);
    reload_on_change(tick, None);
    let kind = RwSignal::new(String::new());
    let q = RwSignal::new(String::new());
    let label = RwSignal::new(String::new());
    let by = RwSignal::new(String::new());
    let list = LocalResource::new(move || {
        tick.track();
        api::realm().track();
        let query = DocQuery {
            kind: Some(kind.get()).filter(|k| !k.is_empty()),
            q: Some(q.get()).filter(|k| !k.trim().is_empty()),
            label: Some(label.get()).filter(|k| !k.trim().is_empty()),
            by: Some(by.get()).filter(|k| !k.is_empty()),
            ..Default::default()
        };
        async move { api::docs_list(&query).await }
    });
    let shown = RwSignal::new(None::<Result<Vec<DocSummary>, String>>);
    Effect::new(move || {
        if let Some(r) = list.get() {
            shown.set(Some(r));
        }
    });
    let error = RwSignal::new(None::<String>);
    let making = RwSignal::new(None::<&'static str>);
    let title = RwSignal::new(String::new());
    let (col, ws) = (use_column(), use_workspace());
    let open_doc = move |id: String, how: How| match &col {
        Some(c) => c.open(View::Docs { id }, how),
        None => ws.open(None, View::Docs { id }, how),
    };
    let open_doc = StoredValue::new(open_doc);
    let make = move || {
        let Some(k) = making.get_untracked() else { return };
        let t = title.get_untracked();
        if t.trim().is_empty() {
            return;
        }
        let n = DocNew { kind: k.into(), title: t, ..Default::default() };
        spawn_local(async move {
            match api::doc_create(&n).await {
                Ok(d) => {
                    making.set(None);
                    title.set(String::new());
                    open_doc.with_value(|f| f(d.summary.id, How::Beside));
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let importing = RwSignal::new(false);
    let import_file = move |ev: leptos::ev::Event| {
        use wasm_bindgen::JsCast;
        let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
        let Some(file) = input.files().and_then(|f| f.get(0)) else { return };
        let name = file.name();
        importing.set(true);
        spawn_local(async move {
            let result = async {
                let buf =
                    wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await.map_err(|_| "could not read the file".to_owned())?;
                let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                let content = base64_of(&bytes);
                api::doc_import(&c6_api::docs::DocImport { name, content, ..Default::default() }).await
            }
            .await;
            importing.set(false);
            match result {
                Ok(d) => open_doc.with_value(|f| f(d.summary.id, How::Beside)),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let filter = |value: &'static str, label_text: &'static str| {
        view! {
            <button class=move || format!("rounded-md px-2 py-1 text-sm {}", if kind.get() == value { "bg-zinc-200 dark:bg-zinc-800" } else { "hover:bg-zinc-100 dark:hover:bg-zinc-900" })
                on:click=move |_| kind.set(value.into())>{label_text}</button>
        }
    };
    view! {
        <main class="mx-auto max-w-4xl space-y-4 px-4 py-5">
            <header class="flex flex-wrap items-baseline gap-x-3 gap-y-1">
                <h1 class="text-xl font-semibold">"Docs"</h1>
                <span class="text-sm text-muted">"Docs and decks you and agents write together"</span>
            </header>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="flex flex-wrap items-center gap-2">
                <button class=BUTTON on:click=move |_| making.set(Some("doc"))>"+ New doc"</button>
                <button class=BUTTON_QUIET on:click=move |_| making.set(Some("deck"))>"+ New deck"</button>
                <label class=format!("{BUTTON_QUIET} cursor-pointer") title="A Word, OpenDocument, PowerPoint, HTML or Markdown file becomes a doc or a deck">
                    {move || if importing.get() { "Importing…" } else { "Import…" }}
                    <input type="file" class="hidden" accept=".docx,.odt,.pptx,.html,.htm,.md,.markdown,.txt" on:change=import_file />
                </label>
                <span class="flex-1"></span>
                {filter("", "All")}
                {filter("doc", "Docs")}
                {filter("deck", "Decks")}
            </div>
            {move || making.get().map(|k| view! {
                <div class="flex gap-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
                    <input class=INPUT placeholder=if k == "deck" { "The deck's title" } else { "The doc's title" } prop:value=move || title.get()
                        on:input=move |ev| title.set(event_target_value(&ev)) on:keydown=move |ev| if ev.key() == "Enter" { make() } />
                    <button class=BUTTON_QUIET on:click=move |_| making.set(None)>"Cancel"</button>
                    <button class=BUTTON on:click=move |_| make()>"Make it"</button>
                </div>
            })}
            <div class="flex flex-wrap gap-2">
                <input class=format!("{INPUT} max-w-xs") placeholder="Search titles…" prop:value=move || q.get() on:input=move |ev| q.set(event_target_value(&ev)) />
                <input class=format!("{INPUT} max-w-[10rem]") placeholder="Label" prop:value=move || label.get() on:input=move |ev| label.set(event_target_value(&ev)) />
                <select class=format!("{INPUT} max-w-[10rem]") on:change=move |ev| by.set(event_target_value(&ev))>
                    <option value="">"Anyone"</option>
                    <option value="you">"You"</option>
                    <option value="session">"Sessions"</option>
                    <option value="assistant">"The assistant"</option>
                </select>
            </div>
            {move || match shown.get() {
                None => view! { <p class="text-sm text-muted">"Loading…"</p> }.into_any(),
                Some(Err(e)) => view! { <ErrorBox message=e /> }.into_any(),
                Some(Ok(l)) if l.is_empty() => view! {
                    <p class="text-sm text-muted">"No documents yet. Make one above, or ask a session to write one (it gets the doc tool)."</p>
                }.into_any(),
                Some(Ok(l)) => view! {
                    {remember_titles(&l)}
                    <div class="divide-y divide-zinc-100 dark:divide-zinc-900">
                        {l.into_iter().map(|d| {
                            let id = d.id.clone();
                            view! {
                                <a href=View::Docs { id: d.id.clone() }.href() class="flex items-center gap-2 px-2 py-2 text-sm hover:bg-zinc-50 dark:hover:bg-zinc-900"
                                    on:click=move |ev: web_sys::MouseEvent| {
                                        if ev.button() != 0 { return }
                                        ev.prevent_default();
                                        open_doc.with_value(|f| f(id.clone(), How::from_mouse(&ev)));
                                    }>
                                    <span class="shrink-0 text-muted" title=d.kind.clone()>{kind_mark(&d.kind)}</span>
                                    <span class="min-w-0 flex-1 truncate font-medium">{d.title.clone()}</span>
                                    {(d.pending > 0).then(|| view! {
                                        <span class="shrink-0 rounded bg-violet-100 px-1.5 text-[11px] text-violet-800 dark:bg-violet-900/40 dark:text-violet-300"
                                            title=format!("Still being written by {}", d.writers.iter().map(|w| author_named(w)).collect::<Vec<_>>().join(", "))>
                                            {format!("✎ {} to write", d.pending)}
                                        </span>
                                    })}
                                    {d.labels.iter().map(|l| view! { <span class="shrink-0 text-[11px] text-muted">{format!("#{l}")}</span> }).collect_view()}
                                    // The session it came from, else who changed it last.
                                    <span class="max-w-[40%] shrink truncate text-xs text-muted">
                                        {format!("{} · {}", if d.created_by.starts_with("session:") { format!("from {}", author_named(&d.created_by)) } else { author_named(&d.updated_by) }, since(d.updated_at))}
                                    </span>
                                </a>
                            }
                        }).collect_view()}
                    </div>
                }.into_any(),
            }}
        </main>
    }
}

// --- A document -------------------------------------------------------------

/// How the comments panel sits: beside the doc in a wide tile, over the
/// doc's right side in a narrower one, or as a sheet from the bottom on a
/// phone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fit {
    Docked,
    Floating,
    Sheet,
}

/// A tile at least this wide (70rem) has the comments panel beside the doc,
/// leaving the doc ~44rem beside the 26rem panel.
const DOCKED_FROM: f64 = 1120.0;

/// A button in the doc's bar.
const BAR: &str = "shrink-0 rounded-md px-2 py-1 text-sm text-soft hover:bg-zinc-100 dark:hover:bg-zinc-800";

/// A doc's comments, shared by its bar, its blocks (each marks its own),
/// the popover by a block and the panel: the threads as last loaded, and
/// what is open.
#[derive(Clone, Copy)]
struct ThreadsCtx {
    doc: StoredValue<String>,
    shown: RwSignal<Vec<DocThread>>,
    tick: RwSignal<u32>,
    /// The new-comment box to put the cursor in, once, as you open it
    /// (its block, "" for the whole tab); never on a redraw, which would
    /// take the cursor from wherever you're typing.
    focus: RwSignal<Option<String>>,
    /// The block (or slide) a new comment is being written on ("": the tab).
    commenting: RwSignal<Option<String>>,
    /// The comments panel is open.
    panel: RwSignal<bool>,
    /// The block whose comments show in a popover by its 💬, and where it
    /// hangs in the page (top and right, in pixels).
    pop: RwSignal<Option<String>>,
    pop_at: RwSignal<(f64, f64)>,
    tabs: Memo<Vec<DocTab>>,
    deck: Memo<bool>,
    /// The page: what the popover hangs in, and blocks are found in.
    root: NodeRef<leptos::html::Div>,
    /// Scrolls to blocks and flashes them (on their tab).
    reveal: Callback<Vec<String>>,
    /// Your open comments not sent to an agent yet, and sending them.
    unsent: Memo<usize>,
    sending: RwSignal<bool>,
    send_all: Callback<()>,
}

impl ThreadsCtx {
    /// A block's 💬 (`marker`) pressed: its popover opens under it (with a
    /// new comment box, `new`), or closes if it was open.
    fn toggle_pop(self, block: String, marker: &web_sys::Element, new: bool) {
        if !new && self.pop.get_untracked().as_deref() == Some(block.as_str()) {
            self.close_pop();
            return;
        }
        self.place_pop(marker);
        self.pop.set(Some(block.clone()));
        if new {
            self.focus.set(Some(block.clone()));
            self.commenting.set(Some(block));
        }
    }

    /// The popover hangs under `marker`, its right edge on the marker's.
    fn place_pop(self, marker: &web_sys::Element) {
        let Some(root) = self.root.try_get_untracked().flatten() else { return };
        let (r, m) = (root.get_bounding_client_rect(), marker.get_bounding_client_rect());
        let _ = self.pop_at.try_set((m.bottom() - r.top() + 4.0, (r.right() - m.right()).max(8.0)));
    }

    /// The popover goes, and the new comment in it (its draft stays).
    fn close_pop(self) {
        if let Some(b) = self.pop.try_get_untracked().flatten() {
            if self.commenting.try_get_untracked().flatten().as_ref() == Some(&b) {
                let _ = self.commenting.try_set(None);
            }
            let _ = self.pop.try_set(None);
        }
    }
}

/// Where a block is drawn in the page.
fn block_sel(id: &str) -> String {
    format!("[data-block=\"{}\"]", id.replace(['"', '\\'], ""))
}

/// Scrolls the page to the first of these blocks (drawn under `root`) and
/// flashes each.
fn flash_blocks(root: NodeRef<leptos::html::Div>, ids: &[String]) {
    use wasm_bindgen::JsCast;
    let Some(root) = root.try_get_untracked().flatten() else { return };
    let mut first = true;
    for id in ids {
        let Some(el) = root.query_selector(&block_sel(id)).ok().flatten() else { continue };
        if first {
            first = false;
            // The tile's scrolling area, the block about a third down it
            // (clear of the bar).
            match root.parent_element() {
                Some(area) => {
                    let (a, r) = (area.get_bounding_client_rect(), el.get_bounding_client_rect());
                    let opts = web_sys::ScrollToOptions::new();
                    opts.set_top((f64::from(area.scroll_top()) + r.top() - a.top() - (a.height() / 3.0).max(56.0)).max(0.0));
                    opts.set_behavior(web_sys::ScrollBehavior::Smooth);
                    area.scroll_to_with_scroll_to_options(&opts);
                }
                None => el.scroll_into_view(),
            }
        }
        let classes = el.class_list();
        let _ = classes.remove_1("c6-flash");
        // Read a size, so the animation starts again.
        let _ = el.dyn_ref::<web_sys::HtmlElement>().map(web_sys::HtmlElement::offset_width);
        let _ = classes.add_1("c6-flash");
    }
}

/// Closes a popover (`close`) on a press outside it (`node`), but not on
/// what matches `keep` (what opens it), and on an Escape that a box in it
/// didn't take.
fn dismiss(node: NodeRef<leptos::html::Div>, keep: &'static str, close: impl Fn() + Clone + 'static) {
    use wasm_bindgen::JsCast;
    let shut = close.clone();
    let press = window_event_listener(leptos::ev::pointerdown, move |ev| {
        let Some(t) = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return };
        let inside = node.try_get_untracked().flatten().is_some_and(|el| el.contains(Some(t.as_ref())));
        if !inside && t.closest(keep).ok().flatten().is_none() {
            shut();
        }
    });
    let keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" && !ev.default_prevented() {
            close();
        }
    });
    on_cleanup(move || {
        press.remove();
        keys.remove();
    });
}

/// A document, open: a bar that stays at the top (its title, its comments,
/// Edit, what uses it, export and history), then its tabs and blocks to
/// read. ✎ by a block, or a double-click, opens it in Markdown; Edit opens
/// any you click. Its comments are in a panel (💬 in the bar) and in a
/// popover by each block's 💬.
#[component]
pub fn DocPage(id: String) -> impl IntoView {
    let id = StoredValue::new(id);
    let tick = RwSignal::new(0u32);
    reload_on_change(tick, Some(id.get_value()));
    let loaded = LocalResource::new(move || {
        tick.track();
        api::doc_get(id.get_value())
    });
    let doc = RwSignal::new(None::<Result<Doc, String>>);
    let error = RwSignal::new(None::<String>);
    Effect::new(move || {
        let Some(r) = loaded.get() else { return };
        match r {
            Ok(d) => {
                remember_titles([&d.summary]);
                if doc.with_untracked(|x| !matches!(x, Some(Ok(o)) if *o == d)) {
                    doc.set(Some(Ok(d)));
                }
            }
            // Once shown, a doc stays, with what went wrong above it.
            Err(e) if doc.with_untracked(|x| matches!(x, Some(Ok(_)))) => error.set(Some(e)),
            Err(e) => doc.set(Some(Err(e))),
        }
    });
    // Each part is drawn again only when what it shows changes: a change
    // anywhere in the doc mustn't draw again a box you're typing in.
    let state = Memo::new(move |_| {
        doc.with(|d| match d {
            None => 0u8,
            Some(Err(_)) => 1,
            Some(Ok(_)) => 2,
        })
    });
    let shown_doc = Memo::new(move |_| doc.with(|d| d.as_ref().and_then(|r| r.as_ref().ok()).cloned()));
    let summary = Memo::new(move |_| shown_doc.with(|d| d.as_ref().map(|d| d.summary.clone()).unwrap_or_default()));
    let tabs = Memo::new(move |_| shown_doc.with(|d| d.as_ref().map(|d| d.tabs.clone()).unwrap_or_default()));
    let tab_names = Memo::new(move |_| tabs.with(|ts| ts.iter().map(|t| (t.id.clone(), t.title.clone())).collect::<Vec<_>>()));
    let tab = RwSignal::new(String::new());
    let current = Memo::new(move |_| {
        let chosen = tab.get();
        tabs.with(|ts| if ts.iter().any(|t| t.id == chosen) { chosen } else { ts.first().map(|t| t.id.clone()).unwrap_or_default() })
    });
    let blocks = Memo::new(move |_| {
        let c = current.get();
        tabs.with(|ts| ts.iter().find(|t| t.id == c).map(|t| t.blocks.clone()).unwrap_or_default())
    });
    let is_deck = Memo::new(move |_| summary.with(|s| s.kind == "deck"));
    let doc_rev = Memo::new(move |_| summary.with(|s| s.rev));
    let renaming = RwSignal::new(false);
    let title = RwSignal::new(String::new());
    let history = RwSignal::new(false);
    let exporting = RwSignal::new(false);
    let edit = RwSignal::new(None::<Editing>);
    let deck_pick = RwSignal::new(None::<String>);
    let draft = Draft { text: RwSignal::new(String::new()), typed: RwSignal::new(0) };
    let adding = RwSignal::new(String::new());
    let adding_open = RwSignal::new(false);
    // Edit mode: a click on a block opens it in Markdown.
    let edit_all = RwSignal::new(false);
    let reload = move || tick.update(|t| *t += 1);
    let reload_cb = Callback::new(move |()| reload());
    let send = move |ops: Vec<DocOp>| {
        spawn_local(async move {
            match api::doc_ops(id.get_value(), &DocBatch { ops, answering: String::new() }).await {
                Ok(_) => reload(),
                Err(e) => {
                    error.set(Some(conflict_text(&e)));
                    reload();
                }
            }
        });
    };
    let send = StoredValue::new(send);
    // Chips and embeds: ref chips' things as they are now, the chip being
    // changed, and a way to send its change.
    let chip_ctx = ChipCtx {
        doc: id,
        refs: RwSignal::new(Default::default()),
        editing: RwSignal::new(None),
        send: StoredValue::new(Box::new(move |ops: Vec<DocOp>| send.with_value(|f| f(ops)))),
    };
    provide_context(chip_ctx);
    Effect::new(move || {
        let Some(d) = shown_doc.get() else { return };
        let refs = ref_chips(&d);
        if refs.is_empty() {
            return;
        }
        spawn_local(async move {
            if let Ok(live) = api::doc_refs(refs).await {
                chip_ctx.refs.try_set(live.into_iter().map(|l| (l.reference.clone(), l)).collect());
            }
        });
    });

    // Comments: the panel, the popover by a block, and where they go.
    let root = NodeRef::<leptos::html::Div>::new();
    let fit = RwSignal::new(Fit::Floating);
    let (tile_w, tile_h) = (RwSignal::new(0.0_f64), RwSignal::new(600.0_f64));
    let panel = RwSignal::new(false);
    let pop = RwSignal::new(None::<String>);
    let shown_threads = RwSignal::new(Vec::<DocThread>::new());
    let reveal = Callback::new(move |ids: Vec<String>| {
        // Each as the block drawn for it (a slide, for a block on one), on
        // the tab of the first.
        let mut on = None::<String>;
        let mut els = Vec::new();
        tabs.with_untracked(|ts| {
            for t in ts {
                for b in &t.blocks {
                    if ids.iter().any(|i| *i == b.id || b.children.iter().any(|c| c.id == *i))
                        && *on.get_or_insert_with(|| t.id.clone()) == t.id
                    {
                        els.push(b.id.clone());
                    }
                }
            }
        });
        let Some(on) = on else { return };
        if on != current.get_untracked() {
            tab.set(on);
        }
        // A sheet covers the doc.
        if fit.get_untracked() == Fit::Sheet {
            panel.set(false);
            pop.set(None);
        }
        request_animation_frame(move || flash_blocks(root, &els));
    });
    let unsent = Memo::new(move |_| shown_threads.with(|ts| ts.iter().filter(|t| t.state == "open" && t.unsent).count()));
    let open_n = Memo::new(move |_| shown_threads.with(|ts| ts.iter().filter(|t| t.state == "open").count()));
    let sending = RwSignal::new(false);
    let send_all = Callback::new(move |()| {
        sending.set(true);
        spawn_local(async move {
            match api::doc_threads_send(id.get_value(), Vec::new()).await {
                Ok(_) => tick.update(|t| *t += 1),
                Err(e) => error.set(Some(e)),
            }
            sending.set(false);
        });
    });
    let threads = ThreadsCtx {
        doc: id,
        shown: shown_threads,
        tick,
        focus: RwSignal::new(None),
        commenting: RwSignal::new(None),
        panel,
        pop,
        pop_at: RwSignal::new((0.0, 0.0)),
        tabs,
        deck: is_deck,
        root,
        reveal,
        unsent,
        sending,
        send_all,
    };
    provide_context(threads);
    let loaded_threads = LocalResource::new(move || {
        tick.track();
        api::doc_threads(id.get_value())
    });
    Effect::new(move || {
        if let Some(Ok(t)) = loaded_threads.get()
            && shown_threads.with_untracked(|s| *s != t)
        {
            shown_threads.set(t);
        }
    });
    // A new comment on what has no popover (the tab, a slide) is written
    // in the panel.
    Effect::new(move || {
        if let Some(b) = threads.commenting.get()
            && pop.get_untracked().as_ref() != Some(&b)
        {
            panel.set(true);
        }
    });
    // The popover follows its block as the doc changes or the tile is
    // resized, and goes with it (another tab, the block deleted).
    Effect::new(move || {
        tile_w.track();
        // The panel docked or not moves the doc.
        panel.track();
        fit.track();
        let here = blocks.with(|bs| pop.get().filter(|p| bs.iter().any(|b| b.id == *p)));
        match here {
            None if pop.get_untracked().is_some() => threads.close_pop(),
            None => {}
            Some(b) => request_animation_frame(move || {
                let marker = root
                    .try_get_untracked()
                    .flatten()
                    .and_then(|r| r.query_selector(&format!("{} > [data-marks]", block_sel(&b))).ok().flatten());
                if let Some(m) = marker {
                    threads.place_pop(&m);
                }
            }),
        }
    });
    // The tile's size: the panel beside the doc, over it, or a sheet.
    Effect::new(move || {
        use wasm_bindgen::JsCast;
        use wasm_bindgen::closure::Closure;
        let Some(el) = root.get() else { return };
        // The tile's scrolling area, which the page fills.
        let Some(area) = el.parent_element() else { return };
        let on = Closure::<dyn Fn(js_sys::Array)>::new(move |entries: js_sys::Array| {
            let Some(e) = entries.get(0).dyn_ref::<web_sys::ResizeObserverEntry>().map(|e| e.content_rect()) else { return };
            // Hidden: nothing to go by.
            if e.width() <= 0.0 {
                return;
            }
            let phone = crate::util::touch_first()
                || !window().match_media("(min-width: 48rem) and (min-height: 32rem)").ok().flatten().is_some_and(|m| m.matches());
            let f = if e.width() >= DOCKED_FROM {
                Fit::Docked
            } else if phone {
                Fit::Sheet
            } else {
                Fit::Floating
            };
            if fit.try_get_untracked().is_some_and(|x| x != f) {
                let _ = fit.try_set(f);
            }
            let _ = tile_w.try_set(e.width());
            let _ = tile_h.try_set(e.height());
        });
        if let Ok(ro) = web_sys::ResizeObserver::new(on.as_ref().unchecked_ref()) {
            ro.observe(&area);
            on.forget();
            let ro = send_wrapper::SendWrapper::new(ro);
            on_cleanup(move || ro.disconnect());
        }
    });
    // Set only when they change: each draws the panel or popover anew.
    let docked = Memo::new(move |_| panel.get() && fit.get() == Fit::Docked);
    let floating = Memo::new(move |_| panel.get() && fit.get() == Fit::Floating);
    let sheet = Memo::new(move |_| panel.get() && fit.get() == Fit::Sheet);
    let popped = Memo::new(move |_| pop.get().map(|b| (b, fit.get() == Fit::Sheet)));

    let attaching = RwSignal::new(false);
    let attach_file = move |ev: leptos::ev::Event| {
        use wasm_bindgen::JsCast;
        let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
        let Some(file) = input.files().and_then(|f| f.get(0)) else { return };
        let (name, mime) = (file.name(), file.type_());
        attaching.set(true);
        spawn_local(async move {
            let result = async {
                let buf =
                    wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await.map_err(|_| "could not read the file".to_owned())?;
                let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                let f = api::doc_add_file(id.get_value(), &c6_api::docs::DocFileIn { name, content: base64_of(&bytes), mime }).await?;
                let kind = if f.mime.starts_with("image/") && f.mime != "image/svg+xml" { "image" } else { "file" };
                let mut blocks = Map::new();
                blocks
                    .insert("f".into(), serde_json::json!({"type": kind, "blob": f.blob, "name": f.name, "mime": f.mime, "size": f.size}));
                let t = tab.get_untracked();
                let op = DocOp::Insert {
                    markdown: "<?c6 block f?>".into(),
                    blocks,
                    before: None,
                    after: None,
                    section: None,
                    slide: None,
                    region: None,
                    tab: (!t.is_empty()).then_some(t),
                };
                api::doc_ops(id.get_value(), &DocBatch { ops: vec![op], answering: String::new() }).await.map(drop)
            }
            .await;
            attaching.set(false);
            match result {
                Ok(()) => reload(),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let used_in = LocalResource::new(move || {
        tick.track();
        api::work_of(format!("doc:{}", id.get_value()))
    });
    let in_projects = LocalResource::new(move || {
        tick.track();
        async move {
            let me = id.get_value();
            let all = api::projects_list(false).await.unwrap_or_default();
            all.into_iter()
                .filter_map(|p| p.docs.iter().find(|d| d.doc == me).map(|d| (p.id.clone(), p.fields.title.clone(), d.role.clone())))
                .collect::<Vec<_>>()
        }
    });
    // What refers to it: its projects, then its work.
    let used = Signal::derive(move || {
        let mut out: Vec<(View, String)> = in_projects
            .get()
            .unwrap_or_default()
            .into_iter()
            .map(|(pid, title, role)| {
                (
                    View::Projects { id: pid },
                    format!("Project · {title}{}", if role.is_empty() { String::new() } else { format!(" ({role})") }),
                )
            })
            .collect();
        out.extend(
            used_in
                .get()
                .and_then(Result::ok)
                .unwrap_or_default()
                .into_iter()
                .map(|w| (View::Work { id: w.id.clone() }, format!("Work · {}", w.fields.title))),
        );
        out
    });
    let rename = move || {
        let t = title.get_untracked();
        renaming.set(false);
        if t.trim().is_empty() {
            return;
        }
        spawn_local(async move {
            match api::doc_patch(id.get_value(), &DocPatch { title: Some(t), labels: None }).await {
                Ok(d) => doc.set(Some(Ok(d))),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let add_block = move || {
        let md = adding.get_untracked();
        if md.trim().is_empty() {
            return;
        }
        adding.set(String::new());
        let tab_id = tab.get_untracked();
        let op = DocOp::Insert {
            markdown: md,
            blocks: Map::new(),
            before: None,
            after: None,
            section: None,
            slide: None,
            region: None,
            tab: Some(tab_id).filter(|t| !t.is_empty()),
        };
        send.with_value(|f| f(vec![op]));
    };
    // The boxes to add a block and rename it: the cursor in them as they open.
    let add_ref = NodeRef::<leptos::html::Textarea>::new();
    let rename_ref = NodeRef::<leptos::html::Input>::new();
    Effect::new(move || {
        if let Some(el) = add_ref.get() {
            let _ = el.focus();
        }
    });
    Effect::new(move || {
        if let Some(el) = rename_ref.get() {
            let _ = el.focus();
        }
    });
    let toggle_edit = move |_| {
        if edit_all.get_untracked() {
            edit_all.set(false);
            adding_open.set(false);
            save(id.get_value(), edit, draft, false, error, reload);
        } else {
            edit_all.set(true);
        }
    };
    // The blocks drawn, each kept while it's unchanged; the one open in
    // Markdown as it was when opened, so a change to it from elsewhere
    // waits until you're done rather than taking what you're typing.
    let drawn = Memo::new(move |_| {
        let open = edit.with(|e| e.as_ref().map(|e| e.block.clone()));
        let mut bs = blocks.get();
        if let Some(o) = open
            && let Some(b) = bs.iter_mut().find(|b| b.id == o.id)
        {
            *b = o;
        }
        bs
    });
    let empty = Memo::new(move |_| blocks.with(Vec::is_empty));
    let show_add = Memo::new(move |_| !is_deck.get() && (edit_all.get() || empty.get()));

    let bar = move || {
        view! {
            <div class="sticky top-0 z-20 border-b border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-950">
                <div class="mx-auto flex max-w-3xl flex-wrap items-center gap-1 px-4 py-1">
                    <span class="shrink-0 text-muted" title=move || summary.with(|s| s.kind.clone())>{move || summary.with(|s| kind_mark(&s.kind))}</span>
                    {move || if renaming.get() {
                        view! {
                            <input node_ref=rename_ref class=format!("{INPUT} min-w-0 flex-1 basis-40 font-semibold") prop:value=move || title.get()
                                on:input=move |ev| title.set(event_target_value(&ev))
                                on:keydown=move |ev| if ev.key() == "Enter" { rename() } else if ev.key() == "Escape" { renaming.set(false) }
                                on:blur=move |_| rename() />
                        }.into_any()
                    } else {
                        view! {
                            <h1 class="min-w-0 flex-1 basis-40 cursor-text truncate text-lg font-semibold" title="Click to rename"
                                on:click=move |_| { title.set(summary.with_untracked(|s| s.title.clone())); renaming.set(true); }>
                                {move || summary.with(|s| s.title.clone())}
                            </h1>
                        }.into_any()
                    }}
                    <button data-opens="comments" class=move || format!("{BAR} {}", if panel.get() { "bg-zinc-100 dark:bg-zinc-800" } else { "" })
                        title=move || if panel.get() { "Hide the comments" } else { "Show the comments" }
                        on:click=move |_| panel.update(|p| *p = !*p)>
                        {move || match open_n.get() { 0 => "💬".to_owned(), n => format!("💬 {n} open") }}
                    </button>
                    {move || (unsent.get() > 0).then(|| send_all_button(threads, true))}
                    {move || (!is_deck.get()).then(|| view! {
                        <button class=move || format!("{BAR} {}", if edit_all.get() { "bg-zinc-100 font-medium dark:bg-zinc-800" } else { "" })
                            title=move || if edit_all.get() { "Back to reading" } else { "Edit the doc: a click on a block opens it in Markdown" }
                            on:click=toggle_edit>
                            {move || if edit_all.get() { "Done" } else { "✎ Edit" }}
                        </button>
                    })}
                    <UsedIn items=used />
                    <crate::util::Menu label="Export and history" items=vec![
                        ("Export…", Callback::new(move |()| exporting.update(|x| *x = !*x))),
                        ("History", Callback::new(move |()| history.update(|x| *x = !*x))),
                    ] />
                </div>
                {move || floating.get().then(|| view! { <FloatingPanel tile_h /> })}
            </div>
        }
    };
    let content = move || {
        view! {
            <header class="space-y-1">
                <p class="flex flex-wrap items-center gap-x-1 text-xs text-muted">
                    {move || summary.with(|s| view! {
                        {s.created_by.strip_prefix("session:").map(|id| view! { <FromSession id=id.to_owned() />" · " })}
                        {format!("Version {} · changed {} by {}", s.rev, since(s.updated_at), author_named(&s.updated_by))}
                        {(!s.labels.is_empty()).then(|| format!(" · {}", s.labels.iter().map(|l| format!("#{l}")).collect::<Vec<_>>().join(" ")))}
                    })}
                </p>
                {move || summary.with(|s| (s.pending > 0).then(|| view! {
                    <p class="text-sm text-violet-700 dark:text-violet-300">
                        {format!("✎ Being written by {}: {} {} to go", s.writers.iter().map(|w| author_named(w)).collect::<Vec<_>>().join(", "), s.pending, if s.pending == 1 { "part" } else { "parts" })}
                    </p>
                }))}
            </header>
            {move || tab_names.with(|ts| ts.len() > 1).then(|| {
                let on_tab = current.get();
                view! {
                    <nav class="flex flex-wrap gap-1 border-b border-zinc-200 dark:border-zinc-800">
                        {tab_names.get().into_iter().map(|(tid, name)| {
                            let on = tid == on_tab;
                            view! {
                                <button class=format!("-mb-px border-b-2 px-2 py-1 text-sm {}", if on { "border-zinc-900 font-medium dark:border-zinc-100" } else { "border-transparent text-muted" })
                                    on:click=move |_| tab.set(tid.clone())>{name}</button>
                            }
                        }).collect_view()}
                    </nav>
                }
            })}
            {move || exporting.get().then(|| view! { <Exports id=id.get_value() kind=summary.with_untracked(|s| s.kind.clone()) /> })}
            {move || history.get().then(|| view! { <History id=id.get_value() reload=reload_cb /> })}
            {move || is_deck.get().then(|| shown_doc.get().map(|d| view! {
                <crate::deck::DeckView doc=d send=Callback::new(move |ops: Vec<DocOp>| send.with_value(|f| f(ops))) error picked=deck_pick commenting=threads.commenting />
            }))}
            {move || (!is_deck.get()).then(|| view! {
                <article class="space-y-1">
                    <For each=move || drawn.get() key=|b: &DocBlock| (b.id.clone(), b.hash.clone(), b.rev) children=move |b: DocBlock| view! {
                        <BlockView doc_id=id b kind=summary.with_untracked(|s| s.kind.clone()) doc_rev edit draft error reload=reload_cb edit_all />
                    } />
                    {move || empty.get().then(|| view! { <p class="text-sm text-muted">"Empty: add a block below to start it."</p> })}
                </article>
            })}
            // The end, in edit mode (or an empty doc): a line that opens
            // into a Markdown box.
            {move || show_add.get().then(|| view! {
                <div>
                    {move || if adding_open.get() {
                        view! {
                            <div class="space-y-1">
                                <textarea node_ref=add_ref class=format!("{INPUT} min-h-[4rem] font-mono text-sm") placeholder="Add to the end in Markdown…"
                                    prop:value=move || adding.get() on:input=move |ev| adding.set(event_target_value(&ev))
                                    on:keydown=move |ev| if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key()) { add_block() } else if ev.key() == "Escape" { ev.prevent_default(); adding_open.set(false) } />
                                <div class="flex items-center justify-end gap-2">
                                    <label class=format!("{BUTTON_QUIET} cursor-pointer text-xs") title="A picture shows in the doc; any other file is a link to download">
                                        {move || if attaching.get() { "Adding…" } else { "+ Picture or file" }}
                                        <input type="file" class="hidden" on:change=attach_file />
                                    </label>
                                    <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| adding_open.set(false)>"Cancel"</button>
                                    <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| add_block()>"Add (⌘↵)"</button>
                                </div>
                            </div>
                        }.into_any()
                    } else {
                        view! {
                            <div class="flex flex-wrap items-center gap-x-2 rounded-md border border-dashed border-zinc-300 px-3 py-2 text-sm text-muted dark:border-zinc-700">
                                <button class="hover:text-zinc-900 dark:hover:text-zinc-100" on:click=move |_| adding_open.set(true)>"+ Add a block at the end"</button>
                                <span aria-hidden="true">"·"</span>
                                <label class="cursor-pointer hover:text-zinc-900 dark:hover:text-zinc-100" title="A picture shows in the doc; any other file is a link to download">
                                    {move || if attaching.get() { "Adding…" } else { "+ Picture or file" }}
                                    <input type="file" class="hidden" on:change=attach_file />
                                </label>
                            </div>
                        }.into_any()
                    }}
                </div>
            })}
        }
    };
    view! {
        <div node_ref=root class="relative">
            // Docked, the panel sits right beside the doc's reading column
            // and the two are centred together: at a wide tile's edge it
            // was far from what it's about.
            <div class="flex items-start justify-center">
                <div class=move || if docked.get() { "min-w-0 max-w-3xl flex-1" } else { "min-w-0 flex-1" }>
                    {move || (state.get() == 2).then(bar)}
                    <main class="mx-auto max-w-3xl space-y-4 px-4 pb-8 pt-3">
                        {move || error.get().map(|e| view! {
                            <div class="flex items-start gap-2">
                                <div class="flex-1"><ErrorBox message=e /></div>
                                <button class="text-xs text-muted" on:click=move |_| error.set(None)>"✕"</button>
                            </div>
                        })}
                        {move || match state.get() {
                            0 => view! { <p class="text-sm text-muted">"Loading…"</p> }.into_any(),
                            1 => view! { <ErrorBox message=doc.with_untracked(|d| d.as_ref().and_then(|r| r.as_ref().err()).cloned().unwrap_or_default()) /> }.into_any(),
                            _ => content().into_any(),
                        }}
                    </main>
                </div>
                // Beside the doc, scrolling on its own.
                {move || docked.get().then(|| view! {
                    <aside class="sticky top-0 flex w-[26rem] shrink-0 flex-col border-x border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-950"
                        style=move || format!("height: {}px", tile_h.get())>
                        <CommentsPanel />
                    </aside>
                })}
            </div>
            {move || sheet.get().then(|| view! {
                <Sheet close=Callback::new(move |()| panel.set(false))><CommentsPanel /></Sheet>
            })}
            {move || popped.get().map(|(b, on_sheet)| if on_sheet {
                view! {
                    <Sheet close=Callback::new(move |()| threads.close_pop())>
                        <div class="min-h-0 flex-1 space-y-2 overflow-y-auto overscroll-contain p-3 text-sm"><PopBody block=b /></div>
                    </Sheet>
                }.into_any()
            } else {
                view! { <BlockPop block=b /> }.into_any()
            })}
        </div>
    }
}

/// "Send N to agent": your open comments not sent yet go, each thread to
/// its session.
fn send_all_button(ctx: ThreadsCtx, arrow: bool) -> impl IntoView {
    view! {
        <button class="shrink-0 rounded-md bg-sky-600 px-2 py-1 text-xs font-medium text-white hover:bg-sky-700 disabled:opacity-50"
            title="Your open comments that haven't gone to an agent: each thread goes to its session, which answers in it"
            disabled=move || ctx.sending.get() on:click=move |_| ctx.send_all.run(())>
            {move || format!("Send {} to agent{}", ctx.unsent.get(), if arrow { " ↑" } else { "" })}
        </button>
    }
}

/// "Used in N ▾": the projects and work that refer to a doc, as a menu
/// (shown only when there are some).
#[component]
fn UsedIn(items: Signal<Vec<(View, String)>>) -> impl IntoView {
    let open = RwSignal::new(false);
    let pop = StoredValue::new(crate::util::dismissable(
        move || open.get(),
        move || {
            open.try_set(false);
        },
    ));
    let n = Memo::new(move |_| items.with(Vec::len));
    move || {
        (n.get() > 0).then(|| {
            view! {
                <div class="relative shrink-0">
                    <button class=BAR data-pop=pop.get_value() aria-haspopup="menu" aria-expanded=move || open.get().to_string()
                        title="What refers to this doc" on:click=move |_| open.update(|o| *o = !*o)>
                        {move || format!("Used in {} ▾", n.get())}
                    </button>
                    {move || open.get().then(|| view! {
                        <div role="menu" data-pop=pop.get_value() on:click=move |_| open.set(false)
                            class="absolute right-0 top-full z-30 mt-1 w-64 max-w-[80vw] rounded-md border border-zinc-200 bg-white py-1 text-sm shadow-lg dark:border-zinc-700 dark:bg-zinc-900">
                            {items.get().into_iter().map(|(to, label)| view! {
                                <Go to class="block truncate px-3 py-1.5 text-sky-700 hover:bg-zinc-100 dark:text-sky-400 dark:hover:bg-zinc-800">{label}</Go>
                            }).collect_view()}
                        </div>
                    })}
                </div>
            }
        })
    }
}

/// The comments panel over the doc's right side, hanging from the bar; a
/// press outside it (or Escape) closes it.
#[component]
fn FloatingPanel(tile_h: RwSignal<f64>) -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let node = NodeRef::<leptos::html::Div>::new();
    dismiss(node, "[data-opens=comments]", move || {
        let _ = ctx.panel.try_set(false);
    });
    view! {
        <div node_ref=node class="absolute right-2 top-full z-10 mt-1 flex w-[26rem] max-w-[calc(100%-1rem)] flex-col overflow-hidden rounded-lg border border-zinc-200 bg-white shadow-xl dark:border-zinc-700 dark:bg-zinc-900"
            style=move || format!("max-height: {}px", (tile_h.get() - 72.0).max(240.0))>
            <CommentsPanel />
        </div>
    }
}

/// A sheet from the bottom of the screen (a phone's), over a dimmed page;
/// a tap above it or Escape closes it.
#[component]
fn Sheet(close: Callback<()>, children: Children) -> impl IntoView {
    let keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" && !ev.default_prevented() {
            close.run(());
        }
    });
    on_cleanup(move || keys.remove());
    view! {
        <div class="fixed inset-0 z-40 bg-black/30" on:click=move |_| close.run(())></div>
        <div class="fixed inset-x-0 bottom-0 z-50 flex max-h-[85dvh] flex-col overflow-hidden rounded-t-xl border-t border-zinc-200 bg-white shadow-2xl dark:border-zinc-700 dark:bg-zinc-900">
            {children()}
        </div>
    }
}

/// Comments (docs/docs.md §8): open or resolved threads, each with what
/// it's about; a new one on the whole tab; and your comments not sent to an
/// agent yet, to send.
#[component]
fn CommentsPanel() -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let resolved = RwSignal::new(false);
    let ids = Memo::new(move |_| {
        let resolved = resolved.get();
        ctx.shown.with(|ts| ts.iter().filter(|t| (t.state != "open") == resolved).map(|t| t.id.clone()).collect::<Vec<_>>())
    });
    // A new comment on the tab, or on what has no popover (a slide).
    let new_on = Memo::new(move |_| ctx.commenting.get().filter(|b| ctx.pop.with(|p| p.as_ref() != Some(b))));
    let seg = move |on: bool| {
        format!(
            "rounded px-2 py-0.5 {}",
            if on { "bg-white font-medium shadow-sm dark:bg-zinc-700" } else { "text-muted hover:text-zinc-900 dark:hover:text-zinc-100" }
        )
    };
    view! {
        <div class="flex shrink-0 items-center gap-2 border-b border-zinc-200 px-3 py-2 dark:border-zinc-800">
            <h2 class="flex-1 text-sm font-semibold">"Comments"</h2>
            <div class="flex rounded-md bg-zinc-100 p-0.5 text-xs dark:bg-zinc-800" role="group">
                <button class=move || seg(!resolved.get()) aria-pressed=move || (!resolved.get()).to_string() on:click=move |_| resolved.set(false)>"Open"</button>
                <button class=move || seg(resolved.get()) aria-pressed=move || resolved.get().to_string() on:click=move |_| resolved.set(true)>"Resolved"</button>
            </div>
            <button class="rounded px-1.5 text-lg leading-none text-muted hover:bg-zinc-100 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                title="Close" aria-label="Close the comments" on:click=move |_| ctx.panel.set(false)>"×"</button>
        </div>
        <div class="min-h-0 flex-1 space-y-3 overflow-y-auto overscroll-contain p-3 text-sm">
            <button class="text-xs text-muted hover:text-zinc-900 hover:underline dark:hover:text-zinc-100"
                on:click=move |_| { ctx.focus.set(Some(String::new())); ctx.commenting.set(Some(String::new())) }>"+ On the whole tab"</button>
            {move || new_on.get().map(|b| view! { <NewComment block=b done=Callback::new(move |()| ctx.commenting.set(None)) /> })}
            <For each=move || ids.get() key=|id| id.clone() children=move |id| view! { <Thread id about=true /> } />
            {move || ids.with(Vec::is_empty).then(|| view! {
                <p class="text-muted">{if resolved.get() { "No resolved comments." } else { "No open comments. Press 💬 by a block to start one." }}</p>
            })}
        </div>
        {move || (ctx.unsent.get() > 0).then(|| view! {
            <div class="flex shrink-0 items-center gap-2 border-t border-zinc-200 px-3 py-2 dark:border-zinc-800">
                <span class="flex-1 text-xs text-muted">{move || format!("{} not sent yet", ctx.unsent.get())}</span>
                {send_all_button(ctx, false)}
            </div>
        })}
    }
}

/// A block's comments, in a popover under its 💬 over the doc: a press
/// outside it or Escape closes it.
#[component]
fn BlockPop(block: String) -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let node = NodeRef::<leptos::html::Div>::new();
    dismiss(node, "[data-marks]", move || ctx.close_pop());
    view! {
        <div node_ref=node class="absolute z-30 max-h-[70vh] w-96 max-w-[calc(100%-1rem)] space-y-2 overflow-y-auto overscroll-contain rounded-lg border border-zinc-200 bg-white p-2 text-sm shadow-xl dark:border-zinc-700 dark:bg-zinc-900"
            style=move || ctx.pop_at.with(|(top, right)| format!("top: {top}px; right: {right}px"))>
            <PopBody block />
        </div>
    }
}

/// What a block's popover holds: its open threads, and a new one.
#[component]
fn PopBody(block: String) -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let bid = StoredValue::new(block);
    let ids = Memo::new(move |_| {
        ctx.shown.with(|ts| {
            ts.iter().filter(|t| t.state == "open" && t.anchor.block == bid.get_value()).map(|t| t.id.clone()).collect::<Vec<_>>()
        })
    });
    let writing = Memo::new(move |_| ctx.commenting.with(|c| c.as_deref() == Some(bid.get_value().as_str())));
    let close = Callback::new(move |()| ctx.close_pop());
    view! {
        <div class="flex items-center justify-end gap-2 text-xs">
            <button class="text-muted hover:text-zinc-900 hover:underline dark:hover:text-zinc-100"
                on:click=move |_| { ctx.close_pop(); ctx.panel.set(true); }>"All comments ›"</button>
            <button class="rounded px-1 text-base leading-none text-muted hover:bg-zinc-100 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                title="Close" aria-label="Close" on:click=move |_| close.run(())>"×"</button>
        </div>
        <For each=move || ids.get() key=|id| id.clone() children=move |id| view! { <Thread id about=false /> } />
        {move || if writing.get() {
            view! { <NewComment block=bid.get_value() done=close /> }.into_any()
        } else {
            view! {
                <button class="text-xs text-muted hover:text-zinc-900 hover:underline dark:hover:text-zinc-100"
                    on:click=move |_| { ctx.focus.set(Some(bid.get_value())); ctx.commenting.set(Some(bid.get_value())) }>"+ New thread on this block"</button>
            }.into_any()
        }}
    }
}

/// A refusal as you read it: someone else's edit won.
fn conflict_text(e: &str) -> String {
    if e.contains("guard_mismatch") {
        format!("Someone changed that block while you were editing it; their edit wins and is shown now. ({e})")
    } else {
        e.to_owned()
    }
}

/// The block being edited: its draft, and the version and hash it was
/// read at (the guard its change is sent with). It lives with the
/// document, so a live update while you type keeps your draft.
#[derive(Clone)]
struct Editing {
    id: String,
    rev: i64,
    hash: String,
    /// What it said when you started (or last saved).
    original: String,
    chips: Map<String, Value>,
    /// The block as it was drawn when you opened it: drawn so until you're
    /// done, whatever changes it meanwhile.
    block: DocBlock,
}

/// The draft of the block being edited, and a count of keystrokes (to
/// save once you pause). Made by the document, so they outlive its blocks.
#[derive(Clone, Copy)]
struct Draft {
    text: RwSignal<String>,
    typed: RwSignal<u32>,
}

/// Sends a block's draft, guarded by what it was read at. `keep`: you go on
/// editing (a pause), with the guard moved to what you saved. Emptied, the
/// block goes.
fn save(
    doc_id: String,
    edit: RwSignal<Option<Editing>>,
    draft: Draft,
    keep: bool,
    error: RwSignal<Option<String>>,
    reload: impl Fn() + 'static,
) {
    let Some(e) = edit.get_untracked() else { return };
    if !keep {
        edit.set(None);
    }
    let md = draft.text.get_untracked();
    if md == e.original {
        return;
    }
    if keep && md.trim().is_empty() {
        return;
    }
    let op = if md.trim().is_empty() {
        DocOp::Delete { id: e.id.clone(), if_rev: Some(e.rev), if_hash: Some(e.hash.clone()) }
    } else {
        DocOp::Replace {
            id: e.id.clone(),
            markdown: Some(md.clone()),
            blocks: e.chips.clone(),
            find: None,
            with_text: None,
            nth: None,
            cell: None,
            text: None,
            layout: None,
            notes: None,
            hidden: None,
            if_rev: Some(e.rev),
            if_hash: Some(e.hash.clone()),
        }
    };
    spawn_local(async move {
        match api::doc_ops(doc_id, &DocBatch { ops: vec![op], answering: String::new() }).await {
            Ok(a) => {
                if keep {
                    edit.update(|x| {
                        if let Some(x) = x.as_mut().filter(|x| x.id == e.id) {
                            x.rev = a.rev;
                            x.hash = a.hashes.get(&e.id).and_then(Value::as_str).unwrap_or(&x.hash).to_owned();
                            x.original = md;
                        }
                    });
                }
                reload();
            }
            Err(err) => {
                // Their edit wins: yours stays in the message to copy.
                if edit.get_untracked().is_some_and(|x| x.id == e.id) {
                    edit.set(None);
                }
                error.set(Some(format!("{}\n\nWhat you wrote:\n{md}", conflict_text(&err))));
                reload();
            }
        }
    });
}

/// One block: read (its links and chips work, and text can be selected),
/// or open in Markdown: by ✎ in its margin, a double-click, or a click in
/// edit mode. Done or Escape saves it and goes back to reading.
#[component]
fn BlockView(
    doc_id: StoredValue<String>,
    b: DocBlock,
    kind: String,
    doc_rev: Memo<i64>,
    edit: RwSignal<Option<Editing>>,
    draft: Draft,
    error: RwSignal<Option<String>>,
    reload: Callback<()>,
    edit_all: RwSignal<bool>,
) -> impl IntoView {
    let bid = StoredValue::new(b.id.clone());
    let block = StoredValue::new(b.clone());
    let pending = b.kind == "pending";
    // Its open comment threads: a marker with their count.
    let threads = expect_context::<ThreadsCtx>();
    let open_n =
        Memo::new(move |_| threads.shown.with(|ts| ts.iter().filter(|t| t.state == "open" && t.anchor.block == bid.get_value()).count()));
    let popped = Memo::new(move |_| threads.pop.with(|p| p.as_deref() == Some(bid.get_value().as_str())));
    let hover = format!("{} · last written by {} (version {})", b.kind, author_named(&b.by), b.rev);
    let chip_ctx = use_context::<ChipCtx>();
    let (col, ws) = (use_column(), use_workspace());
    let is_open = Memo::new(move |_| edit.with(|e| e.as_ref().is_some_and(|e| e.id == bid.get_value())));
    let begin = move |text: String| {
        let b = block.get_value();
        edit.set(Some(Editing {
            id: b.id.clone(),
            rev: doc_rev.get_untracked(),
            hash: b.hash.clone(),
            original: text.clone(),
            chips: b.attrs.get("chips").and_then(Value::as_object).cloned().unwrap_or_default(),
            block: b,
        }));
        draft.text.set(text);
    };
    let start = move || {
        let b = block.get_value();
        if b.kind == "pending" || is_open.get_untracked() {
            return;
        }
        // Another block being edited is saved first.
        if edit.get_untracked().is_some() {
            save(doc_id.get_value(), edit, draft, false, error, move || reload.run(()));
        }
        if b.kind == "slide" || b.kind == "embed" {
            // Its Markdown comes from the head (a slide's header and regions).
            spawn_local(async move {
                if let Ok(v) = api::doc_read(&doc_id.get_value(), &format!("view=block&id={}", api::enc(&b.id))).await {
                    begin(v["markdown"].as_str().unwrap_or("").to_owned());
                }
            });
            return;
        }
        begin(b.text.clone());
    };
    let finish = move |keep: bool| {
        if is_open.get_untracked() {
            save(doc_id.get_value(), edit, draft, keep, error, move || reload.run(()));
        }
    };
    let typed = move || {
        draft.typed.update(|n| *n += 1);
        let n = draft.typed.get_untracked();
        // Saved once you pause. Only values the document owns are used:
        // a live update may have made this block anew by then.
        let (id, doc) = (bid.get_value(), doc_id.get_value());
        set_timeout(
            move || {
                if draft.typed.try_get_untracked() == Some(n)
                    && edit.try_with_untracked(|e| e.as_ref().is_some_and(|e| e.id == id)) == Some(true)
                {
                    save(doc, edit, draft, true, error, move || reload.run(()));
                }
            },
            std::time::Duration::from_millis(1500),
        );
    };
    let marker = move |ev: web_sys::MouseEvent, new: bool| {
        use wasm_bindgen::JsCast;
        if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
            threads.toggle_pop(bid.get_value(), &el, new);
        }
    };
    view! {
        <div class="group relative rounded py-0.5 pl-2 pr-14 hover:bg-zinc-50 dark:hover:bg-zinc-900/60" title=hover data-block=b.id.clone()>
            {move || if is_open.get() {
                let area = NodeRef::<leptos::html::Textarea>::new();
                crate::util::autosize(area, move || draft.text.track());
                Effect::new(move || {
                    if let Some(el) = area.get() {
                        let _ = el.focus();
                        let end = el.value().encode_utf16().count() as u32;
                        let _ = el.set_selection_range(end, end);
                    }
                });
                view! {
                    <div class="space-y-1 py-1">
                        <textarea node_ref=area class=format!("{INPUT} block min-h-[3rem] resize-none font-mono text-sm")
                            prop:value=move || draft.text.get() on:input=move |ev| { draft.text.set(event_target_value(&ev)); typed(); }
                            on:blur=move |_| finish(false)
                            on:keydown=move |ev| if ev.key() == "Escape" { ev.prevent_default(); finish(false) } else if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key()) { finish(false) } />
                        <div class="flex justify-end">
                            // Pressed, the box keeps the focus (no blur) until Done saves.
                            <button class="rounded-md bg-zinc-900 px-2.5 py-1 text-xs font-medium text-white hover:bg-zinc-700 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300"
                                title="Save it and go back to reading (Esc, ⌘↵)" on:mousedown=|ev| ev.prevent_default() on:click=move |_| finish(false)>"Done"</button>
                        </div>
                    </div>
                }.into_any()
            } else {
                let b = block.get_value();
                view! {
                    <div class=move || if edit_all.get() { "cursor-text" } else { "" }
                        on:click=move |ev| {
                            if let Some(c) = chip_ctx && chip_clicked(&ev, &block.get_value(), c, col, ws) { return; }
                            if crate::mentions::follow_link(&ev, col, ws) { return; }
                            if edit_all.get_untracked() { start() }
                        }
                        on:dblclick=move |ev: web_sys::MouseEvent| {
                            use wasm_bindgen::JsCast;
                            // Not on what a click does something with.
                            let on_control = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                                .is_some_and(|el| el.closest("a,button,input,select,textarea,figure,[data-chip],[data-ref]").ok().flatten().is_some());
                            if !on_control { start() }
                        }>
                        <Shown b=b kind=kind.clone() />
                    </div>
                }.into_any()
            }}
            {move || chip_ctx.and_then(|c| c.editing.get().filter(|(b, _, _)| *b == bid.get_value() || block.get_value().children.iter().any(|x| x.id == *b)).map(|(b, k, v)| view! {
                <ChipEditor block=b key=k value=v ctx=c />
            }))}
            // In its right margin, inside the block: outside it, the pointer
            // left the block on its way and the buttons hid, and at the
            // panel's edge they sat over the scrollbar. ✎ and 💬 show on
            // hover (always on a touch screen, which has none); with open
            // comments, a lasting marker with their count opens them.
            <div data-marks="" class="absolute right-0.5 top-0.5 flex items-center">
                {move || (!pending && !is_open.get()).then(|| view! {
                    <button class="invisible rounded px-1 text-xs text-muted hover:bg-zinc-100 hover:text-zinc-900 group-hover:visible pointer-coarse:visible dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                        title="Edit this block in Markdown (or double-click it)" aria-label="Edit this block" on:click=move |_| start()>"✎"</button>
                })}
                {move || match open_n.get() {
                    0 => view! {
                        <button class="invisible rounded px-1 text-xs text-muted hover:bg-zinc-100 group-hover:visible pointer-coarse:visible dark:hover:bg-zinc-800"
                            title="Comment on this" aria-label="Comment on this block" on:click=move |ev| marker(ev, true)>"💬"</button>
                    }.into_any(),
                    n => view! {
                        <button class=move || format!("rounded px-1 text-xs font-medium text-soft hover:bg-zinc-100 dark:hover:bg-zinc-800 {}", if popped.get() { "bg-zinc-100 dark:bg-zinc-800" } else { "" })
                            title="Its comments" on:click=move |ev| marker(ev, false)>{format!("💬 {n}")}</button>
                    }.into_any(),
                }}
            </div>
        </div>
    }
}

/// A block as it reads.
#[component]
fn Shown(b: DocBlock, kind: String) -> impl IntoView {
    match b.kind.as_str() {
        "pending" => {
            let who = b.attrs["by"].as_str().map(author_named).unwrap_or_else(|| "an agent".into());
            view! {
                <div class="rounded border border-dashed border-violet-300 px-3 py-2 text-sm text-violet-800 dark:border-violet-800 dark:text-violet-300">
                    <span class="animate-pulse">"✎ "</span>{b.attrs["intent"].as_str().unwrap_or("").to_owned()}
                    <span class="text-xs text-violet-500">{format!(" — {who} is writing this")}</span>
                </div>
            }
            .into_any()
        }
        "slide" => {
            let layout = b.attrs["layout"].as_str().unwrap_or("body").to_owned();
            let notes = b.attrs["notes"].as_str().unwrap_or("").to_owned();
            let hidden = b.attrs["hidden"] == true;
            view! {
                <div class=format!("my-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800 {}", if hidden { "opacity-50" } else { "" })>
                    <div class="mb-1 text-[11px] uppercase tracking-wide text-muted">{format!("slide · {layout}{}", if hidden { " · hidden" } else { "" })}</div>
                    {b.children.iter().map(|c| {
                        let refs = use_context::<ChipCtx>().map(|c| c.refs);
                        let c = c.clone();
                        if c.kind == "embed" {
                            view! { <Embed v=c.attrs.clone() /> }.into_any()
                        } else {
                            view! { <div class="md" inner_html=move || chips_html(&c.text, &c.attrs, &refs.map(|r| r.get()).unwrap_or_default())></div> }.into_any()
                        }
                    }).collect_view()}
                    {(!notes.is_empty()).then(|| view! { <p class="mt-2 border-t border-zinc-100 pt-1 text-xs text-muted dark:border-zinc-900">{format!("Notes: {notes}")}</p> })}
                </div>
            }
            .into_any()
        }
        "embed" => view! { <Embed v=b.attrs.clone() /> }.into_any(),
        _ => {
            let _ = kind;
            let refs = use_context::<ChipCtx>().map(|c| c.refs);
            view! { <div class="md" inner_html=move || chips_html(&b.text, &b.attrs, &refs.map(|r| r.get()).unwrap_or_default())></div> }
                .into_any()
        }
    }
}

/// What a thread is about, as its card's quote box shows it: a picture's
/// or diagram's thumbnail, and words.
#[derive(Clone, PartialEq)]
struct Subject {
    thumb: Option<String>,
    text: String,
    /// It's in the doc, to go to.
    here: bool,
}

/// What a thread on `block` is about: the block's first line, a picture
/// or diagram with its caption, "Whole tab"; once gone, what it said
/// (`about`).
fn subject(tabs: &[DocTab], block: &str, about: &str, doc: &str) -> Subject {
    let words = |text: String| Subject { thumb: None, text, here: true };
    if block.is_empty() {
        return Subject { thumb: None, text: "Whole tab".into(), here: false };
    }
    let found =
        tabs.iter().flat_map(|t| &t.blocks).find_map(|b| if b.id == block { Some(b) } else { b.children.iter().find(|c| c.id == block) });
    let Some(b) = found else {
        return Subject { thumb: None, text: first_line(about, &Value::Null), here: false };
    };
    let v = &b.attrs;
    let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
    let either = |a: String, b: &str| if a.trim().is_empty() { b.to_owned() } else { a };
    match (b.kind.as_str(), v["type"].as_str().unwrap_or("")) {
        ("embed", "image") => Subject {
            thumb: Some(api::url(&format!("/api/v1/docs/{}/files/{}", api::enc(doc), api::enc(&s("blob"))))),
            text: either(
                if s("caption").is_empty() { if s("alt").is_empty() { s("name") } else { s("alt") } } else { s("caption") },
                "A picture",
            ),
            here: true,
        },
        ("embed", "diagram") => Subject {
            thumb: Some(format!("data:image/svg+xml;base64,{}", base64_of(s("svg").as_bytes()))),
            text: either(s("title"), "A diagram"),
            here: true,
        },
        ("embed", "chart") => words(either(s("title"), "A chart")),
        ("embed", "file") => words(format!("📎 {}", s("name"))),
        ("embed", "view") => words(format!("View · {}", s("view"))),
        ("slide", _) => {
            words(either(b.children.iter().map(|c| first_line(&c.text, &c.attrs)).find(|l| !l.is_empty()).unwrap_or_default(), "A slide"))
        }
        _ => words(either(first_line(&b.text, &b.attrs), &first_line(about, &Value::Null))),
    }
}

/// A block's Markdown as a line of plain words: its first line with text,
/// without the marks that make it a heading, an item or a quote, or bold,
/// code or a link; its chips as they read.
fn first_line(md: &str, attrs: &Value) -> String {
    let chips = attrs.get("chips").and_then(Value::as_object);
    let mut text = String::with_capacity(md.len());
    let mut rest = md;
    while let Some(i) = rest.find("<?c6 block ") {
        let after = &rest[i + 11..];
        let Some(j) = after.find("?>") else { break };
        let key = after[..j].trim();
        text.push_str(&rest[..i]);
        let v = chips.and_then(|c| c.get(key)).cloned().unwrap_or(Value::Null);
        let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
        text.push_str(&match v["type"].as_str().unwrap_or("") {
            "mention" => format!("@{}", if s("name").is_empty() { s("user") } else { s("name") }),
            "ref" => {
                if s("title").is_empty() {
                    s("ref")
                } else {
                    s("title")
                }
            }
            _ => s("value"),
        });
        rest = &after[j + 2..];
    }
    text.push_str(rest);
    let is_rule = |l: &str| l.len() >= 3 && l.chars().all(|c| matches!(c, '-' | '*' | '_' | ' ')) && !l.contains(char::is_alphanumeric);
    let is_table_rule = |l: &str| l.starts_with('|') && l.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '));
    let Some(line) = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("```") && !l.starts_with("~~~") && !is_rule(l) && !is_table_rule(l))
    else {
        return String::new();
    };
    // What starts it: #, >, a bullet, a number, a task's box.
    let mut l = line;
    loop {
        let before = l;
        l = l.trim_start_matches('#').trim_start_matches('>').trim_start();
        for lead in ["- [ ] ", "- [x] ", "- [X] ", "* [ ] ", "* [x] ", "- ", "* ", "+ "] {
            if let Some(r) = l.strip_prefix(lead) {
                l = r;
            }
        }
        let digits = l.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && (l[digits..].starts_with(". ") || l[digits..].starts_with(") ")) {
            l = &l[digits + 2..];
        }
        l = l.trim_start();
        if l == before {
            break;
        }
    }
    // Links and pictures as their words; emphasis and code marks go.
    let mut out = String::with_capacity(l.len());
    let mut chars = l.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '!' if l[i + 1..].starts_with('[') => {}
            '[' => {
                if let Some(close) = l[i..].find("](")
                    && let Some(end) = l[i + close..].find(')')
                {
                    out.push_str(&l[i + 1..i + close]);
                    let skip_to = i + close + end;
                    while chars.peek().is_some_and(|(j, _)| *j <= skip_to) {
                        chars.next();
                    }
                } else {
                    out.push(c);
                }
            }
            '*' | '`' => {}
            '~' if l[i + 1..].starts_with('~') || l[..i].ends_with('~') => {}
            '_' if l[i + 1..].starts_with('_') || l[..i].ends_with('_') => {}
            '|' => {
                if !out.trim().is_empty() && !l[i + 1..].trim().is_empty() {
                    out.push_str(" · ");
                }
            }
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ").replace("· ·", "·")
}

/// A thread's quote box: what it's about, and ↗ to go to it in the doc.
#[component]
fn About(t: Memo<DocThread>) -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let what = Memo::new(move |_| {
        let (block, about) = t.with(|t| (t.anchor.block.clone(), t.about.clone()));
        ctx.tabs.with(|ts| subject(ts, &block, &about, &ctx.doc.get_value()))
    });
    view! {
        <div class="flex items-start gap-2 border-l-2 border-zinc-300 pl-2 dark:border-zinc-600">
            {move || what.with(|w| w.thumb.clone()).map(|src| view! {
                <img src=src alt="" class="h-8 w-8 shrink-0 rounded border border-zinc-200 bg-white object-contain dark:border-zinc-700" />
            })}
            <div class="min-w-0 flex-1">
                <p class="line-clamp-2 break-words">{move || what.with(|w| w.text.clone())}</p>
                {move || t.with(|t| t.anchor.quote.clone()).chars().next().is_some().then(|| view! {
                    <p class="truncate text-xs text-muted">{move || format!("“{}”", t.with(|t| t.anchor.quote.clone()))}</p>
                })}
            </div>
            {move || (what.with(|w| w.here) && !ctx.deck.get()).then(|| view! {
                <button class="shrink-0 rounded px-1 text-muted hover:bg-zinc-100 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                    title="Show it in the doc" aria-label="Show it in the doc"
                    on:click=move |_| ctx.reveal.run(vec![t.with_untracked(|t| t.anchor.block.clone())])>"↗"</button>
            })}
        </div>
    }
}

/// Writing a new comment: on a block (`block`, in its popover) or on the
/// whole tab or a slide (in the panel). `done`: Cancel or Escape (the
/// draft stays).
#[component]
fn NewComment(block: String, done: Callback<()>) -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let doc = ctx.doc;
    let error = RwSignal::new(None::<String>);
    // Kept as drafts: the box goes when its popover does, and may be
    // drawn again.
    let keys = StoredValue::new((format!("comment.{}.{block}", doc.get_value()), format!("comment-quote.{}.{block}", doc.get_value())));
    let (text, quote) = keys.with_value(|(t, q)| (crate::util::draft(t), crate::util::draft(q)));
    let to_agent = RwSignal::new(false);
    let on_block = !block.is_empty();
    let what = if !on_block {
        "Comment on the whole tab"
    } else if ctx.tabs.with_untracked(|ts| ts.iter().flat_map(|t| &t.blocks).any(|b| b.id == block && b.kind == "slide")) {
        "Comment on this slide"
    } else {
        "Comment on this block"
    };
    let block = StoredValue::new(block);
    let writing = NodeRef::<leptos::html::Textarea>::new();
    let focus = ctx.focus;
    Effect::new(move || {
        if let Some(el) = writing.get()
            && focus.get_untracked().is_some_and(|b| block.with_value(|x| *x == b))
        {
            focus.set(None);
            let _ = el.focus();
        }
    });
    crate::util::autosize(writing, move || text.track());
    let post = move || {
        let (said, quoted) = (text.get_untracked(), quote.get_untracked());
        let t = c6_api::docs::ThreadNew {
            anchor: c6_api::docs::Anchor { block: block.get_value(), quote: quoted.clone(), tab: String::new() },
            text: said.clone(),
            to: if to_agent.get_untracked() { "agent".into() } else { String::new() },
        };
        // The draft goes as it's sent, not when the answer comes: a box
        // drawn again in between would otherwise find the draft and keep it.
        keys.with_value(|(t, q)| {
            crate::util::forget_draft(t);
            crate::util::forget_draft(q);
        });
        spawn_local(async move {
            match api::doc_thread_new(doc.get_value(), &t).await {
                Ok(_) => {
                    // The popover stays, showing the new thread.
                    let _ = ctx.commenting.try_set(None);
                    let _ = ctx.tick.try_update(|t| *t += 1);
                }
                // Not posted: what you wrote comes back.
                Err(e) => {
                    keys.with_value(|(t, q)| {
                        crate::util::storage_set(t, &said);
                        crate::util::storage_set(q, &quoted);
                    });
                    let _ = text.try_set(said);
                    let _ = quote.try_set(quoted);
                    let _ = error.try_set(Some(e));
                }
            }
        });
    };
    view! {
        <div class="space-y-1 rounded-lg border border-zinc-200 p-2 dark:border-zinc-800">
            <div class="text-xs text-muted">{what}</div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {on_block.then(|| view! {
                <input class=INPUT placeholder="A few of its words to point at (optional)" prop:value=move || quote.get() on:input=move |ev| quote.set(event_target_value(&ev)) />
            })}
            <textarea node_ref=writing rows="1" class=format!("{INPUT} {COMMENT_GROWS}") placeholder="Your comment" prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))
                on:keydown=move |ev| if ev.key() == "Escape" { ev.prevent_default(); done.run(()) } else if crate::util::sends(&ev) { ev.prevent_default(); post() } />
            <div class="flex items-center gap-2">
                <div class="flex-1">
                    <AgentSwitch on=to_agent title="The session that last wrote it gets the thread (queued if busy), else the newest one of the document's work or project" />
                </div>
                <button class=BUTTON_QUIET on:click=move |_| done.run(())>"Cancel"</button>
                <button class=BUTTON on:click=move |_| post()>"Comment"</button>
            </div>
        </div>
    }
}

/// A comment box: one line, growing with its text to about eight, then
/// scrolling.
const COMMENT_GROWS: &str = "block max-h-44 resize-none overflow-y-auto";

/// "Send to agent", as a switch: what you write goes on to an agent.
#[component]
fn AgentSwitch(on: RwSignal<bool>, #[prop(optional, into)] title: String) -> impl IntoView {
    view! {
        <button type="button" role="switch" aria-checked=move || on.get().to_string() title=title
            class="inline-flex items-center gap-1.5 rounded text-xs text-soft" on:click=move |_| on.update(|v| *v = !*v)>
            <span class=move || format!("relative h-3.5 w-[26px] shrink-0 rounded-full transition-colors {}", if on.get() { "bg-sky-600" } else { "bg-zinc-300 dark:bg-zinc-600" })>
                <span class=move || format!("absolute left-0.5 top-0.5 h-2.5 w-2.5 rounded-full bg-white shadow-sm transition-transform {}", if on.get() { "translate-x-3" } else { "" })></span>
            </span>
            "Send to agent"
        </button>
    }
}

/// A thread waiting on its agent, live: working on it, or yet to start.
/// (Who the agent is shows on its comments.)
#[component]
fn AgentOnIt(session: String) -> impl IntoView {
    let live = api::live();
    let running =
        move || live.sessions.with(|ss| ss.iter().any(|s| s.id == session && s.display_status() == c6_api::SessionStatus::Running));
    move || {
        if running() {
            view! {
                <span class="inline-flex items-center gap-1">
                    <span class="h-1.5 w-1.5 shrink-0 animate-pulse rounded-full bg-zinc-400"></span>"working…"
                </span>
            }
            .into_any()
        } else {
            view! { <span>"sent to the agent"</span> }.into_any()
        }
    }
}

/// A session's mark, kept when it has left the live list.
fn session_mark(id: &str) -> String {
    crate::marks::of_id(id).unwrap_or_else(|| c6_api::marks::hashed(id).to_owned())
}

/// An agent's name on its comments: its session's mark and title, short,
/// linked to the session.
#[component]
fn AgentName(id: String) -> impl IntoView {
    let live = api::live();
    let (mid, tid) = (id.clone(), id.clone());
    let title = move || {
        live.sessions
            .with(|ss| ss.iter().find(|s| s.id == tid).map(|s| s.title.clone()))
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| "an agent".into())
    };
    view! {
        <span class="h-2 w-2 shrink-0 rounded-full" style=move || crate::marks::fill(&session_mark(&mid))></span>
        <Go to=View::session(id) class="min-w-0 max-w-48 truncate hover:underline" stop=true>{title}</Go>
    }
}

/// One comment: yours on the right, a quiet grey-blue; anyone else's on
/// the left, grey, an agent's named by its session (its colour on the dot
/// by the name). `under`: what goes beneath it (that it changed the doc).
fn said(c: &c6_api::docs::DocComment, under: Option<AnyView>) -> AnyView {
    let when = since(c.at).trim_end_matches(" ago").to_owned();
    let text = c.text.clone();
    if c.by == "you" {
        return view! {
            <div class="flex flex-col items-end">
                <div class="mb-0.5 text-xs text-muted">{format!("You · {when}")}</div>
                <p class="max-w-[85%] whitespace-pre-wrap break-words rounded-xl bg-sky-50 px-3 py-1.5 text-zinc-900 dark:bg-sky-950/60 dark:text-zinc-100">{text}</p>
                {under}
            </div>
        }
        .into_any();
    }
    let who = match c.by.strip_prefix("session:") {
        Some(id) => view! { <AgentName id=id.to_owned() /> }.into_any(),
        None => view! { <span class="truncate">{author(&c.by)}</span> }.into_any(),
    };
    view! {
        <div class="flex flex-col items-start">
            <div class="mb-0.5 flex max-w-full items-center gap-1 text-xs text-muted">
                {who}<span class="shrink-0">{format!("· {when}")}</span>
            </div>
            <p class="max-w-[85%] whitespace-pre-wrap break-words rounded-xl bg-zinc-100 px-3 py-1.5 dark:bg-zinc-800">{text}</p>
            {under}
        </div>
    }
    .into_any()
}

/// "changed the doc · Show": Show flashes the blocks its edits made (those
/// last written in one of its versions), else the block it's on.
fn changed_the_doc(ctx: ThreadsCtx, t: &DocThread) -> AnyView {
    let (edits, anchor) = (t.edits.clone(), t.anchor.block.clone());
    let show = move |_| {
        let mut ids = ctx.tabs.with_untracked(|ts| {
            ts.iter()
                .flat_map(|t| &t.blocks)
                .flat_map(|b| std::iter::once(b).chain(&b.children))
                .filter(|b| edits.contains(&b.rev))
                .map(|b| b.id.clone())
                .collect::<Vec<_>>()
        });
        if ids.is_empty() && !anchor.is_empty() {
            ids.push(anchor.clone());
        }
        ctx.reveal.run(ids);
    };
    view! {
        <div class="mt-1 text-xs text-muted">
            "changed the doc · "
            <button class="underline hover:text-zinc-900 dark:hover:text-zinc-100" title="Flash what it changed" on:click=show>"Show"</button>
        </div>
    }
    .into_any()
}

/// One thread: a conversation (yours on the right, an agent's on the
/// left), the newest last, just above the box to reply; in the panel,
/// what it's about first (`about`). Drawn once: as comments come in, only
/// they are drawn again, so the reply box keeps what you type, and the
/// cursor.
#[component]
fn Thread(id: String, about: bool) -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let (doc, tick) = (ctx.doc, ctx.tick);
    let tid = StoredValue::new(id);
    let t = Memo::new(move |_| ctx.shown.with(|ts| ts.iter().find(|t| t.id == tid.get_value()).cloned().unwrap_or_default()));
    // A draft: the thread may be drawn again (in the panel and a popover).
    let key = StoredValue::new(format!("reply.{}.{}", doc.get_value(), tid.get_value()));
    let reply = crate::util::draft(&key.get_value());
    // With an agent already, a reply goes on to it unless you say not.
    let to_agent = RwSignal::new(t.with_untracked(|t| t.to_agent && !t.agent.is_empty()));
    let error = RwSignal::new(None::<String>);
    let done = move |r: Result<DocThread, String>| match r {
        Ok(_) => {
            let _ = error.try_set(None);
            let _ = tick.try_update(|t| *t += 1);
        }
        Err(e) => {
            let _ = error.try_set(Some(e));
        }
    };
    // The draft goes as it's sent, and comes back if sending fails.
    let send = move || {
        let c = c6_api::docs::CommentNew {
            text: reply.get_untracked(),
            to: if to_agent.get_untracked() { "agent".into() } else { String::new() },
        };
        if c.text.trim().is_empty() {
            return;
        }
        crate::util::forget_draft(&key.get_value());
        reply.set(String::new());
        spawn_local(async move {
            let r = api::doc_thread_reply(doc.get_value(), tid.get_value(), &c).await;
            if r.is_err() {
                crate::util::storage_set(&format!("c6.draft.{}", key.get_value()), &c.text);
                let _ = reply.try_set(c.text.clone());
            }
            done(r)
        });
    };
    // Its last word, yours, hasn't gone to an agent: send it now.
    let send_unsent = move |_| {
        spawn_local(async move {
            match api::doc_threads_send(doc.get_value(), vec![tid.get_value()]).await {
                Ok(_) => {
                    let _ = tick.try_update(|t| *t += 1);
                }
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
        });
    };
    let area = NodeRef::<leptos::html::Textarea>::new();
    crate::util::autosize(area, move || reply.track());
    // As comments come in: scrolled to the newest.
    let list = NodeRef::<leptos::html::Div>::new();
    Effect::new(move || {
        t.with(|t| t.comments.len());
        if list.get().is_some() {
            request_animation_frame(move || {
                if let Some(el) = list.try_get_untracked().flatten() {
                    el.set_scroll_top(el.scroll_height());
                }
            });
        }
    });
    let open = Memo::new(move |_| t.with(|t| t.state == "open"));
    // Where it stands: what it was about is gone, the agent working, not
    // sent yet, resolved, your move.
    let status = move || {
        t.with(|t| {
            let waiting = t.to_agent && !t.needs_you && t.comments.last().is_some_and(|c| c.by != format!("session:{}", t.agent));
            let resolved = t.state != "open";
            (t.detached || waiting || t.unsent || resolved || t.needs_you).then(|| {
                view! {
                    <div class="flex flex-wrap items-center gap-2 text-xs text-muted">
                        {t.detached.then(|| view! { <span class="rounded bg-zinc-100 px-1 dark:bg-zinc-800" title="What it was about is gone or changed">"detached"</span> })}
                        {waiting.then(|| view! { <AgentOnIt session=t.agent.clone() /> })}
                        {t.unsent.then(|| view! {
                            <span>"not sent yet"</span>
                            <button class="text-sky-700 hover:underline dark:text-sky-400" title="Your last comment here hasn't gone to an agent: send the thread now"
                                on:click=send_unsent>"Send now"</button>
                        })}
                        {resolved.then(|| view! { <span>{format!("resolved by {}", author(&t.resolved_by))}</span> })}
                        <span class="flex-1"></span>
                        {t.needs_you.then(|| view! { <span class="rounded-full bg-amber-100 px-2 py-0.5 font-medium text-amber-800 dark:bg-amber-900/50 dark:text-amber-300">"Your move"</span> })}
                    </div>
                }
            })
        })
    };
    // That it changed the doc goes under the agent's newest comment (after
    // them all if it hasn't written one): which comment made which edit
    // isn't kept.
    let comments = move || {
        t.with(|t| {
            let mut changed = (!t.edits.is_empty()).then(|| changed_the_doc(ctx, t));
            let last_agent = t.comments.iter().rposition(|c| c.by.starts_with("session:"));
            let list = t
                .comments
                .iter()
                .enumerate()
                .map(|(i, c)| said(c, if Some(i) == last_agent { changed.take() } else { None }))
                .collect_view();
            view! { {list}{changed} }
        })
    };
    view! {
        <div class="space-y-2 rounded-lg border border-zinc-200 p-2 text-sm dark:border-zinc-800" data-thread=tid.get_value()>
            {about.then(|| view! { <About t /> })}
            {status}
            <div node_ref=list class="max-h-96 space-y-2 overflow-y-auto">{comments}</div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="relative">
                <textarea node_ref=area rows="1" class=format!("{INPUT} {COMMENT_GROWS} pr-9") placeholder="Reply…" prop:value=move || reply.get() on:input=move |ev| reply.set(event_target_value(&ev))
                    on:keydown=move |ev| if crate::util::sends(&ev) { ev.prevent_default(); send() }></textarea>
                // Send, as the message box's: a soft square, sky once there's text.
                <button class=move || format!("absolute bottom-1.5 right-1.5 flex h-[22px] w-[22px] items-center justify-center rounded-md {}", if reply.with(|r| r.trim().is_empty()) { "bg-zinc-600 text-zinc-200" } else { "bg-sky-600 text-white" })
                    title="Send the reply (Enter)" aria-label="Send the reply" on:click=move |_| send()>
                    <svg viewBox="0 0 16 16" class="h-3 w-3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                        <path d="M8 13V3M3.5 7.5 8 3l4.5 4.5" />
                    </svg>
                </button>
            </div>
            <div class="flex items-center gap-2">
                <div class="flex-1"><AgentSwitch on=to_agent title="Send the thread to an agent" /></div>
                {move || open.get().then(|| view! {
                    <button class="text-xs text-muted hover:underline" on:click=move |_| spawn_local(async move { done(api::doc_thread_resolve(doc.get_value(), tid.get_value()).await) })>"Resolve"</button>
                })}
            </div>
        </div>
    }
}

/// Export: the formats a document goes to, and the files made before
/// (each with the version it was made from), to download again.
#[component]
fn Exports(id: String, kind: String) -> impl IntoView {
    let id = StoredValue::new(id);
    let tick = RwSignal::new(0u32);
    let past = LocalResource::new(move || {
        tick.track();
        api::doc_exports(id.get_value())
    });
    let busy = RwSignal::new(None::<&'static str>);
    let error = RwSignal::new(None::<String>);
    let formats: &'static [(&'static str, &'static str)] = if kind == "deck" {
        &[("pdf", "PDF"), ("pptx", "PowerPoint"), ("odp", "OpenDocument"), ("png", "Pictures"), ("html", "HTML"), ("md", "Markdown")]
    } else {
        &[("pdf", "PDF"), ("docx", "Word"), ("odt", "OpenDocument"), ("html", "HTML"), ("md", "Markdown")]
    };
    let go = move |f: &'static str| {
        busy.set(Some(f));
        error.set(None);
        spawn_local(async move {
            match api::doc_export(id.get_value(), f.into()).await {
                Ok(e) => {
                    let _ = window().location().set_href(&api::url(&e.url));
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(None);
            tick.update(|t| *t += 1);
        });
    };
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-200 p-3 text-sm dark:border-zinc-800">
            <h2 class="text-xs font-medium uppercase tracking-wide text-muted">"Export"</h2>
            <div class="flex flex-wrap gap-2">
                {formats.iter().map(|(f, label)| {
                    let f = *f;
                    view! {
                        <button class=BUTTON_QUIET disabled=move || busy.get().is_some() on:click=move |_| go(f)
                            title=if matches!(f, "md" | "html") { "Made by carbon6" } else { "Made by the render kit on a worker with Docker or Podman" }>
                            {move || if busy.get() == Some(f) { format!("{label}…") } else { (*label).to_owned() }}
                        </button>
                    }
                }).collect_view()}
            </div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <ToGoogle id=id.get_value() />
            {move || past.get().and_then(Result::ok).filter(|p| !p.is_empty()).map(|p| view! {
                <div class="space-y-0.5">
                    <div class="text-xs text-muted">"Made before"</div>
                    {p.into_iter().map(|e| view! {
                        <div class="flex items-center gap-2">
                            <a href=api::url(&e.url) class="min-w-0 flex-1 truncate text-sky-700 hover:underline dark:text-sky-400">{e.name.clone()}</a>
                            <span class="shrink-0 text-xs text-muted">{format!("v{} · {} KB · {}", e.rev, e.size.div_ceil(1024), since(e.at))}</span>
                        </div>
                    }).collect_view()}
                </div>
            })}
        </section>
    }
}

/// Google Docs or Slides (docs/docs.md §10): connect an account (only the
/// files carbon6 makes), export, and after that update the Google file or
/// make a new one; a Google file edited in Google since is said first.
#[component]
fn ToGoogle(id: String) -> impl IntoView {
    let id = StoredValue::new(id);
    let tick = RwSignal::new(0u32);
    let google = LocalResource::new(move || {
        tick.track();
        api::doc_google(id.get_value())
    });
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let changed = RwSignal::new(false);
    let go = move |new: bool, anyway: bool| {
        busy.set(true);
        error.set(None);
        changed.set(false);
        spawn_local(async move {
            if let Err(e) = api::doc_to_google(id.get_value(), c6_api::docs::GoogleExportIn { new, anyway, account: None }).await {
                changed.set(e.contains("changed in Google"));
                error.set(Some(e));
            }
            busy.set(false);
            tick.update(|t| *t += 1);
        });
    };
    let connect = move |_| {
        spawn_local(async move {
            let back = window().location().pathname().unwrap_or_default();
            match api::docs_google_connect(back, false).await {
                Ok(u) => {
                    let _ = window().location().set_href(&u);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        {move || google.get().and_then(Result::ok).map(|g| {
            let app = g.app.clone();
            view! {
                <div class="space-y-1 border-t border-zinc-200 pt-2 dark:border-zinc-800">
                    <div class="text-xs text-muted">{app.clone()}</div>
                    {if g.accounts.is_empty() {
                        view! {
                            <div class="flex items-center gap-2">
                                <button class=BUTTON_QUIET on:click=connect>"Connect Google"</button>
                                <span class="text-xs text-muted">"Only the files carbon6 makes in your Drive (drive.file)."</span>
                            </div>
                        }.into_any()
                    } else if let Some(f) = g.file.clone() {
                        view! {
                            <div class="space-y-1">
                                <div class="flex items-center gap-2">
                                    <a href=f.url.clone() target="_blank" rel="noopener" class="min-w-0 flex-1 truncate text-sky-700 hover:underline dark:text-sky-400">
                                        {format!("Open in {app}")}
                                    </a>
                                    <span class="shrink-0 text-xs text-muted">{format!("v{} · {} · {}", f.rev, f.account, since(f.at))}</span>
                                </div>
                                <div class="flex flex-wrap gap-2">
                                    <button class=BUTTON_QUIET disabled=move || busy.get() on:click=move |_| go(false, false)>
                                        {move || if busy.get() { "Exporting…" } else { "Update the Google file" }}
                                    </button>
                                    <button class=BUTTON_QUIET disabled=move || busy.get() on:click=move |_| go(true, false)>"Make a new one"</button>
                                    {move || changed.get().then(|| view! {
                                        <button class=BUTTON_QUIET disabled=move || busy.get() on:click=move |_| go(false, true)
                                            title="Replaces the edits made in Google since the last export">"Update anyway"</button>
                                    })}
                                </div>
                            </div>
                        }.into_any()
                    } else {
                        view! {
                            <button class=BUTTON_QUIET disabled=move || busy.get() on:click=move |_| go(false, false)
                                title=format!("Made by the render kit, then uploaded to {} as {app}", g.accounts.join(", "))>
                                {move || if busy.get() { "Exporting…".to_owned() } else { format!("To {}", g.app) }}
                            </button>
                        }.into_any()
                    }}
                    {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
                </div>
            }
        })}
    }
}

/// History: changes by author run, each version openable and restorable.
#[component]
fn History(id: String, reload: Callback<()>) -> impl IntoView {
    let id = StoredValue::new(id);
    let versions = LocalResource::new(move || api::doc_history(id.get_value()));
    let open = RwSignal::new(None::<i64>);
    let error = RwSignal::new(None::<String>);
    let restore = move |rev: i64, block: Option<String>| {
        spawn_local(async move {
            match api::doc_restore(id.get_value(), &DocRestore { rev, block }).await {
                Ok(_) => {
                    open.set(None);
                    reload.run(());
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-200 p-3 text-sm dark:border-zinc-800">
            <h2 class="text-xs font-medium uppercase tracking-wide text-muted">"History"</h2>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || versions.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) => view! { <div class="space-y-2">{runs(&v).into_iter().map(|run| {
                    let first = run.last().map(|x| x.at).unwrap_or(0);
                    let last = run.first().map(|x| x.at).unwrap_or(0);
                    let by = run[0].by.clone();
                    view! {
                        <div>
                            <div class="text-xs text-muted">{format!("{}, {} {}, {}{}", author_named(&by), run.len(), if run.len() == 1 { "edit" } else { "edits" },
                                when_hm(first), if last != first { format!("–{}", when_hm(last)) } else { String::new() })}</div>
                            {run.iter().map(|v| {
                                let rev = v.rev;
                                view! {
                                    <div class="flex items-center gap-2 pl-2">
                                        <span class="w-10 shrink-0 text-xs text-muted">{format!("v{rev}")}</span>
                                        <span class="min-w-0 flex-1 truncate">{v.summary.clone()}</span>
                                        <button class="text-xs text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| open.set(if open.get_untracked() == Some(rev) { None } else { Some(rev) })>"Open"</button>
                                        <button class="text-xs text-sky-700 hover:underline dark:text-sky-400" title="Bring this version back as a new change"
                                            on:click=move |_| restore(rev, None)>"Restore"</button>
                                    </div>
                                }
                            }).collect_view()}
                        </div>
                    }
                }).collect_view()}</div> }.into_any(),
            })}
            {move || open.get().map(|rev| view! { <Version id=id.get_value() rev restore=Callback::new(move |(r, b): (i64, Option<String>)| restore(r, b)) /> })}
        </section>
    }
}

/// Versions grouped into runs by the same author.
fn runs(v: &[DocVersion]) -> Vec<Vec<DocVersion>> {
    let mut out: Vec<Vec<DocVersion>> = Vec::new();
    for x in v {
        match out.last_mut() {
            Some(run) if run[0].by == x.by && run.last().is_some_and(|l| l.at - x.at < 30 * 60 * 1000) => run.push(x.clone()),
            _ => out.push(vec![x.clone()]),
        }
    }
    out
}

fn when_hm(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    format!("{:02}:{:02}", d.get_hours(), d.get_minutes())
}

/// An earlier version, read only, with a Restore per block.
#[component]
fn Version(id: String, rev: i64, restore: Callback<(i64, Option<String>)>) -> impl IntoView {
    let id = StoredValue::new(id);
    let old = LocalResource::new(move || async move { api::doc_read(&id.get_value(), &format!("view=full&at={rev}")).await });
    view! {
        <div class="space-y-1 rounded border border-amber-200 bg-amber-50/50 p-2 dark:border-amber-900 dark:bg-amber-950/20">
            <div class="text-xs font-medium text-amber-800 dark:text-amber-300">{format!("As it was at version {rev}")}</div>
            {move || old.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) => {
                    let d: Option<Doc> = serde_json::from_value(v).ok();
                    view! { <div>{d.map(|d| d.tabs.into_iter().flat_map(|t| t.blocks).map(|b| {
                        let bid = b.id.clone();
                        view! {
                            <div class="group flex items-start gap-2">
                                <div class="min-w-0 flex-1"><Shown b=b kind=String::new() /></div>
                                <button class="invisible shrink-0 text-xs text-sky-700 group-hover:visible dark:text-sky-400" title="Bring back just this block"
                                    on:click=move |_| restore.run((rev, Some(bid.clone())))>"Restore block"</button>
                            </div>
                        }
                    }).collect_view())}</div> }.into_any()
                }
            })}
        </div>
    }
}

/// Bytes as base64 (for uploads).
fn base64_of(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_as_the_head_reads_it() {
        assert_eq!(super::base64_of(b"Man"), "TWFu");
        assert_eq!(super::base64_of(b"Ma"), "TWE=");
        assert_eq!(super::base64_of(b"M"), "TQ==");
        assert_eq!(super::base64_of(b""), "");
    }

    #[test]
    fn a_blocks_first_line_reads_as_plain_words() {
        use serde_json::{Value, json};
        let plain = |md: &str| super::first_line(md, &Value::Null);
        assert_eq!(plain("## The **plan**, in `short`"), "The plan, in short");
        assert_eq!(plain("\n\n- [ ] Ask [the team](https://x.y/z) first\n- then"), "Ask the team first");
        assert_eq!(plain("> 1. Quoted _and_ numbered"), "Quoted _and_ numbered");
        assert_eq!(plain("```rust\nfn main() {}\n```"), "fn main() {}");
        assert_eq!(plain("| Name | Owner |\n|---|---|\n| a | b |"), "Name · Owner");
        assert_eq!(plain("![A chart](x.png) and ~~old~~ words"), "A chart and old words");
        assert_eq!(plain("---"), "");
        let chips = json!({"chips": {"d": {"type": "date", "value": "2026-11-02"}, "m": {"type": "mention", "user": "u1", "name": "Ann"}}});
        assert_eq!(super::first_line("Due <?c6 block d?>, ask <?c6 block m?>", &chips), "Due 2026-11-02, ask @Ann");
    }
}

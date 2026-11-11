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
use crate::workspace::{Go, How, Rect, View, ViewActions, use_column, use_workspace};

fn since(at: i64) -> String {
    ago(now_ms() - at)
}

/// Who wrote something, as you read it.
pub fn author(by: &str) -> String {
    match by {
        "you" => "you".into(),
        "assistant" => "the assistant".into(),
        // The console's assistant, in a realm with your approval.
        "console" => "Ask in the console".into(),
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

fn owning_session(by: &str) -> Option<String> {
    by.strip_prefix("session:").filter(|id| !id.is_empty()).map(str::to_owned)
}

/// The session a document came from: its mark, and a link to it.
#[component]
fn FromSession(id: String) -> impl IntoView {
    let live = api::live();
    let detail = crate::resources::session(id.clone()).detail;
    let id = StoredValue::new(id);
    let session = move || live.session(&id.get_value()).or_else(|| detail.get().and_then(Result::ok).map(|d| d.session));
    let name = Signal::derive(move || {
        session().map(|s| s.title).filter(|s| !s.trim().is_empty()).unwrap_or_else(|| format!("Session {}", id.get_value()))
    });
    let mark = Signal::derive(move || session().as_ref().map(crate::marks::of).unwrap_or_else(|| session_mark(&id.get_value())));
    view! {
        <Go to=View::session(id.get_value()) stop=true title="Open the session that made it"
            class="flex h-11 w-8 shrink-0 items-center justify-center rounded-md hover:bg-zinc-100 pointer-coarse:w-11 dark:hover:bg-zinc-800">
            <span data-doc-owner="" class="flex h-full w-full items-center justify-center" title=move || format!("Open session: {}", name.get())>
                <crate::marks::Glyph mark shape=crate::marks::Shape::Circle size=10 />
                <span class="sr-only">{move || format!("Open the session that made it: {}", name.get())}</span>
            </span>
        </Go>
    }
}

/// Capture the live feed when the component is made, not while its async
/// list or history renders: delayed rendering needn't have a context owner.
fn author_names() -> impl Fn(&str) -> String + Copy {
    let live = use_context::<api::LiveFeed>();
    move |by| {
        if let Some(id) = by.strip_prefix("session:") {
            // Untracked: a session update mustn't redraw an open editor.
            let title =
                live.and_then(|l| l.sessions.try_with_untracked(|ss| ss.iter().find(|s| s.id == id).map(|s| s.title.clone())).flatten());
            return match title.filter(|t| !t.trim().is_empty()) {
                Some(t) if t.chars().count() > 32 => format!("session {}…", t.chars().take(30).collect::<String>().trim_end()),
                Some(t) => format!("session {t}"),
                None => "a session".into(),
            };
        }
        author(by)
    }
}

fn kind_mark(kind: &str) -> AnyView {
    if kind == "deck" { crate::icons::chart() } else { crate::icons::file() }
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
    view! { <Go to=View::Docs { id: String::new(), target: None } class=class title="Docs and decks: what people and agents write together">"Docs"</Go> }
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
    archived: Memo<bool>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// A block's Markdown as HTML with its chips as small widgets (docs/docs.md
/// §3): a date and a dropdown you click to change, a mention, and a ref
/// with the thing's title and state as they are now.
fn chips_html(text: &str, attrs: &Value, refs: &std::collections::HashMap<String, c6_api::work::LiveAttached>, archived: bool) -> String {
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
        let disabled = if archived { " disabled aria-disabled=\"true\"" } else { "" };
        let html = match v["type"].as_str().unwrap_or("") {
            "date" => format!(
                "<button type=\"button\"{disabled} data-chip=\"{}\" title=\"A date\" class=\"{CHIP} bg-sky-50 text-sky-800 enabled:hover:bg-sky-100 dark:bg-sky-950/40 dark:text-sky-300\">{} {}</button>",
                esc(key),
                crate::icons::html::calendar(),
                esc(&s("value"))
            ),
            "mention" => format!(
                "<span class=\"{CHIP} bg-violet-50 text-violet-800 dark:bg-violet-950/40 dark:text-violet-300\">@{}</span>",
                esc(&if s("name").is_empty() { s("user") } else { s("name") })
            ),
            "dropdown" => format!(
                "<button type=\"button\"{disabled} data-chip=\"{}\" title=\"Pick another\" class=\"{CHIP} bg-zinc-100 text-zinc-800 enabled:hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-200\">{} {}</button>",
                esc(key),
                esc(&s("value")),
                crate::icons::html::chevron_down()
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
                    "<button type=\"button\" data-ref=\"{}\" title=\"{}\" class=\"{CHIP} bg-emerald-50 text-emerald-800 hover:underline dark:bg-emerald-950/40 dark:text-emerald-300\">{} {}{}</button>",
                    esc(&r),
                    esc(&r),
                    esc(&title),
                    crate::icons::html::arrow_up_right(),
                    if state.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " <span class=\"text-[10px] uppercase tracking-wide opacity-70\">{}</span>",
                            esc(&if state == "archived" { "Archived".into() } else { state.replace('_', " ") })
                        )
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
    // Markdown mentions retain their query/hash in href, but data-ref is
    // only the thing's identity. Follow the full link before the ref-chip
    // fallback, or a document/session block jump loses its target here.
    if el.tag_name() == "A" && crate::mentions::follow_link(ev, col, ws) {
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
    if ctx.archived.get_untracked() {
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
    let drawing = new_tab.is_none();
    view! {
        // A block: a drawing has no size of its own, and in a box that
        // shrinks to fit its content it would shrink to nothing.
        <a href=href target="_blank" title="Show it bigger" class=if fill { "block w-full cursor-zoom-in" } else { "block w-fit max-w-full cursor-zoom-in" }
            role=drawing.then_some("button") tabindex=drawing.then_some(0)
            on:keydown=move |ev| if drawing && matches!(ev.key().as_str(), "Enter" | " ") {
                ev.prevent_default();
                ev.stop_propagation();
                open.set(true);
            }
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

/// A file embed's drawn paperclip, original name and size.
fn file_embed(href: String, name: String, size: Option<u64>) -> impl IntoView {
    view! {
        <a href=href class="my-1 inline-flex items-center gap-2 rounded border border-zinc-200 px-2 py-1 text-sm text-sky-700 hover:bg-zinc-50 dark:border-zinc-800 dark:text-sky-400 dark:hover:bg-zinc-900"
            on:click=|ev| ev.stop_propagation()>
            {crate::icons::paperclip()} {name}
            <span class="text-xs text-muted">{size.map(|n| format!("{} KB", n.div_ceil(1024))).unwrap_or_default()}</span>
        </a>
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
        "file" => file_embed(file_url(&s("blob")), s("name"), v["size"].as_u64()).into_any(),
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

/// The archive filter is immediate, like the other shared switch rows.
#[component]
fn ArchiveSwitch(checked: RwSignal<bool>) -> impl IntoView {
    view! {
        <crate::components::Switch on=checked on_change=move |on| checked.set(on) label="Show archived" />
    }
}

#[component]
fn ArchivedPill(archived: bool) -> impl IntoView {
    archived.then(|| {
        view! {
            <span class="shrink-0 rounded-full bg-zinc-100 px-2 py-0.5 text-[11px] text-muted dark:bg-zinc-800">"Archived"</span>
        }
    })
}

#[component]
fn DocListTitle(summary: DocSummary) -> impl IntoView {
    view! {
        <span class="shrink-0 text-muted" title=summary.kind.clone()>{kind_mark(&summary.kind)}</span>
        <span class="min-w-0 flex-1 truncate font-medium">{summary.title}</span>
        <ArchivedPill archived=summary.archived_at.is_some() />
    }
}

fn list_query(kind: String, q: String, label: String, by: String, archived: bool) -> DocQuery {
    DocQuery {
        kind: Some(kind).filter(|s| !s.is_empty()),
        q: Some(q).filter(|s| !s.trim().is_empty()),
        label: Some(label).filter(|s| !s.trim().is_empty()),
        by: Some(by).filter(|s| !s.is_empty()),
        archived,
        ..Default::default()
    }
}

#[component]
pub fn DocsPage() -> impl IntoView {
    let author_named = author_names();
    let tick = RwSignal::new(0u32);
    reload_on_change(tick, None);
    let kind = RwSignal::new(String::new());
    let q = RwSignal::new(String::new());
    let label = RwSignal::new(String::new());
    let by = RwSignal::new(String::new());
    let archived = RwSignal::new(false);
    let list = LocalResource::new(move || {
        tick.track();
        api::realm().track();
        let query = list_query(kind.get(), q.get(), label.get(), by.get(), archived.get());
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
        Some(c) => c.open(View::Docs { id, target: None }, how),
        None => ws.open(None, View::Docs { id, target: None }, how),
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
                <button class=BUTTON on:click=move |_| making.set(Some("doc"))>{crate::icons::icon(crate::icons::Icon::Plus, crate::icons::Size::S14)}" New doc"</button>
                <button class=BUTTON_QUIET on:click=move |_| making.set(Some("deck"))>{crate::icons::icon(crate::icons::Icon::Plus, crate::icons::Size::S14)}" New deck"</button>
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
                <div class="flex flex-wrap gap-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
                    <input class=format!("{INPUT} min-w-0 flex-1 basis-full @md/view:basis-40") placeholder=if k == "deck" { "The deck's title" } else { "The doc's title" } prop:value=move || title.get()
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
            <ArchiveSwitch checked=archived />
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
                                <a href=View::Docs { id: d.id.clone(), target: None }.href() class="flex flex-wrap items-center gap-2 px-2 py-2 text-sm hover:bg-zinc-50 dark:hover:bg-zinc-900"
                                    on:click=move |ev: web_sys::MouseEvent| {
                                        if ev.button() != 0 { return }
                                        ev.prevent_default();
                                        open_doc.with_value(|f| f(id.clone(), How::from_mouse(&ev)));
                                    }>
                                    <DocListTitle summary=d.clone() />
                                    {(d.archived_at.is_none() && d.pending > 0).then(|| view! {
                                        <span class="shrink-0 rounded bg-violet-100 px-1.5 text-[11px] text-violet-800 dark:bg-violet-900/40 dark:text-violet-300"
                                            title=format!("Still being written by {}", d.writers.iter().map(|w| author_named(w)).collect::<Vec<_>>().join(", "))>
                                            {crate::icons::icon(crate::icons::Icon::PenLine, crate::icons::Size::S14)}" "{format!("{} to write", d.pending)}
                                        </span>
                                    })}
                                    {d.labels.iter().map(|l| view! { <span class="min-w-0 max-w-full text-[11px] text-muted wrap-anywhere">{format!("#{l}")}</span> }).collect_view()}
                                    // The session it came from, else who changed it last.
                                    <span class="basis-full truncate text-xs text-muted @lg/view:max-w-[40%] @lg/view:basis-auto">
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
/// doc's right side in a narrower one, or as a sheet from the tile's bottom
/// in a narrow tile (including a phone).
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

/// Full-width actions in the phone workspace menu, with a 44px touch target.
const DOC_PHONE_ACTION: &str =
    "flex min-h-11 w-full items-center gap-2 px-4 text-left hover:bg-zinc-100 disabled:opacity-50 dark:hover:bg-zinc-800";

#[derive(Clone, Copy)]
struct ReadOnly(Memo<bool>);

fn read_only() -> Memo<bool> {
    use_context::<ReadOnly>().map(|r| r.0).unwrap_or_else(|| Memo::new(|_| false))
}

#[component]
fn ArchivedBanner(unarchive: Callback<()>, busy: RwSignal<bool>) -> impl IntoView {
    view! {
        <div role="status" class="flex min-h-11 items-center gap-2 rounded-lg bg-zinc-100 px-3 py-2 text-sm text-muted dark:bg-zinc-900">
            {crate::icons::icon(crate::icons::Icon::Archive, crate::icons::Size::S16)}
            <span class="flex-1">"Archived. Read-only."</span>
            <button class=BUTTON_QUIET disabled=move || busy.get() on:click=move |_| unarchive.run(())>"Unarchive"</button>
        </div>
    }
}

#[component]
fn ArchiveMenuItem(archived: bool, run: Callback<()>, busy: RwSignal<bool>) -> impl IntoView {
    view! {
        <button role="menuitem" class="flex min-h-11 w-full items-center gap-2 px-4 text-left hover:bg-zinc-100 disabled:opacity-50 dark:hover:bg-zinc-800"
            disabled=move || busy.get() on:click=move |_| run.run(())>
            {if archived { crate::icons::icon(crate::icons::Icon::ArchiveRestore, crate::icons::Size::S16) } else { crate::icons::icon(crate::icons::Icon::Archive, crate::icons::Size::S16) }}
            {if archived { "Unarchive" } else { "Archive" }}
        </button>
    }
}

/// The shared text-only menu cannot carry the Archive/Unarchive SVGs.
#[component]
fn DocMenu(
    owner: Memo<Option<String>>,
    archived: Memo<bool>,
    busy: RwSignal<bool>,
    archive: Callback<()>,
    export: Callback<()>,
    history: Callback<()>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let pop = crate::util::dismissable(
        move || open.get(),
        move || {
            open.set(false);
        },
    );
    view! {
        <div class="shrink-0">
            <button class="flex h-11 w-11 items-center justify-center rounded-md text-muted hover:bg-zinc-100 dark:hover:bg-zinc-800"
                title="Document actions" aria-label="Document actions" aria-haspopup="menu" aria-expanded=move || open.get().to_string()
                data-pop=pop.clone() on:click=move |_| open.update(|o| *o = !*o)>{crate::icons::ellipsis()}</button>
            {move || open.get().then(|| view! {
                <div role="menu" data-pop=pop.clone() class="absolute right-4 top-full z-50 min-w-40 max-w-full rounded-md border border-zinc-200 bg-white py-1 text-sm shadow-lg dark:border-zinc-700 dark:bg-zinc-900">
                    <DocMenuItems owner archived busy archive export history close=Callback::new(move |()| open.set(false)) />
                </div>
            })}
        </div>
    }
}

/// The same document actions in the desktop menu and the phone workspace bar.
#[component]
fn DocMenuItems(
    owner: Memo<Option<String>>,
    archived: Memo<bool>,
    busy: RwSignal<bool>,
    archive: Callback<()>,
    export: Callback<()>,
    history: Callback<()>,
    close: Callback<()>,
) -> AnyView {
    let (col, ws) = (use_column(), use_workspace());
    let live = api::live();
    view! {
        {move || owner.get().map(|id| {
            let detail = crate::resources::session(id.clone()).detail;
            let mark_id = id.clone();
            let mark = Signal::derive(move || {
                live.session(&mark_id).or_else(|| detail.get().and_then(Result::ok).map(|d| d.session))
                    .as_ref().map(crate::marks::of).unwrap_or_else(|| session_mark(&mark_id))
            });
            view! {
            <button role="menuitem" class="flex min-h-11 w-full items-center px-4 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800"
                on:click=move |ev| {
                    close.run(());
                    let to = View::session(id.clone());
                    let how = How::from_mouse(&ev);
                    match col { Some(c) => c.open(to, how), None => ws.open(None, to, how) }
                }><span class="mr-2 md:hidden"><crate::marks::Glyph mark shape=crate::marks::Shape::Circle size=10 /></span>"Open the session that made it"</button>
        }})}
        <button role="menuitem" class="flex min-h-11 w-full items-center px-4 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800"
            on:click=move |_| { close.run(()); export.run(()); }>"Export…"</button>
        <button role="menuitem" class="flex min-h-11 w-full items-center px-4 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800"
            on:click=move |_| { close.run(()); history.run(()); }>"History"</button>
        {move || view! { <ArchiveMenuItem archived=archived.get() busy run=Callback::new(move |()| { close.run(()); archive.run(()); }) /> }}
    }
    .into_any()
}

/// This browser's independently opened panel rows and last read reply, per doc.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct CommentPrefs {
    expanded: std::collections::HashSet<String>,
    read: std::collections::HashMap<String, String>,
}

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
    /// The block whose comments show in a popover by its 💬.
    pop: RwSignal<Option<String>>,
    selected: RwSignal<Option<String>>,
    prefs: RwSignal<CommentPrefs>,
    edit_epochs: StoredValue<std::collections::HashMap<String, u64>>,
    pop_node: NodeRef<leptos::html::Div>,
    tabs: Memo<Vec<DocTab>>,
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
    fn upsert(self, thread: DocThread) {
        let _ = self.shown.try_update(|ts| {
            if let Some(old) = ts.iter_mut().find(|t| t.id == thread.id) {
                *old = thread;
            } else {
                ts.push(thread);
            }
        });
    }

    fn read(self, t: &DocThread) {
        if let Some(last) = t.comments.last()
            && self.prefs.with_untracked(|p| p.read.get(&t.id) != Some(&last.id))
        {
            self.prefs.update(|p| {
                p.read.insert(t.id.clone(), last.id.clone());
            });
        }
    }

    fn expand(self, ids: &[String], expanded: bool) {
        self.prefs.update(|p| {
            for id in ids {
                if expanded {
                    p.expanded.insert(id.clone());
                    if let Some(last) = self
                        .shown
                        .with_untracked(|ts| ts.iter().find(|t| &t.id == id).and_then(|t| t.comments.last()).map(|c| c.id.clone()))
                    {
                        p.read.insert(id.clone(), last);
                    }
                } else {
                    p.expanded.remove(id);
                }
            }
        });
    }

    /// A block's 💬 (`marker`) pressed: its popover opens by it (with a
    /// new comment box, `new`), or closes if it was open.
    fn toggle_pop(self, block: String, marker: &web_sys::Element, new: bool) {
        if !new && self.pop.get_untracked().as_deref() == Some(block.as_str()) {
            self.close_pop();
            return;
        }
        self.place_pop(marker);
        if self.pop.with_untracked(|p| p.as_ref() != Some(&block)) {
            self.selected.set(
                self.shown.with_untracked(|ts| ts.iter().find(|t| t.state == "open" && t.anchor.block == block).map(|t| t.id.clone())),
            );
        }
        self.pop.set(Some(block.clone()));
        if new {
            self.focus.set(Some(block.clone()));
            self.commenting.set(Some(block));
        }
    }

    /// Prefer below the marker, but keep the whole box in the visible tile.
    fn place_pop(self, marker: &web_sys::Element) {
        let Some(node) = self.pop_node.try_get_untracked().flatten() else { return };
        place_popover(&node, marker, self.root);
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

fn rect(r: web_sys::DomRect) -> Rect {
    Rect { left: r.left(), top: r.top(), width: r.width(), height: r.height() }
}

/// The part of the layout viewport not covered by a phone's keyboard.
fn visible_viewport() -> Rect {
    let w = window();
    match w.visual_viewport() {
        Some(v) => Rect { left: v.offset_left(), top: v.offset_top(), width: v.width(), height: v.height() },
        None => Rect {
            width: w.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(0.0),
            height: w.inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(0.0),
            ..Rect::default()
        },
    }
}

/// Prefer below, then above; if neither fits, clamp and scroll the box.
/// `area` is the intersection of the scrolling tile and visual viewport.
fn popover_at(anchor: Rect, area: Rect, size: (f64, f64)) -> Rect {
    let (left, top) = (area.left + 8.0, area.top + 8.0);
    let (width, height) = (size.0.min((area.width - 16.0).max(0.0)), size.1.min((area.height - 16.0).max(0.0)));
    let below = anchor.top + anchor.height + 4.0;
    let y = if below + height <= area.top + area.height - 8.0 { below } else { anchor.top - height - 4.0 };
    Rect {
        left: (anchor.left + anchor.width - width).clamp(left, (area.left + area.width - 8.0 - width).max(left)),
        top: y.clamp(top, (area.top + area.height - 8.0 - height).max(top)),
        width,
        height,
    }
}

/// Keep a usable thread beside the reading column if possible. Otherwise
/// narrow it to 256px and try every anchor-free strip before accepting any
/// overlap. 160px leaves room for the header, reply and footer; the comments
/// scroll in the remaining space. Never reserve space in the document.
fn thread_at(block: Rect, column: Rect, area: Rect, size: (f64, f64), minimum: f64) -> Rect {
    let left = area.left + 8.0;
    let top = area.top + 8.0;
    let right = (area.left + area.width - 8.0).max(left);
    let bottom = (area.top + area.height - 8.0).max(top);
    let height = size.1.min(bottom - top).max(0.0);
    let at = |x: f64, y: f64, width: f64, height: f64| Rect {
        left: x.clamp(left, (right - width).max(left)),
        top: y.clamp(top, (bottom - height).max(top)),
        width,
        height,
    };
    let margin_right = right - (column.left + column.width).max(block.left + block.width) - 8.0;
    let margin_left = column.left.min(block.left) - 8.0 - left;
    if margin_right.max(margin_left) >= 256.0 {
        let width = size.0.min(margin_right.max(margin_left)).min(right - left).max(0.0);
        return at(
            if margin_right >= margin_left {
                (column.left + column.width).max(block.left + block.width) + 8.0
            } else {
                column.left.min(block.left) - 8.0 - width
            },
            block.top,
            width,
            height,
        );
    }
    let width = 256.0_f64.min(right - left).min(size.0).max(0.0);
    let right_room = right - block.left - block.width - 8.0;
    let left_room = block.left - 8.0 - left;
    // A narrow block can leave usable space within the reading column too.
    if right_room.max(left_room) >= width {
        return at(if right_room >= left_room { right - width } else { left }, block.top, width, height);
    }
    let below = bottom - block.top - block.height - 8.0;
    let above = block.top - 8.0 - top;
    if below.max(above) >= height.min(minimum) {
        let h = height.min(below.max(above));
        return at(right - width, if below >= above { block.top + block.height + 8.0 } else { block.top - 8.0 - h }, width, h);
    }
    // No usable anchor-free rectangle exists. The least-overlapping corner
    // is deterministic; the anchor is not replaced with a dashed ghost.
    let height = height.min(minimum);
    let overlap = |r: &Rect| {
        ((r.left + r.width).min(block.left + block.width) - r.left.max(block.left)).max(0.0)
            * ((r.top + r.height).min(block.top + block.height) - r.top.max(block.top)).max(0.0)
    };
    [
        at(right - width, top, width, height),
        at(right - width, bottom - height, width, height),
        at(left, top, width, height),
        at(left, bottom - height, width, height),
    ]
    .into_iter()
    .min_by(|a, b| overlap(a).total_cmp(&overlap(b)))
    .unwrap()
}

fn place_popover(node: &web_sys::HtmlElement, anchor: &web_sys::Element, root: NodeRef<leptos::html::Div>) {
    let Some(root) = root.try_get_untracked().flatten() else { return };
    let Some(area) = root.parent_element() else { return };
    let Some(parent) = node.offset_parent() else { return };
    let (mut a, v) = (rect(area.get_bounding_client_rect()), visible_viewport());
    // The scrollport excludes borders and the reserved scrollbar gutter.
    a.left += f64::from(area.client_left());
    a.top += f64::from(area.client_top());
    a.width = f64::from(area.client_width());
    a.height = f64::from(area.client_height());
    let (left, top) = (a.left.max(v.left), a.top.max(v.top));
    let visible = Rect {
        left,
        top,
        width: ((a.left + a.width).min(v.left + v.width) - left).max(0.0),
        height: ((a.top + a.height).min(v.top + v.height) - top).max(0.0),
    };
    let style = node.style();
    // Constrain before measuring: wrapping and a growing draft change height.
    let _ = style.set_property("max-width", &format!("{}px", (visible.width - 16.0).max(0.0)));
    let _ = style.set_property("max-height", &format!("{}px", (visible.height - 16.0).max(0.0)));
    let at = if let Some(block) = anchor.closest("[data-block]").ok().flatten() {
        let block = rect(block.get_bounding_client_rect());
        let column =
            root.query_selector("[data-doc-reading]").ok().flatten().map(|el| rect(el.get_bounding_client_rect())).unwrap_or(block);
        let width = thread_at(block, column, visible, (350.0, 160.0), 160.0).width;
        let _ = style.set_property("width", &format!("{width}px"));
        // offsetHeight is untransformed during the popover's entrance.
        let height = f64::from(node.offset_height());
        // Menus, multiline replies and errors are non-scrolling chrome too.
        // Leave them and a line of conversation visible, rather than choosing
        // an anchor-free rectangle that clips the reply controls.
        let chrome = node
            .query_selector("[data-thread-comments]")
            .ok()
            .flatten()
            .map(|list| height - f64::from(list.client_height()) + 32.0)
            .unwrap_or(160.0);
        let at = thread_at(block, column, visible, (350.0, height), chrome.max(160.0));
        let _ = style.set_property("max-height", &format!("{}px", at.height));
        at
    } else {
        let r = node.get_bounding_client_rect();
        popover_at(rect(anchor.get_bounding_client_rect()), visible, (r.width(), r.height()))
    };
    let p = parent.get_bounding_client_rect();
    let _ = style.set_property("left", &format!("{}px", at.left - p.left() - f64::from(parent.client_left())));
    let _ = style.set_property("top", &format!("{}px", at.top - p.top() - f64::from(parent.client_top())));
}

/// Follow the tile, content, scrolling and keyboard without remounting a draft.
fn follow_popover(node: NodeRef<leptos::html::Div>, place: impl Fn() + Clone + 'static) {
    use wasm_bindgen::{JsCast, closure::Closure};
    let root = expect_context::<ThreadsCtx>().root;
    Effect::new(move || {
        let (Some(node), Some(root)) = (node.get(), root.get()) else { return };
        let place = place.clone();
        place();
        let alive = std::rc::Rc::new(std::cell::Cell::new(true));
        let on = Closure::<dyn Fn()>::new({
            let (place, alive) = (place.clone(), alive.clone());
            move || {
                let (place, alive) = (place.clone(), alive.clone());
                // Resizing the observed box during delivery makes an observer
                // loop. Measure on the next frame, only while it still exists.
                request_animation_frame(move || {
                    if alive.get() {
                        place();
                    }
                });
            }
        });
        let callback = on.as_ref().unchecked_ref();
        let observer = web_sys::ResizeObserver::new(callback).ok();
        if let Some(ro) = observer.clone() {
            let alive = alive.clone();
            // A panel can mount during the tile's observer delivery (docked
            // becomes floating). Register its ancestors outside that delivery.
            request_animation_frame(move || {
                if alive.get() {
                    ro.observe(&node);
                    ro.observe(&root);
                    if let Some(area) = root.parent_element() {
                        ro.observe(&area);
                    }
                }
            });
        }
        // Capture catches nested scroll areas too; repositioning never closes it.
        let _ = document().add_event_listener_with_callback_and_bool("scroll", callback, true);
        let _ = window().add_event_listener_with_callback("resize", callback);
        let viewport = window().visual_viewport();
        if let Some(v) = &viewport {
            let _ = v.add_event_listener_with_callback("resize", callback);
            let _ = v.add_event_listener_with_callback("scroll", callback);
        }
        let hooks = send_wrapper::SendWrapper::new((observer, viewport, on, alive));
        on_cleanup(move || {
            let (observer, viewport, on, alive) = &*hooks;
            alive.set(false);
            if let Some(ro) = observer {
                ro.disconnect();
            }
            let callback = on.as_ref().unchecked_ref();
            let _ = document().remove_event_listener_with_callback_and_bool("scroll", callback, true);
            let _ = window().remove_event_listener_with_callback("resize", callback);
            if let Some(v) = viewport {
                let _ = v.remove_event_listener_with_callback("resize", callback);
                let _ = v.remove_event_listener_with_callback("scroll", callback);
            }
        });
    });
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
                    opts.set_behavior(if crate::util::reduced_motion() {
                        web_sys::ScrollBehavior::Instant
                    } else {
                        web_sys::ScrollBehavior::Smooth
                    });
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

/// Blocks as drawn (a slide for its children), on the first matching tab.
fn reveal_targets(tabs: &[DocTab], ids: &[String]) -> Option<(String, Vec<String>)> {
    tabs.iter().find_map(|t| {
        let blocks: Vec<String> = t
            .blocks
            .iter()
            .filter(|b| ids.iter().any(|i| *i == b.id || b.children.iter().any(|c| c.id == *i)))
            .map(|b| b.id.clone())
            .collect();
        (!blocks.is_empty()).then(|| (t.id.clone(), blocks))
    })
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
    let author_named = author_names();
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
    // The single phone bar uses the workspace's title cache. Local edits
    // must update it too, without waiting for a live-feed echo/refetch.
    Effect::new(move || {
        if state.get() == 2 {
            summary.with(|s| remember_titles([s]));
        }
    });
    let owner = Memo::new(move |_| summary.with(|s| owning_session(&s.created_by)));
    let archived = Memo::new(move |_| summary.with(|s| s.archived_at.is_some()));
    provide_context(ReadOnly(archived));
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
    let phone_renaming = RwSignal::new(false);
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
        if archived.get_untracked() {
            return;
        }
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
        archived,
    };
    provide_context(chip_ctx);
    let doc_changes = api::live().doc;
    Effect::new(move || {
        doc_changes.track();
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
    let prefs_key = StoredValue::new(format!("c6.doc-comments.{}", id.get_value()));
    let prefs = RwSignal::new(
        crate::util::storage_get(&prefs_key.get_value()).and_then(|s| serde_json::from_str::<CommentPrefs>(&s).ok()).unwrap_or_default(),
    );
    Effect::new(move || {
        if let Ok(saved) = serde_json::to_string(&prefs.get()) {
            crate::util::storage_set(&prefs_key.get_value(), &saved);
        }
    });
    let shown_threads = RwSignal::new(Vec::<DocThread>::new());
    let reveal = Callback::new(move |ids: Vec<String>| {
        let Some((on, mut els)) = tabs.with_untracked(|ts| reveal_targets(ts, &ids)) else { return };
        if on != current.get_untracked() {
            tab.set(on);
        }
        if is_deck.get_untracked() {
            // Only one slide is drawn at a time. Select it before flashing.
            els.truncate(1);
            deck_pick.set(els.first().cloned());
        }
        // A sheet covers the doc.
        if fit.get_untracked() == Fit::Sheet {
            panel.set(false);
            pop.set(None);
        }
        request_animation_frame(move || flash_blocks(root, &els));
    });
    // Observe the retained pane, not the document's mount-time View. Wait
    // for its tabs to load, then use the same tab/slide reveal as comments.
    let column = use_column();
    let target = Memo::new(move |_| {
        column.and_then(|c| {
            c.ws.layout.with(|l| match &l.tile(c.tile)?.columns.iter().find(|col| col.key == c.col)?.view {
                View::Docs { target, .. } => target.clone(),
                _ => None,
            })
        })
    });
    let visits = column.map(|c| c.ws.navigation_for(c.col));
    let revealed = StoredValue::new(None::<(String, u64)>);
    Effect::new(move || {
        let target = target.get();
        let visit = visits.map(|v| v.get()).unwrap_or_default();
        tabs.track();
        root.track();
        let Some(block) = target else {
            revealed.set_value(None);
            return;
        };
        if revealed.get_value().as_ref() != Some(&(block.clone(), visit))
            && root.get_untracked().is_some()
            && tabs.with_untracked(|ts| reveal_targets(ts, std::slice::from_ref(&block))).is_some()
        {
            revealed.set_value(Some((block.clone(), visit)));
            reveal.run(vec![block]);
        }
    });
    let unsent = Memo::new(move |_| shown_threads.with(|ts| ts.iter().filter(|t| t.state == "open" && t.unsent).count()));
    let open_n = Memo::new(move |_| shown_threads.with(|ts| ts.iter().filter(|t| t.state == "open").count()));
    let sending = RwSignal::new(false);
    let send_all = Callback::new(move |()| {
        if archived.get_untracked() {
            return;
        }
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
        selected: RwSignal::new(None),
        prefs,
        edit_epochs: StoredValue::new(std::collections::HashMap::new()),
        pop_node: NodeRef::new(),
        tabs,
        root,
        reveal,
        unsent,
        sending,
        send_all,
    };
    provide_context(threads);
    // Archiving elsewhere must close editors without blur-saving a late write.
    Effect::new(move || {
        if archived.get() {
            edit.set(None);
            edit_all.set(false);
            renaming.set(false);
            phone_renaming.set(false);
            adding_open.set(false);
            chip_ctx.editing.set(None);
            threads.commenting.set(None);
        }
    });
    let loaded_threads = LocalResource::new(move || {
        tick.track();
        api::doc_threads(id.get_value())
    });
    Effect::new(move || {
        if let Some(Ok(t)) = loaded_threads.get()
            && shown_threads.with_untracked(|s| *s != t)
        {
            prefs.update(|p| {
                for thread in &t {
                    if let Some(last) = thread.comments.last()
                        && !p.read.contains_key(&thread.id)
                    {
                        p.read.insert(thread.id.clone(), last.id.clone());
                    }
                }
            });
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
        // The panel and thread boxes stay out of the reading column's layout.
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
        let previous_phone = StoredValue::new(None::<bool>);
        let on = Closure::<dyn Fn(js_sys::Array)>::new(move |entries: js_sys::Array| {
            let Some(e) = entries.get(0).dyn_ref::<web_sys::ResizeObserverEntry>().map(|e| e.content_rect()) else { return };
            // Hidden: nothing to go by.
            if e.width() <= 0.0 {
                return;
            }
            let phone = crate::util::touch_first()
                || !window().match_media("(min-width: 48rem) and (min-height: 32rem)").ok().flatten().is_some_and(|m| m.matches());
            let changed_layout = previous_phone.get_value() != Some(phone);
            previous_phone.set_value(Some(phone));
            let f = if e.width() >= DOCKED_FROM {
                Fit::Docked
            } else if phone || e.width() < 640.0 {
                Fit::Sheet
            } else {
                Fit::Floating
            };
            // A scrollbar coming or going is no resize: the popover makes
            // the page just tall enough for one, the narrower page rewraps,
            // the popover is placed again, the scrollbar goes, and round it
            // went for good. A real width change or crossing the phone's
            // height-aware breakpoint counts (its toolbar may now be hidden).
            let resized = tile_w.try_get_untracked().is_some_and(|w| (w - e.width()).abs() >= 24.0);
            if (resized || changed_layout) && fit.try_get_untracked().is_some_and(|x| x != f) {
                let _ = fit.try_set(f);
            }
            if resized {
                let _ = tile_w.try_set(e.width());
            }
            let _ = tile_h.try_set(e.height());
        });
        // Room for the scrollbar is kept whether or not it shows, so it
        // doesn't change the page's width either.
        if let Some(area) = area.dyn_ref::<web_sys::HtmlElement>() {
            let _ = area.style().set_property("scrollbar-gutter", "stable");
        }
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
    let docked_present = crate::util::presence(move || docked.get());
    let floating_present = crate::util::presence(move || floating.get());
    let sheet_present = crate::util::presence(move || sheet.get());
    let pop_sheet = Memo::new(move |_| popped.with(|p| p.as_ref().is_some_and(|(_, sheet)| *sheet)));
    let pop_float = Memo::new(move |_| popped.with(|p| p.as_ref().is_some_and(|(_, sheet)| !*sheet)));
    let pop_sheet_present = crate::util::presence(move || pop_sheet.get());
    let pop_float_present = crate::util::presence(move || pop_float.get());
    let (sheet_space, pop_sheet_space) = (RwSignal::new(0.0_f64), RwSignal::new(0.0_f64));
    // Closing clears the selection, not the exiting visual. A reopen of the
    // same block reverses its transition without replacing its draft/body.
    let retained_pop = Memo::new(move |previous: Option<&Option<(String, bool)>>| popped.get().or_else(|| previous.cloned().flatten()));

    let attaching = RwSignal::new(false);
    let attach_file = move |ev: leptos::ev::Event| {
        if archived.get_untracked() {
            return;
        }
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
    let used_in = crate::resources::local(move || {
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
        if archived.get_untracked() {
            return;
        }
        let t = title.get_untracked();
        renaming.set(false);
        phone_renaming.set(false);
        if t.trim().is_empty() {
            return;
        }
        spawn_local(async move {
            match api::doc_patch(id.get_value(), &DocPatch { title: Some(t), ..Default::default() }).await {
                Ok(d) => doc.set(Some(Ok(d))),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let add_block = move || {
        if archived.get_untracked() {
            return;
        }
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
    let phone_rename_ref = NodeRef::<leptos::html::Input>::new();
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
    Effect::new(move || {
        if let Some(el) = phone_rename_ref.get() {
            let _ = el.focus();
        }
    });
    let toggle_edit = Callback::new(move |()| {
        if archived.get_untracked() {
            return;
        }
        if edit_all.get_untracked() {
            edit_all.set(false);
            adding_open.set(false);
            save(id.get_value(), edit, draft, false, error, archived, reload);
        } else {
            edit_all.set(true);
        }
    });
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
    let show_add = Memo::new(move |_| !archived.get() && !is_deck.get() && (edit_all.get() || empty.get()));
    let archive_busy = RwSignal::new(false);
    let archive = Callback::new(move |()| {
        if archive_busy.get_untracked() {
            return;
        }
        let next = !archived.get_untracked();
        archive_busy.set(true);
        spawn_local(async move {
            match api::doc_patch(id.get_value(), &DocPatch { archived: Some(next), ..Default::default() }).await {
                Ok(d) => doc.set(Some(Ok(d))),
                Err(e) => error.set(Some(e)),
            }
            archive_busy.set(false);
        });
    });

    let bar = move || {
        view! {
            <div data-doc-header="" class="sticky top-0 z-20 hidden border-b border-zinc-200 bg-white md:block dark:border-zinc-800 dark:bg-zinc-950">
                <div class="relative mx-auto flex max-w-3xl flex-wrap items-center gap-1 px-4 py-1">
                    <span class="shrink-0 text-muted" title=move || summary.with(|s| s.kind.clone())>{move || summary.with(|s| kind_mark(&s.kind))}</span>
                    <div data-doc-heading="" class="flex min-w-0 flex-1 basis-40 items-center gap-1">
                    {move || owner.get().map(|id| view! { <FromSession id /> })}
                    {move || if renaming.get() && !archived.get() {
                        view! {
                            <input node_ref=rename_ref class=format!("{INPUT} min-w-0 flex-1 font-semibold") prop:value=move || title.get()
                                on:input=move |ev| title.set(event_target_value(&ev))
                                on:keydown=move |ev| if ev.key() == "Enter" { rename() } else if ev.key() == "Escape" { renaming.set(false) }
                                on:blur=move |_| if renaming.get_untracked() { rename() } />
                        }.into_any()
                    } else {
                        view! {
                            <h1 class="min-w-0 flex-1 truncate text-lg font-semibold" title=move || if archived.get() { "Archived document" } else { "Click to rename" }
                                on:click=move |_| { if !archived.get_untracked() { title.set(summary.with_untracked(|s| s.title.clone())); renaming.set(true); } }>
                                {move || summary.with(|s| s.title.clone())}
                            </h1>
                        }.into_any()
                    }}
                    </div>
                    {comments_button(panel, open_n, false)}
                    // A comment on the whole doc, wherever you've scrolled to.
                    <Show when=move || !archived.get()>{whole_doc_comment_button(threads, false)}
                    </Show>
                    {move || (!archived.get() && unsent.get() > 0).then(|| send_all_button(threads, true, false))}
                    {move || (!archived.get() && !is_deck.get()).then(|| edit_doc_button(edit_all, toggle_edit, false))}
                    <UsedIn items=used />
                    <DocMenu owner archived busy=archive_busy archive
                        export=Callback::new(move |()| exporting.update(|x| *x = !*x))
                        history=Callback::new(move |()| history.update(|x| *x = !*x)) />
                </div>
                <Show when=move || floating_present.get()><FloatingPanel open=Signal::derive(move || floating.get()) /></Show>
            </div>
        }
        .into_any()
    };
    let content = move || {
        view! {
            <ViewActions>
                <div data-doc-phone-actions="" class="md:hidden">
                    <div class="flex items-center gap-2 px-4 py-2 text-xs text-muted">
                        {move || summary.with(|s| kind_mark(&s.kind))}
                        <span class="truncate">{move || summary.with(|s| s.title.clone())}</span>
                    </div>
                    <Show when=move || !archived.get()>
                        <button role="menuitem" class=DOC_PHONE_ACTION on:click=move |_| {
                            title.set(summary.with_untracked(|s| s.title.clone()));
                            phone_renaming.set(true);
                        }>{crate::icons::pencil()}" Rename document"</button>
                    </Show>
                    {comments_button(panel, open_n, true)}
                    <Show when=move || !archived.get()>
                        {whole_doc_comment_button(threads, true)}
                    </Show>
                    <Show when=move || { !archived.get() && unsent.get() > 0 }>
                        {send_all_button(threads, true, true)}
                    </Show>
                    <Show when=move || !archived.get() && !is_deck.get()>
                        {edit_doc_button(edit_all, toggle_edit, true)}
                    </Show>
                    <Show when=move || !used.with(Vec::is_empty)>
                        <div class="border-y border-zinc-200 py-1 dark:border-zinc-700">
                            <p class="px-4 py-1 text-xs text-muted">{move || format!("Used in {}", used.with(Vec::len))}</p>
                            {move || used.get().into_iter().map(|(to, label)| view! {
                                <Go to class="flex min-h-11 items-center px-4 text-sky-700 hover:bg-zinc-100 dark:text-sky-400 dark:hover:bg-zinc-800">{label}</Go>
                            }).collect_view()}
                        </div>
                    </Show>
                    <DocMenuItems owner archived busy=archive_busy archive
                        export=Callback::new(move |()| exporting.update(|x| *x = !*x))
                        history=Callback::new(move |()| history.update(|x| *x = !*x))
                        close=Callback::new(|()| ()) />
                </div>
            </ViewActions>
            <Show when=move || archived.get()><ArchivedBanner unarchive=archive busy=archive_busy /></Show>
            // Keep the phone rename editor in the live body, not the transient
            // Actions menu or the desktop toolbar hidden at this breakpoint.
            <Show when=move || phone_renaming.get() && !archived.get()>
                <form data-doc-phone-rename="" class="flex items-center gap-2 md:hidden" on:submit=move |ev| { ev.prevent_default(); rename(); }>
                    <input node_ref=phone_rename_ref class=format!("{INPUT} min-w-0 flex-1") aria-label="Document title"
                        prop:value=move || title.get() on:input=move |ev| title.set(event_target_value(&ev))
                        on:keydown=move |ev| if ev.key() == "Escape" { ev.prevent_default(); phone_renaming.set(false); } />
                    <button type="button" class=BUTTON_QUIET on:click=move |_| phone_renaming.set(false)>"Cancel"</button>
                    <button type="submit" class=BUTTON>"Rename"</button>
                </form>
            </Show>
            <header class="space-y-1">
                <p class="flex flex-wrap items-center gap-x-1 text-xs text-muted">
                    {move || summary.with(|s| view! {
                        {format!("Version {} · changed {} by {}", s.rev, since(s.updated_at), author_named(&s.updated_by))}
                        {(!s.labels.is_empty()).then(|| format!(" · {}", s.labels.iter().map(|l| format!("#{l}")).collect::<Vec<_>>().join(" ")))}
                    })}
                </p>
                {move || summary.with(|s| (s.archived_at.is_none() && s.pending > 0).then(|| view! {
                    <p class="text-sm text-violet-700 dark:text-violet-300">
                        {crate::icons::icon(crate::icons::Icon::PenLine, crate::icons::Size::S14)}" "{format!("Being written by {}: {} {} to go", s.writers.iter().map(|w| author_named(w)).collect::<Vec<_>>().join(", "), s.pending, if s.pending == 1 { "part" } else { "parts" })}
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
            {move || (is_deck.get() && !archived.get()).then(|| shown_doc.get().map(|d| view! {
                <crate::deck::DeckView doc=d send=Callback::new(move |ops: Vec<DocOp>| send.with_value(|f| f(ops))) error picked=deck_pick commenting=threads.commenting />
            }))}
            {move || (is_deck.get() && archived.get()).then(|| shown_doc.get().map(|d| view! {
                <ArchivedDeck doc=d width=Signal::derive(move || (tile_w.get() - 32.0).clamp(160.0, 704.0)) />
            }))}
            {move || (!is_deck.get()).then(|| view! {
                <article class="space-y-1">
                    <For each=move || drawn.get() key=|b: &DocBlock| (b.id.clone(), b.hash.clone(), b.rev) children=move |b: DocBlock| view! {
                        <BlockView doc_id=id b kind=summary.with_untracked(|s| s.kind.clone()) doc_rev edit draft error reload=reload_cb edit_all />
                    } />
                    {move || empty.get().then(|| view! { <p class="text-sm text-muted">{if archived.get() { "Empty document." } else { "Empty: add a block below to start it." }}</p> })}
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
                                        {move || if attaching.get() { "Adding…".into_any() } else { view! { {crate::icons::icon(crate::icons::Icon::Plus, crate::icons::Size::S14)}" Picture or file" }.into_any() }}
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
                                <button class="hover:text-zinc-900 dark:hover:text-zinc-100" on:click=move |_| adding_open.set(true)>{crate::icons::icon(crate::icons::Icon::Plus, crate::icons::Size::S14)}" Add a block at the end"</button>
                                <span aria-hidden="true">"·"</span>
                                <label class="cursor-pointer hover:text-zinc-900 dark:hover:text-zinc-100" title="A picture shows in the doc; any other file is a link to download">
                                    {move || if attaching.get() { "Adding…".into_any() } else { view! { {crate::icons::icon(crate::icons::Icon::Plus, crate::icons::Size::S14)}" Picture or file" }.into_any() }}
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
        <div node_ref=root class="relative" style:padding-bottom=move || format!("{}px",
            (if sheet_present.get() { sheet_space.get() } else { 0.0 })
                .max(if pop_sheet_present.get() { pop_sheet_space.get() } else { 0.0 }))>
            // The panel overlays the same grid cell: even All comments must
            // not shrink, recenter or rewrap the document when it opens.
            <div class="grid">
                <div class="min-w-0 [grid-area:1/1]">
                    {move || (state.get() == 2).then(bar)}
                    <main data-doc-reading="" class="mx-auto max-w-3xl space-y-4 px-4 pb-8 pt-3">
                        {move || error.get().map(|e| view! {
                            <div class="flex items-start gap-2">
                                <div class="flex-1"><ErrorBox message=e /></div>
                                <button class="text-xs text-muted" title="Dismiss error" on:click=move |_| error.set(None)>{crate::icons::x()}</button>
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
                <Show when=move || docked_present.get()>
                    <aside class="c6-pane c6-presence sticky top-0 z-20 ml-auto flex w-[26rem] self-start flex-col border-x border-zinc-200 bg-white [grid-area:1/1] dark:border-zinc-800 dark:bg-zinc-950"
                        data-open=move || docked.get().to_string() inert=move || !docked.get()
                        style=move || format!("height:{}px;{}", tile_h.get(), if docked.get() { "" } else { "position:absolute;right:0" })>
                        <CommentsPanel />
                    </aside>
                </Show>
            </div>
            <Show when=move || sheet_present.get()>
                <Sheet open=Signal::derive(move || sheet.get()) space=sheet_space close=Callback::new(move |()| panel.set(false))><CommentsPanel /></Sheet>
            </Show>
            <Show when=move || pop_sheet_present.get()>
                <Sheet open=Signal::derive(move || pop_sheet.get()) space=pop_sheet_space close=Callback::new(move |()| threads.close_pop())>
                    {move || retained_pop.get().map(|(b, _)| view! {
                        <div class="flex min-h-0 flex-1 flex-col gap-2 overflow-hidden p-3 text-sm"><PopBody block=b /></div>
                    })}
                </Sheet>
            </Show>
            <Show when=move || pop_float_present.get()>
                {move || retained_pop.get().map(|(b, _)| view! { <BlockPop block=b open=Signal::derive(move || pop_float.get()) /> })}
            </Show>
        </div>
    }
}

/// Construct each control once, keeping its typed DOM behind a shared boundary.
/// Presentation changes attributes and labels, never ownership or mount policy.
fn comments_button(panel: RwSignal<bool>, open_n: Memo<usize>, phone: bool) -> AnyView {
    view! {
        <button role=phone.then_some("menuitem") data-opens="comments"
            class=move || if phone { DOC_PHONE_ACTION.to_owned() } else { format!("{BAR} {}", if panel.get() { "bg-zinc-100 dark:bg-zinc-800" } else { "" }) }
            title=move || (!phone).then(|| if panel.get() { "Hide the comments" } else { "Show the comments" })
            aria-pressed=move || phone.then(|| panel.get().to_string())
            on:click=move |_| panel.update(|p| *p = !*p)>
            {crate::icons::icon(crate::icons::Icon::MessageSquare, crate::icons::Size::S14)}
            {(!phone).then_some(" ")}
            {move || if phone {
                format!("{} comments{}", if panel.get() { "Hide" } else { "Show" }, match open_n.get() { 0 => String::new(), n => format!(" ({n} open)") })
            } else {
                match open_n.get() { 0 => "Comments".to_owned(), n => format!("{n} open") }
            }}
        </button>
    }
    .into_any()
}

fn whole_doc_comment_button(ctx: ThreadsCtx, phone: bool) -> AnyView {
    view! {
        <button role=phone.then_some("menuitem") data-opens="comments" class=if phone { DOC_PHONE_ACTION } else { BAR }
            title=(!phone).then_some("Comment on the whole doc")
            on:click=move |_| { ctx.panel.set(true); ctx.focus.set(Some(String::new())); ctx.commenting.set(Some(String::new())); }>
            {crate::icons::icon(crate::icons::Icon::Plus, crate::icons::Size::S14)}
            {if phone { " Comment on the whole doc" } else { " Comment" }}
        </button>
    }
    .into_any()
}

fn edit_doc_button(edit_all: RwSignal<bool>, toggle_edit: Callback<()>, phone: bool) -> AnyView {
    view! {
        <button role=phone.then_some("menuitem")
            class=move || if phone { DOC_PHONE_ACTION.to_owned() } else { format!("{BAR} {}", if edit_all.get() { "bg-zinc-100 font-medium dark:bg-zinc-800" } else { "" }) }
            title=move || (!phone).then(|| if edit_all.get() { "Back to reading" } else { "Edit the doc: a click on a block opens it in Markdown" })
            on:click=move |_| toggle_edit.run(())>
            {phone.then(crate::icons::pencil)}
            {move || if phone {
                (if edit_all.get() { "Done" } else { "Edit document" }).into_any()
            } else if edit_all.get() {
                view! { "Done" }.into_any()
            } else {
                view! { {crate::icons::pencil()}" Edit" }.into_any()
            }}
        </button>
    }
    .into_any()
}

/// "Send N to agent": your open comments not sent yet go, each thread to
/// its session. The phone keeps its arrow before the text; the bar after it.
fn send_all_button(ctx: ThreadsCtx, arrow: bool, phone: bool) -> AnyView {
    view! {
        <button role=phone.then_some("menuitem")
            class=if phone { DOC_PHONE_ACTION } else { "shrink-0 rounded-md bg-sky-600 px-2 py-1 text-xs font-medium text-white hover:bg-sky-700 disabled:opacity-50" }
            title=(!phone).then_some("Your open comments that haven't gone to an agent: each thread goes to its session, which answers in it")
            disabled=move || ctx.sending.get() on:click=move |_| ctx.send_all.run(())>
            {(phone && arrow).then(crate::icons::arrow_up)}
            {move || format!("Send {} to agent", ctx.unsent.get())}{(!phone && arrow).then(|| view! { " "{crate::icons::arrow_up()} })}
        </button>
    }
    .into_any()
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
                        {move || format!("Used in {}", n.get())}
                        {crate::icons::chevron_down()}
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
fn FloatingPanel(open: Signal<bool>) -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let node = NodeRef::<leptos::html::Div>::new();
    dismiss(node, "[data-opens=comments]", move || {
        if open.get_untracked() {
            let _ = ctx.panel.try_set(false);
        }
    });
    follow_popover(node, move || {
        if let Some(el) = node.try_get_untracked().flatten()
            && let Some(bar) = el.parent_element()
        {
            place_popover(&el, &bar, ctx.root);
        }
    });
    view! {
        <div node_ref=node class="c6-pane c6-presence absolute z-10 flex w-[26rem] max-w-[calc(100%-1rem)] flex-col overflow-hidden rounded-lg border border-zinc-200 bg-white shadow-xl dark:border-zinc-700 dark:bg-zinc-900"
            data-open=move || open.get().to_string() inert=move || !open.get()>
            <CommentsPanel />
        </div>
    }
}

/// A sheet from the owning tile's bottom. Deliberately absolute in that
/// tile, not fixed under containment or fixed across the entire workspace.
/// The tile clips the spring's overshoot; other tiles remain interactive.
#[component]
fn Sheet(open: Signal<bool>, space: RwSignal<f64>, close: Callback<()>, children: ChildrenFn) -> impl IntoView {
    use wasm_bindgen::JsCast;
    let ctx = expect_context::<ThreadsCtx>();
    let mount = ctx.root.get_untracked().and_then(|root| root.closest("[data-tile-surface]").ok().flatten());
    let position = if mount.is_some() { "absolute" } else { "fixed" };
    let mount = mount.unwrap_or_else(|| document().body().expect("body").unchecked_into());
    let node = NodeRef::<leptos::html::Div>::new();
    let column = use_column();
    // Safari does not always focus a clicked button. Keep the actual block
    // marker (or panel button) as the fallback, not body or another tile.
    let opener = move || {
        ctx.root
            .get_untracked()
            .and_then(|root| {
                if ctx.pop.get_untracked().is_none() && crate::util::phone() {
                    // The phone's menu item has already unmounted. Return
                    // to its persistent opener, not the hidden desktop bar.
                    return root
                        .closest("[data-tile-surface]")
                        .ok()
                        .flatten()
                        .and_then(|tile| tile.query_selector("[data-tile-bar] button[aria-label=Actions]").ok().flatten());
                }
                let selector = ctx
                    .pop
                    .get_untracked()
                    .map(|b| format!("{} > [data-marks] button", block_sel(&b)))
                    .unwrap_or_else(|| "[data-opens=comments]".into());
                root.query_selector(&selector).ok().flatten().or_else(|| document().active_element().filter(|el| root.contains(Some(el))))
            })
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
    };
    let previous = StoredValue::new(send_wrapper::SendWrapper::new(opener()));
    Effect::new(move || {
        let showing = open.get();
        ctx.pop.track();
        // A retained sheet can reopen on a different block during its exit.
        // Return to that opener, not the one from the original mount.
        if showing {
            previous.set_value(send_wrapper::SendWrapper::new(opener()));
        }
    });
    on_cleanup(move || {
        // Changing Fit or handing a block sheet to All comments is not a
        // dismissal. Do not focus the old marker over the replacement UI.
        if ctx.panel.try_get_untracked() == Some(true) || ctx.pop.try_get_untracked().flatten().is_some() {
            return;
        }
        // Closing one tile must not steal focus back from another. A removed
        // tile/opener likewise has nowhere to return focus to.
        let fallback = ctx
            .root
            .try_get_untracked()
            .flatten()
            .and_then(|r| {
                if crate::util::phone() {
                    r.closest("[data-tile-surface]")
                        .ok()
                        .flatten()
                        .and_then(|tile| tile.query_selector("[data-tile-bar] button[aria-label=Actions]").ok().flatten())
                } else {
                    r.query_selector("[data-opens=comments]").ok().flatten()
                }
            })
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok());
        let previous = previous.get_value();
        if let Some(target) = previous.as_ref().filter(|el| el.is_connected()).or(fallback.as_ref()) {
            let active_tile = document().active_element().and_then(|el| el.closest("[data-tile-surface]").ok().flatten());
            let own_tile = target.closest("[data-tile-surface]").ok().flatten();
            if active_tile.is_none() || active_tile == own_tile {
                let options = web_sys::FocusOptions::new();
                options.set_prevent_scroll(true);
                let _ = target.focus_with_options(&options);
            }
        }
    });
    let placed = StoredValue::new(None::<(Rect, f64, f64, f64, Option<String>)>);
    let place = move || {
        let Some(el) = node.try_get_untracked().flatten() else { return };
        let v = visible_viewport();
        let tile = el.offset_parent().map(|p| rect(p.get_bounding_client_rect())).unwrap_or(v);
        let visible = (tile.top + tile.height).min(v.top + v.height) - tile.top.max(v.top);
        let style = web_sys::HtmlElement::style(&el);
        let bottom_inset = (tile.top + tile.height - v.top - v.height).max(0.0);
        let _ = style.set_property("bottom", &format!("{bottom_inset}px"));
        let block = ctx.pop.get_untracked();
        let mut limit = visible.max(0.0) * 0.85;
        if let Some(root) = ctx.root.try_get_untracked().flatten()
            && let Some(area) = root.parent_element()
            && let Some(block) = block.as_ref().and_then(|b| root.query_selector(&block_sel(b)).ok().flatten())
        {
            // A long thread must leave actual reading room above it, not just
            // 15% of a short tile (mostly occupied by the tile's bar). A huge
            // block gets a readable beginning; the sheet keeps at least half
            // the visible tile for its own scrolling controls.
            let context = block.get_bounding_client_rect().height().min(visible.max(0.0) * 0.35);
            let room = tile.top + tile.height - bottom_inset - area.get_bounding_client_rect().top().max(v.top) - context - 16.0;
            limit = limit.min(room.max(visible.max(0.0) * 0.5));
        }
        let _ = style.set_property("max-height", &format!("{limit}px"));
        // The phone spring paints an overshoot tail below its layout bottom.
        let height = f64::from(el.offset_height())
            + window()
                .get_computed_style(&el)
                .ok()
                .flatten()
                .and_then(|s| s.get_property_value("margin-bottom").ok())
                .and_then(|s| s.trim_end_matches("px").parse::<f64>().ok())
                .unwrap_or(0.0)
                .min(0.0);
        let next = (tile, height, bottom_inset, v.top, block.clone());
        if placed.get_value().as_ref() == Some(&next) {
            return;
        }
        placed.set_value(Some(next));
        // Make room even for the final block. Only scroll on opening or a
        // real geometry change, not on every scroll event while reading.
        // The document owns the reservation across sheet handoffs and exits;
        // an outgoing sheet must not erase the incoming sheet's space.
        let _ = space.try_set(height + 16.0);
        if let Some(block) = block {
            // Reactive styles are queued. Let the reservation reach the DOM
            // before scrolling, or the final block clamps at the old end.
            request_animation_frame(move || {
                if open.try_get_untracked() != Some(true) || ctx.pop.try_get_untracked().flatten().as_ref() != Some(&block) {
                    return;
                }
                let Some(root) = ctx.root.try_get_untracked().flatten() else { return };
                if let Some(area) = root.parent_element()
                    && let Some(block) = root.query_selector(&block_sel(&block)).ok().flatten()
                {
                    let a = area.get_bounding_client_rect();
                    let b = block.get_bounding_client_rect();
                    let top = a.top().max(v.top) + 8.0;
                    // Final layout top, not the entrance animation's translated top.
                    let bottom = tile.top + tile.height - bottom_inset - height - 8.0;
                    let target = b.top().clamp(top, (bottom - b.height()).max(top));
                    area.set_scroll_top(area.scroll_top() + (b.top() - target).round() as i32);
                }
            });
        }
    };
    follow_popover(node, place);
    Effect::new(move || {
        let showing = open.get();
        ctx.pop.track();
        node.track();
        // Presence can reverse an exit without mounting another Sheet. A
        // reopen or a different block still needs its own placement pass.
        placed.set_value(None);
        if showing {
            request_animation_frame(move || {
                if open.try_get_untracked() == Some(true) {
                    place();
                }
            });
        }
    });
    let keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if open.try_get_untracked() != Some(true) || ev.key() != "Escape" || ev.default_prevented() {
            return;
        }
        let Some(el) = node.try_get_untracked().flatten() else { return };
        let target = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok());
        let target_tile = target.as_ref().and_then(|t| t.closest("[data-tile-surface]").ok().flatten());
        let same_tile = if target_tile.is_some() {
            target_tile == el.closest("[data-tile-surface]").ok().flatten()
        } else {
            // With no focused DOM control, Escape belongs to the workspace's
            // focused tile, never every open document sheet.
            column.is_some_and(|c| c.ws.layout.with_untracked(|l| l.tiles.get(l.focus).is_some_and(|t| t.key == c.tile)))
        };
        let other_sheet = target.and_then(|t| t.closest(".c6-sheet").ok().flatten()).is_some_and(|s| !s.is_same_node(Some(&el)));
        if same_tile && !other_sheet {
            ev.prevent_default();
            close.run(());
        }
    });
    on_cleanup(move || keys.remove());
    view! {
        <leptos::portal::Portal mount=mount>
        <div class=format!("c6-comment-scrim c6-scrim c6-presence {position} inset-0 z-40 bg-black/30") data-open=move || open.get().to_string() inert=move || !open.get()
            on:click=move |ev| { ev.stop_propagation(); close.run(()); }></div>
        <div node_ref=node class=format!("c6-sheet c6-presence phone-sheet {position} inset-x-0 bottom-0 z-50 flex max-h-[85%] flex-col overflow-hidden rounded-t-xl border-t border-zinc-200 bg-white text-sm text-zinc-900 shadow-2xl dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-100")
            data-open=move || open.get().to_string() inert=move || !open.get()>
            {children()}
        </div>
        </leptos::portal::Portal>
    }
}

/// Comments (docs/docs.md §8): open or resolved threads, each with what
/// it's about; a new one on the whole tab; and your comments not sent to an
/// agent yet, to send.
#[component]
fn CommentsPanel() -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let archived = read_only();
    let resolved = RwSignal::new(false);
    let list = NodeRef::<leptos::html::Div>::new();
    let ids = Memo::new(move |previous: Option<&Vec<String>>| {
        let resolved = resolved.get();
        let next = ctx.shown.with(|ts| ctx.tabs.with(|tabs| ordered_threads(ts, tabs, resolved)));
        // Capture rows before For changes the keyed DOM; comment edits and
        // new replies keep these IDs and never start list motion.
        if previous.is_some_and(|previous| previous != &next) {
            #[cfg(target_arch = "wasm32")]
            crate::util::flip_list(list, &next);
        }
        next
    });
    // A new comment on the tab, or on what has no popover (a slide).
    let new_on = Memo::new(move |_| ctx.commenting.get().filter(|b| !archived.get() && ctx.pop.with(|p| p.as_ref() != Some(b))));
    let seg = move |on: bool| {
        format!(
            "rounded px-2 py-0.5 {}",
            if on { "bg-white font-medium shadow-sm dark:bg-zinc-700" } else { "text-muted hover:text-zinc-900 dark:hover:text-zinc-100" }
        )
    };
    view! {
        <div data-comments-panel="" class="flex min-h-0 flex-1 flex-col overflow-hidden">
        <div class="flex shrink-0 items-center gap-2 border-b border-zinc-200 px-3 py-2 dark:border-zinc-800">
            <h2 class="flex-1 text-sm font-semibold">"Comments"</h2>
            <div class="flex rounded-md bg-zinc-100 p-0.5 text-xs dark:bg-zinc-800" role="group">
                <button class=move || seg(!resolved.get()) aria-pressed=move || (!resolved.get()).to_string() on:click=move |_| resolved.set(false)>"Open"</button>
                <button class=move || seg(resolved.get()) aria-pressed=move || resolved.get().to_string() on:click=move |_| resolved.set(true)>"Resolved"</button>
            </div>
            <button class="rounded px-1.5 text-lg leading-none text-muted hover:bg-zinc-100 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                title="Close" aria-label="Close the comments" on:click=move |_| ctx.panel.set(false)>{crate::icons::icon(crate::icons::Icon::X, crate::icons::Size::S14)}</button>
        </div>
        <div node_ref=list data-comments-list="" class="min-h-0 flex-1 space-y-2 overflow-y-auto overscroll-contain p-3 text-sm">
            <div class="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted">
            <Show when=move || !archived.get()>
            <button class="text-xs text-muted hover:text-zinc-900 hover:underline dark:hover:text-zinc-100"
                on:click=move |_| { ctx.focus.set(Some(String::new())); ctx.commenting.set(Some(String::new())) }>{crate::icons::icon(crate::icons::Icon::Plus, crate::icons::Size::S14)}" On the whole tab"</button>
            </Show>
                <span class="flex-1"></span>
                <button class="hover:underline" on:click=move |_| ctx.expand(&ids.get_untracked(), false)>"Collapse all"</button>
                <button class="hover:underline" on:click=move |_| ctx.expand(&ids.get_untracked(), true)>"Expand all"</button>
            </div>
            {move || new_on.get().map(|b| view! { <NewComment block=b done=Callback::new(move |()| ctx.commenting.set(None)) /> })}
            <For each=move || ids.get() key=|id| id.clone() children=move |id| view! {
                <div data-motion-key=id.clone()><PanelThread id=id.clone() /></div>
            } />
            {move || ids.with(Vec::is_empty).then(|| view! {
                <p class="text-muted">{if resolved.get() { view! { "No resolved comments." }.into_any() } else { view! { "No open comments. Press "{crate::icons::icon(crate::icons::Icon::MessageSquare, crate::icons::Size::S14)}" by a block to start one." }.into_any() }}</p>
            })}
        </div>
        {move || (!archived.get() && ctx.unsent.get() > 0).then(|| view! {
            <div class="flex shrink-0 items-center gap-2 border-t border-zinc-200 px-3 py-2 dark:border-zinc-800">
                <span class="flex-1 text-xs text-muted">{move || format!("{} not sent yet", ctx.unsent.get())}</span>
                {send_all_button(ctx, false, false)}
            </div>
        })}
        </div>
    }
}

/// Stable document order, including nested blocks and other tabs. Whole-tab
/// comments precede that tab's blocks; missing anchors follow the known doc.
fn ordered_threads(threads: &[DocThread], tabs: &[DocTab], resolved: bool) -> Vec<String> {
    fn blocks(bs: &[DocBlock], tab: usize, next: &mut usize, order: &mut std::collections::HashMap<String, (usize, usize)>) {
        for b in bs {
            order.insert(b.id.clone(), (tab, *next));
            *next += 1;
            blocks(&b.children, tab, next, order);
        }
    }
    let mut order = std::collections::HashMap::new();
    for (i, tab) in tabs.iter().enumerate() {
        blocks(&tab.blocks, i, &mut 1, &mut order);
    }
    let mut shown = threads.iter().filter(|t| (t.state != "open") == resolved).collect::<Vec<_>>();
    shown.sort_by_key(|t| {
        if t.anchor.block.is_empty() {
            (if t.anchor.tab.is_empty() { 0 } else { tabs.iter().position(|tab| tab.id == t.anchor.tab).unwrap_or(tabs.len()) }, 0)
        } else {
            order.get(&t.anchor.block).copied().unwrap_or((tabs.len(), usize::MAX))
        }
    });
    shown.into_iter().map(|t| t.id.clone()).collect()
}

#[component]
fn PanelThread(id: String) -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let id = StoredValue::new(id);
    let t = Memo::new(move |_| ctx.shown.with(|ts| ts.iter().find(|t| t.id == id.get_value()).cloned().unwrap_or_default()));
    let expanded = Memo::new(move |_| ctx.prefs.with(|p| p.expanded.contains(&id.get_value())));
    Effect::new(move || {
        if expanded.get() && ctx.panel.get() {
            t.with(|t| ctx.read(t));
        }
    });
    let unread =
        move || !expanded.get() && t.with(|t| t.comments.last().is_some_and(|c| ctx.prefs.with(|p| p.read.get(&t.id) != Some(&c.id))));
    view! {
        <section data-panel-thread=id.get_value()>
            <button data-thread-toggle=id.get_value() class="flex min-h-10 w-full items-center gap-2 rounded px-1 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800"
                aria-expanded=move || expanded.get().to_string()
                on:click=move |_| ctx.expand(&[id.get_value()], !expanded.get_untracked())>
                <span class=move || if expanded.get() { "shrink-0 rotate-90" } else { "shrink-0" }>{crate::icons::icon(crate::icons::Icon::ChevronRight, crate::icons::Size::S14)}</span>
                <span class="min-w-0 flex-1 truncate">{move || t.with(|t| ctx.tabs.with(|tabs| subject(tabs, &t.anchor.block, &t.about, &ctx.doc.get_value()).text))}</span>
                {move || unread().then(|| view! { <span data-new-reply="" class="shrink-0 text-xs font-medium text-sky-700 dark:text-sky-400">"New reply"</span> })}
                <ThreadStatus t />
            </button>
            <Show when=move || expanded.get()><Thread id=id.get_value() about=true /></Show>
        </section>
    }
}

/// A block's comments, in a popover by its 💬 over the doc: a press
/// outside it or Escape closes it.
#[component]
fn BlockPop(block: String, open: Signal<bool>) -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let node = ctx.pop_node;
    let bid = StoredValue::new(block.clone());
    follow_popover(node, move || {
        let marker = ctx
            .root
            .try_get_untracked()
            .flatten()
            .and_then(|r| r.query_selector(&format!("{} > [data-marks]", block_sel(&bid.get_value()))).ok().flatten());
        if let Some(m) = marker {
            ctx.place_pop(&m);
        }
    });
    dismiss(node, "[data-marks]", move || {
        if open.get_untracked() {
            ctx.close_pop()
        }
    });
    view! {
        <div node_ref=node data-thread-box="" class="c6-pop c6-presence absolute z-30 flex w-[350px] flex-col gap-2 overflow-hidden rounded-lg border border-zinc-200 bg-white p-2 text-sm shadow-xl dark:border-zinc-700 dark:bg-zinc-900"
            data-open=move || open.get().to_string() inert=move || !open.get()>
            <PopBody block />
        </div>
    }
}

/// A block's box keeps focus on one conversation (or a new-thread draft).
#[component]
fn PopBody(block: String) -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let archived = read_only();
    let bid = StoredValue::new(block);
    let ids = Memo::new(move |_| {
        ctx.shown.with(|ts| {
            ts.iter().filter(|t| t.state == "open" && t.anchor.block == bid.get_value()).map(|t| t.id.clone()).collect::<Vec<_>>()
        })
    });
    let writing = Memo::new(move |_| ctx.commenting.with(|c| c.as_deref() == Some(bid.get_value().as_str())));
    let selected = Memo::new(move |_| ids.with(|ids| ctx.selected.get().filter(|id| ids.contains(id)).or_else(|| ids.first().cloned())));
    let t =
        Memo::new(move |_| ctx.shown.with(|ts| ts.iter().find(|t| Some(&t.id) == selected.get().as_ref()).cloned().unwrap_or_default()));
    Effect::new(move || {
        if ctx.pop.get().as_ref() == Some(&bid.get_value()) && !writing.get() {
            t.with(|t| ctx.read(t));
        }
    });
    let menu = RwSignal::new(false);
    let menu_id = StoredValue::new(crate::util::dismissable(move || menu.get(), move || menu.set(false)));
    let close = Callback::new(move |()| ctx.close_pop());
    view! {
        <div class="flex shrink-0 items-center gap-2 text-xs">
            <span class="min-w-0 flex-1"><Show when=move || !writing.get()><ThreadStatus t /></Show></span>
            <button class="rounded px-1 text-muted hover:bg-zinc-100 dark:hover:bg-zinc-800" aria-label="Thread options" title="Thread options"
                aria-expanded=move || menu.get().to_string() data-pop=menu_id.get_value()
                on:click=move |ev| { ev.stop_propagation(); menu.update(|m| *m = !*m); }>{crate::icons::ellipsis()}</button>
            <button class="rounded px-1 text-base leading-none text-muted hover:bg-zinc-100 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                title="Close" aria-label="Close" on:click=move |_| close.run(())>{crate::icons::icon(crate::icons::Icon::X, crate::icons::Size::S14)}</button>
        </div>
        <Show when=move || menu.get()>
        <div data-thread-menu="" data-pop=menu_id.get_value() class="flex shrink-0 flex-wrap gap-x-3 gap-y-1 rounded bg-zinc-100 p-2 text-xs dark:bg-zinc-800">
            <button class="text-muted hover:text-zinc-900 hover:underline dark:hover:text-zinc-100"
                on:click=move |_| { ctx.close_pop(); ctx.panel.set(true); }>"All comments "{crate::icons::icon(crate::icons::Icon::ArrowUpRight, crate::icons::Size::S14)}</button>
            <Show when=move || !archived.get()>
                <button class="text-xs text-muted hover:text-zinc-900 hover:underline dark:hover:text-zinc-100"
                    on:click=move |_| { menu.set(false); ctx.focus.set(Some(bid.get_value())); ctx.commenting.set(Some(bid.get_value())) }>{crate::icons::icon(crate::icons::Icon::Plus, crate::icons::Size::S14)}" New thread on this block"</button>
            </Show>
        </div>
        </Show>
        {move || if writing.get() && !archived.get() {
            view! { <div class="min-h-0 overflow-y-auto"><NewComment block=bid.get_value() done=close /></div> }.into_any()
        } else {
            selected.get().map(|id| view! { <Thread id about=false /> }).into_any()
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
    archived: Memo<bool>,
    reload: impl Fn() + 'static,
) {
    if archived.try_get_untracked() != Some(false) {
        return;
    }
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
    let author_named = author_names();
    let archived = read_only();
    let bid = StoredValue::new(b.id.clone());
    let block = StoredValue::new(b.clone());
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
        if archived.get_untracked() {
            return;
        }
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
        if archived.get_untracked() || b.kind == "pending" || is_open.get_untracked() {
            return;
        }
        // Another block being edited is saved first.
        if edit.get_untracked().is_some() {
            save(doc_id.get_value(), edit, draft, false, error, archived, move || reload.run(()));
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
            save(doc_id.get_value(), edit, draft, keep, error, archived, move || reload.run(()));
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
                    save(doc, edit, draft, true, error, archived, move || {
                        reload.try_run(());
                    });
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
        <div class=move || if archived.get() {
            "group relative rounded py-0.5 pl-2 pr-14"
        } else {
            "group relative rounded py-0.5 pl-2 pr-14 hover:bg-zinc-50 dark:hover:bg-zinc-900/60"
        } title=hover data-block=b.id.clone()>
            {move || if is_open.get() && !archived.get() {
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
                    <div class=move || if !archived.get() && edit_all.get() { "cursor-text" } else { "" }
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
            // panel's edge they sat over the scrollbar. 💬 shows on hover
            // or keyboard focus (always on a touch screen, which has none);
            // opacity keeps the opener focusable when a sheet closes. With open
            // comments, a lasting marker with their count opens them. A
            // block is edited by a double-click, or Edit in the doc's bar.
            <div data-marks="" class="absolute right-0.5 top-0.5 flex items-center">
                {move || match open_n.get() {
                    0 if archived.get() => ().into_any(),
                    0 => view! {
                        <button class="rounded px-1 text-xs text-muted opacity-0 hover:bg-zinc-100 group-hover:opacity-100 focus:opacity-100 pointer-coarse:opacity-100 dark:hover:bg-zinc-800"
                            title="Comment on this" aria-label="Comment on this block" on:click=move |ev| marker(ev, true)>{crate::icons::icon(crate::icons::Icon::MessageSquare, crate::icons::Size::S14)}</button>
                    }.into_any(),
                    n => view! {
                        <button class=move || format!("rounded px-1 text-xs font-medium text-soft hover:bg-zinc-100 dark:hover:bg-zinc-800 {}", if popped.get() { "bg-zinc-100 dark:bg-zinc-800" } else { "" })
                            title="Its comments" on:click=move |ev| marker(ev, false)>{crate::icons::icon(crate::icons::Icon::MessageSquare, crate::icons::Size::S14)}" "{n}</button>
                    }.into_any(),
                }}
            </div>
        </div>
    }
}

/// Archived decks use the same slide renderer without the sorter's write
/// controls. All slides and notes remain readable, including hidden slides.
#[component]
fn ArchivedDeck(doc: Doc, width: Signal<f64>) -> impl IntoView {
    use c6_api::slides::{BASE_CSS, default_themes, scoped_theme, slide_html};
    let id = doc.summary.id.clone();
    let themes = LocalResource::new(api::deck_themes);
    let theme = doc.summary.theme.clone();
    let css = move || {
        let themes = themes.get().and_then(Result::ok).unwrap_or_else(default_themes);
        let css = themes.iter().find(|t| t.name.eq_ignore_ascii_case(&theme)).or(themes.first()).map(|t| t.css.as_str()).unwrap_or("");
        format!("{}{BASE_CSS}", scoped_theme(css, &format!(".c6deck[data-deck=\"{id}\"]")))
    };
    let slides: Vec<_> = doc.tabs.into_iter().flat_map(|t| t.blocks).collect();
    let total = slides.len();
    view! {
        <div class="c6deck space-y-4" data-deck=doc.summary.id on:click=crate::mentions::link_clicks()>
            <style>{css}</style>
            {slides.into_iter().enumerate().map(|(i, slide)| {
                let html = slide_html(&slide, i + 1, total, &doc.summary.title, &crate::markdown::to_html);
                let notes = slide.attrs["notes"].as_str().unwrap_or("").to_owned();
                view! {
                    <section data-block=slide.id>
                        <crate::deck::SlideBox html width />
                        {(slide.attrs["hidden"] == true).then(|| view! { <p class="text-xs text-muted">"Hidden slide"</p> })}
                        {(!notes.is_empty()).then(|| view! { <p class="mt-1 whitespace-pre-wrap text-xs text-muted">{format!("Notes: {notes}")}</p> })}
                    </section>
                }
            }).collect_view()}
        </div>
    }
}

/// A block as it reads.
#[component]
fn Shown(b: DocBlock, kind: String) -> impl IntoView {
    let author_named = author_names();
    let archived = read_only();
    match b.kind.as_str() {
        "pending" => {
            let who = b.attrs["by"].as_str().map(author_named).unwrap_or_else(|| "an agent".into());
            view! {
                <div class="rounded border border-dashed border-violet-300 px-3 py-2 text-sm text-violet-800 dark:border-violet-800 dark:text-violet-300">
                    <span class="animate-pulse">{crate::icons::icon(crate::icons::Icon::PenLine, crate::icons::Size::S14)}" "</span>{b.attrs["intent"].as_str().unwrap_or("").to_owned()}
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
                            view! { <div class="md" inner_html=move || chips_html(&c.text, &c.attrs, &refs.map(|r| r.get()).unwrap_or_default(), archived.get())></div> }.into_any()
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
            view! { <div class="md" inner_html=move || chips_html(&b.text, &b.attrs, &refs.map(|r| r.get()).unwrap_or_default(), archived.get())></div> }
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
    file: bool,
    /// It's in the doc, to go to.
    here: bool,
}

/// What a thread on `block` is about: the block's first line, a picture
/// or diagram with its caption, "Whole tab"; once gone, what it said
/// (`about`).
fn subject(tabs: &[DocTab], block: &str, about: &str, doc: &str) -> Subject {
    let words = |text: String| Subject { thumb: None, text, file: false, here: true };
    if block.is_empty() {
        return Subject { thumb: None, text: "Whole tab".into(), file: false, here: false };
    }
    let found =
        tabs.iter().flat_map(|t| &t.blocks).find_map(|b| if b.id == block { Some(b) } else { b.children.iter().find(|c| c.id == block) });
    let Some(b) = found else {
        return Subject { thumb: None, text: first_line(about, &Value::Null), file: false, here: false };
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
            file: false,
            here: true,
        },
        ("embed", "diagram") => Subject {
            thumb: Some(format!("data:image/svg+xml;base64,{}", base64_of(s("svg").as_bytes()))),
            text: either(s("title"), "A diagram"),
            file: false,
            here: true,
        },
        ("embed", "chart") => words(either(s("title"), "A chart")),
        ("embed", "file") => Subject { file: true, ..words(s("name")) },
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

/// A thread's anchor, or none when its block or quoted words are gone.
fn comment_target(t: &DocThread, here: bool) -> Option<String> {
    (here && !t.detached && !t.anchor.block.is_empty()).then(|| t.anchor.block.clone())
}

/// A thread's quote box: what it's about, and locate it in the doc.
#[component]
fn About(t: Memo<DocThread>) -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let what = Memo::new(move |_| {
        let (block, about) = t.with(|t| (t.anchor.block.clone(), t.about.clone()));
        ctx.tabs.with(|ts| subject(ts, &block, &about, &ctx.doc.get_value()))
    });
    let target = Memo::new(move |_| t.with(|t| comment_target(t, what.with(|w| w.here))));
    let gone = RwSignal::new(false);
    let show = Callback::new(move |()| {
        if let Some(block) = target.get_untracked() {
            gone.set(false);
            ctx.reveal.run(vec![block]);
        } else {
            gone.set(true);
        }
    });
    view! {
        <>
        <div class="flex items-start gap-2 border-l-2 border-zinc-300 pl-2 dark:border-zinc-600">
            {move || what.with(|w| w.thumb.clone()).map(|src| view! {
                <img src=src alt="" class="h-8 w-8 shrink-0 rounded border border-zinc-200 bg-white object-contain dark:border-zinc-700" />
            })}
            <button type="button" class="min-w-0 flex-1 cursor-pointer select-text rounded text-left hover:bg-zinc-100 disabled:cursor-default dark:hover:bg-zinc-800"
                title="Show it in the doc" aria-label="Show it in the doc"
                disabled=move || t.with(|t| t.anchor.block.is_empty()) on:click=move |_| show.run(())>
                <span class="line-clamp-2 break-words">{move || what.with(|w| w.file).then(crate::icons::paperclip)}" "{move || what.with(|w| w.text.clone())}</span>
                {move || t.with(|t| t.anchor.quote.clone()).chars().next().is_some().then(|| view! {
                    <span class="block truncate text-xs text-muted">{move || format!("“{}”", t.with(|t| t.anchor.quote.clone()))}</span>
                })}
            </button>
            {move || t.with(|t| !t.anchor.block.is_empty()).then(|| view! {
                <button type="button" class="shrink-0 rounded px-1 text-muted enabled:hover:bg-zinc-100 enabled:hover:text-zinc-900 disabled:opacity-40 dark:enabled:hover:bg-zinc-800 dark:enabled:hover:text-zinc-100"
                    title="Show it in the doc" aria-label="Show it in the doc"
                    disabled=move || target.with(Option::is_none)
                    on:click=move |_| show.run(())>{crate::icons::locate_fixed()}</button>
            })}
        </div>
        {move || gone.get().then(|| view! { <p class="mt-1 text-xs text-muted" role="status">"That part of the doc is gone"</p> })}
        </>
    }
}

/// Writing a new comment: on a block (`block`, in its popover) or on the
/// whole tab or a slide (in the panel). `done`: Cancel or Escape (the
/// draft stays).
#[component]
fn NewComment(block: String, done: Callback<()>) -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let archived = read_only();
    let doc = ctx.doc;
    let error = RwSignal::new(None::<String>);
    // Kept as drafts: the box goes when its popover does, and may be
    // drawn again.
    let keys = StoredValue::new((format!("comment.{}.{block}", doc.get_value()), format!("comment-quote.{}.{block}", doc.get_value())));
    let (text, quote) = keys.with_value(|(t, q)| (crate::util::draft(t), crate::util::draft(q)));
    // Sticky: as you last left it, for the next comment too.
    let to_agent = RwSignal::new(crate::util::storage_get("comment.to_agent").as_deref() == Some("1"));
    Effect::new(move || crate::util::storage_set("comment.to_agent", if to_agent.get() { "1" } else { "0" }));
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
            let options = web_sys::FocusOptions::new();
            options.set_prevent_scroll(true);
            let _ = el.focus_with_options(&options);
        }
    });
    crate::util::autosize(writing, move || text.track());
    let post = move || {
        if archived.get_untracked() {
            return;
        }
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
        let posted_block = block.get_value();
        let (text_key, quote_key) = keys.get_value();
        let doc_id = doc.get_value();
        spawn_local(async move {
            match api::doc_thread_new(doc_id, &t).await {
                Ok(thread) => {
                    // Put the returned conversation in place before selecting
                    // it; the refresh must not flash an older thread first.
                    let new_id = thread.id.clone();
                    ctx.upsert(thread);
                    if ctx.commenting.try_get_untracked().flatten().as_ref() == Some(&posted_block) {
                        let _ = ctx.selected.try_set(Some(new_id));
                        let _ = ctx.commenting.try_set(None);
                    }
                    let _ = ctx.tick.try_update(|t| *t += 1);
                }
                // Not posted: what you wrote comes back.
                Err(e) => {
                    crate::util::storage_set(&format!("c6.draft.{text_key}"), &said);
                    crate::util::storage_set(&format!("c6.draft.{quote_key}"), &quoted);
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
            <textarea node_ref=writing rows="1" class=format!("{INPUT} {COMMENT_GROWS}") placeholder="Your comment" prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))
                on:keydown=move |ev| if ev.key() == "Escape" { ev.prevent_default(); done.run(()) } else if crate::util::sends(&ev) { ev.prevent_default(); post() } />
            <div class="flex flex-wrap items-center gap-2">
                <div class="min-w-0 flex-1 basis-40">
                    <crate::components::Switch on=move || to_agent.get() on_change=move |v| to_agent.set(v) label="Send to agent" title="The session that last wrote it gets the thread (queued if busy), else the newest one of the document's work or project" />
                </div>
                <div class="flex shrink-0 items-center gap-2">
                    <button class=BUTTON_QUIET on:click=move |_| done.run(())>"Cancel"</button>
                    <button class=BUTTON on:click=move |_| post()>"Comment"</button>
                </div>
            </div>
        </div>
    }
}

/// A comment box: one line, growing with its text to about eight, then
/// scrolling.
const COMMENT_GROWS: &str = "grows block max-h-44 resize-none overflow-y-auto";

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

/// Existing thread states, not a new attention or ordering policy.
#[component]
fn ThreadStatus(t: Memo<DocThread>) -> impl IntoView {
    move || {
        t.with(|t| {
            if t.state != "open" && !t.state.is_empty() {
                view! { <span class="shrink-0 text-xs text-muted">{format!("resolved by {}", author(&t.resolved_by))}</span> }.into_any()
            } else if t.needs_you {
                view! { <span class="shrink-0 text-xs text-amber-800 dark:text-amber-300">"Your move"</span> }.into_any()
            } else if t.detached {
                view! { <span class="shrink-0 text-xs text-muted" title="What it was about is gone or changed">"detached"</span> }
                    .into_any()
            } else if t.unsent {
                view! { <span class="shrink-0 text-xs text-muted">"Send now"</span> }.into_any()
            } else if t.to_agent && t.comments.last().is_some_and(|c| c.by != format!("session:{}", t.agent)) {
                view! { <span class="shrink-0 text-xs text-muted"><AgentOnIt session=t.agent.clone() /></span> }.into_any()
            } else {
                ().into_any()
            }
        })
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

/// An agent's comment: agents write Markdown, so it renders as in the
/// transcript, its links opening in the workspace where there is one.
fn agent_text(text: &str) -> impl IntoView {
    let (col, ws) = (crate::workspace::use_column(), use_context::<crate::workspace::Workspace>());
    view! {
        <div class="md max-w-[85%] break-words rounded-xl bg-zinc-100 px-3 py-1.5 dark:bg-zinc-800"
            on:click=move |ev| if let Some(ws) = ws { crate::mentions::follow_link(&ev, col, ws); }
            inner_html=crate::markdown::to_html(text)></div>
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
            {agent_text(&text)}
            {under}
        </div>
    }
    .into_any()
}

/// An icon button on your comment: shown on hover with a mouse (and to the
/// keyboard), always on a touch screen.
/// Edit, Delete and Cancel under your comment: real buttons, roomy on a
/// phone.
const SMALL_BUTTON: &str = "rounded-md border border-zinc-300 px-3 py-1.5 text-sm hover:bg-zinc-100 pointer-coarse:px-4 pointer-coarse:py-2 dark:border-zinc-600 dark:hover:bg-zinc-800";
const COMMENT_TOOL: &str = "rounded px-1.5 leading-none text-muted opacity-60 pointer-coarse:px-2.5 pointer-coarse:py-1.5 hover:bg-zinc-100 hover:text-zinc-900 focus-visible:opacity-100 group-hover/said:opacity-100 pointer-coarse:opacity-100 dark:hover:bg-zinc-800 dark:hover:text-zinc-100";

/// Your comment that no agent has received yet: as `said`, with edit and
/// delete beside who wrote it. Edit makes it a box like the reply's (Enter
/// saves, Escape cancels); delete asks first. Its thread goes with its only
/// comment.
#[component]
fn MyComment(c: c6_api::docs::DocComment, thread: String) -> impl IntoView {
    let ctx = expect_context::<ThreadsCtx>();
    let archived = read_only();
    let (doc, tick) = (ctx.doc, ctx.tick);
    let key = StoredValue::new(format!("comment-edit.{}.{}.{}", doc.get_value(), thread, c.id));
    let ids = StoredValue::new((thread, c.id));
    let when = since(c.at).trim_end_matches(" ago").to_owned();
    let shown = RwSignal::new(c.text);
    let text = crate::util::draft(&key.get_value());
    let editing = RwSignal::new(!text.get_untracked().is_empty());
    let deleting = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let menu = RwSignal::new(false);
    let pop = StoredValue::new(crate::util::dismissable(move || menu.get(), move || menu.set(false)));
    let area = NodeRef::<leptos::html::Textarea>::new();
    crate::util::autosize(area, move || text.track());
    // Into the box as it opens, the cursor after the text.
    Effect::new(move || {
        if let Some(el) = area.get() {
            let options = web_sys::FocusOptions::new();
            options.set_prevent_scroll(true);
            let _ = el.focus_with_options(&options);
            let end = el.value().encode_utf16().count() as u32;
            let _ = el.set_selection_range(end, end);
        }
    });
    let edit = move |_| {
        if text.get_untracked().is_empty() {
            text.set(shown.get_untracked());
        }
        error.set(None);
        editing.set(true);
    };
    let cancel = move || {
        ctx.edit_epochs.update_value(|versions| {
            *versions.entry(key.get_value()).or_default() += 1;
        });
        crate::util::forget_draft(&key.get_value());
        text.set(String::new());
        editing.set(false);
        error.set(None);
    };
    let save = move || {
        let said = text.get_untracked();
        if archived.get_untracked() || busy.get_untracked() || said.trim().is_empty() {
            return;
        }
        if said.trim() == shown.with_untracked(|s| s.trim().to_owned()) {
            crate::util::forget_draft(&key.get_value());
            text.set(String::new());
            editing.set(false);
            return;
        }
        busy.set(true);
        let (thread, comment) = ids.get_value();
        let (doc_id, draft_key) = (doc.get_value(), key.get_value());
        let epoch = ctx.edit_epochs.with_value(|v| v.get(&draft_key).copied().unwrap_or(0));
        spawn_local(async move {
            let r = api::doc_comment_edit(doc_id, thread, comment, said.clone()).await;
            let _ = busy.try_set(false);
            match r {
                Ok(_) => {
                    if ctx.edit_epochs.try_with_value(|v| v.get(&draft_key).copied().unwrap_or(0)) == Some(epoch) {
                        crate::util::forget_draft(&draft_key);
                        let _ = text.try_set(String::new());
                        let _ = editing.try_set(false);
                    }
                    let _ = shown.try_set(said.trim().to_owned());
                    let _ = tick.try_update(|t| *t += 1);
                }
                // It stays open with what you wrote (an agent may have got
                // it meanwhile: the message says).
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
        });
    };
    let delete = move |_| {
        if archived.get_untracked() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        let (thread, comment) = ids.get_value();
        let (doc_id, draft_key) = (doc.get_value(), key.get_value());
        spawn_local(async move {
            let r = api::doc_comment_delete(doc_id, thread, comment).await;
            let _ = busy.try_set(false);
            let _ = deleting.try_set(false);
            match r {
                Ok(()) => {
                    crate::util::forget_draft(&draft_key);
                    let _ = tick.try_update(|t| *t += 1);
                }
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
        });
    };
    view! {
        <div class="group/said flex flex-col items-end">
            <div class="mb-0.5 flex items-center gap-0.5 text-xs text-muted">
                {move || (!archived.get() && !editing.get() && !deleting.get()).then(|| view! {
                    <button class=COMMENT_TOOL title="Edit or delete your comment (no agent has it yet)" aria-label="Edit or delete your comment"
                        aria-expanded=move || menu.get().to_string() data-pop=pop.get_value()
                        on:click=move |ev| { ev.stop_propagation(); menu.update(|o| *o = !*o) }>{crate::icons::ellipsis()}</button>
                })}
                <span class="ml-1">{format!("You · {when}")}</span>
            </div>
            {move || if editing.get() && !archived.get() {
                view! {
                    <div class="w-full space-y-1">
                        <textarea node_ref=area rows="1" class=format!("{INPUT} {COMMENT_GROWS}") prop:value=move || text.get() on:input=move |ev| {
                            ctx.edit_epochs.update_value(|versions| { *versions.entry(key.get_value()).or_default() += 1; });
                            text.set(event_target_value(&ev));
                        }
                            on:keydown=move |ev| if ev.key() == "Escape" { ev.prevent_default(); cancel() } else if crate::util::sends(&ev) { ev.prevent_default(); save() } />
                        <div class="flex justify-end gap-2">
                            <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| cancel()>"Cancel"</button>
                            <button class=format!("{BUTTON} text-xs") disabled=move || busy.get() || text.with(|t| t.trim().is_empty()) on:click=move |_| save()>"Save"</button>
                        </div>
                    </div>
                }
                .into_any()
            } else {
                view! {
                    <p class="max-w-[85%] whitespace-pre-wrap break-words rounded-xl bg-sky-50 px-3 py-1.5 text-zinc-900 dark:bg-sky-950/60 dark:text-zinc-100">{move || shown.get()}</p>
                }
                .into_any()
            }}
            // Edit and Delete under the comment, in the thread's own flow: a
            // menu floating over it was cut off by the list's scrolling.
            {move || (!archived.get() && menu.get() && !editing.get() && !deleting.get()).then(|| view! {
                <div data-pop=pop.get_value() class="mt-1.5 flex gap-3">
                    <button class=SMALL_BUTTON on:click=move |ev| { menu.set(false); edit(ev) }>"Edit"</button>
                    <button class=format!("{SMALL_BUTTON} text-red-700 dark:text-red-400")
                        on:click=move |_| { menu.set(false); error.set(None); deleting.set(true) }>"Delete"</button>
                </div>
            })}
            {move || (!archived.get() && deleting.get()).then(|| view! {
                <div class="mt-1.5 flex flex-col items-end gap-2">
                    <span class="text-xs text-soft">"Delete this comment?"</span>
                    <div class="flex gap-3">
                        <button class=SMALL_BUTTON on:click=move |_| deleting.set(false)>"Cancel"</button>
                        <button class="rounded-md bg-red-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-red-700 disabled:opacity-50 pointer-coarse:px-4 pointer-coarse:py-2"
                            disabled=move || busy.get() on:click=delete>"Delete"</button>
                    </div>
                </div>
            })}
            {move || error.get().map(|e| view! { <div class="w-full"><ErrorBox message=e /></div> })}
        </div>
    }
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
    let archived = read_only();
    let (doc, tick) = (ctx.doc, ctx.tick);
    let tid = StoredValue::new(id);
    let t = Memo::new(move |_| ctx.shown.with(|ts| ts.iter().find(|t| t.id == tid.get_value()).cloned().unwrap_or_default()));
    // Opening an existing thread is not an arrival. Keyed comment redraws
    // (editing text or updating editability) are not arrivals either.
    let seen = StoredValue::new(t.with_untracked(|t| t.comments.iter().map(|c| c.id.clone()).collect::<std::collections::HashSet<_>>()));
    // A draft: the thread may be drawn again (in the panel and a popover).
    let key = StoredValue::new(format!("reply.{}.{}", doc.get_value(), tid.get_value()));
    let reply = crate::util::draft(&key.get_value());
    // Failed sends are separate from the next draft. Persist immediately:
    // the request can finish after this thread's box has been closed.
    let failed_key = StoredValue::new(format!("c6.failed.{}", key.get_value()));
    let failed = RwSignal::new(failed_replies(&failed_key.get_value()));
    // With an agent already, a reply goes on to it unless you say not.
    let to_agent = RwSignal::new(t.with_untracked(|t| t.to_agent && !t.agent.is_empty()));
    let error = RwSignal::new(None::<String>);
    let done = move |r: Result<DocThread, String>| match r {
        Ok(thread) => {
            // A user-resolved single conversation closes, rather than
            // silently switching to another thread on the same block.
            if !about
                && thread.state != "open"
                && ctx.pop.try_get_untracked().flatten().as_ref() == Some(&thread.anchor.block)
                && ctx.selected.try_get_untracked().flatten().is_none_or(|id| id == thread.id)
            {
                ctx.close_pop();
            }
            ctx.upsert(thread);
            let _ = error.try_set(None);
            let _ = tick.try_update(|t| *t += 1);
        }
        Err(e) => {
            let _ = error.try_set(Some(e));
        }
    };
    // The draft goes as it's sent. A failure stays recoverable without
    // replacing anything written while the request was in flight.
    let send = move || {
        if archived.get_untracked() {
            return;
        }
        let c = c6_api::docs::CommentNew {
            text: reply.get_untracked(),
            to: if to_agent.get_untracked() { "agent".into() } else { String::new() },
        };
        if c.text.trim().is_empty() {
            return;
        }
        crate::util::forget_draft(&key.get_value());
        reply.set(String::new());
        let (doc_id, thread_id, recovery_key) = (doc.get_value(), tid.get_value(), failed_key.get_value());
        spawn_local(async move {
            let r = api::doc_thread_reply(doc_id, thread_id, &c).await;
            if r.is_err() {
                let mut replies = failed_replies(&recovery_key);
                replies.push(c);
                save_failed_replies(&recovery_key, &replies);
                let _ = failed.try_set(replies);
            }
            done(r)
        });
    };
    // Its last word, yours, hasn't gone to an agent: send it now.
    let send_unsent = move |_| {
        if archived.get_untracked() {
            return;
        }
        let (doc_id, thread_id) = (doc.get_value(), tid.get_value());
        spawn_local(async move {
            match api::doc_threads_send(doc_id, vec![thread_id]).await {
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
        if !about && list.get().is_some() {
            request_animation_frame(move || {
                if let Some(el) = list.try_get_untracked().flatten() {
                    el.set_scroll_top(el.scroll_height());
                }
            });
        }
    });
    let open = Memo::new(move |_| t.with(|t| t.state == "open"));
    // That it changed the doc goes under the agent's newest comment (after
    // them all if it hasn't written one): which comment made which edit
    // isn't kept. Each comment is drawn again only when it changes, so one
    // you're editing keeps what you type as others come in.
    let comments = move || {
        t.with(|t| {
            let last_agent = t.comments.iter().rposition(|c| c.by.starts_with("session:"));
            t.comments
                .iter()
                .enumerate()
                .map(|(i, c)| (c.clone(), (Some(i) == last_agent && !t.edits.is_empty()).then(|| t.edits.clone())))
                .collect::<Vec<_>>()
        })
    };
    let changed_after = move || {
        t.with(|t| (!t.edits.is_empty() && !t.comments.iter().any(|c| c.by.starts_with("session:"))).then(|| changed_the_doc(ctx, t)))
    };
    view! {
        <div class=if about { "space-y-2 rounded-lg border border-zinc-200 p-2 text-sm dark:border-zinc-800" } else { "flex min-h-0 flex-1 flex-col gap-2 text-sm" } data-thread=tid.get_value()>
            {about.then(|| view! { <About t /> })}
            <div node_ref=list data-thread-comments="" class=if about { "space-y-2" } else { "min-h-0 flex-1 space-y-2 overflow-y-auto overscroll-contain" }>
                <For each=comments key=|(c, edits)| (c.id.clone(), c.text.clone(), c.editable, edits.clone()) children=move |(c, edits)| {
                    let arrives = !seen.with_value(|ids| ids.contains(&c.id));
                    seen.update_value(|ids| { ids.insert(c.id.clone()); });
                    let body = if c.by == "you" && c.editable {
                        view! { <MyComment c thread=tid.get_value() /> }.into_any()
                    } else {
                        said(&c, edits.map(|_| t.with_untracked(|t| changed_the_doc(ctx, t))))
                    };
                    view! { <div class=if arrives { "c6-arrive" } else { "" }>{body}</div> }
                } />
                {changed_after}
            </div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <Show when=move || !archived.get()>
            <Show when=move || !failed.get().is_empty()>
            <div class="max-h-40 shrink-0 space-y-2 overflow-y-auto">
            <For each=move || failed.get().into_iter().enumerate() key=|(i, c)| (*i, c.text.clone()) children=move |(i, c)| {
                let c = StoredValue::new(c);
                view! {
                    <div data-failed-reply="" class="space-y-1 rounded-md border border-zinc-300 p-2 dark:border-zinc-700">
                        <p class="text-xs text-muted">"Not sent. Kept here while you write your next reply."</p>
                        <pre class="max-h-24 overflow-y-auto whitespace-pre-wrap break-words font-sans text-sm">{c.with_value(|c| c.text.clone())}</pre>
                        <button class="text-xs text-sky-700 hover:underline disabled:opacity-50 dark:text-sky-400"
                            disabled=move || !reply.get().is_empty() title="Restore into an empty reply box"
                            on:click=move |_| {
                                if !reply.get_untracked().is_empty() { return; }
                                reply.set(c.with_value(|c| c.text.clone()));
                                to_agent.set(c.with_value(|c| c.to == "agent"));
                                let mut replies = failed_replies(&failed_key.get_value());
                                if i < replies.len() { replies.remove(i); }
                                save_failed_replies(&failed_key.get_value(), &replies);
                                failed.set(replies);
                            }>"Restore reply"</button>
                    </div>
                }
            } />
            </div>
            </Show>
            <div class="flex shrink-0 items-end gap-2">
            <div class="relative min-w-0 flex-1">
                <textarea node_ref=area rows="1" class=format!("{INPUT} {} pr-9", if about { COMMENT_GROWS } else { "grows block max-h-24 resize-none overflow-y-auto" }) placeholder="Reply…" prop:value=move || reply.get() on:input=move |ev| reply.set(event_target_value(&ev))
                    on:keydown=move |ev| if crate::util::sends(&ev) { ev.prevent_default(); send() }></textarea>
                // Send, as the message box's: a soft square, sky once there's text.
                <button class=move || format!("absolute bottom-1.5 right-1.5 flex h-[22px] w-[22px] items-center justify-center rounded-md {}", if reply.with(|r| r.trim().is_empty()) { "bg-zinc-600 text-zinc-200" } else { "bg-sky-600 text-white" })
                    title="Send the reply (Enter)" aria-label="Send the reply" on:click=move |_| send()>
                    {crate::session::send_icon()}
                </button>
            </div>
                {move || open.get().then(|| view! {
                    <button class="min-h-9 shrink-0 text-xs text-muted hover:underline" on:click=move |_| if !archived.get_untracked() { spawn_local(async move { done(api::doc_thread_resolve(doc.get_value(), tid.get_value()).await) }) }>"Resolve"</button>
                })}
            </div>
            <div class="flex shrink-0 flex-wrap items-center gap-2 text-xs text-muted">
                <div class="min-w-0 flex-1 basis-40 [&>button]:min-h-7 [&>button]:text-xs pointer-coarse:[&>button]:min-h-11"><crate::components::Switch on=move || to_agent.get() on_change=move |v| to_agent.set(v) label="Send to agent" title="Send the thread to an agent" /></div>
                {move || t.with(|t| t.unsent).then(|| view! {
                    <button class="shrink-0 text-sky-700 hover:underline dark:text-sky-400" title="Your last comment here hasn't gone to an agent: send the thread now" on:click=send_unsent>"Send now"</button>
                })}
            </div>
            </Show>
        </div>
    }
}

fn failed_replies(key: &str) -> Vec<c6_api::docs::CommentNew> {
    crate::util::storage_get(key).and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn save_failed_replies(key: &str, replies: &[c6_api::docs::CommentNew]) {
    if let Ok(text) = serde_json::to_string(replies) {
        crate::util::storage_set(key, &text);
    }
}

thread_local! {
    /// Each document's past exports and Google file as last loaded (see
    /// [`Exports`]).
    static EXPORTS: std::cell::RefCell<std::collections::HashMap<String, Vec<c6_api::docs::DocExport>>> = Default::default();
    static GOOGLE: std::cell::RefCell<std::collections::HashMap<String, c6_api::docs::DocGoogle>> = Default::default();
}

/// Export: the formats a document goes to, and the files made before
/// (each with the version it was made from), to download again.
#[component]
fn Exports(id: String, kind: String) -> impl IntoView {
    let archived = read_only();
    let id = StoredValue::new(id);
    let tick = RwSignal::new(0u32);
    // The page draws this box again whenever the document changes (an agent
    // editing a deck): with what it loaded last, so it doesn't empty and
    // refill each time (a flicker), while the list loads again.
    let past = RwSignal::new(EXPORTS.with(|c| c.borrow().get(&id.get_value()).cloned()));
    Effect::new(move || {
        tick.track();
        let doc = id.get_value();
        spawn_local(async move {
            if let Ok(list) = api::doc_exports(doc.clone()).await {
                EXPORTS.with(|c| c.borrow_mut().insert(doc, list.clone()));
                if past.try_get_untracked().is_some_and(|cur| cur.as_ref() != Some(&list)) {
                    let _ = past.try_set(Some(list));
                }
            }
        });
    });
    let busy = RwSignal::new(None::<&'static str>);
    let error = RwSignal::new(None::<String>);
    let formats: &'static [(&'static str, &'static str)] = if kind == "deck" {
        &[("pdf", "PDF"), ("pptx", "PowerPoint"), ("odp", "OpenDocument"), ("png", "Pictures"), ("html", "HTML"), ("md", "Markdown")]
    } else {
        &[("pdf", "PDF"), ("docx", "Word"), ("odt", "OpenDocument"), ("html", "HTML"), ("md", "Markdown")]
    };
    let go = move |f: &'static str| {
        if archived.get_untracked() {
            return;
        }
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
            <Show when=move || archived.get()><p class="text-xs text-muted">"Unarchive to make a new export. Previous exports can still be downloaded below."</p></Show>
            <Show when=move || !archived.get()>
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
            </Show>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <ToGoogle id=id.get_value() />
            {move || past.get().filter(|p| !p.is_empty()).map(|p| view! {
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
    let archived = read_only();
    let id = StoredValue::new(id);
    let tick = RwSignal::new(0u32);
    // As in the box around it: what it loaded last, then fresh.
    let google = RwSignal::new(GOOGLE.with(|c| c.borrow().get(&id.get_value()).cloned()));
    Effect::new(move || {
        tick.track();
        let doc = id.get_value();
        spawn_local(async move {
            if let Ok(g) = api::doc_google(doc.clone()).await {
                GOOGLE.with(|c| c.borrow_mut().insert(doc, g.clone()));
                if google.try_get_untracked().is_some_and(|cur| cur.as_ref() != Some(&g)) {
                    let _ = google.try_set(Some(g));
                }
            }
        });
    });
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let changed = RwSignal::new(false);
    let go = move |new: bool, anyway: bool| {
        if archived.get_untracked() {
            return;
        }
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
        if archived.get_untracked() {
            return;
        }
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
        {move || google.get().map(|g| {
            if archived.get() {
                return view! { <GoogleReadOnly state=g /> }.into_any();
            }
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
            }.into_any()
        })}
    }
}

/// Archiving never removes or updates the remembered Google file, even
/// when its account has since been disconnected from carbon6.
#[component]
fn GoogleReadOnly(state: c6_api::docs::DocGoogle) -> impl IntoView {
    state.file.map(|file| view! {
        <div class="space-y-1 border-t border-zinc-200 pt-2 dark:border-zinc-800">
            <div class="text-xs text-muted">{state.app.clone()}</div>
            <div class="flex items-center gap-2">
                <a href=file.url target="_blank" rel="noopener" class="min-w-0 flex-1 truncate text-sky-700 hover:underline dark:text-sky-400">
                    {format!("Open in {}", state.app)}
                </a>
                <span class="shrink-0 text-xs text-muted">{format!("v{} · {}", file.rev, file.account)}</span>
            </div>
        </div>
    })
}

/// History: changes by author run, each version openable and restorable.
#[component]
fn History(id: String, reload: Callback<()>) -> impl IntoView {
    let author_named = author_names();
    let archived = read_only();
    let id = StoredValue::new(id);
    let versions = LocalResource::new(move || api::doc_history(id.get_value()));
    let open = RwSignal::new(None::<i64>);
    let error = RwSignal::new(None::<String>);
    let restore = move |rev: i64, block: Option<String>| {
        if archived.get_untracked() {
            return;
        }
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
                                        <Show when=move || !archived.get()><button class="text-xs text-sky-700 hover:underline dark:text-sky-400" title="Bring this version back as a new change"
                                            on:click=move |_| restore(rev, None)>"Restore"</button>
                                        </Show>
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
    let archived = read_only();
    let id = StoredValue::new(id);
    let old = crate::resources::local(move || async move { api::doc_read(&id.get_value(), &format!("view=full&at={rev}")).await });
    view! {
        <div class="space-y-1 rounded border border-amber-200 bg-amber-50/50 p-2 dark:border-amber-900 dark:bg-amber-950/20">
            <div class="text-xs font-medium text-amber-800 dark:text-amber-300">{format!("As it was at version {rev}")}</div>
            {move || old.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) => {
                    let d: Option<Doc> = serde_json::from_value(v).ok();
                    view! { <div>{d.map(|d| d.tabs.into_iter().flat_map(|t| t.blocks).map(|b| {
                        let bid = StoredValue::new(b.id.clone());
                        view! {
                            <div class="group flex items-start gap-2">
                                <div class="min-w-0 flex-1"><Shown b=b kind=String::new() /></div>
                                <Show when=move || !archived.get()><button class="invisible shrink-0 text-xs text-sky-700 group-hover:visible dark:text-sky-400" title="Bring back just this block"
                                    on:click=move |_| restore.run((rev, Some(bid.get_value())))>"Restore block"</button>
                                </Show>
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
    fn only_session_creators_have_an_owning_session_link() {
        assert_eq!(super::owning_session("session:creator"), Some("creator".into()));
        for by in ["you", "assistant", "token:session:creator", "session:", ""] {
            assert_eq!(super::owning_session(by), None, "{by}");
        }
    }

    #[test]
    fn session_authors_use_the_captured_feed_during_delayed_rendering() {
        use super::*;
        let owner = Owner::new();
        let named = owner.with(|| {
            let session = serde_json::from_value(serde_json::json!({
                "id": "s1", "worker_id": "00000000-0000-0000-0000-000000000001", "title": "Archive regression",
                "cwd": "", "connection_id": "", "model": "", "status": "idle", "created_at": 0, "updated_at": 0, "last_seq": 0
            }))
            .unwrap();
            let (workers, _) = signal(Vec::new());
            let (sessions, _) = signal(vec![session]);
            let (connected, _) = signal(false);
            let (doc, _) = signal(None);
            let (marks, _) = signal(Default::default());
            let (attention, _) = signal(0);
            let resolved_sessions = RwSignal::new(Default::default());
            provide_context(api::LiveFeed { workers, sessions, resolved_sessions, connected, doc, marks, attention });
            author_names()
        });
        // The list's async render can run under a different root, with no
        // LiveFeed context. Its captured formatter must still name sessions.
        Owner::new_root(None).with(|| {
            assert!(use_context::<api::LiveFeed>().is_none());
            assert_eq!(named("session:s1"), "session Archive regression");
            assert_eq!(named("session:missing"), "a session");
            assert_eq!(named("you"), "you");
        });
    }

    #[test]
    fn archive_list_search_preserves_filters_and_includes_archived_only_when_on() {
        use super::*;
        let query = |archived| list_query("deck".into(), "The plan".into(), "design".into(), "you".into(), archived);
        assert!(!query(false).archived);
        let q = query(true);
        assert!(q.archived);
        assert_eq!(q.kind.as_deref(), Some("deck"));
        assert_eq!(q.q.as_deref(), Some("The plan"));
        assert_eq!(q.label.as_deref(), Some("design"));
        assert_eq!(q.by.as_deref(), Some("you"));
        assert_eq!(list_query(String::new(), "  ".into(), String::new(), String::new(), false).q, None);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn archive_switch_list_banner_and_menu_are_labelled_and_use_drawn_icons() {
        use super::*;
        Owner::new().with(|| {
            let checked = RwSignal::new(false);
            let html = view! { <ArchiveSwitch checked /> }.to_html();
            assert!(html.contains("role=\"switch\"") && html.contains("aria-checked=\"false\""), "{html}");
            assert!(html.find(">Show archived").unwrap() < html.find("h-4 w-7").unwrap(), "label must stay left: {html}");
            assert!(
                html.matches("<button").count() == 1 && html.contains("w-full") && html.contains("pointer-coarse:min-h-11"),
                "the entire row must be clickable, with a 44px touch target: {html}"
            );
            checked.set(true);
            let html = view! { <ArchiveSwitch checked /> }.to_html();
            assert!(html.contains("aria-checked=\"true\""), "{html}");

            for archived in [false, true] {
                let summary = DocSummary {
                    title: "The readable plan".into(),
                    kind: "doc".into(),
                    archived_at: archived.then_some(42),
                    ..Default::default()
                };
                let html = view! { <DocListTitle summary /> }.to_html();
                assert!(html.contains("The readable plan") && html.contains("<svg"), "{html}");
                assert_eq!(html.contains("Archived"), archived, "{html}");
                let busy = RwSignal::new(false);
                let html = view! { <ArchiveMenuItem archived busy run=Callback::new(|()| {}) /> }.to_html();
                assert!(html.contains("role=\"menuitem\"") && html.contains("<svg"), "{html}");
                assert!(html.contains(if archived { "Unarchive" } else { "Archive" }), "{html}");
                crate::icons::tests::assert_no_icon_glyphs(&html);
            }
            let busy = RwSignal::new(false);
            let html = view! { <ArchivedBanner busy unarchive=Callback::new(|()| {}) /> }.to_html();
            assert!(html.contains("Archived. Read-only.") && html.contains("Unarchive") && html.contains("<svg"), "{html}");
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn archived_google_export_keeps_the_remembered_link_without_write_controls() {
        use super::*;
        Owner::new().with(|| {
            // A remembered file remains openable after disconnecting its account.
            let state = c6_api::docs::DocGoogle {
                app: "Google Docs".into(),
                accounts: vec![],
                file: Some(c6_api::docs::GoogleFile {
                    url: "https://docs.google.com/document/d/remembered".into(),
                    rev: 7,
                    ..Default::default()
                }),
            };
            let html = view! { <GoogleReadOnly state /> }.to_html();
            assert!(html.contains("https://docs.google.com/document/d/remembered") && html.contains("Open in Google Docs"), "{html}");
            assert!(
                !html.contains("<button")
                    && !html.contains("Connect Google")
                    && !html.contains("Update the Google file")
                    && !html.contains("Make a new one"),
                "{html}"
            );
            let html = view! { <GoogleReadOnly state=c6_api::docs::DocGoogle::default() /> }.to_html();
            assert!(!html.contains("<button") && !html.contains("<a "), "no remembered link means no controls: {html}");
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn archived_chips_disable_edits_but_keep_archived_refs_readable() {
        use super::*;
        use serde_json::json;
        Owner::new().with(|| {
            let attrs = json!({"chips": {
                "date": {"type": "date", "value": "2026-11-06"},
                "choice": {"type": "dropdown", "value": "Ready"},
                "ref": {"type": "ref", "ref": "doc:d1", "title": "The plan"}
            }});
            let refs = std::collections::HashMap::from([(
                "doc:d1".into(),
                c6_api::work::LiveAttached {
                    reference: "doc:d1".into(),
                    title: "The archived plan".into(),
                    state: "archived".into(),
                    ..Default::default()
                },
            )]);
            let text = "<?c6 block date?> <?c6 block choice?> <?c6 block ref?>";
            let html = chips_html(text, &attrs, &refs, true);
            assert_eq!(html.matches(" disabled ").count(), 2, "only date/dropdown edits are disabled: {html}");
            assert!(html.contains("data-ref=\"doc:d1\"") && html.contains("The archived plan") && html.contains("Archived"), "{html}");
            assert!(!chips_html(text, &attrs, &refs, false).contains(" disabled "));
        });
    }

    #[test]
    fn archived_autosave_does_not_send_or_consume_a_draft() {
        use super::*;
        Owner::new().with(|| {
            let archived = Memo::new(|_| true);
            let edit = RwSignal::new(Some(Editing {
                id: "b1".into(),
                rev: 1,
                hash: "hash".into(),
                original: "Before".into(),
                chips: Map::new(),
                block: DocBlock::default(),
            }));
            let draft = Draft { text: RwSignal::new("After".into()), typed: RwSignal::new(1) };
            save("d1".into(), edit, draft, false, RwSignal::new(None), archived, || panic!("must not reload or send"));
            assert!(edit.get_untracked().is_some());
            assert_eq!(draft.text.get_untracked(), "After");
        });
    }

    #[test]
    fn comments_list_captures_flip_before_keyed_updates_only_when_ids_change() {
        let source = include_str!("docs.rs");
        let panel = source.split("fn CommentsPanel()").nth(1).unwrap().split("fn BlockPop(").next().unwrap();
        let memo = panel.split("let ids = Memo::new").nth(1).unwrap().split("// A new comment").next().unwrap();
        assert!(memo.contains("previous: Option<&Vec<String>>"));
        assert!(memo.contains("previous.is_some_and(|previous| previous != &next)"), "text/reply updates must not animate the list");
        assert!(memo.find("crate::util::flip_list(list, &next)").unwrap() < memo.rfind("next").unwrap());
        assert!(panel.contains("<div node_ref=list data-comments-list=\"\" class=\"min-h-0 flex-1 space-y-2 overflow-y-auto"));
        assert!(panel.contains("<div data-motion-key=id.clone()><PanelThread id=id.clone() /></div>"));
    }

    #[test]
    fn phone_doc_actions_register_live_controls_and_keep_rename_outside_the_menu() {
        let source = include_str!("docs.rs");
        let page = source.split("pub fn DocPage(").nth(1).unwrap().split("fn send_all_button(").next().unwrap();
        let actions = page.split("<ViewActions>").nth(1).unwrap().split("</ViewActions>").next().unwrap();
        for action in [
            "Rename document",
            "comments_button(panel, open_n, true)",
            "whole_doc_comment_button(threads, true)",
            "send_all_button(threads, true, true)",
            "edit_doc_button(edit_all, toggle_edit, true)",
            "used.get()",
            "<DocMenuItems owner archived",
        ] {
            assert!(actions.contains(action), "the phone menu must expose {action}");
        }
        assert!(actions.contains("!archived.get() && unsent.get() > 0"));
        assert!(actions.contains("!archived.get() && !is_deck.get()"));
        assert!(!actions.contains("node_ref=phone_rename_ref"), "closing Actions must not remove the rename editor");
        assert!(page.contains("node_ref=phone_rename_ref"));
        assert!(page.contains("phone_renaming.set(false)"));
        assert!(page.contains("class=\"sticky top-0 z-20 hidden border-b"));
        assert!(page.contains("bg-white md:block"), "use the same height-aware breakpoint as workspace chrome");
        let menu = source.split("fn DocMenuItems(").nth(1).unwrap().split("/// A doc's comments").next().unwrap();
        for action in ["Open the session that made it", "Export…", "History", "<ArchiveMenuItem"] {
            assert!(menu.contains(action), "desktop and phone must share {action}");
        }
        assert!(menu.contains(") -> AnyView"));
        for action in [
            "comments_button(panel, open_n, false)",
            "whole_doc_comment_button(threads, false)",
            "send_all_button(threads, true, false)",
            "edit_doc_button(edit_all, toggle_edit, false)",
        ] {
            assert!(page.contains(action), "desktop must use the same constructor: {action}");
        }
        let controls = source.split("fn comments_button(").nth(1).unwrap().split("/// \"Used in N").next().unwrap();
        for behavior in [
            "role=phone.then_some(\"menuitem\")",
            "DOC_PHONE_ACTION",
            "data-opens=\"comments\"",
            "aria-pressed=move || phone.then(|| panel.get().to_string())",
            "panel.update(|p| *p = !*p)",
            "Hide the comments",
            "Show the comments",
            "bg-zinc-100 dark:bg-zinc-800",
            "bg-zinc-100 font-medium dark:bg-zinc-800",
            "n => format!(\"{n} open\")",
            "n => format!(\" ({n} open)\")",
            "ctx.panel.set(true)",
            "ctx.focus.set(Some(String::new()))",
            "ctx.commenting.set(Some(String::new()))",
            "Comment on the whole doc",
            "on:click=move |_| toggle_edit.run(())",
            "phone.then(crate::icons::pencil)",
            "view! { {crate::icons::pencil()}\" Edit\" }.into_any()",
            "disabled=move || ctx.sending.get()",
            "ctx.send_all.run(())",
            "(phone && arrow).then(crate::icons::arrow_up)",
            "(!phone && arrow).then(|| view! { \" \"{crate::icons::arrow_up()} })",
        ] {
            assert!(controls.contains(behavior), "shared controls must retain {behavior}");
        }
        assert!(controls.find("(phone && arrow)").unwrap() < controls.find("format!(\"Send {} to agent\"").unwrap());
        assert!(controls.find("format!(\"Send {} to agent\"").unwrap() < controls.find("(!phone && arrow)").unwrap());
    }

    #[test]
    fn desktop_document_rename_blur_only_commits_an_open_editor() {
        let source = include_str!("docs.rs");
        let page = source.split("pub fn DocPage(").nth(1).unwrap();
        let input = page.split("<input node_ref=rename_ref").nth(1).unwrap().split("/>").next().unwrap();
        assert!(input.contains("ev.key() == \"Enter\" { rename() }"));
        assert!(input.contains("ev.key() == \"Escape\" { renaming.set(false) }"));
        assert!(input.contains("on:blur=move |_| if renaming.get_untracked() { rename() }"));
        let rename = page.split("let rename = move || {").nth(1).unwrap().split("let add_block").next().unwrap();
        assert!(rename.find("renaming.set(false)").unwrap() < rename.find("spawn_local").unwrap());
    }

    #[test]
    fn docs_motion_retains_panels_and_only_new_comments_arrive() {
        let source = include_str!("docs.rs");
        let page = source.split("pub fn DocPage(").nth(1).unwrap().split("fn send_all_button(").next().unwrap();
        for present in ["docked_present", "floating_present", "sheet_present", "pop_sheet_present", "pop_float_present"] {
            assert!(page.contains(&format!("<Show when=move || {present}.get()>")), "heavy panels need retained presence: {present}");
        }
        assert!(page.contains("let retained_pop = Memo::new"), "closing must not discard the popover's block immediately");
        let thread = source.split("fn Thread(").nth(1).unwrap().split("thread_local!").next().unwrap();
        assert!(
            thread.contains("StoredValue::new(t.with_untracked(|t| t.comments.iter()"),
            "already visible comments must not arrive again"
        );
        assert!(thread.contains("!seen.with_value(|ids| ids.contains(&c.id))"), "edits/editability redraws are not new arrivals");
        assert!(thread.contains("if arrives { \"c6-arrive\" } else { \"\" }"));
        let flash = source.split("fn flash_blocks(").nth(1).unwrap().split("fn reveal_targets(").next().unwrap();
        assert!(flash.contains("crate::util::reduced_motion()"));
        assert!(flash.contains("web_sys::ScrollBehavior::Instant"), "Reduced/Off cannot inherit smooth scrolling");
    }

    #[test]
    fn thread_boxes_use_margins_then_anchor_free_strips_without_reflow() {
        use super::{Rect, thread_at};
        let area = Rect { left: 0.0, top: 0.0, width: 1800.0, height: 900.0 };
        let column = Rect { left: 500.0, top: 60.0, width: 768.0, height: 1800.0 };
        let block = Rect { left: 516.0, top: 100.0, width: 736.0, height: 120.0 };
        let intersects = |a: Rect, b: Rect| {
            a.left < b.left + b.width && b.left < a.left + a.width && a.top < b.top + b.height && b.top < a.top + a.height
        };
        let margin = thread_at(block, column, area, (350.0, 500.0), 160.0);
        assert_eq!(margin.width, 350.0);
        assert!(margin.left >= column.left + column.width + 8.0);
        assert!(!intersects(margin, block));
        let column = Rect { left: 800.0, ..column };
        let block = Rect { left: 816.0, ..block };
        let left = thread_at(block, column, area, (350.0, 500.0), 160.0);
        assert!(left.left + left.width <= column.left - 8.0);
        for room in [255.0, 256.0, 349.0, 350.0] {
            let area = Rect { width: 2.0 * room + 800.0, ..area };
            let column = Rect { left: room + 16.0, width: 768.0, ..column };
            let block = Rect { left: column.left, width: column.width, ..block };
            let placed = thread_at(block, column, area, (350.0, 500.0), 160.0);
            assert!(!intersects(placed, block));
            assert_eq!(placed.width, if room >= 256.0 { room.min(350.0) } else { 256.0 });
        }
        let area = Rect { width: 1000.0, ..area };
        let column = Rect { left: 116.0, ..column };
        for y in [100.0, 700.0] {
            let block = Rect { left: 132.0, top: y, ..block };
            let placed = thread_at(block, column, area, (350.0, 500.0), 160.0);
            assert_eq!(placed.width, 256.0);
            assert!(!intersects(placed, block), "try above/below before covering the block");
        }
        // Outside/partly visible/hidden tiles never produce inverted clamps,
        // NaNs or a width larger than the visible scrollport.
        for (width, height) in [(0.0, 0.0), (12.0, 12.0), (200.0, 400.0), (1000.0, 900.0)] {
            let area = Rect { left: 30.0, top: 40.0, width, height };
            for (x, y) in [(-1200.0, -500.0), (0.0, 0.0), (1000.0, 1000.0)] {
                let block = Rect { left: x, top: y, width: 900.0, height: 900.0 };
                let placed = thread_at(block, block, area, (350.0, 800.0), 160.0);
                assert!(placed.left.is_finite() && placed.top.is_finite());
                assert!(placed.width <= (width - 16.0).max(0.0) && placed.height <= (height - 16.0).max(0.0));
                assert!(placed.left >= area.left + 8.0 && placed.top >= area.top + 8.0);
                assert!(placed.left + placed.width <= (area.left + width - 8.0).max(area.left + 8.0));
                assert!(placed.top + placed.height <= (area.top + height - 8.0).max(area.top + 8.0));
            }
        }
    }

    #[test]
    fn panel_order_follows_tabs_and_nested_blocks_not_attention_or_api_order() {
        use super::*;
        let tabs = vec![
            DocTab {
                id: "one".into(),
                blocks: vec![DocBlock {
                    id: "a".into(),
                    children: vec![DocBlock { id: "child".into(), ..Default::default() }],
                    ..Default::default()
                }],
                ..Default::default()
            },
            DocTab { id: "two".into(), blocks: vec![DocBlock { id: "b".into(), ..Default::default() }], ..Default::default() },
        ];
        let thread = |id: &str, tab: &str, block: &str| DocThread {
            id: id.into(),
            state: "open".into(),
            anchor: c6_api::docs::Anchor { tab: tab.into(), block: block.into(), ..Default::default() },
            ..Default::default()
        };
        let mut threads = vec![
            thread("b", "two", "b"),
            thread("child", "one", "child"),
            thread("a1", "one", "a"),
            thread("a2", "one", "a"),
            thread("tab", "one", ""),
            thread("gone", "unknown", ""),
        ];
        threads[0].needs_you = true;
        assert_eq!(ordered_threads(&threads, &tabs, false), ["tab", "a1", "a2", "child", "b", "gone"]);
        threads[2].state = "resolved".into();
        assert_eq!(ordered_threads(&threads, &tabs, true), ["a1"]);
    }

    #[test]
    fn comment_popover_flips_clamps_and_shrinks_to_the_visible_tile() {
        use super::{Rect, popover_at};
        let area = Rect { left: 100.0, top: 60.0, width: 600.0, height: 400.0 };
        let mut anchor = Rect { left: 650.0, top: 100.0, width: 20.0, height: 20.0 };
        let below = popover_at(anchor, area, (384.0, 160.0));
        assert_eq!((below.left, below.top), (286.0, 124.0));
        anchor.top = 420.0;
        let above = popover_at(anchor, area, (384.0, 160.0));
        assert_eq!((above.left, above.top), (286.0, 256.0));
        // A long thread or a keyboard-shortened viewport: the box scrolls.
        for width in [320.0, 390.0, 1400.0] {
            let area = Rect { left: 30.0, top: 120.0, width, height: 220.0 };
            for (x, y) in [(-100.0, -100.0), (300.0, 200.0), (2000.0, 2000.0)] {
                let at = popover_at(Rect { left: x, top: y, ..anchor }, area, (384.0, 600.0));
                assert!(at.left >= area.left + 8.0 && at.top >= area.top + 8.0);
                assert!(at.left + at.width <= area.left + area.width - 8.0);
                assert!(at.top + at.height <= area.top + area.height - 8.0);
                assert_eq!(at.height, 204.0);
                assert_eq!(at.width, 384.0_f64.min(width - 16.0));
            }
        }
        // A hidden tile has no room, not an inverted clamp range or panic.
        let hidden = popover_at(anchor, Rect::default(), (384.0, 160.0));
        assert_eq!((hidden.width, hidden.height), (0.0, 0.0));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn agents_comments_render_their_markdown() {
        use super::*;
        let owner = Owner::new();
        owner.with(|| {
            let html = agent_text("1. **Work the target back.** Then *check*.").to_html();
            assert!(html.contains("<strong>Work the target back.</strong>") && !html.contains("**"), "{html}");
            assert!(html.contains("<ol>") || html.contains("<ol "), "{html}");
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn comment_quotes_are_labelled_buttons_and_gone_anchors_keep_an_inactive_icon() {
        use super::*;
        let owner = Owner::new();
        owner.with(|| {
            let tabs = RwSignal::new(vec![DocTab {
                blocks: vec![DocBlock { id: "block.1".into(), text: "The release plan".into(), ..Default::default() }],
                ..Default::default()
            }]);
            let thread = RwSignal::new(DocThread {
                anchor: c6_api::docs::Anchor { block: "block.1".into(), quote: "release plan".into(), ..Default::default() },
                about: "The release plan".into(),
                ..Default::default()
            });
            provide_context(ThreadsCtx {
                doc: StoredValue::new("doc.1".into()),
                shown: RwSignal::new(vec![]),
                tick: RwSignal::new(0),
                focus: RwSignal::new(None),
                commenting: RwSignal::new(None),
                panel: RwSignal::new(false),
                pop: RwSignal::new(None),
                selected: RwSignal::new(None),
                prefs: RwSignal::new(CommentPrefs::default()),
                edit_epochs: StoredValue::new(std::collections::HashMap::new()),
                pop_node: NodeRef::new(),
                tabs: Memo::new(move |_| tabs.get()),
                root: NodeRef::new(),
                reveal: Callback::new(|_| {}),
                unsent: Memo::new(|_| 0),
                sending: RwSignal::new(false),
                send_all: Callback::new(|_| {}),
            });
            let t = Memo::new(move |_| thread.get());
            for (deleted, rewritten) in [(false, false), (false, true), (true, false)] {
                thread.update(|t| t.detached = rewritten);
                if deleted {
                    tabs.set(vec![]);
                }
                let html = view! { <About t /> }.to_html();
                let buttons: Vec<_> = html.split("<button").skip(1).map(|b| b.split("</button>").next().unwrap()).collect();
                assert_eq!(buttons.len(), 2, "keep both the quote and locate button: {html}");
                for button in &buttons {
                    assert!(button.contains("type=\"button\""), "{button}");
                    assert!(button.contains("title=\"Show it in the doc\""), "{button}");
                    assert!(button.contains("aria-label=\"Show it in the doc\""), "{button}");
                }
                assert!(buttons[0].contains("The release plan"), "{html}");
                assert!(buttons[0].contains("“release plan”"), "both lines are in the button: {html}");
                let disabled = |button: &str| button.split('>').next().unwrap().split_whitespace().any(|a| a == "disabled");
                assert!(!disabled(buttons[0]), "gone quotes still explain themselves: {html}");
                assert!(buttons[1].contains("<svg"), "keep the locate icon: {html}");
                assert!(buttons[1].contains("r=\"3\""), "use the crosshair, not the opening arrow: {html}");
                assert_eq!(disabled(buttons[1]), deleted || rewritten, "{html}");
                assert_eq!(comment_target(&thread.get_untracked(), !deleted), (!deleted && !rewritten).then(|| "block.1".into()));
            }
        });
    }

    #[test]
    fn comment_reveal_finds_the_tab_and_slide_holding_a_block() {
        use super::*;
        let tabs = vec![DocTab {
            id: "deck-tab".into(),
            blocks: vec![
                DocBlock { id: "slide.1".into(), kind: "slide".into(), ..Default::default() },
                DocBlock {
                    id: "slide.2".into(),
                    kind: "slide".into(),
                    children: vec![DocBlock { id: "words.1".into(), ..Default::default() }],
                    ..Default::default()
                },
            ],
            ..Default::default()
        }];
        let expected = Some(("deck-tab".into(), vec!["slide.2".into()]));
        assert_eq!(reveal_targets(&tabs, &["words.1".into()]), expected);
        assert_eq!(reveal_targets(&tabs, &["slide.2".into()]), expected);
        assert_eq!(reveal_targets(&tabs, &["deleted.1".into()]), None);
        assert_eq!(reveal_targets(&tabs, &[]), None);
        assert_eq!(comment_target(&DocThread::default(), false), None, "whole-tab threads have no jump target");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn document_chips_and_file_embeds_keep_their_drawn_icons() {
        use super::*;
        use serde_json::json;
        let owner = Owner::new();
        owner.with(|| {
            for (kind, attrs, label, icon_first) in [
                ("date", json!({"type": "date", "value": "2026-11-05"}), "2026-11-05", true),
                ("dropdown", json!({"type": "dropdown", "value": "Ready"}), "Ready", false),
                ("ref", json!({"type": "ref", "ref": "work:w1", "title": "The plan"}), "The plan", false),
            ] {
                let html = chips_html("<?c6 block chip?>", &json!({"chips": {"chip": attrs}}), &Default::default(), false);
                let svg = html.find("<svg").unwrap_or_else(|| panic!("{kind} lost its icon: {html}"));
                let text = html.rfind(label).unwrap_or_else(|| panic!("{kind} lost its text: {html}"));
                assert_eq!(svg < text, icon_first, "{kind} moved its icon: {html}");
                crate::icons::tests::assert_no_icon_glyphs(&html);
            }
            let html = file_embed("/file".into(), "notes.txt".into(), Some(42)).to_html();
            assert!(html.contains("href=\"/file\""), "keep the download link: {html}");
            assert!(html.contains("<svg"), "file lost its paperclip: {html}");
            assert!(html.contains("notes.txt"), "{html}");
            crate::icons::tests::assert_no_icon_glyphs(&html);
        });
    }

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

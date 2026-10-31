//! Decks in the document panel (docs/docs.md §5, §12): a slide sorter
//! beside the slide being edited, its notes below; layouts and the theme
//! from menus; drag slides to reorder; Present full screen, with a
//! presenter view (notes, next slide, timer) in a second window. Slides are
//! drawn by carbon6 itself (`c6_api::slides`) with the deck's theme.

use c6_api::docs::{Doc, DocBlock, DocOp};
use c6_api::slides::{BASE_CSS, DeckTheme, scoped_theme, slide_html};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Map;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT};

/// Markdown for a slide's blocks (raw HTML shown as text; html slides are
/// sanitized by the slide renderer itself).
fn md(t: &str) -> String {
    crate::markdown::to_html(t)
}

/// The `<style>` for a deck: its theme's variables on its wrapper, and the
/// base slide CSS.
fn style(deck: &str, css: &str) -> String {
    format!("{}{BASE_CSS}", scoped_theme(css, &format!(".c6deck[data-deck=\"{deck}\"]")))
}

/// A slide drawn at `width` pixels (its 1280×720 scaled down).
#[component]
pub fn SlideBox(html: String, width: Signal<f64>) -> impl IntoView {
    view! {
        <div class="relative overflow-hidden rounded shadow-sm ring-1 ring-zinc-200 dark:ring-zinc-800"
            style=move || format!("width: {}px; height: {}px", width.get(), width.get() * 9.0 / 16.0)>
            <div style=move || format!("transform: scale({}); transform-origin: 0 0; width: 1280px; height: 720px", width.get() / 1280.0)
                inner_html=html></div>
        </div>
    }
}

/// The theme CSS for a deck, from the realm's themes.
fn theme_of(themes: &[DeckTheme], name: &str) -> String {
    themes.iter().find(|t| t.name.eq_ignore_ascii_case(name)).or(themes.first()).map(|t| t.css.clone()).unwrap_or_default()
}

fn slides(d: &Doc) -> Vec<DocBlock> {
    d.tabs.first().map(|t| t.blocks.clone()).unwrap_or_default()
}

fn replace_slide(id: &str, layout: Option<String>, notes: Option<String>, hidden: Option<bool>) -> DocOp {
    DocOp::Replace {
        id: id.to_owned(),
        markdown: None,
        blocks: Map::new(),
        find: None,
        with_text: None,
        nth: None,
        cell: None,
        text: None,
        layout,
        notes,
        hidden,
        if_rev: None,
        if_hash: None,
    }
}

/// A deck in the document panel.
#[component]
pub fn DeckView(
    doc: Doc,
    send: Callback<Vec<DocOp>>,
    error: RwSignal<Option<String>>,
    picked: RwSignal<Option<String>>,
    commenting: RwSignal<Option<String>>,
) -> impl IntoView {
    let id = StoredValue::new(doc.summary.id.clone());
    let themes = LocalResource::new(api::deck_themes);
    let all = slides(&doc);
    let current = RwSignal::new(0usize);
    // The slide you were on stays picked across reloads, by id (`picked`
    // belongs to the document panel).
    let index_of = {
        let ids: Vec<String> = all.iter().map(|s| s.id.clone()).collect();
        move |sid: &str| ids.iter().position(|x| x == sid)
    };
    if let Some(i) = picked.get_untracked().as_deref().and_then(index_of) {
        current.set(i);
    }
    let all = StoredValue::new(all);
    let title = StoredValue::new(doc.summary.title.clone());
    let theme = doc.summary.theme.clone();
    let rev = doc.summary.rev;
    let total = all.with_value(Vec::len);
    let html_of = move |i: usize| -> String {
        all.with_value(|a| a.get(i).map(|s| slide_html(s, i + 1, a.len(), &title.get_value(), &md)).unwrap_or_default())
    };
    let editing = RwSignal::new(None::<(String, String, String)>);
    let notes = RwSignal::new(String::new());
    let dragged = RwSignal::new(None::<usize>);
    let main_width = RwSignal::new(640.0);
    let main_ref = NodeRef::<leptos::html::Div>::new();
    // The slide fills the panel's width, following it as it changes.
    Effect::new(move |_| {
        use wasm_bindgen::JsCast;
        use wasm_bindgen::closure::Closure;
        let Some(el) = main_ref.get() else { return };
        let on = Closure::<dyn Fn(js_sys::Array)>::new(move |entries: js_sys::Array| {
            if let Some(e) = entries.get(0).dyn_ref::<web_sys::ResizeObserverEntry>() {
                let w = e.content_rect().width();
                if w > 100.0 {
                    let _ = main_width.try_set(w.min(1280.0));
                }
            }
        });
        if let Ok(ro) = web_sys::ResizeObserver::new(on.as_ref().unchecked_ref()) {
            ro.observe(&el);
            on.forget();
            let ro = send_wrapper::SendWrapper::new(ro);
            on_cleanup(move || ro.disconnect());
        }
    });
    let select = move |i: usize| {
        current.set(i);
        picked.set(all.with_value(|a| a.get(i).map(|s| s.id.clone())));
        notes.set(all.with_value(|a| a.get(i).and_then(|s| s.attrs["notes"].as_str().map(str::to_owned)).unwrap_or_default()));
        editing.set(None);
    };
    select(current.get_untracked().min(total.saturating_sub(1)));
    let sel = move || all.with_value(|a| a.get(current.get()).cloned());
    let edit_slide = move || {
        let Some(s) = sel() else { return };
        spawn_local(async move {
            match api::doc_read(&id.get_value(), &format!("view=block&id={}", api::enc(&s.id))).await {
                Ok(v) => editing.set(Some((s.id.clone(), s.hash.clone(), v["markdown"].as_str().unwrap_or("").to_owned()))),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let save_slide = move || {
        let Some((sid, hash, markdown)) = editing.get_untracked() else { return };
        editing.set(None);
        let op = DocOp::Replace {
            id: sid,
            markdown: Some(markdown),
            blocks: Map::new(),
            find: None,
            with_text: None,
            nth: None,
            cell: None,
            text: None,
            layout: None,
            notes: None,
            hidden: None,
            if_rev: Some(rev),
            if_hash: Some(hash),
        };
        send.run(vec![op]);
    };
    let add_after = move || {
        let after = sel().map(|s| s.id);
        let op = DocOp::Insert {
            markdown: "<!-- layout: body -->\n## A new slide\n\n- A point".into(),
            blocks: Map::new(),
            before: None,
            after,
            section: None,
            slide: None,
            region: None,
            tab: None,
        };
        send.run(vec![op]);
        current.update(|c| *c += 1);
        picked.set(None);
    };
    let present = move |presenter: bool| {
        let href = crate::place::href(&format!("/docs/{}/present{}", id.get_value(), if presenter { "?presenter=1" } else { "" }));
        let _ = window().open_with_url_and_target(&href, if presenter { "_c6presenter" } else { "_c6present" });
    };
    let theme2 = theme.clone();
    view! {
        <div class="space-y-3" data-deck-view=doc.summary.id.clone()>
            {move || themes.get().and_then(Result::ok).map(|t| view! { <style>{style(&id.get_value(), &theme_of(&t, &theme2))}</style> })}
            <div class="flex flex-wrap items-center gap-2 text-sm">
                <span class="text-zinc-500">"Theme"</span>
                <select class=format!("{INPUT} w-auto!") on:change=move |ev| {
                    let t = event_target_value(&ev);
                    send.run(vec![DocOp::Set { title: None, theme: Some(t), size: None, tab: None, tab_title: None, tabs: None, block: None, chip: None, value: None }]);
                }>
                    {let theme = theme.clone(); move || themes.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|t| {
                        let on = t.name.eq_ignore_ascii_case(&theme);
                        view! { <option value=t.name.clone() selected=on title=t.mood.clone()>{t.name.clone()}</option> }
                    }).collect_view()}
                </select>
                <span class="text-zinc-400">{format!("{total} slides")}</span>
                <span class="flex-1"></span>
                <button class=BUTTON on:click=move |_| present(false)>"▶ Present"</button>
                <button class=BUTTON_QUIET title="Notes, the next slide and a timer, in a window of its own" on:click=move |_| { present(true); present(false); }>"Presenter view"</button>
            </div>
            <div class="flex gap-3">
                // The sorter: drag a slide onto another to move it there.
                <div class="flex max-h-[70vh] w-48 shrink-0 flex-col gap-2 overflow-y-auto pr-1">
                    {(0..total).map(|i| {
                        let s = all.with_value(|a| a[i].clone());
                        let hidden = s.attrs["hidden"] == true;
                        let html = html_of(i);
                        view! {
                            <div class="cursor-pointer" draggable="true" data-slide=s.id.clone()
                                on:click=move |_| select(i)
                                on:dragstart=move |_| dragged.set(Some(i))
                                on:dragover=move |ev| ev.prevent_default()
                                on:drop=move |ev| {
                                    ev.prevent_default();
                                    let Some(from) = dragged.get_untracked() else { return };
                                    dragged.set(None);
                                    if from == i { return; }
                                    let (moving, target) = all.with_value(|a| (a[from].id.clone(), a[i].id.clone()));
                                    let op = if from < i {
                                        DocOp::Move { id: moving, before: None, after: Some(target), tab: None }
                                    } else {
                                        DocOp::Move { id: moving, before: Some(target), after: None, tab: None }
                                    };
                                    send.run(vec![op]);
                                }>
                                <div class=move || format!("rounded p-0.5 {}", if current.get() == i { "ring-2 ring-violet-500" } else { "" })
                                    class:opacity-40=hidden>
                                    <div class="c6deck" data-deck=id.get_value()>
                                        <SlideBox html width=Signal::derive(|| 176.0) />
                                    </div>
                                </div>
                                <div class="text-[11px] text-zinc-400">{format!("{}{}", i + 1, if hidden { " · hidden" } else { "" })}</div>
                            </div>
                        }
                    }).collect_view()}
                    <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| add_after()>"+ Slide"</button>
                </div>
                // The slide being edited.
                <div class="min-w-0 flex-1 space-y-2" node_ref=main_ref>
                    {move || sel().map(|s| {
                        let i = current.get();
                        let html = html_of(i);
                        let layout = s.attrs["layout"].as_str().unwrap_or("body").to_owned();
                        let hidden = s.attrs["hidden"] == true;
                        let (sid, sid2, sid3, sid4) = (s.id.clone(), s.id.clone(), s.id.clone(), s.id.clone());
                        let hash = s.hash.clone();
                        let pending = s.kind == "pending";
                        view! {
                            <div class="c6deck" data-deck=id.get_value() on:dblclick=move |_| if !pending { edit_slide() }>
                                <SlideBox html width=Signal::derive(move || main_width.get()) />
                            </div>
                            <div class="flex flex-wrap items-center gap-2 text-sm">
                                <span class="text-zinc-500">{format!("Slide {} of {total}", i + 1)}</span>
                                {let c = s.id.clone(); view! { <button class=BUTTON_QUIET title="Comment on this slide" on:click=move |_| commenting.set(Some(c.clone()))>"💬"</button> }}
                                {(!pending).then(|| view! {
                                    <select class=format!("{INPUT} w-auto!") title="Its layout" on:change=move |ev| {
                                        send.run(vec![replace_slide(&sid, Some(event_target_value(&ev)), None, None)]);
                                    }>
                                        {c6_api::docs::LAYOUTS.iter().map(|(l, _)| view! { <option value=*l selected=layout == *l>{*l}</option> }).collect_view()}
                                    </select>
                                    <label class="flex items-center gap-1 text-zinc-500">
                                        <input type="checkbox" prop:checked=hidden on:change=move |ev| send.run(vec![replace_slide(&sid2, None, None, Some(event_target_checked(&ev)))]) />
                                        "Hidden"
                                    </label>
                                    <button class=BUTTON_QUIET on:click=move |_| edit_slide()>"Edit slide"</button>
                                })}
                                {pending.then(|| view! { <span class="text-violet-700 dark:text-violet-300">{format!("✎ being written by {}", crate::docs::author(s.attrs["by"].as_str().unwrap_or("")))}</span> })}
                                <span class="flex-1"></span>
                                <button class="text-xs text-zinc-400 hover:text-red-600" title="Delete this slide"
                                    on:click=move |_| send.run(vec![DocOp::Delete { id: sid3.clone(), if_rev: Some(rev), if_hash: Some(hash.clone()) }])>"Delete slide"</button>
                            </div>
                            {move || editing.get().map(|(_, _, text)| view! {
                                <div class="space-y-1">
                                    <textarea class=format!("{INPUT} min-h-[12rem] font-mono text-sm") prop:value=text
                                        on:input=move |ev| editing.update(|e| if let Some(e) = e { e.2 = event_target_value(&ev) }) />
                                    <div class="flex justify-end gap-2">
                                        <button class=BUTTON_QUIET on:click=move |_| editing.set(None)>"Cancel"</button>
                                        <button class=BUTTON on:click=move |_| save_slide()>"Save slide"</button>
                                    </div>
                                </div>
                            })}
                            {(!pending).then(|| view! {
                                <textarea class=format!("{INPUT} min-h-[4rem] text-sm") placeholder="Speaker notes"
                                    prop:value=move || notes.get() on:input=move |ev| notes.set(event_target_value(&ev))
                                    on:blur=move |_| {
                                        let n = notes.get_untracked();
                                        let had = all.with_value(|a| a.iter().find(|x| x.id == sid4).and_then(|x| x.attrs["notes"].as_str().map(str::to_owned)).unwrap_or_default());
                                        if n != had { send.run(vec![replace_slide(&sid4, None, Some(n), None)]); }
                                    } />
                            })}
                        }
                    })}
                    {(total == 0).then(|| view! { <p class="text-sm text-zinc-500">"No slides yet: + Slide, or ask a session to make the deck."</p> })}
                </div>
            </div>
        </div>
    }
    .into_any()
}

/// A deck full screen (`presenter`: its notes, the next slide and a timer).
/// The windows follow each other over a BroadcastChannel; arrow keys,
/// space and clicks move on.
#[component]
pub fn Present(id: String, presenter: bool) -> impl IntoView {
    let id = StoredValue::new(id);
    let doc = LocalResource::new(move || api::doc_get(id.get_value()));
    let themes = LocalResource::new(api::deck_themes);
    let at = RwSignal::new(0usize);
    let started = crate::util::now_ms();
    let now = crate::util::clock();
    let channel = web_sys::BroadcastChannel::new(&format!("c6-present-{}", id.get_value())).ok();
    let channel = StoredValue::new_local(channel);
    {
        use wasm_bindgen::JsCast;
        use wasm_bindgen::closure::Closure;
        let on = Closure::<dyn Fn(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
            if let Some(n) = e.data().as_f64() {
                at.set(n as usize);
            }
        });
        channel.with_value(|c| {
            if let Some(c) = c {
                c.set_onmessage(Some(on.as_ref().unchecked_ref()));
            }
        });
        on.forget();
    }
    let shown = move || -> Vec<DocBlock> {
        doc.get().and_then(Result::ok).map(|d| slides(&d).into_iter().filter(|s| s.attrs["hidden"] != true).collect()).unwrap_or_default()
    };
    let go = move |n: isize| {
        let len = shown().len();
        if len == 0 {
            return;
        }
        let next = (at.get_untracked() as isize + n).clamp(0, len as isize - 1) as usize;
        at.set(next);
        channel.with_value(|c| {
            if let Some(c) = c {
                let _ = c.post_message(&wasm_bindgen::JsValue::from_f64(next as f64));
            }
        });
    };
    let keys = window_event_listener(leptos::ev::keydown, move |ev| match ev.key().as_str() {
        "ArrowRight" | "ArrowDown" | " " | "PageDown" | "Enter" => go(1),
        "ArrowLeft" | "ArrowUp" | "PageUp" | "Backspace" => go(-1),
        "Home" => go(-10_000),
        "End" => go(10_000),
        _ => {}
    });
    on_cleanup(move || keys.remove());
    let width = RwSignal::new(1280.0);
    let fit = move || {
        let w = window().inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1280.0);
        let h = window().inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(720.0);
        width.set(if presenter { (w * 0.62).min(h * 0.6 * 16.0 / 9.0) } else { w.min(h * 16.0 / 9.0) });
    };
    fit();
    let resize = window_event_listener(leptos::ev::resize, move |_| fit());
    on_cleanup(move || resize.remove());
    view! {
        <div class=if presenter { "min-h-screen bg-zinc-900 p-6 text-zinc-100" } else { "flex min-h-screen items-center justify-center bg-black" }
            on:click=move |_| if !presenter { go(1) }>
            {move || match (doc.get(), themes.get()) {
                (Some(Err(e)), _) => view! { <div class="p-8"><ErrorBox message=e /></div> }.into_any(),
                (Some(Ok(d)), Some(Ok(t))) => {
                    let list = shown();
                    let i = at.get().min(list.len().saturating_sub(1));
                    let html = |k: usize| list.get(k).map(|s| slide_html(s, k + 1, list.len(), &d.summary.title, &md)).unwrap_or_default();
                    let css = style(&d.summary.id, &theme_of(&t, &d.summary.theme));
                    let notes = list.get(i).and_then(|s| s.attrs["notes"].as_str().map(str::to_owned)).unwrap_or_default();
                    let current = html(i);
                    let next = html(i + 1);
                    if presenter {
                        view! {
                            <style>{css}</style>
                            <div class="flex gap-6">
                                <div class="c6deck space-y-2" data-deck=d.summary.id.clone()>
                                    <SlideBox html=current width=Signal::derive(move || width.get()) />
                                    <div class="flex items-center gap-3 text-sm text-zinc-400">
                                        <button class="rounded bg-zinc-800 px-3 py-1" on:click=move |_| go(-1)>"◀"</button>
                                        <button class="rounded bg-zinc-800 px-3 py-1" on:click=move |_| go(1)>"▶"</button>
                                        <span>{format!("{} / {}", i + 1, list.len())}</span>
                                        <span class="font-mono">{move || { let s = (now.get() - started).max(0) / 1000; format!("{:02}:{:02}", s / 60, s % 60) }}</span>
                                    </div>
                                </div>
                                <div class="min-w-0 flex-1 space-y-3">
                                    <div class="text-xs uppercase tracking-wide text-zinc-500">"Next"</div>
                                    <div class="c6deck" data-deck=d.summary.id.clone()>
                                        {(!next.is_empty()).then(|| view! { <SlideBox html=next.clone() width=Signal::derive(|| 320.0) /> })}
                                        {next.is_empty().then(|| view! { <p class="text-sm text-zinc-500">"The end."</p> })}
                                    </div>
                                    <div class="text-xs uppercase tracking-wide text-zinc-500">"Notes"</div>
                                    <p class="whitespace-pre-wrap text-lg leading-relaxed">{if notes.is_empty() { "(no notes)".to_owned() } else { notes }}</p>
                                </div>
                            </div>
                        }.into_any()
                    } else {
                        view! {
                            <style>{css}</style>
                            <div class="c6deck" data-deck=d.summary.id.clone()>
                                <SlideBox html=current width=Signal::derive(move || width.get()) />
                            </div>
                        }.into_any()
                    }
                }
                _ => view! { <p class="p-8 text-zinc-500">"Loading…"</p> }.into_any(),
            }}
        </div>
    }
}

/// Settings → Decks: the realm's themes, each editable.
#[component]
pub fn ThemesSetting() -> impl IntoView {
    let loaded = LocalResource::new(move || {
        api::realm().track();
        api::deck_themes()
    });
    let list = RwSignal::new(Vec::<DeckTheme>::new());
    Effect::new(move || {
        if let Some(Ok(t)) = loaded.get() {
            list.set(t);
        }
    });
    let error = RwSignal::new(None::<String>);
    let saved = RwSignal::new(false);
    let save = move || {
        let l = list.get_untracked();
        spawn_local(async move {
            match api::set_deck_themes(&l).await {
                Ok(()) => {
                    error.set(None);
                    saved.set(true);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <h2 class="font-medium">"Deck themes"</h2>
            <p class="text-sm text-zinc-500">"A deck names its theme; changing a theme restyles every slide of the decks that use it. A theme is CSS custom properties on :root: --bg, --fg, --muted, --accent, --accent-bg, --accent-fg, --surface, --font-display, --font-body, --display-weight. How sessions make decks is in the prompts decks.flow, decks.storyline, decks.design, decks.visuals and decks.review."</p>
            {move || list.get().into_iter().enumerate().map(|(i, t)| {
                let sample = DocBlock {
                    id: "sample".into(),
                    kind: "slide".into(),
                    attrs: serde_json::json!({"layout": "title"}),
                    children: vec![
                        DocBlock { id: "a".into(), kind: "heading".into(), text: format!("# {}", t.name), region: "title".into(), ..Default::default() },
                        DocBlock { id: "b".into(), kind: "paragraph".into(), text: t.mood.clone(), region: "subtitle".into(), ..Default::default() },
                    ],
                    ..Default::default()
                };
                let key = format!("theme-{i}");
                let html = slide_html(&sample, 1, 1, "", &md);
                view! {
                    <div class="flex gap-3 border-t border-zinc-100 pt-3 dark:border-zinc-900">
                        <style>{style(&key, &t.css)}</style>
                        <div class="c6deck shrink-0" data-deck=key><SlideBox html width=Signal::derive(|| 240.0) /></div>
                        <div class="min-w-0 flex-1 space-y-1">
                            <input class=INPUT prop:value=t.name.clone() on:change=move |ev| list.update(|l| l[i].name = event_target_value(&ev)) />
                            <input class=INPUT prop:value=t.mood.clone() placeholder="What it's for" on:change=move |ev| list.update(|l| l[i].mood = event_target_value(&ev)) />
                            <textarea class=format!("{INPUT} min-h-[5rem] font-mono text-xs") prop:value=t.css.clone()
                                on:change=move |ev| list.update(|l| l[i].css = event_target_value(&ev)) />
                            <button class="text-xs text-zinc-400 hover:text-red-600" on:click=move |_| list.update(|l| { l.remove(i); })>"Remove"</button>
                        </div>
                    </div>
                }
            }).collect_view()}
            <div class="flex gap-2">
                <button class=BUTTON_QUIET on:click=move |_| list.update(|l| {
                    let base = l.first().cloned().unwrap_or_default();
                    l.push(DeckTheme { name: format!("Theme {}", l.len() + 1), mood: String::new(), css: base.css });
                })>"+ Theme"</button>
                <button class=BUTTON on:click=move |_| save()>"Save themes"</button>
                {move || saved.get().then(|| view! { <span class="self-center text-sm text-emerald-700">"Saved"</span> })}
            </div>
            {move || error.get().map(|e| view! { <p class="text-sm text-red-600">{e}</p> })}
        </section>
    }
}

/// Settings → Decks, while the decks app is installed: its decks come in
/// once as carbon6 decks, its themes as themes (docs/docs.md §13).
#[component]
pub fn BringIn() -> impl IntoView {
    let tick = RwSignal::new(0u32);
    let status = LocalResource::new(move || {
        tick.track();
        api::realm().track();
        api::decks_bring_status()
    });
    let busy = RwSignal::new(false);
    let result = RwSignal::new(None::<Result<serde_json::Value, String>>);
    let go = move |_| {
        busy.set(true);
        spawn_local(async move {
            result.set(Some(api::decks_bring_in().await));
            busy.set(false);
            tick.update(|t| *t += 1);
        });
    };
    move || {
        status.get().and_then(Result::ok).filter(|s| s["app"] == true).map(|s| {
            let (n, left) = (s["decks"].as_u64().unwrap_or(0), s["left"].as_u64().unwrap_or(0));
            view! {
                <section class="space-y-2 rounded-lg border border-violet-200 bg-violet-50/40 p-4 dark:border-violet-900 dark:bg-violet-950/20">
                    <h2 class="font-medium">"Bring in decks"</h2>
                    <p class="text-sm text-zinc-600 dark:text-zinc-400">{format!(
                        "The decks app is installed here with {n} {}, {left} still to bring in. Each comes in once as a carbon6 deck: its slides from its deck.html (on a connected worker) as html slides, its brief as the first slide's notes, its PDFs as past exports; the app's themes become themes. Then you can remove the app (Apps).",
                        if n == 1 { "deck" } else { "decks" }
                    )}</p>
                    <button class=BUTTON disabled=move || busy.get() || left == 0 on:click=go>
                        {move || if busy.get() { "Bringing them in…" } else { "Bring in decks" }}
                    </button>
                    {move || result.get().map(|r| match r {
                        Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                        Ok(v) => view! {
                            <ul class="space-y-0.5 text-sm">
                                {v["made"].as_array().cloned().unwrap_or_default().into_iter().map(|m| view! {
                                    <li>{format!("✓ {}", m["title"].as_str().unwrap_or(""))}</li>
                                }).collect_view()}
                                {v["skipped"].as_array().cloned().unwrap_or_default().into_iter().map(|m| view! {
                                    <li class="text-zinc-500">{format!("– {}: {}", m["title"].as_str().unwrap_or(""), m["why"].as_str().unwrap_or(""))}</li>
                                }).collect_view()}
                                {v["themes"].as_array().filter(|t| !t.is_empty()).map(|t| view! {
                                    <li>{format!("Themes: {}", t.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))}</li>
                                })}
                            </ul>
                        }.into_any(),
                    })}
                </section>
            }
        })
    }
}

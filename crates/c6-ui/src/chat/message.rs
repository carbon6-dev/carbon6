//! A conversation's messages as Slack shows them (docs/chat.md §3): by
//! day, grouped by sender, a "New" line where you'd read to, older ones as
//! you scroll up; each message with its avatar, rendered text, files,
//! reactions, thread and the actions on hover.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};
use wasm_bindgen::JsCast;

use super::{Chat, Place, text};
use crate::api;
use crate::util::ErrorBox;

/// Messages a page.
const PAGE: u64 = 50;
/// Quick reactions on hover.
const QUICK: [&str; 4] = ["white_check_mark", "eyes", "+1", "tada"];

/// What a list's messages share: whether reading it marks it read (not
/// after you mark it unread).
#[derive(Clone, Copy)]
struct ListCtl {
    auto_read: RwSignal<bool>,
}

fn ts_of(m: &Value) -> String {
    m["meta"]["ts"].as_str().unwrap_or("").to_owned()
}

fn date(ms: i64) -> js_sys::Date {
    js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64))
}

fn opts(pairs: &[(&str, &str)]) -> js_sys::Object {
    let o = js_sys::Object::new();
    for (k, v) in pairs {
        let _ = js_sys::Reflect::set(&o, &(*k).into(), &(*v).into());
    }
    o
}

/// "Today", "Yesterday", or "Monday, October 26".
fn day(ms: i64) -> String {
    let d = date(ms);
    let today = js_sys::Date::new_0();
    let yesterday = date(js_sys::Date::now() as i64 - 86_400_000);
    if d.to_date_string() == today.to_date_string() {
        "Today".into()
    } else if d.to_date_string() == yesterday.to_date_string() {
        "Yesterday".into()
    } else {
        let mut o = vec![("weekday", "long"), ("month", "long"), ("day", "numeric")];
        if d.get_full_year() != today.get_full_year() {
            o.push(("year", "numeric"));
        }
        String::from(d.to_locale_date_string("default", &opts(&o)))
    }
}

fn time(ms: i64) -> String {
    String::from(date(ms).to_locale_time_string_with_options("default", &opts(&[("hour", "numeric"), ("minute", "2-digit")])))
}

fn full(ms: i64) -> String {
    String::from(date(ms).to_locale_string("default", &wasm_bindgen::JsValue::UNDEFINED))
}

/// Whether `m` goes under `prev` without its own name: the same sender,
/// the same day, within five minutes.
pub(crate) fn continues(prev: &Value, m: &Value) -> bool {
    let (a, b) = (prev["created_at"].as_i64().unwrap_or(0), m["created_at"].as_i64().unwrap_or(0));
    prev["meta"]["user"] == m["meta"]["user"] && b - a < 5 * 60_000 && day(a) == day(b)
}

/// A conversation's messages, newest at the bottom.
#[component]
pub(crate) fn MessageList(place: Place, at: Option<String>) -> impl IntoView {
    let chat = Chat::get();
    let msgs = RwSignal::new(Vec::<Value>::new());
    let error = RwSignal::new(None::<String>);
    let older_done = RwSignal::new(false);
    let loading_older = RwSignal::new(false);
    let jumped = RwSignal::new(at.clone());
    let auto_read = RwSignal::new(true);
    provide_context(ListCtl { auto_read });
    // Where you'd read to when you came in (the "New" line stays there).
    let read_then = StoredValue::new(None::<Option<String>>);
    let (rp, mp, lp) = (place.clone(), place.clone(), place.clone());
    Effect::new(move || {
        if read_then.with_value(Option::is_none)
            && let Some(c) = chat
                .channels
                .with(|l| l.iter().find(|c| c["workspace"] == rp.workspace.as_str() && c["channel"] == rp.channel.as_str()).cloned())
        {
            read_then.set_value(Some(c["read_ts"].as_str().map(str::to_owned)));
        }
    });
    // The newest page (merged with older ones loaded), or around a message.
    Effect::new(move || {
        chat.tick.track();
        let around = jumped.get();
        let p = lp.clone();
        spawn_local(async move {
            let mut params = json!({"workspace": p.workspace, "channel": p.channel, "limit": PAGE});
            if let Some(ts) = &around {
                params["around"] = json!(ts);
            }
            match api::rpc("memex.chat.messages", params).await {
                Ok(v) => {
                    error.set(None);
                    let page = v.as_array().cloned().unwrap_or_default();
                    if around.is_some() {
                        msgs.set(page);
                        return;
                    }
                    let oldest = page.last().and_then(|m| m["created_at"].as_i64()).unwrap_or(0);
                    let full = page.len() as u64 >= PAGE;
                    msgs.update(|cur| {
                        // Older ones loaded before stay; within the page, what it says.
                        let keep: Vec<Value> =
                            cur.iter().filter(|m| m["created_at"].as_i64().unwrap_or(0) < oldest && full).cloned().collect();
                        *cur = page;
                        cur.extend(keep);
                    });
                    if !full {
                        older_done.set(true);
                    }
                }
                Err(e) => error.set(Some(e)),
            }
        });
    });
    let load_older = move || {
        if loading_older.get_untracked() || older_done.get_untracked() {
            return;
        }
        let Some(before) = msgs.with_untracked(|l| l.last().and_then(|m| m["created_at"].as_i64())) else { return };
        loading_older.set(true);
        let p = mp.clone();
        spawn_local(async move {
            if let Ok(v) =
                api::rpc("memex.chat.messages", json!({"workspace": p.workspace, "channel": p.channel, "before": before, "limit": PAGE}))
                    .await
            {
                let page = v.as_array().cloned().unwrap_or_default();
                if (page.len() as u64) < PAGE {
                    older_done.set(true);
                }
                msgs.update(|cur| cur.extend(page));
            }
            loading_older.set(false);
        });
    };
    // Read: up to the newest you've seen (in Slack too).
    let marked = RwSignal::new(String::new());
    let read = place.clone();
    Effect::new(move || {
        let newest = msgs.with(|l| l.first().map(ts_of)).unwrap_or_default();
        if newest.is_empty() || !auto_read.get() || jumped.get().is_some() || marked.get_untracked() == newest {
            return;
        }
        let current = chat.conversation(&read.workspace, &read.channel).and_then(|c| c["read_ts"].as_str().map(str::to_owned));
        let ahead = current.as_deref().is_none_or(|r| r.parse::<f64>().unwrap_or(0.0) < newest.parse::<f64>().unwrap_or(0.0));
        marked.set(newest.clone());
        if ahead {
            let params = json!({"workspace": read.workspace, "channel": read.channel, "ts": newest});
            spawn_local(async move {
                let _ = api::rpc("memex.chat.mark_read", params).await;
                chat.refresh();
            });
        }
    });
    // Scrolled near the top: older ones.
    let top = NodeRef::<leptos::html::Div>::new();
    let observer = StoredValue::new_local(None::<web_sys::IntersectionObserver>);
    let lo = load_older.clone();
    Effect::new(move || {
        if let Some(el) = top.get() {
            let lo = lo.clone();
            let cb = wasm_bindgen::closure::Closure::<dyn Fn(js_sys::Array)>::new(move |entries: js_sys::Array| {
                if entries.iter().any(|e| e.unchecked_into::<web_sys::IntersectionObserverEntry>().is_intersecting()) {
                    lo();
                }
            });
            let o = web_sys::IntersectionObserverInit::new();
            o.set_root_margin("400px");
            if let Ok(obs) = web_sys::IntersectionObserver::new_with_options(cb.as_ref().unchecked_ref(), &o) {
                obs.observe(&el);
                observer.set_value(Some(obs));
            }
            cb.forget();
        }
    });
    on_cleanup(move || {
        if let Some(o) = observer.try_get_value().flatten() {
            o.disconnect();
        }
    });
    // A message jumped to: in view.
    let target = at.clone();
    Effect::new(move || {
        let n = msgs.with(Vec::len);
        if let Some(ts) = target.clone().filter(|_| n > 0) {
            set_timeout(
                move || {
                    if let Some(el) = document().get_element_by_id(&format!("m-{ts}")) {
                        el.scroll_into_view_with_bool(false);
                    }
                },
                std::time::Duration::from_millis(50),
            );
        }
    });
    let shown = place.clone();
    let highlight = at.clone();
    view! {
        <div class="flex min-h-0 flex-1 flex-col-reverse overflow-y-auto">
            <div class="pb-2">
                <div node_ref=top class="px-4 py-2 text-center text-xs text-zinc-500">
                    {move || if older_done.get() {
                        view! { <span>{format!("The start of {}", shown.name)}</span> }.into_any()
                    } else {
                        let lo = load_older.clone();
                        view! { <button class="hover:underline" on:click=move |_| lo()>{move || if loading_older.get() { "Loading…" } else { "Load older messages" }}</button> }.into_any()
                    }}
                </div>
                {move || error.get().map(|e| view! { <div class="p-2"><ErrorBox message=e /></div> })}
                {move || {
                    let mut list = msgs.get();
                    list.reverse();
                    let read = read_then.get_value().flatten().and_then(|r| r.parse::<f64>().ok());
                    let mut new_shown = false;
                    let mut prev: Option<Value> = None;
                    let place = place.clone();
                    list.into_iter().map(|m| {
                        let at = m["created_at"].as_i64().unwrap_or(0);
                        let divider = prev.as_ref().is_none_or(|p| day(p["created_at"].as_i64().unwrap_or(0)) != day(at));
                        let ts: f64 = ts_of(&m).parse().unwrap_or(0.0);
                        let is_new = !new_shown && read.is_some_and(|r| ts > r && r > 0.0) && m["meta"]["from_me"] != true;
                        new_shown |= is_new;
                        let compact = !divider && !is_new && prev.as_ref().is_some_and(|p| continues(p, &m));
                        prev = Some(m.clone());
                        let lit = highlight.as_deref() == Some(ts_of(&m).as_str());
                        view! {
                            {divider.then(|| view! {
                                <div class="sticky top-0 z-10 my-2 flex items-center gap-2 px-4">
                                    <span class="h-px flex-1 bg-zinc-200 dark:bg-zinc-800"></span>
                                    <span class="rounded-full border border-zinc-200 bg-white px-3 py-0.5 text-[11px] font-medium dark:border-zinc-700 dark:bg-zinc-900">{day(at)}</span>
                                    <span class="h-px flex-1 bg-zinc-200 dark:bg-zinc-800"></span>
                                </div>
                            })}
                            {is_new.then(|| view! {
                                <div class="my-1 flex items-center gap-2 px-4 text-[11px] font-semibold text-red-600">
                                    <span class="h-px flex-1 bg-red-500"></span><span>"New"</span>
                                </div>
                            })}
                            <Message m place=place.clone() compact in_thread=false highlight=lit />
                        }
                    }).collect_view()
                }}
                {move || jumped.get().is_some().then(|| view! {
                    <div class="sticky bottom-0 flex justify-center p-2">
                        <button class="rounded-full bg-sky-700 px-3 py-1 text-xs text-white shadow" on:click=move |_| { jumped.set(None); older_done.set(false); }>"Jump to the newest ↓"</button>
                    </div>
                })}
            </div>
        </div>
    }
}

#[component]
pub(crate) fn Avatar(url: Option<String>, name: String, #[prop(default = "h-9 w-9")] size: &'static str) -> impl IntoView {
    match url.filter(|u| !u.is_empty()) {
        Some(u) => view! { <img src=u alt="" class=format!("{size} shrink-0 rounded-md") /> }.into_any(),
        None => view! {
            <span class=format!("{size} flex shrink-0 items-center justify-center rounded-md bg-zinc-300 font-semibold text-zinc-700 dark:bg-zinc-700 dark:text-zinc-200")>{super::initial(&name)}</span>
        }
        .into_any(),
    }
}

/// One message.
#[component]
pub(crate) fn Message(
    m: Value,
    place: Place,
    #[prop(default = false)] compact: bool,
    #[prop(default = false)] in_thread: bool,
    #[prop(default = false)] highlight: bool,
    /// Shown with where it is (in the views).
    #[prop(default = false)]
    with_place: bool,
) -> impl IntoView {
    let chat = Chat::get();
    let ctl = use_context::<ListCtl>();
    let meta = m["meta"].clone();
    let id = m["id"].as_str().unwrap_or("").to_owned();
    let ts = meta["ts"].as_str().unwrap_or("").to_owned();
    let thread_ts = meta["thread_ts"].as_str().map(str::to_owned);
    let who = meta["user_name"].as_str().unwrap_or("").to_owned();
    let user = meta["user"].as_str().unwrap_or("").to_owned();
    let person = chat.person(&place.workspace, &user);
    let avatar = person.as_ref().and_then(|p| p["avatar"].as_str().map(str::to_owned));
    let title = person.as_ref().and_then(|p| p["title"].as_str().filter(|t| !t.is_empty()).map(str::to_owned));
    let mine = meta["from_me"] == true;
    let bot = meta["bot"] == true || person.as_ref().is_some_and(|p| p["bot"] == true);
    let body = m["text"].as_str().unwrap_or("");
    let text = body.strip_prefix(&format!("{who}: ")).unwrap_or(body).to_owned();
    let files_only = meta["files"].is_array() && body == format!("{who} shared {}", file_names(&meta).join(", "));
    let at = m["created_at"].as_i64().unwrap_or(0);
    let replies = meta["reply_count"].as_u64().unwrap_or(0);
    let url = m["url"].as_str().unwrap_or("").to_owned();
    let mut reactions: Vec<(String, u64)> =
        meta["reactions"].as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(1))).collect()).unwrap_or_default();
    reactions.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let my_reactions: Vec<String> =
        meta["my_reactions"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_owned)).collect();
    let picker = RwSignal::new(false);
    let editing = {
        let id = id.clone();
        Memo::new(move |_| chat.editing.get().as_deref() == Some(id.as_str()))
    };
    let react = {
        let (place, ts, thread_ts) = (place.clone(), ts.clone(), thread_ts.clone());
        move |name: String, remove: bool| {
            let mut params = json!({"workspace": place.workspace, "channel": place.channel, "ts": ts, "name": name, "remove": remove});
            if let Some(t) = thread_ts.clone().filter(|t| *t != ts) {
                params["thread_ts"] = json!(t);
            }
            chat.act("memex.chat.react", params);
        }
    };
    let open_thread = {
        let (place, t) = (place.clone(), thread_ts.clone().unwrap_or_else(|| ts.clone()));
        move || {
            chat.details.set(false);
            chat.thread.set(Some((place.clone(), t.clone())));
        }
    };
    let known = chat.known(&place.workspace);
    let opener = Callback::new(move |channel: String| {
        let p = chat.place(&chat.shown_workspace().unwrap_or_default(), &channel);
        chat.open(p, None);
    });
    let shown_text = text.clone();
    let btn = "rounded px-1.5 py-0.5 hover:bg-zinc-200 dark:hover:bg-zinc-700";
    let where_ = with_place.then(|| {
        let p = place.clone();
        let jump = ts.clone();
        let reply = thread_ts.clone().filter(|t| *t != ts);
        view! {
            <button class="mb-0.5 block text-[11px] font-medium text-zinc-500 hover:underline"
                on:click=move |_| match &reply {
                    Some(t) => { chat.open(p.clone(), Some(t.clone())); chat.thread.set(Some((p.clone(), t.clone()))); }
                    None => chat.open(p.clone(), Some(jump.clone())),
                }>
                {format!("{}{}", place.name, if thread_ts.as_deref().is_some_and(|t| t != ts) { " · in a thread" } else { "" })}
            </button>
        }
    });
    let actions = {
        let (react, open_thread) = (react.clone(), open_thread.clone());
        let (place, ts, thread_ts, id, url) = (place.clone(), ts.clone(), thread_ts.clone(), id.clone(), url.clone());
        move || {
            let react = react.clone();
            let open_thread = open_thread.clone();
            let (p1, p2, ts1, ts2, tt, id1, id2, url1, url2) =
                (place.clone(), place.clone(), ts.clone(), ts.clone(), thread_ts.clone(), id.clone(), id.clone(), url.clone(), url.clone());
            view! {
                <div class="absolute -top-3 right-3 z-20 hidden items-center gap-0.5 rounded-md border border-zinc-200 bg-white px-1 py-0.5 text-xs shadow-sm group-hover:flex dark:border-zinc-700 dark:bg-zinc-900">
                    {QUICK.iter().map(|n| { let react = react.clone(); view! { <button class=btn title=format!(":{n}:") on:click=move |_| react(n.to_string(), false)>{text::face(n)}</button> } }).collect_view()}
                    <button class=btn title="Add a reaction" on:click=move |_| picker.update(|p| *p = !*p)>"☺+"</button>
                    {(!in_thread).then(|| view! { <button class=btn title="Reply in a thread" on:click=move |_| open_thread()>"💬"</button> })}
                    <button class=btn title="Save for later" on:click=move |_| chat.act("memex.chat.save", json!({"id": id1}))>"🔖"</button>
                    <button class=btn title="Copy link" on:click=move |_| { let _ = window().navigator().clipboard().write_text(&url1); }>"🔗"</button>
                    {(!in_thread && ctl.is_some()).then(|| view! {
                        <button class=btn title="Mark unread from here" on:click=move |_| {
                            if let Some(c) = ctl { c.auto_read.set(false); }
                            chat.act("memex.chat.unread", json!({"workspace": p1.workspace, "channel": p1.channel, "ts": ts1}));
                        }>"◉"</button>
                    })}
                    {mine.then(|| view! {
                        <button class=btn title="Edit" on:click=move |_| chat.editing.set(Some(id2.clone()))>"✎"</button>
                        <button class=format!("{btn} text-red-600") title="Delete" on:click=move |_| {
                            if window().confirm_with_message("Delete this message? This can't be undone.").unwrap_or(false) {
                                let mut params = json!({"workspace": p2.workspace, "channel": p2.channel, "ts": ts2});
                                if let Some(t) = tt.clone() { params["thread_ts"] = json!(t); }
                                chat.act("memex.chat.delete", params);
                            }
                        }>"🗑"</button>
                    })}
                    <a class=btn href=url2 target="_blank" rel="noopener" title="Open in Slack">"↗"</a>
                </div>
            }
        }
    };
    let files: Vec<Value> = meta["files"].as_array().cloned().unwrap_or_default();
    let edit_text = text.clone();
    let edit_place = place.clone();
    let (edit_ts, edit_thread) = (ts.clone(), thread_ts.clone());
    view! {
        <div id=format!("m-{ts}")
            class=format!("group relative flex gap-2 px-4 {} {}", if compact { "py-0.5" } else { "pt-2 pb-0.5" },
                if highlight { "bg-amber-50 dark:bg-amber-950/40" } else { "hover:bg-zinc-50 dark:hover:bg-zinc-900/60" })>
            <div class="w-9 shrink-0">
                {if compact {
                    view! { <span class="invisible block pt-0.5 text-right text-[10px] text-zinc-400 group-hover:visible" title=full(at)>{time(at)}</span> }.into_any()
                } else {
                    view! { <Avatar url=avatar name=who.clone() /> }.into_any()
                }}
            </div>
            <div class="min-w-0 flex-1">
                {where_}
                {(!compact).then(|| view! {
                    <div class="flex items-baseline gap-2">
                        <span class="font-semibold" title=title.unwrap_or_default()>{who.clone()}</span>
                        {bot.then(|| view! { <span class="rounded bg-zinc-200 px-1 text-[9px] font-semibold text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400">"APP"</span> })}
                        <span class="text-[11px] text-zinc-500" title=full(at)>{time(at)}</span>
                    </div>
                })}
                {move || if editing.get() {
                    view! { <EditBox text=edit_text.clone() place=edit_place.clone() ts=edit_ts.clone() thread_ts=edit_thread.clone() /> }.into_any()
                } else {
                    view! {
                        <div class="leading-relaxed">
                            {(!files_only).then(|| text::render(&shown_text, &known, opener))}
                            {(meta["edited"] == true).then(|| view! { <span class="ml-1 text-[11px] text-zinc-400">"(edited)"</span> })}
                        </div>
                    }.into_any()
                }}
                {(!files.is_empty()).then(|| view! {
                    <div class="mt-1 flex flex-wrap gap-2">
                        {files.iter().map(|f| {
                            let name = f["name"].as_str().unwrap_or("file").to_owned();
                            let href = f["url"].as_str().map(str::to_owned).unwrap_or_else(|| url.clone());
                            let kind = f["mime"].as_str().unwrap_or("").split('/').next_back().unwrap_or("").to_uppercase();
                            let size = f["size"].as_u64().map(|s| if s > 1 << 20 { format!("{:.1} MB", s as f64 / 1048576.0) } else { format!("{} KB", s.div_ceil(1024)) }).unwrap_or_default();
                            view! {
                                <a href=href target="_blank" rel="noopener" class="flex max-w-xs items-center gap-2 rounded-md border border-zinc-200 px-2 py-1 hover:bg-zinc-50 dark:border-zinc-700 dark:hover:bg-zinc-800">
                                    <span class="text-lg">"📄"</span>
                                    <span class="min-w-0">
                                        <span class="block truncate text-xs font-medium">{name}</span>
                                        <span class="block text-[10px] text-zinc-500">{format!("{kind} {size}")}</span>
                                    </span>
                                </a>
                            }
                        }).collect_view()}
                    </div>
                })}
                {(!reactions.is_empty()).then(|| {
                    let react = react.clone();
                    view! {
                        <div class="mt-1 flex flex-wrap gap-1">
                            {reactions.into_iter().map(|(n, c)| {
                                let yours = my_reactions.contains(&n);
                                let react = react.clone();
                                let name = n.clone();
                                view! {
                                    <button title=format!(":{n}:")
                                        class=format!("flex items-center gap-1 rounded-full border px-1.5 text-xs {}",
                                            if yours { "border-sky-400 bg-sky-50 dark:border-sky-600 dark:bg-sky-950" } else { "border-zinc-200 hover:border-zinc-400 dark:border-zinc-700" })
                                        on:click=move |_| react(name.clone(), yours)>
                                        <span>{text::face(&n)}</span><span class="text-[11px]">{c.to_string()}</span>
                                    </button>
                                }
                            }).collect_view()}
                            <button class="rounded-full border border-zinc-200 px-1.5 text-xs text-zinc-500 hover:border-zinc-400 dark:border-zinc-700" title="Add a reaction"
                                on:click=move |_| picker.update(|p| *p = !*p)>"☺+"</button>
                        </div>
                    }
                })}
                {(!in_thread && replies > 0).then(|| {
                    let open_thread = open_thread.clone();
                    view! {
                        <button class="mt-1 flex items-center gap-2 rounded-md px-1 py-0.5 text-xs hover:bg-white hover:ring-1 hover:ring-zinc-200 dark:hover:bg-zinc-900 dark:hover:ring-zinc-700" on:click=move |_| open_thread()>
                            <span class="font-semibold text-sky-700 dark:text-sky-400">{format!("{replies} repl{}", if replies == 1 { "y" } else { "ies" })}</span>
                            <span class="text-zinc-500">"View thread"</span>
                        </button>
                    }
                })}
            </div>
            {actions}
            {move || picker.get().then(|| {
                let react = react.clone();
                view! { <EmojiPicker pick=Callback::new(move |n: String| { react(n, false); picker.set(false); }) close=picker /> }
            })}
        </div>
    }
}

fn file_names(meta: &Value) -> Vec<String> {
    meta["files"].as_array().into_iter().flatten().filter_map(|f| f["name"].as_str().map(str::to_owned)).collect()
}

/// Your message's text, changed in place.
#[component]
fn EditBox(text: String, place: Place, ts: String, thread_ts: Option<String>) -> impl IntoView {
    let chat = Chat::get();
    let value = RwSignal::new(text);
    let area = NodeRef::<leptos::html::Textarea>::new();
    Effect::new(move || {
        if let Some(el) = area.get() {
            let _ = el.focus();
        }
    });
    let save = move || {
        let t = super::composer::encode(&value.get_untracked(), &place.workspace, chat);
        if t.trim().is_empty() {
            return;
        }
        let mut params = json!({"workspace": place.workspace, "channel": place.channel, "ts": ts, "text": t});
        if let Some(tt) = thread_ts.clone().filter(|x| *x != ts) {
            params["thread_ts"] = json!(tt);
        }
        chat.editing.set(None);
        chat.act("memex.chat.edit", params);
    };
    let s2 = save.clone();
    view! {
        <div class="mt-1 space-y-1">
            <textarea node_ref=area class="w-full rounded-md border border-sky-500 bg-white p-2 text-sm dark:bg-zinc-900" rows="3"
                prop:value=move || value.get() on:input=move |ev| value.set(event_target_value(&ev))
                on:keydown=move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
                    "Enter" if !ev.shift_key() => { ev.prevent_default(); save(); }
                    "Escape" => { ev.prevent_default(); chat.editing.set(None); }
                    _ => {}
                }></textarea>
            <div class="flex gap-2 text-xs">
                <button class="rounded-md bg-sky-700 px-2 py-1 text-white" on:click=move |_| s2()>"Save"</button>
                <button class="rounded-md px-2 py-1 hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=move |_| chat.editing.set(None)>"Cancel"</button>
                <span class="self-center text-zinc-500">"Enter to save · Esc to cancel"</span>
            </div>
        </div>
    }
}

/// Picks an emoji (by name, searched).
#[component]
pub(crate) fn EmojiPicker(pick: Callback<String>, close: RwSignal<bool>) -> impl IntoView {
    let q = RwSignal::new(String::new());
    let input = NodeRef::<leptos::html::Input>::new();
    Effect::new(move || {
        if let Some(el) = input.get() {
            let _ = el.focus();
        }
    });
    view! {
        <div class="absolute right-3 top-6 z-30 w-72 rounded-lg border border-zinc-200 bg-white p-2 shadow-xl dark:border-zinc-700 dark:bg-zinc-900"
            on:keydown=move |ev: web_sys::KeyboardEvent| if ev.key() == "Escape" { close.set(false); }>
            <input node_ref=input class="mb-2 w-full rounded border border-zinc-300 bg-transparent px-2 py-1 text-xs dark:border-zinc-700" placeholder="Search emoji"
                prop:value=move || q.get() on:input=move |ev| q.set(event_target_value(&ev))
                on:keydown=move |ev: web_sys::KeyboardEvent| if ev.key() == "Enter" {
                    let q = q.get_untracked();
                    if let Some((n, _)) = text::EMOJI.iter().find(|(n, _)| n.contains(q.as_str())) { pick.run(n.to_string()); }
                } />
            <div class="grid max-h-48 grid-cols-8 gap-0.5 overflow-y-auto">
                {move || {
                    let q = q.get();
                    let mut seen = std::collections::HashSet::new();
                    text::EMOJI.iter().filter(|(n, e)| n.contains(q.as_str()) && seen.insert(*e)).take(160).map(|(n, e)| {
                        let n = n.to_string();
                        view! { <button class="rounded p-0.5 text-lg hover:bg-zinc-100 dark:hover:bg-zinc-800" title=format!(":{n}:") on:click=move |_| pick.run(n.clone())>{*e}</button> }
                    }).collect_view()
                }}
            </div>
        </div>
    }
}

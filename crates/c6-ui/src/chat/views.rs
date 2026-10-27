//! Unreads, Threads, Activity, Saved, Search results, and a conversation's
//! details (docs/chat.md §3).

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};

use super::message::{Avatar, Message};
use super::{Chat, ConvIcon, Place, place_of, text};
use crate::api;
use crate::util::ErrorBox;

fn header(title: &'static str, note: impl IntoView + 'static) -> impl IntoView {
    view! {
        <header class="flex items-center gap-3 border-b border-zinc-200 px-4 py-2 dark:border-zinc-800">
            <span class="font-semibold">{title}</span>
            {note}
        </header>
    }
}

/// A message's place, from its meta.
fn where_of(m: &Value) -> Place {
    Place {
        workspace: m["meta"]["workspace"].as_str().unwrap_or("").to_owned(),
        channel: m["meta"]["channel"].as_str().unwrap_or("").to_owned(),
        name: m["meta"]["channel_name"].as_str().unwrap_or("").to_owned(),
    }
}

fn empty(what: &'static str) -> AnyView {
    view! { <p class="p-6 text-center text-zinc-500">{what}</p> }.into_any()
}

/// Every unread conversation's new messages.
#[component]
pub(crate) fn Unreads() -> impl IntoView {
    let chat = Chat::get();
    let list = LocalResource::new(move || {
        chat.tick.track();
        api::rpc("memex.chat.unreads", json!({}))
    });
    let read = move |c: Value| {
        let ts = c["messages"].as_array().and_then(|l| l.last()).and_then(|m| m["meta"]["ts"].as_str().map(str::to_owned));
        if let Some(ts) = ts {
            chat.act("memex.chat.mark_read", json!({"workspace": c["workspace"], "channel": c["channel"], "ts": ts}));
        }
    };
    view! {
        {header("Unreads", view! {
            <span class="flex-1"></span>
            <button class="text-xs text-zinc-500 hover:underline" on:click=move |_| {
                if let Some(Ok(v)) = list.get_untracked() { for c in v.as_array().cloned().unwrap_or_default() { read(c); } }
            }>"Mark all as read"</button>
        })}
        <div class="min-h-0 flex-1 space-y-4 overflow-y-auto py-3">
            {move || list.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) if v.as_array().is_none_or(Vec::is_empty) => empty("You're all caught up."),
                Ok(v) => v.as_array().cloned().unwrap_or_default().into_iter().map(|c| {
                    let p = place_of(&c);
                    let (go, mark) = (p.clone(), c.clone());
                    let n = c["unread"].as_i64().unwrap_or(0);
                    let icon = c.clone();
                    view! {
                        <section class="mx-4 overflow-hidden rounded-lg border border-zinc-200 dark:border-zinc-800">
                            <div class="flex items-center gap-2 border-b border-zinc-200 bg-zinc-50 px-3 py-1.5 dark:border-zinc-800 dark:bg-zinc-900/60">
                                <ConvIcon c=icon />
                                <button class="font-semibold hover:underline" on:click=move |_| chat.open(go.clone(), None)>{p.name.clone()}</button>
                                <span class="text-xs text-zinc-500">{format!("{n} new")}</span>
                                <span class="flex-1"></span>
                                <button class="text-xs text-zinc-500 hover:underline" on:click=move |_| read(mark.clone())>"Mark as read"</button>
                            </div>
                            {c["messages"].as_array().cloned().unwrap_or_default().into_iter().map(|m| view! { <Message m place=p.clone() /> }).collect_view()}
                        </section>
                    }
                }).collect_view().into_any(),
            })}
        </div>
    }
}

/// Threads you're in, with what's new.
#[component]
pub(crate) fn Threads() -> impl IntoView {
    let chat = Chat::get();
    let list = LocalResource::new(move || {
        chat.tick.track();
        api::rpc("memex.chat.threads", json!({"limit": 50}))
    });
    view! {
        {header("Threads", view! { <span class="text-xs text-zinc-500">"Threads you've written in or been mentioned in, the latest first"</span> })}
        <div class="min-h-0 flex-1 space-y-3 overflow-y-auto py-3">
            {move || list.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) if v.as_array().is_none_or(Vec::is_empty) => empty("No threads yet."),
                Ok(v) => v.as_array().cloned().unwrap_or_default().into_iter().map(|t| {
                    let p = Place {
                        workspace: t["workspace"].as_str().unwrap_or("").to_owned(),
                        channel: t["channel"].as_str().unwrap_or("").to_owned(),
                        name: t["channel_name"].as_str().unwrap_or("").to_owned(),
                    };
                    let ts = t["thread_ts"].as_str().unwrap_or("").to_owned();
                    let new = t["new"].as_u64().unwrap_or(0);
                    let replies = t["replies"].as_u64().unwrap_or(0);
                    let (go, open) = (p.clone(), p.clone());
                    let ts2 = ts.clone();
                    view! {
                        <section class=format!("mx-4 overflow-hidden rounded-lg border {}", if new > 0 { "border-sky-400 dark:border-sky-700" } else { "border-zinc-200 dark:border-zinc-800" })>
                            <div class="flex items-center gap-2 border-b border-zinc-200 bg-zinc-50 px-3 py-1.5 text-xs dark:border-zinc-800 dark:bg-zinc-900/60">
                                <button class="font-semibold hover:underline" on:click=move |_| chat.open(go.clone(), Some(ts.clone()))>{p.name.clone()}</button>
                                <span class="text-zinc-500">{format!("{replies} repl{}", if replies == 1 { "y" } else { "ies" })}</span>
                                {(new > 0).then(|| view! { <span class="rounded-full bg-sky-600 px-1.5 font-semibold text-white">{format!("{new} new")}</span> })}
                                <span class="flex-1"></span>
                                <button class="text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| { chat.details.set(false); chat.thread.set(Some((open.clone(), ts2.clone()))); }>"Open the thread"</button>
                            </div>
                            {t["first"].is_object().then(|| view! { <Message m=t["first"].clone() place=p.clone() in_thread=true /> })}
                            {(replies > 1).then(|| view! { <p class="px-14 text-[11px] text-zinc-500">"…"</p> })}
                            <Message m=t["latest"].clone() place=p.clone() in_thread=true />
                        </section>
                    }
                }).collect_view().into_any(),
            })}
        </div>
    }
}

/// Mentions of you, and reactions to yours.
#[component]
pub(crate) fn Activity() -> impl IntoView {
    let chat = Chat::get();
    let only = RwSignal::new("all");
    let list = LocalResource::new(move || {
        chat.tick.track();
        api::rpc("memex.chat.activity", json!({"limit": 100}))
    });
    let tab = move |key: &'static str, label: &'static str| {
        view! {
            <button class=move || format!("rounded-full px-2 py-0.5 text-xs {}", if only.get() == key { "bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900" } else { "hover:bg-zinc-100 dark:hover:bg-zinc-800" })
                on:click=move |_| only.set(key)>{label}</button>
        }
    };
    view! {
        {header("Activity", view! { {tab("all", "All")} {tab("mention", "Mentions")} {tab("reaction", "Reactions")} })}
        <div class="min-h-0 flex-1 overflow-y-auto py-2">
            {move || list.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) => {
                    let want = only.get();
                    let items: Vec<Value> = v.as_array().cloned().unwrap_or_default().into_iter().filter(|a| want == "all" || a["why"] == want).collect();
                    if items.is_empty() {
                        return empty("Nothing yet.");
                    }
                    items.into_iter().map(|a| {
                        let m = a["message"].clone();
                        let why = if a["why"] == "mention" { "Mentioned you" } else { "Reactions to your message" };
                        view! {
                            <div class="border-b border-zinc-100 py-1 dark:border-zinc-900">
                                <p class="px-4 pt-1 text-[11px] font-medium text-zinc-500">{why}</p>
                                <Message m=m.clone() place=where_of(&m) with_place=true />
                            </div>
                        }
                    }).collect_view().into_any()
                }
            })}
        </div>
    }
}

/// Saved for later: in progress, and done.
#[component]
pub(crate) fn Saved() -> impl IntoView {
    let chat = Chat::get();
    let done = RwSignal::new(false);
    let list = LocalResource::new(move || {
        chat.tick.track();
        api::rpc("memex.chat.saved", json!({"done": done.get()}))
    });
    let tab = move |d: bool, label: &'static str| {
        view! {
            <button class=move || format!("rounded-full px-2 py-0.5 text-xs {}", if done.get() == d { "bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900" } else { "hover:bg-zinc-100 dark:hover:bg-zinc-800" })
                on:click=move |_| done.set(d)>{label}</button>
        }
    };
    view! {
        {header("Saved", view! { {tab(false, "In progress")} {tab(true, "Completed")} })}
        <div class="min-h-0 flex-1 overflow-y-auto py-2">
            {move || list.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(v) if v.as_array().is_none_or(Vec::is_empty) => empty(if done.get_untracked() { "Nothing completed yet." } else { "Save a message (🔖 on hover) to come back to it." }),
                Ok(v) => v.as_array().cloned().unwrap_or_default().into_iter().map(|m| {
                    let (id1, id2) = (m["id"].clone(), m["id"].clone());
                    let finished = m["done_at"].is_number();
                    view! {
                        <div class="border-b border-zinc-100 py-1 dark:border-zinc-900">
                            <Message m=m.clone() place=where_of(&m) with_place=true />
                            <div class="flex gap-3 px-14 pb-1 text-xs">
                                <button class="text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| chat.act("memex.chat.save", json!({"id": id1, "done": !finished}))>
                                    {if finished { "Move back to in progress" } else { "Mark complete" }}
                                </button>
                                <button class="text-zinc-500 hover:underline" on:click=move |_| chat.act("memex.chat.save", json!({"id": id2, "remove": true}))>"Remove"</button>
                            </div>
                        </div>
                    }
                }).collect_view().into_any(),
            })}
        </div>
    }
}

/// A search's words and filters: `in:#x`, `from:@y` (`from:me`),
/// `has:link|file|reaction`, `before:`/`after:`/`on:` a date, `is:thread`.
pub(crate) fn parse(q: &str) -> Value {
    let mut out = json!({});
    let mut words = Vec::new();
    let date = |s: &str| -> Option<i64> {
        let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(s));
        let t = d.get_time();
        (!t.is_nan()).then_some(t as i64)
    };
    for w in q.split_whitespace() {
        match w.split_once(':') {
            Some(("in", v)) => out["in"] = json!(v),
            Some(("from", v)) => out["from"] = json!(v.trim_start_matches('@')),
            Some(("has", v)) => out["has"] = json!(v.trim_start_matches(':')),
            Some(("before", v)) => out["before"] = json!(date(v)),
            Some(("after", v)) => out["after"] = json!(date(v)),
            Some(("on", v)) => {
                if let Some(t) = date(v) {
                    out["after"] = json!(t - 1);
                    out["before"] = json!(t + 86_400_000);
                }
            }
            Some(("is", "thread")) => out["thread"] = json!(true),
            _ => words.push(w),
        }
    }
    out["q"] = json!(words.join(" "));
    out
}

#[component]
pub(crate) fn SearchResults(q: String) -> impl IntoView {
    let chat = Chat::get();
    let query = q.clone();
    let list = LocalResource::new(move || {
        let mut p = parse(&query);
        if let Some(w) = chat.shown_workspace() {
            p["workspace"] = json!(w);
        }
        p["limit"] = json!(100);
        api::rpc("memex.chat.search", p)
    });
    view! {
        {header("Search", view! { <span class="truncate text-xs text-zinc-500">{format!("“{q}”")}</span> })}
        <div class="min-h-0 flex-1 overflow-y-auto py-2">
            {move || list.get().map(|r| match r {
                Err(e) => view! { <div class="p-3"><ErrorBox message=e /></div> }.into_any(),
                Ok(v) if v.as_array().is_none_or(Vec::is_empty) => empty("No messages found. Try fewer words, or filters: in:#channel from:@person has:link before:2026-10-01 is:thread"),
                Ok(v) => {
                    let n = v.as_array().map_or(0, Vec::len);
                    view! {
                        <p class="px-4 pb-1 text-xs text-zinc-500">{format!("{n}{} messages, the newest first", if n >= 100 { "+" } else { "" })}</p>
                        {v.as_array().cloned().unwrap_or_default().into_iter().map(|m| view! {
                            <div class="border-b border-zinc-100 py-1 dark:border-zinc-900"><Message m=m.clone() place=where_of(&m) with_place=true /></div>
                        }).collect_view()}
                    }.into_any()
                }
            })}
        </div>
    }
}

/// A conversation's details: about it, its people, files and links, and
/// your options for it.
#[component]
pub(crate) fn Details(place: Place) -> impl IntoView {
    let chat = Chat::get();
    let p = place.clone();
    let conv = Memo::new(move |_| chat.conversation(&p.workspace, &p.channel));
    let (fp, lp, rp) = (place.clone(), place.clone(), place.clone());
    let files = LocalResource::new(move || {
        api::rpc("memex.chat.search", json!({"workspace": fp.workspace, "in": fp.channel, "has": "file", "limit": 20}))
    });
    let links = LocalResource::new(move || {
        api::rpc("memex.chat.search", json!({"workspace": lp.workspace, "in": lp.channel, "has": "link", "limit": 20}))
    });
    let recent = LocalResource::new(move || {
        api::rpc("memex.chat.messages", json!({"workspace": rp.workspace, "channel": rp.channel, "limit": 200}))
    });
    let tab = RwSignal::new("about");
    let t = move |key: &'static str, label: &'static str| {
        view! {
            <button class=move || format!("border-b-2 px-2 py-1 text-xs {}", if tab.get() == key { "border-sky-600 font-semibold" } else { "border-transparent text-zinc-500" })
                on:click=move |_| tab.set(key)>{label}</button>
        }
    };
    let (w1, c1) = (place.workspace.clone(), place.channel.clone());
    view! {
        <header class="flex items-center gap-2 border-b border-zinc-200 px-4 py-2 dark:border-zinc-800">
            <span class="flex-1 truncate font-semibold">{place.name.clone()}</span>
            <button class="text-xs text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100" on:click=move |_| chat.details.set(false)>"✕"</button>
        </header>
        <div class="flex gap-1 border-b border-zinc-200 px-2 dark:border-zinc-800">{t("about", "About")}{t("people", "People")}{t("files", "Files")}{t("links", "Links")}</div>
        <div class="space-y-3 p-4 text-sm">
            {move || match tab.get() {
                "about" => conv.get().map(|c| {
                    let (w, ch) = (w1.clone(), c1.clone());
                    let (w2, ch2) = (w1.clone(), c1.clone());
                    let (starred, muted) = (c["starred"] == true, c["muted"] == true);
                    let row = |k: &'static str, v: String| (!v.is_empty()).then(|| view! {
                        <div><div class="text-[11px] font-semibold uppercase text-zinc-500">{k}</div><div class="whitespace-pre-wrap">{v}</div></div>
                    });
                    view! {
                        {row("Topic", c["topic"].as_str().unwrap_or("").to_owned())}
                        {row("Description", c["purpose"].as_str().unwrap_or("").to_owned())}
                        {row("Members", c["members"].as_i64().map(|n| n.to_string()).unwrap_or_default())}
                        {row("Kind", match c["channel_kind"].as_str() { Some("private") => "Private channel", Some("dm") => "Direct message", Some("group") => "Group message", _ => "Public channel" }.to_owned())}
                        {row("Status", c["user_status"].as_str().map(text::status).unwrap_or_default())}
                        {row("Workspace", c["workspace_name"].as_str().unwrap_or("").to_owned())}
                        <div class="flex flex-wrap gap-2 pt-1">
                            <button class="rounded-md border border-zinc-300 px-2 py-1 text-xs hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
                                on:click=move |_| chat.act("memex.chat.prefs", json!({"workspace": w, "channel": ch, "starred": !starred}))>{if starred { "★ Starred" } else { "☆ Star" }}</button>
                            <button class="rounded-md border border-zinc-300 px-2 py-1 text-xs hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
                                on:click=move |_| chat.act("memex.chat.prefs", json!({"workspace": w2, "channel": ch2, "muted": !muted}))>{if muted { "🔕 Muted" } else { "🔔 Mute" }}</button>
                        </div>
                    }.into_any()
                }).unwrap_or_else(|| ().into_any()),
                "people" => recent.get().map(|r| {
                    let list = r.ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
                    let mut seen: Vec<(String, String, usize)> = Vec::new();
                    for m in &list {
                        let (u, n) = (m["meta"]["user"].as_str().unwrap_or("").to_owned(), m["meta"]["user_name"].as_str().unwrap_or("").to_owned());
                        match seen.iter_mut().find(|x| x.0 == u) { Some(x) => x.2 += 1, None => seen.push((u, n, 1)) }
                    }
                    seen.sort_by_key(|x| std::cmp::Reverse(x.2));
                    let ws = place.workspace.clone();
                    view! {
                        <p class="text-xs text-zinc-500">"Who has written here lately"</p>
                        {seen.into_iter().map(|(u, n, k)| {
                            let person = chat.person(&ws, &u);
                            let avatar = person.as_ref().and_then(|p| p["avatar"].as_str().map(str::to_owned));
                            let title = person.as_ref().and_then(|p| p["title"].as_str().map(str::to_owned)).unwrap_or_default();
                            view! {
                                <div class="flex items-center gap-2">
                                    <Avatar url=avatar name=n.clone() size="h-7 w-7" />
                                    <div class="min-w-0 flex-1"><div class="truncate">{n}</div><div class="truncate text-[11px] text-zinc-500">{title}</div></div>
                                    <span class="text-[11px] text-zinc-500">{k.to_string()}</span>
                                </div>
                            }
                        }).collect_view()}
                    }.into_any()
                }).unwrap_or_else(|| ().into_any()),
                "files" => files.get().map(|r| shared(r, true)).unwrap_or_else(|| ().into_any()),
                _ => links.get().map(|r| shared(r, false)).unwrap_or_else(|| ().into_any()),
            }}
        </div>
    }
}

/// Files or links shared in a conversation, newest first.
fn shared(r: Result<Value, String>, files: bool) -> AnyView {
    let list = match r {
        Ok(v) => v.as_array().cloned().unwrap_or_default(),
        Err(e) => return view! { <ErrorBox message=e /> }.into_any(),
    };
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for m in &list {
        let who = m["meta"]["user_name"].as_str().unwrap_or("").to_owned();
        if files {
            for f in m["meta"]["files"].as_array().into_iter().flatten() {
                let href = f["url"].as_str().or(m["url"].as_str()).unwrap_or("").to_owned();
                rows.push((f["name"].as_str().unwrap_or("file").to_owned(), href, who.clone()));
            }
        } else {
            for w in m["text"].as_str().unwrap_or("").split_whitespace() {
                let w = w.trim_matches(|c: char| "()<>.,;!?".contains(c));
                if w.starts_with("http://") || w.starts_with("https://") {
                    rows.push((w.to_owned(), w.to_owned(), who.clone()));
                }
            }
        }
    }
    if rows.is_empty() {
        return view! { <p class="text-xs text-zinc-500">{if files { "No files shared yet." } else { "No links shared yet." }}</p> }
            .into_any();
    }
    rows.into_iter()
        .map(|(name, href, who)| {
            view! {
                <a href=href target="_blank" rel="noopener" class="block rounded-md px-2 py-1 hover:bg-zinc-100 dark:hover:bg-zinc-800">
                    <div class="truncate text-xs font-medium">{format!("{} {name}", if files { "📄" } else { "🔗" })}</div>
                    <div class="text-[11px] text-zinc-500">{who}</div>
                </a>
            }
        })
        .collect_view()
        .into_any()
}

/// Marks a whole conversation read (its newest message).
pub(crate) fn mark_all_read(chat: Chat, place: Place) {
    spawn_local(async move {
        if let Ok(v) = api::rpc("memex.chat.messages", json!({"workspace": place.workspace, "channel": place.channel, "limit": 1})).await
            && let Some(ts) = v[0]["meta"]["ts"].as_str()
        {
            let _ = api::rpc("memex.chat.mark_read", json!({"workspace": place.workspace, "channel": place.channel, "ts": ts})).await;
            chat.refresh();
        }
    });
}

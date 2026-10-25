//! Chat (docs/slack.md §7): a realm's Slack conversations as the memex
//! keeps them, with posting, replying and reactions through the slack app;
//! and Settings → Slack.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, ago, now_ms};

/// Seconds a post can be taken back before it goes.
const UNDO_SECONDS: u32 = 5;
/// Reactions offered at a click.
const QUICK: [(&str, &str); 4] = [("+1", "👍"), ("white_check_mark", "✅"), ("eyes", "👀"), ("tada", "🎉")];

#[derive(Clone, Debug, PartialEq)]
struct Place {
    workspace: String,
    channel: String,
    name: String,
}

fn emoji(name: &str) -> String {
    QUICK.iter().find(|(n, _)| *n == name).map(|(_, e)| e.to_string()).unwrap_or_else(|| format!(":{name}:"))
}

/// The Chat page: conversations, a conversation's messages, a thread.
#[component]
pub fn ChatPage() -> impl IntoView {
    let tick = RwSignal::new(0u32);
    let channels = LocalResource::new(move || {
        tick.track();
        api::rpc("memex.chat.channels", json!({}))
    });
    let open = RwSignal::new(None::<Place>);
    let thread = RwSignal::new(None::<String>);
    // Fresher while you look: a sync now and then, and the lists again.
    let handle = set_interval_with_handle(
        move || {
            spawn_local(async move {
                let _ = api::slack_sync().await;
            });
            tick.update(|t| *t += 1);
        },
        std::time::Duration::from_secs(30),
    )
    .ok();
    on_cleanup(move || {
        if let Some(h) = handle {
            h.clear();
        }
    });
    view! {
        <main class="flex h-full min-h-0">
            <nav class="w-56 shrink-0 overflow-y-auto border-r border-zinc-200 py-2 text-sm dark:border-zinc-800">
                {move || match channels.get() {
                    None => view! { <p class="px-3 text-xs text-zinc-500">"Loading…"</p> }.into_any(),
                    Some(Err(e)) => view! { <div class="px-2"><ErrorBox message=e /></div> }.into_any(),
                    Some(Ok(v)) if v.as_array().is_none_or(Vec::is_empty) => view! {
                        <p class="px-3 text-xs text-zinc-500">"Nothing yet. Connect a workspace in Settings → Slack; its conversations arrive as they're imported."</p>
                    }.into_any(),
                    Some(Ok(v)) => {
                        let list = v.as_array().cloned().unwrap_or_default();
                        let mut workspaces: Vec<(String, String)> = Vec::new();
                        for c in &list {
                            let w = (c["workspace"].as_str().unwrap_or("").to_owned(), c["workspace_name"].as_str().unwrap_or("").to_owned());
                            if !workspaces.contains(&w) {
                                workspaces.push(w);
                            }
                        }
                        workspaces.into_iter().map(|(w, wname)| {
                            let mine: Vec<Value> = list.iter().filter(|c| c["workspace"] == w.as_str()).cloned().collect();
                            view! {
                                <div class="px-3 pb-1 pt-2 text-[11px] font-semibold uppercase tracking-wide text-zinc-500">{wname}</div>
                                {mine.into_iter().map(|c| {
                                    let p = Place {
                                        workspace: c["workspace"].as_str().unwrap_or("").to_owned(),
                                        channel: c["channel"].as_str().unwrap_or("").to_owned(),
                                        name: c["channel_name"].as_str().unwrap_or("").to_owned(),
                                    };
                                    let (here, go) = (p.clone(), p.clone());
                                    view! {
                                        <button class=move || format!("flex w-full items-center gap-2 px-3 py-1 text-left {}",
                                                if open.with(|o| o.as_ref() == Some(&here)) { "bg-zinc-200 font-medium dark:bg-zinc-800" } else { "hover:bg-zinc-100 dark:hover:bg-zinc-900" })
                                            on:click=move |_| { thread.set(None); open.set(Some(go.clone())); }>
                                            <span class="min-w-0 flex-1 truncate">{p.name.clone()}</span>
                                            <span class="shrink-0 text-[10px] text-zinc-400">{ago(now_ms() - c["last_at"].as_i64().unwrap_or(0))}</span>
                                        </button>
                                    }
                                }).collect_view()}
                            }
                        }).collect_view().into_any()
                    }
                }}
            </nav>
            <section class="flex min-w-0 flex-1 flex-col">
                {move || match open.get() {
                    None => view! { <p class="p-6 text-sm text-zinc-500">"Pick a conversation."</p> }.into_any(),
                    Some(p) => view! { <Conversation place=p thread tick /> }.into_any(),
                }}
            </section>
            {move || open.get().zip(thread.get()).map(|(p, t)| view! {
                <aside class="flex w-96 shrink-0 flex-col border-l border-zinc-200 dark:border-zinc-800">
                    <Thread place=p thread_ts=t close=thread tick />
                </aside>
            })}
        </main>
    }
}

/// One message, with its reactions and, for a thread's first message, its
/// replies.
#[component]
fn Message(m: Value, place: Place, thread: Option<RwSignal<Option<String>>>, tick: RwSignal<u32>) -> impl IntoView {
    let meta = m["meta"].clone();
    let ts = meta["ts"].as_str().unwrap_or("").to_owned();
    let who = meta["user_name"].as_str().unwrap_or("").to_owned();
    let prefix = format!("{who}: ");
    let text = m["text"].as_str().unwrap_or("").strip_prefix(&prefix).unwrap_or(m["text"].as_str().unwrap_or("")).to_owned();
    let replies = meta["reply_count"].as_u64().unwrap_or(0);
    let reactions: Vec<(String, u64)> =
        meta["reactions"].as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(1))).collect()).unwrap_or_default();
    let files: Vec<String> = meta["files"].as_array().into_iter().flatten().filter_map(|f| f["name"].as_str().map(str::to_owned)).collect();
    let react = {
        let (place, ts) = (place.clone(), ts.clone());
        move |name: &'static str| {
            let params = json!({"workspace": place.workspace, "channel": place.channel, "ts": ts, "name": name});
            spawn_local(async move {
                if api::rpc("memex.chat.react", params).await.is_ok() {
                    let _ = api::slack_sync().await;
                    tick.update(|t| *t += 1);
                }
            });
        }
    };
    let url = m["url"].as_str().unwrap_or("").to_owned();
    view! {
        <div class="group px-4 py-2 hover:bg-zinc-50 dark:hover:bg-zinc-900/50">
            <div class="flex items-baseline gap-2 text-sm">
                <span class="font-medium">{who.clone()}</span>
                <span class="text-[11px] text-zinc-500">{ago(now_ms() - m["created_at"].as_i64().unwrap_or(0))}</span>
                {(meta["edited"] == true).then(|| view! { <span class="text-[11px] text-zinc-400">"(edited)"</span> })}
                <span class="flex-1"></span>
                <span class="hidden gap-1 group-hover:flex">
                    {QUICK.iter().map(|(name, e)| {
                        let react = react.clone();
                        view! { <button class="text-xs" title=format!("React :{name}:") on:click=move |_| react(name)>{*e}</button> }
                    }).collect_view()}
                    <a href=url target="_blank" rel="noopener" class="text-[11px] text-zinc-400 hover:underline" title="Open in Slack">"↗"</a>
                </span>
            </div>
            <p class="whitespace-pre-wrap break-words text-sm">{text}</p>
            {(!files.is_empty()).then(|| view! { <p class="text-xs text-zinc-500">{format!("📎 {}", files.join(", "))}</p> })}
            {(!reactions.is_empty()).then(|| view! {
                <div class="mt-1 flex flex-wrap gap-1">
                    {reactions.into_iter().map(|(n, c)| view! {
                        <span class="rounded-full border border-zinc-200 px-1.5 text-xs dark:border-zinc-700">{format!("{} {c}", emoji(&n))}</span>
                    }).collect_view()}
                </div>
            })}
            {thread.filter(|_| replies > 0).map(|t| {
                let ts = ts.clone();
                view! {
                    <button class="mt-1 text-xs text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| t.set(Some(ts.clone()))>
                        {format!("{replies} repl{}", if replies == 1 { "y" } else { "ies" })}
                    </button>
                }
            })}
        </div>
    }
}

/// A box to post: it goes after a few seconds, so it can be taken back.
#[component]
fn Composer(place: Place, thread_ts: Option<String>, tick: RwSignal<u32>, placeholder: &'static str) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let pending = RwSignal::new(None::<(String, u32)>);
    let error = RwSignal::new(None::<String>);
    let send = move || {
        let t = text.get_untracked();
        if t.trim().is_empty() || pending.get_untracked().is_some() {
            return;
        }
        text.set(String::new());
        let (place, thread_ts) = (place.clone(), thread_ts.clone());
        let id = js_sys::Math::random().to_bits() as u32;
        pending.set(Some((t.clone(), id)));
        spawn_local(async move {
            gloo_timers::future::sleep(std::time::Duration::from_secs(UNDO_SECONDS as u64)).await;
            // Taken back (or replaced) meanwhile: nothing goes.
            if pending.get_untracked().map(|(_, i)| i) != Some(id) {
                return;
            }
            pending.set(None);
            let mut params = json!({"workspace": place.workspace, "channel": place.channel, "text": t});
            if let Some(ts) = thread_ts {
                params["thread_ts"] = json!(ts);
            }
            match api::rpc("memex.chat.post", params).await {
                Ok(_) => {
                    error.set(None);
                    tick.update(|x| *x += 1);
                }
                Err(e) => {
                    error.set(Some(e));
                    text.set(t);
                }
            }
        });
    };
    view! {
        <div class="space-y-1 border-t border-zinc-200 p-2 dark:border-zinc-800">
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || pending.get().map(|(t, _)| view! {
                <div class="flex items-center gap-2 rounded-md bg-amber-50 px-2 py-1 text-xs text-amber-900 dark:bg-amber-950 dark:text-amber-200">
                    <span class="min-w-0 flex-1 truncate">{format!("Posting in a few seconds: {t}")}</span>
                    <button class="underline" on:click=move |_| { pending.set(None); text.set(t.clone()); }>"Undo"</button>
                </div>
            })}
            <textarea class=format!("{INPUT} h-16 text-sm") placeholder=placeholder prop:value=move || text.get()
                on:input=move |ev| text.set(event_target_value(&ev))
                on:keydown={
                    let send = send.clone();
                    move |ev: web_sys::KeyboardEvent| if ev.key() == "Enter" && !ev.shift_key() { ev.prevent_default(); send(); }
                }></textarea>
        </div>
    }
}

#[component]
fn Conversation(place: Place, thread: RwSignal<Option<String>>, tick: RwSignal<u32>) -> impl IntoView {
    let p = place.clone();
    let messages = LocalResource::new(move || {
        tick.track();
        api::rpc("memex.chat.messages", json!({"workspace": p.workspace, "channel": p.channel, "limit": 100}))
    });
    // Read up to the newest, in Slack too.
    let marked = RwSignal::new(String::new());
    let mp = place.clone();
    Effect::new(move || {
        let Some(Ok(v)) = messages.get() else { return };
        let Some(newest) = v[0]["meta"]["ts"].as_str().map(str::to_owned) else { return };
        if marked.get_untracked() == newest {
            return;
        }
        marked.set(newest.clone());
        let params = json!({"workspace": mp.workspace, "channel": mp.channel, "ts": newest});
        spawn_local(async move {
            let _ = api::rpc("memex.chat.mark_read", params).await;
        });
    });
    let name = place.name.clone();
    let shown = place.clone();
    view! {
        <header class="border-b border-zinc-200 px-4 py-2 text-sm font-medium dark:border-zinc-800">{name}</header>
        <div class="flex min-h-0 flex-1 flex-col-reverse overflow-y-auto">
            <div>
                {move || messages.get().map(|r| match r {
                    Err(e) => view! { <div class="p-3"><ErrorBox message=e /></div> }.into_any(),
                    Ok(v) => {
                        let mut list = v.as_array().cloned().unwrap_or_default();
                        list.reverse();
                        let place = shown.clone();
                        list.into_iter().map(|m| view! { <Message m place=place.clone() thread=Some(thread) tick /> }).collect_view().into_any()
                    }
                })}
            </div>
        </div>
        <Composer place thread_ts=None tick placeholder="Message (Enter to post, Shift+Enter for a new line)" />
    }
}

#[component]
fn Thread(place: Place, thread_ts: String, close: RwSignal<Option<String>>, tick: RwSignal<u32>) -> impl IntoView {
    let (p, t) = (place.clone(), thread_ts.clone());
    let messages = LocalResource::new(move || {
        tick.track();
        api::rpc("memex.chat.thread", json!({"workspace": p.workspace, "channel": p.channel, "thread_ts": t}))
    });
    let shown = place.clone();
    view! {
        <header class="flex items-center border-b border-zinc-200 px-4 py-2 text-sm font-medium dark:border-zinc-800">
            <span class="flex-1">"Thread"</span>
            <button class="text-xs text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100" on:click=move |_| close.set(None)>"✕"</button>
        </header>
        <div class="min-h-0 flex-1 overflow-y-auto">
            {move || messages.get().map(|r| match r {
                Err(e) => view! { <div class="p-3"><ErrorBox message=e /></div> }.into_any(),
                Ok(v) => {
                    let place = shown.clone();
                    v.as_array().cloned().unwrap_or_default().into_iter().map(|m| view! { <Message m place=place.clone() thread=None tick /> }).collect_view().into_any()
                }
            })}
        </div>
        <Composer place thread_ts=Some(thread_ts) tick placeholder="Reply in the thread" />
    }
}

/// Settings → Slack: your Slack app, and the workspaces you connect.
#[component]
pub fn SlackSettings() -> impl IntoView {
    let tick = RwSignal::new(0u32);
    let setup = LocalResource::new(move || {
        tick.track();
        api::slack_setup()
    });
    let error = RwSignal::new(None::<String>);
    let note = RwSignal::new(None::<String>);
    let (client_id, secret) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let run = move |f: std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<String>, String>>>>| {
        spawn_local(async move {
            match f.await {
                Ok(n) => {
                    error.set(None);
                    note.set(n);
                    tick.update(|t| *t += 1);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    // Back from signing in at Slack.
    if let Ok(search) = window().location().search()
        && let Some(v) = web_sys::UrlSearchParams::new_with_str(&search).ok().and_then(|p| p.get("oauth"))
    {
        if v == "connected" {
            note.set(Some("Connected. Its conversations arrive as they're imported.".into()))
        } else {
            error.set(Some(v))
        }
    }
    view! {
        <section class="space-y-4 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <h2 class="font-medium">"Slack"</h2>
            <p class="text-sm text-zinc-500">"Every conversation you're in (channels, private channels, DMs and groups), with its threads and shared documents, kept in this realm's memex and searchable; the Chat page to read and post. It uses your own Slack app, which keeps Slack's normal limits (apps from outside its directory are slowed to a crawl)."</p>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || note.get().map(|n| view! { <p class="rounded-md bg-emerald-50 px-3 py-2 text-sm text-emerald-900 dark:bg-emerald-950 dark:text-emerald-200">{n}</p> })}
            {move || setup.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(s) => {
                    let manifest = s.manifest.clone();
                    let copy = manifest.clone();
                    view! {
                        // 1. The app.
                        <div class="space-y-2">
                            <h3 class="text-sm font-medium">"1. The slack app"</h3>
                            {match (s.installed, s.waiting) {
                                (true, _) => view! { <p class="text-sm text-zinc-500">"Installed in this realm."</p> }.into_any(),
                                (false, true) => view! {
                                    <p class="text-sm text-zinc-500">"Waiting for your approval in the console's "
                                        <a rel="external" class="underline" href="/admin/decisions">"Decisions"</a> "."</p>
                                }.into_any(),
                                (false, false) => view! {
                                    <button class=BUTTON on:click=move |_| run(Box::pin(async { api::slack_install().await.map(|_| Some("Asked: approve it in the console's Decisions.".into())) }))>"Install the slack app"</button>
                                }.into_any(),
                            }}
                        </div>
                        // 2. Your Slack app.
                        <div class="space-y-2">
                            <h3 class="text-sm font-medium">"2. Your Slack app"</h3>
                            <ol class="list-decimal space-y-1 pl-5 text-sm text-zinc-600 dark:text-zinc-400">
                                <li>"Open " <a class="underline" href="https://api.slack.com/apps?new_app=1" target="_blank" rel="noopener">"api.slack.com/apps"</a> ", choose " <b>"From a manifest"</b> " and your workspace."</li>
                                <li>"Paste this manifest (it asks for your user's scopes, and names this head to come back to), and create the app."</li>
                                <li>"From its " <b>"Basic Information"</b> ", copy the Client ID and Client Secret here."</li>
                            </ol>
                            {(!s.https).then(|| view! {
                                <p class="rounded-md bg-amber-50 px-3 py-2 text-xs text-amber-900 dark:bg-amber-950 dark:text-amber-200">
                                    {format!("Slack only sends you back to an HTTPS address, and this one is {}. Open carbon6 at its HTTPS address (or set the public URL in the console) and come back here.", s.redirect_uri)}
                                </p>
                            })}
                            <div class="relative">
                                <pre class="max-h-56 overflow-auto rounded-md bg-zinc-100 p-3 font-mono text-[11px] dark:bg-zinc-900">{manifest}</pre>
                                <button class=format!("{BUTTON_QUIET} absolute right-2 top-2 text-xs") on:click=move |_| {
                                    let _ = window().navigator().clipboard().write_text(&copy);
                                    note.set(Some("Manifest copied.".into()));
                                }>"Copy"</button>
                            </div>
                            <div class="grid gap-2 sm:grid-cols-2">
                                <input class=INPUT placeholder=if s.client_id.is_empty() { "Client ID".to_owned() } else { s.client_id.clone() }
                                    prop:value=move || client_id.get() on:input=move |ev| client_id.set(event_target_value(&ev)) />
                                <input class=INPUT type="password" placeholder=if s.has_secret { "Client Secret (kept)" } else { "Client Secret" }
                                    prop:value=move || secret.get() on:input=move |ev| secret.set(event_target_value(&ev)) />
                            </div>
                            <button class=BUTTON_QUIET on:click=move |_| {
                                let (id, sec) = (client_id.get_untracked(), secret.get_untracked());
                                run(Box::pin(async move { api::slack_client(id, Some(sec).filter(|s| !s.is_empty())).await.map(|_| Some("Saved.".into())) }));
                            }>"Save the client"</button>
                        </div>
                        // 3. Workspaces.
                        <div class="space-y-2">
                            <h3 class="text-sm font-medium">"3. Workspaces"</h3>
                            {s.workspaces.is_empty().then(|| view! { <p class="text-sm text-zinc-500">"None connected yet."</p> })}
                            {s.workspaces.into_iter().map(|w| {
                                let id = w.id.clone();
                                let progress = if w.conversations > 0 && w.imported == w.conversations {
                                    format!("{} conversations, all imported", w.conversations)
                                } else {
                                    format!("importing: {} of {} conversations", w.imported, w.conversations)
                                };
                                view! {
                                    <div class="flex flex-wrap items-center gap-2 rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-800">
                                        <span class="font-medium">{w.name.clone()}</span>
                                        <span class="text-zinc-500">{format!("{progress} · {} people", w.people)}</span>
                                        {(w.files_waiting > 0).then(|| view! { <span class="text-zinc-500">{format!("· {} files waiting", w.files_waiting)}</span> })}
                                        {w.retry_after.filter(|t| *t > now_ms()).map(|_| view! { <span class="text-amber-600">"· Slack asked to slow down"</span> })}
                                        {w.synced_at.map(|t| view! { <span class="text-xs text-zinc-400">{format!("synced {}", ago(now_ms() - t))}</span> })}
                                        <span class="flex-1"></span>
                                        <button class=format!("{BUTTON_QUIET} text-xs text-red-600") on:click=move |_| {
                                            let id = id.clone();
                                            if window().confirm_with_message("Disconnect this workspace? Its sync stops; what it brought stays.").unwrap_or(false) {
                                                run(Box::pin(async move { api::slack_remove(id).await.map(|_| None) }));
                                            }
                                        }>"Disconnect"</button>
                                        {w.error.map(|e| view! { <p class="w-full text-xs text-red-600">{e}</p> })}
                                    </div>
                                }
                            }).collect_view()}
                            <div class="flex gap-2">
                                <button class=BUTTON disabled=!s.installed || !s.has_secret on:click=move |_| {
                                    spawn_local(async move {
                                        match api::slack_connect().await {
                                            Ok(url) => { let _ = window().location().set_href(&url); }
                                            Err(e) => error.set(Some(e)),
                                        }
                                    });
                                }>"Connect a workspace"</button>
                                <button class=BUTTON_QUIET disabled=!s.installed on:click=move |_| run(Box::pin(async { api::slack_sync().await.map(|_| Some("Syncing.".into())) }))>"Sync now"</button>
                            </div>
                            <p class="text-xs text-zinc-500">"Agents see chat only in sessions whose labels a Memex scopes rule gives the kind chat (Settings → Memex scopes); their posts wait for your approval."</p>
                        </div>
                    }.into_any()
                }
            })}
        </section>
    }
}

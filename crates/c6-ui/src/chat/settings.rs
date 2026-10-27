//! Settings → Slack: your Slack app, and the workspaces you connect.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, ago, now_ms};

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

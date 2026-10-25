//! Mail: your Gmail accounts through the memex. Views (inbox, needs reply,
//! waiting, …), conversations, compose with undo and send later; and the
//! Google settings.

use c6_api::{MailMessage, MailThread, Outgoing};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT};

const VIEWS: [(&str, &str); 9] = [
    ("inbox", "Inbox"),
    ("needs_reply", "Needs reply"),
    ("waiting", "Waiting"),
    ("starred", "Starred"),
    ("sent", "Sent"),
    ("drafts", "Drafts"),
    ("archive", "Archive"),
    ("spam", "Spam"),
    ("trash", "Trash"),
];

pub(crate) fn when(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    let today = js_sys::Date::new_0();
    if d.to_date_string() == today.to_date_string() {
        String::from(d.to_locale_time_string("default"))
    } else {
        String::from(d.to_locale_date_string("default", &wasm_bindgen::JsValue::UNDEFINED))
    }
}

/// Who and what, for a reply.
#[derive(Clone, Default)]
struct Draft {
    account: String,
    to: String,
    cc: String,
    subject: String,
    body: String,
    reply_to: Option<String>,
}

#[component]
pub fn MailPage() -> impl IntoView {
    let view = RwSignal::new("inbox".to_owned());
    let refresh = RwSignal::new(0u32);
    let threads = LocalResource::new(move || {
        let (v, _) = (view.get(), refresh.get());
        async move { api::rpc_as::<Vec<MailThread>>("memex.mail.threads", json!({"view": v, "limit": 60})).await }
    });
    let labels = LocalResource::new(|| api::rpc_as::<Vec<(String, String, String)>>("memex.mail.labels", json!({})));
    let open = RwSignal::new(None::<(String, String)>);
    let compose = RwSignal::new(None::<Draft>);
    let status = RwSignal::new(None::<Result<String, String>>);
    view! {
        <main class="mx-auto flex max-w-6xl flex-col gap-3 px-3 py-3 sm:flex-row">
            <nav class="flex shrink-0 gap-1 overflow-x-auto text-sm sm:w-44 sm:flex-col">
                <button class=format!("{BUTTON} mb-1 shrink-0") on:click=move |_| compose.set(Some(Draft::default()))>"Compose"</button>
                {VIEWS.iter().map(|(v, label)| {
                    let v = v.to_string();
                    let v2 = v.clone();
                    view! {
                        <button class=move || format!("shrink-0 rounded-md px-2 py-1 text-left {}", if view.get() == v { "bg-zinc-200 font-medium dark:bg-zinc-800" } else { "text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-900" })
                            on:click=move |_| { view.set(v2.clone()); open.set(None); }>{*label}</button>
                    }
                }).collect_view()}
                {move || labels.get().and_then(Result::ok).filter(|l| !l.is_empty()).map(|ls| view! {
                    <p class="mt-2 hidden px-2 text-[10px] uppercase tracking-wide text-zinc-400 sm:block">"Labels"</p>
                    {ls.into_iter().map(|(_, id, name)| {
                        let key = format!("label:{id}");
                        let k2 = key.clone();
                        view! {
                            <button class=move || format!("shrink-0 truncate rounded-md px-2 py-1 text-left {}", if view.get() == key { "bg-zinc-200 dark:bg-zinc-800" } else { "text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-900" })
                                on:click=move |_| { view.set(k2.clone()); open.set(None); }>{name}</button>
                        }
                    }).collect_view()}
                })}
            </nav>
            <section class="min-w-0 flex-1 space-y-2">
                <Outbox refresh />
                {move || status.get().map(|r| match r {
                    Ok(m) => view! { <p class="text-sm text-emerald-600">{m}</p> }.into_any(),
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                })}
                {move || compose.get().map(|d| view! { <Compose d compose status refresh /> })}
                {move || match open.get() {
                    Some((account, thread)) => view! { <Thread account thread open compose refresh /> }.into_any(),
                    None => view! {
                        <Suspense fallback=|| view! { <p class="text-sm text-zinc-500">"Loading…"</p> }>
                            {move || threads.get().map(|r| match r {
                                Err(e) => view! { <MailError e /> }.into_any(),
                                Ok(v) if v.is_empty() => view! { <p class="p-4 text-center text-sm text-zinc-500">"Nothing here."</p> }.into_any(),
                                Ok(v) => view! {
                                    <div class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                                        {v.into_iter().map(|t| {
                                            let key = (t.account.clone(), t.thread_id.clone());
                                            view! {
                                                <button class=format!("block w-full px-3 py-2 text-left hover:bg-zinc-50 dark:hover:bg-zinc-900 {}", if t.unread { "font-semibold" } else { "" })
                                                    on:click=move |_| open.set(Some(key.clone()))>
                                                    <div class="flex items-baseline gap-2 text-sm">
                                                        <span class="min-w-0 flex-1 truncate">{t.people.join(", ")}{(t.count > 1).then(|| format!(" ({})", t.count))}</span>
                                                        {t.starred.then(|| view! { <span class="text-amber-500">"★"</span> })}
                                                        {t.attachments.then(|| view! { <span class="text-zinc-400">"📎"</span> })}
                                                        <span class="shrink-0 text-xs font-normal text-zinc-500">{when(t.date)}</span>
                                                    </div>
                                                    <div class="truncate text-sm">{t.subject.clone()}</div>
                                                    <div class="truncate text-xs font-normal text-zinc-500">{t.snippet.clone()}</div>
                                                </button>
                                            }
                                        }).collect_view()}
                                    </div>
                                }.into_any(),
                            })}
                        </Suspense>
                    }.into_any(),
                }}
            </section>
        </main>
    }
}

/// When the mail calls fail (no memex, no account): what to do.
#[component]
fn MailError(e: String) -> impl IntoView {
    let hint = if e.contains("no memex") || e.contains("no Google") || e.contains("email is outside") {
        "Connect a memex (Settings → Memex) and a Google account (Settings → Mail & calendar)."
    } else {
        ""
    };
    view! { <ErrorBox message=format!("{e} {hint}") /> }
}

/// Messages on their way out, with Undo while they wait.
#[component]
fn Outbox(refresh: RwSignal<u32>) -> impl IntoView {
    let list = LocalResource::new(move || {
        let _ = refresh.get();
        api::rpc_as::<Vec<Outgoing>>("memex.mail.outgoing", json!({}))
    });
    let tick = set_interval_with_handle(move || list.refetch(), std::time::Duration::from_secs(3)).ok();
    on_cleanup(move || {
        if let Some(t) = tick {
            t.clear();
        }
    });
    view! {
        {move || list.get().and_then(Result::ok).map(|v| v.into_iter().filter(|o| matches!(o.status.as_str(), "pending" | "sending" | "failed")).map(|o| {
            let id = o.id.clone();
            let secs = ((o.send_at - js_sys::Date::now() as i64) / 1000).max(0);
            view! {
                <div class="flex items-center gap-2 rounded-md bg-zinc-100 px-3 py-1.5 text-sm dark:bg-zinc-900">
                    <span class="min-w-0 flex-1 truncate">{match o.status.as_str() {
                        "failed" => format!("Not sent: {} ({})", o.subject, o.error.clone().unwrap_or_default()),
                        "sending" => format!("Sending {}…", o.subject),
                        _ if secs > 60 => format!("{} goes out at {}", o.subject, when(o.send_at)),
                        _ => format!("Sending {} in {secs}s", o.subject),
                    }}</span>
                    {(o.status == "pending").then(|| view! {
                        <button class=BUTTON_QUIET on:click=move |_| {
                            let id = id.clone();
                            spawn_local(async move {
                                let _ = api::rpc("memex.mail.cancel", json!({"id": id})).await;
                                list.refetch();
                            });
                        }>"Undo"</button>
                    })}
                </div>
            }
        }).collect_view())}
    }
}

#[component]
fn Thread(
    account: String,
    thread: String,
    open: RwSignal<Option<(String, String)>>,
    compose: RwSignal<Option<Draft>>,
    refresh: RwSignal<u32>,
) -> impl IntoView {
    let (acc, th) = (StoredValue::new(account.clone()), StoredValue::new(thread.clone()));
    let messages = LocalResource::new(move || {
        api::rpc_as::<Vec<MailMessage>>("memex.mail.thread", json!({"account": acc.get_value(), "thread_id": th.get_value()}))
    });
    let error = RwSignal::new(None::<String>);
    let modify = move |add: Vec<&'static str>, remove: Vec<&'static str>, close: bool| {
        spawn_local(async move {
            match api::rpc(
                "memex.mail.modify",
                json!({"account": acc.get_value(), "thread_id": th.get_value(), "add": add, "remove": remove}),
            )
            .await
            {
                Ok(_) => {
                    refresh.update(|n| *n += 1);
                    if close {
                        open.set(None);
                    } else {
                        messages.refetch();
                    }
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    // Opening it reads it.
    Effect::new(move || {
        if let Some(Ok(m)) = messages.get()
            && m.iter().any(|x| x.unread)
        {
            spawn_local(async move {
                let _ =
                    api::rpc("memex.mail.modify", json!({"account": acc.get_value(), "thread_id": th.get_value(), "remove": ["UNREAD"]}))
                        .await;
                refresh.update(|n| *n += 1);
            });
        }
    });
    view! {
        <section class="space-y-3">
            <div class="flex flex-wrap items-center gap-1 text-sm">
                <button class=BUTTON_QUIET on:click=move |_| open.set(None)>"← Back"</button>
                <span class="flex-1"></span>
                <button class=BUTTON_QUIET on:click=move |_| modify(vec![], vec!["INBOX"], true)>"Archive"</button>
                <button class=BUTTON_QUIET on:click=move |_| modify(vec!["INBOX"], vec![], false)>"Move to inbox"</button>
                <button class=BUTTON_QUIET on:click=move |_| modify(vec!["STARRED"], vec![], false)>"Star"</button>
                <button class=BUTTON_QUIET on:click=move |_| modify(vec!["UNREAD"], vec![], true)>"Mark unread"</button>
                <button class=format!("{BUTTON_QUIET} text-red-600") on:click=move |_| modify(vec!["TRASH"], vec!["INBOX"], true)>"Trash"</button>
            </div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || messages.get().map(|r| match r {
                Err(e) => view! { <MailError e /> }.into_any(),
                Ok(list) => {
                    let subject = list.last().map(|m| m.subject.clone()).unwrap_or_default();
                    let last = list.last().cloned();
                    view! {
                        <h1 class="text-lg font-semibold">{subject}</h1>
                        {list.into_iter().map(|m| view! { <Message m /> }).collect_view()}
                        {last.map(|m| {
                            let (m1, m2) = (m.clone(), m.clone());
                            let account = acc.get_value();
                            let a2 = account.clone();
                            view! {
                                <div class="flex gap-2">
                                    <button class=BUTTON on:click=move |_| compose.set(Some(reply(&a2, &m1, false)))>"Reply"</button>
                                    <button class=BUTTON_QUIET on:click=move |_| compose.set(Some(reply(&account, &m2, true)))>"Reply all"</button>
                                </div>
                            }
                        })}
                    }.into_any()
                }
            })}
        </section>
    }
}

fn reply(account: &str, m: &MailMessage, all: bool) -> Draft {
    let subject = if m.subject.to_lowercase().starts_with("re:") { m.subject.clone() } else { format!("Re: {}", m.subject) };
    let mut to = vec![m.from.clone()];
    let mut cc = Vec::new();
    if all {
        to.extend(m.to.iter().map(|(a, _)| a.clone()).filter(|a| a != account));
        cc.extend(m.cc.iter().map(|(a, _)| a.clone()).filter(|a| a != account));
    }
    let quoted: String = m.text.lines().map(|l| format!("> {l}\n")).collect();
    Draft {
        account: account.to_owned(),
        to: to.join(", "),
        cc: cc.join(", "),
        subject,
        body: format!("\n\nOn {}, {} wrote:\n{quoted}", when(m.date), if m.from_name.is_empty() { &m.from } else { &m.from_name }),
        reply_to: Some(m.item_id.clone()),
    }
}

#[component]
fn Message(m: MailMessage) -> impl IntoView {
    let images = RwSignal::new(false);
    let html = m.html.clone();
    let item = m.item_id.clone();
    let has_html = html.is_some();
    view! {
        <article class="space-y-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
            <div class="flex flex-wrap items-baseline gap-2 text-sm">
                <span class="font-medium">{if m.from_name.is_empty() { m.from.clone() } else { m.from_name.clone() }}</span>
                <span class="text-xs text-zinc-500">{format!("<{}> to {}", m.from, m.to.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>().join(", "))}</span>
                <span class="flex-1"></span>
                <span class="text-xs text-zinc-500">{when(m.date)}</span>
            </div>
            {match html {
                Some(h) => view! {
                    <div class="space-y-1">
                        // Sandboxed: no scripts, an opaque origin; remote
                        // images only when you allow them.
                        <iframe class="h-96 w-full rounded border border-zinc-100 bg-white dark:border-zinc-800" sandbox="allow-popups allow-popups-to-escape-sandbox"
                            srcdoc=move || format!("<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data: {}\"><base target=\"_blank\">{h}",
                                if images.get() { "https: http:" } else { "" })></iframe>
                        {move || (!images.get()).then(|| view! {
                            <button class="text-xs text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| images.set(true)>"Show images (lets the sender see you opened it)"</button>
                        })}
                    </div>
                }.into_any(),
                None => view! { <pre class="whitespace-pre-wrap font-sans text-sm">{m.text.clone()}</pre> }.into_any(),
            }}
            {(!m.attachments.is_empty()).then(|| view! {
                <div class="flex flex-wrap gap-2 text-xs">
                    {m.attachments.iter().map(|a| view! {
                        <a class="rounded bg-zinc-100 px-2 py-1 hover:underline dark:bg-zinc-800" target="_blank" rel="noopener"
                            href=api::mail_attachment(&item, a.index)>{format!("📎 {} ({} KB)", a.name, a.size / 1024)}</a>
                    }).collect_view()}
                </div>
            })}
            {(!has_html && m.text.is_empty()).then(|| view! { <p class="text-xs text-zinc-500">"(no text)"</p> })}
        </article>
    }
}

#[component]
fn Compose(
    d: Draft,
    compose: RwSignal<Option<Draft>>,
    status: RwSignal<Option<Result<String, String>>>,
    refresh: RwSignal<u32>,
) -> impl IntoView {
    let accounts = LocalResource::new(api::google_accounts);
    let (account, to, cc, subject, body) =
        (RwSignal::new(d.account), RwSignal::new(d.to), RwSignal::new(d.cc), RwSignal::new(d.subject), RwSignal::new(d.body));
    let later = RwSignal::new(String::new());
    let reply_to = StoredValue::new(d.reply_to);
    let draft = move || {
        let split = |s: String| s.split(',').map(|x| x.trim().to_owned()).filter(|x| !x.is_empty()).collect::<Vec<_>>();
        let send_at = js_sys::Date::parse(&later.get_untracked());
        json!({"account": account.get_untracked(), "to": split(to.get_untracked()), "cc": split(cc.get_untracked()), "subject": subject.get_untracked(),
               "body": body.get_untracked(), "reply_to": reply_to.get_value(), "send_at": (!send_at.is_nan()).then_some(send_at as i64)})
    };
    let send = move |_| {
        let d = draft();
        spawn_local(async move {
            match api::rpc("memex.mail.send", d).await {
                Ok(_) => {
                    compose.set(None);
                    status.set(None);
                    refresh.update(|n| *n += 1);
                }
                Err(e) => status.set(Some(Err(e))),
            }
        });
    };
    let save = move |_| {
        let d = draft();
        spawn_local(async move {
            match api::rpc("memex.mail.draft", d).await {
                Ok(_) => {
                    compose.set(None);
                    status.set(Some(Ok("Saved to Drafts.".into())));
                }
                Err(e) => status.set(Some(Err(e))),
            }
        });
    };
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-300 p-3 shadow-sm dark:border-zinc-700">
            <div class="grid gap-2 sm:grid-cols-2">
                <select class=format!("{INPUT} py-1 text-sm") prop:value=move || account.get() on:change=move |ev| account.set(event_target_value(&ev))>
                    <option value="">"From…"</option>
                    {move || accounts.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|a| view! { <option value=a.email.clone()>{a.email.clone()}</option> }).collect_view()}
                </select>
                <input class=format!("{INPUT} py-1 text-sm") placeholder="Send later (e.g. 2026-10-14 08:00)" prop:value=move || later.get() on:input=move |ev| later.set(event_target_value(&ev)) />
                <input class=format!("{INPUT} py-1 text-sm sm:col-span-2") placeholder="To" prop:value=move || to.get() on:input=move |ev| to.set(event_target_value(&ev)) />
                <input class=format!("{INPUT} py-1 text-sm sm:col-span-2") placeholder="Cc" prop:value=move || cc.get() on:input=move |ev| cc.set(event_target_value(&ev)) />
                <input class=format!("{INPUT} py-1 text-sm sm:col-span-2") placeholder="Subject" prop:value=move || subject.get() on:input=move |ev| subject.set(event_target_value(&ev)) />
            </div>
            <textarea class=format!("{INPUT} min-h-48 text-sm") placeholder="Write (Markdown)" prop:value=move || body.get() on:input=move |ev| body.set(event_target_value(&ev))></textarea>
            <div class="flex justify-end gap-2">
                <button class=BUTTON_QUIET on:click=move |_| compose.set(None)>"Discard"</button>
                <button class=BUTTON_QUIET on:click=save>"Save draft"</button>
                <button class=BUTTON on:click=send>{move || if later.get().trim().is_empty() { "Send" } else { "Send later" }}</button>
            </div>
        </section>
    }
}

/// Settings → Mail & calendar: your Google OAuth client, accounts and how
/// their sync is going.
#[component]
pub fn GoogleSettings() -> impl IntoView {
    let client = LocalResource::new(api::google_client);
    let accounts = LocalResource::new(api::google_accounts);
    let sync = LocalResource::new(|| api::rpc("memex.google", json!({})));
    let (id, secret) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    Effect::new(move || {
        if let Some(Ok(c)) = client.get() {
            id.set(c.client_id);
        }
    });
    let note = RwSignal::new(None::<Result<String, String>>);
    let save = move |_| {
        let s = Some(secret.get_untracked()).filter(|s| !s.trim().is_empty());
        spawn_local(async move {
            note.set(Some(api::set_google_client(id.get_untracked(), s).await.map(|_| "Saved.".to_owned())));
            secret.set(String::new());
            client.refetch();
        });
    };
    let connect = move |_| {
        spawn_local(async move {
            match api::google_connect().await {
                Ok(url) => {
                    let _ = window().location().set_href(&url);
                }
                Err(e) => note.set(Some(Err(e))),
            }
        });
    };
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <div>
                <h2 class="font-medium">"Mail & calendar"
                    <span class="ml-2 text-sm font-normal text-zinc-500">{move || {
                        let r = api::realm().get();
                        format!("in {} (pick a realm in the header for another's)", if r.is_empty() { "home".to_owned() } else { r })
                    }}</span>
                </h2>
                <p class="text-sm text-zinc-500">"Your Gmail and Google Workspace accounts: mail, calendars and contacts, synced both ways by the apps gmail, google-calendar and google-contacts, in each realm you install them in, with that realm's accounts and mail (they run on a worker with Docker or Podman, reaching only Google). Your OAuth client is every realm's. Google needs an OAuth client from your Google Cloud project: enable the Gmail, Calendar and People APIs, create an OAuth client of type Web application with the redirect URI below, and set the app to In production (personal Gmail: otherwise sign-ins expire weekly; you will see Google's unverified-app notice once) or Internal (Workspace)."</p>
            </div>
            {move || client.get().and_then(Result::ok).filter(|c| c.apps.len() < 3).map(|c| {
                let waiting = !c.waiting.is_empty();
                view! {
                    <div class="flex flex-wrap items-center gap-2 rounded-md bg-amber-50 p-2 text-sm dark:bg-amber-950">
                        <span class="flex-1">{if waiting {
                            "The Google apps wait for your approval (the console's Decisions).".to_owned()
                        } else if c.apps.is_empty() {
                            "Install the Gmail, Calendar and Contacts apps to connect accounts.".to_owned()
                        } else {
                            format!("Installed: {}. Install the others too?", c.apps.join(", "))
                        }}</span>
                        {(!waiting).then(|| view! {
                            <button class=BUTTON on:click=move |_| spawn_local(async move {
                                note.set(Some(api::google_install().await.map(|_| "Approve each app in the console's Decisions.".to_owned())));
                                client.refetch();
                            })>"Install the apps"</button>
                        })}
                    </div>
                }
            })}
            {move || client.get().and_then(Result::ok).map(|c| view! {
                <p class="text-xs text-zinc-500">"Redirect URI: " <code class="select-all font-mono">{c.redirect_uri.clone()}</code></p>
            })}
            <div class="grid gap-2 sm:grid-cols-2">
                <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="Client ID" prop:value=move || id.get() on:input=move |ev| id.set(event_target_value(&ev)) />
                <input class=format!("{INPUT} py-1 font-mono text-xs") type="password"
                    placeholder=move || if client.get().and_then(Result::ok).is_some_and(|c| c.has_secret) { "Client secret (saved; type to replace)" } else { "Client secret" }
                    prop:value=move || secret.get() on:input=move |ev| secret.set(event_target_value(&ev)) />
            </div>
            <div class="flex flex-wrap items-center justify-end gap-2">
                {move || note.get().map(|r| match r {
                    Ok(m) => view! { <span class="text-sm text-emerald-600">{m}</span> }.into_any(),
                    Err(e) => view! { <span class="text-sm text-red-600">{e}</span> }.into_any(),
                })}
                <button class=BUTTON_QUIET on:click=save>"Save client"</button>
                <button class=BUTTON on:click=connect>"Add Google account"</button>
            </div>
            {move || accounts.get().and_then(Result::ok).filter(|a| !a.is_empty()).map(|list| {
                let states: Vec<serde_json::Value> = sync.get().and_then(Result::ok).and_then(|v| v["sync"].as_array().cloned()).unwrap_or_default();
                view! {
                    <div class="divide-y divide-zinc-100 text-sm dark:divide-zinc-800">
                        {list.into_iter().map(|a| {
                            let id = a.id.clone();
                            let mine: Vec<String> = states.iter().filter(|s| s["email"] == a.email.as_str()).map(|s| {
                                let kind = s["kind"].as_str().unwrap_or("").trim_start_matches("cal:").to_owned();
                                let err = s["error"].as_str().map(|e| format!(" ({e})")).unwrap_or_default();
                                format!("{kind}: {} {}{err}", s["phase"].as_str().unwrap_or(""), if s["done"].as_i64().unwrap_or(0) > 0 { format!("· {}", s["done"]) } else { String::new() })
                            }).collect();
                            view! {
                                <div class="flex flex-wrap items-center gap-2 py-1.5">
                                    <span class="font-medium">{a.email.clone()}</span>
                                    <span class="min-w-0 flex-1 truncate text-xs text-zinc-500" title=mine.join("\n")>{mine.join(" · ")}</span>
                                    <button class="text-xs text-red-600 hover:underline" on:click=move |_| {
                                        if !window().confirm_with_message("Disconnect this account from the Google apps? (What is in the memex stays.)").unwrap_or(false) { return; }
                                        let id = id.clone();
                                        spawn_local(async move { let _ = api::google_remove(id).await; accounts.refetch(); });
                                    }>"Disconnect"</button>
                                </div>
                            }
                        }).collect_view()}
                        <button class="mt-1 text-xs text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| {
                            spawn_local(async move { let _ = api::rpc("memex.google_sync", json!({})).await; sync.refetch(); });
                        }>"Sync now"</button>
                    </div>
                }
            })}
        </section>
    }
}

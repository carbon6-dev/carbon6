//! Mail: your Gmail accounts through the memex. Views (inbox, needs reply,
//! waiting, …), conversations, compose with undo and send later; and the
//! Google settings.

use c6_api::{MailMessage, MailThread, Outgoing};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT};

const VIEWS: [(&str, &str); 12] = [
    ("inbox", "Inbox"),
    ("triage:needs_you", "Needs you"),
    ("triage:first_contact", "First contact"),
    ("triage:pitch", "Cold pitches"),
    ("needs_reply", "Needs reply"),
    ("waiting", "Waiting"),
    ("starred", "Starred"),
    ("sent", "Sent"),
    ("drafts", "Drafts"),
    ("archive", "Archive"),
    ("spam", "Spam"),
    ("trash", "Trash"),
];

/// How a conversation was sorted for the brief, as a short label and its
/// colour (docs/brief.md).
pub(crate) fn sorted_as(category: &str) -> (&'static str, &'static str) {
    match category {
        "needs_you" => ("Needs you", "bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-300"),
        "first_contact" => ("First contact", "bg-sky-100 text-sky-800 dark:bg-sky-950 dark:text-sky-300"),
        "pitch" => ("Cold pitch", "bg-zinc-200 text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400"),
        "newsletter" => ("Newsletter", "bg-zinc-100 text-zinc-500 dark:bg-zinc-900 dark:text-zinc-500"),
        "notification" => ("Notification", "bg-zinc-100 text-zinc-500 dark:bg-zinc-900 dark:text-zinc-500"),
        "fyi" => ("FYI", "bg-zinc-100 text-zinc-600 dark:bg-zinc-900 dark:text-zinc-400"),
        "noise" => ("Noise", "bg-zinc-100 text-zinc-500 dark:bg-zinc-900 dark:text-zinc-500"),
        "yours" => ("You wrote last", "bg-zinc-100 text-zinc-500 dark:bg-zinc-900 dark:text-zinc-500"),
        _ => ("", ""),
    }
}

/// Threads a page.
const PAGE: u32 = 50;

/// Calls `f` whenever `el` comes within a screen of being seen.
fn when_near(el: &web_sys::Element, f: impl Fn() + 'static) -> web_sys::IntersectionObserver {
    use wasm_bindgen::JsCast;
    let cb = wasm_bindgen::closure::Closure::<dyn Fn(js_sys::Array)>::new(move |entries: js_sys::Array| {
        if entries.iter().any(|e| e.unchecked_into::<web_sys::IntersectionObserverEntry>().is_intersecting()) {
            f();
        }
    });
    let opts = web_sys::IntersectionObserverInit::new();
    opts.set_root_margin("600px");
    let o = web_sys::IntersectionObserver::new_with_options(cb.as_ref().unchecked_ref(), &opts).expect("an observer");
    cb.forget();
    o.observe(el);
    o
}

/// Who and what, for a reply.
#[derive(Clone, Default)]
pub(crate) struct Draft {
    /// Its reply box (docs/conversations.md §3): the thread it answers, else
    /// a new message's own.
    key: String,
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
    // The view's threads so far, a page at a time as you scroll (by the
    // last one's date); a new view or a refresh starts again.
    let threads = RwSignal::new(None::<Result<Vec<MailThread>, String>>);
    let (busy, done, era) = (RwSignal::new(false), RwSignal::new(false), StoredValue::new(0u32));
    let more = move || {
        if busy.get_untracked() || done.get_untracked() {
            return;
        }
        let before = threads.with_untracked(|t| t.as_ref().and_then(|r| r.as_ref().ok()).and_then(|v| v.last()).map(|t| t.date));
        let (mine, v) = (era.get_value(), view.get_untracked());
        busy.set(true);
        spawn_local(async move {
            let r = api::rpc_as::<Vec<MailThread>>("memex.mail.threads", json!({"view": v, "limit": PAGE, "before": before})).await;
            if era.get_value() != mine {
                return;
            }
            busy.set(false);
            match r {
                Ok(page) => {
                    done.set((page.len() as u32) < PAGE);
                    threads.update(|t| match t {
                        Some(Ok(v)) => v.extend(page),
                        _ => *t = Some(Ok(page)),
                    });
                }
                Err(e) => {
                    done.set(true);
                    if before.is_none() {
                        threads.set(Some(Err(e)));
                    }
                }
            }
        });
    };
    Effect::new(move || {
        view.track();
        refresh.track();
        era.update_value(|e| *e += 1);
        busy.set(false);
        done.set(false);
        threads.set(None);
        more();
    });
    let end = NodeRef::<leptos::html::Div>::new();
    Effect::new(move || {
        if let Some(el) = end.get() {
            let o = when_near(&el, more);
            on_cleanup(move || o.disconnect());
        }
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
                        <div>
                            {move || match threads.get() {
                                None => Some(view! { <p class="text-sm text-zinc-500">"Loading…"</p> }.into_any()),
                                Some(r) => Some(match r {
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
                                                        {t.triage.as_ref().filter(|x| !matches!(x.category.as_str(), "yours" | "fyi")).map(|x| {
                                                            let (label, class) = sorted_as(&x.category);
                                                            view! { <span class=format!("shrink-0 rounded px-1.5 text-[10px] font-medium {class}") title=x.reason.clone()>{label}</span> }
                                                        })}
                                                        {t.starred.then(|| view! { <span class="text-amber-500">"★"</span> })}
                                                        {t.attachments.then(|| view! { <span class="text-zinc-400">"📎"</span> })}
                                                        <span class="shrink-0 text-xs font-normal text-zinc-500">{crate::when::compact(t.date)}</span>
                                                    </div>
                                                    <div class="truncate text-sm">{t.subject.clone()}</div>
                                                    <div class="truncate text-xs font-normal text-zinc-500">{t.snippet.clone()}</div>
                                                </button>
                                            }
                                        }).collect_view()}
                                    </div>
                                    <div node_ref=end class="p-3 text-center text-xs text-zinc-500">
                                        {move || if busy.get() { "Loading more…" } else if done.get() { "That's all." } else { "" }}
                                    </div>
                                }.into_any(),
                            }),
                            }}
                        </div>
                    }.into_any(),
                }}
            </section>
        </main>
    }
}

/// How a thread was sorted for the brief, and putting it right: a pitch
/// (its sender's mail always is), not a pitch (they're someone you know),
/// or another category with a note the sorting learns from.
#[component]
fn Sorting(target: String, refresh: RwSignal<u32>, in_brief: bool) -> impl IntoView {
    let target = StoredValue::new(target);
    let sorted =
        LocalResource::new(move || api::rpc_as::<Option<c6_api::Triage>>("memex.triage.get", json!({"target": target.get_value()})));
    let (category, note) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let error = RwSignal::new(None::<String>);
    let act = move |method: &'static str, params: serde_json::Value| {
        spawn_local(async move {
            match api::rpc(method, params).await {
                Ok(_) => {
                    sorted.refetch();
                    refresh.update(|n| *n += 1);
                    note.set(String::new());
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    move || {
        sorted.get().and_then(Result::ok).flatten().map(|t| {
            let (label, class) = sorted_as(&t.category);
            let stranger = matches!(t.category.as_str(), "pitch" | "first_contact");
            let by = match t.by.as_str() {
                "model" => "the model",
                "you" => "you",
                "mark" => "your mark",
                "signals" | "sequence" => "a sequence's marks",
                _ => "a rule",
            };
            view! {
                <div class="flex flex-wrap items-center gap-2 rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-800">
                    <span class=format!("rounded px-1.5 text-xs font-medium {class}")>{label}</span>
                    <span class="min-w-0 flex-1 text-zinc-600 dark:text-zinc-400">{t.reason.clone()}<span class="text-xs text-zinc-400">{format!(" · by {by}")}</span></span>
                    {(t.kind == "mail" && t.category != "pitch" && t.known == Some(false)).then(|| view! {
                        <button class=BUTTON_QUIET title="Their mail is always a cold pitch"
                            on:click=move |_| act("memex.triage.mark", json!({"target": target.get_value(), "mark": "pitch"}))>"Cold pitch"</button>
                    })}
                    {(t.kind == "mail" && stranger).then(|| view! {
                        <button class=BUTTON_QUIET title="Someone you know: their mail is never a pitch"
                            on:click=move |_| act("memex.triage.mark", json!({"target": target.get_value(), "mark": "known"}))>"Not a pitch"</button>
                    })}
                    {t.request.map(|id| view! { <RequestActions id in_brief refresh=move || { sorted.refetch(); refresh.update(|n| *n += 1) } /> })}
                    <select class=format!("{INPUT} w-auto py-1 text-xs") on:change=move |ev| category.set(event_target_value(&ev))>
                        <option value="">"Sort as…"</option>
                        {["needs_you", "first_contact", "fyi", "newsletter", "notification", "pitch", "noise"].into_iter().map(|c| {
                            view! { <option value=c>{sorted_as(c).0}</option> }
                        }).collect_view()}
                    </select>
                    {move || (!category.get().is_empty()).then(|| view! {
                        <input class=format!("{INPUT} w-56 py-1 text-xs") placeholder="Why (the sorting learns from it)"
                            prop:value=move || note.get() on:input=move |ev| note.set(event_target_value(&ev)) />
                        <button class=BUTTON_QUIET on:click=move |_| {
                            act("memex.triage.set", json!({"target": target.get_value(), "category": category.get_untracked(), "note": note.get_untracked()}));
                            category.set(String::new());
                        }>"Save"</button>
                    })}
                    {move || error.get().map(|e| view! { <span class="w-full text-xs text-red-600">{e}</span> })}
                </div>
            }
        })
    }
}

/// A thread's open request, settled as in the Brief (the same actions),
/// with the way there.
#[component]
fn RequestActions(id: i64, in_brief: bool, refresh: impl Fn() + Copy + Send + Sync + 'static) -> impl IntoView {
    use crate::brief::Settle;
    let error = RwSignal::new(None::<String>);
    let act = move |how: Settle| {
        spawn_local(async move {
            match crate::brief::settle(id, how).await {
                Ok(_) => refresh(),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <span class="flex w-full flex-wrap items-center gap-1 sm:w-auto">
            <span class="text-xs text-zinc-500">"Request:"</span>
            <button class=BUTTON_QUIET on:click=move |_| act(Settle::Done)>"Done"</button>
            <button class=BUTTON_QUIET on:click=move |_| act(Settle::Tomorrow)>"Tomorrow"</button>
            <button class=format!("{BUTTON_QUIET} text-red-600") on:click=move |_| act(Settle::Drop)>"Drop"</button>
            {(!in_brief).then(|| view! {
                <crate::workspace::Go to=crate::workspace::View::Brief own=true brief_request=id class=format!("{BUTTON_QUIET} no-underline")>"Open in the Brief"</crate::workspace::Go>
            })}
            {move || error.get().map(|e| view! { <span class="w-full text-xs text-red-600">{e}</span> })}
        </span>
    }
}

/// The senders you marked (a pitch, or someone you know), to take back.
#[component]
pub fn SenderMarks() -> impl IntoView {
    let marks = LocalResource::new(|| api::rpc("memex.triage.marks", json!({})));
    move || {
        marks.get().and_then(Result::ok).and_then(|v| v.as_array().cloned()).filter(|l| !l.is_empty()).map(|list| view! {
            <section class="space-y-2 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
                <h2 class="font-medium">"Senders you marked"</h2>
                <p class="text-sm text-zinc-500">"A pitch: everything they send is sorted as a cold pitch. Known: nothing they send is."</p>
                <div class="divide-y divide-zinc-100 text-sm dark:divide-zinc-800">
                    {list.into_iter().map(|x| {
                        let pattern = x["pattern"].as_str().unwrap_or_default().to_owned();
                        let p = pattern.clone();
                        view! {
                            <div class="flex items-center gap-2 py-1.5">
                                <span class="min-w-0 flex-1 truncate font-mono text-xs">{pattern}</span>
                                <span class="text-xs text-zinc-500">{if x["mark"] == "pitch" { "cold pitch" } else { "known" }}</span>
                                <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| {
                                    let p = p.clone();
                                    spawn_local(async move {
                                        let _ = api::rpc("memex.triage.unmark", json!({"pattern": p})).await;
                                        marks.refetch();
                                    });
                                }>"Remove"</button>
                            </div>
                        }
                    }).collect_view()}
                </div>
            </section>
        })
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
                        _ if secs > 60 => format!("{} goes out at {}", o.subject, crate::when::compact(o.send_at)),
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
pub(crate) fn Thread(
    account: String,
    thread: String,
    open: RwSignal<Option<(String, String)>>,
    compose: RwSignal<Option<Draft>>,
    refresh: RwSignal<u32>,
    #[prop(optional)] in_brief: bool,
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
            <Sorting target=format!("thread:{account}:{thread}") refresh in_brief />
            {move || messages.get().map(|r| match r {
                Err(e) => view! { <MailError e /> }.into_any(),
                Ok(list) => {
                    let subject = list.last().map(|m| m.subject.clone()).unwrap_or_default();
                    let link = list.last().and_then(|m| m.link.clone());
                    let last = list.last().cloned();
                    // Who the people writing are to you (their cards), the latest first.
                    let mut senders: Vec<String> = Vec::new();
                    for m in list.iter().rev() {
                        let a = m.from.to_lowercase();
                        if a.contains('@') && !senders.contains(&a) {
                            senders.push(a);
                        }
                    }
                    view! {
                        <div class="flex flex-wrap items-baseline justify-between gap-x-2">
                            <h1 class="min-w-0 text-lg font-semibold">{subject}</h1>
                            {link.map(|link| view! { <crate::util::OpenIn link /> })}
                        </div>
                        {senders.into_iter().take(2).map(|a| view! { <crate::understanding::Cards params=serde_json::json!({"address": a}) /> }).collect_view()}
                        {list.into_iter().map(|m| view! { <Message m /> }).collect_view()}
                        {last.map(|m| {
                            let (m1, m2) = (m.clone(), m.clone());
                            let account = acc.get_value();
                            let a2 = account.clone();
                            view! {
                                <div class="flex gap-2">
                                    <button class=BUTTON on:click=move |_| compose.set(Some(reply(&a2, &th.get_value(), &m1, false)))>"Reply"</button>
                                    <button class=BUTTON_QUIET on:click=move |_| compose.set(Some(reply(&account, &th.get_value(), &m2, true)))>"Reply all"</button>
                                </div>
                            }
                        })}
                    }.into_any()
                }
            })}
        </section>
    }
}

fn reply(account: &str, thread: &str, m: &MailMessage, all: bool) -> Draft {
    let subject = if m.subject.to_lowercase().starts_with("re:") { m.subject.clone() } else { format!("Re: {}", m.subject) };
    let mut to = vec![m.from.clone()];
    let mut cc = Vec::new();
    if all {
        to.extend(m.to.iter().map(|(a, _)| a.clone()).filter(|a| a != account));
        cc.extend(m.cc.iter().map(|(a, _)| a.clone()).filter(|a| a != account));
    }
    let quoted: String = m.text.lines().map(|l| format!("> {l}\n")).collect();
    Draft {
        key: format!("thread:{account}:{thread}"),
        account: account.to_owned(),
        to: to.join(", "),
        cc: cc.join(", "),
        subject,
        body: format!(
            "\n\nOn {}, {} wrote:\n{quoted}",
            crate::when::quote(m.date),
            if m.from_name.is_empty() { &m.from } else { &m.from_name }
        ),
        reply_to: Some(m.item_id.clone()),
    }
}

#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = c6Theme, js_name = isDark)]
    fn theme_is_dark() -> bool;
}

/// To or cc: each a name to click for their card.
fn people(list: Vec<(String, String)>) -> impl IntoView {
    list.into_iter()
        .enumerate()
        .map(|(i, (a, n))| {
            let shown = if n.is_empty() { a.clone() } else { n };
            view! {
                {(i > 0).then_some(", ")}
                <crate::people::Person label=shown.clone() name=shown address=a />
            }
        })
        .collect_view()
}

#[component]
fn Message(m: MailMessage) -> impl IntoView {
    let images = RwSignal::new(false);
    let html = m.html.clone();
    let supports_dark = html.as_deref().is_some_and(crate::mail_html::supports_dark);
    let dark = RwSignal::new(supports_dark && theme_is_dark());
    let theme = window_event_listener(leptos::ev::Custom::new("c6-theme"), move |_: web_sys::Event| {
        let next = supports_dark && theme_is_dark();
        if dark.get_untracked() != next {
            dark.set(next);
        }
    });
    on_cleanup(move || theme.remove());
    let item = m.item_id.clone();
    let has_html = html.is_some();
    // Pictures the HTML shows in place (cid:), as data: URLs: the frame
    // still loads nothing from the network.
    let wants_inline = html.as_deref().is_some_and(|h| h.contains("cid:"));
    let inline_item = item.clone();
    let inline = LocalResource::new(move || {
        let id = inline_item.clone();
        async move {
            if !wants_inline {
                return Vec::new();
            }
            api::rpc("memex.mail.inline", json!({"item_id": id}))
                .await
                .ok()
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|i| {
                    Some((i["cid"].as_str()?.to_owned(), format!("data:{};base64,{}", i["mime"].as_str()?, i["data"].as_str()?)))
                })
                .collect::<Vec<_>>()
        }
    });
    let with_inline = move |h: &str| {
        let mut h = h.to_owned();
        for (cid, url) in inline.get().unwrap_or_default() {
            h = h.replace(&format!("cid:{cid}"), &url);
        }
        h
    };
    // Shown in place: not listed again below.
    let attachments: Vec<_> = m
        .attachments
        .iter()
        .filter(|a| !a.cid.as_ref().is_some_and(|c| html.as_deref().is_some_and(|h| h.contains(&format!("cid:{c}")))))
        .cloned()
        .collect();
    let (pictures, files): (Vec<_>, Vec<_>) =
        attachments.into_iter().partition(|a| a.mime.starts_with("image/") && !a.mime.contains("svg"));
    view! {
        <article class="space-y-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
            <div class="flex flex-wrap items-baseline gap-2 text-sm">
                <crate::people::Person class="font-medium" label=if m.from_name.is_empty() { m.from.clone() } else { m.from_name.clone() }
                    name=if m.from_name.is_empty() { m.from.clone() } else { m.from_name.clone() } address=m.from.clone() />
                <span class="text-xs text-zinc-500">
                    {format!("<{}> to ", m.from)}
                    {people(m.to.clone())}
                    {(!m.cc.is_empty()).then(|| { let cc = m.cc.clone(); view! { " · cc " {people(cc)} } })}
                </span>
                <span class="flex-1"></span>
                <span class="text-xs text-zinc-500">{crate::when::full(m.date)}</span>
            </div>
            {match html {
                Some(h) => view! {
                    <div class="space-y-1">
                        // Sandboxed: no scripts, an opaque origin; remote
                        // images only when you allow them.
                        <iframe class="h-96 w-full rounded border border-zinc-100 bg-white dark:border-zinc-800" sandbox="allow-popups allow-popups-to-escape-sandbox"
                            style=move || if dark.get() { "color-scheme: dark; background-color: Canvas" } else { "color-scheme: light; background-color: white" }
                            srcdoc=move || format!("<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data: {}\"><base target=\"_blank\">{}",
                                if images.get() { "https: http:" } else { "" }, crate::mail_html::themed(&with_inline(&h), dark.get()))></iframe>
                        {move || (!images.get()).then(|| view! {
                            <button class="text-xs text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| images.set(true)>"Show images (lets the sender see you opened it)"</button>
                        })}
                    </div>
                }.into_any(),
                None => view! { <pre class="whitespace-pre-wrap font-sans text-sm">{m.text.clone()}</pre> }.into_any(),
            }}
            {(!pictures.is_empty()).then(|| view! {
                <div class="flex flex-wrap gap-2">
                    {pictures.iter().map(|a| {
                        let (url, full) = (api::mail_attachment(&item, a.index), api::mail_attachment(&item, a.index));
                        view! {
                            <a href=full target="_blank" rel="noopener" title=format!("{} ({})", a.name, size(a.size))
                                class="block overflow-hidden rounded-md border border-zinc-200 dark:border-zinc-700">
                                <img src=url alt=a.name.clone() loading="lazy" class="h-36 max-w-60 bg-zinc-50 object-contain dark:bg-zinc-800" />
                            </a>
                        }
                    }).collect_view()}
                </div>
            })}
            {(!files.is_empty()).then(|| view! {
                <div class="flex flex-wrap gap-2">
                    {files.iter().map(|a| view! { <FileChip name=a.name.clone() mime=a.mime.clone() size=a.size href=api::mail_attachment(&item, a.index) /> }).collect_view()}
                </div>
            })}
            {(!has_html && m.text.is_empty()).then(|| view! { <p class="text-xs text-zinc-500">"(no text)"</p> })}
        </article>
    }
}

/// A file to open (a PDF, in a new tab) or download: its kind, name and size.
#[component]
pub(crate) fn FileChip(name: String, mime: String, size: u64, href: String) -> impl IntoView {
    let (icon, kind) = file_kind(&mime, &name);
    view! {
        <a href=href target="_blank" rel="noopener"
            class="flex max-w-xs items-center gap-2 rounded-md border border-zinc-200 px-2 py-1 hover:bg-zinc-50 dark:border-zinc-700 dark:hover:bg-zinc-800">
            <span class="text-lg">{icon}</span>
            <span class="min-w-0">
                <span class="block truncate text-xs font-medium">{name}</span>
                <span class="block text-[10px] text-zinc-500">{format!("{kind} · {}", self::size(size))}</span>
            </span>
        </a>
    }
}

/// An icon and a short name for a file's kind.
pub(crate) fn file_kind(mime: &str, name: &str) -> (&'static str, String) {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_uppercase()).unwrap_or_default();
    match mime {
        "application/pdf" => ("📕", "PDF".into()),
        m if m.starts_with("image/") => ("🖼", ext),
        m if m.starts_with("video/") => ("🎞", ext),
        m if m.starts_with("audio/") => ("🎵", ext),
        m if m.contains("spreadsheet") || m.contains("excel") || m == "text/csv" => ("📊", ext),
        m if m.contains("presentation") || m.contains("powerpoint") => ("📽", ext),
        m if m.contains("zip") || m.contains("compressed") || m.contains("tar") => ("🗜", ext),
        _ => ("📄", if ext.is_empty() { "File".into() } else { ext }),
    }
}

/// "230 KB", "1.4 MB".
pub(crate) fn size(n: u64) -> String {
    if n >= 1 << 20 { format!("{:.1} MB", n as f64 / 1048576.0) } else { format!("{} KB", n.div_ceil(1024).max(1)) }
}

#[component]
pub(crate) fn Compose(
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
    // A new message's box is its own.
    let key = StoredValue::new(if d.key.is_empty() { format!("compose:{}", js_sys::Date::now() as u64) } else { d.key });
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
                    crate::compose::forget(key.get_value());
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
            <div class="flex flex-wrap items-center justify-end gap-2">
                <span class="mr-auto">
                    <crate::compose::WorkInSession key=key.get_value() about=subject.get_untracked() current=body set=Callback::new(move |t: String| body.set(t))
                        to=Signal::derive(move || to.get().split(',').map(|x| x.trim().to_owned()).filter(|x| !x.is_empty()).collect::<Vec<_>>()) />
                </span>
                <button class=BUTTON_QUIET on:click=move |_| { crate::compose::forget(key.get_value()); compose.set(None); }>"Discard"</button>
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
        // What's typed in and not saved yet is the client you mean.
        let saved = client.get_untracked().and_then(Result::ok).map(|c| c.client_id).unwrap_or_default();
        let typed = Some(secret.get_untracked()).filter(|s| !s.trim().is_empty());
        let unsaved = (id.get_untracked().trim() != saved.trim() || typed.is_some()) && !id.get_untracked().trim().is_empty();
        spawn_local(async move {
            if unsaved {
                if let Err(e) = api::set_google_client(id.get_untracked(), typed).await {
                    note.set(Some(Err(e)));
                    return;
                }
                secret.set(String::new());
                client.refetch();
            }
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
                <p class="text-sm text-zinc-500">"Your Gmail and Google Workspace accounts: mail, calendars and contacts, synced both ways by the apps gmail, google-calendar and google-contacts, in each realm you install them in, with that realm's accounts and mail (they run on a worker with Docker or Podman, reaching only Google). The OAuth client here is this realm's own; without one, root's (set in the console) applies. Google needs an OAuth client from your Google Cloud project: enable the Gmail, Calendar and People APIs, create an OAuth client of type Web application with the redirect URI below, and set the app to In production (personal Gmail: otherwise sign-ins expire weekly; you will see Google's unverified-app notice once) or Internal (Workspace)."</p>
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
                {(c.from_root && !c.client_id.is_empty()).then(|| view! {
                    <p class="text-xs text-zinc-500">"Using root's client (set in the console). Saving one here gives this realm its own."</p>
                })}
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

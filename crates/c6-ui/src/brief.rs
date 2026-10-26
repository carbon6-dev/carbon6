//! The brief (docs/brief.md §2): Mail & chat's home. What people need from
//! you (requests, joined across mail and Slack), first contacts, what you
//! owe and wait on, FYI, and how much was set aside; worked one request at
//! a time from the keyboard.

use c6_api::{Commitment, Request, Triage};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox};
use crate::workspace::{Go, View};

fn day(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    String::from(d.to_locale_date_string("default", &json_options(json!({"weekday": "short", "month": "short", "day": "numeric"}))))
}

fn json_options(v: Value) -> wasm_bindgen::JsValue {
    js_sys::JSON::parse(&v.to_string()).unwrap_or(wasm_bindgen::JsValue::UNDEFINED)
}

/// Tomorrow at 9, or next Monday at 9, in your time.
fn at_nine(days: u32) -> i64 {
    let d = js_sys::Date::new_0();
    d.set_date(d.get_date() + days);
    d.set_hours(9);
    d.set_minutes(0);
    d.set_seconds(0);
    d.get_time() as i64
}

fn next_monday() -> u32 {
    let today = js_sys::Date::new_0().get_day();
    (8 - today) % 7 + if today == 1 { 7 } else { 0 }
}

/// Where a request came from, as icons: ✉ mail, 💬 chat.
fn channels(r: &Request) -> String {
    let mail = r.sources.iter().any(|s| s.kind == "mail");
    let chat = r.sources.iter().any(|s| s.kind == "chat");
    match (mail, chat) {
        (true, true) => "✉ 💬".into(),
        (true, false) => "✉".into(),
        _ => "💬".into(),
    }
}

/// What the brief's parts share.
#[derive(Clone, Copy)]
struct Brief {
    queue: Memo<Vec<Request>>,
    open: RwSignal<Option<i64>>,
    tick: RwSignal<u32>,
    error: RwSignal<Option<String>>,
}

impl Brief {
    /// Settles a request (done, dropped, snoozed); an open one gives way to
    /// the next.
    fn settle(self, id: i64, status: &'static str, until: Option<i64>) {
        spawn_local(async move {
            match api::rpc("memex.requests.set", json!({"id": id, "status": status, "until": until})).await {
                Ok(_) => {
                    let q = self.queue.get_untracked();
                    let at = q.iter().position(|r| r.id == id).unwrap_or(0);
                    let next = q.iter().skip(at + 1).chain(q.iter().take(at)).find(|r| r.id != id).map(|r| r.id);
                    self.open.update(|o| {
                        if o.is_some() {
                            *o = next;
                        }
                    });
                    self.tick.update(|t| *t += 1);
                }
                Err(e) => self.error.set(Some(e)),
            }
        });
    }

    fn merge(self, into: i64, from: i64) {
        spawn_local(async move {
            match api::rpc("memex.requests.merge", json!({"into": into, "from": from})).await {
                Ok(_) => self.tick.update(|t| *t += 1),
                Err(e) => self.error.set(Some(e)),
            }
        });
    }
}

#[component]
pub fn BriefPage() -> impl IntoView {
    let tick = RwSignal::new(0u32);
    let requests = LocalResource::new(move || {
        tick.track();
        api::rpc_as::<Vec<Request>>("memex.requests.list", json!({}))
    });
    let counts = LocalResource::new(move || {
        tick.track();
        api::rpc("memex.triage.counts", json!({}))
    });
    let fyi = LocalResource::new(move || {
        tick.track();
        api::rpc_as::<Vec<Triage>>("memex.triage.list", json!({"category": "fyi", "limit": 8}))
    });
    let commitments = LocalResource::new(move || {
        tick.track();
        api::rpc_as::<Vec<Commitment>>("memex.commitments", json!({"status": "open"}))
    });
    // The queue in order (needs you, then first contacts), the one picked,
    // and the one open.
    let queue = Memo::new(move |_| {
        let mut all = requests.get().and_then(Result::ok).unwrap_or_default();
        all.sort_by_key(|r| (r.category != "needs_you", r.due.is_none(), r.due.unwrap_or(0)));
        all
    });
    let picked = RwSignal::new(0usize);
    let open = RwSignal::new(None::<i64>);
    let error = RwSignal::new(None::<String>);
    let b = Brief { queue, open, tick, error };
    let act = move |id: i64, status: &'static str, until: Option<i64>| b.settle(id, status, until);
    // The keys: J/K move, Enter opens, E done, H snooze to tomorrow, # drop,
    // Esc back to the list.
    let _keys = window_event_listener(leptos::ev::keydown, move |ev| {
        let in_field = ev
            .target()
            .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
            .is_some_and(|el| matches!(el.tag_name().as_str(), "TEXTAREA" | "INPUT" | "SELECT") || el.has_attribute("contenteditable"));
        if in_field || ev.meta_key() || ev.ctrl_key() || ev.alt_key() {
            return;
        }
        let q = queue.get_untracked();
        let current = open.get_untracked().or_else(|| q.get(picked.get_untracked()).map(|r| r.id));
        match ev.key().as_str() {
            "j" | "ArrowDown" if open.get_untracked().is_none() => picked.update(|p| *p = (*p + 1).min(q.len().saturating_sub(1))),
            "k" | "ArrowUp" if open.get_untracked().is_none() => picked.update(|p| *p = p.saturating_sub(1)),
            "j" => {
                let at = q.iter().position(|r| Some(r.id) == current).unwrap_or(0);
                open.set(q.get((at + 1).min(q.len().saturating_sub(1))).map(|r| r.id));
            }
            "k" => {
                let at = q.iter().position(|r| Some(r.id) == current).unwrap_or(0);
                open.set(q.get(at.saturating_sub(1)).map(|r| r.id));
            }
            "Enter" if open.get_untracked().is_none() => open.set(current),
            "Escape" => open.set(None),
            "e" => {
                if let Some(id) = current {
                    act(id, "done", None);
                }
            }
            "h" => {
                if let Some(id) = current {
                    act(id, "snoozed", Some(at_nine(1)));
                }
            }
            "#" => {
                if let Some(id) = current {
                    act(id, "dropped", None);
                }
            }
            _ => return,
        }
        ev.prevent_default();
    });
    view! {
        <main class="mx-auto max-w-5xl space-y-5 px-4 py-5">
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || match open.get().and_then(|id| queue.get().into_iter().find(|r| r.id == id)) {
                Some(r) => view! { <RequestView r b /> }.into_any(),
                None => view! {
                    <header class="flex flex-wrap items-baseline gap-x-3 gap-y-1">
                        <h1 class="text-xl font-semibold">"Brief"</h1>
                        <span class="text-sm text-zinc-500">{move || {
                            let n = queue.get().iter().filter(|r| r.category == "needs_you").count();
                            if n == 0 { "Nothing needs you.".to_owned() } else { format!("{n} need you") }
                        }}</span>
                        <span class="flex-1"></span>
                        <span class="hidden text-xs text-zinc-400 sm:inline">"J/K move · Enter open · E done · H tomorrow · # drop · Esc back"</span>
                    </header>
                    <Section title="Needs you" empty="Nothing waits on you.">
                        {move || queue.get().into_iter().enumerate().filter(|(_, r)| r.category == "needs_you").map(|(i, r)| view! { <Row r i picked b /> }).collect_view()}
                    </Section>
                    {move || queue.get().iter().any(|r| r.category == "first_contact").then(|| view! {
                        <Section title="First contact" empty="">
                            {move || queue.get().into_iter().enumerate().filter(|(_, r)| r.category == "first_contact").map(|(i, r)| view! { <Row r i picked b /> }).collect_view()}
                        </Section>
                    })}
                    <div class="grid gap-5 md:grid-cols-2">
                        <Section title="You owe" empty="No promises of yours open.">
                            {move || commitments.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|c| c.who == "me").take(6).map(|c| view! { <Owed c /> }).collect_view()}
                        </Section>
                        <Section title="Waiting on others" empty="Nobody owes you anything.">
                            {move || commitments.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|c| c.who != "me").take(6).map(|c| view! { <Owed c /> }).collect_view()}
                        </Section>
                    </div>
                    <Section title="FYI" empty="Nothing new to know.">
                        {move || fyi.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|t| view! {
                            <div class="flex items-baseline gap-2 px-3 py-1.5 text-sm">
                                <span class="shrink-0 text-xs">{if t.kind == "mail" { "✉" } else { "💬" }}</span>
                                <span class="min-w-0 flex-1 truncate"><b class="font-medium">{t.sender_name.clone()}</b>" · "{t.title.clone()}</span>
                                <span class="shrink-0 truncate text-xs text-zinc-500">{t.reason.clone()}</span>
                            </div>
                        }).collect_view()}
                    </Section>
                    <p class="text-sm text-zinc-500">{move || {
                        let c = counts.get().and_then(Result::ok).unwrap_or_default();
                        let n = |k: &str, one: &str, many: &str| {
                            let n = c[k].as_i64().unwrap_or(0);
                            format!("{n} {}", if n == 1 { one } else { many })
                        };
                        format!(
                            "Set aside: {} · {} · {} (in ",
                            n("pitch", "cold pitch", "cold pitches"),
                            n("newsletter", "newsletter", "newsletters"),
                            n("notification", "notification", "notifications")
                        )
                    }}<Go to=View::Mail class="underline">"Mail"</Go>"'s views)"</p>
                }.into_any(),
            }}
        </main>
    }
}

#[component]
fn Section(title: &'static str, empty: &'static str, children: Children) -> impl IntoView {
    view! {
        <section>
            <h2 class="mb-1 text-[11px] font-semibold uppercase tracking-wide text-zinc-500">{title}</h2>
            <div class="divide-y divide-zinc-100 rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                {children()}
                {(!empty.is_empty()).then(|| view! { <p class="hidden px-3 py-2 text-sm text-zinc-500 only:block">{empty}</p> })}
            </div>
        </section>
    }
}

#[component]
fn Row(r: Request, i: usize, picked: RwSignal<usize>, b: Brief) -> impl IntoView {
    let open = b.open;
    let id = r.id;
    let maybe = r.maybe_same.first().copied();
    view! {
        <div class=move || format!("flex cursor-pointer flex-wrap items-baseline gap-x-2 px-3 py-2 text-sm sm:flex-nowrap {}", if picked.get() == i { "bg-zinc-100 dark:bg-zinc-900" } else { "hover:bg-zinc-50 dark:hover:bg-zinc-900/50" })
            on:click=move |_| { picked.set(i); open.set(Some(id)); }>
            <span class="shrink-0 text-amber-500">"●"</span>
            <span class="min-w-0 flex-1 truncate font-medium sm:w-36 sm:flex-none">{if r.person_name.is_empty() { r.person.clone() } else { r.person_name.clone() }}</span>
            <span class="order-last w-full truncate pl-4 sm:order-none sm:w-auto sm:min-w-0 sm:flex-1 sm:pl-0" title=r.reason.clone()>{r.title.clone()}</span>
            {maybe.map(|other| view! {
                <button class="shrink-0 rounded bg-sky-100 px-1.5 text-[10px] text-sky-800 dark:bg-sky-950 dark:text-sky-300" title="Possibly the same ask as another request: merge them"
                    on:click=move |ev| { ev.stop_propagation(); b.merge(other.min(id), other.max(id)); }>"same? merge"</button>
            })}
            {r.due.map(|d| view! { <span class="shrink-0 text-xs text-amber-700 dark:text-amber-400">{format!("by {}", day(d))}</span> })}
            <span class="shrink-0 text-xs text-zinc-400">{channels(&r)}</span>
        </div>
    }
}

#[component]
fn Owed(c: Commitment) -> impl IntoView {
    view! {
        <div class="flex items-baseline gap-2 px-3 py-1.5 text-sm">
            <span class="min-w-0 flex-1 truncate" title=c.item_title.clone()>{c.what.clone()}{(c.who != "me").then(|| format!(" — {}", c.who))}</span>
            {c.due.map(|d| view! { <span class="shrink-0 text-xs text-zinc-500">{day(d)}</span> })}
        </div>
    }
}

/// One request: its conversations on the left, what's asked and what to do
/// on the right.
#[component]
fn RequestView(r: Request, b: Brief) -> impl IntoView {
    let (id, open) = (r.id, b.open);
    let act = move |id: i64, status: &'static str, until: Option<i64>| b.settle(id, status, until);
    let refresh = RwSignal::new(0u32);
    view! {
        <div class="flex flex-col gap-4 lg:flex-row">
            <div class="min-w-0 flex-1 space-y-4">
                {r.sources.iter().rev().map(|s| view! { <Source target=s.target.clone() kind=s.kind.clone() title=s.title.clone() refresh /> }).collect_view()}
            </div>
            <aside class="w-full shrink-0 space-y-3 lg:sticky lg:top-0 lg:w-80">
                <button class=BUTTON_QUIET on:click=move |_| open.set(None)>"← Brief (Esc)"</button>
                <div class="space-y-1 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
                    <p class="text-xs uppercase tracking-wide text-zinc-500">{if r.category == "first_contact" { "First contact" } else { "Needs you" }}</p>
                    <h1 class="font-semibold">{r.title.clone()}</h1>
                    <p class="text-sm">{if r.person_name.is_empty() { r.person.clone() } else { format!("{} <{}>", r.person_name, r.person) }}</p>
                    {r.due.map(|d| view! { <p class="text-sm text-amber-700 dark:text-amber-400">{format!("By {}", day(d))}</p> })}
                    <p class="text-sm text-zinc-500">{r.reason.clone()}</p>
                </div>
                <div class="flex flex-wrap gap-2">
                    <button class=BUTTON on:click=move |_| act(id, "done", None)>"Done (E)"</button>
                    <button class=BUTTON_QUIET on:click=move |_| act(id, "snoozed", Some(at_nine(1)))>"Tomorrow (H)"</button>
                    <button class=BUTTON_QUIET on:click=move |_| act(id, "snoozed", Some(at_nine(next_monday())))>"Next week"</button>
                    <button class=format!("{BUTTON_QUIET} text-red-600") on:click=move |_| act(id, "dropped", None)>"Drop (#)"</button>
                </div>
                <p class="text-xs text-zinc-400">"Answer in a conversation: once you reply, it's done by itself."</p>
            </aside>
        </div>
    }
}

/// A conversation a request came from: a mail thread or a chat thread or
/// conversation.
#[component]
fn Source(target: String, kind: String, title: String, refresh: RwSignal<u32>) -> impl IntoView {
    if kind == "mail" {
        let rest = target.strip_prefix("thread:").unwrap_or_default();
        let (account, thread) = rest.rsplit_once(':').map(|(a, t)| (a.to_owned(), t.to_owned())).unwrap_or_default();
        let open = RwSignal::new(Some((account.clone(), thread.clone())));
        let compose = RwSignal::new(None::<crate::mail::Draft>);
        let status = RwSignal::new(None::<Result<String, String>>);
        return view! {
            <section class="rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
                {move || status.get().map(|r| match r {
                    Ok(m) => view! { <p class="text-sm text-emerald-600">{m}</p> }.into_any(),
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                })}
                {move || compose.get().map(|d| view! { <crate::mail::Compose d compose status refresh /> })}
                {move || open.get().is_some().then(|| view! { <crate::mail::Thread account=account.clone() thread=thread.clone() open compose refresh /> })}
            </section>
        }
        .into_any();
    }
    let parts: Vec<String> = target.strip_prefix("chat:").unwrap_or_default().splitn(3, ':').map(str::to_owned).collect();
    let place = crate::chat::Place {
        workspace: parts.first().cloned().unwrap_or_default(),
        channel: parts.get(1).cloned().unwrap_or_default(),
        name: title,
    };
    let tick = RwSignal::new(0u32);
    let thread = RwSignal::new(parts.get(2).cloned());
    view! {
        <section class="flex max-h-[70vh] min-h-64 flex-col overflow-hidden rounded-lg border border-zinc-200 dark:border-zinc-800">
            {move || match thread.get() {
                Some(t) => view! { <crate::chat::Thread place=place.clone() thread_ts=t close=thread tick /> }.into_any(),
                None => view! { <crate::chat::Conversation place=place.clone() thread tick /> }.into_any(),
            }}
        </section>
    }
    .into_any()
}

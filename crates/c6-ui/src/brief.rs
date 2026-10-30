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

/// Tomorrow at 9, or next Monday at 9, in your time.
pub(crate) fn at_nine(days: u32) -> i64 {
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

/// How a request is settled: the Brief's actions, Mail's too, so the two
/// don't drift apart.
#[derive(Clone, Copy)]
pub(crate) enum Settle {
    Done,
    /// Snoozed until tomorrow at 9.
    Tomorrow,
    /// Snoozed until next Monday at 9.
    NextWeek,
    Drop,
}

/// Settles a request (done, dropped, or snoozed until a time).
pub(crate) async fn settle(id: i64, how: Settle) -> Result<Value, String> {
    let (status, until) = match how {
        Settle::Done => ("done", None),
        Settle::Tomorrow => ("snoozed", Some(at_nine(1))),
        Settle::NextWeek => ("snoozed", Some(at_nine(next_monday()))),
        Settle::Drop => ("dropped", None),
    };
    api::rpc("memex.requests.set", json!({"id": id, "status": status, "until": until})).await
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
    open: RwSignal<Option<Request>>,
    navigation: RwSignal<u64>,
    loading: RwSignal<bool>,
    notice: RwSignal<Option<String>>,
    /// The commitment or FYI row unfolded (c:<id>, f:<target>).
    unfolded: RwSignal<Option<String>>,
    tick: RwSignal<u32>,
    error: RwSignal<Option<String>>,
}

impl Brief {
    fn show(self, request: Option<Request>) {
        self.navigation.update(|n| *n += 1);
        self.loading.set(false);
        self.notice.set(None);
        self.open.set(request);
    }

    fn resolved(self, navigation: u64, result: Result<Request, String>, now: i64) {
        if self.navigation.try_get_untracked() != Some(navigation) {
            return;
        }
        self.loading.set(false);
        match request_to_open(result, now) {
            Ok(r) => self.show(Some(r)),
            Err(note) => self.notice.set(Some(note.into())),
        }
    }

    /// Settles a request; an open one gives way to the next.
    fn settle(self, id: i64, how: Settle) {
        spawn_local(async move {
            match settle(id, how).await {
                Ok(_) => self.gone(id),
                Err(e) => self.error.set(Some(e)),
            }
        });
    }

    /// A request is settled: the next one opens (as Superhuman does).
    fn gone(self, id: i64) {
        if self.open.get_untracked().is_some_and(|r| r.id == id) {
            let q = self.queue.get_untracked();
            let at = q.iter().position(|r| r.id == id).map_or(0, |at| at + 1);
            let next = q.iter().skip(at).chain(q.iter().take(at)).find(|r| r.id != id).cloned();
            self.show(next);
        }
        self.tick.update(|t| *t += 1);
    }

    /// Calls `method`; then the brief again, the row folded.
    fn call(self, method: &'static str, params: Value) {
        spawn_local(async move {
            match api::rpc(method, params).await {
                Ok(_) => {
                    self.unfolded.set(None);
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

pub(crate) fn request_id(value: &str) -> Option<i64> {
    value.parse::<i64>().ok().filter(|id| *id > 0)
}

pub(crate) fn request_path(id: i64) -> String {
    format!("/brief?request={id}")
}

fn request_to_open(result: Result<Request, String>, now: i64) -> Result<Request, &'static str> {
    let r = result.map_err(|_| "This request is unavailable.")?;
    if r.status == "open" || (r.status == "snoozed" && r.until.is_some_and(|until| until <= now)) {
        Ok(r)
    } else {
        Err("This request is no longer open.")
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
    let status = LocalResource::new(move || {
        tick.track();
        api::rpc("memex.triage.status", json!({}))
    });
    let fyi = LocalResource::new(move || {
        tick.track();
        api::rpc_as::<Vec<Triage>>("memex.triage.list", json!({"category": "fyi", "unseen": true, "limit": 8}))
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
    let open = RwSignal::new(None::<Request>);
    let error = RwSignal::new(None::<String>);
    let unfolded = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let notice = RwSignal::new(None::<String>);
    let b = Brief { queue, open, unfolded, tick, error, navigation: RwSignal::new(0), loading, notice };
    let column = crate::workspace::use_column();
    let active = Signal::derive(move || column.is_none_or(|c| c.active.get()));
    let requested = crate::workspace::use_workspace().brief_request;
    Effect::new(move || {
        if !active.get() {
            return;
        }
        let Some(id) = requested.get() else { return };
        requested.set(None);
        b.show(None);
        loading.set(true);
        let navigation = b.navigation.get_untracked();
        // The queue is only its first page. A link can name any open request.
        spawn_local(async move {
            let result = api::rpc_as::<Request>("memex.requests.get", json!({"id": id})).await;
            b.resolved(navigation, result, js_sys::Date::now() as i64);
        });
    });
    // Fresh while you look: the brief again every half minute, unless you're
    // in a request or a row.
    let refresh = set_interval_with_handle(
        move || {
            if open.get_untracked().is_none() && unfolded.get_untracked().is_none() {
                tick.update(|t| *t += 1);
            }
        },
        std::time::Duration::from_secs(30),
    )
    .ok();
    on_cleanup(move || {
        if let Some(h) = refresh {
            h.clear();
        }
    });
    let act = move |id: i64, how: Settle| b.settle(id, how);
    // The keys: J/K move, Enter opens, E done, H snooze to tomorrow, # drop,
    // Esc back to the list.
    let keys = window_event_listener(leptos::ev::keydown, move |ev| {
        let in_field = ev
            .target()
            .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
            .is_some_and(|el| matches!(el.tag_name().as_str(), "TEXTAREA" | "INPUT" | "SELECT") || el.has_attribute("contenteditable"));
        if !active.get_untracked() || in_field || ev.meta_key() || ev.ctrl_key() || ev.alt_key() {
            return;
        }
        if loading.get_untracked() && ev.key() != "Escape" {
            return;
        }
        let q = queue.get_untracked();
        let current = open.get_untracked().map(|r| r.id).or_else(|| q.get(picked.get_untracked()).map(|r| r.id));
        match ev.key().as_str() {
            "j" | "ArrowDown" if open.get_untracked().is_none() => picked.update(|p| *p = (*p + 1).min(q.len().saturating_sub(1))),
            "k" | "ArrowUp" if open.get_untracked().is_none() => picked.update(|p| *p = p.saturating_sub(1)),
            "j" => {
                let at = q.iter().position(|r| Some(r.id) == current).unwrap_or(0);
                b.show(q.get((at + 1).min(q.len().saturating_sub(1))).cloned());
            }
            "k" => {
                let at = q.iter().position(|r| Some(r.id) == current).unwrap_or(0);
                b.show(q.get(at.saturating_sub(1)).cloned());
            }
            "Enter" if open.get_untracked().is_none() => b.show(q.get(picked.get_untracked()).cloned()),
            "Escape" => b.show(None),
            "e" => {
                if let Some(id) = current {
                    act(id, Settle::Done);
                }
            }
            "h" => {
                if let Some(id) = current {
                    act(id, Settle::Tomorrow);
                }
            }
            "#" => {
                if let Some(id) = current {
                    act(id, Settle::Drop);
                }
            }
            _ => return,
        }
        ev.prevent_default();
    });
    on_cleanup(move || keys.remove());
    view! {
        <main class="mx-auto max-w-5xl space-y-5 px-4 py-5">
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || notice.get().map(|note| view! { <p class="text-sm text-zinc-500">{note}</p> })}
            {move || loading.get().then(|| view! { <p class="text-sm text-zinc-500">"Opening request…"</p> })}
            {move || match open.get() {
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
                    // Why it may be short: sorting waits for a model, or for
                    // tomorrow's budget.
                    {move || status.get().and_then(Result::ok).and_then(|s| {
                        let waiting = s["waiting"].as_u64().unwrap_or(0);
                        let why = if s["model"] != true {
                            "No understanding model is chosen, so only what rules settle (lists, notifications, known pitches) is sorted. Choose one in Settings → Memex."
                        } else if s["budget_left"].as_u64() == Some(0) && waiting > 0 {
                            "Today's model calls are used up, so new mail and chat wait until tomorrow. Raise calls per day in Settings → Memex."
                        } else {
                            return None;
                        };
                        Some(view! {
                            <div class="flex flex-wrap items-center gap-2 rounded-md bg-amber-50 p-2 text-sm dark:bg-amber-950">
                                <span class="flex-1">{why}{(waiting > 0).then(|| format!(" {}{} conversations wait.", waiting, if waiting >= 20 { "+" } else { "" }))}</span>
                                <Go to=View::Settings { note: None, section: "memex".into() } class=BUTTON>"Settings → Memex"</Go>
                            </div>
                        })
                    })}
                    <Section title="Needs you" empty="Nothing waits on you.">
                        {move || queue.get().into_iter().enumerate().filter(|(_, r)| r.category == "needs_you").map(|(i, r)| view! { <Row r i picked b /> }).collect_view()}
                    </Section>
                    {move || queue.get().iter().any(|r| r.category == "first_contact").then(|| view! {
                        <Section title="First contact" empty="">
                            {move || queue.get().into_iter().enumerate().filter(|(_, r)| r.category == "first_contact").map(|(i, r)| view! { <Row r i picked b /> }).collect_view()}
                        </Section>
                    })}
                    // Work you left open that needs you or went quiet (docs/work.md §7).
                    <crate::work::LeftOpen />
                    <div class="grid gap-5 md:grid-cols-2">
                        <Section title="You owe" empty="No promises of yours open.">
                            {move || commitments.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|c| c.who == "me").take(6).map(|c| view! { <Owed c b /> }).collect_view()}
                        </Section>
                        <Section title="Waiting on others" empty="Nobody owes you anything.">
                            {move || commitments.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|c| c.who != "me").take(6).map(|c| view! { <Owed c b /> }).collect_view()}
                        </Section>
                    </div>
                    <Section title="FYI" empty="Nothing new to know.">
                        {move || fyi.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|t| view! { <Fyi t b /> }).collect_view()}
                    </Section>
                    <p class="text-sm text-zinc-500">{move || {
                        let s = status.get().and_then(Result::ok).unwrap_or_default();
                        let n = |k: &str| s["sorted"][k]["n"].as_i64().unwrap_or(0);
                        let by_model = s["sorted"]["mail"]["by_model"].as_i64().unwrap_or(0) + s["sorted"]["chat"]["by_model"].as_i64().unwrap_or(0);
                        let (mail, chat) = (n("mail"), n("chat"));
                        format!(
                            "Read in the last {} days: {mail} {}, {chat} {} ({by_model} by the model, the rest by rules).",
                            s["days"].as_i64().unwrap_or(14),
                            if mail == 1 { "email thread" } else { "email threads" },
                            if chat == 1 { "chat" } else { "chats" },
                        )
                    }}</p>
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
    let selected = r.clone();
    let id = r.id;
    let maybe = r.maybe_same.first().copied();
    view! {
        <div class=move || format!("flex cursor-pointer flex-wrap items-baseline gap-x-2 px-3 py-2 text-sm sm:flex-nowrap {}", if picked.get() == i { "bg-zinc-100 dark:bg-zinc-900" } else { "hover:bg-zinc-50 dark:hover:bg-zinc-900/50" })
            on:click=move |_| { picked.set(i); b.show(Some(selected.clone())); }>
            <span class="shrink-0 text-amber-500">"●"</span>
            <span class="min-w-0 flex-1 truncate font-medium sm:w-36 sm:flex-none">{if r.person_name.is_empty() { r.person.clone() } else { r.person_name.clone() }}</span>
            <span class="order-last w-full truncate pl-4 sm:order-none sm:w-auto sm:min-w-0 sm:flex-1 sm:pl-0" title=r.reason.clone()>{r.title.clone()}</span>
            {maybe.map(|other| view! {
                <button class="shrink-0 rounded bg-sky-100 px-1.5 text-[10px] text-sky-800 dark:bg-sky-950 dark:text-sky-300" title="Possibly the same ask as another request: merge them"
                    on:click=move |ev| { ev.stop_propagation(); b.merge(other.min(id), other.max(id)); }>"same? merge"</button>
            })}
            {r.due.map(|d| view! { <span class="shrink-0 text-xs text-amber-700 dark:text-amber-400">{format!("by {}", crate::when::compact(d))}</span> })}
            <span class="shrink-0 text-xs text-zinc-400">{channels(&r)}</span>
        </div>
    }
}

/// A row that unfolds on a click: `head` always, `body` when unfolded.
#[component]
fn Fold(key: String, b: Brief, head: ViewFn, body: ViewFn) -> impl IntoView {
    let k = key.clone();
    let unfolded = move || b.unfolded.get().as_deref() == Some(k.as_str());
    let shown = unfolded.clone();
    view! {
        <div>
            <div class=move || format!("flex cursor-pointer items-baseline gap-2 px-3 py-1.5 text-sm {}", if unfolded() { "bg-zinc-100 dark:bg-zinc-900" } else { "hover:bg-zinc-50 dark:hover:bg-zinc-900/50" })
                on:click=move |_| b.unfolded.update(|u| *u = if u.as_deref() == Some(key.as_str()) { None } else { Some(key.clone()) })>
                {head.run()}
            </div>
            {move || shown().then(|| view! { <div class="space-y-2 border-t border-zinc-100 p-3 dark:border-zinc-800">{body.run()}</div> })}
        </div>
    }
}

/// A promise, yours or someone's: unfolded, where it was made, and done or
/// drop.
#[component]
fn Owed(c: Commitment, b: Brief) -> impl IntoView {
    let id = c.id;
    let (target, title) = (c.target.clone(), c.item_title.clone());
    view! {
        <Fold key=format!("c:{id}") b
            head=ViewFn::from({
                let c = c.clone();
                move || view! {
                    <span class="min-w-0 flex-1 truncate" title=c.item_title.clone()>{c.what.clone()}{(c.who != "me").then(|| format!(" — {}", c.who))}</span>
                    {c.due.map(|d| view! { <span class="shrink-0 text-xs text-zinc-500">{crate::when::compact(d)}</span> })}
                }.into_any()
            })
            body=ViewFn::from(move || view! {
                <div class="flex flex-wrap gap-2">
                    <button class=BUTTON on:click=move |_| b.call("memex.commitment", json!({"id": id, "status": "done"}))>"Done"</button>
                    <button class=format!("{BUTTON_QUIET} text-red-600") on:click=move |_| b.call("memex.commitment", json!({"id": id, "status": "dropped"}))>"Drop"</button>
                </div>
                {if target.is_empty() {
                    view! { <p class="text-xs text-zinc-500">{format!("From {title}")}</p> }.into_any()
                } else {
                    view! { <Source target=target.clone() kind="mail".into() title=title.clone() refresh=RwSignal::new(0) /> }.into_any()
                }}
            }.into_any())
        />
    }
}

/// Something to know: unfolded, the conversation, and seen, needs me, or
/// not for me (the sorting learns from the last two).
#[component]
fn Fyi(t: Triage, b: Brief) -> impl IntoView {
    let (target, kind, title) = (t.target.clone(), t.kind.clone(), t.title.clone());
    view! {
        <Fold key=format!("f:{}", t.target) b
            head=ViewFn::from({
                let t = t.clone();
                move || view! {
                    <span class="shrink-0 text-xs">{if t.kind == "mail" { "✉" } else { "💬" }}</span>
                    <span class="min-w-0 flex-1 truncate"><b class="font-medium">{t.sender_name.clone()}</b>" · "{t.title.clone()}</span>
                    <span class="hidden shrink-0 truncate text-xs text-zinc-500 sm:inline sm:max-w-[40%]">{t.reason.clone()}</span>
                }.into_any()
            })
            body=ViewFn::from(move || {
                let (a, n, x) = (target.clone(), target.clone(), target.clone());
                view! {
                    <div class="flex flex-wrap gap-2">
                        <button class=BUTTON on:click=move |_| b.call("memex.triage.seen", json!({"target": a}))>"Seen"</button>
                        <button class=BUTTON_QUIET title="It needs you after all: it becomes a request"
                            on:click=move |_| b.call("memex.triage.set", json!({"target": n, "category": "needs_you", "note": "You said it needs you"}))>"Needs me"</button>
                        <button class=BUTTON_QUIET title="Not worth knowing: set aside, and the sorting learns"
                            on:click=move |_| b.call("memex.triage.set", json!({"target": x, "category": "noise", "note": "You said it isn't worth knowing"}))>"Not for me"</button>
                    </div>
                    <Source target=target.clone() kind=kind.clone() title=title.clone() refresh=RwSignal::new(0) />
                }.into_any()
            })
        />
    }
}

/// One request: its conversations on the left, what's asked and what to do
/// on the right.
#[component]
fn RequestView(r: Request, b: Brief) -> impl IntoView {
    let id = r.id;
    let act = move |id: i64, how: Settle| b.settle(id, how);
    let refresh = RwSignal::new(0u32);
    view! {
        <div class="flex flex-col gap-4 lg:flex-row">
            <div class="min-w-0 flex-1 space-y-4">
                {r.sources.iter().rev().map(|s| view! { <Source target=s.target.clone() kind=s.kind.clone() title=s.title.clone() refresh /> }).collect_view()}
            </div>
            <aside class="w-full shrink-0 space-y-3 lg:sticky lg:top-0 lg:w-80">
                <button class=BUTTON_QUIET on:click=move |_| b.show(None)>"← Brief (Esc)"</button>
                <div class="space-y-1 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
                    <p class="text-xs uppercase tracking-wide text-zinc-500">{if r.category == "first_contact" { "First contact" } else { "Needs you" }}</p>
                    <h1 class="font-semibold">{r.title.clone()}</h1>
                    <p class="text-sm">
                        <crate::people::Person label=if r.person_name.is_empty() { r.person.clone() } else { format!("{} <{}>", r.person_name, r.person) }
                            name=if r.person_name.is_empty() { r.person.clone() } else { r.person_name.clone() } address=r.person.clone() />
                    </p>
                    {r.due.map(|d| view! { <p class="text-sm text-amber-700 dark:text-amber-400">{format!("By {}", crate::when::full(d))}</p> })}
                    <p class="text-sm text-zinc-500">{r.reason.clone()}</p>
                    <crate::work::RequestWork id title=if r.person_name.is_empty() { r.title.clone() } else { format!("{}: {}", r.person_name, r.title) } />
                </div>
                // A mail thread shows its senders' cards itself.
                {(r.person.contains('@') && !r.sources.iter().any(|s| s.kind == "mail")).then(|| view! { <crate::understanding::Cards params=json!({"address": r.person.clone()}) /> })}
                <DraftCard r=r.clone() b />
                <div class="flex flex-wrap gap-2">
                    <button class=BUTTON_QUIET on:click=move |_| act(id, Settle::Done)>"Done (E)"</button>
                    <button class=BUTTON_QUIET on:click=move |_| act(id, Settle::Tomorrow)>"Tomorrow (H)"</button>
                    <button class=BUTTON_QUIET on:click=move |_| act(id, Settle::NextWeek)>"Next week"</button>
                    <button class=format!("{BUTTON_QUIET} text-red-600") on:click=move |_| act(id, Settle::Drop)>"Drop (#)"</button>
                </div>
                <p class="text-xs text-zinc-400">"Or answer in a conversation: once you reply, it's done by itself."</p>
            </aside>
        </div>
    }
}

/// The choices a draft leaves you (`[[one | other]]`): each, with its
/// options.
fn choices(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let Some(len) = rest[start..].find("]]") else { break };
        let whole = &rest[start..start + len + 2];
        let options = whole[2..whole.len() - 2].split('|').map(|o| o.trim().to_owned()).filter(|o| !o.is_empty()).collect();
        out.push((whole.to_owned(), options));
        rest = &rest[start + len + 2..];
    }
    out
}

/// The reply drafted for a request: where it goes, your choices, the text
/// to edit; sent with ⌘↵.
#[component]
fn DraftCard(r: Request, b: Brief) -> impl IntoView {
    let id = r.id;
    let about = if r.person_name.is_empty() { r.title.clone() } else { format!("{}: {}", r.person_name, r.title) };
    let draft = RwSignal::new(r.draft.clone().filter(|d| d.error.is_none()));
    let failed = r.draft.as_ref().and_then(|d| d.error.clone());
    let text = RwSignal::new(r.draft.as_ref().map(|d| d.text.clone()).unwrap_or_default());
    let busy = RwSignal::new(false);
    let to = LocalResource::new(move || {
        let target = draft.get().map(|d| d.target).unwrap_or_default();
        async move {
            if target.is_empty() {
                return Ok(Value::Null);
            }
            api::rpc("memex.requests.reply_to", json!({"target": target})).await
        }
    });
    let redraft = move || {
        busy.set(true);
        spawn_local(async move {
            match api::rpc_as::<c6_api::RequestDraft>("memex.requests.draft", json!({"id": id})).await {
                Ok(d) => {
                    text.set(d.text.clone());
                    draft.set(Some(d));
                }
                Err(e) => b.error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    let send = move || {
        let Some(d) = draft.get_untracked() else { return };
        busy.set(true);
        spawn_local(async move {
            match api::rpc("memex.requests.send", json!({"id": id, "target": d.target, "text": text.get_untracked()})).await {
                Ok(_) => {
                    crate::compose::forget(format!("request:{id}"));
                    b.gone(id)
                }
                Err(e) => b.error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    view! {
        <div class="space-y-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
            {move || match draft.get() {
                None => view! {
                    <p class="text-sm text-zinc-500">{failed.clone().map(|e| format!("Couldn't draft: {e}")).unwrap_or_else(|| {
                        if r.category == "needs_you" { "A draft is on its way.".into() } else { "No draft yet.".into() }
                    })}</p>
                    <button class=BUTTON disabled=move || busy.get() on:click=move |_| redraft()>{move || if busy.get() { "Drafting…" } else { "Draft a reply" }}</button>
                }.into_any(),
                Some(d) => view! {
                    <p class="text-xs text-zinc-500">{move || {
                        let v = to.get().and_then(Result::ok).unwrap_or_default();
                        if v["kind"] == "mail" {
                            format!("✉ To {} <{}> · {}", v["to_name"].as_str().unwrap_or(""), v["to"][0].as_str().unwrap_or(""), v["subject"].as_str().unwrap_or(""))
                        } else if v["kind"] == "chat" {
                            format!("💬 In {}", if v["thread_ts"].is_string() { "the thread" } else { "the conversation" })
                        } else {
                            String::new()
                        }
                    }}</p>
                    {(!d.note.is_empty()).then(|| view! { <p class="text-xs text-amber-700 dark:text-amber-400">{format!("Check: {}", d.note)}</p> })}
                    {move || choices(&text.get()).into_iter().map(|(whole, options)| view! {
                        <div class="flex flex-wrap items-center gap-1 text-xs">
                            <span class="text-zinc-500">"Choose:"</span>
                            {options.into_iter().map(|o| {
                                let (whole, chosen) = (whole.clone(), o.clone());
                                view! {
                                    <button class="rounded border border-zinc-300 px-1.5 py-0.5 hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
                                        on:click=move |_| text.update(|t| *t = t.replacen(&whole, &chosen, 1))>{o}</button>
                                }
                            }).collect_view()}
                        </div>
                    }).collect_view()}
                    <textarea class="h-40 w-full rounded-md border border-zinc-300 bg-transparent p-2 text-sm dark:border-zinc-700"
                        prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))
                        on:keydown=move |ev| if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key()) { ev.prevent_default(); send(); } />
                    <div class="flex flex-wrap gap-2">
                        <button class=BUTTON disabled=move || busy.get() || !choices(&text.get()).is_empty() on:click=move |_| send()>"Send (⌘↵)"</button>
                        <button class=BUTTON_QUIET disabled=move || busy.get() on:click=move |_| redraft()>{move || if busy.get() { "…" } else { "Draft again" }}</button>
                    </div>
                }.into_any(),
            }}
            <crate::compose::WorkInSession key=format!("request:{id}") about current=text set=Callback::new(move |t: String| {
                text.set(t);
                // Not drafted before: the session's draft, with where it goes.
                if draft.get_untracked().is_none() {
                    spawn_local(async move {
                        if let Ok(r) = api::rpc_as::<c6_api::Request>("memex.requests.get", json!({"id": id})).await {
                            draft.set(r.draft.filter(|d| d.error.is_none()));
                        }
                    });
                }
            }) />
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
                {move || open.get().is_some().then(|| view! { <crate::mail::Thread account=account.clone() thread=thread.clone() open compose refresh in_brief=true /> })}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_links_carry_a_positive_id() {
        assert_eq!(request_path(42), "/brief?request=42");
        assert_eq!(request_id("42"), Some(42));
        for invalid in ["", "0", "-1", "other", "9223372036854775808"] {
            assert_eq!(request_id(invalid), None);
        }
    }

    #[test]
    fn linked_requests_must_be_open() {
        for (status, until, available) in [
            ("open", None, true),
            ("snoozed", Some(99), true),
            ("snoozed", Some(100), true),
            ("snoozed", Some(101), false),
            ("snoozed", None, false),
            ("done", None, false),
            ("dropped", None, false),
        ] {
            let request = Request { id: 42, status: status.into(), until, ..Request::default() };
            assert_eq!(request_to_open(Ok(request), 100).is_ok(), available, "{status}, {until:?}");
        }
        assert_eq!(request_to_open(Err("no request 42".into()), 100), Err("This request is unavailable."));
    }

    #[test]
    fn settling_advances_only_the_request_still_selected() {
        let owner = Owner::new();
        owner.with(|| {
            let request = |id| Request { id, status: "open".into(), ..Request::default() };
            let b = Brief {
                queue: Memo::new(move |_| [1, 3, 2].into_iter().map(request).collect()),
                open: RwSignal::new(None),
                navigation: RwSignal::new(0),
                loading: RwSignal::new(false),
                notice: RwSignal::new(None),
                unfolded: RwSignal::new(None),
                tick: RwSignal::new(0),
                error: RwSignal::new(None),
            };
            b.show(Some(request(1)));
            // The link to B resolves before A's Done action completes.
            b.show(None);
            b.resolved(b.navigation.get_untracked(), Ok(request(2)), 100);
            b.gone(1);
            assert_eq!(b.open.get_untracked().unwrap().id, 2);
            assert_eq!(b.tick.get_untracked(), 1);

            b.show(Some(request(1)));
            b.gone(1);
            assert_eq!(b.open.get_untracked().unwrap().id, 3);
            assert_eq!(b.tick.get_untracked(), 2);

            b.show(None);
            b.gone(3);
            assert!(b.open.get_untracked().is_none());
            assert_eq!(b.tick.get_untracked(), 3);
        });
    }

    #[test]
    fn linked_requests_do_not_depend_on_the_queue_or_leave_invisible_targets() {
        let owner = Owner::new();
        owner.with(|| {
            let b = Brief {
                queue: Memo::new(|_| (1..=100).map(|id| Request { id, status: "open".into(), ..Request::default() }).collect()),
                open: RwSignal::new(None),
                navigation: RwSignal::new(0),
                loading: RwSignal::new(false),
                notice: RwSignal::new(None),
                unfolded: RwSignal::new(Some("c:7".into())),
                tick: RwSignal::new(0),
                error: RwSignal::new(None),
            };
            b.loading.set(true);
            b.resolved(0, Ok(Request { id: 101, status: "open".into(), ..Request::default() }), 100);
            assert_eq!(b.open.get_untracked().unwrap().id, 101);
            assert!(!b.loading.get_untracked());

            for result in [
                Err("no request 42".into()),
                Ok(Request { id: 42, status: "done".into(), ..Request::default() }),
                Ok(Request { id: 42, status: "snoozed".into(), until: Some(101), ..Request::default() }),
            ] {
                b.show(None);
                b.loading.set(true);
                b.resolved(b.navigation.get_untracked(), result, 100);
                assert!(b.open.get_untracked().is_none());
                assert!(!b.loading.get_untracked());
                assert!(b.notice.get_untracked().is_some());
                b.show(b.queue.get_untracked().get(4).cloned());
                assert_eq!(b.open.get_untracked().unwrap().id, 5);
                b.show(None);
            }
            // Escape or opening a row also cancels an earlier lookup.
            let navigation = b.navigation.get_untracked();
            b.show(None);
            b.resolved(navigation, Ok(Request { id: 101, status: "open".into(), ..Request::default() }), 100);
            assert!(b.open.get_untracked().is_none());
            assert_eq!(b.unfolded.get_untracked().as_deref(), Some("c:7"));
            assert_eq!(b.tick.get_untracked(), 0);
        });
    }
}

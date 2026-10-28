//! Chat (docs/chat.md): a realm's Slack as a client like Slack's own —
//! workspaces, a sidebar of sections with unreads and mentions, the
//! conversation, threads, Unreads, Threads, Activity, Saved and Search,
//! and a composer — over the memex's `chat` items, writing through the
//! slack app's operations. And Settings → Slack.

mod composer;
mod message;
mod settings;
mod text;
mod views;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};

pub use settings::SlackSettings;

use crate::api;
use crate::util::{ErrorBox, storage_get, storage_set};
use composer::Composer;
use message::MessageList;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Place {
    pub workspace: String,
    pub channel: String,
    pub name: String,
}

/// What the middle shows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Pane {
    Home,
    /// A conversation, at a message (its ts) or the newest.
    Conv(Place, Option<String>),
    Unreads,
    Threads,
    Activity,
    Saved,
    Search(String),
}

/// The Chat page's state, shared by its parts.
#[derive(Clone, Copy)]
pub(crate) struct Chat {
    pub tick: RwSignal<u32>,
    pub pane: RwSignal<Pane>,
    pub thread: RwSignal<Option<(Place, String)>>,
    pub details: RwSignal<bool>,
    pub channels: RwSignal<Vec<Value>>,
    pub people: RwSignal<Vec<Value>>,
    pub sections: RwSignal<Value>,
    /// The message being edited (its id).
    pub editing: RwSignal<Option<String>>,
    pub error: RwSignal<Option<String>>,
    pub threads_new: RwSignal<usize>,
    /// The workspace shown (the rail picks it).
    pub workspace: RwSignal<Option<String>>,
    /// The sidebar's conversations, in its order (for ⌥↑/↓).
    pub order: RwSignal<Vec<Place>>,
}

impl Chat {
    fn new() -> Self {
        let c = Chat {
            tick: RwSignal::new(0),
            pane: RwSignal::new(Pane::Home),
            thread: RwSignal::new(None),
            details: RwSignal::new(false),
            channels: RwSignal::new(Vec::new()),
            people: RwSignal::new(Vec::new()),
            sections: RwSignal::new(json!({})),
            editing: RwSignal::new(None),
            error: RwSignal::new(None),
            threads_new: RwSignal::new(0),
            workspace: RwSignal::new(None),
            order: RwSignal::new(Vec::new()),
        };
        // One reload at a time: asked again meanwhile, once more after it.
        let busy = StoredValue::new((false, false));
        Effect::new(move || {
            let n = c.tick.get();
            if busy.get_value().0 {
                busy.update_value(|b| b.1 = true);
                return;
            }
            busy.set_value((true, false));
            spawn_local(async move {
                if let Ok(v) = api::rpc("memex.chat.channels", json!({})).await {
                    c.channels.set(v.as_array().cloned().unwrap_or_default());
                }
                if n.is_multiple_of(10)
                    && let Ok(v) = api::rpc("memex.chat.people", json!({})).await
                {
                    c.people.set(v.as_array().cloned().unwrap_or_default());
                }
                if let Ok(v) = api::rpc("memex.chat.sections", json!({})).await {
                    c.sections.set(v);
                }
                if let Ok(v) = api::rpc("memex.chat.threads", json!({"limit": 100})).await {
                    c.threads_new.set(v.as_array().map_or(0, |l| l.iter().filter(|t| t["new"].as_u64().unwrap_or(0) > 0).count()));
                }
                let again = busy.get_value().1;
                busy.set_value((false, false));
                if again {
                    c.refresh();
                }
            });
        });
        c
    }

    /// The page's, or (embedded elsewhere, as in the Brief) a new one.
    pub fn get() -> Self {
        use_context::<Chat>().unwrap_or_else(|| {
            let c = Chat::new();
            provide_context(c);
            c
        })
    }

    pub fn refresh(&self) {
        self.tick.update(|t| *t += 1);
    }

    /// Calls a memex method, then shows what changed (or the error).
    pub fn act(&self, method: &'static str, params: Value) {
        let c = *self;
        spawn_local(async move {
            match api::rpc(method, params).await {
                Ok(_) => {
                    c.error.set(None);
                    c.refresh();
                }
                Err(e) => c.error.set(Some(e)),
            }
        });
    }

    pub fn person(&self, workspace: &str, id: &str) -> Option<Value> {
        self.people.with(|p| p.iter().find(|x| x["workspace"] == workspace && x["id"] == id).cloned())
    }

    pub fn me(&self, workspace: &str) -> Option<Value> {
        self.people.with(|p| p.iter().find(|x| x["workspace"] == workspace && x["me"] == true).cloned())
    }

    pub fn conversation(&self, workspace: &str, channel: &str) -> Option<Value> {
        self.channels.with(|l| l.iter().find(|c| c["workspace"] == workspace && c["channel"] == channel).cloned())
    }

    pub fn place(&self, workspace: &str, channel: &str) -> Place {
        let name = self
            .conversation(workspace, channel)
            .and_then(|c| c["channel_name"].as_str().map(str::to_owned))
            .unwrap_or_else(|| channel.to_owned());
        Place { workspace: workspace.to_owned(), channel: channel.to_owned(), name }
    }

    /// Opens a conversation (at a message).
    pub fn open(&self, place: Place, at: Option<String>) {
        // Another conversation's thread closes.
        if self.thread.with_untracked(|t| t.as_ref().is_some_and(|(p, _)| p.channel != place.channel || p.workspace != place.workspace)) {
            self.thread.set(None);
        }
        self.pane.set(Pane::Conv(place, at));
    }

    /// What a message's text can point at, in a workspace.
    pub fn known(&self, workspace: &str) -> text::Known {
        let me = self.me(workspace).and_then(|p| p["name"].as_str().map(str::to_owned)).unwrap_or_default();
        let people = self
            .people
            .with(|p| p.iter().filter(|x| x["workspace"] == workspace).filter_map(|x| x["name"].as_str().map(str::to_owned)).collect());
        let channels = self.channels.with(|l| {
            l.iter()
                .filter(|c| c["workspace"] == workspace && matches!(c["channel_kind"].as_str(), Some("public" | "private")))
                .filter_map(|c| Some((c["channel_name"].as_str()?.trim_start_matches('#').to_owned(), c["channel"].as_str()?.to_owned())))
                .collect()
        });
        text::Known { me, people, channels }
    }

    /// The workspaces, in the order their conversations were last active.
    fn workspaces(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        self.channels.with(|l| {
            for c in l {
                let w = (c["workspace"].as_str().unwrap_or("").to_owned(), c["workspace_name"].as_str().unwrap_or("").to_owned());
                if !out.iter().any(|x| x.0 == w.0) {
                    out.push(w);
                }
            }
        });
        out
    }

    fn shown_workspace(&self) -> Option<String> {
        self.workspace.get().or_else(|| self.workspaces().first().map(|w| w.0.clone()))
    }
}

/// A conversation's icon: # public, a lock private, a DM's avatar or
/// initial, a group's count.
#[component]
pub(crate) fn ConvIcon(c: Value) -> impl IntoView {
    let kind = c["channel_kind"].as_str().unwrap_or("public").to_owned();
    match kind.as_str() {
        "dm" => match c["user_avatar"].as_str().filter(|a| !a.is_empty()) {
            Some(a) => view! { <img src=a.to_owned() alt="" class="h-4 w-4 shrink-0 rounded" /> }.into_any(),
            None => view! { <span class="flex h-4 w-4 shrink-0 items-center justify-center rounded bg-zinc-300 text-[9px] font-semibold dark:bg-zinc-700">{initial(c["channel_name"].as_str().unwrap_or("?").trim_start_matches('@'))}</span> }.into_any(),
        },
        "group" => {
            let n = c["channel_name"].as_str().unwrap_or("").split(", ").count();
            view! { <span class="flex h-4 w-4 shrink-0 items-center justify-center rounded bg-zinc-200 text-[9px] font-semibold dark:bg-zinc-800">{n.to_string()}</span> }.into_any()
        }
        "private" => view! { <span class="w-4 shrink-0 text-center text-xs text-zinc-500">"🔒"</span> }.into_any(),
        _ => view! { <span class="w-4 shrink-0 text-center text-zinc-500">"#"</span> }.into_any(),
    }
}

pub(crate) fn initial(name: &str) -> String {
    name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into())
}

/// A conversation's name for the sidebar: a channel's without its #.
fn label(c: &Value) -> String {
    let n = c["channel_name"].as_str().unwrap_or("");
    match c["channel_kind"].as_str() {
        Some("public" | "private") => n.trim_start_matches('#').to_owned(),
        Some("dm") => n.trim_start_matches('@').to_owned(),
        _ => n.to_owned(),
    }
}

fn place_of(c: &Value) -> Place {
    Place {
        workspace: c["workspace"].as_str().unwrap_or("").to_owned(),
        channel: c["channel"].as_str().unwrap_or("").to_owned(),
        name: c["channel_name"].as_str().unwrap_or("").to_owned(),
    }
}

/// Listens for keys before anything else on the page (⌘K is the
/// sessions search elsewhere; here it's the switcher).
fn first_keys(f: impl Fn(web_sys::KeyboardEvent) + 'static) -> impl FnOnce() {
    use wasm_bindgen::JsCast;
    let cb = wasm_bindgen::closure::Closure::<dyn Fn(web_sys::KeyboardEvent)>::new(f);
    let _ = window().add_event_listener_with_callback_and_bool("keydown", cb.as_ref().unchecked_ref(), true);
    move || {
        let _ = window().remove_event_listener_with_callback_and_bool("keydown", cb.as_ref().unchecked_ref(), true);
        drop(cb);
    }
}

fn in_field(ev: &web_sys::KeyboardEvent) -> bool {
    ev.target()
        .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
        .is_some_and(|el| matches!(el.tag_name().as_str(), "TEXTAREA" | "INPUT" | "SELECT"))
}

/// The Chat page.
#[component]
pub fn ChatPage() -> impl IntoView {
    let chat = Chat::new();
    provide_context(chat);
    let switcher = RwSignal::new(false);
    // Fresher while you look: a sync now and then, and everything again.
    let handle = set_interval_with_handle(
        move || {
            spawn_local(async move {
                let _ = api::slack_sync().await;
            });
            chat.refresh();
        },
        std::time::Duration::from_secs(30),
    )
    .ok();
    let stop = send_wrapper::SendWrapper::new(first_keys(move |ev| {
        let key = ev.key();
        if (ev.meta_key() || ev.ctrl_key()) && key == "k" {
            ev.prevent_default();
            ev.stop_immediate_propagation();
            switcher.update(|s| *s = !*s);
            return;
        }
        if ev.alt_key() && (key == "ArrowUp" || key == "ArrowDown") {
            ev.prevent_default();
            let order = chat.order.get_untracked();
            let unread_only = ev.shift_key();
            let here = match chat.pane.get_untracked() {
                Pane::Conv(p, _) => order.iter().position(|x| x.channel == p.channel && x.workspace == p.workspace),
                _ => None,
            };
            let unread = |p: &Place| chat.conversation(&p.workspace, &p.channel).is_some_and(|c| c["unread"].as_i64().unwrap_or(0) > 0);
            let next = if key == "ArrowDown" {
                order.iter().enumerate().skip(here.map_or(0, |h| h + 1)).find(|(_, p)| !unread_only || unread(p))
            } else {
                order.iter().enumerate().take(here.unwrap_or(order.len())).rev().find(|(_, p)| !unread_only || unread(p))
            };
            if let Some((_, p)) = next {
                chat.open(p.clone(), None);
            }
            return;
        }
        if key == "Escape" && !in_field(&ev) {
            if switcher.get_untracked() {
                switcher.set(false);
            } else if chat.thread.get_untracked().is_some() {
                chat.thread.set(None);
            } else if chat.details.get_untracked() {
                chat.details.set(false);
            }
        }
    }));
    on_cleanup(move || {
        if let Some(h) = handle {
            h.clear();
        }
        (stop.take())();
    });
    view! {
        <main class="relative flex h-full min-h-0 text-sm">
            {move || (chat.workspaces().len() > 1).then(|| view! { <Rail /> })}
            <Sidebar />
            <section class="flex min-w-0 flex-1 flex-col">
                {move || chat.error.get().map(|e| view! { <div class="p-2"><ErrorBox message=e /></div> })}
                {move || match chat.pane.get() {
                    Pane::Home => view! { <Welcome /> }.into_any(),
                    Pane::Conv(p, at) => view! { <ConversationPane place=p at /> }.into_any(),
                    Pane::Unreads => view! { <views::Unreads /> }.into_any(),
                    Pane::Threads => view! { <views::Threads /> }.into_any(),
                    Pane::Activity => view! { <views::Activity /> }.into_any(),
                    Pane::Saved => view! { <views::Saved /> }.into_any(),
                    Pane::Search(q) => view! { <views::SearchResults q /> }.into_any(),
                }}
            </section>
            {move || chat.thread.get().map(|(p, t)| view! {
                <aside class="flex w-[26rem] shrink-0 flex-col border-l border-zinc-200 dark:border-zinc-800">
                    <ThreadPane place=p thread_ts=t />
                </aside>
            })}
            {move || (chat.details.get() && chat.thread.get().is_none()).then(|| match chat.pane.get() {
                Pane::Conv(p, _) => Some(view! {
                    <aside class="flex w-80 shrink-0 flex-col overflow-y-auto border-l border-zinc-200 dark:border-zinc-800">
                        <views::Details place=p />
                    </aside>
                }),
                _ => None,
            })}
            {move || switcher.get().then(|| view! { <Switcher open=switcher /> })}
        </main>
    }
}

#[component]
fn Welcome() -> impl IntoView {
    let chat = Chat::get();
    view! {
        <div class="m-auto max-w-md space-y-2 p-6 text-center text-zinc-500">
            {move || if chat.channels.with(Vec::is_empty) {
                view! { <p>"Nothing yet. Connect a workspace in Settings → Slack; its conversations arrive as they're imported."</p> }.into_any()
            } else {
                view! {
                    <p class="text-zinc-700 dark:text-zinc-300">"Pick a conversation, or see what's new."</p>
                    <p class="text-xs">"⌘K jump to a conversation · ⌥↑/↓ the one above or below · ⌥⇧↑/↓ the next unread · Esc closes a thread"</p>
                }.into_any()
            }}
        </div>
    }
}

/// Workspaces, when there's more than one.
#[component]
fn Rail() -> impl IntoView {
    let chat = Chat::get();
    view! {
        <nav class="flex w-14 shrink-0 flex-col items-center gap-2 border-r border-zinc-200 bg-zinc-100 py-2 dark:border-zinc-800 dark:bg-zinc-950">
            {move || chat.workspaces().into_iter().map(|(id, name)| {
                let (here, go) = (id.clone(), id.clone());
                let unread = chat.channels.with(|l| l.iter().any(|c| c["workspace"] == id.as_str() && c["unread"].as_i64().unwrap_or(0) > 0 && c["muted"] != true));
                view! {
                    <button title=name.clone() on:click=move |_| { chat.workspace.set(Some(go.clone())); chat.pane.set(Pane::Home); }
                        class=move || format!("relative flex h-9 w-9 items-center justify-center rounded-lg font-semibold {}",
                            if chat.shown_workspace().as_deref() == Some(here.as_str()) { "bg-zinc-900 text-white ring-2 ring-zinc-400 dark:bg-zinc-100 dark:text-zinc-900" } else { "bg-zinc-300 hover:bg-zinc-400 dark:bg-zinc-700" })>
                        {initial(&name)}
                        {unread.then(|| view! { <span class="absolute -right-0.5 -top-0.5 h-2.5 w-2.5 rounded-full border-2 border-zinc-100 bg-sky-500 dark:border-zinc-950"></span> })}
                    </button>
                }
            }).collect_view()}
        </nav>
    }
}

/// A section's sort and whether it's folded, kept in this browser.
fn section_pref(section: &str, what: &str, default: &str) -> String {
    storage_get(&format!("c6.chat.{what}.{section}")).unwrap_or_else(|| default.to_owned())
}

#[component]
fn Sidebar() -> impl IntoView {
    let chat = Chat::get();
    let filter = RwSignal::new(String::new());
    let unreads_only = RwSignal::new(storage_get("c6.chat.unreads_only").as_deref() == Some("1"));
    let search = RwSignal::new(String::new());
    let prefs_tick = RwSignal::new(0u32);
    // The sections, their conversations, in order.
    let grouped = Memo::new(move |_| {
        prefs_tick.track();
        let ws = chat.shown_workspace();
        let f = filter.get().to_lowercase();
        let only = unreads_only.get();
        let open = match chat.pane.get() {
            Pane::Conv(p, _) => Some(p.channel),
            _ => None,
        };
        let list: Vec<Value> = chat.channels.with(|l| {
            l.iter()
                .filter(|c| ws.as_deref().is_none_or(|w| c["workspace"] == w))
                .filter(|c| if f.is_empty() { c["hidden"] != true } else { label(c).to_lowercase().contains(&f) })
                .filter(|c| !only || c["unread"].as_i64().unwrap_or(0) > 0 || open.as_deref() == c["channel"].as_str())
                .cloned()
                .collect()
        });
        let mut names: Vec<String> = ws
            .as_deref()
            .and_then(|w| chat.sections.with(|s| s[w].as_array().cloned()))
            .unwrap_or_default()
            .iter()
            .filter_map(|n| n.as_str().map(str::to_owned))
            .collect();
        for c in &list {
            if let Some(s) = c["section"].as_str().filter(|s| !s.is_empty())
                && !names.iter().any(|n| n == s)
            {
                names.push(s.to_owned());
            }
        }
        let mut groups: Vec<(String, &'static str, Vec<Value>)> = Vec::new();
        let starred: Vec<Value> = list.iter().filter(|c| c["starred"] == true).cloned().collect();
        groups.push(("Starred".into(), "az", starred));
        let free = |c: &&Value| c["starred"] != true && c["section"].as_str().unwrap_or("").is_empty();
        for n in &names {
            groups.push((n.clone(), "az", list.iter().filter(|c| c["starred"] != true && c["section"] == n.as_str()).cloned().collect()));
        }
        groups.push((
            "Channels".into(),
            "az",
            list.iter().filter(free).filter(|c| matches!(c["channel_kind"].as_str(), Some("public" | "private"))).cloned().collect(),
        ));
        groups.push((
            "Direct messages".into(),
            "recent",
            list.iter()
                .filter(free)
                .filter(|c| matches!(c["channel_kind"].as_str(), Some("dm" | "group")) && c["user_bot"] != true)
                .cloned()
                .collect(),
        ));
        groups.push(("Apps".into(), "az", list.iter().filter(free).filter(|c| c["user_bot"] == true).cloned().collect()));
        groups
            .into_iter()
            .filter(|(n, _, l)| !l.is_empty() || names.contains(n))
            .map(|(n, default, mut l)| {
                if section_pref(&n, "sort", default) == "az" {
                    l.sort_by_key(|c| label(c).to_lowercase());
                } else {
                    l.sort_by_key(|c| std::cmp::Reverse(c["last_at"].as_i64().unwrap_or(0)));
                }
                (n, l)
            })
            .collect::<Vec<_>>()
    });
    // The order ⌥↑/↓ follows: what's showing.
    Effect::new(move || {
        let g = grouped.get();
        let order = g.iter().filter(|(n, _)| section_pref(n, "fold", "0") != "1").flat_map(|(_, l)| l.iter().map(place_of)).collect();
        chat.order.set(order);
    });
    let total = move |key: &str| {
        let ws = chat.shown_workspace();
        chat.channels.with(|l| {
            l.iter()
                .filter(|c| ws.as_deref().is_none_or(|w| c["workspace"] == w) && c["muted"] != true && c["hidden"] != true)
                .map(|c| c[key].as_i64().unwrap_or(0))
                .sum::<i64>()
        })
    };
    let nav = move |label: &'static str, icon: &'static str, pane: Pane, count: Signal<i64>, strong: bool| {
        let (here, go) = (pane.clone(), pane);
        view! {
            <button class=move || format!("flex w-full items-center gap-2 rounded-md px-2 py-1 text-left {}",
                    if chat.pane.with(|p| *p == here) { "bg-sky-700 text-white" } else { "hover:bg-zinc-200/70 dark:hover:bg-zinc-800" })
                on:click=move |_| chat.pane.set(go.clone())>
                <span class="w-4 text-center">{icon}</span>
                <span class=move || format!("flex-1 {}", if count.get() > 0 { "font-semibold" } else { "" })>{label}</span>
                {move || (count.get() > 0).then(|| view! {
                    <span class=if strong { "rounded-full bg-red-600 px-1.5 text-[10px] font-semibold text-white" } else { "text-[11px] text-zinc-500" }>{count.get().to_string()}</span>
                })}
            </button>
        }
    };
    view! {
        <nav class="flex w-64 shrink-0 flex-col border-r border-zinc-200 bg-zinc-50 dark:border-zinc-800 dark:bg-zinc-900/40">
            <div class="space-y-1.5 border-b border-zinc-200 p-2 dark:border-zinc-800">
                <div class="flex items-center gap-2 px-1">
                    <span class="min-w-0 flex-1 truncate font-semibold">{move || {
                        let ws = chat.shown_workspace();
                        chat.workspaces().into_iter().find(|w| Some(&w.0) == ws.as_ref()).map(|w| w.1).unwrap_or_else(|| "Chat".into())
                    }}</span>
                    <button title="Show only unread conversations"
                        class=move || format!("rounded px-1.5 text-[11px] {}", if unreads_only.get() { "bg-sky-700 text-white" } else { "text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-800" })
                        on:click=move |_| { let v = !unreads_only.get_untracked(); unreads_only.set(v); storage_set("c6.chat.unreads_only", if v { "1" } else { "0" }); }>
                        "Unreads only"
                    </button>
                </div>
                <input class="w-full rounded-md border border-zinc-300 bg-white px-2 py-1 text-xs dark:border-zinc-700 dark:bg-zinc-900" type="search"
                    placeholder="Search messages (in: from: has:)" prop:value=move || search.get()
                    on:input=move |ev| search.set(event_target_value(&ev))
                    on:keydown=move |ev: web_sys::KeyboardEvent| if ev.key() == "Enter" && !search.get_untracked().trim().is_empty() { chat.pane.set(Pane::Search(search.get_untracked())); } />
            </div>
            <div class="min-h-0 flex-1 space-y-3 overflow-y-auto p-2">
                <div class="space-y-0.5">
                    {nav("Unreads", "≡", Pane::Unreads, Signal::derive(move || total("unread")), false)}
                    {nav("Threads", "💬", Pane::Threads, Signal::derive(move || chat.threads_new.get() as i64), true)}
                    {nav("Activity", "@", Pane::Activity, Signal::derive(move || total("mentions")), true)}
                    {nav("Saved", "🔖", Pane::Saved, Signal::derive(|| 0), false)}
                </div>
                <input class="w-full rounded-md border border-zinc-200 bg-white px-2 py-1 text-xs dark:border-zinc-800 dark:bg-zinc-900" type="search"
                    placeholder="Filter conversations" prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev)) />
                {move || grouped.get().into_iter().map(|(name, list)| view! { <Section name list prefs_tick /> }).collect_view()}
                <button class="px-2 text-xs text-zinc-500 hover:underline" on:click=move |_| {
                    if let Some(w) = chat.shown_workspace()
                        && let Ok(Some(n)) = window().prompt_with_message("A new section's name")
                        && !n.trim().is_empty()
                    {
                        let mut names: Vec<String> = chat.sections.with(|s| s[&w].as_array().cloned()).unwrap_or_default().iter().filter_map(|x| x.as_str().map(str::to_owned)).collect();
                        names.push(n.trim().to_owned());
                        chat.act("memex.chat.sections", json!({"workspace": w, "names": names}));
                    }
                }>"+ New section"</button>
            </div>
        </nav>
    }
}

#[component]
fn Section(name: String, list: Vec<Value>, prefs_tick: RwSignal<u32>) -> impl IntoView {
    let chat = Chat::get();
    let folded = section_pref(&name, "fold", "0") == "1";
    let sort = section_pref(&name, "sort", if name == "Direct messages" { "recent" } else { "az" });
    let custom = !matches!(name.as_str(), "Starred" | "Channels" | "Direct messages" | "Apps");
    let (n1, n2, n3) = (name.clone(), name.clone(), name.clone());
    let unread_folded: Vec<Value> =
        if folded { list.iter().filter(|c| c["unread"].as_i64().unwrap_or(0) > 0).cloned().collect() } else { Vec::new() };
    let shown = if folded { unread_folded } else { list };
    view! {
        <div>
            <div class="group flex items-center gap-1 px-1 text-[11px] font-semibold uppercase tracking-wide text-zinc-500">
                <button class="flex flex-1 items-center gap-1 text-left hover:text-zinc-900 dark:hover:text-zinc-100" on:click=move |_| {
                    storage_set(&format!("c6.chat.fold.{n1}"), if folded { "0" } else { "1" });
                    prefs_tick.update(|t| *t += 1);
                }>
                    <span class="w-3">{if folded { "▸" } else { "▾" }}</span>{name.clone()}
                </button>
                <button class="hidden text-[10px] normal-case group-hover:inline hover:underline" title="Sort"
                    on:click=move |_| {
                        storage_set(&format!("c6.chat.sort.{n2}"), if sort == "az" { "recent" } else { "az" });
                        prefs_tick.update(|t| *t += 1);
                    }>{if section_pref(&name, "sort", "az") == "az" { "A–Z" } else { "Recent" }}</button>
                {custom.then(|| view! {
                    <button class="hidden text-[10px] normal-case group-hover:inline hover:underline" title="Delete the section (its conversations go back)"
                        on:click=move |_| {
                            if let Some(w) = chat.shown_workspace() {
                                let names: Vec<String> = chat.sections.with(|s| s[&w].as_array().cloned()).unwrap_or_default().iter().filter_map(|x| x.as_str().map(str::to_owned)).filter(|x| *x != n3).collect();
                                for c in chat.channels.get_untracked().iter().filter(|c| c["section"] == n3.as_str()) {
                                    chat.act("memex.chat.prefs", json!({"workspace": c["workspace"], "channel": c["channel"], "section": ""}));
                                }
                                chat.act("memex.chat.sections", json!({"workspace": w, "names": names}));
                            }
                        }>"✕"</button>
                })}
            </div>
            <div class="mt-0.5 space-y-px">
                {shown.into_iter().map(|c| view! { <ConvRow c /> }).collect_view()}
            </div>
        </div>
    }
}

#[component]
fn ConvRow(c: Value) -> impl IntoView {
    let chat = Chat::get();
    let p = place_of(&c);
    let (here, go) = (p.clone(), p.clone());
    let unread = c["unread"].as_i64().unwrap_or(0);
    let mentions = c["mentions"].as_i64().unwrap_or(0);
    let muted = c["muted"] == true;
    let menu = RwSignal::new(false);
    let status = c["user_status"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let name = label(&c);
    let icon = c.clone();
    view! {
        <div class="group relative">
            <button
                class=move || format!("flex w-full items-center gap-2 rounded-md px-2 py-0.5 text-left {} {}",
                    if matches!(chat.pane.get(), Pane::Conv(ref x, _) if *x == here) { "bg-sky-700 text-white" } else { "hover:bg-zinc-200/70 dark:hover:bg-zinc-800" },
                    if muted { "opacity-50" } else { "" })
                on:click=move |_| chat.open(go.clone(), None)>
                <ConvIcon c=icon />
                <span class=format!("min-w-0 flex-1 truncate {}", if unread > 0 && !muted { "font-semibold" } else { "" })>{name}</span>
                {status.map(|s| view! { <span class="max-w-16 truncate text-[10px] opacity-70" title=s.clone()>{s.split(' ').next().map(text::face_of).unwrap_or_default()}</span> })}
                {(mentions > 0).then(|| view! { <span class="rounded-full bg-red-600 px-1.5 text-[10px] font-semibold text-white">{mentions.to_string()}</span> })}
            </button>
            <button class="absolute right-1 top-0.5 hidden rounded px-1 text-xs text-zinc-500 hover:bg-zinc-300 group-hover:block dark:hover:bg-zinc-700"
                title="Options" on:click=move |_| menu.update(|m| *m = !*m)>"⋯"</button>
            {move || menu.get().then(|| view! { <ConvMenu c=c.clone() close=menu /> })}
        </div>
    }
}

/// A conversation's options: star, mute, mark read, move to a section,
/// hide.
#[component]
fn ConvMenu(c: Value, close: RwSignal<bool>) -> impl IntoView {
    let chat = Chat::get();
    let (w, ch) = (c["workspace"].clone(), c["channel"].clone());
    let set = move |k: &'static str, v: Value| {
        chat.act("memex.chat.prefs", json!({"workspace": w, "channel": ch, k: v}));
        close.set(false);
    };
    let (starred, muted) = (c["starred"] == true, c["muted"] == true);
    let names: Vec<String> = chat
        .sections
        .with(|s| s[c["workspace"].as_str().unwrap_or("")].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|x| x.as_str().map(str::to_owned))
        .collect();
    let section = c["section"].as_str().unwrap_or("").to_owned();
    let item = "block w-full px-3 py-1 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800";
    let read = c.clone();
    view! {
        <div class="absolute right-0 top-6 z-30 w-52 rounded-md border border-zinc-200 bg-white py-1 text-xs shadow-lg dark:border-zinc-700 dark:bg-zinc-900"
            on:mouseleave=move |_| close.set(false)>
            <button class=item on:click={ let set = set.clone(); move |_| set("starred", json!(!starred)) }>{if starred { "Unstar" } else { "Star" }}</button>
            <button class=item on:click={ let set = set.clone(); move |_| set("muted", json!(!muted)) }>{if muted { "Unmute" } else { "Mute" }}</button>
            {(read["unread"].as_i64().unwrap_or(0) > 0).then(|| {
                let p = place_of(&read);
                view! { <button class=item on:click=move |_| { views::mark_all_read(chat, p.clone()); close.set(false); }>"Mark as read"</button> }
            })}
            <div class="my-1 border-t border-zinc-200 dark:border-zinc-700"></div>
            <div class="px-3 py-0.5 text-[10px] uppercase text-zinc-500">"Move to"</div>
            {names.into_iter().filter(|n| *n != section).map(|n| {
                let set = set.clone();
                let label = n.clone();
                view! { <button class=item on:click=move |_| set("section", json!(n.clone()))>{label}</button> }
            }).collect_view()}
            {(!section.is_empty()).then(|| { let set = set.clone(); view! { <button class=item on:click=move |_| set("section", json!(""))>"Back to its own section"</button> } })}
            <button class=item on:click={ let set = set.clone(); let ws = c["workspace"].as_str().unwrap_or("").to_owned(); move |_| {
                if let Ok(Some(n)) = window().prompt_with_message("A new section's name") && !n.trim().is_empty() {
                    let mut names: Vec<String> = chat.sections.with(|s| s[&ws].as_array().cloned()).unwrap_or_default().iter().filter_map(|x| x.as_str().map(str::to_owned)).collect();
                    names.push(n.trim().to_owned());
                    chat.act("memex.chat.sections", json!({"workspace": ws, "names": names}));
                    set("section", json!(n.trim()));
                }
            }}>"New section…"</button>
            <div class="my-1 border-t border-zinc-200 dark:border-zinc-700"></div>
            <button class=item on:click={ let set = set.clone(); move |_| set("hidden", json!(c["hidden"] != true)) }>{if c["hidden"] == true { "Show in the sidebar" } else { "Hide (the filter still finds it)" }}</button>
        </div>
    }
}

/// ⌘K: jump to a conversation or a person's DM.
#[component]
fn Switcher(open: RwSignal<bool>) -> impl IntoView {
    let chat = Chat::get();
    let q = RwSignal::new(String::new());
    let picked = RwSignal::new(0usize);
    let input = NodeRef::<leptos::html::Input>::new();
    Effect::new(move || {
        if let Some(el) = input.get() {
            let _ = el.focus();
        }
    });
    let found = Memo::new(move |_| {
        let q = q.get().to_lowercase();
        let mut list: Vec<Value> =
            chat.channels.with(|l| l.iter().filter(|c| q.is_empty() || label(c).to_lowercase().contains(&q)).cloned().collect());
        // Unread first, then the most recent; names starting with it before
        // those containing it.
        list.sort_by_key(|c| {
            (
                !label(c).to_lowercase().starts_with(&q),
                c["unread"].as_i64().unwrap_or(0) == 0,
                std::cmp::Reverse(c["last_at"].as_i64().unwrap_or(0)),
            )
        });
        list.truncate(12);
        list
    });
    let go = move |c: &Value| {
        chat.open(place_of(c), None);
        open.set(false);
    };
    view! {
        <div class="absolute inset-0 z-40 flex items-start justify-center bg-black/30 pt-24" on:click=move |_| open.set(false)>
            <div class="w-[32rem] overflow-hidden rounded-lg border border-zinc-200 bg-white shadow-2xl dark:border-zinc-700 dark:bg-zinc-900" on:click=|ev| ev.stop_propagation()>
                <input node_ref=input class="w-full border-b border-zinc-200 bg-transparent px-4 py-3 text-base outline-none dark:border-zinc-700"
                    placeholder="Jump to a conversation…" prop:value=move || q.get()
                    on:input=move |ev| { q.set(event_target_value(&ev)); picked.set(0); }
                    on:keydown=move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
                        "ArrowDown" => { ev.prevent_default(); picked.update(|p| *p = (*p + 1).min(found.get_untracked().len().saturating_sub(1))); }
                        "ArrowUp" => { ev.prevent_default(); picked.update(|p| *p = p.saturating_sub(1)); }
                        "Enter" => { if let Some(c) = found.get_untracked().get(picked.get_untracked()) { go(c); } }
                        "Escape" => open.set(false),
                        _ => {}
                    } />
                <div class="max-h-96 overflow-y-auto py-1">
                    {move || found.get().into_iter().enumerate().map(|(i, c)| {
                        let pick = c.clone();
                        let unread = c["unread"].as_i64().unwrap_or(0);
                        let ws = c["workspace_name"].as_str().unwrap_or("").to_owned();
                        let icon = c.clone();
                        view! {
                            <button class=move || format!("flex w-full items-center gap-2 px-4 py-1.5 text-left {}", if picked.get() == i { "bg-sky-700 text-white" } else { "hover:bg-zinc-100 dark:hover:bg-zinc-800" })
                                on:click=move |_| go(&pick)>
                                <ConvIcon c=icon />
                                <span class=format!("flex-1 truncate {}", if unread > 0 { "font-semibold" } else { "" })>{label(&c)}</span>
                                <span class="text-[11px] opacity-60">{ws}</span>
                            </button>
                        }
                    }).collect_view()}
                </div>
            </div>
        </div>
    }
}

/// A conversation: its header, messages and the box to post.
#[component]
fn ConversationPane(place: Place, at: Option<String>) -> impl IntoView {
    let chat = Chat::get();
    let p = place.clone();
    let conv = Memo::new(move |_| chat.conversation(&p.workspace, &p.channel));
    let header = place.clone();
    let star = place.clone();
    view! {
        <header class="flex items-center gap-2 border-b border-zinc-200 px-4 py-2 dark:border-zinc-800">
            {move || conv.get().map(|c| view! { <ConvIcon c /> })}
            <span class="font-semibold">{label(&json!({"channel_name": header.name, "channel_kind": conv.with(|c| c.as_ref().map(|c| c["channel_kind"].clone()).unwrap_or_default())}))}</span>
            {move || conv.get().map(|c| {
                let starred = c["starred"] == true;
                let (w, ch) = (star.workspace.clone(), star.channel.clone());
                view! {
                    <button class="text-sm text-zinc-400 hover:text-amber-500" title=if starred { "Unstar" } else { "Star" }
                        on:click=move |_| chat.act("memex.chat.prefs", json!({"workspace": w, "channel": ch, "starred": !starred}))>
                        {if starred { "★" } else { "☆" }}
                    </button>
                    {(c["muted"] == true).then(|| view! { <span class="rounded bg-zinc-200 px-1 text-[10px] text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400">"muted"</span> })}
                    <span class="min-w-0 flex-1 truncate text-xs text-zinc-500" title=c["topic"].as_str().unwrap_or("").to_owned()>{c["topic"].as_str().unwrap_or("").to_owned()}</span>
                    {c["members"].as_i64().map(|n| view! { <span class="text-xs text-zinc-500" title="Members">{format!("👤 {n}")}</span> })}
                    {c["user_status"].as_str().filter(|s| !s.is_empty()).map(|s| view! { <span class="text-xs text-zinc-500">{text::status(s)}</span> })}
                }
            })}
            <span class="flex-1"></span>
            <button class=move || format!("rounded px-2 py-0.5 text-xs {}", if chat.details.get() { "bg-zinc-200 dark:bg-zinc-800" } else { "text-zinc-500 hover:bg-zinc-100 dark:hover:bg-zinc-800" })
                title="Details" on:click=move |_| { chat.thread.set(None); chat.details.update(|d| *d = !*d); }>"ⓘ Details"</button>
        </header>
        <MessageList place=place.clone() at />
        <Composer place thread_ts=None />
    }
}

/// A thread: its first message, the replies, and a box to reply.
#[component]
fn ThreadPane(place: Place, thread_ts: String) -> impl IntoView {
    let chat = Chat::get();
    let (p, t) = (place.clone(), thread_ts.clone());
    let messages = LocalResource::new(move || {
        chat.tick.track();
        api::rpc("memex.chat.thread", json!({"workspace": p.workspace, "channel": p.channel, "thread_ts": t}))
    });
    // Read up to its latest reply.
    let (rp, rt) = (place.clone(), thread_ts.clone());
    let marked = RwSignal::new(String::new());
    Effect::new(move || {
        let Some(Ok(v)) = messages.get() else { return };
        let Some(last) = v.as_array().and_then(|l| l.last()).and_then(|m| m["meta"]["ts"].as_str().map(str::to_owned)) else { return };
        if marked.get_untracked() != last {
            marked.set(last.clone());
            let params = json!({"workspace": rp.workspace, "channel": rp.channel, "thread_ts": rt, "ts": last});
            spawn_local(async move {
                if api::rpc("memex.chat.thread_read", params).await.is_ok() {
                    chat.refresh();
                }
            });
        }
    });
    let shown = place.clone();
    let name = place.name.clone();
    view! {
        <header class="flex items-center gap-2 border-b border-zinc-200 px-4 py-2 dark:border-zinc-800">
            <span class="font-semibold">"Thread"</span>
            <span class="flex-1 truncate text-xs text-zinc-500">{name}</span>
            <button class="text-xs text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100" title="Close (Esc)" on:click=move |_| chat.thread.set(None)>"✕"</button>
        </header>
        <div class="min-h-0 flex-1 overflow-y-auto py-2">
            {move || messages.get().map(|r| match r {
                Err(e) => view! { <div class="p-3"><ErrorBox message=e /></div> }.into_any(),
                Ok(v) => {
                    let list = v.as_array().cloned().unwrap_or_default();
                    let n = list.len().saturating_sub(1);
                    let place = shown.clone();
                    let mut prev: Option<Value> = None;
                    list.into_iter().enumerate().map(|(i, m)| {
                        let compact = prev.as_ref().is_some_and(|p| message::continues(p, &m)) && i > 1;
                        prev = Some(m.clone());
                        view! {
                            <message::Message m place=place.clone() compact in_thread=true />
                            {(i == 0 && n > 0).then(|| view! {
                                <div class="mx-4 my-1 flex items-center gap-2 text-[11px] text-zinc-500">
                                    <span>{format!("{n} repl{}", if n == 1 { "y" } else { "ies" })}</span>
                                    <span class="h-px flex-1 bg-zinc-200 dark:bg-zinc-800"></span>
                                </div>
                            })}
                        }
                    }).collect_view().into_any()
                }
            })}
        </div>
        <Composer place thread_ts=Some(thread_ts) />
    }
}

/// A conversation as other pages show it (the Brief): the same as here,
/// its thread opening in place.
#[component]
pub(crate) fn Conversation(place: Place, thread: RwSignal<Option<String>>, tick: RwSignal<u32>) -> impl IntoView {
    let chat = Chat::get();
    Effect::new(move || {
        tick.track();
        chat.refresh();
    });
    Effect::new(move || {
        if let Some((_, t)) = chat.thread.get() {
            thread.set(Some(t));
        }
    });
    view! {
        <MessageList place=place.clone() at=None />
        <Composer place thread_ts=None />
    }
}

#[component]
pub(crate) fn Thread(place: Place, thread_ts: String, close: RwSignal<Option<String>>, tick: RwSignal<u32>) -> impl IntoView {
    let chat = Chat::get();
    Effect::new(move || {
        tick.track();
        chat.refresh();
    });
    Effect::new(move |seen: Option<bool>| {
        let open = chat.thread.get().is_some();
        if seen == Some(true) && !open {
            close.set(None);
        }
        open
    });
    chat.thread.set(Some((place.clone(), thread_ts.clone())));
    view! { <ThreadPane place thread_ts /> }
}

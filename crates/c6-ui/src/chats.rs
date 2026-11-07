//! Chat: your Ask conversations, to come back to (docs/ask.md). Each is a
//! session the head runs; this lists them, starts new ones (with a model of
//! your choice, and your mail, chat and calendar when you say so), and
//! opens them beside the list.

use c6_api::{ASSISTANT_PERSONAL, Session, SessionStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, now_ms};
use crate::workspace::{Go, How, View, use_column, use_workspace};

/// Ask conversations of the realm you're in, newest first.
fn conversations(all: &[Session], realm: &str) -> Vec<Session> {
    let mut list: Vec<Session> = all
        .iter()
        .filter(|s| s.on_head && s.parent_id.is_none() && s.labels.iter().any(|l| l == "assistant"))
        .filter(|s| realm.is_empty() || s.realm == realm)
        .cloned()
        .collect();
    list.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    list
}

/// Your Ask conversations, in a tile of their own (the narrow menu's; the
/// header has the Ask button, whose box links here).
#[component]
pub fn ChatLink(class: &'static str) -> impl IntoView {
    view! {
        <Go to=View::Chats class=class title="Your Ask conversations: talk to a model about anything, or about your setup (⌘J asks from anywhere)">
            "Ask conversations"
        </Go>
    }
}

#[component]
pub fn ChatsPage() -> impl IntoView {
    let live = api::live();
    let ws = use_workspace();
    let col = use_column();
    let list = Memo::new(move |_| live.sessions.with(|ss| conversations(ss, &api::realm().get())));
    let open = move |id: String, how: How| match col {
        Some(c) => c.open(View::session(id), how),
        None => ws.open(None, View::session(id), how),
    };
    // Back to where you were: the latest conversation beside the list,
    // unless one is already in view.
    let opened = StoredValue::new(false);
    Effect::new(move || {
        if opened.get_value() {
            return;
        }
        let Some(latest) = list.with(|l| l.first().map(|s| s.id.clone())) else { return };
        opened.set_value(true);
        let shown = ws.layout.with_untracked(|l| l.visible_sessions());
        let any_shown = list.with_untracked(|l| l.iter().any(|s| shown.contains(&s.id)));
        if !any_shown {
            open(latest, How::Beside);
        }
    });
    view! {
        <main class="mx-auto max-w-3xl space-y-4 px-4 py-5" data-hold-reloads="">
            <header class="flex flex-wrap items-baseline gap-x-3 gap-y-1">
                <h1 class="text-xl font-semibold">"Ask"</h1>
                <span class="text-sm text-zinc-500">"Ask anything, or about your setup: no worker or folder needed. ⌘J asks from any page."</span>
            </header>
            <NewChat
                like=Signal::derive(move || list.with(|l| l.first().map(|s| (s.connection_id.clone(), s.model.clone()))))
                on_started=Callback::new(move |s: Session| open(s.id, How::Beside)) />
            {move || {
                let l = list.get();
                if l.is_empty() {
                    view! { <p class="text-sm text-zinc-500">"No conversations yet."</p> }.into_any()
                } else {
                    view! {
                        <div class="space-y-1">
                            {l.into_iter().map(|s| view! { <ChatRow s open=Callback::new(move |(id, how)| open(id, how)) /> }).collect_view()}
                        </div>
                    }.into_any()
                }
            }}
        </main>
    }
}

#[component]
fn ChatRow(s: Session, open: Callback<(String, How)>) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let renaming = RwSignal::new(None::<String>);
    let id = StoredValue::new(s.id.clone());
    let labels = StoredValue::new(s.labels.clone());
    let personal = s.labels.iter().any(|l| l == ASSISTANT_PERSONAL);
    let running = s.status == SessionStatus::Running;
    let toggle = move |on: bool| {
        let mut l = labels.get_value();
        l.retain(|x| x != ASSISTANT_PERSONAL);
        if on {
            l.push(ASSISTANT_PERSONAL.to_owned());
        }
        spawn_local(async move {
            if let Err(e) = api::set_labels(id.get_value(), l).await {
                error.set(Some(e));
            }
        });
    };
    let save_name = move || {
        let Some(t) = renaming.get_untracked() else { return };
        renaming.set(None);
        if t.trim().is_empty() {
            return;
        }
        spawn_local(async move {
            if let Err(e) = api::rename(id.get_value(), t.trim().to_owned()).await {
                error.set(Some(e));
            }
        });
    };
    let archive = move |_| {
        spawn_local(async move {
            if let Err(e) = api::archive(id.get_value()).await {
                error.set(Some(e));
            }
        });
    };
    let title = s.title.clone();
    view! {
        <div class="rounded-md border border-zinc-200 px-3 py-2 dark:border-zinc-800">
            {move || match renaming.get() {
                Some(t) => view! {
                    <div class="flex gap-2">
                        <input class=INPUT prop:value=t on:input=move |ev| renaming.set(Some(event_target_value(&ev)))
                            on:keydown=move |ev| match ev.key().as_str() { "Enter" => save_name(), "Escape" => renaming.set(None), _ => {} } />
                        <button class=BUTTON on:click=move |_| save_name()>"Save"</button>
                    </div>
                }.into_any(),
                None => {
                    let t = title.clone();
                    view! {
                        <div class="flex items-center gap-2">
                            {running.then(|| view! { <span class="h-1.5 w-1.5 shrink-0 animate-pulse rounded-full bg-blue-500" title="running"></span> })}
                            <button class="min-w-0 flex-1 truncate text-left font-medium hover:underline" title="Open beside (⌘/Ctrl-click: new tile if not already open)"
                                on:click=move |ev| open.run((id.get_value(), How::from_mouse(&ev)))>{t.clone()}</button>
                            <span class="shrink-0 text-xs text-zinc-500">{crate::when::ago(now_ms() - s.updated_at)}</span>
                        </div>
                    }.into_any()
                }
            }}
            <div class="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-zinc-500">
                <span class="font-mono">{s.model.clone()}</span>
                <crate::components::Switch on=Signal::stored(personal) on_change=move |v| toggle(v)
                    label="mail, chat, calendar" title="Let this conversation read your mail, chat and calendar" />
                <span class="flex-1"></span>
                <button class=BUTTON_QUIET on:click={ let t = s.title.clone(); move |_| renaming.set(Some(t.clone())) }>"Rename"</button>
                <button class=BUTTON_QUIET on:click=archive>"Archive"</button>
            </div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
        </div>
    }
}

/// A new conversation: the question, its model, and your personal data.
#[component]
fn NewChat(like: Signal<Option<(String, String)>>, on_started: Callback<Session>) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let (connection, model, effort) = (RwSignal::new(String::new()), RwSignal::new(String::new()), RwSignal::new(String::new()));
    // Like your latest conversation, until you pick another.
    Effect::new(move || {
        if let Some((c, m)) = like.get()
            && connection.get_untracked().is_empty()
        {
            connection.set(c);
            model.set(m);
        }
    });
    let personal = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let claude = LocalResource::new(api::ask_claude);
    let claude_off = Signal::derive(move || claude.get().and_then(Result::ok).and_then(|c| (!c.available).then_some(c.reason)));
    let send = move || {
        let prompt = text.get_untracked();
        if prompt.trim().is_empty() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        let req = c6_api::AskAssistant {
            prompt,
            context: "Chat".into(),
            realm: Some(api::realm().get_untracked()).filter(|r| !r.is_empty()),
            connection: Some(connection.get_untracked()).filter(|c| !c.is_empty()),
            model: Some(model.get_untracked()).filter(|m| !m.trim().is_empty()),
            personal: personal.get_untracked(),
        };
        spawn_local(async move {
            match api::ask(req).await {
                Ok(s) => {
                    text.set(String::new());
                    error.set(None);
                    on_started.run(s);
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
            <textarea class=format!("{INPUT} {} min-h-16", crate::util::GROWS)
                placeholder="Ask anything… (it can look at your setup, do what the UI does, and run work on your machines through your templates)"
                prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))
                on:keydown=move |ev| if crate::util::sends(&ev) { ev.prevent_default(); send(); }></textarea>
            <div class="flex flex-wrap items-center gap-2 text-xs">
                <span class="text-zinc-500">"Model"</span>
                <crate::session::ModelPicker connection model effort claude_off=claude_off />

            </div>
            <div class="flex flex-wrap items-center gap-3">
                <label class="flex min-h-8 items-center gap-1 text-xs text-zinc-600 pointer-coarse:min-h-11 dark:text-zinc-400" title="Off: this switch does not add mail, chat or calendar to this conversation's knowledge scope. You can still @-mention one item in a message.">
                    <input type="checkbox" class=crate::components::CHECKBOX prop:checked=move || personal.get() on:change=move |ev| personal.set(event_target_checked(&ev)) />
                    "Include my mail, chat and calendar"
                </label>
                <span class="flex-1"></span>
                <button class=BUTTON disabled=move || busy.get() on:click=move |_| send()>{move || if busy.get() { "Starting…" } else { "New chat" }}</button>
            </div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
        </section>
    }
}

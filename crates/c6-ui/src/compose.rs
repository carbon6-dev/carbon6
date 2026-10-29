//! A session on a reply box (docs/conversations.md §3): started from the
//! box with the conversation's whole context, and followed by the box:
//! when the session rewrites the draft, the new text comes into the box,
//! for you to read and send or not.

use c6_api::SessionStatus;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};

use crate::api;
use crate::util::{BUTTON_QUIET, storage_get, storage_set};
use crate::workspace::{Go, View};

/// `key` names the box (request:<id>, thread:…, chat:…, compose:…);
/// `current` is its text now and `set` puts text in it; `to`: who a new
/// message goes to (their cards go in the context).
#[component]
pub fn WorkInSession(
    #[prop(into)] key: String,
    #[prop(into)] current: Signal<String>,
    #[prop(into)] set: Callback<String>,
    #[prop(optional, into)] to: Option<Signal<Vec<String>>>,
) -> impl IntoView {
    let live = api::live();
    let key = StoredValue::new(key);
    let session = RwSignal::new(None::<String>);
    // The latest of the session's drafts this box took (kept in this
    // browser: a reload doesn't take it again over your edits or Undo).
    let mark = format!("c6.compose.seen.{}", key.get_value());
    let seen = StoredValue::new(storage_get(&mark).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0));
    let mark = StoredValue::new(mark);
    let dialog = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // When and what it replaced: for Undo.
    let note = RwSignal::new(None::<(i64, String)>);
    // Takes the session's draft, if it wrote one since.
    let take = move |d: Value| {
        let at = d["at"].as_i64().unwrap_or(0);
        let text = d["text"].as_str().unwrap_or("").to_owned();
        if d["by"].as_str().is_some_and(|b| b.starts_with("session:")) && at > seen.get_value() {
            seen.set_value(at);
            storage_set(&mark.get_value(), &at.to_string());
            let before = current.get_untracked();
            if text != before {
                set.run(text);
                note.set(Some((at, before)));
            }
        }
    };
    let fetch = move || {
        spawn_local(async move {
            if let Ok(d) = api::rpc("memex.drafts.get", json!({"for": key.get_value()})).await {
                session.set(d["session"].as_str().filter(|s| !s.is_empty()).map(str::to_owned));
                take(d);
            }
        });
    };
    fetch();
    // Each time the session finishes a turn, its draft may be new.
    let status = Memo::new(move |_| {
        let id = session.get()?;
        live.sessions.with(|ss| ss.iter().find(|s| s.id == id).map(|s| s.status))
    });
    Effect::new(move |was: Option<Option<SessionStatus>>| {
        let now = status.get();
        if was.flatten() == Some(SessionStatus::Running) && now != Some(SessionStatus::Running) {
            fetch();
        }
        now
    });
    let start = move |_| {
        busy.set(true);
        error.set(None);
        let to = to.map(|t| t.get_untracked()).unwrap_or_default();
        spawn_local(async move {
            match api::rpc("memex.compose.context", json!({"for": key.get_value(), "draft": current.get_untracked(), "to": to})).await {
                Ok(v) => dialog.set(v["prompt"].as_str().map(str::to_owned)),
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    let on_session = Callback::new(move |s: c6_api::Session| {
        // What it writes from now on comes into the box.
        seen.set_value(js_sys::Date::now() as i64 - 1);
        storage_set(&mark.get_value(), &seen.get_value().to_string());
        session.set(Some(s.id.clone()));
        spawn_local(async move {
            let _ = api::rpc("memex.drafts.link", json!({"for": key.get_value(), "session": s.id})).await;
        });
    });
    let title = move || {
        let id = session.get()?;
        let t = live.sessions.with(|ss| ss.iter().find(|s| s.id == id).map(|s| s.title.clone())).unwrap_or_default();
        Some((id, if t.is_empty() { "the session".to_owned() } else { t }))
    };
    view! {
        <span class="inline-flex flex-wrap items-center gap-2 text-xs">
            {move || match title() {
                Some((id, t)) => view! {
                    <span class="text-zinc-500">"Session: "</span>
                    <Go to=View::session(id) class="max-w-56 truncate text-sky-700 hover:underline dark:text-sky-400">{t}</Go>
                    <button type="button" class=format!("{BUTTON_QUIET} text-xs") disabled=move || busy.get() on:click=start title="Another session on it">"New session"</button>
                }.into_any(),
                None => view! {
                    <button type="button" class=format!("{BUTTON_QUIET} whitespace-nowrap text-xs") disabled=move || busy.get() on:click=start
                        title="Start an agent session with this conversation's context; it can rewrite the draft here">
                        {move || if busy.get() { "Gathering the context…" } else { "Work on it in a session" }}
                    </button>
                }.into_any(),
            }}
            {move || note.get().map(|(at, before)| view! {
                <span class="text-amber-700 dark:text-amber-400">{format!("Rewritten by the session at {}", crate::when::time(at))}</span>
                <button type="button" class="underline" on:click=move |_| { set.run(before.clone()); note.set(None); }>"Undo"</button>
            })}
            {move || error.get().map(|e| view! { <span class="text-red-600">{e}</span> })}
        </span>
        {move || dialog.get().map(|first| view! {
            <crate::sessions::SessionDialog title="Work on this reply in a session"
                note="The session gets the conversation, the people, the request and your draft. It can rewrite the draft here; you read it and send it, or not."
                first_message=first on_session on_close=move |()| dialog.set(None) />
        })}
    }
}

/// The box was sent or discarded: its draft and session go.
pub fn forget(key: String) {
    spawn_local(async move {
        let _ = api::rpc("memex.drafts.clear", json!({"for": key})).await;
    });
}

//! A person's name you can click for who they are to you
//! (docs/conversations.md §1): their card and their organization's, a
//! way to have one written, and where else they are.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};

use crate::api;
use crate::util::BUTTON_QUIET;
use crate::workspace::{Go, View};

/// `label` as shown (a name, `@name`); `address` their mail address (none:
/// no card to look for); `about` what else is known (a chat title, status).
#[component]
pub fn Person(
    #[prop(into)] label: String,
    #[prop(into)] name: String,
    #[prop(into)] address: String,
    #[prop(optional, into)] about: Option<String>,
    #[prop(optional, into)] class: String,
) -> impl IntoView {
    // Where it opens: under the name, else above it when there's no room;
    // fixed to the window, so a scrolling list doesn't cut it off.
    let open = RwSignal::new(None::<String>);
    let _esc = window_event_listener(leptos::ev::keydown, move |ev| {
        if open.get_untracked().is_some() && ev.key() == "Escape" {
            open.set(None);
        }
    });
    let button = NodeRef::<leptos::html::Button>::new();
    let place = move || {
        let r = button.get_untracked()?.get_bounding_client_rect();
        let (w, h) = (window().inner_width().ok()?.as_f64()?, window().inner_height().ok()?.as_f64()?);
        let left = r.left().min(w - 336.0).max(8.0);
        let below = h - r.bottom();
        Some(if below < 280.0 && r.top() > below {
            format!("left: {left}px; bottom: {}px; max-height: {}px", h - r.top() + 4.0, r.top() - 12.0)
        } else {
            format!("left: {left}px; top: {}px; max-height: {}px", r.bottom() + 4.0, below - 12.0)
        })
    };
    let (name, address, about) = (StoredValue::new(name), StoredValue::new(address), StoredValue::new(about));
    view! {
        <span class="inline">
            <button node_ref=button type="button" class=format!("cursor-pointer hover:underline {class}") title="Who they are to you"
                on:click=move |ev| { ev.stop_propagation(); open.set(if open.get_untracked().is_some() { None } else { place() }); }>{label}</button>
            {move || open.get().map(|style| view! {
                <span class="fixed inset-0 z-40" on:click=move |ev| { ev.stop_propagation(); open.set(None); }></span>
                <span style=style class="fixed z-50 block w-80 max-w-[90vw] space-y-2 overflow-y-auto rounded-lg border border-zinc-200 bg-white p-3 text-left text-sm font-normal text-zinc-900 shadow-lg dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-100"
                    on:click=|ev| ev.stop_propagation()>
                    <Card name=name.get_value() address=address.get_value() about=about.get_value() />
                </span>
            })}
        </span>
    }
}

#[component]
fn Card(name: String, address: String, about: Option<String>) -> impl IntoView {
    let has_address = address.contains('@');
    let addr = StoredValue::new(address.to_lowercase());
    let cards = LocalResource::new(move || {
        let a = addr.get_value();
        async move {
            if !a.contains('@') {
                return Ok(Vec::new());
            }
            api::rpc("memex.relations.cards", json!({"address": a})).await.map(|v| v.as_array().cloned().unwrap_or_default())
        }
    });
    let writing = RwSignal::new(false);
    let note = RwSignal::new(None::<String>);
    let n = StoredValue::new(name.clone());
    let write = move |_| {
        writing.set(true);
        spawn_local(async move {
            let key = format!("person:{}", addr.get_value());
            match api::rpc("memex.relations.write", json!({"key": key, "name": n.get_value()})).await {
                Ok(v) if v.is_null() => note.set(Some("Too little history with them for a card.".into())),
                Ok(_) => cards.refetch(),
                Err(e) => note.set(Some(e)),
            }
            writing.set(false);
        });
    };
    view! {
        <span class="block">
            <span class="block font-medium">{name.clone()}</span>
            {has_address.then(|| view! { <span class="block text-xs text-zinc-500">{address.clone()}</span> })}
            {about.filter(|a| !a.is_empty()).map(|a| view! { <span class="block text-xs text-zinc-500">{a}</span> })}
        </span>
        {move || has_address.then(|| match cards.get() {
            None => view! { <span class="block text-xs text-zinc-500">"Looking…"</span> }.into_any(),
            Some(Err(e)) => view! { <span class="block text-xs text-red-600">{e}</span> }.into_any(),
            Some(Ok(list)) if list.is_empty() => view! {
                <span class="flex items-center gap-2 text-xs text-zinc-500">
                    <span>"No card yet."</span>
                    <button type="button" class=format!("{BUTTON_QUIET} text-xs") disabled=move || writing.get() on:click=write>
                        {move || if writing.get() { "Writing…" } else { "Write a card" }}
                    </button>
                </span>
            }.into_any(),
            Some(Ok(list)) => list.into_iter().map(card).collect_view().into_any(),
        })}
        {move || note.get().map(|n| view! { <span class="block text-xs text-zinc-500">{n}</span> })}
        {has_address.then(|| view! {
            <Go to=View::Search { q: address.clone(), kind: String::new(), worker: String::new() } class="block text-xs text-sky-700 hover:underline dark:text-sky-400">
                "Find them in the memex"
            </Go>
        })}
    }
}

/// A card, as the model wrote it from your history with them.
pub(crate) fn card(c: Value) -> impl IntoView {
    let f = |k: &str| c["card"][k].as_str().unwrap_or("").to_owned();
    view! {
        <span class="block space-y-0.5 rounded-md border border-zinc-200 p-2 text-sm dark:border-zinc-800">
            <span class="block">
                <span class="font-medium">{c["name"].as_str().unwrap_or("").to_owned()}</span>
                <span class="ml-1.5 rounded bg-sky-100 px-1.5 text-[11px] text-sky-800 dark:bg-sky-950 dark:text-sky-300">{f("relationship").replace('_', " ")}</span>
                {(!f("role").is_empty()).then(|| view! { <span class="ml-1.5 text-xs text-zinc-500">{f("role")}</span> })}
            </span>
            {(!f("story").is_empty()).then(|| view! { <span class="block">{f("story")}</span> })}
            {(!f("open").is_empty()).then(|| view! { <span class="block text-xs"><span class="text-zinc-500">"Open: "</span>{f("open")}</span> })}
            {(!f("how").is_empty()).then(|| view! { <span class="block text-xs"><span class="text-zinc-500">"You: "</span>{f("how")}</span> })}
            <span class="block text-[11px] text-zinc-400">{format!("From {} thread{} with you, the model's reading", c["threads"].as_i64().unwrap_or(0), if c["threads"] == 1 { "" } else { "s" })}</span>
        </span>
    }
}

//! Settings: LLM connections.

use c6_api::{Connection, CreateConnection};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT};

#[component]
pub fn SettingsPage() -> impl IntoView {
    let connections = LocalResource::new(api::connections);
    let error = RwSignal::new(None::<String>);

    view! {
        <main class="mx-auto max-w-3xl space-y-6 px-4 py-6">
            <section class="space-y-3">
                <div>
                    <h1 class="text-lg font-semibold">"LLM connections"</h1>
                    <p class="text-sm text-zinc-500">
                        "OpenAI-compatible endpoints. Keys are encrypted on the head; workers fetch them when they call the model and keep them only in memory."
                    </p>
                </div>
                {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
                <div class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                    <Suspense fallback=|| ()>
                        {move || connections.get().map(|r| match r {
                            Ok(cs) if cs.is_empty() => view! { <p class="px-4 py-6 text-center text-sm text-zinc-500">"No connections yet."</p> }.into_any(),
                            Ok(cs) => cs.into_iter().map(|c| view! { <ConnectionRow c on_change=move || connections.refetch() error /> }).collect_view().into_any(),
                            Err(e) => view! { <div class="p-3"><ErrorBox message=e /></div> }.into_any(),
                        })}
                    </Suspense>
                </div>
            </section>
            <AddConnection on_added=move || connections.refetch() />
        </main>
    }
}

#[component]
fn ConnectionRow(c: Connection, on_change: impl Fn() + Copy + Send + Sync + 'static, error: RwSignal<Option<String>>) -> impl IntoView {
    let id = c.id.clone();
    let name = c.name.clone();
    let remove = move |_| {
        if !window().confirm_with_message(&format!("Delete connection {name}?")).unwrap_or(false) {
            return;
        }
        let id = id.clone();
        spawn_local(async move {
            match api::delete_connection(id).await {
                Ok(()) => on_change(),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <div class="flex items-center gap-3 px-4 py-3">
            <div class="min-w-0 flex-1">
                <div class="font-medium">{c.name.clone()}</div>
                <div class="truncate font-mono text-xs text-zinc-500">
                    {c.base_url.clone()}
                    {c.default_model.clone().map(|m| format!(" · {m}"))}
                    {if c.has_api_key { " · key set" } else { " · no key" }}
                </div>
            </div>
            <button class=format!("{BUTTON_QUIET} text-zinc-500 hover:text-red-600") on:click=remove>"Delete"</button>
        </div>
    }
}

#[component]
fn AddConnection(on_added: impl Fn() + Copy + Send + Sync + 'static) -> impl IntoView {
    let name = RwSignal::new(String::new());
    let base_url = RwSignal::new(String::new());
    let api_key = RwSignal::new(String::new());
    let default_model = RwSignal::new(String::new());
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let add = move |_| {
        pending.set(true);
        let req = CreateConnection {
            name: name.get_untracked(),
            base_url: base_url.get_untracked(),
            api_key: Some(api_key.get_untracked()).filter(|k| !k.is_empty()),
            default_model: Some(default_model.get_untracked()).filter(|m| !m.is_empty()),
        };
        spawn_local(async move {
            match api::create_connection(req).await {
                Ok(_) => {
                    error.set(None);
                    name.set(String::new());
                    base_url.set(String::new());
                    api_key.set(String::new());
                    default_model.set(String::new());
                    on_added();
                }
                Err(e) => error.set(Some(e)),
            }
            pending.set(false);
        });
    };

    let field = move |label: &'static str, sig: RwSignal<String>, placeholder: &'static str, kind: &'static str| {
        view! {
            <label class="block text-xs text-zinc-500">
                {label}
                <input type=kind class=format!("mt-1 {INPUT}") placeholder=placeholder autocomplete="off"
                    prop:value=move || sig.get() on:input=move |ev| sig.set(event_target_value(&ev)) />
            </label>
        }
    };

    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <h2 class="font-medium">"Add a connection"</h2>
            <div class="grid gap-3 sm:grid-cols-2">
                {field("Name", name, "OpenAI", "text")}
                {field("Base URL", base_url, "https://api.openai.com/v1", "url")}
                {field("API key (optional for local servers)", api_key, "sk-…", "password")}
                {field("Default model (optional)", default_model, "gpt-5", "text")}
            </div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="flex justify-end">
                <button class=BUTTON disabled=move || pending.get() on:click=add>"Add connection"</button>
            </div>
        </section>
    }
}

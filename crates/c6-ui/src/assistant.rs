//! "Ask carbon6": a question for the assistant (docs/assistant.md), from
//! anywhere. It starts a conversation with the page you are on as context
//! and opens it beside the page.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT};
use crate::workspace::{How, View, use_workspace};

/// The header button; ⌘J / Ctrl+J opens it too.
#[component]
pub fn AskButton() -> impl IntoView {
    let open = RwSignal::new(false);
    let _hotkey = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key().eq_ignore_ascii_case("j") && (ev.meta_key() || ev.ctrl_key()) {
            ev.prevent_default();
            open.set(true);
        }
    });
    view! {
        <button class="rounded-md border border-zinc-300 px-2 py-1 text-xs text-zinc-700 hover:bg-zinc-200/60 dark:border-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-800"
            title="Ask carbon6 to do or explain anything (⌘J)" on:click=move |_| open.set(true)>"✦ Ask"</button>
        <Show when=move || open.get()>
            <AskBox on_close=Callback::new(move |()| open.set(false)) />
        </Show>
    }
}

#[component]
fn AskBox(on_close: Callback<()>) -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let text = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let area = NodeRef::<leptos::html::Textarea>::new();
    Effect::new(move || {
        if let Some(el) = area.get() {
            let _ = el.focus();
        }
    });
    // Where you are: the focused column.
    let context =
        move || ws.layout.with_untracked(|l| l.focused_view().map(|v| format!("{} ({})", v.label(&live), v.path()))).unwrap_or_default();
    let send = move || {
        let prompt = text.get_untracked();
        if prompt.trim().is_empty() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        let ctx = context();
        spawn_local(async move {
            match api::ask_assistant(prompt, ctx).await {
                Ok(s) => {
                    on_close.run(());
                    ws.open(None, View::session(s.id), How::Beside);
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    view! {
        <div class="fixed inset-0 z-40 flex items-start justify-center bg-black/40 px-4 pt-24" on:click=move |_| on_close.run(())>
            <div class="w-full max-w-xl space-y-3 rounded-lg border border-zinc-200 bg-white p-4 shadow-xl dark:border-zinc-700 dark:bg-zinc-900"
                on:click=|ev| ev.stop_propagation()>
                <div class="flex items-center gap-2">
                    <h2 class="font-medium">"Ask carbon6"</h2>
                    <span class="text-xs text-zinc-500">"anything you can do in the UI, or about your setup"</span>
                </div>
                <textarea node_ref=area class=format!("{INPUT} min-h-24")
                    placeholder="e.g. What can I do in carbon6? · Which models are set up for vision? · Create an enrollment token for a new worker"
                    prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" && !ev.shift_key() { ev.prevent_default(); send(); }
                        if ev.key() == "Escape" { on_close.run(()); }
                    }></textarea>
                {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
                <div class="flex items-center justify-end gap-2">
                    <span class="flex-1 text-xs text-zinc-500">"Enter to ask · Shift+Enter for a new line · it asks before anything destructive"</span>
                    <button class=BUTTON_QUIET on:click=move |_| on_close.run(())>"Cancel"</button>
                    <button class=BUTTON disabled=move || busy.get() on:click=move |_| send()>{move || if busy.get() { "Asking…" } else { "Ask" }}</button>
                </div>
            </div>
        </div>
    }
}

/// The assistant's model (Settings).
#[component]
pub fn AssistantSettings() -> impl IntoView {
    let loaded = LocalResource::new(move || api::get_setting("assistant"));
    let connections = LocalResource::new(api::connections);
    let (conn, model) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let note = RwSignal::new(None::<Result<String, String>>);
    Effect::new(move || {
        if let Some(Ok(v)) = loaded.get() {
            conn.set(v["model"]["connection"].as_str().unwrap_or_default().to_owned());
            model.set(v["model"]["model"].as_str().unwrap_or_default().to_owned());
        }
    });
    let save = move |_| {
        let mut v = serde_json::json!({});
        if !conn.get_untracked().is_empty() {
            v["model"] = serde_json::json!({"connection": conn.get_untracked(), "model": model.get_untracked().trim()});
        }
        spawn_local(async move {
            note.set(Some(api::put_setting("assistant", v).await.map(|_| "Saved. New conversations use it.".to_owned())));
        });
    };
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <div>
                <h2 class="font-medium">"Assistant"</h2>
                <p class="text-sm text-zinc-500">"Ask carbon6 (✦ Ask, ⌘J) to do anything you can do here, or to explain your setup. It runs in the head, needs no worker, asks you for missing details with a form, shows tokens only to you, and asks before anything destructive. Its conversations have the label assistant, so routes, policies and budgets for that label apply."</p>
            </div>
            <div class="grid gap-2 sm:grid-cols-3">
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Connection"</span>
                    <select class=format!("{INPUT} py-1 text-sm") prop:value=move || conn.get() on:change=move |ev| conn.set(event_target_value(&ev))>
                        <option value="">"Default (routes for assistant)"</option>
                        {move || connections.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|c| c.kind != "claude_code")
                            .map(|c| view! { <option value=c.name.clone()>{c.name.clone()}</option> }).collect_view()}
                    </select>
                </label>
                <label class="space-y-0.5 sm:col-span-2">
                    <span class="text-xs text-zinc-500">"Model"</span>
                    <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="e.g. claude-sonnet-5-5, gpt-5, qwen3.6:27b"
                        prop:value=move || model.get() on:input=move |ev| model.set(event_target_value(&ev)) />
                </label>
            </div>
            <div class="flex items-center justify-end gap-3">
                {move || note.get().map(|r| match r {
                    Ok(m) => view! { <span class="text-sm text-emerald-600">{m}</span> }.into_any(),
                    Err(e) => view! { <span class="text-sm text-red-600">{e}</span> }.into_any(),
                })}
                <button class=BUTTON on:click=save>"Save"</button>
            </div>
        </section>
    }
}

//! "Ask carbon6": a question for the assistant (docs/assistant.md), from
//! anywhere. It starts a conversation with the page you are on as context
//! and opens it beside the page.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::models::ModelInput;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT};
use crate::workspace::{How, View, use_workspace};

/// One host, even with buttons in the desktop header and phone Sessions sheet.
#[derive(Clone, Copy)]
pub struct AskState(pub RwSignal<bool>);

#[component]
pub fn AskHost() -> impl IntoView {
    let open = expect_context::<AskState>().0;
    let hotkey = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key().eq_ignore_ascii_case("j") && (ev.meta_key() || ev.ctrl_key()) {
            ev.prevent_default();
            open.set(true);
        }
    });
    on_cleanup(move || hotkey.remove());
    view! {
        <Show when=move || open.get()>
            <leptos::portal::Portal><AskBox on_close=Callback::new(move |()| open.set(false)) /></leptos::portal::Portal>
        </Show>
    }
}

/// The header or Sessions-sheet button; ⌘J / Ctrl+J opens the same dialog.
#[component]
pub fn AskButton(#[prop(optional)] touch: bool) -> impl IntoView {
    let open = expect_context::<AskState>().0;
    view! {
        <button class=if touch { format!("{} min-h-11 min-w-11 px-1 text-xs text-zinc-700 dark:text-zinc-300", crate::components::BTN_QUIET) } else { "inline-flex items-center gap-1 rounded-md border border-zinc-300 px-2 py-1 text-xs text-zinc-700 hover:bg-zinc-200/60 dark:border-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-800".to_owned() }
            title="Ask carbon6 to do or explain anything (⌘J)" on:click=move |_| open.set(true)>
            <span class=if touch { "inline-flex h-5 items-center justify-center gap-1 rounded-full border border-zinc-300 px-1.5 dark:border-zinc-700" } else { "inline-flex items-center gap-1" }>{crate::icons::sparkles()}" Ask"</span>
        </button>
    }
}

#[component]
fn AskBox(on_close: Callback<()>) -> impl IntoView {
    let ws = use_workspace();
    let pop = crate::util::escapable(|| true, move || on_close.run(()));
    let live = api::live();
    let text = crate::util::draft("ask");
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let area = NodeRef::<leptos::html::Textarea>::new();
    crate::util::autosize(area, move || text.track());
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
                    crate::util::forget_draft("ask");
                    on_close.run(());
                    ws.open(None, View::session(s.id), How::Beside);
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    view! {
        <div class="phone-dialog fixed inset-0 z-50 flex items-start justify-center bg-black/40 px-4 pt-24" on:click=move |_| on_close.run(())>
            <div role="dialog" aria-modal="true" aria-label="Ask carbon6" data-pop=pop class="max-h-full w-full max-w-xl space-y-3 overflow-y-auto rounded-lg border border-zinc-200 bg-white p-4 shadow-xl dark:border-zinc-700 dark:bg-zinc-900"
                on:click=|ev| ev.stop_propagation()>
                <div class="flex items-center gap-2">
                    <h2 class="font-medium">"Ask carbon6"</h2>
                    <span class="flex-1 text-xs text-muted">"anything you can do in the UI, or about your setup"</span>
                    <button class=format!("{BUTTON_QUIET} text-xs") title="Your Ask conversations: come back to one, or start one with a model of your choice"
                        on:click=move |_| { on_close.run(()); ws.open(None, View::Chats, How::OwnTile); }>"Your conversations "{crate::icons::arrow_up_right()}</button>
                </div>
                <textarea node_ref=area class=format!("{INPUT} {} min-h-24", crate::util::GROWS)
                    placeholder="e.g. What can I do in carbon6? · Which models are set up for vision? · Create an enrollment token for a new worker"
                    prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if crate::util::sends(&ev) { ev.prevent_default(); send(); }
                        if ev.key() == "Escape" { on_close.run(()); }
                    }></textarea>
                {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
                <div class="flex items-center justify-end gap-2">
                    <span class="flex-1 text-xs text-muted">"Enter to ask · Shift+Enter for a new line · it asks before anything destructive"</span>
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
    let connections = crate::resources::connections();
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
                <p class="text-sm text-muted">"Ask carbon6 ("<span class="inline-flex items-center">{crate::icons::sparkles()}" Ask"</span>", ⌘J) to do anything you can do here, or to explain your setup. It runs in the head, needs no worker, asks you for missing details with a form, shows tokens only to you, and asks before anything destructive. Its conversations have the label assistant, so routes, policies and budgets for that label apply."</p>
            </div>
            <div class="grid gap-2 sm:grid-cols-3">
                <label class="space-y-0.5">
                    <span class="text-xs text-muted">"Connection"</span>
                    <select class=format!("{INPUT} py-1 text-sm") prop:value=move || conn.get() on:change=move |ev| conn.set(event_target_value(&ev))>
                        <option value="">"Default (routes for assistant)"</option>
                        {move || connections.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|c| c.kind != "claude_code")
                            .map(|c| view! { <option value=c.name.clone()>{c.name.clone()}</option> }).collect_view()}
                    </select>
                </label>
                <label class="space-y-0.5 sm:col-span-2">
                    <span class="text-xs text-muted">"Model"</span>
                    <ModelInput connection=conn value=model on_set=move |v| model.set(v) class=format!("{INPUT} py-1 text-xs") placeholder="e.g. claude-sonnet-5-5, gpt-5, qwen3.6:27b" />
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

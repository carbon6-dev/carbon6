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
    let query = leptos_router::hooks::use_query_map();
    let signed_in = move || query.read().get("signed_in").is_some();

    view! {
        <main class="mx-auto max-w-3xl space-y-6 px-4 py-6">
            <section class="space-y-3">
                <div>
                    <h1 class="text-lg font-semibold">"LLM connections"</h1>
                    <p class="text-sm text-zinc-500">
                        "Model providers for sessions. Keys and sign-ins are encrypted on the head; workers fetch short-lived credentials when they call the model and keep them only in memory."
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
            {move || signed_in().then(|| view! {
                <div class="rounded-md border border-emerald-300 bg-emerald-50 px-3 py-2 text-sm text-emerald-800 dark:border-emerald-900 dark:bg-emerald-950 dark:text-emerald-200">
                    "Signed in with ChatGPT."
                </div>
            })}
            <AddConnection on_added=move || connections.refetch() />
            <ChatGptSignIn on_added=move || connections.refetch() />
            <JsonSetting
                name="mcp-servers"
                title="MCP servers"
                help="Started for each new session: stdio servers run on the worker in the session's directory; HTTP servers are reached from the worker. Their tools appear as mcp__<server>__<tool>."
                example=r#"[
  {"name": "github", "transport": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"GITHUB_TOKEN": "…"}},
  {"name": "docs", "transport": "http", "url": "http://10.0.0.5:8080/mcp"}
]"#
            />
            <JsonSetting
                name="skills"
                title="Skills"
                help="Instructions the agent loads on demand by name. Skills in a repository's .claude/skills or .agents/skills directories are found automatically and take precedence."
                example=r#"[
  {"name": "release", "description": "How to cut a release", "body": "1. Update CHANGELOG.md…"}
]"#
            />
        </main>
    }
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "anthropic" => "Anthropic",
        "openai" => "OpenAI",
        "chatgpt" => "ChatGPT plan",
        "claude_code" => "Claude Code",
        _ => "OpenAI-compatible",
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
                    {kind_label(&c.kind)} {(c.kind != "claude_code").then(|| format!(" · {}", c.base_url))}
                    {c.default_model.clone().map(|m| format!(" · {m}"))}
                    {match (c.kind.as_str(), c.has_api_key) { ("chatgpt" | "claude_code", _) => "", (_, true) => " · key set", _ => " · no key" }}
                </div>
            </div>
            <button class=format!("{BUTTON_QUIET} text-zinc-500 hover:text-red-600") on:click=remove>"Delete"</button>
        </div>
    }
}

#[component]
fn AddConnection(on_added: impl Fn() + Copy + Send + Sync + 'static) -> impl IntoView {
    let kind = RwSignal::new("anthropic".to_owned());
    let name = RwSignal::new(String::new());
    let base_url = RwSignal::new(String::new());
    let api_key = RwSignal::new(String::new());
    let default_model = RwSignal::new(String::new());
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let add = move |_| {
        pending.set(true);
        let req = CreateConnection {
            kind: Some(kind.get_untracked()),
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

    let field = move |label: &'static str, sig: RwSignal<String>, placeholder: Signal<&'static str>, input_type: &'static str| {
        view! {
            <label class="block text-xs text-zinc-500">
                {label}
                <input type=input_type class=format!("mt-1 {INPUT}") placeholder=placeholder autocomplete="off"
                    prop:value=move || sig.get() on:input=move |ev| sig.set(event_target_value(&ev)) />
            </label>
        }
    };
    let by_kind = move |anthropic: &'static str, openai: &'static str, compat: &'static str| {
        Signal::derive(move || match kind.get().as_str() {
            "anthropic" => anthropic,
            "openai" => openai,
            "claude_code" => "",
            _ => compat,
        })
    };

    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <h2 class="font-medium">"Add a connection"</h2>
            <label class="block text-xs text-zinc-500">
                "Kind"
                <select class=format!("mt-1 {INPUT}") prop:value=move || kind.get() on:change=move |ev| kind.set(event_target_value(&ev))>
                    <option value="anthropic">"Anthropic API (Claude)"</option>
                    <option value="openai">"OpenAI API (Responses)"</option>
                    <option value="openai_compat">"OpenAI-compatible (Chat Completions: Ollama, vLLM, OpenRouter, …)"</option>
                    <option value="claude_code">"Claude Code (each worker's own signed-in claude CLI)"</option>
                </select>
            </label>
            {move || (kind.get() == "claude_code").then(|| view! {
                <p class="text-sm text-zinc-500">
                    "Sessions run the official Claude Code CLI on the worker, with its own tools and sign-in (your Claude subscription or API key). Install it on each worker and run "
                    <code class="font-mono">"claude auth login"</code> " there. No key is stored here."
                </p>
            })}
            <div class="grid gap-3 sm:grid-cols-2">
                {field("Name", name, by_kind("Anthropic", "OpenAI", "Ollama"), "text")}
                {field("Base URL", base_url, by_kind("https://api.anthropic.com (default)", "https://api.openai.com/v1 (default)", "http://127.0.0.1:11434/v1"), "url")}
                {field("API key", api_key, by_kind("sk-ant-…", "sk-…", "optional for local servers"), "password")}
                {field("Default model (optional)", default_model, by_kind("claude-opus-5-5", "gpt-5.5", "qwen3.6:27b"), "text")}
            </div>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="flex justify-end">
                <button class=BUTTON disabled=move || pending.get() on:click=add>"Add connection"</button>
            </div>
        </section>
    }
}

/// Sign in with ChatGPT: opens OpenAI's consent page; the redirect lands on
/// the head directly when this browser runs on the head machine, otherwise
/// the user pastes the address it ended on.
#[component]
fn ChatGptSignIn(on_added: impl Fn() + Copy + Send + Sync + 'static) -> impl IntoView {
    let name = RwSignal::new("ChatGPT".to_owned());
    let url = RwSignal::new(None::<String>);
    let callback = RwSignal::new(String::new());
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let start = move |_| {
        pending.set(true);
        spawn_local(async move {
            match api::start_chatgpt(name.get_untracked()).await {
                Ok(r) => {
                    let _ = window().open_with_url_and_target(&r.authorize_url, "_blank");
                    url.set(Some(r.authorize_url));
                    error.set(None);
                }
                Err(e) => error.set(Some(e)),
            }
            pending.set(false);
        });
    };
    let complete = move |_| {
        pending.set(true);
        spawn_local(async move {
            match api::complete_chatgpt(callback.get_untracked()).await {
                Ok(_) => {
                    url.set(None);
                    callback.set(String::new());
                    error.set(None);
                    on_added();
                }
                Err(e) => error.set(Some(e)),
            }
            pending.set(false);
        });
    };

    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <div>
                <h2 class="font-medium">"Sign in with ChatGPT"</h2>
                <p class="text-sm text-zinc-500">"Use your ChatGPT plan for OpenAI models. The head keeps and refreshes the sign-in; workers only ever get short-lived tokens."</p>
            </div>
            {move || match url.get() {
                None => view! {
                    <div class="flex items-end gap-2">
                        <label class="block flex-1 text-xs text-zinc-500">
                            "Connection name"
                            <input class=format!("mt-1 {INPUT}") prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
                        </label>
                        <button class=BUTTON disabled=move || pending.get() on:click=start>"Sign in"</button>
                    </div>
                }.into_any(),
                Some(u) => view! {
                    <div class="space-y-2 text-sm">
                        <p class="text-zinc-600 dark:text-zinc-400">
                            "Approve access in the tab that opened (or " <a class="underline" href=u target="_blank">"open it again"</a> "). "
                            "If this browser runs on the head's machine, you'll come straight back here. Otherwise the tab ends on a page that "
                            "fails to load: copy its full address and paste it below."
                        </p>
                        <div class="flex gap-2">
                            <input class=format!("{INPUT} font-mono text-xs") placeholder="http://127.0.0.1:…/auth/callback?code=…"
                                prop:value=move || callback.get() on:input=move |ev| callback.set(event_target_value(&ev)) />
                            <button class=BUTTON disabled=move || pending.get() on:click=complete>"Finish"</button>
                        </div>
                    </div>
                }.into_any(),
            }}
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
        </section>
    }
}

/// A JSON-array setting edited as text, validated by the head on save.
#[component]
fn JsonSetting(name: &'static str, title: &'static str, help: &'static str, example: &'static str) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let status = RwSignal::new(None::<Result<&'static str, String>>);
    let loaded = LocalResource::new(move || api::get_setting(name));
    Effect::new(move || {
        if let Some(Ok(v)) = loaded.get() {
            let pretty = serde_json::to_string_pretty(&v).unwrap_or_default();
            text.set(if pretty == "[]" { String::new() } else { pretty });
        }
    });
    let save = move |_| {
        let raw = text.get_untracked();
        let value = if raw.trim().is_empty() {
            Ok(serde_json::json!([]))
        } else {
            serde_json::from_str::<serde_json::Value>(&raw).map_err(|e| format!("Not valid JSON: {e}"))
        };
        spawn_local(async move {
            let r = match value {
                Ok(v) => api::put_setting(name, v).await.map(|_| "Saved. New sessions will use it."),
                Err(e) => Err(e),
            };
            status.set(Some(r));
        });
    };
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <h2 class="font-medium">{title}</h2>
            <p class="text-sm text-zinc-500">{help}</p>
            <textarea class=format!("{INPUT} min-h-40 font-mono text-xs") placeholder=example
                prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))></textarea>
            <div class="flex items-center justify-end gap-3">
                {move || status.get().map(|r| match r {
                    Ok(m) => view! { <span class="text-sm text-emerald-600">{m}</span> }.into_any(),
                    Err(e) => view! { <span class="text-sm text-red-600">{e}</span> }.into_any(),
                })}
                <button class=BUTTON on:click=save>"Save"</button>
            </div>
        </section>
    }
}

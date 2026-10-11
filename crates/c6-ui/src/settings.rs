//! Settings: LLM connections, MCP servers, skills and hooks.

use c6_api::{Connection, CreateConnection, Hook};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT};

#[component]
/// `note`: a notice after a redirect ("chatgpt": signed in with ChatGPT;
/// "link": signed in with a login link).
pub fn SettingsPage(note: Option<String>) -> impl IntoView {
    let connections = LocalResource::new(api::connections);
    let error = RwSignal::new(None::<String>);
    let after_link = note.as_deref() == Some("link");
    let signed_in = note.as_deref() == Some("chatgpt");
    let signed_in = move || signed_in;

    let auth_required = LocalResource::new(api::auth_status);
    view! {
        <main class="mx-auto max-w-3xl space-y-6 px-4 py-6">
            {move || auth_required.get().and_then(Result::ok).is_some_and(|s| s.required).then(|| view! { <crate::auth::SignInSettings after_link /> })}
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
            <ListSetting
                name="commands"
                title="Slash commands"
                noun="command"
                help="Your own /commands, offered when you type / in a message. Changes apply to the next message. In the prompt, $ARGUMENTS (or $1…$9) takes what follows the command, !`cmd` the output of a shell command run in the session's folder, and @path attaches a file. A command named like a built-in one replaces it."
                fields=COMMAND_FIELDS
                examples=command_examples
            />
            <ListSetting
                name="agents"
                title="Agents"
                noun="agent"
                help="Specialized subagents the model can launch (with the Task tool), each with its own instructions and, optionally, a limited set of tools. The model picks one by its description. New sessions pick up changes."
                fields=AGENT_FIELDS
                examples=agent_examples
            />
            <ListSetting
                name="skills"
                title="Skills"
                noun="skill"
                help="Instructions the agent loads by name when they apply; only the name and description sit in its prompt until then. New sessions pick up changes."
                fields=SKILL_FIELDS
                examples=skill_examples
            />
            <ListSetting
                name="templates"
                title="Templates"
                noun="template"
                help="Saved ways to start a session: where (a worker, every online worker with *, or SSH hosts through a worker), how (connection, model, labels) and what to ask. Several targets make a fleet run: one session each, sharing a run: label, so mission control shows them together. Run them from the Sessions page, with c6 run -t, on a schedule or from a webhook. In the prompt, {{input}} is what the run was given, {{target}} the worker or host, {{date}} and {{time}} now."
                fields=TEMPLATE_FIELDS
                examples=template_examples
            />
            <HooksSetting />
            <SecretsSetting />
            <WebSearchSetting />
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
    let check = RwSignal::new(None::<Result<Vec<String>, String>>);
    let test_id = c.id.clone();
    let test = move |_| {
        let id = test_id.clone();
        check.set(None);
        spawn_local(async move { check.set(Some(api::models(id).await)) });
    };
    view! {
        <div class="flex items-center gap-3 px-4 py-3">
            <div class="min-w-0 flex-1">
                <div class="font-medium">{c.name.clone()}</div>
                {move || check.get().map(|r| match r {
                    Ok(m) => view! { <div class="text-xs text-emerald-700 dark:text-emerald-400">{format!("✓ Connected · {} models", m.len())}</div> }.into_any(),
                    Err(e) => view! { <div class="text-xs text-red-600">{format!("✕ {e}")}</div> }.into_any(),
                })}
                <div class="truncate font-mono text-xs text-zinc-500">
                    {kind_label(&c.kind)} {(c.kind != "claude_code").then(|| format!(" · {}", c.base_url))}
                    {c.default_model.clone().map(|m| format!(" · {m}"))}
                    {match (c.kind.as_str(), c.has_api_key) { ("chatgpt" | "claude_code", _) => "", (_, true) => " · key set", _ => " · no key" }}
                </div>
            </div>
            <button class=format!("{BUTTON_QUIET} text-zinc-500") on:click=test>"Test"</button>
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
    // Result of "Test connection": the server's models, or why it failed.
    let tested = RwSignal::new(None::<Result<Vec<String>, String>>);
    let testing = RwSignal::new(false);
    // Any edit invalidates an earlier test.
    Effect::new(move || {
        kind.track();
        base_url.track();
        api_key.track();
        tested.set(None);
    });
    let test = move |_| {
        testing.set(true);
        let (k, url, key) = (kind.get_untracked(), base_url.get_untracked(), api_key.get_untracked());
        spawn_local(async move {
            let r = api::test_connection(k, url, Some(key).filter(|k| !k.is_empty()), None).await;
            if let Ok(models) = &r
                && default_model.get_untracked().is_empty()
                && models.len() == 1
            {
                default_model.set(models[0].clone());
            }
            tested.set(Some(r));
            testing.set(false);
        });
    };

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
                    tested.set(None);
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
                <label class="block text-xs text-zinc-500">
                    "Default model (optional)"
                    // Suggestions from the server after a test; any name can be typed.
                    <input class=format!("mt-1 font-mono {INPUT}") list="c6-conn-models" autocomplete="off"
                        placeholder=move || if matches!(tested.get(), Some(Ok(_))) { "pick or type a model" } else { by_kind("claude-opus-5-5", "gpt-5.5", "qwen3.6:27b").get() }
                        prop:value=move || default_model.get() on:input=move |ev| default_model.set(event_target_value(&ev)) />
                    <datalist id="c6-conn-models">
                        {move || tested.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|m| view! { <option value=m></option> }).collect_view()}
                    </datalist>
                </label>
            </div>
            {move || tested.get().map(|r| match r {
                Ok(models) => view! {
                    <p class="text-sm text-emerald-700 dark:text-emerald-400">
                        {format!("✓ Connected · {} model{} available", models.len(), if models.len() == 1 { "" } else { "s" })}
                        {(!models.is_empty()).then(|| view! { <span class="text-zinc-500">" (suggested in the model field)"</span> })}
                    </p>
                }.into_any(),
                Err(e) => view! { <p class="text-sm text-red-600">{format!("✕ {e}")}</p> }.into_any(),
            })}
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="flex justify-end gap-2">
                <button class=BUTTON_QUIET disabled=move || testing.get() on:click=test>
                    {move || if testing.get() { "Testing…" } else { "Test connection" }}
                </button>
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

/// Starting points for a new hook: (label, hook).
fn hook_examples() -> Vec<(&'static str, Hook)> {
    let h = |name: &str, event: &str, matcher: &str, command: &str| Hook {
        name: name.into(),
        event: event.into(),
        matcher: matcher.into(),
        folder: String::new(),
        command: command.into(),
        timeout_s: None,
        enabled: true,
        applies_to: Vec::new(),
    };
    vec![
        ("Empty", h("", "PreToolUse", "", "")),
        (
            "Refuse rm -rf",
            h(
                "Refuse rm -rf",
                "PreToolUse",
                "Bash",
                "grep -q 'rm -rf' && { echo 'Do not use rm -rf; delete specific files instead.' >&2; exit 2; }; exit 0",
            ),
        ),
        (
            "Protect .env files",
            h(
                "Protect .env files",
                "PreToolUse",
                "Edit|Write",
                "grep -qE '\"(file_)?path\": *\"[^\"]*\\.env' && { echo 'Do not edit .env files.' >&2; exit 2; }; exit 0",
            ),
        ),
        ("Format Rust after edits", h("Format Rust after edits", "PostToolUse", "Edit|Write", "cargo fmt --quiet 2>&1 || exit 2")),
        (
            "Tell the agent the git branch",
            h("Git branch", "UserPromptSubmit", "", "echo \"Current git branch: $(git branch --show-current 2>/dev/null)\""),
        ),
        (
            "Keep going until tests pass",
            h(
                "Tests must pass",
                "Stop",
                "",
                "grep -q '\"stop_hook_active\":true' && exit 0; cargo test --quiet >/dev/null 2>&1 || { echo 'The tests fail; fix them before finishing.' >&2; exit 2; }",
            ),
        ),
        ("Project status at start", h("Project status", "SessionStart", "", "git status --short 2>/dev/null | head -50")),
    ]
}

fn is_tool_event(event: &str) -> bool {
    matches!(event, "PreToolUse" | "PostToolUse")
}

/// Settings: hooks, edited as a list.
#[component]
fn HooksSetting() -> impl IntoView {
    let rows = RwSignal::new(Vec::<(u64, RwSignal<Hook>)>::new());
    let next = StoredValue::new(0u64);
    let add = move |h: Hook| {
        let id = next.get_value();
        next.set_value(id + 1);
        rows.update(|r| r.push((id, RwSignal::new(h))));
    };
    let status = RwSignal::new(None::<Result<&'static str, String>>);
    let loaded = LocalResource::new(move || api::get_setting("hooks"));
    Effect::new(move || {
        if let Some(Ok(v)) = loaded.get() {
            rows.set(Vec::new());
            for mut h in serde_json::from_value::<Vec<Hook>>(v).unwrap_or_default() {
                // A folder is now a folder: label.
                if !h.folder.trim().is_empty() {
                    h.applies_to.push(format!("folder:{}", h.folder.trim()));
                    h.folder.clear();
                }
                add(h);
            }
        }
    });
    let save = move |_| {
        let hooks: Vec<Hook> = rows.get_untracked().iter().map(|(_, h)| h.get_untracked()).collect();
        spawn_local(async move {
            let r = match serde_json::to_value(&hooks) {
                Ok(v) => api::put_setting("hooks", v).await.map(|_| "Saved. New sessions will use them."),
                Err(e) => Err(e.to_string()),
            };
            status.set(Some(r));
        });
    };
    let example = RwSignal::new(String::new());
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <div>
                <h2 class="font-medium">"Hooks"</h2>
                <p class="text-sm text-zinc-500">
                    "Shell commands run on the worker, in the session's folder, at points in every session: to refuse risky tool calls, check work after edits, add context to your messages, or keep the agent going until something passes. New sessions pick up changes."
                </p>
                <details class="mt-1 text-sm text-zinc-500">
                    <summary class="cursor-pointer select-none">"How a hook works"</summary>
                    <ul class="mt-1 list-disc space-y-1 pl-5">
                        <li>"The event arrives as JSON on stdin: "<code>"tool_name"</code>", "<code>"tool_input"</code>" (tool events), "<code>"tool_response"</code>" (after a tool), "<code>"prompt"</code>" (your message), "<code>"stop_hook_active"</code>" (Stop), plus "<code>"session_id"</code>" and "<code>"cwd"</code>". "<code>"$C6_PROJECT_DIR"</code>" is the repository root."</li>
                        <li><b>"Exit 0"</b>": carry on. For "<i>"UserPromptSubmit"</i>" and "<i>"SessionStart"</i>", what it prints is given to the agent as context."</li>
                        <li><b>"Exit 2"</b>": block, with stderr as the reason. "<i>"PreToolUse"</i>": the call is refused and the agent told why. "<i>"UserPromptSubmit"</i>": your message is not sent. "<i>"Stop"</i>": the agent keeps working. "<i>"PostToolUse"</i>": stderr is added to the tool's result."</li>
                        <li>"Other exit codes are errors and block nothing. Printing "<code>"{\"decision\": \"block\", \"reason\": \"…\"}"</code>" also blocks."</li>
                        <li>"The matcher is a regular expression over tool names; Claude Code's names ("<code>"Bash"</code>", "<code>"Edit"</code>", "<code>"Write"</code>", "<code>"Read"</code>") work with every model."</li>
                    </ul>
                </details>
            </div>
            <div class="space-y-3">
                <For each=move || rows.get() key=|(id, _)| *id let:row>
                    <HookRow hook=row.1 on_remove=move || rows.update(|r| r.retain(|(id, _)| *id != row.0)) />
                </For>
                {move || rows.with(Vec::is_empty).then(|| view! { <p class="text-sm text-zinc-500">"No hooks."</p> })}
            </div>
            <div class="flex flex-wrap items-center justify-between gap-3">
                <select class=format!("{INPUT} w-auto!") prop:value=move || example.get()
                    on:change=move |ev| {
                        let label = event_target_value(&ev);
                        if let Some((_, h)) = hook_examples().into_iter().find(|(l, _)| *l == label) {
                            add(h);
                        }
                        example.set(String::new());
                    }>
                    <option value="">"Add a hook…"</option>
                    {hook_examples().into_iter().map(|(l, _)| view! { <option value=l>{l}</option> }).collect_view()}
                </select>
                <div class="flex items-center gap-3">
                    {move || status.get().map(|r| match r {
                        Ok(m) => view! { <span class="text-sm text-emerald-600">{m}</span> }.into_any(),
                        Err(e) => view! { <span class="text-sm text-red-600">{e}</span> }.into_any(),
                    })}
                    <button class=BUTTON on:click=save>"Save"</button>
                </div>
            </div>
        </section>
    }
}

#[component]
fn HookRow(hook: RwSignal<Hook>, on_remove: impl Fn() + Copy + Send + Sync + 'static) -> impl IntoView {
    let label = "text-xs text-zinc-500";
    view! {
        <div class=move || format!("space-y-2 rounded-md border border-zinc-200 p-3 dark:border-zinc-800 {}", if hook.with(|h| h.enabled) { "" } else { "opacity-60" })>
            <div class="flex flex-wrap items-center gap-2">
                <input type="checkbox" title="Enabled" prop:checked=move || hook.with(|h| h.enabled)
                    on:change=move |ev| hook.update(|h| h.enabled = event_target_checked(&ev)) />
                <input class=format!("{INPUT} w-auto! min-w-0 flex-1 py-1") placeholder="Name (what it is for)" prop:value=move || hook.with(|h| h.name.clone())
                    on:input=move |ev| hook.update(|h| h.name = event_target_value(&ev)) />
                <select class=format!("{INPUT} w-auto! py-1") prop:value=move || hook.with(|h| h.event.clone())
                    on:change=move |ev| hook.update(|h| h.event = event_target_value(&ev))>
                    {c6_api::HOOK_EVENTS.into_iter().map(|e| view! { <option value=e>{e}</option> }).collect_view()}
                </select>
                <button class=format!("{BUTTON_QUIET} text-xs text-red-600") on:click=move |_| on_remove()>"Remove"</button>
            </div>
            <div class="grid gap-2 sm:grid-cols-[1fr_1fr_6rem]">
                <label class="space-y-0.5">
                    <span class=label>"Tools (regex)"</span>
                    <input class=format!("{INPUT} py-1 font-mono text-xs")
                        prop:disabled=move || !hook.with(|h| is_tool_event(&h.event))
                        placeholder=move || if hook.with(|h| is_tool_event(&h.event)) { "all tools, or e.g. Bash|Edit" } else { "not used by this event" }
                        prop:value=move || hook.with(|h| h.matcher.clone())
                        on:input=move |ev| hook.update(|h| h.matcher = event_target_value(&ev)) />
                </label>
                <label class="space-y-0.5">
                    <span class=label>"Applies to"</span>
                    <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="everywhere, or labels, e.g. repo:github.com/me/app"
                        title="Sessions with all of these labels: yours, or worker:, repo:, folder: (a path prefix) and model:"
                        prop:value=move || hook.with(|h| h.applies_to.join(", "))
                        on:change=move |ev| hook.update(|h| h.applies_to = c6_api::labels::parse_labels(&event_target_value(&ev))) />
                </label>
                <label class="space-y-0.5">
                    <span class=label>"Timeout (s)"</span>
                    <input class=format!("{INPUT} py-1 text-xs") type="number" min="1" max="3600" placeholder="60"
                        prop:value=move || hook.with(|h| h.timeout_s.map(|t| t.to_string()).unwrap_or_default())
                        on:input=move |ev| hook.update(|h| h.timeout_s = event_target_value(&ev).trim().parse().ok()) />
                </label>
            </div>
            <label class="block space-y-0.5">
                <span class=label>"Command (sh)"</span>
                <textarea class=format!("{INPUT} min-h-16 font-mono text-xs") rows="2" placeholder="./scripts/check.sh"
                    prop:value=move || hook.with(|h| h.command.clone())
                    on:input=move |ev| hook.update(|h| h.command = event_target_value(&ev))></textarea>
            </label>
        </div>
    }
}

#[derive(Clone, Copy, PartialEq)]
enum FieldKind {
    /// One line.
    Line,
    /// One line, monospace.
    Mono,
    /// Several lines.
    Text,
    /// Labels, typed separated by commas; stored as a list.
    Labels,
}

/// A field of a list setting's items. The first is the item's name, shown
/// in its header.
struct Field {
    key: &'static str,
    label: &'static str,
    placeholder: &'static str,
    kind: FieldKind,
}

const COMMAND_FIELDS: &[Field] = &[
    Field { key: "name", label: "Name", placeholder: "name, typed as /name", kind: FieldKind::Mono },
    Field { key: "description", label: "Description", placeholder: "shown in the / list", kind: FieldKind::Line },
    Field { key: "argument_hint", label: "Arguments hint", placeholder: "e.g. <issue> [priority]", kind: FieldKind::Mono },
    Field {
        key: "applies_to",
        label: "Applies to",
        placeholder: "everywhere, or labels: e.g. repo:github.com/me/app, client:acme",
        kind: FieldKind::Labels,
    },
    Field { key: "body", label: "Prompt", placeholder: "Fix issue #$1. Here it is: !`gh issue view $1`", kind: FieldKind::Text },
];

const AGENT_FIELDS: &[Field] = &[
    Field { key: "name", label: "Name", placeholder: "e.g. code-reviewer", kind: FieldKind::Mono },
    Field { key: "description", label: "When to use it", placeholder: "the model reads this to pick the agent", kind: FieldKind::Line },
    Field { key: "tools", label: "Tools", placeholder: "all, or e.g. Read, Grep, Glob", kind: FieldKind::Mono },
    Field { key: "model", label: "Model", placeholder: "same as the session (or opus, sonnet, haiku on Anthropic)", kind: FieldKind::Mono },
    Field {
        key: "applies_to",
        label: "Applies to",
        placeholder: "everywhere, or labels: e.g. repo:github.com/me/app, client:acme",
        kind: FieldKind::Labels,
    },
    Field { key: "prompt", label: "Instructions", placeholder: "You are a … Your job is …", kind: FieldKind::Text },
];

const TEMPLATE_FIELDS: &[Field] = &[
    Field { key: "name", label: "Name", placeholder: "e.g. disk-check", kind: FieldKind::Mono },
    Field { key: "description", label: "Description", placeholder: "what it is for", kind: FieldKind::Line },
    Field { key: "workers", label: "Workers", placeholder: "the online one; or names; * for every online worker", kind: FieldKind::Labels },
    Field { key: "ssh", label: "SSH hosts", placeholder: "none; or user@host, … (one session each)", kind: FieldKind::Labels },
    Field { key: "cwd", label: "Folder", placeholder: "scratch workspace (or home on SSH hosts)", kind: FieldKind::Mono },
    Field { key: "connection", label: "Connection", placeholder: "the first one", kind: FieldKind::Line },
    Field { key: "model", label: "Model", placeholder: "the connection's default", kind: FieldKind::Mono },
    Field { key: "labels", label: "Labels", placeholder: "e.g. ops, daily", kind: FieldKind::Labels },
    Field { key: "prompt", label: "Prompt", placeholder: "Check {{target}} for … {{input}}", kind: FieldKind::Text },
];

fn template_examples() -> Vec<(&'static str, serde_json::Value)> {
    let t = |pairs: &[(&str, &str)], lists: &[(&str, &[&str])]| {
        let mut v = item(pairs);
        for (k, l) in lists {
            v[*k] = serde_json::json!(l);
        }
        v
    };
    vec![
        ("Empty", item(&[("name", ""), ("prompt", "")])),
        (
            "Health check across hosts",
            t(
                &[
                    ("name", "health-check"),
                    ("description", "Disk, memory, failed services and pending updates on each host"),
                    (
                        "prompt",
                        "Check the health of {{target}}: disk usage (flag anything over 85%), memory, load, failed systemd services, and pending security updates. Do not change anything. Start your reply with OK if all is well, otherwise with ATTENTION and what needs doing.",
                    ),
                ],
                &[("ssh", &["admin@host1", "admin@host2"]), ("labels", &["ops"])],
            ),
        ),
        (
            "Update packages on every worker",
            t(
                &[
                    ("name", "update-workers"),
                    (
                        "prompt",
                        "Update the system packages on this machine ({{target}}) with its package manager, non-interactively. Report what was upgraded and whether a reboot is needed; do not reboot.",
                    ),
                ],
                &[("workers", &["*"]), ("labels", &["ops"])],
            ),
        ),
        (
            "Research digest",
            t(
                &[
                    ("name", "digest"),
                    ("description", "A short digest of news on a topic"),
                    (
                        "prompt",
                        "Research what happened in the last week on: {{input}}. Search the web, read the best sources, and write digest.md: five bullet points with links, newest first, then a one-paragraph summary. Publish it.",
                    ),
                ],
                &[("labels", &["research"])],
            ),
        ),
    ]
}

const SKILL_FIELDS: &[Field] = &[
    Field { key: "name", label: "Name", placeholder: "e.g. release", kind: FieldKind::Mono },
    Field { key: "description", label: "When to use it", placeholder: "the agent reads this to decide to load it", kind: FieldKind::Line },
    Field {
        key: "applies_to",
        label: "Applies to",
        placeholder: "everywhere, or labels: e.g. repo:github.com/me/app, client:acme",
        kind: FieldKind::Labels,
    },
    Field { key: "body", label: "Instructions", placeholder: "1. …", kind: FieldKind::Text },
];

fn item(pairs: &[(&str, &str)]) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (k, v) in pairs {
        m.insert((*k).into(), serde_json::Value::String((*v).into()));
    }
    m.insert("enabled".into(), serde_json::Value::Bool(true));
    serde_json::Value::Object(m)
}

fn command_examples() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("Empty", item(&[("name", ""), ("body", "")])),
        (
            "Fix a GitHub issue",
            item(&[
                ("name", "fix-issue"),
                ("description", "Fix a GitHub issue"),
                ("argument_hint", "<issue number>"),
                ("body", "Fix GitHub issue #$1.\n\n!`gh issue view $1`\n\nFind the cause, fix it, add a test, and run the tests."),
            ]),
        ),
        (
            "Write a commit message",
            item(&[
                ("name", "commit-message"),
                ("description", "Write a commit message for the staged changes"),
                ("body", "Write a commit message for these staged changes: a short summary line, then why.\n\n!`git diff --staged`"),
            ]),
        ),
        (
            "Explain code",
            item(&[
                ("name", "explain"),
                ("description", "Explain a file or function"),
                ("argument_hint", "<file or symbol>"),
                ("body", "Explain how $ARGUMENTS works: its purpose, the main flow, and anything surprising. Read the code first."),
            ]),
        ),
    ]
}

fn agent_examples() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("Empty", item(&[("name", ""), ("prompt", "")])),
        (
            "Code reviewer",
            item(&[
                ("name", "code-reviewer"),
                ("description", "Reviews changes for bugs, security problems and missing tests. Use after writing code."),
                ("tools", "Read, Grep, Glob, Bash"),
                (
                    "prompt",
                    "You are a careful code reviewer. Read the changes (git diff) and enough surrounding code to be sure. Report real problems only, most severe first, each with file, line, what goes wrong and a fix. Do not edit files.",
                ),
            ]),
        ),
        (
            "Test runner",
            item(&[
                ("name", "test-runner"),
                ("description", "Runs the tests and fixes failures. Use when tests need running or are failing."),
                ("tools", "Bash, Read, Edit, Grep, Glob"),
                (
                    "prompt",
                    "Run the project's tests. For each failure, find the cause and fix the code (not the test, unless the test is wrong). Report what failed and what you changed.",
                ),
            ]),
        ),
        (
            "Researcher",
            item(&[
                ("name", "researcher"),
                ("description", "Answers questions about the codebase or the web without changing anything."),
                ("tools", "Read, Grep, Glob, WebFetch"),
                (
                    "prompt",
                    "Investigate the question thoroughly and report what you found, with file paths and links. Do not change anything.",
                ),
            ]),
        ),
    ]
}

fn skill_examples() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("Empty", item(&[("name", ""), ("body", "")])),
        (
            "Cut a release",
            item(&[
                ("name", "release"),
                ("description", "How to cut a release of this project"),
                ("body", "1. Update CHANGELOG.md.\n2. Bump the version.\n3. Tag vX.Y.Z and push the tag."),
            ]),
        ),
        (
            "Slide deck (HTML)",
            item(&[
                ("name", "slide-deck"),
                ("description", "Making a slide deck or presentation"),
                (
                    "body",
                    "Make the deck as ONE self-contained HTML file (deck.html): no external scripts, fonts or images, so it works offline and in a sandbox (embed images as data: URLs or inline SVG).\n\n\
                     - Each slide is a <section> sized 16:9 (e.g. 1280x720), centered, one idea per slide: a title and at most 5 short bullets, or one chart or diagram.\n\
                     - A clean theme with CSS variables, large type (title 48px, body 28px), generous spacing, consistent accent colour.\n\
                     - Navigation: arrow keys and space move between slides, a slide counter in a corner, the URL hash tracks the slide.\n\
                     - Printing: @media print puts each slide on its own landscape page, so Print → Save as PDF gives a PDF deck.\n\
                     - Start with a title slide, end with a summary or next steps; put details in speaker notes (<aside class=notes>, hidden).\n\n\
                     Check it renders (the HTML is valid and every slide fits), then publish it with the publish tool, titled after the talk. On changes, edit the same file and publish again.",
                ),
            ]),
        ),
        (
            "Research report",
            item(&[
                ("name", "research-report"),
                ("description", "Researching a topic and writing up the findings"),
                (
                    "body",
                    "1. Restate the question and what a good answer needs; split it into 3-6 sub-questions.\n\
                     2. For each, search the web (several phrasings), read the most promising primary sources with the fetch tool, and note facts with their URLs. For independent sub-questions, use background subagents.\n\
                     3. Cross-check important claims in at least two sources; note disagreements and dates.\n\
                     4. Write report.md: a summary up front (the answer in a few sentences), then a section per sub-question, numbered citations [1] after the claims they support, and a Sources list (title, URL, date) at the end. Say plainly what is uncertain.\n\
                     5. Publish report.md with the publish tool.",
                ),
            ]),
        ),
    ]
}

/// A list setting (commands, agents, skills) edited item by item and saved
/// to the head, which checks it.
#[component]
fn ListSetting(
    name: &'static str,
    title: &'static str,
    noun: &'static str,
    help: &'static str,
    fields: &'static [Field],
    examples: fn() -> Vec<(&'static str, serde_json::Value)>,
) -> impl IntoView {
    let rows = RwSignal::new(Vec::<(u64, RwSignal<serde_json::Map<String, serde_json::Value>>)>::new());
    let next = StoredValue::new(0u64);
    let add = move |v: serde_json::Value| {
        let serde_json::Value::Object(m) = v else { return };
        let id = next.get_value();
        next.set_value(id + 1);
        rows.update(|r| r.push((id, RwSignal::new(m))));
    };
    let status = RwSignal::new(None::<Result<&'static str, String>>);
    let loaded = LocalResource::new(move || api::get_setting(name));
    Effect::new(move || {
        if let Some(Ok(serde_json::Value::Array(items))) = loaded.get() {
            rows.set(Vec::new());
            for v in items {
                add(v);
            }
        }
    });
    let save = move |_| {
        let items: Vec<serde_json::Value> =
            rows.get_untracked().iter().map(|(_, m)| serde_json::Value::Object(m.get_untracked())).collect();
        spawn_local(async move {
            let r = api::put_setting(name, serde_json::Value::Array(items)).await.map(|_| "Saved.");
            status.set(Some(r));
        });
    };
    let picked = RwSignal::new(String::new());
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <div>
                <h2 class="font-medium">{title}</h2>
                <p class="text-sm text-zinc-500">{help}</p>
            </div>
            <div class="space-y-3">
                <For each=move || rows.get() key=|(id, _)| *id let:row>
                    <ListItem item=row.1 fields on_remove=move || rows.update(|r| r.retain(|(id, _)| *id != row.0)) />
                </For>
                {move || rows.with(Vec::is_empty).then(|| view! { <p class="text-sm text-zinc-500">{format!("No {noun}s.")}</p> })}
            </div>
            <div class="flex flex-wrap items-center justify-between gap-3">
                <select class=format!("{INPUT} w-auto!") prop:value=move || picked.get()
                    on:change=move |ev| {
                        let label = event_target_value(&ev);
                        if let Some((_, v)) = examples().into_iter().find(|(l, _)| *l == label) {
                            add(v);
                        }
                        picked.set(String::new());
                    }>
                    <option value="">{format!("Add {} {noun}…", if noun.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" })}</option>
                    {examples().into_iter().map(|(l, _)| view! { <option value=l>{l}</option> }).collect_view()}
                </select>
                <div class="flex items-center gap-3">
                    {move || status.get().map(|r| match r {
                        Ok(m) => view! { <span class="text-sm text-emerald-600">{m}</span> }.into_any(),
                        Err(e) => view! { <span class="text-sm text-red-600">{e}</span> }.into_any(),
                    })}
                    <button class=BUTTON on:click=save>"Save"</button>
                </div>
            </div>
        </section>
    }
}

#[component]
fn ListItem(
    item: RwSignal<serde_json::Map<String, serde_json::Value>>,
    fields: &'static [Field],
    on_remove: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    // Labels fields hold lists, shown as comma-separated text.
    let get = move |k: &'static str| {
        move || {
            item.with(|m| match m.get(k) {
                Some(serde_json::Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "),
                Some(v) => v.as_str().unwrap_or("").to_owned(),
                None => String::new(),
            })
        }
    };
    let set = move |k: &'static str, v: String| {
        let labels = fields.iter().any(|f| f.key == k && f.kind == FieldKind::Labels);
        item.update(|m| {
            let value = if labels {
                serde_json::Value::Array(c6_api::labels::parse_labels(&v).into_iter().map(serde_json::Value::String).collect())
            } else {
                serde_json::Value::String(v)
            };
            m.insert(k.into(), value);
        })
    };
    let enabled = move || item.with(|m| m.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true));
    let (first, rest) = fields.split_first().expect("a name field");
    let lines: Vec<&'static Field> = rest.iter().filter(|f| f.kind != FieldKind::Text).collect();
    let texts: Vec<&'static Field> = rest.iter().filter(|f| f.kind == FieldKind::Text).collect();
    let input_class = |f: &Field| {
        format!("{INPUT} py-1 {}", if matches!(f.kind, FieldKind::Mono | FieldKind::Labels) { "font-mono text-xs" } else { "text-sm" })
    };
    view! {
        <div class=move || format!("space-y-2 rounded-md border border-zinc-200 p-3 dark:border-zinc-800 {}", if enabled() { "" } else { "opacity-60" })>
            <div class="flex items-center gap-2">
                <input type="checkbox" title="Enabled" prop:checked=enabled
                    on:change=move |ev| item.update(|m| { m.insert("enabled".into(), serde_json::Value::Bool(event_target_checked(&ev))); }) />
                <input class=format!("{} w-auto! min-w-0 flex-1", input_class(first)) placeholder=first.placeholder prop:value=get(first.key)
                    on:input=move |ev| set(first.key, event_target_value(&ev)) />
                <button class=format!("{BUTTON_QUIET} text-xs text-red-600") on:click=move |_| on_remove()>"Remove"</button>
            </div>
            <div class="grid gap-2 sm:grid-cols-2">
                {lines.into_iter().map(|f| if f.kind == FieldKind::Labels {
                    // Parsed when you leave the field, so typing is not reformatted.
                    view! {
                        <label class="space-y-0.5 sm:col-span-2">
                            <span class="text-xs text-zinc-500">{f.label}</span>
                            <input class=input_class(f) placeholder=f.placeholder prop:value=get(f.key)
                                title="Sessions with all of these labels: yours, or worker:, repo:, folder: (a path prefix) and model:"
                                on:change=move |ev| set(f.key, event_target_value(&ev)) />
                        </label>
                    }.into_any()
                } else {
                    view! {
                        <label class="space-y-0.5">
                            <span class="text-xs text-zinc-500">{f.label}</span>
                            <input class=input_class(f) placeholder=f.placeholder prop:value=get(f.key) on:input=move |ev| set(f.key, event_target_value(&ev)) />
                        </label>
                    }.into_any()
                }).collect_view()}
            </div>
            {texts.into_iter().map(|f| view! {
                <label class="block space-y-0.5">
                    <span class="text-xs text-zinc-500">{f.label}</span>
                    <textarea class=format!("{INPUT} min-h-24 font-mono text-xs") placeholder=f.placeholder prop:value=get(f.key)
                        on:input=move |ev| set(f.key, event_target_value(&ev))></textarea>
                </label>
            }).collect_view()}
        </div>
    }
}

/// Settings: secrets, as environment variables for sessions' commands.
/// Values are write-only.
#[component]
fn SecretsSetting() -> impl IntoView {
    let list = LocalResource::new(api::secrets);
    let name = RwSignal::new(String::new());
    let value = RwSignal::new(String::new());
    let description = RwSignal::new(String::new());
    let applies = RwSignal::new(String::new());
    // Editing an existing secret (its value can be left blank to keep it).
    let editing = RwSignal::new(false);
    let status = RwSignal::new(None::<Result<&'static str, String>>);
    let reset = move || {
        name.set(String::new());
        value.set(String::new());
        description.set(String::new());
        applies.set(String::new());
        editing.set(false);
    };
    let save = move |_| {
        let n = name.get_untracked().trim().to_owned();
        let req = c6_api::PutSecret {
            value: Some(value.get_untracked()).filter(|v| !v.is_empty()),
            description: description.get_untracked(),
            applies_to: c6_api::labels::parse_labels(&applies.get_untracked()),
        };
        spawn_local(async move {
            match api::put_secret(n, req).await {
                Ok(()) => {
                    status.set(Some(Ok("Saved. Sessions get it from their next turn.")));
                    reset();
                    list.refetch();
                }
                Err(e) => status.set(Some(Err(e))),
            }
        });
    };
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <div>
                <h2 class="font-medium">"Secrets"</h2>
                <p class="text-sm text-zinc-500">
                    "API keys, tokens and passwords for the agent's shell commands. Each is an environment variable (e.g. $GITHUB_TOKEN) in the sessions it applies to. The agent sees only its name and description: the value is never shown to it, is replaced by [secret:NAME] in command output, and stays encrypted on the head. Values can't be read back here, only replaced."
                </p>
            </div>
            <div class="divide-y divide-zinc-200 dark:divide-zinc-800">
                {move || list.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(v) if v.is_empty() => view! { <p class="py-2 text-sm text-zinc-500">"No secrets."</p> }.into_any(),
                    Ok(v) => v.into_iter().map(|s| {
                        let (edit, del) = (s.clone(), s.name.clone());
                        view! {
                            <div class="flex flex-wrap items-center gap-2 py-2 text-sm">
                                <span class="font-mono">{format!("${}", s.name)}</span>
                                <span class="min-w-0 flex-1 truncate text-zinc-500">{s.description.clone()}</span>
                                {s.applies_to.iter().cloned().map(|l| view! { <crate::labels::LabelChip label=l auto=true stay=true /> }).collect_view()}
                                <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| {
                                    name.set(edit.name.clone());
                                    description.set(edit.description.clone());
                                    applies.set(edit.applies_to.join(", "));
                                    value.set(String::new());
                                    editing.set(true);
                                }>"Edit"</button>
                                <button class=format!("{BUTTON_QUIET} text-xs text-red-600") on:click=move |_| {
                                    let n = del.clone();
                                    spawn_local(async move {
                                        match api::delete_secret(n).await {
                                            Ok(()) => list.refetch(),
                                            Err(e) => status.set(Some(Err(e))),
                                        }
                                    });
                                }>"Delete"</button>
                            </div>
                        }
                    }).collect_view().into_any(),
                })}
            </div>
            <div class="grid gap-2 sm:grid-cols-2">
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Name"</span>
                    <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="GITHUB_TOKEN" prop:disabled=move || editing.get()
                        prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev).to_ascii_uppercase()) />
                </label>
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Value"</span>
                    <input type="password" autocomplete="off" class=format!("{INPUT} py-1 font-mono text-xs")
                        placeholder=move || if editing.get() { "leave empty to keep the current value" } else { "the secret" }
                        prop:value=move || value.get() on:input=move |ev| value.set(event_target_value(&ev)) />
                </label>
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Description (shown to the agent)"</span>
                    <input class=format!("{INPUT} py-1 text-sm") placeholder="GitHub token with repo scope"
                        prop:value=move || description.get() on:input=move |ev| description.set(event_target_value(&ev)) />
                </label>
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Applies to"</span>
                    <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="everywhere, or labels, e.g. repo:github.com/me/app"
                        prop:value=move || applies.get() on:input=move |ev| applies.set(event_target_value(&ev)) />
                </label>
            </div>
            <div class="flex items-center justify-end gap-3">
                {move || status.get().map(|r| match r {
                    Ok(m) => view! { <span class="text-sm text-emerald-600">{m}</span> }.into_any(),
                    Err(e) => view! { <span class="text-sm text-red-600">{e}</span> }.into_any(),
                })}
                {move || editing.get().then(|| view! { <button class=BUTTON_QUIET on:click=move |_| reset()>"Cancel"</button> })}
                <button class=BUTTON on:click=save>{move || if editing.get() { "Save changes" } else { "Add secret" }}</button>
            </div>
        </section>
    }
}

/// Settings: the web search provider for models without one of their own.
#[component]
fn WebSearchSetting() -> impl IntoView {
    let setting = RwSignal::new(c6_api::WebSearchSetting::default());
    let loaded = LocalResource::new(move || api::get_setting("web-search"));
    Effect::new(move || {
        if let Some(Ok(v)) = loaded.get() {
            setting.set(serde_json::from_value(v).unwrap_or_default());
        }
    });
    let secrets = LocalResource::new(api::secrets);
    let status = RwSignal::new(None::<Result<&'static str, String>>);
    let save = move |_| {
        let v = serde_json::to_value(setting.get_untracked()).unwrap_or_default();
        spawn_local(async move {
            status.set(Some(api::put_setting("web-search", v).await.map(|_| "Saved. New sessions will use it.")));
        });
    };
    let provider = move || setting.with(|s| s.provider.clone());
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <div>
                <h2 class="font-medium">"Web search"</h2>
                <p class="text-sm text-zinc-500">
                    "Claude and OpenAI models search the web through their own provider. Others (Ollama, other OpenAI-compatible servers) get a web_search tool through the provider chosen here. Brave and Tavily need an API key, kept as a secret (it must apply to the sessions that search); SearXNG is self-hosted and needs only its URL (with the JSON format enabled)."
                </p>
            </div>
            <div class="grid gap-2 sm:grid-cols-2">
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Provider"</span>
                    <select class=format!("{INPUT} py-1") prop:value=provider on:change=move |ev| setting.update(|s| s.provider = event_target_value(&ev))>
                        <option value="">"Off"</option>
                        <option value="brave">"Brave Search API"</option>
                        <option value="tavily">"Tavily"</option>
                        <option value="searxng">"SearXNG (self-hosted)"</option>
                    </select>
                </label>
                {move || match provider().as_str() {
                    "searxng" => view! {
                        <label class="space-y-0.5">
                            <span class="text-xs text-zinc-500">"Instance URL"</span>
                            <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="http://10.0.0.5:8888"
                                prop:value=move || setting.with(|s| s.url.clone()) on:input=move |ev| setting.update(|s| s.url = event_target_value(&ev)) />
                        </label>
                    }.into_any(),
                    "brave" | "tavily" => view! {
                        <label class="space-y-0.5">
                            <span class="text-xs text-zinc-500">"API key (a secret)"</span>
                            <select class=format!("{INPUT} py-1 font-mono text-xs") prop:value=move || setting.with(|s| s.secret.clone())
                                on:change=move |ev| setting.update(|s| s.secret = event_target_value(&ev))>
                                <option value="">"Choose a secret…"</option>
                                {move || secrets.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|s| {
                                    let n = s.name.clone();
                                    view! { <option value=n.clone()>{format!("${n}")}</option> }
                                }).collect_view()}
                            </select>
                        </label>
                    }.into_any(),
                    _ => ().into_any(),
                }}
            </div>
            <label class="flex items-center gap-2 text-sm" class:hidden=move || provider().is_empty()>
                <input type="checkbox" prop:checked=move || setting.with(|s| s.always) on:change=move |ev| setting.update(|s| s.always = event_target_checked(&ev)) />
                "Offer it to every model, even those with a web search of their own"
            </label>
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

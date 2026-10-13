//! Settings: LLM connections, MCP servers, skills and hooks.

use c6_api::{Connection, CreateConnection, Hook};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT};

/// The sections of Settings: (id, title, what is in it).
const SECTIONS: [(&str, &str, &str); 11] = [
    ("models", "Models", "Connections, model routing, the assistant"),
    ("tools", "Agents and tools", "MCP servers, commands, agents, skills, hooks, web search, secrets"),
    ("rules", "Policies", "Ask or deny the agent's tool calls"),
    ("automation", "Automation", "Templates, workflows, automations"),
    ("notifications", "Notifications", "Channels, push on this device, links"),
    ("spending", "Prices and budgets", "What models cost, and spending limits"),
    ("hosts", "Hosts", "Machines reached over SSH"),
    ("memex", "Memory and memex", "Memory notes, the memex, its folders and scopes"),
    ("mail", "Mail and calendar", "Google accounts"),
    ("account", "Sign-in", "Passkeys, signed-in browsers, API tokens"),
    ("system", "System", "Upgrades of carbon6's databases, and their backups"),
];

/// A section's title, for column headers.
pub fn section_title(id: &str) -> Option<&'static str> {
    SECTIONS.iter().find(|(s, _, _)| *s == id).map(|(_, t, _)| *t)
}

#[component]
/// `note`: a notice after a redirect ("chatgpt": signed in with ChatGPT;
/// "link": signed in with a login link). `section`: which one is shown
/// ("" for the first).
pub fn SettingsPage(note: Option<String>, section: String) -> impl IntoView {
    let col = crate::workspace::use_column();
    let auth_required = LocalResource::new(api::auth_status);
    let current = RwSignal::new(if section_title(&section).is_some() { section } else { SECTIONS[0].0.to_owned() });
    let note = StoredValue::new(note);
    // Switching sections keeps this column and updates its address.
    let go = move |id: &'static str| {
        current.set(id.to_owned());
        if let Some(c) = col {
            c.replace(crate::workspace::View::Settings { note: None, section: id.to_owned() });
        }
    };
    let visible = move |id: &str| id != "account" || auth_required.get().and_then(Result::ok).is_some_and(|s| s.required);
    // On a phone the menu scrolls sideways: show the chosen section.
    let nav = NodeRef::<leptos::html::Nav>::new();
    Effect::new(move || {
        let id = current.get();
        if let Some(n) = nav.get()
            && let Ok(Some(b)) = n.query_selector(&format!("[data-section='{id}']"))
            && let Ok(b) = wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(b)
        {
            n.set_scroll_left(b.offset_left() - 16);
        }
    });
    let heading = move || SECTIONS.iter().find(|(s, _, _)| current.with(|c| c == s)).map(|(_, t, w)| (*t, *w)).unwrap_or(("", ""));
    view! {
        <main class="mx-auto max-w-5xl px-4 py-6 md:flex md:gap-8">
            <nav node_ref=nav class="relative -mx-4 mb-4 flex gap-1 overflow-x-auto px-4 pb-1 text-sm md:sticky md:top-4 md:mx-0 md:mb-0 md:w-48 md:shrink-0 md:flex-col md:self-start md:overflow-visible md:px-0">
                {SECTIONS.iter().map(|(id, t, w)| {
                    let id = *id;
                    view! {
                        <button class=move || format!("shrink-0 whitespace-nowrap rounded-md px-3 py-1.5 text-left {}", if current.with(|c| c == id) {
                                "bg-zinc-200 font-medium text-zinc-900 dark:bg-zinc-800 dark:text-zinc-100"
                            } else {
                                "text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-900"
                            })
                            class:hidden=move || !visible(id)
                            data-section=id title=*w on:click=move |_| go(id)>{*t}</button>
                    }
                }).collect_view()}
            </nav>
            <div class="min-w-0 flex-1 space-y-6">
                <div>
                    <h1 class="text-lg font-semibold">{move || heading().0}</h1>
                    <p class="text-sm text-zinc-500">{move || heading().1}</p>
                </div>
                {move || match current.get().as_str() {
                    "tools" => view! {
                    <JsonSetting
                        name="mcp-servers"
                        title="MCP servers (connectors)"
                        help="Started for each new session: stdio servers run on the worker in the session's directory; HTTP servers are reached from the worker. Their tools appear as mcp__<server>__<tool>. ${NAME} in a command, argument, env value, URL or bearer token is replaced by that secret (Settings → Secrets); \"applies_to\": [labels] limits a server to matching sessions. Start from a connector preset below."
                        presets=connector_presets
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
                        <HooksSetting />
                        <WebSearchSetting />
                        <SecretsSetting />
                    }.into_any(),
                    "rules" => view! {
                    <ListSetting
                        name="policies"
                        title="Policies"
                        noun="policy"
                        help="Rules for the agent's tool calls in sessions with given labels: deny them, or have them wait for your approval (in the transcript, the inbox and a notification; an hour without an answer means no). Tools is a regular expression over tool names (Bash, Edit, Write, … work with every model); Pattern one over the call's input. The first matching policy applies. Claude Code sessions are not covered."
                        fields=POLICY_FIELDS
                        examples=policy_examples
                    />
                    }.into_any(),
                    "automation" => view! {
                    <ListSetting
                        name="templates"
                        title="Templates"
                        noun="template"
                        help="Saved ways to start a session: where (a worker, every online worker with *, or SSH hosts through a worker), how (connection, model, labels) and what to ask. Several targets make a fleet run: one session each, sharing a run: label, so mission control shows them together. Run them from the Sessions page, with c6 run -t, on a schedule or from a webhook. In the prompt, {{input}} is what the run was given, {{target}} the worker or host, {{date}} and {{time}} now."
                        fields=TEMPLATE_FIELDS
                        examples=template_examples
                    />
                    <JsonSetting
                        name="workflows"
                        title="Workflows"
                        help="Steps in order: run a template (its sessions' last replies are the step's output), approve (wait for your go-ahead: in the panel below, or from the notification's link), or notify. In inputs, titles and messages, {{input}} is the workflow's input and {{steps.<id>.output}} an earlier step's output. when: {\"step\": id, \"contains\" / \"not_contains\": text, or \"changed\": true} runs a step only if that holds (changed compares the output's last RESULT: line with the previous run's, so ask the template for one). A failed run step ends the run unless continue_on_failure. Run workflows by hand below, or from an automation (choose the workflow as its template)."
                        presets=workflow_presets
                        example=r#"[
          {"name": "disk-watch", "steps": [
            {"id": "check", "kind": "run", "template": "disk-check", "input": "{{input}}"},
            {"id": "tell", "kind": "notify", "title": "Disks changed", "message": "{{steps.check.output}}", "when": {"step": "check", "changed": true}}
          ]}
        ]"#
                    />
                        <WorkflowRunsPanel />
                        <AutomationsSetting />
                    }.into_any(),
                    "notifications" => view! {
                    <ListSetting
                        name="channels"
                        title="Notifications"
                        noun="channel"
                        help="Where carbon6 tells you about your sessions while you are away: push (your browsers and phones, turned on below on each device), ntfy (a phone app; the URL is your topic, e.g. https://ntfy.sh/a-long-random-name), a Slack or Discord webhook, any webhook (JSON), or email over SMTP. Events: approval (with a link to Allow or Deny that works without signing in, for an hour), failed, finished, notice (the agent's notify tool), output, automation, budget; empty means approval, failed, notice, automation and budget. Applies to: only sessions with these labels. Tokens and SMTP passwords are secrets (Settings → Secrets)."
                        fields=CHANNEL_FIELDS
                        examples=channel_examples
                    />
                        <NotifyExtras />
                        <crate::push::PushSetting />
                    }.into_any(),
                    "spending" => view! {
                    <ListSetting
                        name="prices"
                        title="Prices"
                        noun="price"
                        help="What model calls cost, in USD per million tokens, for Usage and budgets. A model is an exact id or a prefix ending in * (claude-sonnet-*); the most specific entry wins. Cache read and write default to the input price. Copy the numbers from your provider's price list; models without a price count as $0 (Usage lists them)."
                        fields=PRICE_FIELDS
                        examples=price_examples
                    />
                    <ListSetting
                        name="budgets"
                        title="Budgets"
                        noun="budget"
                        help="Spending limits per day, week (from Monday) or month, for sessions with given labels (e.g. automation:, template:triage, client:acme; empty: everything). Notify tells you once per period when one is used up (as a budget notification); block also stops running turns and refuses new sessions and messages until the period ends. See where each stands under Usage."
                        fields=BUDGET_FIELDS
                        examples=budget_examples
                    />
                    }.into_any(),
                    "hosts" => view! {
                    <ListSetting
                        name="hosts"
                        title="Hosts"
                        noun="host"
                        help="Machines your workers reach over SSH (keys and ~/.ssh/config on the worker; carbon6 never handles SSH keys). Start a session on one by its name; it gets the host's labels (so policies, budgets, routing and memory for env:prod apply). Templates can run on every host with given labels. Check every N minutes to be told when one stops answering (a host notification). See them on the Workers page."
                        fields=HOST_FIELDS
                        examples=host_examples
                    />
                    }.into_any(),
                    "memex" => view! {
                        <MemorySetting />
                        <crate::memex::MemexSettings />
                    <ListSetting
                        name="memex_folders"
                        title="Memex folders"
                        noun="folder"
                        help="Folders on the memex's machine that it keeps in sync: documents and papers (PDF, Word, Markdown, HTML, text) in them and their subfolders are read, changes are picked up every few minutes, and removed files leave the memex. Hidden files are skipped."
                        fields=FOLDER_FIELDS
                        examples=folder_examples
                    />
                    <ListSetting
                        name="memex_scopes"
                        title="Memex scopes"
                        noun="rule"
                        help="What sessions may see in the memex. Every session sees sessions, outputs, web pages, notes, documents and papers; a rule lets sessions with its labels also see more kinds (email, photo). You, in the UI, see everything."
                        fields=SCOPE_FIELDS
                        examples=scope_examples
                    />
                    }.into_any(),
                    "mail" => view! { <crate::mail::GoogleSettings /> }.into_any(),
                    "system" => view! { <Upgrades /> }.into_any(),
                    "account" => view! {
                        {move || match auth_required.get().and_then(Result::ok) {
                            Some(s) if s.required => view! { <crate::auth::SignInSettings after_link=note.get_value().as_deref() == Some("link") /> }.into_any(),
                            Some(_) => view! { <p class="text-sm text-zinc-500">"Sign-in is off on this head (it is started without authentication)."</p> }.into_any(),
                            None => ().into_any(),
                        }}
                    }.into_any(),
                    _ => view! {
                        <Connections signed_in=note.get_value().as_deref() == Some("chatgpt") />
                    <ListSetting
                        name="routes"
                        title="Model routing"
                        noun="rule"
                        help="Which connection and model a new session gets when it is started without one (from the Sessions page, c6 run, templates or automations), by its labels: yours, worker:, folder:, host:, template:. The first enabled rule that matches wins. Without a match: the connection's default model, else the model of your latest session on it. A subagent model, if set, runs the Task tool's subagents (a named agent's own model still wins)."
                        fields=ROUTE_FIELDS
                        examples=route_examples
                    />
                        <crate::assistant::AssistantSettings />
                    }.into_any(),
                }}
            </div>
        </main>
    }
}

/// Upgrades of the head's and the memex's databases, newest first.
#[component]
fn Upgrades() -> impl IntoView {
    let list = LocalResource::new(crate::api::upgrades);
    view! {
        <section class="space-y-2">
            <h2 class="font-medium">"Upgrades"</h2>
            <p class="text-sm text-zinc-500">"Each new version of carbon6 upgrades its databases when it starts (or with c6h upgrade and c6m upgrade). The database is copied to backups/ next to it first; a failed upgrade puts the copy back, so nothing changes. To go back to an earlier version, stop carbon6 and put the backup in place of the database. To see what an upgrade would do first: c6h upgrade --check (or c6m upgrade --check)."</p>
            {move || list.get().map(|r| match r {
                Err(e) => view! { <p class="text-sm text-red-600">{e}</p> }.into_any(),
                Ok(v) if v.is_empty() => view! { <p class="text-sm text-zinc-500">"No upgrades yet."</p> }.into_any(),
                Ok(v) => v.into_iter().map(|u| {
                    let what = match u.from_version {
                        None => format!("created at version {}", u.to_version),
                        Some(f) => format!("version {f} → {}", u.to_version),
                    };
                    view! {
                        <details class="rounded-md border border-zinc-200 px-3 py-2 dark:border-zinc-800">
                            <summary class="cursor-pointer select-none text-sm">
                                <span class="font-medium">{format!("{} · {}", u.service, u.database)}</span>
                                <span class="ml-2 text-zinc-500">{what}</span>
                                <span class="ml-2 text-xs text-zinc-500">{crate::util::ago(crate::util::now_ms() - u.at)}</span>
                            </summary>
                            <pre class="mt-2 overflow-auto whitespace-pre-wrap font-mono text-[11px]">{u.report}</pre>
                        </details>
                    }
                }).collect_view().into_any(),
            })}
        </section>
    }
}

/// LLM connections: the list, adding one, signing in with ChatGPT.
#[component]
fn Connections(signed_in: bool) -> impl IntoView {
    let connections = LocalResource::new(api::connections);
    let error = RwSignal::new(None::<String>);
    let signed_in = move || signed_in;
    view! {
            <section class="space-y-3">
                <div>
                    <h2 class="font-medium">"LLM connections"</h2>
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
    let reauth = RwSignal::new(false);
    let chatgpt = c.kind == "chatgpt";
    let again_id = c.id.clone();
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
            {chatgpt.then(|| view! { <button class=format!("{BUTTON_QUIET} text-zinc-500") title="Sign this account in again" on:click=move |_| reauth.update(|r| *r = !*r)>"Sign in again"</button> })}
            <button class=format!("{BUTTON_QUIET} text-zinc-500") on:click=test>"Test"</button>
            <button class=format!("{BUTTON_QUIET} text-zinc-500 hover:text-red-600") on:click=remove>"Delete"</button>
        </div>
        {move || reauth.get().then(|| view! { <ChatGptSignIn on_added=move || { reauth.set(false); on_change(); } again=again_id.clone() /> })}
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
fn ChatGptSignIn(
    on_added: impl Fn() + Copy + Send + Sync + 'static,
    /// Sign this connection's account in again (instead of adding one).
    #[prop(optional)]
    again: Option<String>,
) -> impl IntoView {
    let again = StoredValue::new(again);
    let name = RwSignal::new(String::new());
    let email = RwSignal::new(String::new());
    let url = RwSignal::new(None::<String>);
    let callback = RwSignal::new(String::new());
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let start = move |_| {
        pending.set(true);
        spawn_local(async move {
            let hint = Some(email.get_untracked()).filter(|e| !e.trim().is_empty());
            match api::start_chatgpt(name.get_untracked(), again.get_value(), hint).await {
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

    let adding = again.with_value(Option::is_none);
    view! {
        <section class=if adding { "space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800" } else { "space-y-2 px-4 pb-3" }>
            {adding.then(|| view! {
                <div>
                    <h2 class="font-medium">"Add a ChatGPT account"</h2>
                    <p class="text-sm text-zinc-500">"Use a ChatGPT plan for OpenAI models; add as many accounts and workspaces as you like, each its own connection. The head keeps and refreshes each sign-in; workers only ever get short-lived tokens. To add a different account than the one signed in at chatgpt.com in this browser, give its email, or sign out there first."</p>
                </div>
            })}
            {move || match url.get() {
                None if adding => view! {
                    <div class="flex flex-wrap items-end gap-2">
                        <label class="block min-w-40 flex-1 text-xs text-zinc-500">
                            "Connection name"
                            <input class=format!("mt-1 {INPUT}") placeholder="ChatGPT (your email)" prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
                        </label>
                        <label class="block min-w-40 flex-1 text-xs text-zinc-500">
                            "Account email (optional)"
                            <input class=format!("mt-1 {INPUT}") type="email" placeholder="to pick the account" prop:value=move || email.get() on:input=move |ev| email.set(event_target_value(&ev)) />
                        </label>
                        <button class=BUTTON disabled=move || pending.get() on:click=start>"Sign in"</button>
                    </div>
                }.into_any(),
                None => view! {
                    <div class="flex items-center gap-2 text-sm">
                        <span class="flex-1 text-zinc-500">"Sign this account in again (if its sign-in expired or was revoked)."</span>
                        <button class=BUTTON disabled=move || pending.get() on:click=start>"Sign in again"</button>
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
                            <input class=format!("{INPUT} font-mono text-xs") placeholder="http://127.0.0.1:…/callback?code=…"
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

/// A ready-made entry: (label, what it needs, the entry).
type Preset = (&'static str, &'static str, serde_json::Value);

/// Ready-made MCP servers: (label, the secrets it needs, its config).
fn workflow_presets() -> Vec<Preset> {
    use serde_json::json;
    vec![
        (
            "Tell me only when a check's result changes",
            "a template (here check) whose reply ends with a RESULT: line",
            json!({"name": "watch", "steps": [
                {"id": "check", "kind": "run", "template": "check", "input": "{{input}}"},
                {"id": "tell", "kind": "notify", "title": "Something changed", "message": "{{steps.check.output}}", "when": {"step": "check", "changed": true}}
            ]}),
        ),
        (
            "Check, ask me, then fix",
            "templates check (ending with RESULT: OK or RESULT: PROBLEM) and fix",
            json!({"name": "check-and-fix", "steps": [
                {"id": "check", "kind": "run", "template": "check", "input": "{{input}}"},
                {"id": "approve", "kind": "approve", "title": "Fix it?", "message": "{{steps.check.output}}", "when": {"step": "check", "contains": "RESULT: PROBLEM"}},
                {"id": "fix", "kind": "run", "template": "fix", "input": "{{steps.check.output}}", "when": {"step": "check", "contains": "RESULT: PROBLEM"}},
                {"id": "done", "kind": "notify", "title": "Fixed", "message": "{{steps.fix.output}}", "when": {"step": "check", "contains": "RESULT: PROBLEM"}}
            ]}),
        ),
        (
            "Research, then write it up",
            "templates research and write-up",
            json!({"name": "brief", "steps": [
                {"id": "research", "kind": "run", "template": "research", "input": "{{input}}"},
                {"id": "write", "kind": "run", "template": "write-up", "input": "Write a one-page brief from these notes:\n{{steps.research.output}}"},
                {"id": "tell", "kind": "notify", "title": "Brief ready", "message": "{{steps.write.output}}"}
            ]}),
        ),
    ]
}

fn step_tone(status: &str) -> &'static str {
    match status {
        "ok" => "bg-emerald-100 text-emerald-800 dark:bg-emerald-950 dark:text-emerald-300",
        "failed" | "denied" => "bg-red-100 text-red-800 dark:bg-red-950 dark:text-red-300",
        "running" => "bg-sky-100 text-sky-800 dark:bg-sky-950 dark:text-sky-300",
        "waiting" => "bg-amber-100 text-amber-900 dark:bg-amber-950 dark:text-amber-200",
        _ => "bg-zinc-100 text-zinc-500 dark:bg-zinc-800",
    }
}

/// Run workflows and follow their runs; answer approve steps.
#[component]
fn WorkflowRunsPanel() -> impl IntoView {
    let flows = LocalResource::new(api::workflows);
    let runs = LocalResource::new(api::workflow_runs);
    let (pick, input) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let status = RwSignal::new(None::<Result<String, String>>);
    let ws = crate::workspace::use_workspace();
    // Follow runs while any is unfinished.
    let busy =
        move || runs.get().and_then(Result::ok).is_some_and(|v| v.iter().any(|r| matches!(r.status.as_str(), "running" | "waiting")));
    let tick = leptos::prelude::set_interval_with_handle(
        move || {
            if busy() {
                runs.refetch();
            }
        },
        std::time::Duration::from_secs(3),
    )
    .ok();
    on_cleanup(move || {
        if let Some(t) = tick {
            t.clear();
        }
    });
    let start = move |_| {
        let name = pick.get_untracked();
        if name.is_empty() {
            status.set(Some(Err("Choose a workflow".into())));
            return;
        }
        spawn_local(async move {
            match api::run_workflow(name.clone(), input.get_untracked()).await {
                Ok(_) => {
                    status.set(Some(Ok(format!("Started {name}."))));
                    runs.refetch();
                }
                Err(e) => status.set(Some(Err(e))),
            }
        });
    };
    let decide = move |run: String, step: String, allow: bool| {
        spawn_local(async move {
            if let Err(e) = api::decide_workflow_step(run, step, allow).await {
                status.set(Some(Err(e)));
            }
            runs.refetch();
        });
    };
    view! {
        <section class="-mt-3 space-y-3 rounded-lg border border-zinc-200 p-4 text-sm dark:border-zinc-800">
            <div class="flex flex-wrap items-end gap-2">
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Run a workflow"</span>
                    <select class=format!("{INPUT} py-1") prop:value=move || pick.get() on:change=move |ev| pick.set(event_target_value(&ev))>
                        <option value="">"Choose…"</option>
                        {move || flows.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|w| w.enabled).map(|w| view! { <option value=w.name.clone()>{w.name.clone()}</option> }).collect_view()}
                    </select>
                </label>
                <label class="min-w-0 flex-1 space-y-0.5">
                    <span class="text-xs text-zinc-500">"Input ({{input}})"</span>
                    <input class=format!("{INPUT} py-1") placeholder="optional" prop:value=move || input.get() on:input=move |ev| input.set(event_target_value(&ev)) />
                </label>
                <button class=BUTTON on:click=start>"Run"</button>
            </div>
            {move || status.get().map(|r| match r {
                Ok(m) => view! { <p class="text-sm text-emerald-600">{m}</p> }.into_any(),
                Err(e) => view! { <p class="text-sm text-red-600">{e}</p> }.into_any(),
            })}
            <div class="divide-y divide-zinc-200 dark:divide-zinc-800">
                {move || runs.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(v) if v.is_empty() => view! { <p class="py-1 text-xs text-zinc-500">"No workflow runs yet."</p> }.into_any(),
                    Ok(v) => v.into_iter().map(|run| {
                        let rid = run.id.clone();
                        view! {
                            <div class="space-y-1 py-2">
                                <div class="flex flex-wrap items-center gap-2">
                                    <span class="font-medium">{run.workflow.clone()}</span>
                                    <span class=format!("rounded px-1.5 text-[11px] {}", step_tone(&run.status))>{run.status.clone()}</span>
                                    <span class="text-xs text-zinc-500">{format!("{} · {}", when(run.started_at), run.trigger)}</span>
                                    {run.error.clone().map(|e| view! { <span class="text-xs text-red-600">{e}</span> })}
                                </div>
                                <div class="flex flex-wrap items-center gap-1">
                                    {run.steps.into_iter().map(|s| {
                                        let (r1, r2, s1, s2) = (rid.clone(), rid.clone(), s.id.clone(), s.id.clone());
                                        let sessions = s.sessions.clone();
                                        let waiting = s.status == "waiting";
                                        let tip: String = s.output.chars().take(600).collect();
                                        view! {
                                            <span class=format!("inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px] {}", step_tone(&s.status))
                                                title=tip>
                                                {format!("{} · {}", s.id, s.status)}
                                                {sessions.into_iter().map(|sid| view! {
                                                    <button class="underline" title="Open the session" on:click=move |_| ws.open(None, crate::workspace::View::session(sid.clone()), crate::workspace::How::Beside)>"↗"</button>
                                                }).collect_view()}
                                            </span>
                                            {waiting.then(|| view! {
                                                <span class="flex gap-1">
                                                    <button class="rounded bg-emerald-600 px-2 text-xs text-white" on:click=move |_| decide(r1.clone(), s1.clone(), true)>"Go ahead"</button>
                                                    <button class="rounded bg-red-600 px-2 text-xs text-white" on:click=move |_| decide(r2.clone(), s2.clone(), false)>"Stop"</button>
                                                </span>
                                            })}
                                        }
                                    }).collect_view()}
                                </div>
                            </div>
                        }
                    }).collect_view().into_any(),
                })}
            </div>
        </section>
    }
}

fn connector_presets() -> Vec<Preset> {
    use serde_json::json;
    vec![
        (
            "GitHub (issues, PRs, code)",
            "GITHUB_TOKEN: a personal access token",
            json!({"name": "github", "transport": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
                   "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": "${GITHUB_TOKEN}"}}),
        ),
        (
            "Slack (read and post messages)",
            "SLACK_BOT_TOKEN (xoxb-…) and SLACK_TEAM_ID",
            json!({"name": "slack", "transport": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-slack"],
                   "env": {"SLACK_BOT_TOKEN": "${SLACK_BOT_TOKEN}", "SLACK_TEAM_ID": "${SLACK_TEAM_ID}"}}),
        ),
        (
            "PostgreSQL (read-only queries)",
            "DATABASE_URL: postgres://user:password@host/db",
            json!({"name": "postgres", "transport": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-postgres", "${DATABASE_URL}"]}),
        ),
        (
            "Browser (headless Chromium, for research and web tasks)",
            "nothing (needs Node on the worker; downloads a browser on first use)",
            json!({"name": "browser", "transport": "stdio", "command": "npx", "args": ["-y", "@playwright/mcp@latest", "--headless", "--isolated"]}),
        ),
        (
            "Remote MCP server (HTTP, bearer token)",
            "a secret with the server's token, e.g. MCP_TOKEN",
            json!({"name": "remote", "transport": "http", "url": "https://example.com/mcp", "bearer": "${MCP_TOKEN}"}),
        ),
    ]
}

/// A JSON-array setting edited as text, validated by the head on save.
#[component]
fn JsonSetting(
    name: &'static str,
    title: &'static str,
    help: &'static str,
    example: &'static str,
    /// Entries to add from a menu: (label, what it needs, entry).
    #[prop(optional)]
    presets: Option<fn() -> Vec<Preset>>,
) -> impl IntoView {
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
            {presets.map(|presets| {
                let needs = RwSignal::new(None::<String>);
                view! {
                    <div class="flex flex-wrap items-center gap-2">
                        <select class=format!("{INPUT} w-auto! py-1 text-sm") on:change=move |ev| {
                            let label = event_target_value(&ev);
                            if let Some((l, need, entry)) = presets().into_iter().find(|(l, _, _)| *l == label) {
                                // Append to the list being edited.
                                let mut list = serde_json::from_str::<Vec<serde_json::Value>>(&text.get_untracked()).unwrap_or_default();
                                list.push(entry);
                                text.set(serde_json::to_string_pretty(&list).unwrap_or_default());
                                needs.set(Some(format!("Added {l}. It needs {need}. Save to use it in new sessions.")));
                            }
                        }>
                            <option value="">"Add a connector…"</option>
                            {presets().into_iter().map(|(l, _, _)| view! { <option value=l>{l}</option> }).collect_view()}
                        </select>
                        {move || needs.get().map(|n| view! { <span class="text-xs text-zinc-500">{n}</span> })}
                    </div>
                }
            })}
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
    /// One of these values.
    Choice(&'static [&'static str]),
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

const POLICY_FIELDS: &[Field] = &[
    Field { key: "name", label: "Name", placeholder: "e.g. Ask before changes on prod", kind: FieldKind::Line },
    Field { key: "applies_to", label: "Applies to", placeholder: "everywhere, or labels, e.g. env:prod", kind: FieldKind::Labels },
    Field { key: "action", label: "Action", placeholder: "", kind: FieldKind::Choice(&["ask", "deny"]) },
    Field { key: "tools", label: "Tools (regex)", placeholder: "every tool, or e.g. Bash|Write|Edit", kind: FieldKind::Mono },
    Field { key: "pattern", label: "Input pattern (regex)", placeholder: "any input, or e.g. rm\\s+-rf", kind: FieldKind::Mono },
    Field { key: "message", label: "Message", placeholder: "why (shown to you and the agent)", kind: FieldKind::Line },
];

fn policy_examples() -> Vec<(&'static str, serde_json::Value)> {
    let p = |name: &str, labels: &[&str], action: &str, tools: &str, pattern: &str, message: &str| {
        let mut v = item(&[("name", name), ("action", action), ("tools", tools), ("pattern", pattern), ("message", message)]);
        v["applies_to"] = serde_json::json!(labels);
        v
    };
    vec![
        ("Empty", p("", &[], "ask", "", "", "")),
        (
            "Ask before any change on prod",
            p("Ask before changes on prod", &["env:prod"], "ask", "Bash|Write|Edit", "", "Production: every command and edit needs a yes."),
        ),
        (
            "Never rm -rf or wipe disks on prod",
            p(
                "No destructive commands on prod",
                &["env:prod"],
                "deny",
                "Bash",
                r"rm\s+-[a-zA-Z]*r[a-zA-Z]*f|mkfs|dd\s+if=|DROP\s+(TABLE|DATABASE)",
                "Too destructive for production; do it by hand.",
            ),
        ),
        (
            "Ask before installing or removing packages",
            p(
                "Ask before package changes",
                &[],
                "ask",
                "Bash",
                r"(apt|apt-get|dnf|yum|brew|pip|npm)\s+(-\S+\s+)*(install|remove|purge|uninstall|upgrade)",
                "",
            ),
        ),
        ("Ask before git push", p("Ask before git push", &[], "ask", "Bash", r"git\s+push", "")),
        (
            "Ask before the browser fills in forms",
            p(
                "Ask before browser input",
                &[],
                "ask",
                "browser|Browser",
                r#""action":"(type|select)""#,
                "The agent wants to type into a web page.",
            ),
        ),
        (
            "Read-only research sessions",
            p("Research is read-only", &["research"], "deny", "Write|Edit|Bash", "", "Research sessions only read and report."),
        ),
    ]
}

const PRICE_FIELDS: &[Field] = &[
    Field { key: "model", label: "Model", placeholder: "e.g. claude-sonnet-* or gpt-5.5", kind: FieldKind::Mono },
    Field { key: "input", label: "Input ($/M tokens)", placeholder: "e.g. 3", kind: FieldKind::Mono },
    Field { key: "output", label: "Output ($/M tokens)", placeholder: "e.g. 15", kind: FieldKind::Mono },
    Field { key: "cache_read", label: "Cache read ($/M)", placeholder: "the input price", kind: FieldKind::Mono },
    Field { key: "cache_write", label: "Cache write ($/M)", placeholder: "the input price", kind: FieldKind::Mono },
];

fn price_examples() -> Vec<(&'static str, serde_json::Value)> {
    let p = |model: &str, i: &str, o: &str, cr: &str, cw: &str| {
        item(&[("model", model), ("input", i), ("output", o), ("cache_read", cr), ("cache_write", cw)])
    };
    vec![
        ("Empty", p("", "", "", "", "")),
        ("A family by prefix (fill in its prices)", p("claude-sonnet-*", "", "", "", "")),
        ("Local models are free", p("qwen*", "0", "0", "", "")),
    ]
}

const BUDGET_FIELDS: &[Field] = &[
    Field { key: "name", label: "Name", placeholder: "e.g. Automations per day", kind: FieldKind::Line },
    Field { key: "applies_to", label: "Applies to", placeholder: "everything, or labels, e.g. automation:", kind: FieldKind::Labels },
    Field { key: "period", label: "Period", placeholder: "", kind: FieldKind::Choice(&["day", "week", "month"]) },
    Field { key: "limit", label: "Limit (USD)", placeholder: "e.g. 5", kind: FieldKind::Mono },
    Field { key: "action", label: "When used up", placeholder: "", kind: FieldKind::Choice(&["notify", "block"]) },
];

fn budget_examples() -> Vec<(&'static str, serde_json::Value)> {
    let b = |name: &str, labels: &[&str], period: &str, limit: &str, action: &str| {
        let mut v = item(&[("name", name), ("period", period), ("limit", limit), ("action", action)]);
        v["applies_to"] = serde_json::json!(labels);
        v
    };
    vec![
        ("Empty", b("", &[], "day", "", "notify")),
        ("Automations: $5 a day, then stop", b("Automations per day", &["automation:"], "day", "5", "block")),
        ("Everything: tell me past $100 a month", b("Monthly total", &[], "month", "100", "notify")),
        ("A client: $50 a week", b("Acme per week", &["client:acme"], "week", "50", "block")),
    ]
}

const FOLDER_FIELDS: &[Field] = &[
    Field { key: "path", label: "Folder", placeholder: "e.g. ~/Documents/Papers (on the memex's machine)", kind: FieldKind::Mono },
    Field { key: "labels", label: "Labels", placeholder: "e.g. research", kind: FieldKind::Labels },
];

fn folder_examples() -> Vec<(&'static str, serde_json::Value)> {
    let f = |path: &str, labels: &[&str]| {
        let mut v = item(&[("path", path)]);
        v["labels"] = serde_json::json!(labels);
        v
    };
    vec![("Empty", f("", &[])), ("Research papers", f("~/Documents/Papers", &["research"])), ("Notes", f("~/Documents/Notes", &[]))]
}

const SCOPE_FIELDS: &[Field] = &[
    Field { key: "name", label: "Name", placeholder: "e.g. Mail sessions see email", kind: FieldKind::Line },
    Field { key: "applies_to", label: "Sessions with labels", placeholder: "e.g. mail", kind: FieldKind::Labels },
    Field { key: "kinds", label: "May also see", placeholder: "email, photo", kind: FieldKind::Labels },
];

fn scope_examples() -> Vec<(&'static str, serde_json::Value)> {
    let r = |name: &str, labels: &[&str], kinds: &[&str]| {
        let mut v = item(&[("name", name)]);
        v["applies_to"] = serde_json::json!(labels);
        v["kinds"] = serde_json::json!(kinds);
        v
    };
    vec![
        ("Empty", r("", &[], &[])),
        ("Mail sessions see email", r("Mail sessions see email", &["mail"], &["email"])),
        ("Photo sessions see photos", r("Photo sessions see photos", &["photos"], &["photo"])),
    ]
}

const CHANNEL_FIELDS: &[Field] = &[
    Field { key: "name", label: "Name", placeholder: "e.g. phone", kind: FieldKind::Line },
    Field {
        key: "kind",
        label: "Kind",
        placeholder: "",
        kind: FieldKind::Choice(&["push", "ntfy", "slack", "discord", "webhook", "email"]),
    },
    Field {
        key: "url",
        label: "URL (email: SMTP server)",
        placeholder: "https://ntfy.sh/your-topic, a webhook URL, or smtp.example.com:587",
        kind: FieldKind::Mono,
    },
    Field {
        key: "secret",
        label: "Token or password (a secret's name)",
        placeholder: "none; or e.g. NTFY_TOKEN, SMTP_PASSWORD",
        kind: FieldKind::Mono,
    },
    Field { key: "user", label: "Email: user (and sender)", placeholder: "only for email: you@example.com", kind: FieldKind::Mono },
    Field { key: "to", label: "Email: to", placeholder: "only for email: you@example.com", kind: FieldKind::Mono },
    Field { key: "events", label: "Events", placeholder: "approval, failed, notice, automation, budget", kind: FieldKind::Labels },
    Field {
        key: "applies_to",
        label: "Applies to",
        placeholder: "every session, or labels, e.g. env:prod, automation:",
        kind: FieldKind::Labels,
    },
];

fn channel_examples() -> Vec<(&'static str, serde_json::Value)> {
    let c = |name: &str, kind: &str, url: &str, secret: &str, events: &[&str], labels: &[&str]| {
        let mut v = item(&[("name", name), ("kind", kind), ("url", url), ("secret", secret), ("user", ""), ("to", "")]);
        v["events"] = serde_json::json!(events);
        v["applies_to"] = serde_json::json!(labels);
        v
    };
    let mut email = c("email", "email", "smtp.gmail.com:587", "SMTP_PASSWORD", &["failed", "automation", "budget"], &[]);
    email["user"] = "you@gmail.com".into();
    email["to"] = "you@gmail.com".into();
    vec![
        ("Empty", c("", "ntfy", "", "", &[], &[])),
        ("This browser and phone (web push): approvals and problems", c("devices", "push", "", "", &[], &[])),
        ("Phone (ntfy): approvals and problems", c("phone", "ntfy", "https://ntfy.sh/pick-a-long-random-topic", "", &[], &[])),
        (
            "Slack: automation results",
            c("slack", "slack", "https://hooks.slack.com/services/…", "", &["finished", "failed", "notice"], &["automation:"]),
        ),
        (
            "Discord: everything on prod",
            c(
                "discord",
                "discord",
                "https://discord.com/api/webhooks/…",
                "",
                &["approval", "failed", "finished", "notice", "output"],
                &["env:prod"],
            ),
        ),
        ("Email: failures and budgets", email),
        ("Webhook (JSON) for your own tools", c("hook", "webhook", "https://example.com/carbon6", "HOOK_TOKEN", &[], &[])),
    ]
}

#[component]
fn NotifyExtras() -> impl IntoView {
    let url = RwSignal::new(String::new());
    let loaded = LocalResource::new(move || api::get_setting("public_url"));
    Effect::new(move || {
        if let Some(Ok(v)) = loaded.get() {
            url.set(v["url"].as_str().unwrap_or_default().to_owned());
        }
    });
    let channels = LocalResource::new(move || api::get_setting("channels"));
    let test_name = RwSignal::new(String::new());
    let status = RwSignal::new(None::<Result<String, String>>);
    let save = move |_| {
        let v = serde_json::json!({"url": url.get_untracked()});
        spawn_local(async move {
            status.set(Some(api::put_setting("public_url", v).await.map(|_| "Saved.".to_owned())));
        });
    };
    let test = move |_| {
        let name = test_name.get_untracked();
        if name.is_empty() {
            status.set(Some(Err("Save a channel, then pick it to test".into())));
            return;
        }
        status.set(Some(Ok("Sending…".into())));
        spawn_local(async move {
            status.set(Some(api::test_channel(name.clone()).await.map(|_| format!("Sent a test to {name}."))));
        });
    };
    view! {
        <section class="-mt-3 space-y-2 rounded-lg border border-zinc-200 p-4 text-sm dark:border-zinc-800">
            <label class="block space-y-0.5">
                <span class="text-xs text-zinc-500">"Links in notifications open"</span>
                <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="https://head.example:7700 (set when you first save a channel)"
                    prop:value=move || url.get() on:input=move |ev| url.set(event_target_value(&ev)) />
            </label>
            <p class="text-xs text-zinc-500">"The address your phone or other devices reach this head at. Approval links work there without signing in, for an hour."</p>
            <div class="flex flex-wrap items-center justify-end gap-2">
                {move || status.get().map(|r| match r {
                    Ok(m) => view! { <span class="text-sm text-emerald-600">{m}</span> }.into_any(),
                    Err(e) => view! { <span class="text-sm text-red-600">{e}</span> }.into_any(),
                })}
                <button class=BUTTON_QUIET on:click=save>"Save address"</button>
                <select class=format!("{INPUT} w-auto py-1") prop:value=move || test_name.get() on:change=move |ev| test_name.set(event_target_value(&ev))
                    on:focus=move |_| channels.refetch()>
                    <option value="">"Channel to test…"</option>
                    {move || channels.get().and_then(Result::ok).and_then(|v| v.as_array().cloned()).unwrap_or_default().into_iter().filter_map(|c| {
                        let n = c["name"].as_str()?.to_owned();
                        Some(view! { <option value=n.clone()>{n.clone()}</option> })
                    }).collect_view()}
                </select>
                <button class=BUTTON on:click=test>"Send a test"</button>
            </div>
        </section>
    }
}

const ROUTE_FIELDS: &[Field] = &[
    Field { key: "name", label: "Name", placeholder: "e.g. Ops on the cheap model", kind: FieldKind::Line },
    Field {
        key: "applies_to",
        label: "Applies to",
        placeholder: "every session, or labels, e.g. host:, template:triage",
        kind: FieldKind::Labels,
    },
    Field { key: "connection", label: "Connection", placeholder: "the session's (or your latest)", kind: FieldKind::Line },
    Field { key: "model", label: "Model", placeholder: "e.g. claude-sonnet-5-5", kind: FieldKind::Mono },
    Field { key: "effort", label: "Effort", placeholder: "the model's default; or low, medium, high, xhigh, max", kind: FieldKind::Mono },
    Field { key: "subagent_model", label: "Subagent model", placeholder: "the session's; or e.g. claude-haiku-4-5", kind: FieldKind::Mono },
];

fn route_examples() -> Vec<(&'static str, serde_json::Value)> {
    let r = |name: &str, labels: &[&str], model: &str, effort: &str, sub: &str| {
        let mut v = item(&[("name", name), ("connection", ""), ("model", model), ("effort", effort), ("subagent_model", sub)]);
        v["applies_to"] = serde_json::json!(labels);
        v
    };
    vec![
        ("Empty", r("", &[], "", "", "")),
        ("Default: Opus, Haiku subagents", r("Default", &[], "claude-opus-5-5", "", "claude-haiku-4-5")),
        ("Remote hosts on a fast model", r("Ops over SSH", &["host:"], "claude-sonnet-5-5", "low", "")),
        ("Research at high effort", r("Research", &["research"], "claude-opus-5-5", "high", "claude-sonnet-5-5")),
        ("A template on a local model", r("Triage locally", &["template:triage"], "qwen3.6:27b", "", "")),
    ]
}

const HOST_FIELDS: &[Field] = &[
    Field { key: "name", label: "Name", placeholder: "e.g. web1", kind: FieldKind::Mono },
    Field { key: "target", label: "SSH target", placeholder: "user@host[:port] or an ssh_config alias", kind: FieldKind::Mono },
    Field { key: "worker", label: "Through worker", placeholder: "any online worker", kind: FieldKind::Mono },
    Field { key: "check_every", label: "Check every (minutes)", placeholder: "only when asked", kind: FieldKind::Mono },
    Field { key: "labels", label: "Labels", placeholder: "e.g. role:web, env:prod", kind: FieldKind::Labels },
    Field { key: "description", label: "Description", placeholder: "what it is", kind: FieldKind::Line },
];

fn host_examples() -> Vec<(&'static str, serde_json::Value)> {
    let h = |name: &str, target: &str, labels: &[&str], every: &str, description: &str| {
        let mut v = item(&[("name", name), ("target", target), ("worker", ""), ("check_every", every), ("description", description)]);
        v["labels"] = serde_json::json!(labels);
        v
    };
    vec![
        ("Empty", h("", "", &[], "", "")),
        (
            "A production web server, checked every 15 minutes",
            h("web1", "deploy@web1.example.com", &["role:web", "env:prod"], "15", "nginx + app"),
        ),
        ("A database server", h("db1", "admin@db1.example.com", &["role:db", "env:prod"], "15", "Postgres primary")),
        ("A home lab box (ssh_config alias)", h("nas", "nas", &["env:home"], "", "")),
    ]
}

const TEMPLATE_FIELDS: &[Field] = &[
    Field { key: "name", label: "Name", placeholder: "e.g. disk-check", kind: FieldKind::Mono },
    Field { key: "description", label: "Description", placeholder: "what it is for", kind: FieldKind::Line },
    Field { key: "workers", label: "Workers", placeholder: "the online one; or names; * for every online worker", kind: FieldKind::Labels },
    Field { key: "ssh", label: "SSH hosts", placeholder: "none; or user@host, … (one session each)", kind: FieldKind::Labels },
    Field {
        key: "hosts",
        label: "Hosts by label",
        placeholder: "none; or e.g. role:web, env:prod (or * for all); one session each",
        kind: FieldKind::Labels,
    },
    Field { key: "cwd", label: "Folder", placeholder: "scratch workspace (or home on SSH hosts)", kind: FieldKind::Mono },
    Field { key: "connection", label: "Connection", placeholder: "by routing rules", kind: FieldKind::Line },
    Field { key: "model", label: "Model", placeholder: "by routing rules (Settings → Model routing)", kind: FieldKind::Mono },
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
                Some(serde_json::Value::Number(n)) => n.to_string(),
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
                {lines.into_iter().map(|f| if let FieldKind::Choice(options) = f.kind {
                    view! {
                        <label class="space-y-0.5">
                            <span class="text-xs text-zinc-500">{f.label}</span>
                            <select class=format!("{INPUT} py-1 text-sm") prop:value=get(f.key) on:change=move |ev| set(f.key, event_target_value(&ev))>
                                {options.iter().map(|o| view! { <option value=*o>{*o}</option> }).collect_view()}
                            </select>
                        </label>
                    }.into_any()
                } else if f.kind == FieldKind::Labels {
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

const SCHEDULE_PRESETS: &[(&str, &str)] = &[
    ("Every hour", "0 * * * *"),
    ("Every day at 8:00", "0 8 * * *"),
    ("Weekdays at 9:00", "0 9 * * mon-fri"),
    ("Mondays at 7:30", "30 7 * * mon"),
    ("Every 15 minutes", "*/15 * * * *"),
];

fn when(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    let s: String = d.to_locale_string("default", &wasm_bindgen::JsValue::UNDEFINED).into();
    s
}

/// Settings: automations, which start templates on a schedule or from a
/// webhook.
#[component]
fn AutomationsSetting() -> impl IntoView {
    let list = LocalResource::new(api::automations);
    let templates = LocalResource::new(api::templates);
    let workflow_list = LocalResource::new(api::workflows);
    let filter = crate::labels::use_filter().0;
    let ws = crate::workspace::use_workspace();
    let (name, template, trigger, cron, input) = (
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
        RwSignal::new("schedule".to_owned()),
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
    );
    let disable_after = RwSignal::new(3u32);
    // The automation whose run history is open.
    let history = RwSignal::new(None::<String>);
    let status = RwSignal::new(None::<Result<String, String>>);
    // A webhook's URL, shown once (after creating it or a new token).
    let url = RwSignal::new(None::<String>);
    let origin = StoredValue::new(window().location().origin().unwrap_or_default());
    let show_url = move |a: &c6_api::Automation| {
        if let Some(p) = &a.webhook_url {
            url.set(Some(format!("{}{p}", origin.get_value())));
        }
    };
    let create = move |_| {
        let e = c6_api::AutomationEdit {
            name: Some(name.get_untracked()),
            template: Some(template.get_untracked()),
            trigger: Some(trigger.get_untracked()),
            cron: Some(cron.get_untracked()),
            input: Some(input.get_untracked()),
            enabled: Some(true),
            disable_after: Some(disable_after.get_untracked()),
        };
        spawn_local(async move {
            match api::create_automation(e).await {
                Ok(a) => {
                    show_url(&a);
                    status.set(Some(Ok(format!("Added {}.", a.name))));
                    name.set(String::new());
                    input.set(String::new());
                    list.refetch();
                }
                Err(e) => status.set(Some(Err(e))),
            }
        });
    };
    let open_run = move |run: String| {
        filter.set(vec![format!("run:{run}")]);
        ws.open(None, crate::workspace::View::Mission, crate::workspace::How::Beside);
    };
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <div>
                <h2 class="font-medium">"Automations"</h2>
                <p class="text-sm text-zinc-500">
                    "Start a template on a schedule (the head's local time) or when a webhook is called (the request body becomes the template's {{input}}; the URL is its only credential, so keep it private). Runs are labelled automation:<name> and land in your inbox when they finish."
                </p>
            </div>
            {move || url.get().map(|u| view! {
                <div class="space-y-1 rounded-md bg-amber-50 p-3 text-sm dark:bg-amber-950">
                    <p class="font-medium">"The webhook's URL (shown once; make a new one if it leaks):"</p>
                    <div class="select-all break-all font-mono text-xs">{u.clone()}</div>
                    <div class="select-all break-all font-mono text-xs text-zinc-500">{format!("curl -X POST --data '{{\"alert\":\"…\"}}' {u}")}</div>
                </div>
            })}
            <div class="divide-y divide-zinc-200 dark:divide-zinc-800">
                {move || list.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(v) if v.is_empty() => view! { <p class="py-2 text-sm text-zinc-500">"No automations."</p> }.into_any(),
                    Ok(v) => v.into_iter().map(|a| {
                        let (id1, id2, id3, id4) = (a.id.clone(), a.id.clone(), a.id.clone(), a.id.clone());
                        let (id5, id6, id7) = (a.id.clone(), a.id.clone(), a.id.clone());
                        let recent = a.recent.clone();
                        let streak = a.disable_after;
                        let enabled = a.enabled;
                        let what = if a.trigger == "schedule" {
                            format!("{} · {}", a.cron, a.next_run_at.map(|t| format!("next {}", when(t))).unwrap_or_else(|| "off".into()))
                        } else {
                            "webhook".to_owned()
                        };
                        let last = a.last_run_id.clone();
                        view! {
                            <div class=format!("space-y-1 py-2 text-sm {}", if enabled { "" } else { "opacity-60" })>
                                <div class="flex flex-wrap items-center gap-2">
                                    <input type="checkbox" title="On" prop:checked=enabled on:change=move |ev| {
                                        let (id, on) = (id1.clone(), event_target_checked(&ev));
                                        spawn_local(async move {
                                            let _ = api::update_automation(id, c6_api::AutomationEdit { enabled: Some(on), ..Default::default() }).await;
                                            list.refetch();
                                        });
                                    } />
                                    <span class="font-medium">{a.name.clone()}</span>
                                    <span class="font-mono text-xs text-zinc-500">{format!("→ {} · {what}", a.template)}</span>
                                    <span class="flex-1"></span>
                                    <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| {
                                        let id = id2.clone();
                                        spawn_local(async move {
                                            match api::run_automation(id).await {
                                                Ok(r) => { list.refetch(); open_run(r.run_id); }
                                                Err(e) => status.set(Some(Err(e))),
                                            }
                                        });
                                    }>"Run now"</button>
                                    {(a.trigger == "webhook").then(|| view! {
                                        <button class=format!("{BUTTON_QUIET} text-xs") title="A new URL; the old one stops working" on:click=move |_| {
                                            let id = id3.clone();
                                            spawn_local(async move {
                                                match api::rotate_automation(id).await {
                                                    Ok(a) => show_url(&a),
                                                    Err(e) => status.set(Some(Err(e))),
                                                }
                                            });
                                        }>"New URL"</button>
                                    })}
                                    <button class=format!("{BUTTON_QUIET} text-xs text-red-600") on:click=move |_| {
                                        let id = id4.clone();
                                        spawn_local(async move {
                                            let _ = api::delete_automation(id).await;
                                            list.refetch();
                                        });
                                    }>"Delete"</button>
                                </div>
                                <div class="flex flex-wrap items-center gap-2 pl-6 text-xs text-zinc-500">
                                    {a.last_run_at.map(|t| format!("last ran {}", when(t)))}
                                    {last.map(|run| {
                                        let r = run.clone();
                                        view! { <button class="underline" on:click=move |_| open_run(r.clone())>{format!("see run:{run}")}</button> }
                                    })}
                                    {a.last_error.clone().map(|e| view! { <span class="text-red-600">{e}</span> })}
                                </div>
                                <div class="flex flex-wrap items-center gap-2 pl-6 text-xs text-zinc-500">
                                    {(!recent.is_empty()).then(|| view! {
                                        <span class="flex items-center gap-0.5" title="Latest runs, oldest to newest">
                                            {recent.iter().rev().map(|o| view! { <span class=format!("h-2 w-2 rounded-full {}", outcome_dot(o)) title=o.clone()></span> }).collect_view()}
                                        </span>
                                        <button class="underline" on:click=move |_| history.update(|h| *h = if h.as_deref() == Some(id5.as_str()) { None } else { Some(id5.clone()) })>
                                            {let id = id6.clone(); move || if history.get().as_deref() == Some(id.as_str()) { "hide history" } else { "history" }}
                                        </button>
                                    })}
                                    <label class="flex items-center gap-1">
                                        "switch off after"
                                        <select class="rounded border border-zinc-300 bg-transparent px-1 dark:border-zinc-700" prop:value=streak.to_string() on:change=move |ev| {
                                            let (id, n) = (id7.clone(), event_target_value(&ev).parse().unwrap_or(0));
                                            spawn_local(async move {
                                                let _ = api::update_automation(id, c6_api::AutomationEdit { disable_after: Some(n), ..Default::default() }).await;
                                                list.refetch();
                                            });
                                        }>
                                            {[0u32, 1, 2, 3, 5, 10].into_iter().map(|n| view! { <option value=n.to_string()>{if n == 0 { "never".to_owned() } else { format!("{n} failed runs") }}</option> }).collect_view()}
                                        </select>
                                    </label>
                                </div>
                                {let id = a.id.clone(); move || (history.get().as_deref() == Some(id.as_str())).then(|| view! { <RunHistory id=id.clone() /> })}
                            </div>
                        }
                    }).collect_view().into_any(),
                })}
            </div>
            <div class="grid gap-2 sm:grid-cols-2">
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Name"</span>
                    <input class=format!("{INPUT} py-1 text-sm") placeholder="e.g. Morning health check" prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
                </label>
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Template or workflow"</span>
                    <select class=format!("{INPUT} py-1 text-sm") prop:value=move || template.get() on:change=move |ev| template.set(event_target_value(&ev))>
                        <option value="">"Choose…"</option>
                        {move || templates.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|t| view! { <option value=t.name.clone()>{t.name.clone()}</option> }).collect_view()}
                        {move || workflow_list.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|w| view! { <option value=w.name.clone()>{format!("{} (workflow)", w.name)}</option> }).collect_view()}
                    </select>
                </label>
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Trigger"</span>
                    <select class=format!("{INPUT} py-1 text-sm") prop:value=move || trigger.get() on:change=move |ev| trigger.set(event_target_value(&ev))>
                        <option value="schedule">"On a schedule"</option>
                        <option value="webhook">"When a webhook is called"</option>
                        <option value="memex">"When mail arrives (memex)"</option>
                    </select>
                </label>
                <label class="space-y-0.5" class:hidden=move || trigger.get() != "schedule">
                    <span class="text-xs text-zinc-500">"Schedule (cron: minute hour day month weekday)"</span>
                    <div class="flex gap-1">
                        <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="0 8 * * mon-fri" prop:value=move || cron.get() on:input=move |ev| cron.set(event_target_value(&ev)) />
                        <select class=format!("{INPUT} w-auto! py-1 text-xs") on:change=move |ev| { let v = event_target_value(&ev); if !v.is_empty() { cron.set(v); } }>
                            <option value="">"Presets…"</option>
                            {SCHEDULE_PRESETS.iter().map(|(l, c)| view! { <option value=*c>{*l}</option> }).collect_view()}
                        </select>
                    </div>
                </label>
                <label class="space-y-0.5">
                    <span class="text-xs text-zinc-500">"Switch it off after"</span>
                    <select class=format!("{INPUT} py-1 text-sm") prop:value=move || disable_after.get().to_string()
                        on:change=move |ev| disable_after.set(event_target_value(&ev).parse().unwrap_or(0))>
                        {[0u32, 1, 2, 3, 5, 10].into_iter().map(|n| view! { <option value=n.to_string()>{if n == 0 { "never".to_owned() } else { format!("{n} failed runs in a row") }}</option> }).collect_view()}
                    </select>
                </label>
                <label class="space-y-0.5 sm:col-span-2" class:hidden=move || trigger.get() != "memex">
                    <span class="text-xs text-zinc-500">"Which mail (the message becomes the template's {{input}})"</span>
                    <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="mail.received from:@acme.com subject:invoice (from:, to:, subject:, account:)"
                        prop:value=move || cron.get() on:input=move |ev| cron.set(event_target_value(&ev)) />
                </label>
                <label class="space-y-0.5 sm:col-span-2" class:hidden=move || trigger.get() != "schedule">
                    <span class="text-xs text-zinc-500">"Input (the template's {{input}})"</span>
                    <input class=format!("{INPUT} py-1 text-sm") placeholder="optional" prop:value=move || input.get() on:input=move |ev| input.set(event_target_value(&ev)) />
                </label>
            </div>
            <div class="flex items-center justify-end gap-3">
                {move || status.get().map(|r| match r {
                    Ok(m) => view! { <span class="text-sm text-emerald-600">{m}</span> }.into_any(),
                    Err(e) => view! { <span class="text-sm text-red-600">{e}</span> }.into_any(),
                })}
                <button class=BUTTON on:click=create>"Add automation"</button>
            </div>
        </section>
    }
}

fn outcome_dot(outcome: &str) -> &'static str {
    match outcome {
        "ok" => "bg-emerald-500",
        "failed" => "bg-red-500",
        "stopped" => "bg-amber-500",
        _ => "bg-sky-400 animate-pulse",
    }
}

/// An automation's latest runs: when, how, what they cost, their sessions.
#[component]
fn RunHistory(id: String) -> impl IntoView {
    let runs = LocalResource::new(move || api::automation_runs(id.clone()));
    let ws = crate::workspace::use_workspace();
    view! {
        <div class="ml-6 mt-1 rounded-md border border-zinc-200 text-xs dark:border-zinc-800">
            {move || runs.get().map(|r| match r {
                Err(e) => view! { <div class="p-2"><ErrorBox message=e /></div> }.into_any(),
                Ok(v) if v.is_empty() => view! { <p class="p-2 text-zinc-500">"No runs yet."</p> }.into_any(),
                Ok(v) => v.into_iter().map(|run| {
                    let took = run.finished_at.map(|f| format!(" · {}s", (f - run.started_at) / 1000)).unwrap_or_default();
                    let cost = if run.calls == 0 { String::new() } else { format!(" · {} calls · ${:.2}", run.calls, run.cost) };
                    view! {
                        <div class="space-y-0.5 border-t border-zinc-100 px-2 py-1.5 first:border-t-0 dark:border-zinc-800/60">
                            <div class="flex flex-wrap items-center gap-2">
                                <span class=format!("h-2 w-2 rounded-full {}", outcome_dot(&run.outcome))></span>
                                <span>{when(run.started_at)}</span>
                                <span class="text-zinc-500">{format!("{} · {}{took}{cost}", run.trigger, run.outcome)}</span>
                            </div>
                            {run.error.clone().map(|e| view! { <p class="pl-4 text-red-600">{e}</p> })}
                            <div class="flex flex-wrap gap-x-3 pl-4">
                                {run.sessions.into_iter().map(|s| {
                                    let sid = s.id.clone();
                                    view! {
                                        <button class="flex items-center gap-1 hover:underline" on:click=move |_| ws.open(None, crate::workspace::View::session(sid.clone()), crate::workspace::How::Beside)>
                                            <span class=format!("h-1.5 w-1.5 rounded-full {}", outcome_dot(&s.outcome))></span>
                                            {if s.title.is_empty() { s.id.clone() } else { s.title.clone() }}
                                        </button>
                                    }
                                }).collect_view()}
                            </div>
                        </div>
                    }
                }).collect_view().into_any(),
            })}
        </div>
    }
}

/// Settings: memory notes, by scope.
#[component]
fn MemorySetting() -> impl IntoView {
    let list = LocalResource::new(api::memories);
    let (scope, text) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let editing = RwSignal::new(None::<String>);
    let status = RwSignal::new(None::<Result<&'static str, String>>);
    let save = move |_| {
        let m = c6_api::Memory {
            id: editing.get_untracked().unwrap_or_default(),
            scope: scope.get_untracked(),
            text: text.get_untracked(),
            ..Default::default()
        };
        spawn_local(async move {
            match api::save_memory(m).await {
                Ok(_) => {
                    status.set(Some(Ok("Saved.")));
                    text.set(String::new());
                    editing.set(None);
                    list.refetch();
                }
                Err(e) => status.set(Some(Err(e))),
            }
        });
    };
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <div>
                <h2 class="font-medium">"Memory"</h2>
                <p class="text-sm text-zinc-500">
                    "Notes kept across sessions. Each has a scope, one label (host:web1, repo:github.com/acme/app, client:acme) or none for every session; new sessions get the notes for their labels in their instructions. Agents add and correct notes as they learn (remember / forget); you can edit them here."
                </p>
            </div>
            <div class="divide-y divide-zinc-200 dark:divide-zinc-800">
                {move || list.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(v) if v.is_empty() => view! { <p class="py-2 text-sm text-zinc-500">"No notes yet."</p> }.into_any(),
                    Ok(v) => v.into_iter().map(|m| {
                        let (edit, del) = (m.clone(), m.id.clone());
                        view! {
                            <div class="flex items-start gap-2 py-2 text-sm">
                                {if m.scope.is_empty() {
                                    view! { <span class="shrink-0 rounded bg-zinc-100 px-1.5 py-0.5 text-[11px] text-zinc-500 dark:bg-zinc-800">"all"</span> }.into_any()
                                } else {
                                    view! { <crate::labels::LabelChip label=m.scope.clone() auto=true stay=true /> }.into_any()
                                }}
                                <span class="min-w-0 flex-1 whitespace-pre-wrap">{m.text.clone()}</span>
                                {m.session_id.clone().map(|s| view! { <crate::workspace::Go to=crate::workspace::View::session(s) class="shrink-0 text-xs text-zinc-500 underline">"by an agent"</crate::workspace::Go> })}
                                <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| {
                                    scope.set(edit.scope.clone());
                                    text.set(edit.text.clone());
                                    editing.set(Some(edit.id.clone()));
                                }>"Edit"</button>
                                <button class=format!("{BUTTON_QUIET} text-xs text-red-600") on:click=move |_| {
                                    let id = del.clone();
                                    spawn_local(async move {
                                        let _ = api::delete_memory(id).await;
                                        list.refetch();
                                    });
                                }>"Delete"</button>
                            </div>
                        }
                    }).collect_view().into_any(),
                })}
            </div>
            <div class="grid gap-2 sm:grid-cols-[14rem_1fr]">
                <input class=format!("{INPUT} py-1 font-mono text-xs") placeholder="scope: a label, or empty for all" prop:value=move || scope.get()
                    on:input=move |ev| scope.set(event_target_value(&ev)) />
                <textarea class=format!("{INPUT} min-h-10 text-sm") rows="2" placeholder="e.g. web1 runs Ubuntu 24.04; nginx is managed by docker compose in /srv/web"
                    prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))></textarea>
            </div>
            <div class="flex items-center justify-end gap-3">
                {move || status.get().map(|r| match r {
                    Ok(m) => view! { <span class="text-sm text-emerald-600">{m}</span> }.into_any(),
                    Err(e) => view! { <span class="text-sm text-red-600">{e}</span> }.into_any(),
                })}
                {move || editing.get().is_some().then(|| view! { <button class=BUTTON_QUIET on:click=move |_| { editing.set(None); text.set(String::new()); }>"Cancel"</button> })}
                <button class=BUTTON on:click=save>{move || if editing.get().is_some() { "Save note" } else { "Add note" }}</button>
            </div>
        </section>
    }
}

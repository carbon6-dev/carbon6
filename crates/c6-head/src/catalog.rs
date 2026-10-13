//! The operation catalog: everything you can do in carbon6, described for
//! the assistant (and the CLI). Each HTTP route the UI uses, and each memex
//! method it calls, is an operation with a stable name, what it does, its
//! input schema, its effect, and where its choices come from. A test keeps
//! the catalog and the router in step (docs/assistant.md).

use std::sync::OnceLock;

use c6_api as api;
use serde::Serialize;
use serde_json::{Value, json};

use crate::api::{
    After, FindQuery, ForSession, IdQuery, MemexSearchQuery, RunsQuery, TestConnection, TodoQuery, TrailEdit, TrailStepIn, UpdateSession,
    UsageQuery, WorkflowRunsQuery,
};

/// What an operation does to carbon6.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Only reads.
    Read,
    /// Creates or changes something.
    Write,
    /// Deletes, revokes, sends, or replaces a list with fewer entries: the
    /// assistant asks first.
    Destructive,
    /// Its result holds a secret (a token, a code): shown to you, never to
    /// the model.
    Secret,
}

/// How an operation is called.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Via {
    Http {
        method: &'static str,
        path: &'static str,
    },
    /// A memex method (through `POST /api/v1/memex/rpc`).
    Memex {
        method: &'static str,
    },
}

/// Where an operation's input comes from.
#[derive(Clone, Copy)]
pub enum Input {
    None,
    /// A JSON body, under `body`.
    Body(fn() -> Value),
    /// Query parameters, at the top level.
    Query(fn() -> Value),
    /// Memex parameters, at the top level (a hand-written schema).
    Params(&'static str),
    /// Memex parameters, at the top level (a type's schema).
    Typed(fn() -> Value),
}

#[derive(Clone)]
pub struct Op {
    pub name: &'static str,
    pub area: &'static str,
    pub summary: &'static str,
    pub via: Via,
    pub effect: Effect,
    pub input: Input,
    /// Output fields (top level) that hold a secret.
    pub secret: &'static [&'static str],
    /// Inputs whose valid values another operation lists: (field, op).
    pub choices: &'static [(&'static str, &'static str)],
    /// For a list setting replaced whole: the op that reads it now (to
    /// tell whether the change removes entries).
    pub replaces: Option<&'static str>,
}

fn schema<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

fn any_object() -> Value {
    json!({"type": "object"})
}

fn mcp_servers() -> Value {
    json!({"type": "array", "description": "MCP servers: each {name, command, args, env, url, headers, enabled, applies_to}; read settings.mcp_servers.get first and send the whole list back", "items": {"type": "object"}})
}

macro_rules! op {
    ($name:literal, $area:literal, $method:literal $path:literal, $effect:ident, $input:expr, $summary:literal $(, secret = $secret:expr)? $(, choices = $choices:expr)? $(, replaces = $replaces:literal)?) => {
        Op {
            name: $name,
            area: $area,
            summary: $summary,
            via: Via::Http { method: $method, path: $path },
            effect: Effect::$effect,
            input: $input,
            secret: op!(@or $($secret)?; &[]),
            choices: op!(@or $($choices)?; &[]),
            replaces: op!(@opt $($replaces)?),
        }
    };
    (memex $name:literal, $area:literal, $method:literal, $effect:ident, $params:literal, $summary:literal) => {
        Op {
            name: $name,
            area: $area,
            summary: $summary,
            via: Via::Memex { method: $method },
            effect: Effect::$effect,
            input: Input::Params($params),
            secret: &[],
            choices: &[],
            replaces: None,
        }
    };
    (memex_typed $name:literal, $area:literal, $method:literal, $effect:ident, $ty:ty, $summary:literal) => {
        Op {
            name: $name,
            area: $area,
            summary: $summary,
            via: Via::Memex { method: $method },
            effect: Effect::$effect,
            input: Input::Typed(schema::<$ty>),
            secret: &[],
            choices: &[],
            replaces: None,
        }
    };
    (@or $v:expr; $d:expr) => { $v };
    (@or ; $d:expr) => { $d };
    (@opt $v:literal) => { Some($v) };
    (@opt) => { None };
}

/// A list setting: get it, and replace it whole.
macro_rules! list_setting {
    ($get:literal, $put:literal, $area:literal, $path:literal, $ty:ty, $what:literal) => {
        [
            op!($get, $area, "GET" $path, Read, Input::None, $what),
            op!($put, $area, "PUT" $path, Write, Input::Body(schema::<Vec<$ty>>),
                "Replaces this whole list. Read it first and send every entry you want to keep.", replaces = $get),
        ]
    };
}

const AREAS: &[(&str, &str)] = &[
    ("assistant", "This assistant: what it can do and how."),
    ("sessions", "Agent sessions: start, message, steer, label, search, export."),
    ("fleet", "Workers (machines that run sessions) and the memex node: enrol, revoke, files, git, terminals."),
    ("connections", "Model providers: API keys, ChatGPT sign-in, local servers; their models."),
    ("routing", "Which model a session uses (routes by label), and policies (ask/deny for tools)."),
    ("automation", "Automations (cron, webhooks, memex events), templates, workflows and their runs."),
    ("knowledge", "Memory notes, and the memex: search, notes, trails, documents, settings, folders, review, commitments, graph."),
    ("mail", "Mail, calendar and contacts through the memex's Google accounts."),
    ("photos", "Photos and videos in the memex."),
    ("notifications", "Notification channels, web push devices, the public URL for links."),
    ("usage", "Usage, prices and budgets."),
    ("hosts", "The SSH host inventory."),
    ("settings", "Tools and context for sessions: MCP servers, skills, commands, agents, hooks, secrets, web search."),
    ("account", "Sign-in: passkeys, signed-in browsers, API tokens, one-time codes."),
    ("system", "carbon6 itself: upgrades of its databases, and the backups made before each."),
    ("todos", "Action items: things for the user to settle when they want to (facts to confirm, disputes, open questions)."),
];

pub fn areas() -> &'static [(&'static str, &'static str)] {
    AREAS
}

pub fn ops() -> &'static [Op] {
    static OPS: OnceLock<Vec<Op>> = OnceLock::new();
    OPS.get_or_init(build)
}

pub fn find(name: &str) -> Option<&'static Op> {
    ops().iter().find(|o| o.name == name)
}

#[rustfmt::skip]
fn build() -> Vec<Op> {
    let mut v = vec![
        // Fleet.
        op!("workers.list", "fleet", "GET" "/api/v1/workers", Read, Input::None, "Workers and the memex node: name, host, whether connected, sessions running, capabilities."),
        op!("workers.enrollments.create", "fleet", "POST" "/api/v1/workers/enrollments", Secret, Input::Body(schema::<api::CreateEnrollment>), "Creates a one-time enrollment token for a new worker (c6w enroll <token>) or the memex (c6m enroll <token>); valid for an hour.", secret = &["token"]),
        op!("workers.revoke", "fleet", "POST" "/api/v1/workers/{id}/revoke", Destructive, Input::None, "Revokes a worker: it is disconnected and can no longer attach; its sessions stop.", choices = &[("id", "workers.list")]),
        op!("workers.delete", "fleet", "DELETE" "/api/v1/workers/{id}", Destructive, Input::None, "Deletes a revoked worker and its records.", choices = &[("id", "workers.list")]),
        op!("workers.fs.list", "fleet", "GET" "/api/v1/workers/{id}/fs/list", Read, Input::Query(any_object), "Lists a folder on a worker: query path (absolute), hidden (bool).", choices = &[("id", "workers.list")]),
        op!("workers.fs.find", "fleet", "GET" "/api/v1/workers/{id}/fs/find", Read, Input::Query(any_object), "Finds files on a worker: query root, q.", choices = &[("id", "workers.list")]),
        op!("workers.fs.read", "fleet", "GET" "/api/v1/workers/{id}/fs/read", Read, Input::Query(any_object), "Reads a text file on a worker: query path.", choices = &[("id", "workers.list")]),
        op!("workers.fs.mkdir", "fleet", "POST" "/api/v1/workers/{id}/fs/mkdir", Write, Input::Body(any_object), "Makes a folder on a worker: body {path}.", choices = &[("id", "workers.list")]),
        op!("workers.git.status", "fleet", "GET" "/api/v1/workers/{id}/git/status", Read, Input::Query(any_object), "Git status of a repository on a worker: query path.", choices = &[("id", "workers.list")]),
        op!("workers.git.diff", "fleet", "GET" "/api/v1/workers/{id}/git/diff", Read, Input::Query(any_object), "Git diff in a repository on a worker: query path, file (optional), staged (bool).", choices = &[("id", "workers.list")]),
        op!("workers.git.log", "fleet", "GET" "/api/v1/workers/{id}/git/log", Read, Input::Query(any_object), "Recent commits in a repository on a worker: query path, limit.", choices = &[("id", "workers.list")]),
        op!("workers.terminals.list", "fleet", "GET" "/api/v1/workers/{id}/terminals", Read, Input::None, "Open terminals on a worker.", choices = &[("id", "workers.list")]),
        op!("workers.terminals.open", "fleet", "POST" "/api/v1/workers/{id}/terminals", Write, Input::Body(any_object), "Opens a terminal on a worker (you use it in the UI): body {cwd, cols, rows}.", choices = &[("id", "workers.list")]),
        op!("workers.terminals.close", "fleet", "DELETE" "/api/v1/workers/{id}/terminals/{tid}", Destructive, Input::None, "Closes a terminal on a worker (its process ends).", choices = &[("id", "workers.list"), ("tid", "workers.terminals.list")]),

        // Connections.
        op!("connections.list", "connections", "GET" "/api/v1/connections", Read, Input::None, "Model connections: name, kind (openai, openai_compat, anthropic, chatgpt, claude_code), base URL, whether a key is set."),
        op!("connections.create", "connections", "POST" "/api/v1/connections", Write, Input::Body(schema::<api::CreateConnection>), "Adds a connection (API key, or a local OpenAI-compatible server such as Ollama). The key is stored encrypted. For a ChatGPT plan use connections.chatgpt.start."),
        op!("connections.test", "connections", "POST" "/api/v1/connections/test", Read, Input::Body(schema::<TestConnection>), "Tests a connection (a saved one by id, or the details given) and lists its models."),
        op!("connections.delete", "connections", "DELETE" "/api/v1/connections/{id}", Destructive, Input::None, "Deletes a connection. Sessions using it can no longer call their model.", choices = &[("id", "connections.list")]),
        op!("connections.models", "connections", "GET" "/api/v1/connections/{id}/models", Read, Input::None, "The models a connection offers.", choices = &[("id", "connections.list")]),
        op!("connections.chatgpt.start", "connections", "POST" "/api/v1/connections/chatgpt/start", Write, Input::Body(schema::<api::StartChatGptSignIn>), "Starts signing in with ChatGPT (a ChatGPT plan): returns the address for you to open; then connections.chatgpt.complete with where the browser ended up (unless it was this machine)."),
        op!("connections.chatgpt.complete", "connections", "POST" "/api/v1/connections/chatgpt/complete", Write, Input::Body(schema::<api::CompleteChatGptSignIn>), "Finishes signing in with ChatGPT with the address the browser ended on."),

        // Sessions.
        op!("sessions.list", "sessions", "GET" "/api/v1/sessions", Read, Input::None, "Sessions (not archived): title, worker, model, status, labels, whether waiting for approval."),
        op!("sessions.create", "sessions", "POST" "/api/v1/sessions", Write, Input::Body(schema::<api::CreateSession>), "Starts an agent session on a worker, with a first message (prompt), a folder (cwd), model and labels.", choices = &[("worker_id", "workers.list"), ("connection_id", "connections.list")]),
        op!("sessions.get", "sessions", "GET" "/api/v1/sessions/{id}", Read, Input::None, "A session's details.", choices = &[("id", "sessions.list")]),
        op!("sessions.update", "sessions", "PATCH" "/api/v1/sessions/{id}", Write, Input::Body(schema::<UpdateSession>), "Renames or archives (or unarchives) a session.", choices = &[("id", "sessions.list")]),
        op!("sessions.events", "sessions", "GET" "/api/v1/sessions/{id}/events", Read, Input::Query(schema::<After>), "A session's log: messages, tool calls and results, after event seq `after`.", choices = &[("id", "sessions.list")]),
        op!("sessions.children", "sessions", "GET" "/api/v1/sessions/{id}/children", Read, Input::None, "A session's subagents and side chats.", choices = &[("id", "sessions.list")]),
        op!("sessions.side_chat", "sessions", "POST" "/api/v1/sessions/{id}/side-chats", Write, Input::Body(schema::<api::CreateSideChat>), "Branches a side chat off a session at an event.", choices = &[("id", "sessions.list")]),
        op!("sessions.send", "sessions", "POST" "/api/v1/sessions/{id}/messages", Write, Input::Body(schema::<api::SendMessage>), "Sends a message to a session (it runs next), optionally switching its model.", choices = &[("id", "sessions.list")]),
        op!("sessions.interrupt", "sessions", "POST" "/api/v1/sessions/{id}/interrupt", Write, Input::None, "Stops a session's running turn.", choices = &[("id", "sessions.list")]),
        op!("sessions.rewind", "sessions", "POST" "/api/v1/sessions/{id}/rewind", Destructive, Input::Body(schema::<api::RewindRequest>), "Rewinds a session to before one of your messages (later events are discarded; files optionally restored).", choices = &[("id", "sessions.list")]),
        op!("sessions.labels.set", "sessions", "PUT" "/api/v1/sessions/{id}/labels", Write, Input::Body(schema::<api::SetLabels>), "Sets a session's labels.", choices = &[("id", "sessions.list")]),
        op!("sessions.config", "sessions", "GET" "/api/v1/sessions/{id}/config", Read, Input::None, "What applies to a session: labels, MCP servers, skills, hooks, policies, memory, secrets, model route.", choices = &[("id", "sessions.list")]),
        op!("sessions.tasks.stop", "sessions", "POST" "/api/v1/sessions/{id}/tasks/{task}/stop", Destructive, Input::None, "Stops a session's background task.", choices = &[("id", "sessions.list")]),
        op!("sessions.approvals.decide", "sessions", "POST" "/api/v1/sessions/{id}/approvals/{call_id}", Write, Input::Body(schema::<api::ApprovalDecision>), "Allows or denies a tool call waiting for approval.", choices = &[("id", "sessions.list")]),
        op!("sessions.queue.dismiss", "sessions", "DELETE" "/api/v1/sessions/{id}/queue/rejected", Write, Input::None, "Dismisses a session's rejected queued messages.", choices = &[("id", "sessions.list")]),
        op!("sessions.find", "sessions", "GET" "/api/v1/sessions/{id}/matches", Read, Input::Query(schema::<FindQuery>), "Finds text in one session (and its side chats).", choices = &[("id", "sessions.list")]),
        op!("sessions.search", "sessions", "GET" "/api/v1/search", Read, Input::Query(schema::<api::SearchQuery>), "Searches every session's messages, tool calls and outputs."),
        op!("commands.list", "settings", "GET" "/api/v1/commands", Read, Input::Query(schema::<ForSession>), "Slash commands available (for a session, by its labels)."),

        // Settings that shape sessions.
        op!("settings.mcp_servers.get", "settings", "GET" "/api/v1/settings/mcp-servers", Read, Input::None, "MCP servers given to sessions (by label)."),
        op!("settings.mcp_servers.put", "settings", "PUT" "/api/v1/settings/mcp-servers", Write, Input::Body(mcp_servers), "Replaces the MCP server list. Read it first and send every entry you want to keep.", replaces = "settings.mcp_servers.get"),
    ];
    v.extend(list_setting!("settings.skills.get", "settings.skills.put", "settings", "/api/v1/settings/skills", api::SkillDef, "Skills given to sessions (by label)."));
    v.extend(list_setting!("settings.commands.get", "settings.commands.put", "settings", "/api/v1/settings/commands", api::CommandDef, "Custom slash commands (by label)."));
    v.extend(list_setting!("settings.agents.get", "settings.agents.put", "settings", "/api/v1/settings/agents", api::AgentDef, "Named agents for the Task tool (subagents)."));
    v.extend(list_setting!("settings.hooks.get", "settings.hooks.put", "settings", "/api/v1/settings/hooks", api::Hook, "Hooks: commands run around tool use and prompts (by label)."));
    v.extend(list_setting!("settings.templates.get", "settings.templates.put", "automation", "/api/v1/settings/templates", api::Template, "Session templates (a prompt, folder, model and labels to start from; fleet runs)."));
    v.extend(list_setting!("settings.policies.get", "settings.policies.put", "routing", "/api/v1/settings/policies", api::Policy, "Policies: ask or deny tool calls matching a tool and input pattern, for sessions with given labels."));
    v.extend(list_setting!("settings.routes.get", "settings.routes.put", "routing", "/api/v1/settings/routes", api::Route, "Model routes: which connection and model sessions use, by label (and their subagents' model)."));
    v.extend(list_setting!("settings.channels.get", "settings.channels.put", "notifications", "/api/v1/settings/channels", api::Channel, "Notification channels (push, ntfy, Slack, Discord, webhook, email) and what goes to each."));
    v.extend(list_setting!("settings.prices.get", "settings.prices.put", "usage", "/api/v1/settings/prices", api::Price, "Model prices, for cost estimates."));
    v.extend(list_setting!("settings.budgets.get", "settings.budgets.put", "usage", "/api/v1/settings/budgets", api::Budget, "Budgets: spending limits per period and label, warn or stop."));
    v.extend(list_setting!("settings.hosts.get", "settings.hosts.put", "hosts", "/api/v1/settings/hosts", api::Host, "The SSH host inventory: name, target, worker to reach it through, labels."));
    v.extend(list_setting!("settings.workflows.get", "settings.workflows.put", "automation", "/api/v1/settings/workflows", api::Workflow, "Workflows: steps of sessions, approvals and conditions."));
    v.extend(list_setting!("settings.memex_scopes.get", "settings.memex_scopes.put", "knowledge", "/api/v1/settings/memex_scopes", api::MemexScope, "Memex scope rules: which kinds of items (email, photo…) sessions with given labels may see."));
    v.extend(list_setting!("settings.memex_folders.get", "settings.memex_folders.put", "knowledge", "/api/v1/settings/memex_folders", api::MemexFolder, "Folders on the memex's machine it reads (documents, papers, photos, videos)."));
    v.extend([
        op!("settings.secrets.list", "settings", "GET" "/api/v1/settings/secrets", Read, Input::None, "Secrets sessions can use as environment variables (names and labels; never values)."),
        op!("settings.secrets.put", "settings", "PUT" "/api/v1/settings/secrets/{name}", Write, Input::Body(schema::<api::PutSecret>), "Sets a secret's value (stored encrypted) and which sessions get it. Ask the user for the value with c6_ask (secret field); never repeat it."),
        op!("settings.secrets.delete", "settings", "DELETE" "/api/v1/settings/secrets/{name}", Destructive, Input::None, "Deletes a secret.", choices = &[("name", "settings.secrets.list")]),
        op!("settings.web_search.get", "settings", "GET" "/api/v1/settings/web-search", Read, Input::None, "The web search provider sessions use."),
        op!("settings.web_search.put", "settings", "PUT" "/api/v1/settings/web-search", Write, Input::Body(schema::<api::WebSearchSetting>), "Sets the web search provider."),
        op!("settings.public_url.get", "notifications", "GET" "/api/v1/settings/public_url", Read, Input::None, "The address links in notifications point to."),
        op!("settings.public_url.put", "notifications", "PUT" "/api/v1/settings/public_url", Write, Input::Body(schema::<api::PublicUrl>), "Sets the address links in notifications point to."),
        op!("settings.assistant.get", "assistant", "GET" "/api/v1/settings/assistant", Read, Input::None, "Which model the assistant uses (none: routes for the label assistant, else the default)."),
        op!("settings.assistant.put", "assistant", "PUT" "/api/v1/settings/assistant", Write, Input::Body(schema::<api::AssistantSettings>), "Sets the assistant's model.", choices = &[("model.connection", "connections.list")]),
        op!("settings.memex.get", "knowledge", "GET" "/api/v1/settings/memex", Read, Input::None, "Which models the memex uses for each task: embeddings (search by meaning), understanding (summaries, topics, commitments), vision (photo captions) and transcription (videos); also calls a day, brief size, places file, face models, contacts and sending options."),
        op!("settings.memex.put", "knowledge", "PUT" "/api/v1/settings/memex", Write, Input::Body(schema::<api::MemexSettings>), "Replaces the memex settings (read them first and keep what should stay).", choices = &[("understanding.connection", "connections.list"), ("vision.connection", "connections.list"), ("embedding.connection", "connections.list")]),

        // Knowledge: memory.
        op!("memories.list", "knowledge", "GET" "/api/v1/memories", Read, Input::None, "Memory notes sessions start with (by scope)."),
        op!("memories.create", "knowledge", "POST" "/api/v1/memories", Write, Input::Body(schema::<api::Memory>), "Adds a memory note."),
        op!("memories.update", "knowledge", "PUT" "/api/v1/memories/{id}", Write, Input::Body(schema::<api::Memory>), "Changes a memory note.", choices = &[("id", "memories.list")]),
        op!("memories.delete", "knowledge", "DELETE" "/api/v1/memories/{id}", Destructive, Input::None, "Deletes a memory note.", choices = &[("id", "memories.list")]),

        // Automation.
        op!("automations.list", "automation", "GET" "/api/v1/automations", Read, Input::None, "Automations: trigger (cron, webhook, memex event), what they start, last run."),
        op!("automations.create", "automation", "POST" "/api/v1/automations", Write, Input::Body(schema::<api::AutomationEdit>), "Creates an automation."),
        op!("automations.update", "automation", "PATCH" "/api/v1/automations/{id}", Write, Input::Body(schema::<api::AutomationEdit>), "Changes an automation (or turns it on or off).", choices = &[("id", "automations.list")]),
        op!("automations.delete", "automation", "DELETE" "/api/v1/automations/{id}", Destructive, Input::None, "Deletes an automation.", choices = &[("id", "automations.list")]),
        op!("automations.run", "automation", "POST" "/api/v1/automations/{id}/run", Write, Input::None, "Runs an automation now.", choices = &[("id", "automations.list")]),
        op!("automations.runs", "automation", "GET" "/api/v1/automations/{id}/runs", Read, Input::Query(schema::<RunsQuery>), "An automation's recent runs and how they went.", choices = &[("id", "automations.list")]),
        op!("automations.rotate", "automation", "POST" "/api/v1/automations/{id}/rotate", Secret, Input::None, "Gives a webhook automation a new URL (the old one stops working).", secret = &["url", "token"], choices = &[("id", "automations.list")]),
        op!("templates.run", "automation", "POST" "/api/v1/templates/{name}/run", Write, Input::None, "Starts sessions from a template (on each matching worker, for a fleet run).", choices = &[("name", "settings.templates.get")]),
        op!("workflows.run", "automation", "POST" "/api/v1/workflows/{name}/run", Write, Input::Body(schema::<api::RunWorkflow>), "Runs a workflow.", choices = &[("name", "settings.workflows.get")]),
        op!("workflows.runs", "automation", "GET" "/api/v1/workflows/runs", Read, Input::Query(schema::<WorkflowRunsQuery>), "Workflow runs, newest first."),
        op!("workflows.runs.get", "automation", "GET" "/api/v1/workflows/runs/{id}", Read, Input::None, "A workflow run with its steps.", choices = &[("id", "workflows.runs")]),
        op!("workflows.steps.decide", "automation", "POST" "/api/v1/workflows/runs/{id}/steps/{step}/decide", Write, Input::Body(schema::<api::ApprovalDecision>), "Approves or rejects a workflow step waiting for you.", choices = &[("id", "workflows.runs")]),

        // Action items.
        op!("todos.list", "todos", "GET" "/api/v1/todos", Read, Input::Query(schema::<TodoQuery>), "Action items (open by default), most important first."),
        op!("todos.count", "todos", "GET" "/api/v1/todos/count", Read, Input::None, "How many action items are open."),
        op!("todos.get", "todos", "GET" "/api/v1/todos/{id}", Read, Input::None, "An action item with its context.", choices = &[("id", "todos.list")]),
        op!("todos.add", "todos", "POST" "/api/v1/todos", Write, Input::Body(schema::<api::TodoIn>), "Adds an action item for the user (an open question, something to confirm or follow up)."),
        op!("todos.edit", "todos", "PATCH" "/api/v1/todos/{id}", Write, Input::Body(schema::<api::TodoEdit>), "Settles (done, with a resolution), snoozes, dismisses or reopens an action item.", choices = &[("id", "todos.list")]),
        op!("todos.session", "todos", "POST" "/api/v1/todos/{id}/session", Write, Input::None, "Starts a session that helps the user settle an action item, with its context.", choices = &[("id", "todos.list")]),

        // Hosts.
        op!("hosts.list", "hosts", "GET" "/api/v1/hosts", Read, Input::None, "Hosts in the inventory with their last check."),
        op!("hosts.check", "hosts", "POST" "/api/v1/hosts/{name}/check", Read, Input::None, "Checks a host now (reachable over SSH, system details).", choices = &[("name", "hosts.list")]),

        // Usage.
        op!("usage.report", "usage", "GET" "/api/v1/usage", Read, Input::Query(schema::<UsageQuery>), "Usage and estimated cost by day, model and label, for the last `days`, optionally filtered by labels."),

        // Notifications.
        op!("notifications.test", "notifications", "POST" "/api/v1/settings/channels/test", Write, Input::Body(schema::<api::TestChannel>), "Sends a test notification to a channel."),
        op!("push.key", "notifications", "GET" "/api/v1/push/key", Read, Input::None, "The web push public key."),
        op!("push.devices", "notifications", "GET" "/api/v1/push/subscriptions", Read, Input::None, "Devices subscribed to web push."),
        op!("push.subscribe", "notifications", "POST" "/api/v1/push/subscriptions", Write, Input::Body(schema::<api::PushSubscription>), "Subscribes a device to web push (the browser does this)."),
        op!("push.unsubscribe", "notifications", "DELETE" "/api/v1/push/subscriptions/{id}", Destructive, Input::None, "Removes a web push device.", choices = &[("id", "push.devices")]),

        // Memex.
        op!("memex.status", "knowledge", "GET" "/api/v1/memex/status", Read, Input::None, "The memex: connected, items per kind, things, embedding progress, model calls today, photos, review queue."),
        op!("memex.search", "knowledge", "GET" "/api/v1/memex/search", Read, Input::Query(schema::<MemexSearchQuery>), "Searches the memex by keyword and meaning: q, kinds (comma-separated), about (labels), limit."),
        op!("memex.get", "knowledge", "GET" "/api/v1/memex/get", Read, Input::Query(schema::<IdQuery>), "An item in full, or a thing (by id or label) with its notes, items and related things."),
        op!("memex.mentions", "knowledge", "GET" "/api/v1/memex/mentions", Read, Input::Query(schema::<IdQuery>), "Things whose name or label matches q (for @-mentions)."),
        op!("memex.notes.create", "knowledge", "POST" "/api/v1/memex/notes", Write, Input::Body(schema::<api::MemexNoteIn>), "Saves a note to the memex, about labels."),
        op!("memex.items.delete", "knowledge", "DELETE" "/api/v1/memex/items/{id}", Destructive, Input::None, "Deletes an item from the memex (not the original file)."),
        op!("memex.upload", "knowledge", "POST" "/api/v1/memex/upload", Write, Input::Body(schema::<api::MemexUpload>), "Adds a document or paper (base64 data)."),
        op!("memex.scan", "knowledge", "POST" "/api/v1/memex/scan", Write, Input::None, "Scans the memex folders now."),
        op!("memex.trails.list", "knowledge", "GET" "/api/v1/memex/trails", Read, Input::None, "Trails: named paths through items and things."),
        op!("memex.trails.get", "knowledge", "GET" "/api/v1/memex/trails/{id}", Read, Input::None, "A trail with its steps.", choices = &[("id", "memex.trails.list")]),
        op!("memex.trails.append", "knowledge", "POST" "/api/v1/memex/trails", Write, Input::Body(schema::<TrailStepIn>), "Adds a step (an item or thing, with a comment) to a trail, making the trail if new."),
        op!("memex.trails.update", "knowledge", "PATCH" "/api/v1/memex/trails/{id}", Write, Input::Body(schema::<TrailEdit>), "Renames a trail or changes its description.", choices = &[("id", "memex.trails.list")]),
        op!("memex.trails.delete", "knowledge", "DELETE" "/api/v1/memex/trails/{id}", Destructive, Input::None, "Deletes a trail.", choices = &[("id", "memex.trails.list")]),
        op!("memex.trails.remove_step", "knowledge", "DELETE" "/api/v1/memex/trails/{id}/steps/{step}", Destructive, Input::None, "Removes a step from a trail.", choices = &[("id", "memex.trails.list")]),

        // Google accounts.
        op!("google.client.get", "mail", "GET" "/api/v1/google/client", Read, Input::None, "The Google OAuth client carbon6 uses (its id; the secret is never shown)."),
        op!("google.client.put", "mail", "PUT" "/api/v1/google/client", Write, Input::Body(schema::<api::GoogleClientIn>), "Sets the Google OAuth client (from your Google Cloud project). Ask for the secret with c6_ask (secret field)."),
        op!("google.connect", "mail", "POST" "/api/v1/google/connect", Write, Input::None, "Starts adding a Google account: returns the address for you to open and sign in."),
        op!("google.accounts", "mail", "GET" "/api/v1/google/accounts", Read, Input::None, "Google accounts the memex syncs."),
        op!("google.accounts.remove", "mail", "DELETE" "/api/v1/google/accounts/{id}", Destructive, Input::None, "Removes a Google account (its mail, events and contacts stay in the memex).", choices = &[("id", "google.accounts")]),

        // Account.
        op!("auth.status", "account", "GET" "/api/v1/auth/status", Read, Input::None, "Whether sign-in is required and how you are signed in."),
        op!("auth.codes.create", "account", "POST" "/api/v1/auth/codes", Secret, Input::None, "Creates a one-time sign-in code for another device (valid a few minutes).", secret = &["code"]),
        op!("auth.passkeys.list", "account", "GET" "/api/v1/auth/passkeys", Read, Input::None, "Your passkeys."),
        op!("auth.passkeys.rename", "account", "PATCH" "/api/v1/auth/passkeys/{id}", Write, Input::Body(schema::<api::RenamePasskey>), "Renames a passkey.", choices = &[("id", "auth.passkeys.list")]),
        op!("auth.passkeys.delete", "account", "DELETE" "/api/v1/auth/passkeys/{id}", Destructive, Input::None, "Deletes a passkey (it can no longer sign in).", choices = &[("id", "auth.passkeys.list")]),
        op!("auth.sessions.list", "account", "GET" "/api/v1/auth/sessions", Read, Input::None, "Browsers signed in to carbon6."),
        op!("auth.sessions.delete", "account", "DELETE" "/api/v1/auth/sessions/{id}", Destructive, Input::None, "Signs a browser out.", choices = &[("id", "auth.sessions.list")]),
        op!("system.upgrades", "system", "GET" "/api/v1/system/upgrades", Read, Input::None, "Upgrades of carbon6's databases (the head's and the memex's), newest first: versions, what changed, and where the backup is."),
        op!("auth.tokens.list", "account", "GET" "/api/v1/auth/tokens", Read, Input::None, "API tokens (for the c6 CLI and scripts)."),
        op!("auth.tokens.create", "account", "POST" "/api/v1/auth/tokens", Secret, Input::Body(schema::<api::CreateApiToken>), "Creates an API token for the c6 CLI or a script.", secret = &["token"]),
        op!("auth.tokens.delete", "account", "DELETE" "/api/v1/auth/tokens/{id}", Destructive, Input::None, "Deletes an API token (it stops working).", choices = &[("id", "auth.tokens.list")]),

        // The memex, through its methods.
        op!(memex "memex.review.list", "knowledge", "memex.review", Read, r#"{"type":"object","properties":{"limit":{"type":"integer"}}}"#, "Suggestions waiting for you: people who may be the same (merge), faces to name."),
        op!(memex "memex.review.decide", "knowledge", "memex.review_decide", Write, r#"{"type":"object","required":["id","accept"],"properties":{"id":{"type":"integer"},"accept":{"type":"boolean","description":"true merges (or names the face); false dismisses"},"name":{"type":"string","description":"for a face: who it is"}}}"#, "Accepts or dismisses a suggestion."),
        op!(memex_typed "memex.ingest", "knowledge", "memex.ingest", Write, api::MemexIngest, "Adds items of any supported type (mail as message/rfc822, documents as PDF, Word, Markdown, text, HTML): each typed by its MIME type and handled by that type's handler. Per item: created, updated, duplicate or rejected with the reason. Imported items are never synced back."),
        op!(memex "memex.types", "knowledge", "memex.types", Read, r#"{"type":"object"}"#, "The types memex.ingest takes: each handler's MIME types, attribute schema, how it recognizes duplicates, size limit and what it creates."),
        op!(memex "memex.commitments", "knowledge", "memex.commitments", Read, r#"{"type":"object","properties":{"status":{"type":"string","enum":["open","done","dropped"]}}}"#, "Commitments read from mail and sessions: who owes what, by when."),
        op!(memex "memex.commitment.set", "knowledge", "memex.commitment", Write, r#"{"type":"object","required":["id","status"],"properties":{"id":{"type":"integer"},"status":{"type":"string","enum":["open","done","dropped"]}}}"#, "Marks a commitment done, dropped or open."),
        op!(memex "memex.graph", "knowledge", "memex.graph", Read, r#"{"type":"object","required":["id"],"properties":{"id":{"type":"string","description":"a thing's id or label"},"limit":{"type":"integer"}}}"#, "The things around a thing and the links among them."),
        op!(memex "memex.months", "knowledge", "memex.months", Read, r#"{"type":"object","required":["id"],"properties":{"id":{"type":"string"}}}"#, "A thing's items per month (its timeline)."),
        op!(memex "memex.timeline", "knowledge", "memex.timeline", Read, r#"{"type":"object","required":["about"],"properties":{"about":{"type":"string"},"limit":{"type":"integer"}}}"#, "The newest items about a thing."),
        op!(memex "memex.related", "knowledge", "memex.related", Read, r#"{"type":"object","required":["id"],"properties":{"id":{"type":"string"}}}"#, "Things sharing items with a thing."),
        op!(memex "memex.summarize", "knowledge", "memex.summarize", Write, r#"{"type":"object","required":["id"],"properties":{"id":{"type":"string"}}}"#, "Summarizes what the memex knows about a thing (saved as a note)."),
        op!(memex "memex.merge", "knowledge", "memex.merge", Destructive, r#"{"type":"object","required":["from","into"],"properties":{"from":{"type":"string"},"into":{"type":"string"}}}"#, "Merges one thing into another (the first is gone)."),
        op!(memex "knowledge.list", "knowledge", "memex.knowledge.list", Read, r#"{"type":"object","properties":{"about":{"type":"array","items":{"type":"string"},"description":"thing keys, e.g. ci:argo or repo:github.com/you/app; empty: all"},"status":{"type":"string","enum":["reviewed","verified","observed","inferred","disputed","retired"]},"due":{"type":"boolean","description":"only facts due for a re-check"},"include_retired":{"type":"boolean"}}}"#, "Environment knowledge: facts about systems, with how each is known, where it applies, its check and evidence."),
        op!(memex "knowledge.get", "knowledge", "memex.knowledge.get", Read, r#"{"type":"object","required":["ids"],"properties":{"ids":{"type":"array","items":{"type":"string"}}}}"#, "Facts by id."),
        op!(memex "knowledge.page", "knowledge", "memex.knowledge.page", Read, r#"{"type":"object","required":["key"],"properties":{"key":{"type":"string","description":"a system's key, e.g. ci:argo"}}}"#, "A system's page: its summary, facts and procedures."),
        op!(memex "knowledge.record", "knowledge", "memex.knowledge.record", Write, r#"{"type":"object","required":["claim","about"],"properties":{"claim":{"type":"string"},"about":{"type":"array","items":{"type":"string"}},"status":{"type":"string","enum":["observed","inferred"]},"volatility":{"type":"string","enum":["stable","slow"]},"labels":{"type":"array","items":{"type":"string"}},"conditions":{"type":"string"},"negative":{"type":"boolean"},"check":{"type":"object","properties":{"command":{"type":"string"},"on":{"type":"string"},"worker":{"type":"string"}}},"evidence":{"type":"object","properties":{"command":{"type":"string"},"excerpt":{"type":"string"},"source":{"type":"string"},"where":{"type":"string"}}},"supports":{"type":"array","items":{"type":"string"}},"replaces":{"type":"string"},"contradicts":{"type":"string"}}}"#, "Records a fact about the environment (the same claim again adds evidence)."),
        op!(memex "knowledge.set", "knowledge", "memex.knowledge.set", Write, r#"{"type":"object","required":["id"],"properties":{"id":{"type":"string"},"status":{"type":"string","enum":["reviewed","observed","inferred","disputed","retired"]},"claim":{"type":"string","description":"a corrected claim (replaces the fact, reviewed)"},"labels":{"type":"array","items":{"type":"string"}},"conditions":{"type":"string"}}}"#, "Settles a fact: confirm (reviewed), correct, retire, or narrow where it applies."),
        op!(memex "google.status", "mail", "memex.google", Read, r#"{"type":"object"}"#, "Google sync state per account (mail, calendars, contacts)."),
        op!(memex "google.sync", "mail", "memex.google_sync", Write, r#"{"type":"object"}"#, "Syncs Google accounts now."),
        op!(memex "mail.threads", "mail", "memex.mail.threads", Read, r#"{"type":"object","properties":{"account":{"type":"string","description":"an account id or address; empty: all"},"view":{"type":"string","description":"inbox (default), needs_reply, waiting, starred, sent, drafts, all, spam, trash, or a Gmail label"},"before":{"type":"integer","description":"ms, for the next page"},"limit":{"type":"integer"}}}"#, "Mail threads in a view (to search mail, use memex.search with kinds email)."),
        op!(memex "mail.thread", "mail", "memex.mail.thread", Read, r#"{"type":"object","required":["thread_id"],"properties":{"account":{"type":"string"},"thread_id":{"type":"string"}}}"#, "A mail thread with its messages."),
        op!(memex "mail.labels", "mail", "memex.mail.labels", Read, r#"{"type":"object","properties":{"account":{"type":"string"}}}"#, "Gmail labels with counts."),
        op!(memex "mail.modify", "mail", "memex.mail.modify", Write, r#"{"type":"object","required":["thread_id"],"properties":{"account":{"type":"string"},"thread_id":{"type":"string"},"add":{"type":"array","items":{"type":"string"}},"remove":{"type":"array","items":{"type":"string"}}}}"#, "Archives, stars, marks read, labels or trashes a thread (label changes)."),
        op!(memex_typed "mail.draft", "mail", "memex.mail.draft", Write, api::MailDraft, "Saves a draft (in Gmail)."),
        op!(memex_typed "mail.send", "mail", "memex.mail.send", Destructive, api::MailDraft, "Sends mail (after a short undo window)."),
        op!(memex "mail.outgoing", "mail", "memex.mail.outgoing", Read, r#"{"type":"object"}"#, "Mail waiting to be sent."),
        op!(memex "mail.cancel", "mail", "memex.mail.cancel", Write, r#"{"type":"object","required":["id"],"properties":{"id":{"type":"string","description":"from mail.outgoing"}}}"#, "Cancels mail waiting to be sent."),
        op!(memex "calendar.calendars", "mail", "memex.calendar.calendars", Read, r#"{"type":"object"}"#, "Calendars of each account, and which are shown."),
        op!(memex "calendar.select", "mail", "memex.calendar.select", Write, r#"{"type":"object","required":["account","id"],"properties":{"account":{"type":"string"},"id":{"type":"string","description":"the calendar's id"},"on":{"type":"boolean"}}}"#, "Shows or hides a calendar."),
        op!(memex "calendar.events", "mail", "memex.calendar.events", Read, r#"{"type":"object","required":["from","to"],"properties":{"from":{"type":"integer","description":"ms since the epoch"},"to":{"type":"integer","description":"ms since the epoch"}}}"#, "Calendar events between two times."),
        op!(memex "calendar.busy", "mail", "memex.calendar.busy", Read, r#"{"type":"object","required":["from","to"],"properties":{"from":{"type":"integer","description":"ms"},"to":{"type":"integer","description":"ms"}}}"#, "Busy times between two times."),
        op!(memex_typed "calendar.create", "mail", "memex.calendar.create", Write, api::EventEdit, "Creates an event (inviting attendees, if any)."),
        op!(memex_typed "calendar.update", "mail", "memex.calendar.update", Write, api::EventEdit, "Changes an event (this one, this and following, or all)."),
        op!(memex_typed "calendar.delete", "mail", "memex.calendar.delete", Destructive, api::EventEdit, "Deletes an event (cancelling it for attendees)."),
        op!(memex "calendar.rsvp", "mail", "memex.calendar.rsvp", Write, r#"{"type":"object","required":["account","calendar_id","event_id","response"],"properties":{"account":{"type":"string"},"calendar_id":{"type":"string"},"event_id":{"type":"string"},"instance_start":{"type":"integer","description":"for one occurrence of a recurring event: its start (ms)"},"response":{"type":"string","enum":["accepted","declined","tentative"]}}}"#, "Answers an invitation."),
        op!(memex "contacts.list", "mail", "memex.contacts.list", Read, r#"{"type":"object","properties":{"q":{"type":"string"}}}"#, "Contacts."),
        op!(memex "contacts.candidates", "mail", "memex.contacts.candidates", Read, r#"{"type":"object"}"#, "People you correspond with who are not in your contacts yet."),
        op!(memex "contacts.create", "mail", "memex.contacts.create", Write, r#"{"type":"object","required":["email"],"properties":{"account":{"type":"string"},"email":{"type":"string"},"name":{"type":"string"}}}"#, "Adds a contact (in Google Contacts, in the carbon6 group)."),
        op!(memex "photos.list", "photos", "memex.media.list", Read, r#"{"type":"object","properties":{"q":{"type":"string","description":"a description"},"about":{"type":"string","description":"a person, place or event (id or label)"},"before":{"type":"integer"},"limit":{"type":"integer"}}}"#, "Photos and videos, newest first, or matching a description, person, place or event."),
        op!(memex "photos.get", "photos", "memex.media.get", Read, r#"{"type":"object","required":["id"],"properties":{"id":{"type":"string"}}}"#, "A photo or video: caption, date, place, faces, things."),
        op!(memex "photos.facets", "photos", "memex.media.facets", Read, r#"{"type":"object"}"#, "People, places and events in the photos, most frequent first."),
    ]);
    v
}

/// Routes the assistant does not call, and why (the parity test allows
/// these).
pub const NOT_OPERATIONS: &[(&str, &str, &str)] = &[
    ("GET", "/api/v1/live", "a live WebSocket for the UI"),
    ("GET", "/api/v1/sessions/{id}/stream", "a live WebSocket for the UI"),
    ("GET", "/api/v1/workers/{id}/terminals/{tid}/attach", "a terminal WebSocket"),
    ("GET", "/api/v1/sessions/{id}/attachments/{seq}/{index}", "a file download"),
    ("GET", "/api/v1/sessions/{id}/outputs/{seq}", "a file download"),
    ("GET", "/api/v1/sessions/{id}/export", "a file download"),
    ("GET", "/api/v1/memex/items/{id}/content", "a file download"),
    ("GET", "/api/v1/memex/media/{id}/thumb", "an image"),
    ("GET", "/api/v1/memex/face/{id}", "an image"),
    ("GET", "/api/v1/mail/attachment", "a file download"),
    ("POST", "/api/v1/memex/rpc", "the memex's methods are operations of their own"),
    ("POST", "/api/v1/webhooks/{id}/{token}", "called by other systems, with its own token"),
    ("GET", "/callback", "the ChatGPT sign-in redirect"),
    ("GET", "/auth/google/callback", "the Google sign-in redirect"),
    ("GET", "/ca.pem", "a certificate download"),
    ("GET", "/ca.crt", "a certificate download"),
    ("GET", "/ca.mobileconfig", "a certificate download"),
    ("GET", "/approve/{token}", "a signed approval page"),
    ("POST", "/approve/{token}", "a signed approval page"),
    ("POST", "/api/v1/auth/link", "signing in a browser"),
    ("POST", "/api/v1/auth/code", "signing in a browser"),
    ("POST", "/api/v1/auth/logout", "signing out this browser"),
    ("POST", "/api/v1/auth/passkeys/login/begin", "a passkey ceremony in the browser"),
    ("POST", "/api/v1/auth/passkeys/login/finish", "a passkey ceremony in the browser"),
    ("POST", "/api/v1/auth/passkeys/register/begin", "a passkey ceremony in the browser"),
    ("POST", "/api/v1/auth/passkeys/register/finish", "a passkey ceremony in the browser"),
    ("POST", "/api/v1/assistant", "starts an assistant conversation (this one)"),
    ("GET", "/api/v1/catalog", "this catalog"),
    ("GET", "/api/v1/openapi.json", "this catalog, as OpenAPI"),
    ("POST", "/api/v1/catalog/call", "this catalog"),
    ("POST", "/api/v1/sessions/{id}/answers/{call_id}", "answers the assistant's questions (the UI's forms)"),
];

/// An operation's input schema: path parameters, then the body (under
/// `body`) or query parameters.
pub fn input_schema(op: &Op) -> Value {
    let mut props = serde_json::Map::new();
    let mut required: Vec<String> = Vec::new();
    if let Via::Http { path, .. } = &op.via {
        for p in path_params(path) {
            props.insert(p.to_owned(), json!({"type": "string"}));
            required.push(p.to_owned());
        }
    }
    match op.input {
        Input::None => {}
        Input::Body(f) => {
            props.insert("body".into(), f());
            required.push("body".into());
        }
        Input::Query(f) => {
            let s = f();
            if let Some(p) = s["properties"].as_object() {
                for (k, v) in p {
                    props.insert(k.clone(), v.clone());
                }
            } else {
                props.insert("query".into(), json!({"type": "object", "description": "query parameters"}));
            }
        }
        Input::Params(s) => return serde_json::from_str(s).unwrap_or_else(|_| any_object()),
        Input::Typed(f) => return f(),
    }
    json!({"type": "object", "properties": props, "required": required})
}

pub fn path_params(path: &str) -> Vec<&str> {
    path.split('/').filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}'))).collect()
}

/// An operation as the assistant sees it in a list.
pub fn brief(op: &Op) -> Value {
    json!({"op": op.name, "area": op.area, "effect": op.effect, "summary": op.summary})
}

/// An operation in full.
pub fn describe(op: &Op) -> Value {
    json!({
        "op": op.name, "area": op.area, "effect": op.effect, "summary": op.summary, "via": op.via,
        "input": input_schema(op),
        "choices": op.choices.iter().map(|(f, o)| json!({"field": f, "from": o})).collect::<Vec<_>>(),
        "secret_output": op.secret,
        "replaces_list": op.replaces.is_some(),
    })
}

/// Sections of carbon6's documentation, for "how do I…".
pub fn docs() -> &'static [(String, String)] {
    static DOCS: OnceLock<Vec<(String, String)>> = OnceLock::new();
    DOCS.get_or_init(|| {
        let files = [
            ("README", include_str!("../../../README.md")),
            ("assistant", include_str!("../../../docs/assistant.md")),
            ("follow-through", include_str!("../../../docs/follow-through.md")),
            ("memex", include_str!("../../../docs/memex.md")),
            ("architecture", include_str!("../../../docs/architecture.md")),
        ];
        let mut out = Vec::new();
        for (file, text) in files {
            let mut title = file.to_owned();
            let mut body = String::new();
            for line in text.lines() {
                if line.starts_with('#') {
                    if !body.trim().is_empty() {
                        out.push((title.clone(), std::mem::take(&mut body)));
                    }
                    title = format!("{file}: {}", line.trim_start_matches('#').trim());
                    body.clear();
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
            }
            if !body.trim().is_empty() {
                out.push((title, body));
            }
        }
        out
    })
}

fn words(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2).map(|w| w.chars().take(6).collect()).collect()
}

/// Operations and documentation matching a query (all areas when empty).
pub fn discover(query: &str, area: Option<&str>) -> Value {
    let q = words(query);
    let in_area = |o: &&Op| area.is_none_or(|a| o.area == a);
    if q.is_empty() {
        return json!({
            "areas": AREAS.iter().map(|(a, d)| json!({"area": a, "what": d, "operations": ops().iter().filter(|o| o.area == *a).count()})).collect::<Vec<_>>(),
            "operations": if area.is_some() { ops().iter().filter(in_area).map(brief).collect::<Vec<_>>() } else { Vec::new() },
            "hint": "Ask again with a query or an area to see operations.",
        });
    }
    let score = |text: &str| -> usize {
        let t = words(text);
        q.iter().filter(|w| t.contains(w)).count()
    };
    // With a query, an area ranks its operations first rather than hiding
    // the rest.
    let mut found: Vec<(usize, &Op)> = ops()
        .iter()
        .map(|o| (score(&format!("{} {} {} {}", o.name.replace(['.', '_'], " "), o.area, o.summary, o.name)), o))
        .filter(|(s, _)| *s > 0)
        .map(|(s, o)| (s * 2 + usize::from(area == Some(o.area)), o))
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    let mut docs: Vec<(usize, &(String, String))> =
        docs().iter().map(|d| (score(&format!("{} {}", d.0, d.1)) * 2 / 3 + score(&d.0), d)).filter(|(s, _)| *s > 0).collect();
    docs.sort_by_key(|d| std::cmp::Reverse(d.0));
    json!({
        "operations": found.iter().take(12).map(|(_, o)| brief(o)).collect::<Vec<_>>(),
        "docs": docs.iter().take(2).map(|(_, (t, b))| json!({"section": t, "text": crate::util_clip(b.trim(), 700)})).collect::<Vec<_>>(),
    })
}

/// `discover` as the assistant reads it: operations first, then docs.
pub fn discover_text(query: &str, area: Option<&str>) -> String {
    let v = discover(query, area);
    let mut out = String::new();
    if let Some(areas) = v["areas"].as_array() {
        out.push_str("Areas (operations):\n");
        for a in areas {
            out.push_str(&format!("- {} ({}): {}\n", a["area"].as_str().unwrap_or(""), a["operations"], a["what"].as_str().unwrap_or("")));
        }
    }
    let ops = v["operations"].as_array().cloned().unwrap_or_default();
    if !ops.is_empty() {
        out.push_str("Operations (best first):\n");
        for o in &ops {
            out.push_str(&format!(
                "- {} [{}] {}\n",
                o["op"].as_str().unwrap_or(""),
                o["effect"].as_str().unwrap_or(""),
                o["summary"].as_str().unwrap_or("")
            ));
        }
    } else if v["areas"].is_null() {
        out.push_str("No operations match; try other words, or no query to see the areas.\n");
    }
    for d in v["docs"].as_array().into_iter().flatten() {
        out.push_str(&format!("\nDocs — {}:\n{}\n", d["section"].as_str().unwrap_or(""), d["text"].as_str().unwrap_or("")));
    }
    if let Some(h) = v["hint"].as_str() {
        out.push_str(&format!("\n{h}\n"));
    }
    out
}

// --- The API reference, for agents writing code against carbon6 ------------

/// How scripts reach the API: the base URL, auth, errors and the memex
/// envelope.
fn api_basics(urls: &[String]) -> String {
    let base = if urls.is_empty() { "the head's web address".to_owned() } else { urls.join(" or ") };
    format!(
        "carbon6 HTTP API\n\
- Base URL: {base}. Code should take it from configuration (for example C6_URL), not hard-code it.\n\
- Auth: every /api/ request sends `Authorization: Bearer c6_…`, an API token the user creates in Settings → Sign-in → API tokens. \
Read it from the environment (for example C6_TOKEN); never hard-code, print or log it. The user can give sessions a token as a secret \
(Settings → Agents and tools → Secrets). Tokens reach everything except sign-in management (/api/v1/auth/…).\n\
- TLS: an https head may use carbon6's own certificate authority; trust it from <base URL>/ca.pem rather than turning verification off.\n\
- Requests and responses are JSON. An error is a non-2xx status with {{\"error\": \"message\"}}.\n\
- Memex operations are one endpoint: POST /api/v1/memex/rpc with {{\"method\": \"memex.…\", \"params\": {{…}}}}; the reply is the method's result.\n\
- Effects: read; write; destructive (deletes, revokes, sends: code should do these only when the user asked for exactly that); \
secret (the reply holds a secret: never log it).\n\
- The reference gives each operation's input. For a reply's shape, call a read operation once and look.\n"
    )
}

fn http_of(op: &Op) -> (&'static str, String) {
    match &op.via {
        Via::Http { method, path } => (method, path.to_string()),
        Via::Memex { .. } => ("POST", "/api/v1/memex/rpc".to_owned()),
    }
}

/// One operation, as a developer reads it.
fn op_reference(op: &Op) -> String {
    let (method, path) = http_of(op);
    let mut out = format!(
        "{} [{}]: {}\n{method} {path}\n",
        op.name,
        serde_json::to_value(op.effect).unwrap_or_default().as_str().unwrap_or(""),
        op.summary
    );
    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default();
    let example_body: Option<String> = match (&op.via, op.input) {
        (Via::Memex { method }, input) => {
            let params = match input {
                Input::Params(s) => serde_json::from_str(s).unwrap_or_else(|_| any_object()),
                Input::Typed(f) => f(),
                _ => any_object(),
            };
            out.push_str(&format!(
                "Body: {{\"method\": \"{method}\", \"params\": <params>}}\nparams (JSON Schema):\n{}\n",
                pretty(&params)
            ));
            Some(format!("{{\"method\": \"{method}\", \"params\": {{…}}}}"))
        }
        (Via::Http { path: p, .. }, input) => {
            let params = path_params(p);
            if !params.is_empty() {
                out.push_str("Path parameters:");
                for name in &params {
                    let from = op.choices.iter().find(|(f, _)| f == name).map(|(_, o)| format!(" (from {o})")).unwrap_or_default();
                    out.push_str(&format!(" {name}{from};"));
                }
                out.push('\n');
            }
            match input {
                Input::Body(f) => {
                    out.push_str(&format!("Body (JSON Schema):\n{}\n", pretty(&f())));
                    Some("{…}".to_owned())
                }
                Input::Query(f) => {
                    out.push_str(&format!("Query parameters (JSON Schema):\n{}\n", pretty(&f())));
                    None
                }
                _ => None,
            }
        }
    };
    let url_path = path.replace('{', "<").replace('}', ">");
    out.push_str(&format!("Example:\n  curl -sS -X {method} \"$C6_URL{url_path}\" -H \"Authorization: Bearer $C6_TOKEN\""));
    if let Some(b) = example_body {
        out.push_str(&format!(" -H 'Content-Type: application/json' -d '{b}'"));
    }
    out.push('\n');
    if !op.secret.is_empty() {
        out.push_str(&format!("The reply's {} hold a secret.\n", op.secret.join(", ")));
    }
    out
}

/// The reference for agents: the basics and the areas (no arguments),
/// operations matching `query`, or one operation in full.
pub fn reference(query: &str, op: &str, urls: &[String]) -> Result<String, String> {
    if !op.trim().is_empty() {
        let o = find(op.trim()).ok_or_else(|| format!("no operation {op:?}; search with query"))?;
        return Ok(op_reference(o));
    }
    let mut out = api_basics(urls);
    if query.trim().is_empty() {
        out.push_str("\nAreas (operations):\n");
        for (a, d) in AREAS {
            out.push_str(&format!("- {a} ({}): {d}\n", ops().iter().filter(|o| o.area == *a).count()));
        }
        out.push_str("\nSearch with query, read one operation with op, or save the OpenAPI document for code generation.\n");
        return Ok(out);
    }
    let found = discover(query, None);
    let names: Vec<&str> = found["operations"].as_array().into_iter().flatten().filter_map(|o| o["op"].as_str()).collect();
    if names.is_empty() {
        return Ok(format!("{out}\nNo operations match {query:?}; try other words, or no query to see the areas.\n"));
    }
    out.push_str("\nMatching operations (best first; the first in full):\n\n");
    for (i, n) in names.iter().enumerate() {
        let Some(o) = find(n) else { continue };
        if i == 0 {
            out.push_str(&op_reference(o));
            out.push('\n');
        } else {
            let (m, p) = http_of(o);
            let via = if let Via::Memex { method } = &o.via { format!("{m} {p} ({method})") } else { format!("{m} {p}") };
            out.push_str(&format!("- {n}: {} — {via}\n", o.summary));
        }
    }
    Ok(out)
}

/// Moves a JSON Schema's `$defs` into `defs` and points its references
/// there (OpenAPI keeps shared schemas under components).
fn hoist(mut schema: Value, defs: &mut serde_json::Map<String, Value>) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        obj.remove("$schema");
        for key in ["$defs", "definitions"] {
            if let Some(Value::Object(d)) = obj.remove(key) {
                for (k, v) in d {
                    let v = hoist(v, defs);
                    defs.entry(k).or_insert(v);
                }
            }
        }
    }
    fn refs(v: &mut Value) {
        match v {
            Value::Object(o) => {
                if let Some(Value::String(r)) = o.get_mut("$ref") {
                    for from in ["#/$defs/", "#/definitions/"] {
                        if let Some(name) = r.strip_prefix(from) {
                            *r = format!("#/components/schemas/{name}");
                        }
                    }
                }
                o.values_mut().for_each(refs);
            }
            Value::Array(a) => a.iter_mut().for_each(refs),
            _ => {}
        }
    }
    refs(&mut schema);
    schema
}

/// The API as an OpenAPI 3.1 document, from the catalog. Memex operations
/// are the variants of one request body on /api/v1/memex/rpc.
pub fn openapi(urls: &[String]) -> Value {
    let mut defs = serde_json::Map::new();
    let mut paths = serde_json::Map::new();
    let json_body = |schema: Value| json!({"required": true, "content": {"application/json": {"schema": schema}}});
    let responses = json!({
        "2XX": {"description": "The result", "content": {"application/json": {"schema": {}}}},
        "default": {"description": "An error", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ApiError"}}}},
    });
    let mut rpc = Vec::new();
    for op in ops() {
        let effect = serde_json::to_value(op.effect).unwrap_or_default();
        match (&op.via, op.input) {
            (Via::Memex { method }, input) => {
                let params = match input {
                    Input::Params(s) => serde_json::from_str(s).unwrap_or_else(|_| any_object()),
                    Input::Typed(f) => f(),
                    _ => any_object(),
                };
                rpc.push(json!({
                    "title": op.name, "description": op.summary, "type": "object", "x-c6-effect": effect,
                    "required": ["method"],
                    "properties": {"method": {"const": method}, "params": hoist(params, &mut defs)},
                }));
            }
            (Via::Http { method, path }, input) => {
                let mut parameters: Vec<Value> = path_params(path)
                    .into_iter()
                    .map(|p| json!({"name": p, "in": "path", "required": true, "schema": {"type": "string"}}))
                    .collect();
                let mut entry = json!({
                    "operationId": op.name, "summary": op.summary, "tags": [op.area], "x-c6-effect": effect, "responses": responses,
                });
                match input {
                    Input::Body(f) => entry["requestBody"] = json_body(hoist(f(), &mut defs)),
                    Input::Query(f) => {
                        let q = hoist(f(), &mut defs);
                        let required: Vec<&str> = q["required"].as_array().into_iter().flatten().filter_map(|r| r.as_str()).collect();
                        for (name, schema) in q["properties"].as_object().into_iter().flatten() {
                            parameters.push(
                                json!({"name": name, "in": "query", "required": required.contains(&name.as_str()), "schema": schema}),
                            );
                        }
                    }
                    _ => {}
                }
                if !parameters.is_empty() {
                    entry["parameters"] = Value::Array(parameters);
                }
                let item = paths.entry(path.to_string()).or_insert_with(|| json!({}));
                item[method.to_lowercase()] = entry;
            }
        }
    }
    paths.insert(
        "/api/v1/memex/rpc".into(),
        json!({"post": {
            "operationId": "memex.rpc", "summary": "Calls a memex method: one variant per memex operation (its title).",
            "tags": ["memex"], "requestBody": json_body(json!({"oneOf": rpc})), "responses": responses,
        }}),
    );
    defs.insert("ApiError".into(), json!({"type": "object", "required": ["error"], "properties": {"error": {"type": "string"}}}));
    json!({
        "openapi": "3.1.0",
        "info": {"title": "carbon6", "version": env!("CARGO_PKG_VERSION"), "description": api_basics(urls)},
        "servers": urls.iter().map(|u| json!({"url": u})).collect::<Vec<_>>(),
        "security": [{"token": []}],
        "tags": AREAS.iter().map(|(a, d)| json!({"name": a, "description": d})).collect::<Vec<_>>(),
        "paths": paths,
        "components": {"securitySchemes": {"token": {"type": "http", "scheme": "bearer", "description": "An API token (c6_…)"}}, "schemas": defs},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every route is an operation or a known exception, and every
    /// operation is a route; every memex method the UI calls is an
    /// operation.
    #[test]
    fn catalog_matches_the_router() {
        let mut routes: Vec<(String, String)> = Vec::new();
        let re = regex::Regex::new(r#"(?m)\.route\(\s*"([^"]+)",(.*)$"#).unwrap();
        let method = regex::Regex::new(r"\b(get|post|put|delete|patch)\(").unwrap();
        for src in [include_str!("api.rs"), include_str!("auth.rs"), include_str!("assistant.rs")] {
            for c in re.captures_iter(src) {
                for m in method.captures_iter(&c[2]) {
                    routes.push((m[1].to_uppercase(), c[1].to_owned()));
                }
            }
        }
        assert!(routes.len() > 100, "routes found: {}", routes.len());
        let ops_http: Vec<(String, String)> = ops()
            .iter()
            .filter_map(|o| match o.via {
                Via::Http { method, path } => Some((method.to_owned(), path.to_owned())),
                _ => None,
            })
            .collect();
        let excepted = |m: &str, p: &str| NOT_OPERATIONS.iter().any(|(em, ep, _)| *em == m && *ep == p);
        let missing: Vec<_> = routes.iter().filter(|(m, p)| !ops_http.contains(&(m.clone(), p.clone())) && !excepted(m, p)).collect();
        assert!(missing.is_empty(), "routes without an operation (add them to the catalog or NOT_OPERATIONS): {missing:?}");
        let stale: Vec<_> = ops_http.iter().filter(|r| !routes.contains(r)).collect();
        assert!(stale.is_empty(), "operations without a route: {stale:?}");
        // Names are unique.
        let mut names: Vec<&str> = ops().iter().map(|o| o.name).collect();
        names.sort();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len(), "duplicate operation names");
        // The memex methods the UI calls.
        let ui_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../c6-ui/src");
        let ui: Vec<String> = std::fs::read_dir(&ui_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .collect();
        let called = regex::Regex::new(r#""(memex\.[a-z_.]+)""#).unwrap();
        let methods: Vec<&str> = ops().iter().filter_map(|o| if let Via::Memex { method } = o.via { Some(method) } else { None }).collect();
        for src in &ui {
            for c in called.captures_iter(src) {
                let m = &c[1];
                assert!(methods.contains(&m), "the UI calls {m}, which has no operation");
            }
        }
    }

    #[test]
    fn schemas_and_discovery() {
        for op in ops() {
            let s = input_schema(op);
            assert_eq!(s["type"], "object", "{}", op.name);
        }
        let d = discover("enrollment token worker", None);
        assert_eq!(d["operations"][0]["op"], "workers.enrollments.create", "{d}");
        let d = discover("vision model memex", None);
        assert!(d["operations"].as_array().unwrap().iter().any(|o| o["op"] == "settings.memex.get"), "{d}");
        let all = discover("", None);
        assert_eq!(all["areas"].as_array().unwrap().len(), AREAS.len());
    }

    #[test]
    fn api_reference() {
        let urls = vec!["https://head.example:7443".to_owned()];
        let basics = reference("", "", &urls).unwrap();
        assert!(basics.contains("https://head.example:7443") && basics.contains("Authorization: Bearer c6_"), "{basics}");
        assert!(AREAS.iter().all(|(a, _)| basics.contains(&format!("- {a} ("))), "{basics}");

        let one = reference("", "sessions.create", &urls).unwrap();
        assert!(one.contains("POST /api/v1/sessions\n") && one.contains("Body (JSON Schema)") && one.contains("curl -sS -X POST"), "{one}");
        let path = reference("", "sessions.send", &urls).unwrap();
        assert!(
            path.contains("Path parameters: id (from sessions.list);") && path.contains("$C6_URL/api/v1/sessions/<id>/messages"),
            "{path}"
        );
        let memex = ops().iter().find(|o| matches!(o.via, Via::Memex { .. })).unwrap();
        let m = reference("", memex.name, &urls).unwrap();
        assert!(m.contains("POST /api/v1/memex/rpc") && m.contains("\"method\": \"memex."), "{m}");
        assert!(reference("", "no.such.op", &urls).is_err());

        let found = reference("enrollment token worker", "", &urls).unwrap();
        assert!(found.contains("Matching operations") && found.contains("workers.enrollments.create [secret]"), "{found}");
    }

    #[test]
    fn openapi_document() {
        let doc = openapi(&["http://localhost:7070".to_owned()]);
        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(doc["servers"][0]["url"], "http://localhost:7070");
        // Every HTTP operation is there once, under its method and path.
        let paths = doc["paths"].as_object().unwrap();
        let http = ops().iter().filter(|o| matches!(o.via, Via::Http { .. })).count();
        let listed: usize = paths.iter().filter(|(p, _)| *p != "/api/v1/memex/rpc").map(|(_, i)| i.as_object().unwrap().len()).sum();
        assert_eq!(listed, http, "an operation shares a method and path with another");
        let memex = ops().iter().filter(|o| matches!(o.via, Via::Memex { .. })).count();
        let variants = &paths["/api/v1/memex/rpc"]["post"]["requestBody"]["content"]["application/json"]["schema"]["oneOf"];
        assert_eq!(variants.as_array().unwrap().len(), memex);
        // Every reference resolves, and no nested $defs are left.
        let text = doc.to_string();
        assert!(!text.contains("\"$defs\"") && !text.contains("#/$defs/") && !text.contains("#/definitions/"));
        let schemas = doc["components"]["schemas"].as_object().unwrap();
        let re = regex::Regex::new(r##""\$ref":"#/components/schemas/([^"]+)""##).unwrap();
        for c in re.captures_iter(&text) {
            assert!(schemas.contains_key(&c[1]), "unresolved reference {}", &c[1]);
        }
    }
}

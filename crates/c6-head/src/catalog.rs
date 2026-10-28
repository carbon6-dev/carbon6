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
    After, AnalyticsQuery, DecisionQuery, FindQuery, ForSession, IdQuery, MemexSearchQuery, RunsQuery, TestConnection, TrailEdit,
    TrailStepIn, UpdateSession, UsageQuery, WorkflowRunsQuery,
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

/// Who may call an operation (docs/realms-ux.md §3): a realm's callers,
/// the console, or anyone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The caller's realm's own data: lists narrowed to it, its objects
    /// only, new ones made in it.
    Realm,
    /// Infrastructure a realm uses but doesn't own, to read.
    Shared,
    /// carbon6's administration: the console (root) only.
    Admin,
    /// This browser's own business, from anywhere.
    Device,
}

/// An operation's scope. `None` for an operation nobody has placed yet
/// (a test fails on it; callers treat it as administration).
pub fn scope(name: &str) -> Option<Scope> {
    use Scope::*;
    const DEVICE: &[&str] = &[
        "auth.status",
        "system.version",
        "push.key",
        "push.subscribe",
        "push.unsubscribe",
        "push.devices",
        "realms.elsewhere",
        "sessions.locate",
    ];
    const SHARED: &[&str] = &["workers.list", "connections.list", "connections.models", "commands.list", "realms.list", "hosts.list"];
    // Realm data whose handlers don't narrow to the caller's realm yet
    // (or that are carbon6-wide today): the console's, until they do.
    const ADMIN: &[&str] = &[
        "imports.list",
        "imports.settings.get",
        "imports.settings.put",
        "explorer.map",
        "apps.list",
        "apps.install",
        "apps.plan",
        "apps.remove",
        "apps.upgrade",
        "apps.export",
        "realms.create",
        "realms.update",
        "realms.delete",
        "settings.public_url.get",
        "settings.public_url.put",
        "settings.memex.get",
        "settings.memex.put",
        "settings.routes.get",
        "settings.routes.put",
        "settings.prices.get",
        "settings.prices.put",
        "settings.vocabulary.get",
        "settings.vocabulary.put",
    ];
    const ADMIN_AREAS: &[&str] = &["workers.", "connections.", "auth.", "system.", "installs.", "packages.", "grants."];
    const REALM_AREAS: &[&str] = &[
        "sessions.",
        "decisions.",
        "memories.",
        "memex.",
        "claims.",
        "things.",
        "knowledge.",
        "photos.",
        "chat.",
        "slack.",
        "mail.",
        "calendar.",
        "contacts.",
        "google.",
        "collections.",
        "records.",
        "series.",
        "views.",
        "templates.",
        "triggers.",
        "explorer.",
        "apps.accounts",
        "apps.operations",
        "settings.secrets.",
        "settings.skills.",
        "settings.commands.",
        "settings.agents.",
        "settings.hooks.",
        "settings.mcp_servers.",
        "settings.templates.",
        "settings.policies.",
        "settings.channels.",
        "settings.budgets.",
        "settings.views.",
        "settings.hosts.",
        "settings.claim_policies.",
        "settings.memex_scopes.",
        "settings.memex_folders.",
        "settings.workflows.",
        "settings.prompts.",
        "settings.subagent_limits.",
        "settings.web_search.",
        "settings.brief.",
        "settings.assistant.",
        "settings.own.",
        "workflows.",
        "usage.",
    ];
    // A worker's files, git and terminals: for a realm the worker serves.
    const REALM_FIRST: &[&str] = &["workers.fs.", "workers.git.", "workers.terminals."];
    if DEVICE.contains(&name) {
        Some(Device)
    } else if REALM_FIRST.iter().any(|a| name.starts_with(a)) {
        Some(Realm)
    } else if SHARED.contains(&name) {
        Some(Shared)
    } else if ADMIN.contains(&name) || ADMIN_AREAS.iter().any(|a| name.starts_with(a)) {
        Some(Admin)
    } else if REALM_AREAS.iter().any(|a| name.starts_with(a)) || matches!(name, "realms.effective" | "notifications.test" | "hosts.check") {
        Some(Realm)
    } else {
        None
    }
}

/// Whether a request path fits a route's path (`{x}` takes one segment).
fn fits(template: &str, path: &str) -> Option<usize> {
    let (t, p): (Vec<&str>, Vec<&str>) = (template.split('/').collect(), path.split('/').collect());
    if t.len() != p.len() {
        return None;
    }
    let mut literal = 0;
    for (a, b) in t.iter().zip(&p) {
        if a.starts_with('{') && a.ends_with('}') {
            if b.is_empty() {
                return None;
            }
        } else if a == b {
            literal += 1;
        } else {
            return None;
        }
    }
    Some(literal)
}

/// The scope of a request: its operation's (the route that fits it most
/// literally), else administration.
pub fn route_scope(method: &str, path: &str) -> Scope {
    let ops = ops().iter().filter_map(|o| match o.via {
        Via::Http { method: m, path: t } if m == method => fits(t, path).map(|n| (n, scope(o.name).unwrap_or(Scope::Admin))),
        _ => None,
    });
    let other = NOT_OPERATIONS.iter().filter(|(m, ..)| *m == method).filter_map(|(_, t, _, s)| fits(t, path).map(|n| (n, *s)));
    ops.chain(other).max_by_key(|(n, _)| *n).map_or(Scope::Admin, |(_, s)| s)
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
    (
        "automation",
        "Triggers (schedules, webhooks, memex, record, decision, session and series events, with filters and actions), templates, workflows and their runs.",
    ),
    ("knowledge", "Memory notes, and the memex: search, notes, trails, documents, settings, folders, review, commitments, graph."),
    ("mail", "Mail, calendar and contacts through the memex's Google accounts."),
    ("chat", "Chat: Slack workspaces' conversations, messages and threads in the memex; posting through the slack app's operations."),
    ("photos", "Photos and videos in the memex."),
    ("notifications", "Notification channels, web push devices, the public URL for links."),
    ("usage", "Usage, prices and budgets."),
    ("hosts", "The SSH host inventory."),
    ("settings", "Tools and context for sessions: MCP servers, skills, commands, agents, hooks, secrets, web search."),
    ("account", "Sign-in: passkeys, signed-in browsers, API tokens, one-time codes."),
    (
        "collections",
        "Collections: typed tables of records in a realm, for anything apps and agents keep (strict schemas, unique keys, history).",
    ),
    (
        "apps",
        "Apps: packages of configuration (collections, series, triggers, views, skills, templates…) installed into realms of their own; install, upgrade, export, remove.",
    ),
    ("explorer", "The realm explorer: what each realm holds, what happened lately, and every object with what is connected to it."),
    ("views", "Views: a realm's configured screens (tables, boards, records, charts, feeds, pages) and their actions."),
    ("series", "Series: numbers over time in a realm (points with dimensions), summarized over windows; thresholds are triggers."),
    ("system", "carbon6 itself: realms, upgrades of its databases, and the backups made before each."),
    (
        "decisions",
        "Decisions: every time carbon6 needs the user — agents' questions, approvals of tool calls and workflow steps, the assistant's questions, facts to confirm, disputes.",
    ),
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
        op!("workers.enrollments.create", "fleet", "POST" "/api/v1/workers/enrollments", Secret, Input::Body(schema::<api::CreateEnrollment>), "Creates a one-time enrollment token for a new worker (c6w enroll <token>); valid for an hour.", secret = &["token"]),
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
        op!("sessions.children", "sessions", "GET" "/api/v1/sessions/{id}/children", Read, Input::None, "A session's subagents and side chats (?tree=1: everything below it).", choices = &[("id", "sessions.list")]),
        op!("sessions.side_chat", "sessions", "POST" "/api/v1/sessions/{id}/side-chats", Write, Input::Body(schema::<api::CreateSideChat>), "Branches a side chat off a session at an event.", choices = &[("id", "sessions.list")]),
        op!("sessions.send", "sessions", "POST" "/api/v1/sessions/{id}/messages", Write, Input::Body(schema::<api::SendMessage>), "Sends a message to a session (it runs next), optionally switching its model.", choices = &[("id", "sessions.list")]),
        op!("sessions.interrupt", "sessions", "POST" "/api/v1/sessions/{id}/interrupt", Write, Input::None, "Stops a session's running turn.", choices = &[("id", "sessions.list")]),
        op!("sessions.resume", "sessions", "POST" "/api/v1/sessions/{id}/resume", Write, Input::Body(schema::<api::ResumeRequest>), "Continues a subagent whose last turn did not finish, with the resume prompt."),
        op!("sessions.rewind", "sessions", "POST" "/api/v1/sessions/{id}/rewind", Destructive, Input::Body(schema::<api::RewindRequest>), "Rewinds a session to before one of your messages (later events are discarded; files optionally restored).", choices = &[("id", "sessions.list")]),
        op!("sessions.subagent_model.set", "sessions", "PUT" "/api/v1/sessions/{id}/subagent-model", Write, Input::Body(schema::<api::SetSubagentModel>), "Sets the connection, model and effort a session's subagents (Task tool) run on from now on; an empty model: the session's own. Not a Claude Code connection: subagents run in carbon6's own loop.", choices = &[("id", "sessions.list")]),
        op!("sessions.colour.set", "sessions", "PUT" "/api/v1/sessions/{id}/colour", Write, Input::Body(schema::<api::SetColour>), "Sets a session's colour (sky, emerald, amber, rose, violet, teal, orange, indigo, lime, pink, cyan or fuchsia): the cue that tells its tiles apart.", choices = &[("id", "sessions.list")]),
        op!("sessions.labels.set", "sessions", "PUT" "/api/v1/sessions/{id}/labels", Write, Input::Body(schema::<api::SetLabels>), "Sets a session's labels.", choices = &[("id", "sessions.list")]),
        op!("sessions.config", "sessions", "GET" "/api/v1/sessions/{id}/config", Read, Input::None, "What applies to a session: labels, MCP servers, skills, hooks, policies, memory, secrets, model route.", choices = &[("id", "sessions.list")]),
        op!("sessions.tasks.stop", "sessions", "POST" "/api/v1/sessions/{id}/tasks/{task}/stop", Destructive, Input::None, "Stops a session's background task.", choices = &[("id", "sessions.list")]),
        op!("sessions.approvals.decide", "sessions", "POST" "/api/v1/sessions/{id}/approvals/{call_id}", Write, Input::Body(schema::<api::ApprovalDecision>), "Allows or denies a tool call waiting for approval.", choices = &[("id", "sessions.list")]),
        op!("sessions.queue.dismiss", "sessions", "DELETE" "/api/v1/sessions/{id}/queue/rejected", Write, Input::None, "Dismisses a session's rejected queued messages.", choices = &[("id", "sessions.list")]),
        op!("sessions.decisions", "sessions", "GET" "/api/v1/sessions/{id}/decisions", Read, Input::None, "A session's open decisions: what its agent raised or asked without stopping, grants it asked for, and what it was started to work on (its tool approvals and questions are in its log).", choices = &[("id", "sessions.list")]),
        op!("sessions.suggestion", "sessions", "GET" "/api/v1/sessions/{id}/suggestion", Read, Input::None, "The message the user would likely send next, once a session's turn finished (made by a cheap model, at most once per turn; 204 when there is none).", choices = &[("id", "sessions.list")]),
        op!("sessions.suggestion.seen", "sessions", "POST" "/api/v1/sessions/{id}/suggestion/seen", Write, Input::Body(schema::<api::SuggestionSeen>), "Records that a suggestion was shown, and whether it was taken.", choices = &[("id", "sessions.list")]),
        op!("sessions.grouping.get", "sessions", "GET" "/api/v1/settings/session_grouping", Read, Input::None, "How the session list is grouped: label keys, outermost first."),
        op!("sessions.grouping.put", "sessions", "PUT" "/api/v1/settings/session_grouping", Write, Input::Body(schema::<api::SessionGrouping>), "Sets how the session list is grouped: label keys such as repo, host or client, outermost first."),
        op!("settings.own.get", "settings", "GET" "/api/v1/settings/own/{name}", Read, Input::None, "Whether this realm sets a setting itself (suggestions, web-search, assistant, subagent-limits) or follows root's."),
        op!("settings.own.reset", "settings", "DELETE" "/api/v1/settings/own/{name}", Write, Input::None, "This realm goes back to following root's value of a setting (suggestions, web-search, assistant, subagent-limits)."),
        op!("sessions.suggestions.settings.get", "sessions", "GET" "/api/v1/settings/suggestions", Read, Input::None, "Whether sessions suggest your next message (per realm)."),
        op!("sessions.suggestions.settings.put", "sessions", "PUT" "/api/v1/settings/suggestions", Write, Input::Body(schema::<api::SuggestionSettings>), "Turns suggested next messages on or off (per realm)."),
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
    v.extend([
        op!("explorer.map", "explorer", "GET" "/api/v1/explorer/map", Read, Input::None, "Every realm with what it holds (counts by data, knowledge, work, config), its app, spend this month, what needs attention, and the grants between realms."),
        op!("explorer.summary", "explorer", "GET" "/api/v1/realms/{id}/summary", Read, Input::None, "A realm's inventory (each collection, series, kind of knowledge, sessions, decisions, triggers, settings lists with what root gives it, grants, workers), what needs attention, and spend this month.", choices = &[("id", "realms.list")]),
        op!("explorer.activity", "explorer", "GET" "/api/v1/realms/{id}/activity", Read, Input::Query(schema::<crate::api::ActivityQuery>), "What happened in a realm lately: records changed, decisions, sessions, trigger runs, points, knowledge added; newest first.", choices = &[("id", "realms.list")]),
        op!("explorer.list", "explorer", "GET" "/api/v1/realms/{id}/list", Read, Input::Query(schema::<crate::api::ExListQuery>), "The objects of one section of a realm's inventory (the section names come from its summary).", choices = &[("id", "realms.list")]),
        op!("explorer.object", "explorer", "GET" "/api/v1/realms/{id}/object", Read, Input::Query(schema::<crate::api::ExObjectQuery>), "One object of a realm (collection, record <collection>/<id>, series, thing, item, fact, session, decision, trigger, grant, view, entry) with everything connected to it: the things a record links to, sessions and decisions about it, triggers that watch or wrote it, and so on.", choices = &[("id", "realms.list")]),
        op!("explorer.search", "explorer", "GET" "/api/v1/realms/{id}/search", Read, Input::Query(schema::<crate::api::ExSearchQuery>), "Everything in a realm matching q: records, things, knowledge, facts, sessions (titles and what was said), decisions, triggers, series and settings entries, grouped by kind.", choices = &[("id", "realms.list")]),
        op!("explorer.neighborhood", "explorer", "GET" "/api/v1/realms/{id}/neighborhood", Read, Input::Query(schema::<crate::api::ExObjectQuery>), "An object's connections two hops deep, as nodes (with depth and the node each hangs from) and labelled edges, for drawing.", choices = &[("id", "realms.list")]),
    ]);
    v.extend([
        op!("packages.list", "apps", "GET" "/api/v1/packages", Read, Input::None, "The local registry: every package version (apps, kits, libraries), what it requires, and where it is installed."),
        op!("packages.add", "apps", "POST" "/api/v1/packages", Write, Input::Body(schema::<api::PackageIn>), "Adds a package version to the registry: its manifest (package, version, description, realm for an app, requires {name: range}, permissions, contents, exports, kit {build {dockerfile, files}, tools}) and its files (path → base64). The same version with other content is refused."),
        op!("packages.get", "apps", "GET" "/api/v1/packages/{name}/{version}", Read, Input::None, "A package version: its manifest and registry entry (digest, roles, files)."),
        op!("packages.delete", "apps", "DELETE" "/api/v1/packages/{name}/{version}", Destructive, Input::None, "Removes a package version nothing has installed."),
        op!("installs.list", "apps", "GET" "/api/v1/installs", Read, Input::None, "What is installed: each package version, where (an app or library: its realm and instance name; a kit: for workers), its settings, what it reads, what it requires (locked) and what requires it."),
        op!("installs.plan", "apps", "POST" "/api/v1/installs/plan", Read, Input::Body(schema::<api::InstallIn>), "What installing a package would install (with what it requires) and ask permission for."),
        op!("installs.add", "apps", "POST" "/api/v1/installs", Write, Input::Body(schema::<api::InstallIn>), "Asks to install a package from the registry (and what it requires) in a realm, as an instance (name: default the package's): one decision for the user listing everything it asks for. Installing never creates a realm."),
        op!("installs.upgrade", "apps", "POST" "/api/v1/installs/{id}/upgrade", Write, Input::Body(schema::<crate::api::UpgradeIn>), "Asks to replace an instance's code with another version (an exact older version downgrades) or with a fork of its package (package): nothing merges, its settings and data stay, and it asks again only for what grew.", choices = &[("id", "installs.list")]),
        op!("installs.remove", "apps", "DELETE" "/api/v1/installs/{id}", Destructive, Input::None, "Asks to remove an install (an app or library: what it owns in its realm; a kit: its tools). Refused while other installs require it.", choices = &[("id", "installs.list")]),
        op!("installs.settings", "apps", "GET" "/api/v1/installs/{id}/settings", Read, Input::None, "An app instance's settings (the values, defaults filled in).", choices = &[("id", "installs.list")]),
        op!("installs.set_settings", "apps", "PUT" "/api/v1/installs/{id}/settings", Write, Input::Body(any_object), "Sets an app instance's settings, checked against the JSON Schema its package declares.", choices = &[("id", "installs.list")]),
        op!("realms.effective", "apps", "GET" "/api/v1/realms/{id}/effective/{key}", Read, Input::Query(schema::<crate::api::EffectiveQuery>), "A settings list (skills, templates, views, …) as a realm's sessions use it, or (instance) as an app instance's sessions do: each entry with where it comes from (root, the realm, or app:<name>).", choices = &[("id", "realms.list")]),
        op!("apps.operations", "apps", "GET" "/api/v1/apps/{realm}/{instance}/operations", Read, Input::None, "An app instance's operations: how anything but the app changes what it owns, each with its input's JSON Schema.", choices = &[("realm", "realms.list")]),
        op!("apps.operations.call", "apps", "POST" "/api/v1/apps/{realm}/{instance}/operations/{name}", Write, Input::Body(schema::<api::OperationCall>), "Calls an app's operation with its input: a write runs at once, a template starts the app's session, as the app.", choices = &[("realm", "realms.list")]),
        op!("claims.list", "knowledge", "GET" "/api/v1/memex/claims", Read, Input::Query(schema::<crate::api::ClaimsQuery>), "Claims about a thing (about: thing:<key>) or an item (item:<id>): who claims what, with what evidence, and whether the realm's reader policies believe it (policy=all shows every claim). same_as=true follows the things taken to be the same."),
        op!("claims.add", "knowledge", "POST" "/api/v1/memex/claims", Write, Input::Body(schema::<api::ClaimIn>), "Adds a claim of yours (a correction outranks an app's under the default policies): from, label, to, confidence, evidence (item ids)."),
        op!("claims.retract", "knowledge", "DELETE" "/api/v1/memex/claims/{id}", Destructive, Input::None, "Retracts a claim of yours (only its owner can; others' claims you answer with your own)."),
        op!("things.create", "knowledge", "POST" "/api/v1/memex/things", Write, Input::Body(any_object), "Makes a thing (a person, a place, a topic) anyone may make claims about: {kind, name, key?}."),
        op!("apps.accounts", "apps", "GET" "/api/v1/apps/{realm}/{instance}/accounts", Read, Input::None, "The accounts connected for an app instance's OAuth providers.", choices = &[("realm", "realms.list")]),
        op!("apps.accounts.connect", "apps", "POST" "/api/v1/apps/{realm}/{instance}/providers/{provider}/connect", Write, Input::None, "Starts connecting an account to one of an app's OAuth providers: the address to send the user to (they sign in there).", choices = &[("realm", "realms.list")]),
        op!("apps.accounts.remove", "apps", "DELETE" "/api/v1/apps/{realm}/{instance}/accounts/{id}", Destructive, Input::None, "Disconnects an account from an app.", choices = &[("realm", "realms.list")]),
        op!("apps.list", "apps", "GET" "/api/v1/apps", Read, Input::None, "Installed apps: each an instance of a package in a realm, with its version and state."),
        op!("apps.plan", "apps", "POST" "/api/v1/apps/plan", Read, Input::Body(schema::<api::AppPackage>), "What installing a package would create, what it needs, and why it could not go ahead."),
        op!("apps.install", "apps", "POST" "/api/v1/apps", Write, Input::Body(schema::<api::AppPackage>), "Asks to install a package (JSON: package, version, description, reads {items {kinds}, collections}, settings (a JSON Schema), permissions {secrets, workers, grants}, contents {collections, series, triggers, memory, records, and settings lists by key: views, skills, agents, commands, hooks, mcp_servers, templates, workflows, policies, routes, prices, budgets}) in ?realm= (default: the realm it suggests if it exists, else home). The user decides; then what it owns is created there."),
        op!("apps.upgrade", "apps", "POST" "/api/v1/apps/{realm}/upgrade", Write, Input::Body(schema::<api::AppPackage>), "Asks to upgrade the app of that package in a realm to a new version: its code becomes the new version's; its settings and data stay.", choices = &[("realm", "apps.list")]),
        op!("apps.export", "apps", "GET" "/api/v1/apps/export", Read, Input::Query(schema::<crate::api::ExportQuery>), "A realm's own configuration as a package, or (instance) an app instance's package, or (instance, fork) a fork of it to change and install; data: the records too. Without secret values."),
        op!("apps.remove", "apps", "DELETE" "/api/v1/apps/{realm}", Destructive, Input::None, "Asks to remove an app from a realm: what it owns there is deleted once the user decides; the realm stays.", choices = &[("realm", "apps.list")]),
    ]);
    v.extend(list_setting!("settings.views.get", "settings.views.put", "views", "/api/v1/settings/views", api::View, "A realm's views (?realm=), its configured screens: name, title, kind (table, board, record, chart, feed, page), source ({collection, where?, sort?, limit?} for table, board and record; {series, agg?, minutes?, bucket?, group_by?} for chart; {decisions: \"open\"} or {sessions: {labels}} for feed), fields, group_by (a board's column field), columns, body (a page's Markdown; {{view:name}} embeds another view), actions ({label, start: template, set: {field: value}}), menu."));
    v.extend(list_setting!("settings.hosts.get", "settings.hosts.put", "hosts", "/api/v1/settings/hosts", api::Host, "The SSH host inventory: name, target, worker to reach it through, labels."));
    v.extend(list_setting!("settings.workflows.get", "settings.workflows.put", "automation", "/api/v1/settings/workflows", api::Workflow, "Workflows: steps of sessions, approvals and conditions."));
    v.extend(list_setting!("settings.claim_policies.get", "settings.claim_policies.put", "knowledge", "/api/v1/settings/claim_policies", api::ClaimPolicy, "Reader policies: per label, whose claims the realm's readers (search, the graph, agents) believe, in order: you, app:<name>, realm, a program, app:*, or * for everyone else. Without one: yours first, then everyone's."));
    v.extend(list_setting!("settings.vocabulary.get", "settings.vocabulary.put", "knowledge", "/api/v1/settings/vocabulary", api::VocabularyEntry, "The shared vocabulary (in root, for every realm): labels and item kinds, and what each means."));
    v.extend(list_setting!("settings.memex_scopes.get", "settings.memex_scopes.put", "knowledge", "/api/v1/settings/memex_scopes", api::MemexScope, "Memex scope rules: which kinds of items (email, photo…) sessions with given labels may see."));
    v.extend(list_setting!("settings.memex_folders.get", "settings.memex_folders.put", "knowledge", "/api/v1/settings/memex_folders", api::MemexFolder, "Folders on workers the memex reads where they are (documents, papers, photos, videos)."));
    v.extend([
        op!("settings.secrets.list", "settings", "GET" "/api/v1/settings/secrets", Read, Input::None, "Secrets sessions can use as environment variables (names and labels; never values)."),
        op!("settings.secrets.put", "settings", "PUT" "/api/v1/settings/secrets/{name}", Write, Input::Body(schema::<api::PutSecret>), "Sets a secret's value (stored encrypted) and which sessions get it. Ask the user for the value with c6_ask (secret field); never repeat it."),
        op!("settings.secrets.delete", "settings", "DELETE" "/api/v1/settings/secrets/{name}", Destructive, Input::None, "Deletes a secret.", choices = &[("name", "settings.secrets.list")]),
        op!("settings.brief.get", "settings", "GET" "/api/v1/settings/brief", Read, Input::None, "When the brief comes (times, in your calendar's time zone), the close of day's note, and whether urgent requests tell you at once (per realm)."),
        op!("settings.brief.put", "settings", "PUT" "/api/v1/settings/brief", Write, Input::Body(schema::<api::BriefSetting>), "Sets when the brief comes and what may interrupt you."),
        op!("settings.web_search.get", "settings", "GET" "/api/v1/settings/web-search", Read, Input::None, "The web search provider sessions use."),
        op!("settings.web_search.put", "settings", "PUT" "/api/v1/settings/web-search", Write, Input::Body(schema::<api::WebSearchSetting>), "Sets the web search provider."),
        op!("settings.subagent_limits.get", "settings", "GET" "/api/v1/settings/subagent-limits", Read, Input::None, "How deep subagents nest and how many may run at once in one session's tree."),
        op!("settings.subagent_limits.put", "settings", "PUT" "/api/v1/settings/subagent-limits", Write, Input::Body(schema::<api::SubagentLimits>), "Sets how deep subagents nest and how many may run at once."),
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
        op!("triggers.list", "automation", "GET" "/api/v1/triggers", Read, Input::None, "Triggers: each an event (schedule, webhook, memex event, record created/updated/deleted, decision opened/answered/expired, session finished/failed/needs_approval), a filter on it (JSONPath conditions), and an action (start a template or workflow, write a record, ask a decision, notify), with how its last runs went."),
        op!("triggers.create", "automation", "POST" "/api/v1/triggers", Write, Input::Body(schema::<api::AutomationEdit>), "Creates a trigger. trigger: the event; cron: a schedule, or a memex event filter (mail.received from:…); event_params: {collection, ops} for records, {ops} for decisions, {ops, labels} for sessions; filter: {\"$.path\": value or {$gt: …}}; action and action_params, whose \"$.path\" strings and {{$.path}} take the event's values (write: {collection, fields, id?}; ask: {title, document, options}; notify: {title, message}; start: template, and input). per_hour limits its runs. A schedule's event has open_decisions {count, high, text}, for digests."),
        op!("triggers.update", "automation", "PATCH" "/api/v1/triggers/{id}", Write, Input::Body(schema::<api::AutomationEdit>), "Changes a trigger (or turns it on or off); its event kind stays.", choices = &[("id", "triggers.list")]),
        op!("triggers.delete", "automation", "DELETE" "/api/v1/triggers/{id}", Destructive, Input::None, "Deletes a trigger.", choices = &[("id", "triggers.list")]),
        op!("triggers.run", "automation", "POST" "/api/v1/triggers/{id}/run", Write, Input::None, "Runs a trigger's action now.", choices = &[("id", "triggers.list")]),
        op!("triggers.runs", "automation", "GET" "/api/v1/triggers/{id}/runs", Read, Input::Query(schema::<RunsQuery>), "A trigger's recent runs: what fired each, the chain of triggers that caused it, how it went, its cost.", choices = &[("id", "triggers.list")]),
        op!("triggers.rotate", "automation", "POST" "/api/v1/triggers/{id}/rotate", Secret, Input::None, "Gives a webhook trigger a new URL (the old one stops working).", secret = &["url", "token"], choices = &[("id", "triggers.list")]),
        op!("templates.list", "automation", "GET" "/api/v1/templates", Read, Input::None, "Every template you can run, with the realm it is in (an app's templates are in its realm)."),
        op!("templates.run", "automation", "POST" "/api/v1/templates/{name}/run", Write, Input::Body(schema::<api::RunTemplate>), "Starts sessions from a template (on each matching worker, for a fleet run); realm: the template's realm (default: the one that has it).", choices = &[("name", "templates.list")]),
        op!("workflows.run", "automation", "POST" "/api/v1/workflows/{name}/run", Write, Input::Body(schema::<api::RunWorkflow>), "Runs a workflow.", choices = &[("name", "settings.workflows.get")]),
        op!("workflows.runs", "automation", "GET" "/api/v1/workflows/runs", Read, Input::Query(schema::<WorkflowRunsQuery>), "Workflow runs, newest first."),
        op!("workflows.runs.get", "automation", "GET" "/api/v1/workflows/runs/{id}", Read, Input::None, "A workflow run with its steps.", choices = &[("id", "workflows.runs")]),
        op!("workflows.steps.decide", "automation", "POST" "/api/v1/workflows/runs/{id}/steps/{step}/decide", Write, Input::Body(schema::<api::ApprovalDecision>), "Approves or rejects a workflow step waiting for you.", choices = &[("id", "workflows.runs")]),

        // Action items.
        op!("decisions.list", "decisions", "GET" "/api/v1/decisions", Read, Input::Query(schema::<DecisionQuery>), "Decisions (open by default): what waits first, then by priority."),
        op!("decisions.count", "decisions", "GET" "/api/v1/decisions/count", Read, Input::None, "How many decisions are open."),
        op!("decisions.get", "decisions", "GET" "/api/v1/decisions/{id}", Read, Input::None, "A decision: its case, options or form, context, and answer.", choices = &[("id", "decisions.list")]),
        op!("decisions.add", "decisions", "POST" "/api/v1/decisions", Write, Input::Body(schema::<api::DecisionIn>), "Adds a decision for the user (a question, something to confirm or provide, a choice between options)."),
        op!("decisions.answer", "decisions", "POST" "/api/v1/decisions/{id}/answer", Write, Input::Body(schema::<api::DecisionAnswer>), "Answers a decision (an option, the form's fields, a note); what waits on it gets the answer.", choices = &[("id", "decisions.list")]),
        op!("decisions.edit", "decisions", "PATCH" "/api/v1/decisions/{id}", Write, Input::Body(schema::<api::DecisionEdit>), "Snoozes, dismisses or reopens a decision.", choices = &[("id", "decisions.list")]),
        op!("decisions.session", "decisions", "POST" "/api/v1/decisions/{id}/session", Write, Input::None, "Starts a session that helps the user settle a decision, with its context.", choices = &[("id", "decisions.list")]),

        // Hosts.
        op!("hosts.list", "hosts", "GET" "/api/v1/hosts", Read, Input::None, "Hosts in the inventory with their last check."),
        op!("hosts.check", "hosts", "POST" "/api/v1/hosts/{name}/check", Read, Input::None, "Checks a host now (reachable over SSH, system details).", choices = &[("name", "hosts.list")]),

        // Usage.
        op!("usage.analytics", "usage", "GET" "/api/v1/analytics", Read, Input::Query(schema::<AnalyticsQuery>), "Analytics for the last `days`: agent, model and tool time, cost, your messages, corrections and answers, and waits on you, by day, model, tool and a label key (`by`), optionally filtered by labels."),
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
        op!("google.client.get", "mail", "GET" "/api/v1/google/client", Read, Input::Query(schema::<api::InRealm>), "The Google OAuth client carbon6 uses (its id; the secret is never shown), every realm's; and the realm's Google apps."),
        op!("google.client.put", "mail", "PUT" "/api/v1/google/client", Write, Input::Body(schema::<api::GoogleClientIn>), "Sets a realm's Google OAuth client (from your Google Cloud project); root's (from the console) is every realm's that has none. Ask for the secret with c6_ask (secret field)."),
        op!("google.connect", "mail", "POST" "/api/v1/google/connect", Write, Input::Query(schema::<api::InRealm>), "Starts adding a Google account to a realm's Gmail, Calendar and Contacts apps: returns the address for you to open and sign in."),
        op!("imports.settings.get", "sessions", "GET" "/api/v1/settings/session_imports", Read, Input::None, "Importing sessions from Claude Code and Codex: which workers import, from which tools, and which realm a folder's sessions go to."),
        op!("imports.settings.put", "sessions", "PUT" "/api/v1/settings/session_imports", Write, Input::Body(schema::<api::SessionImports>), "Sets which workers import their Claude Code and Codex sessions, and the folder → realm rules."),
        op!("imports.list", "sessions", "GET" "/api/v1/imports", Read, Input::None, "Sessions imported from Claude Code and Codex, by source and realm."),
        op!("google.install", "mail", "POST" "/api/v1/google/install", Write, Input::Query(schema::<api::InRealm>), "Asks to install the Gmail, Calendar and Contacts apps in a realm (default home) (a decision each; they run on a worker with Docker or Podman)."),
        op!("google.accounts", "mail", "GET" "/api/v1/google/accounts", Read, Input::Query(schema::<api::InRealm>), "A realm's Google accounts (its Gmail, Calendar and Contacts apps')."),
        op!("google.accounts.remove", "mail", "DELETE" "/api/v1/google/accounts/{id}", Destructive, Input::None, "Removes a Google account (its mail, events and contacts stay in the memex).", choices = &[("id", "google.accounts")]),

        // Account.
        op!("auth.status", "account", "GET" "/api/v1/auth/status", Read, Input::None, "Whether sign-in is required and how you are signed in."),
        op!("auth.codes.create", "account", "POST" "/api/v1/auth/codes", Secret, Input::None, "Creates a one-time sign-in code for another device (valid a few minutes).", secret = &["code"]),
        op!("auth.passkeys.list", "account", "GET" "/api/v1/auth/passkeys", Read, Input::None, "Your passkeys."),
        op!("auth.passkeys.rename", "account", "PATCH" "/api/v1/auth/passkeys/{id}", Write, Input::Body(schema::<api::RenamePasskey>), "Renames a passkey.", choices = &[("id", "auth.passkeys.list")]),
        op!("auth.passkeys.delete", "account", "DELETE" "/api/v1/auth/passkeys/{id}", Destructive, Input::None, "Deletes a passkey (it can no longer sign in).", choices = &[("id", "auth.passkeys.list")]),
        op!("auth.sessions.list", "account", "GET" "/api/v1/auth/sessions", Read, Input::None, "Browsers signed in to carbon6."),
        op!("auth.sessions.delete", "account", "DELETE" "/api/v1/auth/sessions/{id}", Destructive, Input::None, "Signs a browser out.", choices = &[("id", "auth.sessions.list")]),
        op!("collections.list", "collections", "GET" "/api/v1/collections", Read, Input::Query(schema::<api::InRealm>), "Collections in a realm (default: home): name, description, schema, keys, how many records."),
        op!("collections.create", "collections", "POST" "/api/v1/collections", Write, Input::Body(schema::<api::CollectionIn>), "Creates a collection: a name, a description, an object JSON Schema for its records (strict: unknown fields are rejected) and optional unique keys."),
        op!("collections.get", "collections", "GET" "/api/v1/collections/{name}", Read, Input::None, "A collection's description, schema and keys.", choices = &[("name", "collections.list")]),
        op!("collections.update", "collections", "PATCH" "/api/v1/collections/{name}", Write, Input::Body(schema::<api::CollectionIn>), "Changes a collection's description, keys or schema (a new schema must accept every record already there).", choices = &[("name", "collections.list")]),
        op!("collections.delete", "collections", "DELETE" "/api/v1/collections/{name}", Destructive, Input::None, "Deletes a collection and its records (their history stays).", choices = &[("name", "collections.list")]),
        op!("records.query", "collections", "POST" "/api/v1/collections/{name}/query", Read, Input::Body(schema::<api::RecordQuery>), "Finds records: by field values and comparisons, words, sorted, paged, or counted per value of a field.", choices = &[("name", "collections.list")]),
        op!("records.list", "collections", "GET" "/api/v1/collections/{name}/records", Read, Input::None, "A collection's records, newest first.", choices = &[("name", "collections.list")]),
        op!("records.create", "collections", "POST" "/api/v1/collections/{name}/records", Write, Input::Body(schema::<api::RecordIn>), "Adds a record (its fields must match the schema). An app's collection is written only by the app, through its operations.", choices = &[("name", "collections.list")]),
        op!("records.get", "collections", "GET" "/api/v1/collections/{name}/records/{id}", Read, Input::None, "A record.", choices = &[("name", "collections.list")]),
        op!("records.put", "collections", "PUT" "/api/v1/collections/{name}/records/{id}", Write, Input::Body(schema::<api::RecordIn>), "Replaces a record's fields, giving the version you read. An app's collection is written only by the app, through its operations.", choices = &[("name", "collections.list")]),
        op!("records.delete", "collections", "DELETE" "/api/v1/collections/{name}/records/{id}", Destructive, Input::None, "Deletes a record (its history stays).", choices = &[("name", "collections.list")]),
        op!("records.history", "collections", "GET" "/api/v1/collections/{name}/records/{id}/history", Read, Input::None, "Every version of a record, with who changed it.", choices = &[("name", "collections.list")]),
        op!("settings.prompts.get", "settings", "GET" "/api/v1/settings/prompts", Read, Input::None, "The prompts carbon6 gives models (sessions, compaction, subagents, the assistant, action items, the memex), with their defaults and whether they were changed."),
        op!("settings.prompts.put", "settings", "PUT" "/api/v1/settings/prompts/{name}", Write, Input::Body(schema::<api::PromptIn>), "Changes one of carbon6's prompts (an empty body returns it to the default).", choices = &[("name", "settings.prompts.get")]),
        op!("realms.create", "system", "POST" "/api/v1/realms", Write, Input::Body(schema::<api::RealmIn>), "Creates a realm: an isolated place for an app or a project, with its own sessions, collections, decisions, secrets and settings (it inherits root's)."),
        op!("realms.update", "system", "PATCH" "/api/v1/realms/{id}", Write, Input::Body(schema::<api::RealmIn>), "Renames a realm, changes its description, or sets whether it reads commons (reads_commons).", choices = &[("id", "realms.list")]),
        op!("realms.delete", "system", "DELETE" "/api/v1/realms/{id}", Destructive, Input::None, "Deletes a realm that owns nothing yet.", choices = &[("id", "realms.list")]),
        op!("workers.realms", "fleet", "PUT" "/api/v1/workers/{id}/realms", Write, Input::Body(schema::<api::RealmsIn>), "Sets which realms a worker runs sessions for (empty: all; one: dedicated to it).", choices = &[("id", "workers.list")]),
        op!("connections.realms", "connections", "PUT" "/api/v1/connections/{id}/realms", Write, Input::Body(schema::<api::RealmsIn>), "Sets which realms' sessions may use a model connection (empty: all).", choices = &[("id", "connections.list")]),
        op!("views.list", "views", "GET" "/api/v1/views", Read, Input::Query(schema::<api::InRealm>), "The views a realm shows in its menu: root's and its own."),
        op!("views.act", "views", "POST" "/api/v1/views/{name}/act", Write, Input::Body(schema::<api::ViewAct>), "Runs a view's action on a record (sets its fields, and/or starts its template with the record as input, sessions labelled record:<id>), or, with item, on an item of a messages, calendar or gallery view (calls the owning app's operation, or starts the template).", choices = &[("name", "views.list")]),
        op!("views.items", "views", "GET" "/api/v1/views/{name}/items", Read, Input::Query(schema::<api::ViewItemsQuery>), "The items a messages, calendar or gallery view shows: newest first (before a time), or from a time on, oldest first.", choices = &[("name", "views.list")]),
        op!("series.list", "series", "GET" "/api/v1/series", Read, Input::Query(schema::<api::InRealm>), "Series: numbers over time in a realm (revenue, uptime pings, costs), each with its unit, dimensions and latest point; plus model_spend, model_tokens and model_calls, computed from model usage."),
        op!("series.get", "series", "GET" "/api/v1/series/{name}", Read, Input::Query(schema::<api::InRealm>), "A series.", choices = &[("name", "series.list")]),
        op!("series.create", "series", "POST" "/api/v1/series", Write, Input::Body(schema::<api::SeriesIn>), "Creates a series: name, unit, description, dimensions (names points may carry, e.g. service), retention_days."),
        op!("series.update", "series", "PATCH" "/api/v1/series/{name}", Write, Input::Body(schema::<api::SeriesIn>), "Changes a series' unit, description, dimensions or retention.", choices = &[("name", "series.list")]),
        op!("series.delete", "series", "DELETE" "/api/v1/series/{name}", Destructive, Input::None, "Deletes a series and its points.", choices = &[("name", "series.list")]),
        op!("series.record", "series", "POST" "/api/v1/series/{name}/points", Write, Input::Body(schema::<api::Point>), "Adds a point: value, ts (default now), dims. Thresholds on the series are checked at once.", choices = &[("name", "series.list")]),
        op!("series.query", "series", "POST" "/api/v1/series/{name}/query", Read, Input::Body(schema::<api::SeriesQuery>), "Sums, averages, counts, the minimum, maximum or last value of a series over a window (minutes, or from/to), optionally per dimension value (group_by) or per hour or day (bucket), for points matching where.", choices = &[("name", "series.list")]),
        op!("grants.list", "system", "GET" "/api/v1/grants", Read, Input::Query(schema::<api::InRealm>), "Grants: how data may flow between realms (a realm reading or adding to another's collection, taking a copy, reading its knowledge, or calling its operations), with how often each was used. realm: those given by or to one realm."),
        op!("grants.get", "system", "GET" "/api/v1/grants/{id}", Read, Input::None, "A grant.", choices = &[("id", "grants.list")]),
        op!("grants.create", "system", "POST" "/api/v1/grants", Write, Input::Body(schema::<api::GrantIn>), "Lets realm to_realm use realm from_realm's data. kind collection: mode read (scope {collection, where?: a filter limiting it to some records}), append (add records) or snapshot (a copy of the records now, into to_realm's collection of the same name); kind memex: mode read (scope {kinds?}); kind operations: mode call (scope {methods: [\"memex.mail.draft\", …]}). expires_at ends it."),
        op!("grants.revoke", "system", "DELETE" "/api/v1/grants/{id}", Destructive, Input::None, "Revokes a grant (or a request for one): nothing more flows; what already crossed stays.", choices = &[("id", "grants.list")]),
        op!("grants.uses", "system", "GET" "/api/v1/grants/{id}/uses", Read, Input::None, "Each use of a grant: when, by whom, what.", choices = &[("id", "grants.list")]),
        op!("realms.list", "system", "GET" "/api/v1/realms", Read, Input::None, "Realms: root (carbon6's administration) and home (your sessions, knowledge and data); each owns its own sessions, action items, memory notes, triggers, secrets and knowledge."),
        op!("realms.elsewhere", "system", "GET" "/api/v1/elsewhere", Read, Input::None, "How many things wait on you in each realm (open decisions and approvals): counts only, for switching realm."),
        op!("sessions.locate", "sessions", "GET" "/api/v1/locate/sessions/{id}", Read, Input::None, "The realm a session is in, to open it there."),
        op!("system.version", "system", "GET" "/api/v1/version", Read, Input::None, "The version this head runs: a release (0.3.11) or an edge build of main (0.3.11+git<commit time>.<commit>)."),
        op!("system.activity", "system", "GET" "/api/v1/system/activity", Read, Input::None, "What's going on now: model calls in flight and the last 200 (what each is for, connection, model, state, tokens, the latest line of its answer; system.activity.call has the whole), sessions running, each connector's sync (schedule, the round going on or the last, each account's state) and the memex's work. Kept in memory only."),
        op!("system.activity.counts", "system", "GET" "/api/v1/system/activity/counts", Read, Input::None, "How many model calls are in flight, connector syncs under way and sessions running."),
        op!("system.activity.call", "system", "GET" "/api/v1/system/activity/calls/{id}", Read, Input::None, "One model call from the activity panel, whole: its prompt, thinking and answer so far."),
        op!("system.updates", "system", "GET" "/api/v1/system/updates", Read, Input::None, "carbon6 releases found on GitHub and how far each machine (the head, each worker) is in updating to one: versions, states, how each was installed."),
        op!("system.updates.check", "system", "POST" "/api/v1/system/updates/check", Write, Input::None, "Looks at GitHub for a newer carbon6 release now."),
        op!("system.updates.apply", "system", "POST" "/api/v1/system/updates/{version}/apply", Write, Input::None, "Updates carbon6 to a release found: the head first, then each worker once its sessions are idle."),
        op!("system.updates.skip", "system", "POST" "/api/v1/system/updates/{version}/skip", Write, Input::None, "Never updates to that release."),
        op!("system.updates.settings.get", "system", "GET" "/api/v1/settings/updates", Read, Input::None, "When carbon6 updates itself: off, notify, approve (a decision for each release) or auto (within a window, up to patch or minor)."),
        op!("system.updates.settings.put", "system", "PUT" "/api/v1/settings/updates", Write, Input::Body(schema::<api::UpdateSettings>), "Sets when carbon6 updates itself."),
        op!("system.upgrades", "system", "GET" "/api/v1/system/upgrades", Read, Input::None, "Upgrades of carbon6's databases (the head's and the memex's), newest first: versions, what changed, and where the backup is."),
        op!("auth.tokens.list", "account", "GET" "/api/v1/auth/tokens", Read, Input::None, "API tokens (for the c6 CLI and scripts)."),
        op!("auth.tokens.create", "account", "POST" "/api/v1/auth/tokens", Secret, Input::Body(schema::<api::CreateApiToken>), "Creates an API token for the c6 CLI or a script.", secret = &["token"]),
        op!("auth.tokens.delete", "account", "DELETE" "/api/v1/auth/tokens/{id}", Destructive, Input::None, "Deletes an API token (it stops working).", choices = &[("id", "auth.tokens.list")]),

        // The memex, through its methods.
        op!(memex "memex.review.list", "knowledge", "memex.review", Read, r#"{"type":"object","properties":{"limit":{"type":"integer"}}}"#, "Suggestions waiting for you: people who may be the same (merge), faces to name."),
        op!(memex "memex.review.decide", "knowledge", "memex.review_decide", Write, r#"{"type":"object","required":["id","accept"],"properties":{"id":{"type":"integer"},"accept":{"type":"boolean","description":"true merges (or names the face); false dismisses"},"name":{"type":"string","description":"for a face: who it is"}}}"#, "Accepts or dismisses a suggestion."),
        op!(memex_typed "memex.ingest", "knowledge", "memex.ingest", Write, api::MemexIngest, "Adds items of any supported type (mail as message/rfc822, documents as PDF, Word, Markdown, text, HTML): each typed by its MIME type and handled by that type's handler. Per item: created, updated, duplicate or rejected with the reason. Imported items are never synced back."),
        op!(memex "memex.types", "knowledge", "memex.types", Read, r#"{"type":"object"}"#, "The types memex.ingest takes: each handler's MIME types, attribute schema, how it recognizes duplicates, size limit and what it creates."),
        op!(memex "memex.relations.cards", "knowledge", "memex.relations.cards", Read, r#"{"type":"object","properties":{"id":{"type":"string","description":"a person's or organization's thing id or key (person:<address>, org:<domain>)"},"address":{"type":"string","description":"or an email address: its person's and organization's cards"}}}"#, "Who a person or organization is to you, as the model wrote it from your mail's history with them (docs/relationships.md): relationship, role, the story so far, what's open, how you deal with them."),
        op!(memex "memex.relations.write", "knowledge", "memex.relations.write", Write, r#"{"type":"object","required":["key"],"properties":{"key":{"type":"string","description":"person:<address> or org:<domain>"},"name":{"type":"string"}}}"#, "Writes (or grows) a person's or organization's card now, reading up to 5 of their unread threads first (model calls)."),
        op!(memex "memex.commitments", "knowledge", "memex.commitments", Read, r#"{"type":"object","properties":{"status":{"type":"string","enum":["open","done","dropped"]}}}"#, "Commitments read from mail and sessions: who owes what, by when, and the conversation it was read from (target, for mail)."),
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
        op!(memex "mail.feeds", "mail", "memex.feeds", Read, r#"{"type":"object"}"#, "Which of mail, calendar and contacts a realm has: an app feeds it, or it holds imported data."),
        op!(memex "mail.threads", "mail", "memex.mail.threads", Read, r#"{"type":"object","properties":{"account":{"type":"string","description":"an account id or address; empty: all"},"view":{"type":"string","description":"inbox (default), needs_reply, waiting, starred, sent, drafts, all, spam, trash, or a Gmail label"},"before":{"type":"integer","description":"ms, for the next page"},"limit":{"type":"integer"}}}"#, "Mail threads in a view (to search mail, use memex.search with kinds email)."),
        op!("slack.get", "chat", "GET" "/api/v1/slack", Read, Input::Query(schema::<api::InRealm>), "A realm's Slack: whether the slack app is installed, your Slack app's client and the manifest to make one from, and each connected workspace with how far its sync is."),
        op!("slack.client.put", "chat", "PUT" "/api/v1/slack/client", Write, Input::Body(schema::<api::SlackClientIn>), "Sets the realm's Slack app client (ID and secret, from the app you made from the manifest)."),
        op!("slack.install", "chat", "POST" "/api/v1/slack/install", Write, Input::Query(schema::<api::InRealm>), "Asks you to install the slack app in the realm (a decision to approve)."),
        op!("slack.connect", "chat", "POST" "/api/v1/slack/connect", Write, Input::Query(schema::<api::InRealm>), "Where to send you to connect a Slack workspace (Slack's sign-in)."),
        op!("slack.sync", "chat", "POST" "/api/v1/slack/sync", Write, Input::Query(schema::<api::InRealm>), "Runs the realm's Slack sync now."),
        op!("slack.accounts.remove", "chat", "DELETE" "/api/v1/slack/accounts/{id}", Destructive, Input::Query(schema::<api::InRealm>), "Disconnects a Slack workspace: its sync stops."),
        op!(memex "chat.post", "chat", "memex.chat.post", Write, r#"{"type":"object","required":["workspace","channel","text"],"properties":{"workspace":{"type":"string"},"channel":{"type":"string"},"text":{"type":"string"},"thread_ts":{"type":"string"},"broadcast":{"type":"boolean"}}}"#, "Posts a message as you in a Slack conversation, or a thread (thread_ts; broadcast: also to the channel)."),
        op!(memex "chat.edit", "chat", "memex.chat.edit", Write, r#"{"type":"object","required":["workspace","channel","ts","text"],"properties":{"workspace":{"type":"string"},"channel":{"type":"string"},"ts":{"type":"string"},"thread_ts":{"type":"string"},"text":{"type":"string"}}}"#, "Changes the text of one of your Slack messages."),
        op!(memex "chat.delete", "chat", "memex.chat.delete", Write, r#"{"type":"object","required":["workspace","channel","ts"],"properties":{"workspace":{"type":"string"},"channel":{"type":"string"},"ts":{"type":"string"}}}"#, "Deletes one of your Slack messages."),
        op!(memex "chat.unread", "chat", "memex.chat.unread", Write, r#"{"type":"object","required":["workspace","channel","ts"],"properties":{"workspace":{"type":"string"},"channel":{"type":"string"},"ts":{"type":"string"}}}"#, "Marks a Slack conversation unread from a message on."),
        op!(memex "chat.thread_read", "chat", "memex.chat.thread_read", Write, r#"{"type":"object","required":["workspace","channel","thread_ts","ts"],"properties":{"workspace":{"type":"string"},"channel":{"type":"string"},"thread_ts":{"type":"string"},"ts":{"type":"string"}}}"#, "Notes a thread read up to a reply."),
        op!(memex "chat.prefs", "chat", "memex.chat.prefs", Write, r#"{"type":"object","required":["workspace","channel"],"properties":{"workspace":{"type":"string"},"channel":{"type":"string"},"starred":{"type":"boolean"},"muted":{"type":"boolean"},"hidden":{"type":"boolean"},"section":{"type":"string"}}}"#, "Stars, mutes, hides or moves a chat conversation to a section."),
        op!(memex "chat.sections", "chat", "memex.chat.sections", Write, r#"{"type":"object","properties":{"workspace":{"type":"string"},"names":{"type":"array","items":{"type":"string"}}}}"#, "Your chat sidebar's sections, in order (names sets them for a workspace)."),
        op!(memex "chat.save", "chat", "memex.chat.save", Write, r#"{"type":"object","required":["id"],"properties":{"id":{"type":"string"},"done":{"type":"boolean"},"remove":{"type":"boolean"}}}"#, "Saves a chat message for later (done: marks it done; remove: unsaves it)."),
        op!(memex "chat.people", "chat", "memex.chat.people", Read, r#"{"type":"object","properties":{"workspace":{"type":"string"}}}"#, "The people of your chat workspaces: names, avatars, titles, status."),
        op!(memex "chat.unreads", "chat", "memex.chat.unreads", Read, r#"{"type":"object","properties":{}}"#, "Chat conversations with unread messages, and those messages."),
        op!(memex "chat.threads", "chat", "memex.chat.threads", Read, r#"{"type":"object","properties":{"limit":{"type":"integer"}}}"#, "Chat threads you're in, the most recently active first, with new replies."),
        op!(memex "chat.activity", "chat", "memex.chat.activity", Read, r#"{"type":"object","properties":{"limit":{"type":"integer"}}}"#, "Chat messages that mention you, and yours that got reactions, newest first."),
        op!(memex "chat.saved", "chat", "memex.chat.saved", Read, r#"{"type":"object","properties":{"done":{"type":"boolean"}}}"#, "Chat messages saved for later (done: those marked done)."),
        op!(memex "chat.search", "chat", "memex.chat.search", Read, r#"{"type":"object","properties":{"q":{"type":"string"},"workspace":{"type":"string"},"in":{"type":"string"},"from":{"type":"string"},"has":{"type":"string","enum":["link","file","reaction"]},"before":{"type":"integer"},"after":{"type":"integer"},"thread":{"type":"boolean"},"limit":{"type":"integer"}}}"#, "Searches chat messages by words, conversation (in), sender (from; me for yours), what they have, and when."),
        op!(memex "chat.react", "chat", "memex.chat.react", Write, r#"{"type":"object","required":["workspace","channel","ts","name"],"properties":{"workspace":{"type":"string"},"channel":{"type":"string"},"ts":{"type":"string"},"name":{"type":"string"},"remove":{"type":"boolean"}}}"#, "Adds (or removes) a reaction to a Slack message."),
        op!(memex "chat.mark_read", "chat", "memex.chat.mark_read", Write, r#"{"type":"object","required":["workspace","channel","ts"],"properties":{"workspace":{"type":"string"},"channel":{"type":"string"},"ts":{"type":"string"}}}"#, "Marks a Slack conversation read up to a message."),
        op!(memex "chat.channels", "chat", "memex.chat.channels", Read, r#"{"type":"object","properties":{}}"#, "Chat conversations (Slack channels, DMs and groups), each workspace's, the most recently active first, with unread and mention counts and your prefs."),
        op!(memex "chat.messages", "chat", "memex.chat.messages", Read, r#"{"type":"object","required":["workspace","channel"],"properties":{"workspace":{"type":"string"},"channel":{"type":"string"},"before":{"type":"integer"},"around":{"type":"string"},"limit":{"type":"integer"}}}"#, "A conversation's messages, newest first, without thread replies (each first message says how many it has); before a time, or around a message (its ts)."),
        op!(memex "chat.thread", "chat", "memex.chat.thread", Read, r#"{"type":"object","required":["workspace","channel","thread_ts"],"properties":{"workspace":{"type":"string"},"channel":{"type":"string"},"thread_ts":{"type":"string"}}}"#, "A thread: its first message and the replies, oldest first."),
        op!(memex "mail.thread", "mail", "memex.mail.thread", Read, r#"{"type":"object","required":["thread_id"],"properties":{"account":{"type":"string"},"thread_id":{"type":"string"}}}"#, "A mail thread with its messages."),
        op!(memex "mail.triage.list", "mail", "memex.triage.list", Read, r#"{"type":"object","properties":{"category":{"type":"string","description":"needs_you, first_contact, fyi, newsletter, notification, pitch, noise or yours"},"kind":{"type":"string","description":"mail or chat"},"unseen":{"type":"boolean","description":"only those with news since you marked them seen"},"limit":{"type":"integer"}}}"#, "Recent mail threads and chat conversations as sorted for the brief (docs/brief.md): category, why, by what (rule, the sequence marks, your mark, the model, you), newest first."),
        op!(memex "mail.triage.get", "mail", "memex.triage.get", Read, r#"{"type":"object","required":["target"],"properties":{"target":{"type":"string","description":"thread:<account>:<thread>, or chat:<workspace>:<channel>[:<thread ts>]"}}}"#, "How one mail thread or chat conversation was sorted, or null."),
        op!(memex "mail.triage.status", "mail", "memex.triage.status", Read, r#"{"type":"object"}"#, "Whether sorting for the brief can go on: a model chosen (Settings → Memex), model calls left today, and how many conversations wait to be sorted (up to 20); and how many recent mail threads and chat conversations were sorted, and how many of those by the model."),
        op!(memex "mail.triage.seen", "mail", "memex.triage.seen", Write, r#"{"type":"object","required":["target"],"properties":{"target":{"type":"string","description":"thread:<account>:<thread>, or chat:<workspace>:<channel>[:<thread ts>]"}}}"#, "Marks a conversation seen: it leaves the brief's FYI until something new comes in it."),
        op!(memex "mail.triage.counts", "mail", "memex.triage.counts", Read, r#"{"type":"object"}"#, "How many recent conversations are in each category."),
        op!(memex "mail.triage.set", "mail", "memex.triage.set", Write, r#"{"type":"object","required":["target","category"],"properties":{"target":{"type":"string","description":"thread:<account>:<thread>, or chat:<workspace>:<channel>[:<thread ts>]"},"category":{"type":"string"},"note":{"type":"string","description":"why: the sorting learns from it"}}}"#, "Sorts a conversation yourself; the note becomes one of the model's examples."),
        op!(memex "mail.triage.mark", "mail", "memex.triage.mark", Write, r#"{"type":"object","required":["target","mark"],"properties":{"target":{"type":"string","description":"thread:<account>:<thread>, or chat:<workspace>:<channel>[:<thread ts>]"},"mark":{"type":"string","enum":["pitch","known"]},"domain":{"type":"boolean","description":"the sender's whole domain"}}}"#, "Marks a mail thread's sender: a cold pitch (everything they send is), or someone you know (nothing they send is)."),
        op!(memex "mail.triage.marks", "mail", "memex.triage.marks", Read, r#"{"type":"object"}"#, "The senders you marked."),
        op!(memex "mail.triage.unmark", "mail", "memex.triage.unmark", Write, r#"{"type":"object","required":["pattern"],"properties":{"pattern":{"type":"string","description":"an address, or @domain"}}}"#, "Takes a sender mark back; what it sorted is sorted again."),
        op!(memex "mail.requests.list", "mail", "memex.requests.list", Read, r#"{"type":"object","properties":{"status":{"type":"string","description":"open (the default: with snoozed ones whose time came), snoozed, done or dropped"},"limit":{"type":"integer"}}}"#, "What people need from you (docs/brief.md §5): one request per ask, with the mail threads and chat conversations it came from, the soonest due first."),
        op!(memex "mail.requests.get", "mail", "memex.requests.get", Read, r#"{"type":"object","required":["id"],"properties":{"id":{"type":"integer"}}}"#, "One request."),
        op!(memex "mail.requests.set", "mail", "memex.requests.set", Write, r#"{"type":"object","required":["id","status"],"properties":{"id":{"type":"integer"},"status":{"type":"string","enum":["open","snoozed","done","dropped"]},"until":{"type":"integer","description":"snoozed: when it comes back (ms)"}}}"#, "Settles a request: done, dropped, snoozed until a time, or open again."),
        op!(memex "mail.requests.merge", "mail", "memex.requests.merge", Write, r#"{"type":"object","required":["into","from"],"properties":{"into":{"type":"integer"},"from":{"type":"integer"}}}"#, "Two requests that are one ask: from's conversations join into's."),
        op!(memex "mail.requests.draft", "mail", "memex.requests.draft", Write, r#"{"type":"object","required":["id"],"properties":{"id":{"type":"integer"}}}"#, "Drafts (again) a request's reply now (one model call)."),
        op!(memex "mail.requests.reply_to", "mail", "memex.requests.reply_to", Read, r#"{"type":"object","required":["target"],"properties":{"target":{"type":"string"}}}"#, "Who and what a reply in a request's conversation goes to: the mail it answers (to, subject), or the chat conversation or thread."),
        op!(memex "mail.requests.send", "mail", "memex.requests.send", Destructive, r#"{"type":"object","required":["id","target","text"],"properties":{"id":{"type":"integer"},"target":{"type":"string","description":"one of the request's conversations"},"text":{"type":"string","description":"the reply, its choices made"}}}"#, "Sends your reply to a request in one of its conversations (mail after its undo window; Slack at once); the request is done."),
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
        op!(memex "contacts.create_many", "mail", "memex.contacts.create_many", Write, r#"{"type":"object","required":["people"],"properties":{"account":{"type":"string"},"people":{"type":"array","items":{"type":"object","required":["email"],"properties":{"email":{"type":"string"},"name":{"type":"string"}}}}}}"#, "Adds several contacts at once (in Google Contacts, in the carbon6 group), in one run; those already contacts are left out. Returns the addresses added."),
        op!(memex "photos.list", "photos", "memex.media.list", Read, r#"{"type":"object","properties":{"q":{"type":"string","description":"a description"},"about":{"type":"string","description":"a person, place or event (id or label)"},"before":{"type":"integer"},"limit":{"type":"integer"}}}"#, "Photos and videos, newest first, or matching a description, person, place or event."),
        op!(memex "photos.get", "photos", "memex.media.get", Read, r#"{"type":"object","required":["id"],"properties":{"id":{"type":"string"}}}"#, "A photo or video: caption, date, place, faces, things."),
        op!(memex "photos.facets", "photos", "memex.media.facets", Read, r#"{"type":"object"}"#, "People, places and events in the photos, most frequent first."),
    ]);
    v
}

/// The schema of one entry of a system list (docs/os.md §12.2).
pub fn list_schema(key: &str) -> Option<Value> {
    Some(match key {
        "skills" => schema::<api::SkillDef>(),
        "agents" => schema::<api::AgentDef>(),
        "commands" => schema::<api::CommandDef>(),
        "hooks" => schema::<api::Hook>(),
        "mcp_servers" => {
            json!({"type": "object", "required": ["name"], "additionalProperties": true, "properties": {"name": {"type": "string"}}})
        }
        "templates" => schema::<api::Template>(),
        "workflows" => schema::<api::Workflow>(),
        "policies" => schema::<api::Policy>(),
        "routes" => schema::<api::Route>(),
        "prices" => schema::<api::Price>(),
        "budgets" => schema::<api::Budget>(),
        "channels" => schema::<api::Channel>(),
        "hosts" => schema::<api::Host>(),
        "memex_folders" => schema::<api::MemexFolder>(),
        "memex_scopes" => schema::<api::MemexScope>(),
        "views" => schema::<api::View>(),
        "claim_policies" => schema::<api::ClaimPolicy>(),
        "vocabulary" => schema::<api::VocabularyEntry>(),
        "prompts" => json!({"type": "object", "required": ["name", "body"], "properties": {
            "name": {"type": "string"}, "description": {"type": "string"}, "body": {"type": "string"}
        }}),
        "memory" => json!({"type": "object", "required": ["text"], "properties": {
            "scope": {"type": "string", "description": "One label, or empty: every session"},
            "text": {"type": "string", "maxLength": 2000},
            "session_id": {"type": ["string", "null"], "description": "The session that wrote it"}
        }}),
        _ => return None,
    })
}

/// Routes the assistant does not call, and why (the parity test allows
/// these).
pub const NOT_OPERATIONS: &[(&str, &str, &str, Scope)] = &[
    ("GET", "/api/v1/live", "a live WebSocket for the UI", Scope::Realm),
    ("GET", "/api/v1/head/fs/list", "the head's own files, for the UI's file picker only", Scope::Admin),
    ("GET", "/api/v1/head/fs/read", "the head's own files, for the UI's file picker only", Scope::Admin),
    ("GET", "/api/v1/automations", "the old name of /api/v1/triggers", Scope::Realm),
    ("POST", "/api/v1/automations", "the old name of /api/v1/triggers", Scope::Realm),
    ("PATCH", "/api/v1/automations/{id}", "the old name of /api/v1/triggers", Scope::Realm),
    ("DELETE", "/api/v1/automations/{id}", "the old name of /api/v1/triggers", Scope::Realm),
    ("POST", "/api/v1/automations/{id}/run", "the old name of /api/v1/triggers", Scope::Realm),
    ("GET", "/api/v1/automations/{id}/runs", "the old name of /api/v1/triggers", Scope::Realm),
    ("POST", "/api/v1/automations/{id}/rotate", "the old name of /api/v1/triggers", Scope::Realm),
    ("GET", "/api/v1/sessions/{id}/stream", "a live WebSocket for the UI", Scope::Realm),
    ("GET", "/api/v1/workers/{id}/terminals/{tid}/attach", "a terminal WebSocket", Scope::Realm),
    ("GET", "/api/v1/sessions/{id}/attachments/{seq}/{index}", "a file download", Scope::Realm),
    ("GET", "/api/v1/sessions/{id}/outputs/{seq}", "a file download", Scope::Realm),
    ("GET", "/api/v1/sessions/{id}/export", "a file download", Scope::Realm),
    ("GET", "/api/v1/packages/{name}/{version}/files/{*path}", "a file download", Scope::Admin),
    ("GET", "/api/v1/memex/items/{id}/content", "a file download", Scope::Realm),
    ("GET", "/api/v1/memex/media/{id}/thumb", "an image", Scope::Realm),
    ("GET", "/api/v1/memex/face/{id}", "an image", Scope::Realm),
    ("GET", "/api/v1/views/{name}/items/{id}/thumb", "an image", Scope::Realm),
    ("GET", "/api/v1/mail/attachment", "a file download", Scope::Realm),
    ("POST", "/api/v1/memex/rpc", "the memex's methods are operations of their own", Scope::Realm),
    ("POST", "/api/v1/webhooks/{id}/{token}", "called by other systems, with its own token", Scope::Device),
    ("GET", "/callback", "the ChatGPT sign-in redirect", Scope::Device),
    ("GET", "/auth/google/callback", "the Google sign-in redirect", Scope::Device),
    ("GET", "/auth/oauth/callback", "the sign-in redirect of apps' OAuth providers", Scope::Device),
    ("GET", "/ca.pem", "a certificate download", Scope::Device),
    ("GET", "/ca.crt", "a certificate download", Scope::Device),
    ("GET", "/ca.mobileconfig", "a certificate download", Scope::Device),
    ("GET", "/approve/{token}", "a signed approval page", Scope::Device),
    ("POST", "/approve/{token}", "a signed approval page", Scope::Device),
    ("POST", "/api/v1/auth/link", "signing in a browser", Scope::Device),
    ("POST", "/api/v1/auth/code", "signing in a browser", Scope::Device),
    ("POST", "/api/v1/auth/logout", "signing out this browser", Scope::Device),
    ("POST", "/api/v1/auth/passkeys/login/begin", "a passkey ceremony in the browser", Scope::Device),
    ("POST", "/api/v1/auth/passkeys/login/finish", "a passkey ceremony in the browser", Scope::Device),
    ("POST", "/api/v1/auth/passkeys/register/begin", "a passkey ceremony in the browser", Scope::Admin),
    ("POST", "/api/v1/auth/passkeys/register/finish", "a passkey ceremony in the browser", Scope::Admin),
    ("POST", "/api/v1/assistant", "starts an assistant conversation (this one)", Scope::Realm),
    ("GET", "/api/v1/catalog", "this catalog", Scope::Shared),
    ("GET", "/api/v1/openapi.json", "this catalog, as OpenAPI", Scope::Shared),
    ("POST", "/api/v1/catalog/call", "this catalog", Scope::Admin),
    ("POST", "/api/v1/sessions/{id}/answers/{call_id}", "answers the assistant's questions (the UI's forms)", Scope::Realm),
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
        let excepted = |m: &str, p: &str| NOT_OPERATIONS.iter().any(|(em, ep, ..)| *em == m && *ep == p);
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
        // Every operation has a scope (docs/realms-ux.md §3).
        let unplaced: Vec<&str> = ops().iter().map(|o| o.name).filter(|n| scope(n).is_none()).collect();
        assert!(unplaced.is_empty(), "operations without a scope (place them in catalog::scope): {unplaced:?}");
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

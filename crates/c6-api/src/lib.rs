//! JSON types for the head's `/api/v1`, shared by `c6h` and the web UI.
//! Must stay wasm-compatible: no tokio, no std-only dependencies.

pub mod labels;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Timestamps are Unix milliseconds.
pub type UnixMs = i64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Online,
    Offline,
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Worker {
    pub id: Uuid,
    pub name: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: Option<String>,
    pub status: WorkerStatus,
    pub enrolled_at: UnixMs,
    pub last_seen: Option<UnixMs>,
    pub connected_since: Option<UnixMs>,
    pub rtt_us: Option<u32>,
    /// Optional features of a connected worker, e.g. `claude_code`.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CreateEnrollment {
    /// Display name for the worker; defaults to its hostname.
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Enrollment {
    pub token: String,
    pub expires_at: UnixMs,
}

/// Messages on `WS /api/v1/live`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LiveEvent {
    /// Full fleet snapshot; sent on connect and whenever anything changes.
    Workers { workers: Vec<Worker> },
    /// Full session list snapshot (not archived); same cadence.
    Sessions { sessions: Vec<Session> },
}

// --- LLM connections --------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub name: String,
    /// `openai_compat` for now.
    pub kind: String,
    pub base_url: String,
    pub default_model: Option<String>,
    pub has_api_key: bool,
    /// The connection passes a reasoning effort on to the model.
    #[serde(default)]
    pub supports_effort: bool,
    pub created_at: UnixMs,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CreateConnection {
    /// `openai_compat` (default), `openai`, `anthropic`, or `claude_code`
    /// (runs the worker's own signed-in Claude Code). ChatGPT
    /// connections are created by signing in instead.
    #[serde(default)]
    pub kind: Option<String>,
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub default_model: Option<String>,
}

// --- Sessions ---------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Created on the head; the worker has not started it yet.
    Starting,
    Idle,
    Running,
    /// The worker could not start it; see `Session::error`.
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub worker_id: Uuid,
    pub title: String,
    pub cwd: String,
    pub connection_id: String,
    pub model: String,
    pub status: SessionStatus,
    pub error: Option<String>,
    pub effort: Option<String>,
    /// Subagent sessions: the parent session and the Task call that ran it.
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub parent_call_id: Option<String>,
    /// Side chats: the parent event they branch from.
    #[serde(default)]
    pub fork_seq: Option<u64>,
    pub created_at: UnixMs,
    pub updated_at: UnixMs,
    pub last_seq: u64,
    /// Labels you attached.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Automatic labels: worker:, repo:, folder: (absolute and ~/), model:,
    /// host:.
    #[serde(default)]
    pub auto_labels: Vec<String>,
    /// The machine it works on over SSH (through its worker), if not the
    /// worker itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
}

/// A side chat: a new session that branches from `anchor_seq` of a session
/// (with everything up to there as context) on the same worker.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateSideChat {
    pub anchor_seq: u64,
    pub connection_id: String,
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
    pub prompt: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateSession {
    pub worker_id: Uuid,
    pub cwd: String,
    pub connection_id: String,
    pub model: String,
    pub title: Option<String>,
    /// First message, sent right after creation.
    pub prompt: Option<String>,
    /// Images and text files sent with the first message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentUpload>,
    /// Your labels for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Work on another machine over SSH, through the worker: `user@host`
    /// or an alias from the worker's ~/.ssh/config. `cwd` is then a folder
    /// there (default: the home folder).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
    /// Reasoning effort: low, medium, high, xhigh, max. Default: per model.
    #[serde(default)]
    pub effort: Option<String>,
    /// Run in a new git worktree on a new `c6/…` branch.
    #[serde(default)]
    pub worktree: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartChatGptSignIn {
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatGptSignIn {
    /// Open this in a browser and approve.
    pub authorize_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompleteChatGptSignIn {
    /// The address the browser landed on after approving.
    pub callback_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendMessage {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentUpload>,
    /// Switch the session to this connection and model from this message
    /// on. Both or neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttachmentInfo {
    pub name: String,
    pub mime: String,
    pub size: u64,
}

/// An image or file sent with a message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttachmentUpload {
    pub name: String,
    /// image/png, image/jpeg, image/gif, image/webp, or a text type.
    pub mime: String,
    /// Base64.
    pub data: String,
}

/// `POST /api/v1/sessions/{id}/rewind`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RewindRequest {
    /// The user message to rewind to before (its event seq).
    pub to_seq: u64,
    /// Also put files back as they were before that message.
    #[serde(default)]
    pub restore_files: bool,
}

/// Input accepted by the head but not yet recorded by the worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueuedInput {
    pub command_id: String,
    pub text: String,
    pub created_at: UnixMs,
    /// Set if the worker refused it.
    pub rejected: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionDetail {
    pub session: Session,
    pub worker_name: String,
    pub worker_online: bool,
    pub queued: Vec<QueuedInput>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub ts: UnixMs,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    Started {
        connection_id: String,
        model: String,
        cwd: String,
        system_prompt: String,
        tools: Vec<String>,
        #[serde(default)]
        worktree_branch: String,
        #[serde(default)]
        repo_root: String,
        #[serde(default)]
        context_window: u64,
        /// Reasoning effort; empty for the model's default.
        #[serde(default)]
        effort: String,
    },
    User {
        command_id: String,
        text: String,
        /// Images and files sent with it, and files it mentions (@path).
        /// The bytes: `GET /api/v1/sessions/{id}/attachments/{seq}/{index}`.
        #[serde(default)]
        attachments: Vec<AttachmentInfo>,
        /// The slash command it invoked, if any.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        command: String,
        /// What the model got instead of the text, when a command expanded it.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        prompt: String,
        /// Why a UserPromptSubmit hook blocked it (the model never saw it).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        blocked: String,
    },
    TurnStarted {
        turn: u64,
    },
    Assistant {
        turn: u64,
        blocks: Vec<ContentBlock>,
        stop_reason: String,
    },
    ToolResult {
        call_id: String,
        tool: String,
        output: String,
        is_error: bool,
        truncated: bool,
        duration_ms: u64,
    },
    Usage {
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        reasoning_tokens: u64,
    },
    TurnEnded {
        turn: u64,
        reason: TurnEndReason,
    },
    Error {
        message: String,
    },
    /// Earlier conversation was replaced, for the model, by this summary.
    Compaction {
        summary: String,
        tokens_before: u64,
    },
    /// A snapshot of the working directory, before a turn (for rewinding).
    Checkpoint {
        #[serde(default)]
        error: String,
    },
    /// The conversation was rewound to before event `to_seq`; the events in
    /// between no longer count.
    Rewound {
        to_seq: u64,
        restored_files: bool,
        #[serde(default)]
        restore_error: String,
    },
    /// A background task (shell command or subagent) started or ended.
    BackgroundTask {
        task_id: String,
        /// "shell" or "agent".
        kind: String,
        description: String,
        /// "started", "finished", "stopped" (by the agent),
        /// "stopped_by_user" or "lost" (the worker restarted).
        state: String,
        #[serde(default)]
        exit_code: i32,
        #[serde(default)]
        elapsed_ms: u64,
        #[serde(default)]
        notify: bool,
    },
    /// A deliverable the agent published. Its content:
    /// `GET /api/v1/sessions/{id}/outputs/{seq}` (`?download=1` to save).
    Output {
        name: String,
        title: String,
        mime: String,
        size: u64,
    },
    /// The agent changed a memory note.
    Memory {
        op: String,
        id: String,
        scope: String,
        text: String,
    },
    /// From here on the session runs on this model.
    ModelSwitched {
        connection_id: String,
        model: String,
        #[serde(default)]
        effort: String,
        context_window: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// A tool the provider ran itself (e.g. web search), for display.
    ServerTool {
        label: String,
    },
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    Completed,
    Interrupted,
    Error,
    WorkerRestarted,
    Unknown,
}

/// Messages on `WS /api/v1/sessions/{id}/stream`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamMsg {
    /// A durable event, in seq order with no gaps.
    Event {
        event: Event,
    },
    /// Live-only deltas, superseded by the next durable event.
    Text {
        turn: u64,
        text: String,
    },
    Thinking {
        turn: u64,
        text: String,
    },
    ToolOutput {
        call_id: String,
        text: String,
    },
    Activity {
        running: bool,
    },
    /// Current queued inputs, sent whenever they change.
    Queue {
        queued: Vec<QueuedInput>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

// --- Sign-in ------------------------------------------------------------------

/// `GET /api/v1/auth/status`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuthStatus {
    pub authenticated: bool,
    /// The head requires sign-in (`auth = true`, the default).
    pub required: bool,
    /// Passkeys work at the address this page was loaded from (HTTPS with a
    /// host name, or `localhost`).
    pub passkeys_here: bool,
    /// Passkeys exist for this host name.
    pub has_passkeys: bool,
    /// The head serves HTTPS with its own CA, downloadable at `/ca.pem` and
    /// `/ca.mobileconfig` for other devices to trust.
    #[serde(default)]
    pub head_ca: bool,
}

/// `POST /api/v1/auth/link`: redeem a one-time login link.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RedeemLink {
    pub token: String,
}

/// `POST /api/v1/auth/codes`: a one-time code for signing in another device.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoginCode {
    /// e.g. "K7QM-2XRD"
    pub code: String,
    pub expires_at: UnixMs,
}

/// `POST /api/v1/auth/code`: sign in with a one-time code.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RedeemCode {
    pub code: String,
}

/// `POST /api/v1/auth/passkeys/register/finish`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegisterPasskey {
    pub name: String,
    /// What `navigator.credentials.create` returned, as the UI encodes it.
    pub credential: serde_json::Value,
}

/// `PATCH /api/v1/auth/passkeys/{id}`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RenamePasskey {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Passkey {
    pub id: String,
    pub name: String,
    /// The host name it signs in to.
    pub rp_id: String,
    pub created_at: UnixMs,
    pub last_used_at: Option<UnixMs>,
}

/// An API token (for scripts and the `c6` CLI). The secret is only in the
/// response that creates it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    pub created_at: UnixMs,
    pub last_used_at: Option<UnixMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateApiToken {
    pub name: String,
}

/// A hook: a shell command run on the worker at a point in each session
/// (`GET`/`PUT /api/v1/settings/hooks`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hook {
    /// What it is for, for display.
    #[serde(default)]
    pub name: String,
    /// One of `HOOK_EVENTS`.
    pub event: String,
    /// Tool events: a regular expression over the tool name (Claude Code
    /// names like `Bash` or `Edit` work in every tool set); empty: all.
    #[serde(default)]
    pub matcher: String,
    /// Only sessions in this folder (on the worker) or below; empty: all.
    #[serde(default)]
    pub folder: String,
    pub command: String,
    /// Seconds before it is stopped (default 60).
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    /// Labels a session must all have for this to apply; empty: everywhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applies_to: Vec<String>,
}

fn enabled_default() -> bool {
    true
}

/// A custom slash command (`GET`/`PUT /api/v1/settings/commands`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandDef {
    /// Typed as `/name`; letters, digits, `-`, `_`, `:` and `.`.
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Shown after the name while typing, e.g. `<issue> [priority]`.
    #[serde(default)]
    pub argument_hint: String,
    /// The prompt: `$ARGUMENTS` and `$1`…`$9` take the arguments,
    /// `` !`cmd` `` a shell command's output (run in the session's folder),
    /// `@path` attaches a file.
    pub body: String,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    /// Labels a session must all have for this to apply; empty: everywhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applies_to: Vec<String>,
}

/// A named agent the Task tool can launch (`/api/v1/settings/agents`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentDef {
    pub name: String,
    /// When to use it; the model reads this to pick an agent.
    #[serde(default)]
    pub description: String,
    /// Comma-separated tool names it may use (Claude Code names work with
    /// every model); empty: all.
    #[serde(default)]
    pub tools: String,
    /// `inherit` (or empty), `opus`, `sonnet`, `haiku` or a model id
    /// (Anthropic connections only).
    #[serde(default)]
    pub model: String,
    /// Its instructions (system prompt).
    pub prompt: String,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    /// Labels a session must all have for this to apply; empty: everywhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applies_to: Vec<String>,
}

/// A skill: instructions the agent loads by name (`/api/v1/settings/skills`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub body: String,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    /// Labels a session must all have for this to apply; empty: everywhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applies_to: Vec<String>,
}

/// A secret: an environment variable for sessions' commands. Its value is
/// write-only (`PUT /api/v1/settings/secrets/{name}`); lists never show it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SecretInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub applies_to: Vec<String>,
    #[serde(default)]
    pub updated_at: UnixMs,
}

/// `PUT /api/v1/settings/secrets/{name}`: create or update a secret. A
/// missing `value` keeps the current one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PutSecret {
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub applies_to: Vec<String>,
}

/// Whether `name` can be a secret's name: an environment variable name.
pub fn valid_secret_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && name.len() <= 64
}

/// The web search provider for models whose own provider has none
/// (`GET`/`PUT /api/v1/settings/web-search`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WebSearchSetting {
    /// "brave", "tavily", "searxng"; empty: off.
    #[serde(default)]
    pub provider: String,
    /// SearXNG: the instance's URL.
    #[serde(default)]
    pub url: String,
    /// The secret holding the API key (Brave, Tavily).
    #[serde(default)]
    pub secret: String,
    /// Offer it to every model, even those with a web search of their own.
    #[serde(default)]
    pub always: bool,
}

/// A session template: where and how to start a session (or one per
/// target, a fleet run), and what to ask (`/api/v1/settings/templates`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Worker names. Empty: the online worker (the first, if several);
    /// `*`: every online worker (one session each).
    #[serde(default)]
    pub workers: Vec<String>,
    /// SSH targets, reached through the worker(s): one session each.
    #[serde(default)]
    pub ssh: Vec<String>,
    /// The folder (on the worker, or on the SSH target); empty: a scratch
    /// workspace (or the home folder there).
    #[serde(default)]
    pub cwd: String,
    /// Connection name or id; empty: the first connection.
    #[serde(default)]
    pub connection: String,
    /// Empty: the connection's default model.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub effort: String,
    /// Labels for its sessions (they also get template:<name> and
    /// run:<id>).
    #[serde(default)]
    pub labels: Vec<String>,
    /// The first message. {{input}} is what the run was given (a webhook's
    /// body), {{target}} the worker or host, {{date}} and {{time}} now.
    pub prompt: String,
    /// Off: it cannot be run (by hand, schedule or webhook).
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

/// `POST /api/v1/templates/{name}/run`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunTemplate {
    #[serde(default)]
    pub input: String,
}

/// The sessions a template run started (one per target).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TemplateRun {
    /// Its sessions carry the label run:<run_id>.
    pub run_id: String,
    pub sessions: Vec<Session>,
    /// Targets that could not start, and why.
    #[serde(default)]
    pub errors: Vec<String>,
}

/// An automation: starts a template on a schedule or from a webhook.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Automation {
    pub id: String,
    pub name: String,
    pub template: String,
    /// "schedule" or "webhook".
    pub trigger: String,
    /// Schedules: five cron fields in the head's local time, or @daily etc.
    #[serde(default)]
    pub cron: String,
    /// Schedules: the template's {{input}}.
    #[serde(default)]
    pub input: String,
    pub enabled: bool,
    pub created_at: UnixMs,
    #[serde(default)]
    pub last_run_at: Option<UnixMs>,
    /// Its sessions carry the label run:<last_run_id>.
    #[serde(default)]
    pub last_run_id: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Schedules: when it runs next.
    #[serde(default)]
    pub next_run_at: Option<UnixMs>,
    /// Webhooks: the URL to call, only in the response that creates the
    /// automation or a new token (it holds the token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
}

/// `POST /api/v1/automations` (create) and `PATCH /api/v1/automations/{id}`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AutomationEdit {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub cron: Option<String>,
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// A memory note: kept across sessions, given to those whose labels match
/// its scope (`/api/v1/memories`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    #[serde(default)]
    pub id: String,
    /// A label (e.g. host:db-2, repo:github.com/acme/app, client:acme);
    /// empty: every session.
    #[serde(default)]
    pub scope: String,
    pub text: String,
    /// The session that wrote it, if an agent did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub updated_at: UnixMs,
}

/// The commands built into carbon6: (name, description, argument hint).
/// `compact`, `init` and `review` run on the worker; `clear` and `export`
/// in the UI.
pub const BUILTIN_COMMANDS: [(&str, &str, &str); 5] = [
    ("compact", "Summarize the conversation so far to free context", "[what to focus on]"),
    ("init", "Write an AGENTS.md describing this codebase", ""),
    ("review", "Review the changes (uncommitted, or a branch or PR)", "[what to review]"),
    ("clear", "Start a fresh session in the same folder, with the same model", ""),
    ("export", "Download this conversation as Markdown", ""),
];

/// A command as the composer lists it (`GET /api/v1/commands`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub argument_hint: String,
    /// "built-in" or "custom".
    pub source: String,
}

/// Whether `name` can be a slash command's name.
pub fn valid_command_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || "_-:.".contains(c))
}

pub const HOOK_EVENTS: [&str; 5] = ["PreToolUse", "PostToolUse", "UserPromptSubmit", "Stop", "SessionStart"];

/// `PUT /api/v1/sessions/{id}/labels`: the labels you attach to a session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetLabels {
    pub labels: Vec<String>,
}

/// `GET /api/v1/sessions/{id}/config`: what applies to a session, from the
/// head's current settings and the session's labels.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EffectiveConfig {
    pub labels: Vec<String>,
    pub commands: Vec<Applied>,
    pub agents: Vec<Applied>,
    pub skills: Vec<Applied>,
    pub hooks: Vec<Applied>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub name: String,
    pub applies_to: Vec<String>,
    /// Set when it does not apply here: why (e.g. "needs repo:…", "off",
    /// "replaced by a more specific one").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// A signed-in browser.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuthSession {
    pub id: String,
    /// "passkey", "link" or "code".
    pub method: String,
    pub user_agent: Option<String>,
    pub created_at: UnixMs,
    pub last_seen_at: UnixMs,
    /// The session making this request.
    pub current: bool,
}

// --- Search -------------------------------------------------------------------

/// `GET /api/v1/search`
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchQuery {
    /// Words and "quoted phrases" (all must match, as substrings of 3+
    /// characters); `-word` excludes.
    pub q: String,
    /// Comma-separated entry kinds: user, assistant, thinking, tool_call,
    /// tool_result, error, summary. Empty: all.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub worker: Option<Uuid>,
    /// Sessions to return (default 30).
    /// Comma-separated labels a session must all have (yours or automatic;
    /// `repo:` and `folder:` match path prefixes). Empty: all sessions.
    #[serde(default)]
    pub labels: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    /// Sessions, best match first.
    pub results: Vec<SearchResult>,
    /// The terms that were searched for, for highlighting.
    pub terms: Vec<String>,
    /// Words left out of the search for being shorter than 3 characters.
    #[serde(default)]
    pub ignored: Vec<String>,
    /// More matches exist than were read; narrow the search.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub session: Session,
    pub worker_name: String,
    /// The best few matches in the session.
    pub hits: Vec<SearchHit>,
    /// Matches in the session (among those read).
    pub total_hits: u32,
    /// For a side chat: open its top-level session with these side chats
    /// expanded, instead of the side chat on its own.
    #[serde(default)]
    pub open_in: Option<OpenIn>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpenIn {
    pub session_id: String,
    pub title: String,
    /// Side chat ids to expand, outermost first; the last is the result's.
    pub side_chats: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The transcript element showing the match: `{session}-e{seq}`, or
    /// `call-{id}` for tool output (shown in its call's card).
    pub anchor: String,
    pub seq: u64,
    /// The block within an assistant message.
    pub part: u32,
    pub kind: String,
    pub ts: UnixMs,
    pub snippet: Vec<SnippetPart>,
}

/// A piece of a snippet; `hit` pieces match the query.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnippetPart {
    pub text: String,
    pub hit: bool,
}

/// `GET /api/v1/sessions/{id}/matches?q=`: find in a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionMatches {
    /// In transcript order, side chats included.
    pub matches: Vec<SessionMatch>,
    pub terms: Vec<String>,
    #[serde(default)]
    pub ignored: Vec<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionMatch {
    /// The transcript element showing it (see `SearchHit::anchor`).
    pub anchor: String,
    /// Side chats to expand first, outermost first.
    pub open: Vec<String>,
    pub kind: String,
}

//! JSON types for the head's `/api/v1`, shared by `c6h` and the web UI.
//! Must stay wasm-compatible: no tokio, no std-only dependencies.

pub mod labels;
pub mod prompts;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Timestamps are Unix milliseconds.
pub type UnixMs = i64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Online,
    Offline,
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// "worker", or "memex" for the knowledge service (which runs no
    /// sessions); empty until it first connects.
    #[serde(default)]
    pub role: String,
    /// The realms it may run sessions for (empty: all).
    #[serde(default)]
    pub realms: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CreateEnrollment {
    /// Display name for the worker; defaults to its hostname.
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Enrollment {
    pub token: String,
    pub expires_at: UnixMs,
}

/// Messages on `WS /api/v1/live`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LiveEvent {
    /// Full fleet snapshot; sent on connect and whenever anything changes.
    Workers { workers: Vec<Worker> },
    /// Full session list snapshot (not archived); same cadence.
    Sessions { sessions: Vec<Session> },
}

// --- LLM connections --------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// The realms whose sessions may use it (empty: all).
    #[serde(default)]
    pub realms: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// A tool call is waiting for your approval.
    #[serde(default)]
    pub awaiting_approval: bool,
    /// The realm it runs in (docs/os.md §3).
    #[serde(default = "home_realm")]
    pub realm: String,
}

fn home_realm() -> String {
    "home".to_owned()
}

/// `GET /api/v1/settings/prompts`: a prompt carbon6 gives models, with its
/// default (docs/os.md §4.6).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PromptInfo {
    pub name: String,
    /// What it is for, and the placeholders it may use.
    pub description: String,
    pub body: String,
    pub default: String,
    /// Changed from the default.
    pub edited: bool,
}

/// `PUT /api/v1/settings/prompts/{name}`: an empty body (or the default's
/// text) returns the prompt to following the default.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PromptIn {
    pub body: String,
}

/// A collection: a typed table in a realm (docs/os.md §4.1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Collection {
    pub realm: String,
    pub name: String,
    pub description: String,
    /// A JSON Schema for its records' fields. Strict: fields it does not
    /// name are rejected.
    pub schema: serde_json::Value,
    pub schema_version: i64,
    /// Fields whose values are unique among its records.
    pub keys: Vec<String>,
    /// carbon6's own (configuration): not mirrored into the memex.
    pub system: bool,
    pub records: i64,
    pub created_at: UnixMs,
    pub updated_at: UnixMs,
}

/// `POST /api/v1/collections` and `PATCH /api/v1/collections/{name}`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CollectionIn {
    /// Letters, digits, - and _ (creating only).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// An object schema with properties. A new schema must accept every
    /// record already in the collection.
    #[serde(default)]
    pub schema: Option<serde_json::Value>,
    #[serde(default)]
    pub keys: Option<Vec<String>>,
}

/// A record in a collection.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Record {
    pub id: String,
    pub realm: String,
    pub collection: String,
    pub fields: serde_json::Value,
    /// Memex things and items it links to (keys like client:acme, or item ids).
    pub links: Vec<String>,
    pub labels: Vec<String>,
    pub version: i64,
    /// you, session:<id>, trigger:<name> or token:<name>.
    pub created_by: String,
    pub updated_by: String,
    pub created_at: UnixMs,
    pub updated_at: UnixMs,
}

/// `POST /api/v1/collections/{name}/records` and `PUT …/records/{id}`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RecordIn {
    /// All its fields (a whole record, checked against the schema).
    pub fields: serde_json::Value,
    #[serde(default)]
    pub links: Option<Vec<String>>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// Updating: the version you read. A newer one means someone else
    /// changed it since, and the write is refused.
    #[serde(default)]
    pub version: Option<i64>,
}

/// `POST /api/v1/collections/{name}/query`: which records, in what order.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RecordQuery {
    /// field → a value (equal), or an object of comparisons: $eq, $ne, $in,
    /// $nin, $gt, $gte, $lt, $lte, $contains (text, or an item of a list).
    #[serde(default, rename = "where")]
    pub filter: serde_json::Map<String, serde_json::Value>,
    /// Words anywhere in the record.
    #[serde(default)]
    pub text: String,
    /// A field, or -field for descending; also created_at and updated_at.
    /// Default: -updated_at.
    #[serde(default)]
    pub sort: String,
    /// Count the matching records per value of this field instead.
    #[serde(default)]
    pub group_by: String,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: Option<u32>,
    /// A filter that must hold as well (a grant's on some records); set by
    /// the head, never taken from a request.
    #[serde(skip)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub within: serde_json::Map<String, serde_json::Value>,
}

/// The records a query found (or, with group_by, the counts).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RecordPage {
    pub records: Vec<Record>,
    /// All that match.
    pub total: i64,
    /// With group_by: (value, count), most first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<(serde_json::Value, i64)>,
}

/// A version of a record.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RecordChange {
    pub version: i64,
    /// create, update or delete.
    pub op: String,
    pub fields: serde_json::Value,
    pub changed_by: String,
    pub changed_at: UnixMs,
}

/// `?realm=`: which realm (you; a realm's token is always in its own).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct InRealm {
    #[serde(default)]
    pub realm: Option<String>,
}

/// A grant (docs/os.md §3.7): realm `from`'s data may flow into realm `to`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Grant {
    pub id: String,
    pub from_realm: String,
    pub to_realm: String,
    /// collection, memex or operations.
    pub kind: String,
    /// collection: {"collection": name, "where": {filter}}; memex: {"kinds": […]}
    /// (empty: every kind but spam); operations: {"methods": ["memex.mail.draft", …]}.
    pub scope: serde_json::Value,
    /// read, append or snapshot (collections); read (memex); call (operations).
    pub mode: String,
    /// active, requested, revoked or denied.
    pub state: String,
    #[serde(default)]
    pub expires_at: Option<UnixMs>,
    #[serde(default)]
    pub note: String,
    pub created_by: String,
    pub created_at: UnixMs,
    #[serde(default)]
    pub revoked_at: Option<UnixMs>,
    /// Times it was used.
    #[serde(default)]
    pub uses: i64,
}

/// `POST /api/v1/grants`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GrantIn {
    pub from_realm: String,
    pub to_realm: String,
    pub kind: String,
    #[serde(default)]
    pub scope: serde_json::Value,
    pub mode: String,
    #[serde(default)]
    pub expires_at: Option<UnixMs>,
    #[serde(default)]
    pub note: String,
}

/// One use of a grant.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GrantUse {
    pub at: UnixMs,
    pub by: String,
    pub what: String,
}

/// `POST /api/v1/realms` (and `PATCH …/{id}`: name and description).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RealmIn {
    /// Lowercase letters, digits and - (creating only).
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Whether the realm reads commons (updating only; unset: unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reads_commons: Option<bool>,
}

/// `PUT /api/v1/workers/{id}/realms` and `…/connections/{id}/realms`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RealmsIn {
    /// The realms it is available to (empty: all).
    pub realms: Vec<String>,
}

/// `GET /api/v1/realms`: a realm, which owns sessions, knowledge and data
/// (docs/os.md §3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Realm {
    pub id: String,
    pub name: String,
    pub description: String,
    pub created_at: UnixMs,
    /// system, you, or app:<name>.
    pub created_by: String,
    /// The app it was made for, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Whether it reads commons (docs/os.md §3.3).
    #[serde(default)]
    pub reads_commons: bool,
}

/// A side chat: a new session that branches from `anchor_seq` of a session
/// (with everything up to there as context) on the same worker.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CreateSideChat {
    pub anchor_seq: u64,
    pub connection_id: String,
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
    pub prompt: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CreateSession {
    pub worker_id: Uuid,
    pub cwd: String,
    /// Empty: the matching routing rule's, or the first connection.
    #[serde(default)]
    pub connection_id: String,
    /// Empty: the matching routing rule's, else the connection's default
    /// model, else the model of your latest session on that connection.
    #[serde(default)]
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
    /// The realm to run in. Default: the API token's realm, else home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realm: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StartChatGptSignIn {
    #[serde(default)]
    pub name: String,
    /// Sign this connection's account in again (its sign-in expired or was
    /// revoked); none: add an account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    /// An email to suggest to OpenAI's sign-in page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_hint: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatGptSignIn {
    /// Open this in a browser and approve.
    pub authorize_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CompleteChatGptSignIn {
    /// The address the browser landed on after approving.
    pub callback_url: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AttachmentInfo {
    pub name: String,
    pub mime: String,
    pub size: u64,
}

/// An image or file sent with a message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AttachmentUpload {
    pub name: String,
    /// image/png, image/jpeg, image/gif, image/webp, or a text type.
    pub mime: String,
    /// Base64.
    pub data: String,
}

/// `POST /api/v1/sessions/{id}/rewind`
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RewindRequest {
    /// The user message to rewind to before (its event seq).
    pub to_seq: u64,
    /// Also put files back as they were before that message.
    #[serde(default)]
    pub restore_files: bool,
}

/// Input accepted by the head but not yet recorded by the worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct QueuedInput {
    pub command_id: String,
    pub text: String,
    pub created_at: UnixMs,
    /// Set if the worker refused it.
    pub rejected: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionDetail {
    pub session: Session,
    pub worker_name: String,
    pub worker_online: bool,
    pub queued: Vec<QueuedInput>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Event {
    pub seq: u64,
    pub ts: UnixMs,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// A tool call held for your approval by a policy, or its outcome.
    Approval {
        call_id: String,
        /// "requested", "allowed", "denied" or "expired".
        state: String,
        tool: String,
        input: String,
        policy: String,
        message: String,
    },
    /// The agent changed a memory note.
    Memory {
        op: String,
        id: String,
        scope: String,
        text: String,
    },
    /// The assistant asked you for inputs (a form), and what became of it.
    Question {
        call_id: String,
        title: String,
        fields: Vec<QuestionField>,
        /// "asked", "answered" or "cancelled".
        state: String,
        /// Your answers (secret ones as "[secret]").
        #[serde(default)]
        answer: serde_json::Value,
    },
    /// The agent sent you a notification.
    Notice {
        title: String,
        message: String,
        priority: String,
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    Completed,
    Interrupted,
    Error,
    WorkerRestarted,
    Unknown,
}

/// A command to run, and where (a fact's check).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Probe {
    /// A shell command.
    pub command: String,
    /// session (its worker, in its folder; default), worker (by name, e.g.
    /// one inside a private network) or head.
    #[serde(default)]
    pub on: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worker: String,
    /// Seconds before it is stopped (default 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
    /// carbon6 secrets to set as environment variables.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
}

/// A fact about the systems around the code (environment knowledge).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Fact {
    pub id: String,
    pub claim: String,
    /// The systems it is about (thing keys, e.g. ci:argo, repo:github.com/you/app).
    pub about: Vec<String>,
    /// reviewed, verified, observed, inferred, disputed or retired.
    pub status: String,
    /// stable (until contradicted) or slow (re-checked after 30 days).
    pub volatility: String,
    /// It applies only to sessions with these labels; empty: everywhere.
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub conditions: String,
    /// "X does not apply to Y".
    #[serde(default)]
    pub negative: bool,
    /// A command that re-checks it (exit 0: still true), and where it runs.
    #[serde(default)]
    pub check: Option<Probe>,
    #[serde(default)]
    pub evidence: Vec<serde_json::Value>,
    #[serde(default)]
    pub supports: Vec<String>,
    #[serde(default)]
    pub replaced_by: Option<String>,
    pub valid_from: UnixMs,
    #[serde(default)]
    pub valid_to: Option<UnixMs>,
    #[serde(default)]
    pub last_verified: Option<UnixMs>,
    /// Due for a re-check.
    #[serde(default)]
    pub due: bool,
    pub made_by: String,
    pub created_at: UnixMs,
    pub updated_at: UnixMs,
}

/// A decision: any time carbon6 needs you (docs/os.md §4.2) — an agent's
/// question, an approval of a tool call or a workflow step, something to
/// confirm or provide, a dispute to settle.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Decision {
    pub id: i64,
    pub realm: String,
    /// approve, choose, confirm, provide, question, dispute or follow_up.
    pub kind: String,
    pub title: String,
    /// The case: what is asked, the evidence, what each option does
    /// (Markdown).
    #[serde(default)]
    pub document: String,
    /// What it is about: facts, things, sessions, links (JSON).
    #[serde(default)]
    pub context: serde_json::Value,
    /// Who asked: session:<id>, memex, workflow:<run>, you.
    pub source: String,
    /// high, normal or low.
    pub priority: String,
    /// Shown together with others of the same group.
    #[serde(default)]
    pub group: String,
    /// The answers to choose from (none: settle it in your own words).
    #[serde(default)]
    pub options: Vec<DecisionOption>,
    /// What to fill in (for provide and question).
    #[serde(default)]
    pub form: Vec<FormField>,
    /// open, snoozed, answered, dismissed, expired or withdrawn.
    pub state: String,
    #[serde(default)]
    pub snooze_until: Option<UnixMs>,
    /// Unanswered by then: the default answer is taken (else "no").
    #[serde(default)]
    pub deadline: Option<UnixMs>,
    #[serde(default)]
    pub default_answer: Option<String>,
    /// {"option": …, "values": {…}} once answered.
    #[serde(default)]
    pub answer: Option<serde_json::Value>,
    /// Your words with the answer.
    #[serde(default)]
    pub note: String,
    /// Where it was answered: carbon6, a link, the session, expired.
    #[serde(default)]
    pub answered_via: String,
    #[serde(default)]
    pub answered_at: Option<UnixMs>,
    /// Something waits on the answer (an agent, a tool call, a workflow).
    #[serde(default)]
    pub waiting: bool,
    /// Sessions started to settle it.
    #[serde(default)]
    pub sessions: Vec<String>,
    /// Raised again (grouped) this many times.
    #[serde(default)]
    pub count: u32,
    pub created_at: UnixMs,
    pub updated_at: UnixMs,
}

/// One answer a decision offers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionOption {
    pub label: String,
    /// What happens if you choose it.
    #[serde(default)]
    pub does: String,
}

/// A field a decision asks you to fill in.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FormField {
    pub name: String,
    #[serde(default)]
    pub label: String,
    /// text, long or choice.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub choices: Vec<String>,
    #[serde(default)]
    pub required: bool,
}

/// `POST /api/v1/decisions` (and what agents raise).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionIn {
    #[serde(default = "decision_kind")]
    pub kind: String,
    pub title: String,
    #[serde(default, alias = "detail")]
    pub document: String,
    #[serde(default)]
    pub context: serde_json::Value,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub priority: String,
    /// Decisions with the same group are one (raised again: its count).
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub options: Vec<DecisionOption>,
    #[serde(default)]
    pub form: Vec<FormField>,
    #[serde(default)]
    pub deadline: Option<UnixMs>,
    /// One of the options, taken at the deadline.
    #[serde(default)]
    pub default_answer: Option<String>,
}

fn decision_kind() -> String {
    "question".into()
}

/// `PATCH /api/v1/decisions/{id}`: snooze, dismiss, or reopen.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionEdit {
    /// open, snoozed or dismissed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snooze_until: Option<UnixMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `POST /api/v1/decisions/{id}/answer`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionAnswer {
    /// One of its options (when it has options).
    #[serde(default)]
    pub option: Option<String>,
    /// Its form's fields.
    #[serde(default)]
    pub values: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub note: String,
}

/// One input the assistant asks for.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct QuestionField {
    pub name: String,
    #[serde(default)]
    pub label: String,
    /// "text", "longtext", "number", "boolean", "secret" or "choice".
    #[serde(default = "text_field")]
    pub r#type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

fn text_field() -> String {
    "text".into()
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Choice {
    pub value: String,
    #[serde(default)]
    pub label: String,
}

/// `POST /api/v1/sessions/{id}/answers/{call_id}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AnswerIn {
    #[serde(default)]
    pub values: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub cancelled: bool,
}

/// `POST /api/v1/assistant`: a question for the assistant.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AskAssistant {
    pub prompt: String,
    /// Where you are in the UI ("Settings → Memex", "worker build-1").
    #[serde(default)]
    pub context: String,
}

/// `GET`/`PUT /api/v1/settings/assistant`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AssistantSettings {
    /// The assistant's model; none: the routes for the label `assistant`,
    /// else the first connection's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<MemexModel>,
}

/// `POST /api/v1/catalog/call` (the CLI).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogCall {
    pub op: String,
    #[serde(default)]
    pub input: serde_json::Value,
    /// Destructive operations need this (the CLI asks you first).
    #[serde(default)]
    pub confirmed: bool,
}

/// Messages on `WS /api/v1/sessions/{id}/stream`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// A secret an operation returned (a token, a code), for you only:
    /// shown live, never logged or given to the model.
    Secret {
        call_id: String,
        title: String,
        value: String,
        note: String,
    },
    /// Current queued inputs, sent whenever they change.
    Queue {
        queued: Vec<QueuedInput>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApiError {
    pub error: String,
}

// --- Sign-in ------------------------------------------------------------------

/// `GET /api/v1/auth/status`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RedeemLink {
    pub token: String,
}

/// `POST /api/v1/auth/codes`: a one-time code for signing in another device.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LoginCode {
    /// e.g. "K7QM-2XRD"
    pub code: String,
    pub expires_at: UnixMs,
}

/// `POST /api/v1/auth/code`: sign in with a one-time code.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RedeemCode {
    pub code: String,
}

/// `POST /api/v1/auth/passkeys/register/finish`
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RegisterPasskey {
    pub name: String,
    /// What `navigator.credentials.create` returned, as the UI encodes it.
    pub credential: serde_json::Value,
}

/// `PATCH /api/v1/auth/passkeys/{id}`
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RenamePasskey {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    /// The one realm it reaches (none: every realm, and administration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realm: Option<String>,
    pub created_at: UnixMs,
    pub last_used_at: Option<UnixMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CreateApiToken {
    pub name: String,
    /// Limit it to one realm's data (none: every realm, and administration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realm: Option<String>,
}

/// A hook: a shell command run on the worker at a point in each session
/// (`GET`/`PUT /api/v1/settings/hooks`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// Hosts from your inventory with all of these labels (or `*` for every
    /// host): one session each, through the host's worker.
    #[serde(default)]
    pub hosts: Vec<String>,
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RunTemplate {
    #[serde(default)]
    pub input: String,
}

/// The sessions a template run started (one per target).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TemplateRun {
    /// Its sessions carry the label run:<run_id>.
    pub run_id: String,
    pub sessions: Vec<Session>,
    /// Targets that could not start, and why.
    #[serde(default)]
    pub errors: Vec<String>,
}

/// A trigger (docs/os.md §4.3; called an automation before): an event,
/// an optional filter on it, and an action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Automation {
    pub id: String,
    pub name: String,
    /// The start action's template or workflow.
    #[serde(default)]
    pub template: String,
    /// The event: "schedule", "webhook", "memex", "record", "decision" or
    /// "session".
    pub trigger: String,
    /// Schedules: five cron fields in the head's local time, or @daily etc.
    /// Memex events: the event and its conditions (mail.received from:…).
    #[serde(default)]
    pub cron: String,
    /// The event's parameters: records {collection, ops}; decisions {ops};
    /// sessions {ops, labels}. ops: created, updated, deleted; opened,
    /// answered, expired; finished, failed, needs_approval.
    #[serde(default)]
    pub event_params: serde_json::Value,
    /// Conditions on the event, by JSONPath: {"$.record.fields.stage":
    /// "won", "$.open_decisions.count": {"$gt": 0}}; all must hold.
    #[serde(default)]
    pub filter: serde_json::Value,
    /// "start" (the template or workflow), "write" (a record), "ask" (a
    /// decision) or "notify".
    #[serde(default = "start_action")]
    pub action: String,
    /// The action's parameters; strings "$.path" take the event's value,
    /// and "{{$.path}}" inside text is replaced by it. write: {collection,
    /// fields, id?}; ask: {title, document, options, priority}; notify:
    /// {title, message, priority}; start: {input?}.
    #[serde(default)]
    pub action_params: serde_json::Value,
    /// Runs in an hour beyond this are skipped, and you are told.
    #[serde(default = "per_hour_default")]
    pub per_hour: u32,
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
    /// Switch it off after this many failed runs in a row; 0: never.
    #[serde(default)]
    pub disable_after: u32,
    /// How its latest runs went, newest first (up to 10): "running", "ok",
    /// "failed" or "stopped".
    #[serde(default)]
    pub recent: Vec<String>,
}

/// A workflow (`/api/v1/settings/workflows`): steps that run templates,
/// wait for your approval or notify you, in order, each one able to use
/// earlier steps' results and to run only when they say so.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Workflow {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub steps: Vec<WorkflowStep>,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowStep {
    /// How later steps refer to it ({{steps.<id>.output}}); default: step1,
    /// step2, ….
    #[serde(default)]
    pub id: String,
    /// "run" (a template), "approve" (wait for your yes) or "notify".
    pub kind: String,
    /// run: the template.
    #[serde(default)]
    pub template: String,
    /// run: the template's {{input}}. With {{input}} (the workflow's) and
    /// {{steps.<id>.output}}.
    #[serde(default)]
    pub input: String,
    /// approve, notify: what to tell you (same placeholders).
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub message: String,
    /// Run this step only if this holds; otherwise skip it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<StepCondition>,
    /// run: go on even if its sessions fail.
    #[serde(default)]
    pub continue_on_failure: bool,
}

/// A condition on an earlier step's output. For `changed`, the output's
/// last line starting with `RESULT:` is compared (else all of it), so ask
/// for one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StepCondition {
    pub step: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_contains: Option<String>,
    /// Different from that step's in the workflow's last finished run.
    #[serde(default)]
    pub changed: bool,
}

/// A run of a workflow (`GET /api/v1/workflows/runs`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowRun {
    pub id: String,
    pub workflow: String,
    #[serde(default)]
    pub input: String,
    pub started_at: UnixMs,
    #[serde(default)]
    pub finished_at: Option<UnixMs>,
    /// "manual", "schedule", "webhook".
    pub trigger: String,
    /// "running", "waiting" (for your approval), "ok", "failed", "denied".
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
    pub steps: Vec<WorkflowStepRun>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowStepRun {
    pub id: String,
    pub kind: String,
    /// "pending", "running", "waiting", "ok", "failed", "skipped",
    /// "denied".
    pub status: String,
    #[serde(default)]
    pub output: String,
    /// run: its sessions.
    #[serde(default)]
    pub sessions: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub started_at: Option<UnixMs>,
    #[serde(default)]
    pub finished_at: Option<UnixMs>,
}

/// `POST /api/v1/workflows/{name}/run`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RunWorkflow {
    #[serde(default)]
    pub input: String,
}

/// One run of an automation (`GET /api/v1/automations/{id}/runs`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AutomationRun {
    pub run_id: String,
    pub started_at: UnixMs,
    #[serde(default)]
    pub finished_at: Option<UnixMs>,
    /// What fired it: "schedule", "webhook", "memex", "record", "decision",
    /// "session" or "manual".
    pub trigger: String,
    /// The triggers whose actions led to this run, first first.
    #[serde(default)]
    pub cause: Vec<String>,
    /// "running", "ok", "failed" or "stopped" (a turn was interrupted).
    pub outcome: String,
    /// Why it did not start (or why some of its targets did not).
    #[serde(default)]
    pub error: Option<String>,
    pub sessions: Vec<RunSession>,
    /// USD, for models with a price (subagents included).
    pub cost: f64,
    pub calls: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RunSession {
    pub id: String,
    pub title: String,
    /// Empty while its first turn runs; "ok", "failed" or "stopped".
    pub outcome: String,
}

fn start_action() -> String {
    "start".into()
}

fn per_hour_default() -> u32 {
    60
}

/// What triggers are called from now on.
pub type Trigger = Automation;
pub type TriggerEdit = AutomationEdit;

/// `POST /api/v1/automations` (create) and `PATCH /api/v1/automations/{id}`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AutomationEdit {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub event_params: Option<serde_json::Value>,
    #[serde(default)]
    pub filter: Option<serde_json::Value>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub action_params: Option<serde_json::Value>,
    #[serde(default)]
    pub per_hour: Option<u32>,
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
    #[serde(default)]
    pub disable_after: Option<u32>,
}

/// A memory note: kept across sessions, given to those whose labels match
/// its scope (`/api/v1/memories`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

/// A policy: what happens to matching tool calls in sessions with its
/// labels (`/api/v1/settings/policies`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Policy {
    pub name: String,
    /// Labels a session must all have; empty: every session.
    #[serde(default)]
    pub applies_to: Vec<String>,
    /// A regular expression over tool names (Claude Code's names, like
    /// Bash or Edit, work with every model); empty: every tool.
    #[serde(default)]
    pub tools: String,
    /// A regular expression over the call's input (as JSON); empty: any.
    #[serde(default)]
    pub pattern: String,
    /// "ask" (wait for your approval) or "deny".
    pub action: String,
    /// Shown to you and to the agent.
    #[serde(default)]
    pub message: String,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

/// A routing rule (`/api/v1/settings/routes`): the connection and model for
/// new sessions with its labels that are started without a model. The
/// first enabled rule that matches wins.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Route {
    pub name: String,
    /// Labels a session must all have; empty: every session.
    #[serde(default)]
    pub applies_to: Vec<String>,
    /// A connection name or id; empty: the session's connection (else that
    /// of your latest session, else the first).
    #[serde(default)]
    pub connection: String,
    pub model: String,
    /// Reasoning effort, unless the session asks for one.
    #[serde(default)]
    pub effort: String,
    /// A cheaper model for subagents (Task tool); empty: the session's.
    #[serde(default)]
    pub subagent_model: String,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

/// A notification channel (`/api/v1/settings/channels`): where the head
/// tells you about events in sessions with its labels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Channel {
    pub name: String,
    /// "push" (your browsers and phones, see Settings), "ntfy", "slack",
    /// "discord", "webhook" or "email".
    pub kind: String,
    /// ntfy: the topic URL (https://ntfy.sh/my-topic); Slack, Discord,
    /// webhook: the webhook URL; email: the SMTP server, host[:port] (465:
    /// TLS, else STARTTLS).
    #[serde(default)]
    pub url: String,
    /// Email: the SMTP user, also the sender.
    #[serde(default)]
    pub user: String,
    /// Email: the recipient.
    #[serde(default)]
    pub to: String,
    /// The name of a secret (Settings → Secrets): the ntfy access token,
    /// the SMTP password, or a bearer token for a webhook.
    #[serde(default)]
    pub secret: String,
    /// Which events (see `NOTIFY_EVENTS`); empty: approvals, decisions,
    /// failures, notices, automation failures, budgets and hosts.
    #[serde(default)]
    pub events: Vec<String>,
    /// Labels a session must all have; empty: every session.
    #[serde(default)]
    pub applies_to: Vec<String>,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

/// Events a channel can carry: (name, description).
pub const NOTIFY_EVENTS: [(&str, &str); 10] = [
    ("approval", "a tool call waits for your approval (with Allow / Deny links)"),
    ("decision", "something needs your decision (with a link to answer, when it has options)"),
    ("failed", "a turn ended with an error"),
    ("finished", "a turn finished"),
    ("notice", "the agent sent you a notification (notify)"),
    ("output", "the agent published a file"),
    ("automation", "a trigger's run failed, failures switched one off, or loop protection stopped it"),
    ("trigger", "a trigger's notify action"),
    ("budget", "a budget ran out"),
    ("host", "a host's scheduled check failed, or it is reachable again"),
];

/// The events a channel without a list carries.
pub const DEFAULT_NOTIFY_EVENTS: [&str; 8] = ["approval", "decision", "failed", "notice", "automation", "trigger", "budget", "host"];

/// A machine reached over SSH through a worker (`/api/v1/settings/hosts`).
/// Sessions on it get its labels; templates can target hosts by label.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Host {
    /// A short name (letters, digits, - _ .); you can start a session
    /// with it as the SSH target.
    pub name: String,
    /// `user@host[:port]` or an alias from the worker's ~/.ssh/config.
    pub target: String,
    /// The worker that reaches it (by name); empty: any online worker.
    #[serde(default)]
    pub worker: String,
    /// Labels for its sessions, like role:web, env:prod.
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub description: String,
    /// Check that it is reachable every this many minutes; empty or 0:
    /// only when you ask.
    #[serde(default, deserialize_with = "maybe_number", skip_serializing_if = "Option::is_none")]
    pub check_every: Option<f64>,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

/// A host and its last reachability check (`GET /api/v1/hosts`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HostStatus {
    pub host: Host,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<HostCheck>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HostCheck {
    pub at_ms: i64,
    pub ok: bool,
    /// The worker that checked.
    pub worker: String,
    /// The system (OS, kernel) when reachable; why not, when not.
    pub message: String,
    pub latency_ms: u64,
}

/// A model's price in USD per million tokens (`/api/v1/settings/prices`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Price {
    /// A model id, or a prefix ending in `*` (`claude-sonnet-*`).
    pub model: String,
    #[serde(deserialize_with = "number")]
    pub input: f64,
    #[serde(deserialize_with = "number")]
    pub output: f64,
    /// Default: the input price.
    #[serde(default, deserialize_with = "maybe_number", skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Default: the input price.
    #[serde(default, deserialize_with = "maybe_number", skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// A number, or a string holding one (as the settings editor saves it).
fn maybe_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    #[derive(Deserialize)]
    #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
    #[serde(untagged)]
    enum N {
        Num(f64),
        Text(String),
        Null(()),
    }
    match N::deserialize(d)? {
        N::Num(n) => Ok(Some(n)),
        N::Null(()) => Ok(None),
        N::Text(s) if s.trim().is_empty() => Ok(None),
        N::Text(s) => {
            s.trim().trim_start_matches('$').parse().map(Some).map_err(|_| serde::de::Error::custom(format!("{s:?} is not a number")))
        }
    }
}

fn number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    maybe_number(d)?.ok_or_else(|| serde::de::Error::custom("a number is required"))
}

/// A spending limit for sessions with given labels
/// (`/api/v1/settings/budgets`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Budget {
    pub name: String,
    /// Labels a session must all have; empty: every session.
    #[serde(default)]
    pub applies_to: Vec<String>,
    /// "day", "week" (from Monday) or "month", in the head's time zone.
    pub period: String,
    /// USD.
    #[serde(deserialize_with = "number")]
    pub limit: f64,
    /// "notify" (tell you once per period) or "block" (also stop running
    /// turns and refuse new work until the period ends).
    pub action: String,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

/// Tokens and cost for one group of model calls.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageRow {
    /// The group: a day (YYYY-MM-DD), model, connection id, label or
    /// session id.
    pub key: String,
    /// What to show for it (a connection's or session's name).
    pub name: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// USD, for calls on models with a price.
    pub cost: f64,
    /// Model calls.
    pub calls: u64,
    /// Calls on models without a price.
    pub unpriced_calls: u64,
}

/// A budget and where it stands this period.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BudgetStatus {
    pub name: String,
    pub period: String,
    pub limit: f64,
    pub spent: f64,
    pub action: String,
    /// Used up and blocking new work.
    pub blocking: bool,
}

/// `GET /api/v1/usage?days=N&labels=a,b`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageReport {
    pub since_ms: i64,
    pub total: UsageRow,
    pub by_day: Vec<UsageRow>,
    pub by_model: Vec<UsageRow>,
    pub by_connection: Vec<UsageRow>,
    /// Your labels and host:, template:, automation: (most expensive first).
    pub by_label: Vec<UsageRow>,
    /// The most expensive sessions (subagents count toward their parent).
    pub top_sessions: Vec<UsageRow>,
    /// Models used without a price.
    pub unpriced_models: Vec<String>,
    pub budgets: Vec<BudgetStatus>,
}

/// A browser's push subscription (`POST /api/v1/push/subscriptions`), as
/// `PushSubscription.toJSON()` gives it, plus a name for the device.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PushSubscription {
    pub endpoint: String,
    pub keys: PushKeys,
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PushKeys {
    pub p256dh: String,
    pub auth: String,
}

/// A subscribed device (`GET /api/v1/push/subscriptions`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PushDevice {
    pub id: String,
    pub name: String,
    /// Where the push service takes its messages (this device's, if it
    /// matches the browser's own subscription).
    pub endpoint: String,
    /// The push service's host (e.g. fcm.googleapis.com).
    pub service: String,
    pub created_at: UnixMs,
    #[serde(default)]
    pub last_sent_at: Option<UnixMs>,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// `GET /api/v1/push/key`: the head's public key for subscribing.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PushKey {
    pub key: String,
}

/// `POST /api/v1/settings/channels/test`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TestChannel {
    pub name: String,
}

/// `GET /api/v1/system/upgrades`: an upgrade of one of carbon6's
/// databases, newest first (docs/os.md §13).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Upgrade {
    /// head or memex.
    pub service: String,
    pub database: String,
    pub at: i64,
    /// The schema version before (none: the database was created).
    pub from_version: Option<i64>,
    pub to_version: i64,
    /// Where the database was copied before upgrading.
    pub backup: Option<String>,
    /// What changed, as text.
    pub report: String,
}

/// `GET`/`PUT /api/v1/settings/public_url`: the address links in
/// notifications point to.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PublicUrl {
    pub url: String,
}

/// `POST /api/v1/sessions/{id}/approvals/{call_id}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApprovalDecision {
    pub allow: bool,
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SetLabels {
    pub labels: Vec<String>,
}

/// `GET /api/v1/sessions/{id}/config`: what applies to a session, from the
/// head's current settings and the session's labels.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EffectiveConfig {
    pub labels: Vec<String>,
    pub commands: Vec<Applied>,
    pub agents: Vec<Applied>,
    pub skills: Vec<Applied>,
    pub hooks: Vec<Applied>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OpenIn {
    pub session_id: String,
    pub title: String,
    /// Side chat ids to expand, outermost first; the last is the result's.
    pub side_chats: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SnippetPart {
    pub text: String,
    pub hit: bool,
}

/// `GET /api/v1/sessions/{id}/matches?q=`: find in a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionMatches {
    /// In transcript order, side chats included.
    pub matches: Vec<SessionMatch>,
    pub terms: Vec<String>,
    #[serde(default)]
    pub ignored: Vec<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionMatch {
    /// The transcript element showing it (see `SearchHit::anchor`).
    pub anchor: String,
    /// Side chats to expand first, outermost first.
    pub open: Vec<String>,
    pub kind: String,
}

// ---------------------------------------------------------------------------
// The memex (knowledge service): see docs/memex.md.
// ---------------------------------------------------------------------------

/// What kinds of items there are. Sessions see the default kinds, plus what
/// their scope rules add.
pub const MEMEX_KINDS: [&str; 13] =
    ["session", "output", "web", "note", "document", "paper", "fact", "record", "email", "event", "photo", "video", "spam"];
/// What every session may search unless a scope rule says more (facts:
/// environment knowledge, for every session).
pub const MEMEX_DEFAULT_SCOPE: [&str; 8] = ["session", "output", "web", "note", "document", "paper", "fact", "record"];

/// An item to store (head → memex `memex.ingest`). The same source and
/// external id again replaces it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexItemIn {
    pub kind: String,
    /// Where it comes from: "carbon6", "memory", "you", a session, ….
    pub source: String,
    pub external_id: String,
    pub title: String,
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// When it happened; default: now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<UnixMs>,
    /// carbon6 labels it is about; each becomes (or is) a thing in the graph.
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub meta: serde_json::Value,
}

/// `memex.search` (and `GET /api/v1/memex/search`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexQuery {
    pub query: String,
    /// Only these kinds; empty: all the caller may see.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Only items about all of these things (labels or thing ids).
    #[serde(default)]
    pub about: Vec<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexHit {
    pub id: String,
    pub kind: String,
    pub title: String,
    /// The best matching passage.
    pub snippet: String,
    pub created_at: UnixMs,
    pub source: String,
    pub external_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub score: f64,
    /// "keyword", "meaning" or both.
    pub matched: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexThingRef {
    pub id: String,
    pub kind: String,
    pub name: String,
    /// The carbon6 label it stands for, if any.
    #[serde(default)]
    pub label: String,
}

/// An item, in full (`memex.get` of an item id).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexItem {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub text: String,
    pub created_at: UnixMs,
    pub added_at: UnixMs,
    pub source: String,
    pub external_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub about: Vec<MemexThingRef>,
    #[serde(default)]
    pub meta: serde_json::Value,
}

/// A thing in the graph with what is known about it (`memex.get` of a
/// thing id or label).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexThing {
    pub thing: MemexThingRef,
    /// Notes about it, newest first.
    pub notes: Vec<MemexHit>,
    /// Items about it, newest first.
    pub items: Vec<MemexHit>,
    /// Things that share items with it, most shared first.
    pub related: Vec<(MemexThingRef, u32)>,
}

/// `memex.brief`: context for a session about these labels.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexBrief {
    pub text: String,
    /// The items it draws on.
    pub refs: Vec<String>,
}

/// The memex's state (`GET /api/v1/memex/status`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexStatus {
    pub connected: bool,
    #[serde(default)]
    pub node: Option<String>,
    /// Items per kind.
    #[serde(default)]
    pub items: std::collections::BTreeMap<String, u64>,
    #[serde(default)]
    pub things: u64,
    #[serde(default)]
    pub passages: u64,
    /// Passages with a vector for the current embedding model.
    #[serde(default)]
    pub embedded: u64,
    #[serde(default)]
    pub embedding_model: String,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Waiting to be sent to the memex.
    #[serde(default)]
    pub outbox: u64,
    /// Items and threads the understanding model has read (this version).
    #[serde(default)]
    pub understood: u64,
    /// Model calls today, and the day's budget.
    #[serde(default)]
    pub calls_today: u64,
    #[serde(default)]
    pub calls_per_day: u64,
    /// Photos and videos, and how many have been looked at.
    #[serde(default)]
    pub media: u64,
    #[serde(default)]
    pub media_seen: u64,
    /// Suggestions waiting for you.
    #[serde(default)]
    pub review: u64,
}

/// Who owes what, by when (`/api/v1/memex/commitments`), read from mail and
/// sessions.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Commitment {
    pub id: i64,
    pub item_id: String,
    #[serde(default)]
    pub item_title: String,
    /// "me", or a person's address or name.
    pub who: String,
    pub what: String,
    #[serde(default)]
    pub due: Option<UnixMs>,
    /// open, done or dropped.
    pub status: String,
}

/// A suggestion for you to decide (`/api/v1/memex/review`): two things that
/// may be the same (`merge`), or a face to name (`face`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexReview {
    pub id: i64,
    pub kind: String,
    pub a: MemexThingRef,
    #[serde(default)]
    pub b: Option<MemexThingRef>,
    pub reason: String,
    /// A picture to show with it (a face's crop), as a media URL path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    pub created_at: UnixMs,
}

/// A thing's neighbourhood (`memex.graph`): things linked to it directly or
/// through items, with how strongly.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexGraph {
    pub center: MemexThingRef,
    pub nodes: Vec<MemexGraphNode>,
    /// Links among the neighbours (ids into `nodes`), for drawing.
    #[serde(default)]
    pub edges: Vec<(String, String, u32)>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexGraphNode {
    pub thing: MemexThingRef,
    /// Direct relations to the centre (e.g. member_of), if any.
    #[serde(default)]
    pub rels: Vec<String>,
    /// Items both are in.
    pub shared: u32,
    /// The most recent of those.
    #[serde(default)]
    pub last: Option<UnixMs>,
}

/// A model for one of the memex's tasks.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexModel {
    /// A connection name or id.
    pub connection: String,
    pub model: String,
}

/// `GET`/`PUT /api/v1/settings/memex`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexSettings {
    /// Turns text into vectors for search by meaning; none: keyword search
    /// only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<MemexModel>,
    /// The size of a session's context brief, in characters (default 4000;
    /// 0: none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief_chars: Option<u32>,
    /// Write people you correspond with (written to twice, heard from) back
    /// to Google Contacts, in a carbon6 group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contacts_auto: Option<bool>,
    /// Agents' mail_send and calendar invitations wait for your approval
    /// (default on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_needs_approval: Option<bool>,
    /// Reads mail threads, sessions, documents and papers: summaries,
    /// topics, commitments, claims. None: no understanding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub understanding: Option<MemexModel>,
    /// Model calls a day for understanding and vision (default 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls_per_day: Option<u32>,
    /// Looks at photos (and video frames): captions, objects, scenes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<MemexModel>,
    /// Transcribes videos' speech (an OpenAI-style /audio/transcriptions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcription: Option<MemexModel>,
    /// A GeoNames cities file on the memex's machine (e.g. cities1000.txt),
    /// for naming where photos were taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub places_file: Option<String>,
    /// Face models (ONNX files on the memex's machine): a detector
    /// (UltraFace-style) and an embedder (ArcFace or MobileFaceNet-style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub face_detector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub face_embedder: Option<String>,
}

/// A scope rule (`/api/v1/settings/memex_scopes`): sessions with these
/// labels may also see these kinds of items.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexScope {
    pub name: String,
    #[serde(default)]
    pub applies_to: Vec<String>,
    pub kinds: Vec<String>,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

/// A note you write (`POST /api/v1/memex/notes`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexNoteIn {
    pub text: String,
    /// Labels or thing ids it is about.
    #[serde(default)]
    pub about: Vec<String>,
}

/// An associative trail: a named path through items and things, with
/// comments (`/api/v1/memex/trails`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexTrail {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub made_by: String,
    pub updated_at: UnixMs,
    #[serde(default)]
    pub step_count: u32,
    #[serde(default)]
    pub steps: Vec<MemexTrailStep>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexTrailStep {
    pub id: i64,
    /// An item id, or a thing (id or label).
    pub target: String,
    #[serde(default)]
    pub comment: String,
    pub made_by: String,
    pub created_at: UnixMs,
    /// What the target is now (filled in when read).
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: String,
}

/// A file to add (`POST /api/v1/memex/upload`): base64 data.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexUpload {
    pub name: String,
    #[serde(default)]
    pub mime: String,
    pub data: String,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// `memex.ingest`: items to add, each typed by its MIME type and handled by
/// that type's handler (`memex.types`). docs/memex.md §4.5.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexIngest {
    /// Up to 500 items.
    pub items: Vec<IngestItem>,
}

/// One item to add.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct IngestItem {
    /// Its MIME type, e.g. message/rfc822 or application/pdf: picks the
    /// handler.
    #[serde(rename = "type")]
    pub mime: String,
    /// Its bytes in that format, base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// For text types, the text instead of `data`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Where it comes from.
    pub source: IngestSource,
    /// The source's own id for it: sending it again is harmless.
    pub external_id: String,
    /// Facts that aren't in the bytes (a folder, flags), as the handler's
    /// schema allows.
    #[serde(default)]
    pub attributes: serde_json::Value,
    /// carbon6 labels: which sessions may see it.
    #[serde(default)]
    pub labels: Vec<String>,
}

/// Where ingested items come from, e.g. {kind: "mbox", id: "archive-2009"}.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct IngestSource {
    pub kind: String,
    #[serde(default)]
    pub id: String,
}

/// What became of one ingested item.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct IngestResult {
    /// created, updated (the same source and id, new content), duplicate
    /// (already here: `id` is the existing item) or rejected (see `error`).
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The item's kind: email, document, paper, ….
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A handler in `memex.types`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct IngestType {
    pub name: String,
    /// The MIME types it takes.
    pub types: Vec<String>,
    /// What `attributes` may hold (JSON Schema; other keys are rejected).
    pub attributes: serde_json::Value,
    /// How it recognizes an item it already has.
    pub identity: String,
    /// The largest item, in bytes.
    pub limit: u64,
    /// The item kinds it creates.
    pub produces: Vec<String>,
}

/// A folder the memex watches (`/api/v1/settings/memex_folders`), on the
/// machine the memex runs on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemexFolder {
    pub path: String,
    /// Labels for what is found there (e.g. research, client:acme).
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

/// Your Google OAuth client (`/api/v1/google/client`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GoogleClient {
    pub client_id: String,
    pub has_secret: bool,
    /// The redirect URI to register with Google for this head.
    #[serde(default)]
    pub redirect_uri: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GoogleClientIn {
    pub client_id: String,
    /// Kept as is when absent.
    #[serde(default)]
    pub client_secret: Option<String>,
}

/// A connected Google account (`/api/v1/google/accounts`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GoogleAccount {
    pub id: String,
    pub email: String,
    #[serde(default)]
    pub name: String,
    pub added_at: UnixMs,
}

// --- Mail, calendar and contacts (through the memex) ---

/// A conversation in a mail view (`GET /api/v1/mail/threads`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MailThread {
    pub account: String,
    pub thread_id: String,
    pub subject: String,
    pub snippet: String,
    /// Its latest message's.
    pub date: UnixMs,
    pub count: u32,
    pub unread: bool,
    pub starred: bool,
    pub attachments: bool,
    /// Who wrote in it ("me" for you), in order.
    pub people: Vec<String>,
    /// Gmail label ids of its latest message.
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MailMessage {
    pub item_id: String,
    pub gmail_id: String,
    pub from: String,
    pub from_name: String,
    /// (address, name).
    pub to: Vec<(String, String)>,
    pub cc: Vec<(String, String)>,
    pub date: UnixMs,
    pub subject: String,
    pub text: String,
    /// Shown sandboxed, remote images off until you allow them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    pub attachments: Vec<MailAttachment>,
    pub labels: Vec<String>,
    pub unread: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MailAttachment {
    pub index: u32,
    pub name: String,
    pub mime: String,
    pub size: u64,
}

/// A message to send or keep as a draft (`POST /api/v1/mail/send`,
/// `/api/v1/mail/drafts`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MailDraft {
    /// The account's id or address.
    pub account: String,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    #[serde(default)]
    pub subject: String,
    /// Markdown (sent as text and HTML).
    #[serde(default)]
    pub body: String,
    /// The message this answers (an item id): same thread, In-Reply-To.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub attachments: Vec<AttachmentUpload>,
    /// Send later; default: after the undo window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_at: Option<UnixMs>,
    /// Replace this draft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_id: Option<String>,
}

/// A message on its way out (undo while pending).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Outgoing {
    pub id: String,
    pub send_at: UnixMs,
    /// pending, sending, sent, cancelled, failed.
    pub status: String,
    pub subject: String,
    pub to: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Calendar {
    pub account: String,
    pub id: String,
    pub summary: String,
    pub color: String,
    pub primary: bool,
    pub writable: bool,
    pub selected: bool,
}

/// An event, or one instance of a recurring one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CalendarEvent {
    pub account: String,
    pub calendar_id: String,
    pub event_id: String,
    /// For an instance of a recurring event: its start (to change "this"
    /// or "following").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_start: Option<UnixMs>,
    pub summary: String,
    pub description: String,
    pub location: String,
    pub start: UnixMs,
    pub end: UnixMs,
    pub all_day: bool,
    pub recurring: bool,
    pub attendees: Vec<Attendee>,
    /// Your answer: accepted, declined, tentative, needsAction; "" when not
    /// invited (your own event).
    pub my_status: String,
    pub conference: String,
    pub html_link: String,
    /// Counts as busy (not free time, not declined).
    pub busy: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Attendee {
    pub email: String,
    pub name: String,
    pub response: String,
    pub me: bool,
}

/// Creating or changing an event (`/api/v1/calendar/events`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EventEdit {
    pub account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// "this", "following" or "all" (for recurring events).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_start: Option<UnixMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<UnixMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<UnixMs>,
    #[serde(default)]
    pub all_day: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attendees: Option<Vec<String>>,
    /// RRULE etc. lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Contact {
    pub account: String,
    pub resource: String,
    pub name: String,
    pub emails: Vec<String>,
    pub phones: Vec<String>,
    pub orgs: Vec<String>,
    pub photo: String,
    /// The person in the memex's graph.
    #[serde(default)]
    pub thing_id: Option<String>,
}

/// Someone you write to who is not a contact yet.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContactCandidate {
    pub email: String,
    pub name: String,
    /// Messages you sent them, and they sent you.
    pub sent: u32,
    pub received: u32,
}

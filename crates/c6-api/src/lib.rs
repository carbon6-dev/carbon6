//! JSON types for the head's `/api/v1`, shared by `c6h` and the web UI.
//! Must stay wasm-compatible: no tokio, no std-only dependencies.

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
    /// Switch the session to this connection and model from this message
    /// on. Both or neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
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

//! Model clients and the provider-neutral conversation model.

mod accumulate;
pub mod activity;
pub mod anthropic;
mod http;
pub mod limit;
pub mod models;
pub mod openai_chat;
pub mod openai_responses;
pub mod pool;
mod sse;
pub mod window;

use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

pub use accumulate::Accumulator;
pub use models::{ModelInfo, ToolProfile, model_info};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

/// One piece of a message. Reasoning variants are provider-specific and are
/// replayed verbatim to the provider family that produced them; other
/// families skip them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Block {
    Text(String),
    /// Unsigned reasoning text (Chat Completions servers). Display only.
    Thinking(String),
    /// Anthropic thinking. Must go back unchanged, even with empty text.
    SignedThinking {
        text: String,
        signature: String,
    },
    /// Anthropic redacted thinking (opaque).
    RedactedThinking(String),
    /// OpenAI Responses reasoning item, carried statelessly.
    Reasoning {
        id: String,
        encrypted: String,
        summary: String,
    },
    /// A provider-run tool block (web search …), replayed verbatim to the
    /// same provider. `label` is for display.
    Opaque {
        provider: String,
        json: String,
        label: String,
    },
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
    /// An image from the user (PNG, JPEG, GIF or WebP).
    Image {
        media_type: String,
        data: Vec<u8>,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<Block>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self { role: Role::User, blocks: vec![Block::Text(text.into())] }
    }

    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.blocks.iter().filter_map(|b| match b {
            Block::ToolUse { id, name, input_json } => Some((id.as_str(), name.as_str(), input_json.as_str())),
            _ => None,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input object.
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ModelRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: Option<u32>,
    /// Stable per-conversation key for providers with keyed prefix caching.
    pub cache_key: Option<String>,
    /// Reasoning effort ("low" … "max", or "none": no reasoning); `None`
    /// uses the model default.
    pub effort: Option<String>,
    /// Offer the provider's own web search tool, where it has one.
    pub web_search: bool,
    /// The answer is one JSON object; Chat Completions asks for it with
    /// `response_format`, other APIs ignore it.
    pub json: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModelEvent {
    TextDelta(String),
    ThinkingDelta(String),
    /// A tool call begins. `index` identifies it in later deltas.
    ToolCallStart {
        index: u32,
        id: String,
        name: String,
    },
    ToolCallDelta {
        index: u32,
        args: String,
    },
    /// A finished block at position `index` of the response. Providers that
    /// emit these (Anthropic, Responses) define the message content entirely
    /// by them; deltas are then for live display only.
    Block {
        index: u32,
        block: Block,
    },
    Usage(Usage),
    Done {
        stop_reason: String,
    },
}

/// Normalized stop reasons the agent loop acts on.
pub mod stop {
    pub const MAX_TOKENS: &str = "max_tokens";
    pub const REFUSAL: &str = "refusal";
    pub const CONTEXT_EXCEEDED: &str = "model_context_window_exceeded";
    /// The provider paused a long server-tool turn; send the same request
    /// (with the partial response appended) to continue.
    pub const PAUSE_TURN: &str = "pause_turn";
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String, retry_after: Option<Duration> },
    #[error("network error: {0}")]
    Network(String),
    #[error("bad response from provider: {0}")]
    Protocol(String),
    #[error("{0}")]
    Config(String),
    /// The request does not fit the model's context window.
    #[error("context window exceeded: {0}")]
    ContextOverflow(String),
    /// No server can take the call now (a pool's members are all busy,
    /// resting after failures, or without the model).
    #[error("no model server available: {0}")]
    Unavailable(String),
}

impl LlmError {
    /// Failed for now only (the server unreachable, timed out, busy or
    /// failing, or none available): the same call may work later.
    pub fn retryable(&self) -> bool {
        match self {
            LlmError::Http { status, .. } => *status == 408 || *status == 409 || *status == 429 || *status >= 500,
            LlmError::Network(_) | LlmError::Unavailable(_) => true,
            _ => false,
        }
    }
}

pub type EventStream = BoxStream<'static, Result<ModelEvent, LlmError>>;

#[async_trait]
pub trait ModelClient: Send + Sync {
    /// Starts a streaming completion. Dropping the stream cancels the request.
    async fn stream(&self, req: &ModelRequest) -> Result<EventStream, LlmError>;
}

/// Which wire protocol a connection speaks. Matches `CredentialGrant.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
}

/// An image as a `data:` URL (OpenAI's APIs take images this way).
pub(crate) fn data_url(media_type: &str, data: &[u8]) -> String {
    use base64::Engine;
    format!("data:{media_type};base64,{}", base64::engine::general_purpose::STANDARD.encode(data))
}

/// Whether a connection passes the reasoning effort on to the model. Every
/// kind does: a Chat Completions server that rejects the field is asked
/// again without it (openai_chat.rs).
pub fn supports_effort(_kind: &str, _base_url: &str) -> bool {
    true
}

pub(crate) fn is_openai_api(base_url: &str) -> bool {
    url::Url::parse(base_url).ok().and_then(|u| u.host_str().map(|h| h == "api.openai.com")).unwrap_or(false)
}

impl Provider {
    /// Connection kinds: `openai_compat`, `openai`, `chatgpt`, `anthropic`.
    pub fn from_kind(kind: &str) -> Option<Self> {
        match kind {
            "openai_compat" | "" => Some(Self::OpenAiChat),
            "openai" | "chatgpt" => Some(Self::OpenAiResponses),
            "anthropic" => Some(Self::Anthropic),
            _ => None,
        }
    }
}

/// Builds the client for a connection kind.
pub fn client_for(
    kind: &str,
    base_url: &str,
    bearer: &str,
    headers: std::collections::HashMap<String, String>,
) -> Result<Box<dyn ModelClient>, LlmError> {
    Ok(match Provider::from_kind(kind).ok_or_else(|| LlmError::Config(format!("unknown connection kind `{kind}`")))? {
        Provider::OpenAiChat => Box::new(openai_chat::OpenAiChat::new(base_url, bearer, headers)?),
        Provider::OpenAiResponses => Box::new(openai_responses::OpenAiResponses::new(base_url, bearer, headers, kind == "chatgpt")?),
        Provider::Anthropic => Box::new(anthropic::Anthropic::new(base_url, bearer, headers)?),
    })
}

/// `client_for`, giving up after the first failed attempt: a pool member
/// that fails is passed over for another, not waited on.
pub fn client_once(
    kind: &str,
    base_url: &str,
    bearer: &str,
    headers: std::collections::HashMap<String, String>,
) -> Result<Box<dyn ModelClient>, LlmError> {
    Ok(match Provider::from_kind(kind).ok_or_else(|| LlmError::Config(format!("unknown connection kind `{kind}`")))? {
        Provider::OpenAiChat => Box::new(openai_chat::OpenAiChat::new(base_url, bearer, headers)?.once()),
        Provider::OpenAiResponses => Box::new(openai_responses::OpenAiResponses::new(base_url, bearer, headers, kind == "chatgpt")?.once()),
        Provider::Anthropic => Box::new(anthropic::Anthropic::new(base_url, bearer, headers)?.once()),
    })
}

/// rustls needs one process-wide crypto provider; we standardize on ring.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    #[test]
    fn effort_support() {
        for kind in ["openai_compat", "openai", "chatgpt", "anthropic", "claude_code"] {
            assert!(super::supports_effort(kind, "http://anything"), "{kind}");
        }
    }
}

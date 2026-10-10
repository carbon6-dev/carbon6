//! Model clients and the provider-neutral conversation model.

mod accumulate;
pub mod openai_chat;
mod sse;

use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

pub use accumulate::Accumulator;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Block {
    Text(String),
    Thinking(String),
    ToolUse { id: String, name: String, input_json: String },
    ToolResult { id: String, content: String, is_error: bool },
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
    Usage(Usage),
    Done {
        stop_reason: String,
    },
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
}

impl LlmError {
    pub fn retryable(&self) -> bool {
        match self {
            LlmError::Http { status, .. } => *status == 408 || *status == 409 || *status == 429 || *status >= 500,
            LlmError::Network(_) => true,
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

/// rustls needs one process-wide crypto provider; we standardize on ring.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

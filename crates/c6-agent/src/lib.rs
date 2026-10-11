//! The agent loop: turns user input into model calls and tool calls, recording
//! everything as session events.

mod base_prompt;
mod history;
mod prompt;
mod turn;

use async_trait::async_trait;
use c6_proto::{ephemeral_event, session_event};

pub use base_prompt::BASE_PROMPT;
pub use history::{
    compacted_context, effective_events, history_from_events, portable_events, portable_messages, transcript_text, user_blocks, user_text,
};
pub use prompt::build_system_prompt;
pub use turn::{Conversation, TurnConfig, run_turn};

/// Where a session's events go. Durable appends are journaled before they
/// return; ephemeral events are best-effort live updates.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Appends a durable event and returns its seq.
    async fn append(&self, kind: session_event::Kind) -> anyhow::Result<u64>;
    fn ephemeral(&self, kind: ephemeral_event::Kind);
}

/// A user message waiting for the next model request boundary.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PendingInput {
    pub command_id: String,
    pub text: String,
    /// Images and files sent with it, and files it mentions (@path).
    pub attachments: Vec<c6_proto::Attachment>,
}

impl PendingInput {
    /// The event recording it.
    pub fn event(&self) -> c6_proto::session_event::Kind {
        c6_proto::session_event::Kind::User(c6_proto::UserMessage {
            command_id: self.command_id.clone(),
            text: self.text.clone(),
            attachments: self.attachments.clone(),
        })
    }

    /// What the model gets: the text, then each attachment.
    pub fn blocks(&self) -> Vec<c6_llm::Block> {
        history::user_blocks(&self.text, &self.attachments)
    }
}

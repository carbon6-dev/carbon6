//! The agent loop: turns user input into model calls and tool calls, recording
//! everything as session events.

mod history;
mod prompt;
mod turn;

use async_trait::async_trait;
use c6_proto::{ephemeral_event, session_event};

pub use history::history_from_events;
pub use prompt::build_system_prompt;
pub use turn::{TurnConfig, run_turn};

/// Where a session's events go. Durable appends are journaled before they
/// return; ephemeral events are best-effort live updates.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Appends a durable event and returns its seq.
    async fn append(&self, kind: session_event::Kind) -> anyhow::Result<u64>;
    fn ephemeral(&self, kind: ephemeral_event::Kind);
}

/// A user message waiting for the next model request boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingInput {
    pub command_id: String,
    pub text: String,
}

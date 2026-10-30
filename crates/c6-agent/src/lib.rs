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
    HANDOFF_BUDGET, compacted_context, effective_events, handoff_context, history_from_events, model_text, portable_events,
    portable_messages, transcript_text, user_blocks, user_text,
};
pub use prompt::build_system_prompt;
pub use turn::{COMPACT_PROMPT, COMPACT_WHY, COMPACT_WHY_ASKED, CompactPrompts, Conversation, TurnConfig, compact_turn, run_turn};

/// What a subagent is told (docs/os.md §4.6: an editable prompt).
pub const SUBAGENT_NOTE: &str = "You are a subagent working for another agent, which cannot see this conversation. \
Work autonomously. When you are done, reply with a concise but complete report of what you found or did; that report is all the other agent receives.";

/// What an agent is told after the digest of a subagent that did not
/// finish (docs/subagents.md §10: an editable prompt).
pub const SUBAGENT_PARTIAL: &str = "The subagent did not finish. Its work so far is summarized above and its whole conversation is kept: \
to continue it with what it already did, send it a message with the send-message tool (to: its id) rather than starting a new one. \
Check its changes before relying on them.";

/// What a subagent is told when you resume it after a turn that did not
/// finish (an editable prompt).
pub const SUBAGENT_RESUME: &str = "You were stopped partway through. Your work so far is above. Check where you left off, \
then continue the task and finish with your report.";

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
    /// The slash command it invoked, if any.
    pub command: String,
    /// A custom command's template (from the head), to expand; not logged.
    pub template: String,
    /// What the model gets instead of the text, when a command expanded it.
    pub prompt: String,
    /// Why a UserPromptSubmit hook blocked it, if one did.
    pub blocked: String,
    /// In a subagent: the parent's tool call that sent it.
    pub delegated_by: String,
    /// In a subagent: its report should start a turn in its idle parent.
    pub wake: bool,
}

impl PendingInput {
    /// The event recording it.
    pub fn event(&self) -> c6_proto::session_event::Kind {
        c6_proto::session_event::Kind::User(c6_proto::UserMessage {
            command_id: self.command_id.clone(),
            text: self.text.clone(),
            attachments: self.attachments.clone(),
            command: self.command.clone(),
            prompt: self.prompt.clone(),
            blocked: self.blocked.clone(),
            delegated_by: self.delegated_by.clone(),
            wake: self.wake,
        })
    }

    /// What the model gets: the text (or its expansion), then each attachment.
    pub fn blocks(&self) -> Vec<c6_llm::Block> {
        history::user_blocks(self.model_text(), &self.attachments)
    }

    pub fn model_text(&self) -> &str {
        if self.prompt.is_empty() { &self.text } else { &self.prompt }
    }

    /// `/compact`: summarize the conversation instead of running a turn.
    pub fn is_compact(&self) -> bool {
        self.command == "compact"
    }
}

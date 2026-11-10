//! The agent loop: turns user input into model calls and tool calls, recording
//! everything as session events.

mod base_prompt;
mod history;
mod prompt;
mod turn;

use async_trait::async_trait;
use c6_proto::{ephemeral_event, session_event};

pub use base_prompt::{BASE_PROMPT, base_prompt_for_tools};
pub use history::{
    HANDOFF_BUDGET, compacted_context, context_tokens_from_events, effective_events, handoff_context, history_from_events, model_text,
    portable_events, portable_messages, transcript_text, user_blocks, user_text,
};
pub use prompt::build_system_prompt;
pub use turn::{
    COMPACT_AT, COMPACT_AT_LARGE, COMPACT_PROMPT, COMPACT_WHY, COMPACT_WHY_ASKED, CompactPrompts, Conversation, LARGE_WINDOW, TurnConfig,
    compact_share, compact_turn, run_turn,
};

/// What a subagent is told (docs/os.md §4.6: an editable prompt).
pub const SUBAGENT_NOTE: &str = "You are a subagent working for another agent, which cannot see this conversation. \
Work autonomously. When you are done, reply with a concise but complete report of what you found or did; that report is all the other agent receives.";

/// What an agent is told after the digest of a subagent that did not
/// finish (docs/subagents.md §10: an editable prompt).
pub const SUBAGENT_PARTIAL: &str = "The subagent did not finish. Its work so far is summarized above and its whole conversation is kept: \
to continue it with what it already did, send it a message with the send-message tool (to: its id) rather than starting a new one. \
Check its changes before relying on them.";

/// What an agent is told after a message from another session
/// (docs/background.md §6: an editable prompt).
/// What a Claude Code session is told about carbon6 (`session.claude_code`),
/// at the start of what carbon6 adds to Claude Code's system prompt.
pub const CLAUDE_CODE_NOTE: &str = "You are working in a carbon6 session. carbon6 gives you tools of its own over MCP, named \
`mcp__carbon6__<Name>`: subagents on other models, its documents, the work this session serves, memory, task cards, and, where they \
are on, its memex, mail, chat, calendar and apps. Some are loaded already; when a task needs one that isn't, find it with tool \
search. For carbon6's own things (its documents, work, memory, cards), use its tools rather than files. Subagents you start with \
carbon6's Task run on Claude Code, as you do, unless the user asks for another model: don't name another connection on your own \
(API connections are billed per call). For a quick helper that needs nothing of carbon6's, your own Agent tool is fine too.";

pub const SESSION_MESSAGE_NOTE: &str = "This message is from the agent of another session, not from the user. Act on it as part of your work if it is relevant. Reply with the message-session tool (to: its id) only when it asks for something or needs an answer; do not reply just to acknowledge it.";

/// What a subagent is told when you resume it after a turn that did not
/// finish (an editable prompt).
pub const SUBAGENT_RESUME: &str = "You were stopped partway through. Your work so far is above. Check where you left off, \
then continue the task and finish with your report.";

/// Introduces, in Task's description, the templates an agent may start as
/// remote subagents (docs/remote-subagents.md §6: an editable prompt).
pub const SUBAGENT_TEMPLATES: &str = "Templates (set template, and on when it lists several places): each starts the subagent as a \
session of its own somewhere else, set up by the user: another worker, folder, repository or model, with its own tools. It cannot \
see this conversation or your files; give it a complete prompt, and expect files it changes to stay where it runs.";

/// The result of a remote subagent's call after this worker restarted
/// while the call waited (an editable prompt).
pub const SUBAGENT_REMOTE_REATTACH: &str = "This worker restarted while you waited, but the subagent runs elsewhere and keeps \
working: its report will arrive as a notice when it finishes. Carry on with other work, or wait for it.";

/// What the agent is told when its worker restarted mid-turn and its turn
/// goes on from the log (an editable prompt; docs/restarts.md §4).
pub const RESUME_AFTER_RESTART: &str = "carbon6's worker restarted while you were working (an update, most likely), so your turn \
was cut short where the conversation above ends. A command or subagent that was still running then was stopped; its result says \
so. Carry on with the task from there, running again whatever you still need.";

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
    /// A message from another session (docs/background.md §6): its id, its
    /// title, and how many session messages led to it.
    pub from_session_id: String,
    pub from_title: String,
    pub hops: u32,
    /// HTTP API origin, with only a token's display name (no secret).
    pub api_origin: Option<c6_proto::ApiMessageOrigin>,
    /// Send now: a running turn gets it at once, not at its next step
    /// (docs/background.md §4). Not logged.
    pub send_now: bool,
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
            from_session_id: self.from_session_id.clone(),
            from_title: self.from_title.clone(),
            hops: self.hops,
            api_origin: self.api_origin.clone(),
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

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
    HANDOFF_BUDGET, NATIVE_PDF_REJECTED_NOTE, TRIM_KEEP_AFTER_INSTRUCTION, TRIM_KEEP_LATEST, TRIM_MIN_CHARS, TRIMMED_OUTPUT,
    compacted_context, context_tokens_from_events, effective_events, handoff_context, history_from_events, model_text, portable_events,
    portable_messages, rewind_context, transcript_text, user_blocks, user_text,
};
pub use prompt::build_system_prompt;
pub use turn::{
    COMPACT_AT, COMPACT_AT_LARGE, COMPACT_PROMPT, COMPACT_WHY, COMPACT_WHY_ASKED, CompactPrompts, Conversation, IDLE_COMPACT_PROMPT,
    LARGE_WINDOW, ToolReminders, TurnConfig, compact_share, compact_turn, idle_compact_turn, run_turn, run_turn_with_tool_reminders,
};

pub const PROJECT_CONTRACT_REMINDER: &str = "Project contract reminder: The recent tool activity matches these documented capabilities: \
    {{capabilities}}. Check the project contract and use the appropriate capability if it applies, before improvising an alternative.";

/// What a subagent is told (docs/os.md §4.6: an editable prompt).
pub const SUBAGENT_NOTE: &str = "You are a subagent working for another agent, which does not receive this conversation automatically. \
Work autonomously. When you are done, reply with a concise but complete report of what you found or did; your report is returned to the other agent.";

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
/// The tiny, editable maintenance input used while background work runs.
pub const KEEP_WARM_NOTE: &str = "Your background work is still running; nothing to do yet. Reply only: ok";
pub const NOTICE_LOOP_WARNING: &str = "The session detected {{evidence}}. Automatic notices may slow or pause if this continues. Before acting on this report, check what has changed. Avoid repeating the same task or replying only to acknowledge a message. If you cannot identify a concrete next step, stop and tell the user what would unblock the work.";

/// What a subagent is told when you resume it after a turn that did not
/// finish (an editable prompt).
pub const SUBAGENT_RESUME: &str = "You were stopped partway through. Your work so far is above. Check where you left off, \
then continue the task and finish with your report.";

/// Introduces, in Task's description, the templates an agent may start as
/// remote subagents (docs/remote-subagents.md §6: an editable prompt).
pub const SUBAGENT_TEMPLATES: &str = "Templates (set template, and on when it lists several places): each starts the subagent as a \
session of its own in a place set up by the user: another worker, folder, repository or model, with its own tools. It does not \
receive this conversation or copies of your files automatically; give it a complete prompt. File access depends on where it runs \
and the tools and OS permissions available there; changes happen there. This is delegation, not isolation.";

/// The result of a remote subagent's call after this worker restarted
/// while the call waited (an editable prompt).
pub const SUBAGENT_REMOTE_REATTACH: &str = "This worker restarted while you waited, but the subagent runs elsewhere and keeps \
working: its report will arrive as a notice when it finishes. Carry on with other work, or wait for it.";

/// What the agent is told when its worker restarted mid-turn and its turn
/// goes on from the log (an editable prompt; docs/restarts.md §4).
pub const RESUME_AFTER_RESTART: &str = "carbon6's worker restarted while you were working (an update, most likely), so your turn \
was cut short where the conversation above ends. A command or subagent that was still running then was stopped; its result says \
so. Carry on with the task from there, running again whatever you still need.";

/// Editable recovery prompts: keep the cause distinct from the task's outcome.
pub const RESUME_AFTER_FAILURE: &str = "Your session stopped after a journal write failed; the worker did not restart. Your turn \
was cut short where the conversation above ends. Check what was saved and what is still running, then continue from there.";
pub const TASK_LOST_AFTER_RESTART: &str = "The worker restarted before this task's outcome could be recorded.";
pub const TASK_LOST_AFTER_FAILURE: &str = "The subagent's session stopped after a journal write failed; the worker did not restart.";
pub const TASK_LOST_UNKNOWN: &str =
    "This task was no longer being tracked when the session reloaded, before its outcome could be recorded.";
pub const TASK_LOST_WITH_CLI: &str = "The Claude Code process stopped before this task's outcome could be recorded. Files it changed \
stay as they are; check them before deciding whether to run the task again.";
pub const SUBAGENT_REMOTE_REATTACH_AFTER_RELOAD: &str = "This session reloaded while it waited. The subagent runs elsewhere and keeps \
working: its report will arrive as a notice when it finishes.";

/// Where a session's events go. Durable appends are journaled before they
/// return; ephemeral events are best-effort live updates.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Appends a durable event and returns its seq.
    async fn append(&self, kind: session_event::Kind) -> anyhow::Result<u64>;

    /// Records initial input receipts and their turn boundary before delivery.
    /// The default is sequential; the worker overrides it with one journal
    /// transaction so a failed boundary leaves the entire unsent batch queued.
    async fn start_turn(&self, turn: u64, inputs: &[PendingInput]) -> anyhow::Result<()> {
        for input in inputs {
            self.append(input.event()).await?;
        }
        self.append(session_event::Kind::TurnStarted(c6_proto::TurnStarted { turn, ..Default::default() })).await?;
        Ok(())
    }

    fn ephemeral(&self, kind: ephemeral_event::Kind);

    /// Optional preflight for a whole model-proposed batch. Entries are in
    /// call order (id, name, input JSON); an advisory at an index replaces
    /// that invocation's result. An empty vector defers nothing.
    fn defer_tools(&self, _calls: &[(String, String, String)], _is_available: &dyn Fn(&str) -> bool) -> Vec<Option<String>> {
        Vec::new()
    }

    /// A tool actually ran and returned an error, and its result has been
    /// recorded. Advisory observers must not block the turn or change its result.
    fn tool_failed(&self, _name: &str, _input_json: &str, _output: &str) {}
}

/// A user message waiting for the next model request boundary.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PendingInput {
    pub command_id: String,
    pub text: String,
    /// Images and files sent with it, and files it mentions (@path).
    pub attachments: Vec<c6_proto::Attachment>,
    /// Number of worker-generated notes appended to `attachments`.
    pub generated_attachment_count: u32,
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
    /// Producer-supplied display metadata; the original text stays unchanged.
    pub task_notice: Option<c6_proto::TaskNotice>,
    /// Known human input from the head's authenticated browser route.
    /// False for internal/legacy input, even when it has user authority.
    pub human_origin: bool,
    /// Send now: a running turn gets it at once, not at its next step
    /// (docs/background.md §4). Not logged.
    pub send_now: bool,
    /// Head-classified task shift: request old tool-output trimming at a safe boundary.
    pub trim_context: bool,
}

impl PendingInput {
    /// The event recording it.
    pub fn event(&self) -> c6_proto::session_event::Kind {
        c6_proto::session_event::Kind::User(c6_proto::UserMessage {
            command_id: self.command_id.clone(),
            text: self.text.clone(),
            attachments: self.attachments.clone(),
            generated_attachment_count: self.generated_attachment_count,
            command: self.command.clone(),
            prompt: self.prompt.clone(),
            blocked: self.blocked.clone(),
            delegated_by: self.delegated_by.clone(),
            wake: self.wake,
            from_session_id: self.from_session_id.clone(),
            from_title: self.from_title.clone(),
            hops: self.hops,
            api_origin: self.api_origin.clone(),
            task_notice: self.task_notice.clone(),
            human_origin: self.human_origin,
            origin_recorded: true,
            // The head restores task-card provenance from its durable command.
            task_card_origin: 0,
        })
    }

    /// What the model gets: the text (or its expansion), then each attachment.
    pub fn blocks(&self) -> Vec<c6_llm::Block> {
        history::user_blocks_with_generated_attachments(&self.attributed_text(), &self.attachments, self.generated_attachment_count)
    }

    /// The model-facing text with server-derived attribution. Kept separate
    /// from raw text and command expansions so hooks and logs retain them.
    pub fn attributed_text(&self) -> std::borrow::Cow<'_, str> {
        history::attributed_input_text(self.model_text(), &self.from_session_id, self.api_origin.as_ref(), self.human_origin)
    }

    pub fn model_text(&self) -> &str {
        if self.prompt.is_empty() { &self.text } else { &self.prompt }
    }

    /// `/compact`: summarize the conversation instead of running a turn.
    pub fn is_compact(&self) -> bool {
        self.command == "compact"
    }
}

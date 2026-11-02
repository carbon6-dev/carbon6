//! Claude Code for a conversation the head runs itself: Ask on a Claude
//! Code connection (docs/ask.md §3). The same runtime as a worker's Claude
//! Code sessions (`claude_code`), with the conversation's own tools served
//! over the MCP bridge, all of them loaded up front, and none of Claude
//! Code's: the head runs no commands and touches no files.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use c6_agent::{EventSink, PendingInput};
use c6_proto::{ErrorEvent, TurnEndReason, TurnEnded, session_event};
use c6_tools::{SessionTools, Tool};
use tokio_util::sync::CancellationToken;

use crate::claude_code::{Launch, Process, Stdin};
pub use crate::mcp_bridge::EventLog;
use crate::mcp_bridge::{Bridge, Entry};

/// What makes Claude Code's tools carbon6's only: none of its own, no MCP
/// server but carbon6's, no slash commands.
pub const LOCKED_DOWN: &[&str] = &["--tools", "", "--strict-mcp-config", "--disable-slash-commands"];

/// The `claude` this machine would run, if any.
pub fn claude_bin() -> Option<PathBuf> {
    crate::claude_code::find_claude()
}

/// One conversation's Claude Code process, kept between its turns.
#[derive(Default)]
pub struct HostedClaude {
    process: Option<Process>,
    stdin: Stdin,
    /// The last turn it ran, so an idle close can tell no turn came since.
    last_turn: u64,
}

/// A turn of a hosted conversation.
pub struct HostedTurn<'a> {
    /// The `claude` to run (none: the one found on this machine).
    pub bin: Option<PathBuf>,
    pub turn: u64,
    pub inputs: Vec<PendingInput>,
    /// The conversation's carbon6 id: also Claude Code's conversation.
    pub session_id: &'a str,
    /// It has turns already, so Claude Code's conversation exists.
    pub resume: bool,
    /// For a Claude Code conversation that turns out not to exist (its
    /// earlier turns ran on another model): the conversation so far.
    pub preamble: Option<String>,
    pub model: &'a str,
    pub effort: Option<String>,
    /// Its instructions, added to Claude Code's system prompt.
    pub system_prompt: String,
    /// Its tools, served over the bridge and all loaded up front.
    pub tools: Vec<Arc<dyn Tool>>,
    /// What Claude Code is told about the server (MCP `instructions`).
    pub instructions: String,
    pub log: Arc<dyn EventLog>,
    pub session_tools: Arc<SessionTools>,
    /// An empty folder of its own (no project instructions are read).
    pub cwd: &'a Path,
    pub sink: &'a Arc<dyn EventSink>,
    pub cancel: &'a CancellationToken,
}

/// The head's MCP bridge for hosted conversations (one, on first use).
async fn bridge() -> anyhow::Result<&'static Bridge> {
    static BRIDGE: tokio::sync::OnceCell<Bridge> = tokio::sync::OnceCell::const_new();
    BRIDGE.get_or_try_init(Bridge::start).await
}

impl HostedClaude {
    /// Runs one turn, starting Claude Code if it isn't running (or was
    /// started with other tools or instructions and has nothing under way).
    pub async fn turn(&mut self, t: HostedTurn<'_>) -> anyhow::Result<()> {
        self.last_turn = t.turn;
        let names: Vec<String> = t.tools.iter().map(|x| x.spec().name).collect();
        let launch = Launch {
            conversation: t.session_id.to_owned(),
            effort: t.effort.clone(),
            settings: None,
            system_prompt: Some(t.system_prompt.clone()).filter(|p| !p.trim().is_empty()),
            tools: names.clone(),
            worker: c6_core::build().to_owned(),
            extra_args: LOCKED_DOWN.iter().map(|a| (*a).to_owned()).collect(),
        };
        if let Some(p) = self.process.take_if(|p| p.launch() != &launch && p.idle_until(std::time::Duration::ZERO).is_some()) {
            p.close().await;
        }
        let failed = async |message: String| -> anyhow::Result<()> {
            for input in &t.inputs {
                t.sink.append(input.event()).await?;
            }
            t.sink.append(session_event::Kind::TurnStarted(c6_proto::TurnStarted { turn: t.turn, ..Default::default() })).await?;
            t.sink.append(session_event::Kind::Error(ErrorEvent { message })).await?;
            t.sink.append(session_event::Kind::TurnEnded(TurnEnded { turn: t.turn, reason: TurnEndReason::Error as i32 })).await?;
            Ok(())
        };
        let bridge = match bridge().await {
            Ok(b) => b,
            Err(e) => return failed(format!("could not serve carbon6's tools to Claude Code: {e:#}")).await,
        };
        let register = || {
            bridge.register(Entry {
                session_id: t.session_id.to_owned(),
                session: t.session_tools.clone(),
                tools: t.tools.clone(),
                cancel: Default::default(),
                journal: t.log.clone(),
                matched: Default::default(),
                policies: None,
                instructions: t.instructions.clone(),
                // Its own tools are off, tool search with them: all up front.
                core: names.clone(),
            })
        };
        let (mut p, bin) = match self.process.take() {
            Some(p) => (p, None),
            None => {
                let Some(bin) = t.bin.clone().or_else(claude_bin) else {
                    return failed(
                        "Claude Code (`claude`) is not on the head machine's PATH. Install it there and sign in with `claude auth login`, or \
                         pick another model for this conversation."
                            .into(),
                    )
                    .await;
                };
                match Process::spawn(&bin, launch.clone(), t.resume, t.cwd, t.model, self.stdin.clone(), Some(register()), None) {
                    Ok(p) => (p, Some(bin)),
                    Err(e) => return failed(format!("failed to start Claude Code: {e}")).await,
                }
            }
        };
        p.set_turn_cancel(t.cancel);
        let more = tokio::sync::Notify::new();
        let feed = Vec::new;
        let mut end = p.turn(t.turn, t.inputs.clone(), false, None, t.model, t.sink, &feed, &more, t.cancel).await?;
        // Asked to resume a conversation Claude Code doesn't have: a new one.
        if end.no_conversation
            && let Some(bin) = &bin
        {
            drop(p);
            p = match Process::spawn(bin, launch, false, t.cwd, t.model, self.stdin.clone(), Some(register()), None) {
                Ok(p) => p,
                Err(e) => return failed(format!("failed to start Claude Code: {e}")).await,
            };
            p.set_turn_cancel(t.cancel);
            end = p.turn(t.turn, t.inputs, true, t.preamble.as_deref(), t.model, t.sink, &feed, &more, t.cancel).await?;
        }
        if end.alive {
            self.process = Some(p);
        }
        Ok(())
    }

    /// The last turn it ran.
    pub fn last_turn(&self) -> u64 {
        self.last_turn
    }

    /// Ends its process when it has nothing under way (no background
    /// tasks): after `idle` with no turn, the conversation's next turn
    /// starts it again, resuming.
    pub async fn close_if_idle(&mut self) {
        if self.process.as_ref().is_some_and(|p| p.idle_until(std::time::Duration::ZERO).is_some()) {
            self.close().await;
        }
    }

    /// Ends its process (the conversation goes on with the next turn).
    pub async fn close(&mut self) {
        if let Some(p) = self.process.take() {
            p.close().await;
        }
    }
}

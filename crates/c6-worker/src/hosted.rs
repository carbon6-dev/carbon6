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
            // Locked down by its own flags: none of Claude Code's tools.
            native: None,
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
        let feed = crate::claude_code::Joining::default;
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

/// Claude Code answering one call, for what the head asks a model itself
/// (a work item's note, a project's, a guess at its project): `claude -p`
/// with the prompt on stdin and one JSON result out, locked down as Ask is
/// and in an empty folder of its own.
pub struct ClaudeOnce {
    bin: PathBuf,
}

impl ClaudeOnce {
    pub fn new(bin: PathBuf) -> Self {
        Self { bin }
    }
}

/// The conversation as one prompt: a single message as it is, more as a
/// transcript.
fn once_prompt(req: &c6_llm::ModelRequest) -> String {
    let text = |m: &c6_llm::Message| -> String {
        m.blocks.iter().filter_map(|b| if let c6_llm::Block::Text(t) = b { Some(t.as_str()) } else { None }).collect::<Vec<_>>().join("\n")
    };
    if let [only] = req.messages.as_slice() {
        return text(only);
    }
    req.messages
        .iter()
        .map(|m| format!("{}: {}", if m.role == c6_llm::Role::User { "User" } else { "Assistant" }, text(m)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[async_trait::async_trait]
impl c6_llm::ModelClient for ClaudeOnce {
    async fn stream(&self, req: &c6_llm::ModelRequest) -> Result<c6_llm::EventStream, c6_llm::LlmError> {
        use c6_llm::{LlmError, ModelEvent, Usage};
        use tokio::io::AsyncWriteExt;
        let dir = std::env::temp_dir().join(format!("c6-claude-once-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).map_err(|e| LlmError::Config(format!("making a folder for Claude Code: {e}")))?;
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.args(["-p", "--output-format", "json"]).args(LOCKED_DOWN);
        if !req.model.trim().is_empty() {
            cmd.args(["--model", req.model.trim()]);
        }
        if !req.system.trim().is_empty() {
            cmd.args(["--append-system-prompt", &req.system]);
        }
        cmd.current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let result = async {
            let mut child = cmd.spawn().map_err(|e| LlmError::Config(format!("failed to start Claude Code: {e}")))?;
            let mut stdin = child.stdin.take().expect("piped");
            stdin.write_all(once_prompt(req).as_bytes()).await.map_err(|e| LlmError::Network(format!("writing to Claude Code: {e}")))?;
            drop(stdin);
            let out = child.wait_with_output().await.map_err(|e| LlmError::Network(format!("reading Claude Code: {e}")))?;
            let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| {
                let err = String::from_utf8_lossy(&out.stderr);
                LlmError::Protocol(format!(
                    "Claude Code exited ({}) with no result: {}",
                    out.status,
                    err.trim().chars().take(400).collect::<String>()
                ))
            })?;
            let text = v["result"].as_str().unwrap_or_default().to_owned();
            if v["is_error"].as_bool() == Some(true) || !out.status.success() {
                return Err(LlmError::Protocol(format!("Claude Code: {}", text.chars().take(400).collect::<String>())));
            }
            let n = |k: &str| v["usage"][k].as_u64().unwrap_or(0);
            let usage = Usage {
                input_tokens: n("input_tokens"),
                output_tokens: n("output_tokens"),
                cache_read_tokens: n("cache_read_input_tokens"),
                cache_write_tokens: n("cache_creation_input_tokens"),
                reasoning_tokens: 0,
            };
            Ok((text, usage))
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        let (text, usage) = result?;
        let events =
            vec![Ok(ModelEvent::TextDelta(text)), Ok(ModelEvent::Usage(usage)), Ok(ModelEvent::Done { stop_reason: "end_turn".into() })];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

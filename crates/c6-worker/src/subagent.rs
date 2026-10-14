//! Subagents: `Task` starts a child session on this worker with a fresh
//! conversation and the parent's tools (filtered by the named agent). The
//! child is a session like any other, with its own actor and log, linked
//! to the parent call (docs/subagents.md). Its log syncs to the head like
//! any session's.

use std::sync::Arc;

use async_trait::async_trait;
use c6_agent::{EventSink, PendingInput, TurnConfig};
use c6_llm::ModelClient;
use c6_proto::{CreateSession, Delegation, ModelSpec, SessionStarted, SubagentSpawned, TurnEndReason, session_event};
use c6_tools::{AgentDef, SendMessage, Subagents, Task, Tool};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::journal::Journal;
use crate::session::{SessionManager, Writer};

/// How deep subagents nest (the session itself is depth 0) and how many
/// delegations may run at once in one tree, unless the spec says.
fn limits(spec: &CreateSession) -> (u32, usize) {
    let depth = if spec.max_subagent_depth == 0 { c6_api::DEFAULT_SUBAGENT_DEPTH } else { spec.max_subagent_depth };
    let running = if spec.max_running_subagents == 0 { c6_api::DEFAULT_RUNNING_SUBAGENTS } else { spec.max_running_subagents };
    (depth, running as usize)
}

/// Whether a session at `depth` may start or continue subagents.
pub(crate) fn may_delegate(spec: &CreateSession, depth: u32) -> bool {
    depth < limits(spec).0
}

/// The session a subagent is started from.
pub struct Parent {
    pub session_id: String,
    pub spec: CreateSession,
    pub started: SessionStarted,
    /// The parent's log (for `SubagentSpawned`).
    pub log: Arc<Writer>,
    pub manager: SessionManager,
}

pub struct Runner {
    parent: Parent,
    cfg: TurnConfig,
    tools: Vec<Arc<dyn Tool>>,
}

impl Runner {
    pub fn new(parent: Parent, cfg: &TurnConfig, _client: Arc<dyn ModelClient>, tools: &[Arc<dyn Tool>]) -> Self {
        Self { parent, cfg: cfg.clone(), tools: tools.to_vec() }
    }
}

/// What a subagent's journaled spec says about its agent (a reloaded
/// subagent keeps the tools its `SessionStarted` lists).
#[derive(serde::Serialize, serde::Deserialize)]
struct AgentJson {
    name: String,
    #[serde(default)]
    tools: Vec<String>,
}

/// The tools a subagent has, of its parent's: those its named agent
/// allows, and none that start or continue subagents at the depth limit.
fn filter_tools(spec: &CreateSession, tools: Vec<Arc<dyn Tool>>, agent: Option<&AgentDef>, depth: u32) -> Vec<Arc<dyn Tool>> {
    tools
        .into_iter()
        .filter(|t| {
            let name = t.spec().name;
            let delegates = Task::is_task(&name) || SendMessage::is_send(&name);
            !(delegates && !may_delegate(spec, depth)) && agent.is_none_or(|a| a.allows(&name))
        })
        .collect()
}

#[async_trait]
impl Subagents for Runner {
    async fn run(
        &self,
        parent_call_id: &str,
        description: &str,
        prompt: &str,
        agent: Option<&AgentDef>,
        background: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let child_id = uuid::Uuid::now_v7().to_string();
        let p = &self.parent;
        let depth = p.spec.delegation.as_ref().map_or(0, |d| d.depth) + 1;
        let root = p.spec.delegation.as_ref().map_or(p.session_id.as_str(), |d| d.root_session_id.as_str()).to_owned();
        let tools = filter_tools(&p.spec, self.tools.clone(), agent, depth);
        let _working = self.permit(&root)?;
        let mut model = self.cfg.model.clone();
        if !p.spec.subagent_model.trim().is_empty() {
            model = p.spec.subagent_model.trim().to_owned();
        }
        let mut system_prompt = p.started.system_prompt.clone();
        if let Some(a) = agent {
            // The agent's instructions are its system prompt.
            system_prompt = format!(
                "{}\n\n# Environment\n\nWorking directory: {}\nToday's date: {}",
                a.prompt,
                p.started.cwd,
                time::OffsetDateTime::now_utc().date()
            );
            let kind = p.spec.model.as_ref().map(|m| m.connection_kind.as_str()).unwrap_or_default();
            if let Some(m) = agent_model(&a.model, kind) {
                model = m;
            }
        }
        let model_spec = p.spec.model.clone().map(|m| ModelSpec { model: model.clone(), ..m });
        let started = SessionStarted {
            title: match agent {
                Some(a) => format!("{description} ({})", a.name),
                None => description.to_owned(),
            },
            parent_session_id: p.session_id.clone(),
            parent_call_id: parent_call_id.to_owned(),
            tools: tools.iter().map(|t| t.spec().name).collect(),
            system_prompt,
            model: model_spec.clone(),
            agent: agent.map(|a| a.name.clone()).unwrap_or_default(),
            depth,
            ..p.started.clone()
        };
        let delegation = Delegation {
            parent_session_id: p.session_id.clone(),
            parent_call_id: parent_call_id.to_owned(),
            root_session_id: root,
            agent_json: agent
                .map(|a| serde_json::to_string(&AgentJson { name: a.name.clone(), tools: a.tools.clone() }).unwrap_or_default())
                .unwrap_or_default(),
            depth,
            observed: false,
        };
        // Subagents start fresh: a side chat's seed is not theirs.
        let spec = CreateSession { seed: Vec::new(), model: model_spec, delegation: Some(delegation), ..p.spec.clone() };

        // The parent's log links to the child before the child's exists.
        p.log
            .append(session_event::Kind::SubagentSpawned(SubagentSpawned {
                call_id: parent_call_id.to_owned(),
                child_session_id: child_id.clone(),
                agent: agent.map(|a| a.name.clone()).unwrap_or_default(),
                description: description.to_owned(),
                background_task_id: background.unwrap_or_default().to_owned(),
            }))
            .await
            .map_err(|e| format!("{e:#}"))?;
        let child = p.manager.start_subagent(&child_id, spec, started, tools).await.map_err(|e| format!("{e:#}"))?;
        info!(parent = %p.session_id, child = %child_id, %description, "subagent started");

        let note = crate::session::spec_prompt(&p.spec, "session.subagent", c6_agent::SUBAGENT_NOTE);
        let input = PendingInput {
            command_id: format!("delegation:{}", uuid::Uuid::now_v7()),
            text: prompt.to_owned(),
            prompt: format!("{prompt}\n\n---\n{note}"),
            delegated_by: parent_call_id.to_owned(),
            ..Default::default()
        };
        self.delegate(&child_id, &child, input, cancel).await
    }

    async fn running(&self, to: &str) -> Result<bool, String> {
        let child = self.resolve(to).await?;
        Ok(self.parent.manager.is_running(&child))
    }

    async fn send(
        &self,
        call_id: &str,
        to: &str,
        message: &str,
        background: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<Option<String>, String> {
        let p = &self.parent;
        let child_id = self.resolve(to).await?;
        let child = match p.manager.actor_for(&child_id).await {
            Ok(Some(c)) => c,
            Ok(None) => return Err(format!("subagent {to} is no longer on this worker")),
            Err(e) => return Err(format!("subagent {to} could not be loaded: {e:#}")),
        };
        let input = PendingInput {
            command_id: format!("delegation:{}", uuid::Uuid::now_v7()),
            text: message.to_owned(),
            delegated_by: call_id.to_owned(),
            ..Default::default()
        };
        // Running: the message joins its turn; the report comes as it would.
        if p.manager.is_running(&child_id) && background.is_none() {
            child.send(crate::session::ActorMsg::Delegate(Box::new(input), None)).map_err(|_| format!("subagent {to} stopped"))?;
            return Ok(None);
        }
        let root = p.spec.delegation.as_ref().map_or(p.session_id.as_str(), |d| d.root_session_id.as_str()).to_owned();
        let _working = self.permit(&root)?;
        self.delegate(&child_id, &child, input, cancel).await.map(Some)
    }

    async fn peek(&self, to: &str) -> Result<String, String> {
        let p = &self.parent;
        let child_id = self.resolve(to).await?;
        let journal = p.manager.journal();
        let status = if p.manager.is_running(&child_id) {
            "running"
        } else {
            let events = journal.events_after(&child_id, 0, u32::MAX).await.map_err(|e| format!("{e:#}"))?;
            match events.iter().rev().find_map(|e| match &e.kind {
                Some(session_event::Kind::TurnEnded(t)) => Some(TurnEndReason::try_from(t.reason).unwrap_or_default()),
                _ => None,
            }) {
                Some(r) => status_of(r, "x"),
                None => "not started",
            }
        };
        Ok(digest(journal, &child_id, status, "").await)
    }
}

impl Runner {
    /// Counts a delegation against its tree's limit while it runs.
    fn permit(&self, root: &str) -> Result<crate::session::Working, String> {
        let max = limits(&self.parent.spec).1;
        self.parent.manager.working(root, max).ok_or_else(|| {
            format!("{max} subagents are already working in this session; wait for one to finish (or stop one) before starting another")
        })
    }

    /// The session id of one of this session's subagents, named by its
    /// session id or background task id (it must have been started here).
    async fn resolve(&self, to: &str) -> Result<String, String> {
        let p = &self.parent;
        let events = p.manager.journal().events_after(&p.session_id, 0, u32::MAX).await.map_err(|e| format!("{e:#}"))?;
        events
            .iter()
            .find_map(|e| match &e.kind {
                Some(session_event::Kind::SubagentSpawned(sp))
                    if !to.is_empty() && (sp.child_session_id == to || sp.background_task_id == to) =>
                {
                    Some(sp.child_session_id.clone())
                }
                _ => None,
            })
            .ok_or_else(|| format!("no subagent {to:?} was started in this session"))
    }

    /// Gives the subagent `input` and waits for the turn that takes it:
    /// its report, or (as `Err`) a digest of what it did.
    async fn delegate(
        &self,
        child_id: &str,
        child: &tokio::sync::mpsc::UnboundedSender<crate::session::ActorMsg>,
        input: PendingInput,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let p = &self.parent;
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        child.send(crate::session::ActorMsg::Delegate(Box::new(input), Some(tx))).map_err(|_| "the subagent stopped before it started".to_owned())?;
        // Stopping the parent's turn (or this background task) stops the
        // subagent's turn; its log keeps what it did. The delegation's
        // outcome follows once that turn closes.
        let mut guard = InterruptOnDrop(Some(child.clone()));
        let mut stopped_here = false;
        let outcome = tokio::select! {
            o = &mut rx => o,
            _ = cancel.cancelled() => {
                stopped_here = true;
                let _ = child.send(crate::session::ActorMsg::Interrupt);
                (&mut rx).await
            }
        };
        guard.0 = None;
        let journal = p.manager.journal();
        match outcome {
            Ok(o) if o.reason == TurnEndReason::Completed && !o.report.trim().is_empty() => {
                Ok(format!("{}\n\n[subagent session {child_id}]", o.report))
            }
            Ok(o) => {
                let status = status_of(o.reason, &o.report);
                // Stopped from elsewhere than this call: by you.
                let by = if o.reason == TurnEndReason::Interrupted && !stopped_here { "user" } else { "" };
                Err(partial(&p.spec, &digest(journal, child_id, status, by).await))
            }
            Err(_) => Err(partial(&p.spec, &digest(journal, child_id, "failed", "").await)),
        }
    }
}

/// How a delegation's turn ended, as the digest names it.
pub(crate) fn status_of(reason: TurnEndReason, report: &str) -> &'static str {
    match reason {
        TurnEndReason::Completed if report.trim().is_empty() => "completed without a report",
        TurnEndReason::Completed => "completed",
        TurnEndReason::Interrupted => "interrupted",
        TurnEndReason::WorkerRestarted => "lost",
        _ => "failed",
    }
}

/// A digest followed by what the agent is told about it (an editable
/// prompt).
pub(crate) fn partial(spec: &CreateSession, digest: &str) -> String {
    format!("{digest}\n{}", crate::session::spec_prompt(spec, "session.subagent_partial", c6_agent::SUBAGENT_PARTIAL))
}

/// What a subagent did, from its log, for an agent that did not get its
/// report: how far it got, the files it changed (by its own calls, so other
/// agents' edits do not show), its errors and its own subagents.
pub(crate) async fn digest(journal: &Journal, child_id: &str, status: &str, by: &str) -> String {
    let events = journal.events_after(child_id, 0, u32::MAX).await.unwrap_or_default();
    let mut turns = 0;
    let mut calls = 0;
    let mut last_text = String::new();
    let mut edits: Vec<(String, Vec<String>)> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut children: Vec<(String, String)> = Vec::new();
    for e in &events {
        match &e.kind {
            Some(session_event::Kind::TurnStarted(_)) => turns += 1,
            Some(session_event::Kind::Assistant(a)) => {
                for b in &a.blocks {
                    match &b.block {
                        Some(c6_proto::content_block::Block::Text(t)) if !t.trim().is_empty() => last_text = t.trim().to_owned(),
                        Some(c6_proto::content_block::Block::ToolUse(u)) => {
                            let paths = edited_paths(&u.name, &u.input_json);
                            if !paths.is_empty() {
                                edits.push((u.id.clone(), paths));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some(session_event::Kind::ToolResult(r)) => {
                calls += 1;
                if r.is_error {
                    failed.push(r.call_id.clone());
                    errors.push(format!("{}: {}", r.tool, first_line(&r.output)));
                } else if let Some((_, paths)) = edits.iter().find(|(id, _)| id == &r.call_id) {
                    for p in paths {
                        if !changed.contains(p) {
                            changed.push(p.clone());
                        }
                    }
                }
            }
            Some(session_event::Kind::Error(err)) => errors.push(first_line(&err.message)),
            Some(session_event::Kind::SubagentSpawned(sp)) => children.push((sp.child_session_id.clone(), sp.description.clone())),
            _ => {}
        }
    }
    let elapsed = match (events.first(), events.last()) {
        (Some(a), Some(b)) => crate::session::duration_text(((b.ts_ms - a.ts_ms).max(0) / 1000) as u64),
        _ => "0s".into(),
    };
    let by = if by.is_empty() { String::new() } else { format!(" by=\"{by}\"") };
    let mut out = format!("<subagent id=\"{child_id}\" status=\"{status}\"{by} turns=\"{turns}\" tool_calls=\"{calls}\" elapsed=\"{elapsed}\">\n");
    if !last_text.is_empty() {
        out.push_str(&format!("Last message: {}\n", clip(&last_text, 800)));
    }
    if !changed.is_empty() {
        out.push_str(&format!("Changed: {}\n", changed.join(", ")));
    }
    if !errors.is_empty() {
        out.push_str(&format!("Errors: {} (last: {})\n", errors.len(), clip(errors.last().unwrap(), 200)));
    }
    if !children.is_empty() {
        let mut list = Vec::new();
        for (id, description) in &children {
            let ev = journal.events_after(id, 0, u32::MAX).await.unwrap_or_default();
            let state = ev
                .iter()
                .rev()
                .find_map(|e| match &e.kind {
                    Some(session_event::Kind::TurnEnded(t)) => Some(status_of(TurnEndReason::try_from(t.reason).unwrap_or_default(), "x")),
                    Some(session_event::Kind::TurnStarted(_)) => Some("running"),
                    _ => None,
                })
                .unwrap_or("not started");
            list.push(format!("{id} \"{description}\" ({state})"));
        }
        out.push_str(&format!("Subagents: {}\n", list.join(", ")));
    }
    out.push_str("</subagent>");
    out
}

/// The files a call to a file-changing tool names.
fn edited_paths(tool: &str, input_json: &str) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(input_json).unwrap_or_default();
    match tool.to_ascii_lowercase().as_str() {
        "write" | "edit" | "multiedit" | "notebookedit" => {
            v["file_path"].as_str().or(v["path"].as_str()).map(|p| vec![p.to_owned()]).unwrap_or_default()
        }
        "apply_patch" => v["input"]
            .as_str()
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                ["*** Add File: ", "*** Update File: ", "*** Delete File: ", "*** Move to: "].iter().find_map(|p| l.strip_prefix(p))
            })
            .map(|p| p.trim().to_owned())
            .collect(),
        _ => Vec::new(),
    }
}

fn first_line(s: &str) -> String {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().trim().to_owned()
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

/// Interrupts the subagent if the call is dropped before it returns.
struct InterruptOnDrop(Option<tokio::sync::mpsc::UnboundedSender<crate::session::ActorMsg>>);

impl Drop for InterruptOnDrop {
    fn drop(&mut self) {
        if let Some(c) = &self.0 {
            let _ = c.send(crate::session::ActorMsg::Interrupt);
        }
    }
}

/// The model a named agent asks for, where it can have it: Claude aliases
/// and ids on an Anthropic connection. Otherwise it runs on the parent's.
fn agent_model(wanted: &str, connection_kind: &str) -> Option<String> {
    if connection_kind != "anthropic" {
        return None;
    }
    match wanted.trim() {
        "" | "inherit" => None,
        "opus" => Some("claude-opus-5-5".into()),
        "sonnet" => Some("claude-sonnet-5-5".into()),
        "haiku" => Some("claude-haiku-4-5".into()),
        id if id.starts_with("claude-") => Some(id.into()),
        _ => None,
    }
}

/// The parent's tool hooks; Stop hooks are for the main session only.
pub(crate) struct SubagentHooks(pub(crate) Arc<dyn c6_tools::Hooks>);

#[async_trait]
impl c6_tools::Hooks for SubagentHooks {
    async fn pre_tool_use(&self, call_id: &str, tool: &str, input: &serde_json::Value) -> Option<String> {
        self.0.pre_tool_use(call_id, tool, input).await
    }

    async fn post_tool_use(&self, tool: &str, input: &serde_json::Value, output: &str, is_error: bool) -> Option<String> {
        self.0.post_tool_use(tool, input, output, is_error).await
    }

    async fn stop(&self, _active: bool) -> Option<String> {
        None
    }
}

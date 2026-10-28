//! Subagents: `Task` starts a child session on this worker with a fresh
//! conversation and the parent's tools (filtered by the named agent). The
//! child is a session like any other, with its own actor and log, linked
//! to the parent call (docs/subagents.md). Its log syncs to the head like
//! any session's.

use std::sync::Arc;

use async_trait::async_trait;
use c6_agent::{EventSink, PendingInput};
use c6_api::SubagentConnection;
use c6_llm::{Provider, ToolProfile, model_info};
use c6_proto::{CreateSession, Delegation, ModelSpec, SessionStarted, SubagentSpawned, TurnEndReason, session_event};
use c6_tools::{AgentDef, ModelChoice, SendMessage, Skill, SkillTool, Subagents, Task, Tool};
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

/// A session's default for its subagents: from the head (a routing rule,
/// or picked in the session), changed by `SetSubagentModel`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubagentDefault {
    /// A connection, model and effort.
    pub target: Option<ModelSpec>,
    /// A model on the session's own connection (a routing rule, from a
    /// head that sends no target).
    pub model: String,
}

impl SubagentDefault {
    pub fn of(spec: &CreateSession) -> Self {
        Self { target: spec.subagent_target.clone(), model: spec.subagent_model.trim().to_owned() }
    }
}

/// Shared by a session's actor and its turn's runner, so a change applies
/// to the next `Task` call, also in a running turn.
pub type SharedDefault = Arc<std::sync::Mutex<SubagentDefault>>;

/// The session a subagent is started from.
pub struct Parent {
    pub session_id: String,
    pub spec: CreateSession,
    pub started: SessionStarted,
    /// The parent's log (for `SubagentSpawned`).
    pub log: Arc<Writer>,
    pub manager: SessionManager,
    pub default: SharedDefault,
}

const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The connection, model and effort a subagent runs on. In order, each
/// overriding the one before: its parent's model, the session's default
/// for subagents, a named agent's model, and what the `Task` call asks for.
/// Only `connections` (those the session's realm may use, never Claude
/// Code) or the parent's own may be used: a subagent runs in carbon6's own
/// loop.
pub fn resolve_model(
    parent: &ModelSpec,
    default: &SubagentDefault,
    agent_model: &str,
    choice: &ModelChoice,
    connections: &[SubagentConnection],
) -> Result<ModelSpec, String> {
    let names = || {
        if connections.is_empty() { "none".to_owned() } else { connections.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ") }
    };
    let find = |want: &str| -> Option<&SubagentConnection> {
        let want = want.trim();
        connections.iter().find(|c| c.id == want).or_else(|| connections.iter().find(|c| c.name.eq_ignore_ascii_case(want)))
    };
    // `connection/model`, where the part before a slash names a connection
    // (model ids may have slashes of their own).
    let split = |s: &str| -> Option<(&SubagentConnection, String)> {
        s.match_indices('/').find_map(|(i, _)| find(&s[..i]).map(|c| (c, s[i + 1..].trim().to_owned()))).filter(|(_, m)| !m.is_empty())
    };
    // `model` on `c`; the effort carries over on the same connection.
    let on = |m: &ModelSpec, c: &SubagentConnection, model: String| ModelSpec {
        connection_id: c.id.clone(),
        model,
        max_output_tokens: if c.id == m.connection_id { m.max_output_tokens } else { 0 },
        effort: if c.id == m.connection_id { m.effort.clone() } else { String::new() },
        connection_kind: c.kind.clone(),
    };

    let mut m = parent.clone();
    if let Some(t) = &default.target {
        m = t.clone();
    } else if !default.model.is_empty() {
        m.model = default.model.clone();
    }
    let wanted = agent_model.trim();
    if !wanted.is_empty() && wanted != "inherit" {
        if let Some((c, model)) = split(wanted) {
            m = on(&m, c, model);
        } else if let Some(id) = agent_alias(wanted, &m.connection_kind) {
            m.model = id;
        }
    }
    if !choice.connection.is_empty() {
        let c = find(&choice.connection)
            .ok_or_else(|| format!("no connection {:?} for subagents; available: {}", choice.connection, names()))?;
        let model = match split(&choice.model) {
            Some((named, model)) if named.id == c.id => model,
            _ if !choice.model.is_empty() => choice.model.clone(),
            _ if c.id == m.connection_id => m.model.clone(),
            _ if !c.default_model.is_empty() => c.default_model.clone(),
            _ => return Err(format!("the connection {} has no default model: name the model to run on it", c.name)),
        };
        m = on(&m, c, model);
    } else if !choice.model.is_empty() {
        match split(&choice.model) {
            Some((c, model)) => m = on(&m, c, model),
            None => m.model = choice.model.clone(),
        }
    }
    if !choice.effort.is_empty() {
        if !EFFORTS.contains(&choice.effort.as_str()) {
            return Err(format!("effort must be one of {}", EFFORTS.join(", ")));
        }
        m.effort = choice.effort.clone();
    }
    match connections.iter().find(|c| c.id == m.connection_id) {
        Some(c) => m.connection_kind = c.kind.clone(),
        None if m.connection_id == parent.connection_id && m.connection_kind != "claude_code" => {}
        None if m.connection_kind == "claude_code" || m.connection_id == parent.connection_id => {
            return Err(format!(
                "subagents run in carbon6's own loop, not on Claude Code: name a connection to run this one on ({}), or set the session's subagent model",
                names()
            ));
        }
        None => return Err(format!("that connection is not available to this session's subagents; available: {}", names())),
    }
    if m.model.trim().is_empty() {
        return Err("no model to run the subagent on: name one".into());
    }
    Ok(m)
}

pub struct Runner {
    parent: Parent,
    /// The parent's tools, which subagents on a model of the same tool
    /// profile share.
    tools: Vec<Arc<dyn Tool>>,
}

impl Runner {
    pub fn new(parent: Parent, tools: &[Arc<dyn Tool>]) -> Self {
        Self { parent, tools: tools.to_vec() }
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
            // What a subagent finds goes back to its parent, not into task cards.
            !(delegates && !may_delegate(spec, depth)) && !c6_tools::knowledge::is_task_card(&name) && agent.is_none_or(|a| a.allows(&name))
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
        choice: &ModelChoice,
        background: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let child_id = uuid::Uuid::now_v7().to_string();
        let p = &self.parent;
        let depth = p.spec.delegation.as_ref().map_or(0, |d| d.depth) + 1;
        let root = p.spec.delegation.as_ref().map_or(p.session_id.as_str(), |d| d.root_session_id.as_str()).to_owned();
        let default = p.default.lock().unwrap().clone();
        let parent_model = p.spec.model.clone().unwrap_or_default();
        let connections = crate::session::subagent_connections(&p.spec);
        let model_spec = resolve_model(&parent_model, &default, agent.map_or("", |a| a.model.as_str()), choice, &connections)?;
        let _working = self.permit(&root)?;
        let provider = Provider::from_kind(&model_spec.connection_kind).unwrap_or(Provider::OpenAiChat);
        let info = model_info(provider, &model_spec.model);
        let profile = info.tool_profile;
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
        // Subagents start fresh: a side chat's seed is not theirs. Their
        // own subagents get the same default.
        let spec = CreateSession {
            seed: Vec::new(),
            model: Some(model_spec.clone()),
            delegation: Some(delegation),
            subagent_target: default.target.clone(),
            subagent_model: default.model.clone(),
            ..p.spec.clone()
        };
        let claude_code_parent = p.started.tool_profile == crate::claude_code::PROFILE;
        // The parent's tools where its model takes the same profile; else
        // (another model family, or a Claude Code parent, whose tools
        // carbon6 does not run) tools of its own in its model's profile.
        let (tools, skills) = if p.started.tool_profile == profile.as_str() {
            (filter_tools(&p.spec, self.tools.clone(), agent, depth), None)
        } else {
            let built = p.manager.session_tool_set(&child_id, &spec, &p.started, profile).await;
            for e in &built.errors {
                tracing::warn!(child = %child_id, "subagent setup: {e}");
            }
            (filter_tools(&p.spec, built.tools, agent, depth), Some(built.skills))
        };
        let mut system_prompt = match &skills {
            None => crate::session::without_task_cards(&p.started.system_prompt),
            Some(skills) => {
                let base = if claude_code_parent {
                    // Claude Code's prompt is its own: write the one a
                    // carbon6 session would have.
                    let labels = crate::session::session_labels(
                        &spec.labels,
                        &p.started.repo,
                        &p.started.cwd,
                        &p.started.home,
                        &model_spec.model,
                        &p.started.ssh_target,
                    );
                    let date = time::OffsetDateTime::now_utc().date().to_string();
                    let mut prompt = c6_agent::build_system_prompt(&p.spec.base_prompt, std::path::Path::new(&p.started.cwd), &date);
                    prompt.push_str(&crate::session::context_sections(&spec, &labels));
                    prompt
                } else {
                    crate::session::without_skills(&crate::session::without_task_cards(&p.started.system_prompt))
                };
                if skills.is_empty() || !tools.iter().any(|t| t.spec().name == SkillTool::tool_name(profile == ToolProfile::Claude)) {
                    base
                } else {
                    format!("{base}\n\n{}", Skill::index(skills, SkillTool::tool_name(profile == ToolProfile::Claude)))
                }
            }
        };
        if let Some(a) = agent {
            // The agent's instructions are its system prompt.
            system_prompt = format!(
                "{}\n\n# Environment\n\nWorking directory: {}\nToday's date: {}",
                a.prompt,
                p.started.cwd,
                time::OffsetDateTime::now_utc().date()
            );
        }
        let started = SessionStarted {
            title: match agent {
                Some(a) => format!("{description} ({})", a.name),
                None => description.to_owned(),
            },
            parent_session_id: p.session_id.clone(),
            parent_call_id: parent_call_id.to_owned(),
            tools: tools.iter().map(|t| t.spec().name).collect(),
            system_prompt,
            model: Some(model_spec.clone()),
            tool_profile: profile.as_str().to_owned(),
            context_window: info.context_window,
            agent: agent.map(|a| a.name.clone()).unwrap_or_default(),
            depth,
            ..p.started.clone()
        };

        // The parent's log links to the child before the child's exists.
        p.log
            .append(session_event::Kind::SubagentSpawned(SubagentSpawned {
                call_id: parent_call_id.to_owned(),
                child_session_id: child_id.clone(),
                agent: agent.map(|a| a.name.clone()).unwrap_or_default(),
                description: description.to_owned(),
                background_task_id: background.unwrap_or_default().to_owned(),
                model: Some(model_spec.clone()),
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
        child
            .send(crate::session::ActorMsg::Delegate(Box::new(input), Some(tx)))
            .map_err(|_| "the subagent stopped before it started".to_owned())?;
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
    let mut out =
        format!("<subagent id=\"{child_id}\" status=\"{status}\"{by} turns=\"{turns}\" tool_calls=\"{calls}\" elapsed=\"{elapsed}\">\n");
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

/// The model a named agent asks for by alias, where it can have it: Claude
/// aliases and ids on an Anthropic connection (`connection/model` names
/// any connection).
fn agent_alias(wanted: &str, connection_kind: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: &str, name: &str, kind: &str, default_model: &str) -> SubagentConnection {
        SubagentConnection { id: id.into(), name: name.into(), kind: kind.into(), default_model: default_model.into() }
    }

    fn spec(connection_id: &str, kind: &str, model: &str, effort: &str) -> ModelSpec {
        ModelSpec {
            connection_id: connection_id.into(),
            model: model.into(),
            max_output_tokens: 0,
            effort: effort.into(),
            connection_kind: kind.into(),
        }
    }

    fn choice(connection: &str, model: &str, effort: &str) -> ModelChoice {
        ModelChoice { connection: connection.into(), model: model.into(), effort: effort.into() }
    }

    #[test]
    fn resolves_where_a_subagent_runs() {
        let conns = vec![
            conn("c1", "work", "anthropic", "claude-sonnet-5-5"),
            conn("c2", "blacktitan ChatGPT", "chatgpt", "gpt-6.1"),
            conn("c3", "router", "openai_compat", ""),
        ];
        let parent = spec("c1", "anthropic", "claude-opus-5-5", "high");
        let none = SubagentDefault::default();
        let no = ModelChoice::default();
        // The parent's, effort and all.
        assert_eq!(resolve_model(&parent, &none, "", &no, &conns).unwrap(), parent);
        // A routing rule's model on the session's connection keeps its effort.
        let rule = SubagentDefault { target: None, model: "claude-haiku-4-5".into() };
        assert_eq!(resolve_model(&parent, &rule, "", &no, &conns).unwrap(), spec("c1", "anthropic", "claude-haiku-4-5", "high"));
        // The session's default target wins over the rule's model.
        let picked = SubagentDefault { target: Some(spec("c2", "chatgpt", "gpt-6.1-sol", "medium")), model: "x".into() };
        assert_eq!(resolve_model(&parent, &picked, "", &no, &conns).unwrap(), spec("c2", "chatgpt", "gpt-6.1-sol", "medium"));
        // A named agent's alias applies on an Anthropic connection only.
        assert_eq!(resolve_model(&parent, &none, "haiku", &no, &conns).unwrap().model, "claude-haiku-4-5");
        assert_eq!(resolve_model(&parent, &picked, "haiku", &no, &conns).unwrap().model, "gpt-6.1-sol");
        // ...and connection/model names any connection.
        assert_eq!(resolve_model(&parent, &none, "router/qwen/qwen3", &no, &conns).unwrap(), spec("c3", "openai_compat", "qwen/qwen3", ""));
        // The call wins: by connection name (any case) and model; the
        // effort carries over on the same connection, not to another.
        assert_eq!(
            resolve_model(&parent, &picked, "haiku", &choice("BLACKTITAN chatgpt", "gpt-6.1-sol", ""), &conns).unwrap(),
            spec("c2", "chatgpt", "gpt-6.1-sol", "medium")
        );
        assert_eq!(
            resolve_model(&parent, &none, "", &choice("blacktitan chatgpt", "gpt-6.1-sol", ""), &conns).unwrap(),
            spec("c2", "chatgpt", "gpt-6.1-sol", "")
        );
        assert_eq!(
            resolve_model(&parent, &none, "", &choice("c2", "", "high"), &conns).unwrap(),
            spec("c2", "chatgpt", "gpt-6.1", "high"),
            "by id, on its default model"
        );
        assert_eq!(
            resolve_model(&parent, &none, "", &choice("", "blacktitan ChatGPT/gpt-6.1-sol", "xhigh"), &conns).unwrap(),
            spec("c2", "chatgpt", "gpt-6.1-sol", "xhigh")
        );
        assert_eq!(
            resolve_model(&parent, &none, "", &choice("", "claude-haiku-4-5", ""), &conns).unwrap(),
            spec("c1", "anthropic", "claude-haiku-4-5", "high"),
            "a model on the same connection"
        );
        // Refusals.
        let err = resolve_model(&parent, &none, "", &choice("nope", "", ""), &conns).unwrap_err();
        assert!(err.contains("no connection \"nope\"") && err.contains("work, blacktitan ChatGPT, router"), "{err}");
        assert!(resolve_model(&parent, &none, "", &choice("router", "", ""), &conns).unwrap_err().contains("no default model"));
        assert!(resolve_model(&parent, &none, "", &choice("", "", "huge"), &conns).unwrap_err().contains("effort"));
        let gone = SubagentDefault { target: Some(spec("c9", "openai", "gpt-5", "")), model: String::new() };
        assert!(resolve_model(&parent, &gone, "", &no, &conns).unwrap_err().contains("not available"));
        // A Claude Code parent: its subagents need a connection of their own.
        let cc = spec("cc", "claude_code", "opus", "");
        assert!(resolve_model(&cc, &none, "", &no, &conns).unwrap_err().contains("not on Claude Code"));
        assert_eq!(
            resolve_model(&cc, &none, "", &choice("work", "", ""), &conns).unwrap(),
            spec("c1", "anthropic", "claude-sonnet-5-5", "")
        );
        assert_eq!(resolve_model(&cc, &picked, "", &no, &conns).unwrap(), spec("c2", "chatgpt", "gpt-6.1-sol", "medium"));
        // A head that lists no connections: the parent's own only.
        let p2 = spec("c3", "openai_compat", "m", "");
        assert_eq!(resolve_model(&p2, &rule, "", &no, &[]).unwrap().model, "claude-haiku-4-5");
        assert!(resolve_model(&p2, &none, "", &choice("work", "", ""), &[]).unwrap_err().contains("available: none"));
    }
}

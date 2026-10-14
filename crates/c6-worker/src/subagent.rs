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
use c6_tools::{AgentDef, Subagents, Task, Tool};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::session::{SessionManager, Writer};

/// How deep subagents may nest: the top-level session is depth 0, so its
/// subagents (depth 1) cannot start their own.
const MAX_DEPTH: u32 = 1;

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

/// What a subagent's journaled spec says about its agent, to rebuild its
/// tools when it is loaded again.
#[derive(serde::Serialize, serde::Deserialize)]
struct AgentJson {
    name: String,
    #[serde(default)]
    tools: Vec<String>,
}

/// The tools a subagent has, of those its session would otherwise get:
/// those its named agent allows, and no Task below the depth limit.
pub(crate) fn delegated_tools(d: &Delegation, tools: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
    let agent = serde_json::from_str::<AgentJson>(&d.agent_json).ok().map(|a| AgentDef {
        name: a.name,
        description: String::new(),
        tools: a.tools,
        model: String::new(),
        prompt: String::new(),
    });
    filter_tools(tools, agent.as_ref(), d.depth)
}

fn filter_tools(tools: Vec<Arc<dyn Tool>>, agent: Option<&AgentDef>, depth: u32) -> Vec<Arc<dyn Tool>> {
    tools
        .into_iter()
        .filter(|t| {
            let name = t.spec().name;
            !(Task::is_task(&name) && depth >= MAX_DEPTH) && agent.is_none_or(|a| a.allows(&name))
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
        let tools = filter_tools(self.tools.clone(), agent, depth);
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
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        child.send(crate::session::ActorMsg::Delegate(Box::new(input), tx)).map_err(|_| "the subagent stopped before it started".to_owned())?;
        // Stopping the parent's turn (or this background task) stops the
        // subagent's turn; its log keeps what it did. The delegation's
        // outcome follows once that turn closes.
        let mut guard = InterruptOnDrop(Some(child.clone()));
        let outcome = tokio::select! {
            o = &mut rx => o,
            _ = cancel.cancelled() => {
                let _ = child.send(crate::session::ActorMsg::Interrupt);
                (&mut rx).await
            }
        };
        guard.0 = None;
        finish(&child_id, outcome.ok())
    }
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

/// The tool result for a delegation's outcome.
fn finish(child_id: &str, outcome: Option<crate::session::Outcome>) -> Result<String, String> {
    let Some(o) = outcome else { return Err(format!("the subagent stopped unexpectedly [subagent session {child_id}]")) };
    match o.reason {
        TurnEndReason::Completed if !o.report.trim().is_empty() => Ok(format!("{}\n\n[subagent session {child_id}]", o.report)),
        reason => {
            let why = if o.error.is_empty() { String::new() } else { format!(": {}", o.error) };
            Err(format!("the subagent ended ({reason:?}) without a report{why}"))
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

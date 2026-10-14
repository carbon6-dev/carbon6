//! Subagents: `Task` runs a child session on this worker with a fresh
//! conversation and the parent's tools minus `Task` itself. The child's log
//! syncs to the head like any session, linked to the parent call.

use std::sync::Arc;

use async_trait::async_trait;
use c6_agent::{Conversation, EventSink, TurnConfig, run_turn};
use c6_llm::{Block, Message, ModelClient, Role};
use c6_proto::{CreateSession, ModelSpec, SessionEvent, SessionStarted, UserMessage, session_event};
use c6_tools::{AgentDef, SessionTools, Subagents, Task, Tool};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::journal::Journal;
use crate::link::HeadLink;
use crate::session::Writer;

/// What a subagent inherits from its parent session.
pub struct Parent {
    pub session_id: String,
    pub spec: CreateSession,
    pub started: SessionStarted,
    pub journal: Arc<Journal>,
    pub link: Arc<HeadLink>,
    /// Its hooks apply to the subagent's tool calls too.
    pub hooks: Option<Arc<dyn c6_tools::Hooks>>,
    /// And its secrets to the subagent's commands.
    pub secrets: c6_tools::Secrets,
    /// What a subagent publishes appears in the parent's Outputs.
    pub publisher: Option<c6_tools::PublishSink>,
    /// And its memory changes are the parent's.
    pub memory: Option<c6_tools::MemorySink>,
    /// And its notices.
    pub notices: Option<c6_tools::NoticeSink>,
    /// And its way to the head (the memex tools, in its scope).
    pub head: Option<Arc<dyn c6_tools::HeadCaller>>,
}

pub struct Runner {
    parent: Parent,
    cfg: TurnConfig,
    client: Arc<dyn ModelClient>,
    tools: Vec<Arc<dyn Tool>>,
}

impl Runner {
    pub fn new(parent: Parent, cfg: &TurnConfig, client: Arc<dyn ModelClient>, tools: &[Arc<dyn Tool>]) -> Self {
        let cfg = cfg.clone();
        let tools = tools.iter().filter(|t| !Task::is_task(&t.spec().name)).cloned().collect();
        Self { parent, cfg, client, tools }
    }
}

#[async_trait]
impl Subagents for Runner {
    async fn run(
        &self,
        parent_call_id: &str,
        description: &str,
        prompt: &str,
        agent: Option<&AgentDef>,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let child_id = uuid::Uuid::now_v7().to_string();
        let p = &self.parent;
        let tools: Vec<Arc<dyn Tool>> = self.tools.iter().filter(|t| agent.is_none_or(|a| a.allows(&t.spec().name))).cloned().collect();
        let mut cfg = TurnConfig { cache_key: child_id.clone(), ..self.cfg.clone() };
        if !p.spec.subagent_model.trim().is_empty() {
            cfg.model = p.spec.subagent_model.trim().to_owned();
        }
        if let Some(a) = agent {
            // The agent's instructions are its system prompt.
            cfg.system_prompt = format!(
                "{}\n\n# Environment\n\nWorking directory: {}\nToday's date: {}",
                a.prompt,
                p.started.cwd,
                time::OffsetDateTime::now_utc().date()
            );
            let kind = p.spec.model.as_ref().map(|m| m.connection_kind.as_str()).unwrap_or_default();
            if let Some(model) = agent_model(&a.model, kind) {
                cfg.model = model;
            }
        }
        let started = SessionStarted {
            title: match agent {
                Some(a) => format!("{description} ({})", a.name),
                None => description.to_owned(),
            },
            parent_session_id: p.session_id.clone(),
            parent_call_id: parent_call_id.to_owned(),
            tools: tools.iter().map(|t| t.spec().name).collect(),
            system_prompt: if agent.is_some() { cfg.system_prompt.clone() } else { p.started.system_prompt.clone() },
            model: p.started.model.clone().map(|m| ModelSpec { model: cfg.model.clone(), ..m }),
            ..p.started.clone()
        };
        let ev = SessionEvent { seq: 1, ts_ms: c6_core::now_ms(), kind: Some(session_event::Kind::Started(started.clone())) };
        // Subagents start fresh: a side chat's seed is not theirs.
        let spec = CreateSession { seed: Vec::new(), ..p.spec.clone() };
        p.journal.create_session(&child_id, &spec, &ev).await.map_err(|e| format!("{e:#}"))?;
        info!(parent = %p.session_id, child = %child_id, %description, "subagent started");

        let writer: Arc<dyn EventSink> = Arc::new(Writer {
            session_id: child_id.clone(),
            journal: p.journal.clone(),
            link: p.link.clone(),
            next_seq: tokio::sync::Mutex::new(2),
        });
        let note = crate::session::spec_prompt(&p.spec, "session.subagent", c6_agent::SUBAGENT_NOTE);
        let text = format!("{prompt}\n\n---\n{note}");
        writer
            .append(session_event::Kind::User(UserMessage { command_id: String::new(), text: text.clone(), ..Default::default() }))
            .await
            .map_err(|e| format!("{e:#}"))?;

        let session_tools = Arc::new(SessionTools::new(std::path::PathBuf::from(&started.cwd)));
        session_tools.set_hooks(p.hooks.clone().map(|h| Arc::new(SubagentHooks(h)) as Arc<dyn c6_tools::Hooks>));
        session_tools.set_secrets(p.secrets.env());
        session_tools.set_publisher(p.publisher.clone());
        session_tools.set_memory(p.memory.clone());
        session_tools.set_notices(p.notices.clone());
        session_tools.set_head(p.head.clone());
        let mut conv = Conversation { history: vec![Message::user_text(text)], context_tokens: 0 };
        let reason = run_turn(1, &cfg, self.client.as_ref(), &tools, &session_tools, &writer, &mut conv, &mut Vec::new, &cancel)
            .await
            .map_err(|e| format!("{e:#}"))?;
        let answer: Vec<String> = conv
            .history
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && m.blocks.iter().any(|b| matches!(b, Block::Text(t) if !t.trim().is_empty())))
            .map(|m| m.blocks.iter().filter_map(|b| if let Block::Text(t) = b { Some(t.clone()) } else { None }).collect())
            .unwrap_or_default();
        if answer.is_empty() {
            return Err(format!("the subagent ended ({reason:?}) without a report"));
        }
        Ok(format!("{}\n\n[subagent session {child_id}]", answer.join("\n")))
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
struct SubagentHooks(Arc<dyn c6_tools::Hooks>);

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

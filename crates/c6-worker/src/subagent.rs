//! Subagents: `Task` runs a child session on this worker with a fresh
//! conversation and the parent's tools minus `Task` itself. The child's log
//! syncs to the head like any session, linked to the parent call.

use std::sync::Arc;

use async_trait::async_trait;
use c6_agent::{Conversation, EventSink, TurnConfig, run_turn};
use c6_llm::{Block, Message, ModelClient, Role};
use c6_proto::{CreateSession, SessionEvent, SessionStarted, UserMessage, session_event};
use c6_tools::{SessionTools, Subagents, Task, Tool};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::journal::Journal;
use crate::link::HeadLink;
use crate::session::Writer;

const SUBAGENT_NOTE: &str = "\n\n---\nYou are a subagent working for another agent, which cannot see this conversation. \
Work autonomously. When you are done, reply with a concise but complete report of what you found or did; that report is all the other agent receives.";

/// What a subagent inherits from its parent session.
pub struct Parent {
    pub session_id: String,
    pub spec: CreateSession,
    pub started: SessionStarted,
    pub journal: Arc<Journal>,
    pub link: Arc<HeadLink>,
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
    async fn run(&self, parent_call_id: &str, description: &str, prompt: &str, cancel: CancellationToken) -> Result<String, String> {
        let child_id = uuid::Uuid::now_v7().to_string();
        let p = &self.parent;
        let started = SessionStarted {
            title: description.to_owned(),
            parent_session_id: p.session_id.clone(),
            parent_call_id: parent_call_id.to_owned(),
            tools: self.tools.iter().map(|t| t.spec().name).collect(),
            ..p.started.clone()
        };
        let ev = SessionEvent { seq: 1, ts_ms: c6_core::now_ms(), kind: Some(session_event::Kind::Started(started.clone())) };
        p.journal.create_session(&child_id, &p.spec, &ev).await.map_err(|e| format!("{e:#}"))?;
        info!(parent = %p.session_id, child = %child_id, %description, "subagent started");

        let writer: Arc<dyn EventSink> = Arc::new(Writer {
            session_id: child_id.clone(),
            journal: p.journal.clone(),
            link: p.link.clone(),
            next_seq: tokio::sync::Mutex::new(2),
        });
        let text = format!("{prompt}{SUBAGENT_NOTE}");
        writer
            .append(session_event::Kind::User(UserMessage { command_id: String::new(), text: text.clone() }))
            .await
            .map_err(|e| format!("{e:#}"))?;

        let cfg = TurnConfig { cache_key: child_id.clone(), ..self.cfg.clone() };
        let session_tools = Arc::new(SessionTools::new(std::path::PathBuf::from(&started.cwd)));
        let mut conv = Conversation { history: vec![Message::user_text(text)], context_tokens: 0 };
        let reason = run_turn(1, &cfg, self.client.as_ref(), &self.tools, &session_tools, &writer, &mut conv, &mut Vec::new, &cancel)
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

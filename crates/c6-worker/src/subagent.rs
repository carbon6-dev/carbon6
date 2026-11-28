//! Subagents: `Task` starts a child session on this worker with a fresh
//! conversation and the parent's tools (filtered by the named agent). The
//! child is a session like any other, with its own actor and log, linked
//! to the parent call (docs/subagents.md). Its log syncs to the head like
//! any session's.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use c6_agent::{EventSink, PendingInput};
use c6_api::SubagentConnection;
use c6_llm::{Provider, ToolProfile, model_info};
use c6_proto::{
    CreateSession, Delegation, ModelSpec, SessionEvent, SessionStarted, SubagentSpawned, TurnEndReason, UserMessage, session_event,
};
use c6_tools::{AgentDef, ModelChoice, SendMessage, Skill, SkillTool, Subagents, Task, TaskGroup, Tool};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::journal::Journal;
use crate::session::{Outcome, SessionManager, Writer};

/// The live realm settings a running delegate consults and passes to a
/// new local child, which may not have existed during the head's refresh.
#[derive(Clone, PartialEq)]
pub(crate) struct LaunchLimits {
    pub depth: u32,
    pub running: u32,
    notices: c6_proto::NoticeLimits,
}

impl LaunchLimits {
    pub(crate) fn of(spec: &CreateSession) -> Self {
        Self {
            depth: spec.max_subagent_depth,
            running: spec.max_running_subagents,
            notices: c6_proto::NoticeLimits {
                notice_quiet_seconds: spec.notice_quiet_seconds,
                notice_max_wait_seconds: spec.notice_max_wait_seconds,
                notice_no_progress_limit: spec.notice_no_progress_limit,
                notice_failure_relaunch_limit: spec.notice_failure_relaunch_limit,
                notice_tokens_per_hour: spec.notice_tokens_per_hour,
                notice_loop_backoff_rounds: spec.notice_loop_backoff_rounds,
                notice_loop_backoff_max_seconds: spec.notice_loop_backoff_max_seconds,
                max_subagent_depth: Some(spec.max_subagent_depth),
                max_running_subagents: Some(spec.max_running_subagents),
                claude_code_trim_enabled: spec.claude_code_trim_enabled,
                claude_code_trim_at: Some(spec.claude_code_trim_at),
                keep_warm_json: spec.keep_warm_json.clone(),
            },
        }
    }

    fn allows(&self, depth: u32) -> bool {
        depth < if self.depth == 0 { c6_api::DEFAULT_SUBAGENT_DEPTH } else { self.depth }
    }

    fn inherit(&self, spec: &mut CreateSession) {
        Self::apply_notice_limits(spec, &self.notices);
    }

    pub(crate) fn apply_notice_limits(spec: &mut CreateSession, limits: &c6_proto::NoticeLimits) {
        spec.notice_quiet_seconds = limits.notice_quiet_seconds;
        spec.notice_max_wait_seconds = limits.notice_max_wait_seconds;
        spec.notice_no_progress_limit = limits.notice_no_progress_limit;
        spec.notice_failure_relaunch_limit = limits.notice_failure_relaunch_limit;
        spec.notice_tokens_per_hour = limits.notice_tokens_per_hour;
        spec.notice_loop_backoff_rounds = limits.notice_loop_backoff_rounds;
        spec.notice_loop_backoff_max_seconds = limits.notice_loop_backoff_max_seconds;
        if let Some(settings) = &limits.keep_warm_json {
            spec.keep_warm_json = Some(settings.clone());
        }
        if let Some(depth) = limits.max_subagent_depth {
            spec.max_subagent_depth = depth;
        }
        if let Some(running) = limits.max_running_subagents {
            spec.max_running_subagents = running;
        }
        if let Some(enabled) = limits.claude_code_trim_enabled {
            spec.claude_code_trim_enabled = Some(enabled);
        }
        if let Some(at) = limits.claude_code_trim_at {
            spec.claude_code_trim_at = at;
        }
    }
}

/// Whether a session at `depth` may start or continue subagents.
pub(crate) fn may_delegate(spec: &CreateSession, depth: u32) -> bool {
    LaunchLimits::of(spec).allows(depth)
}

pub(crate) fn delegation_tool(name: &str) -> bool {
    Task::is_task(name) || SendMessage::is_send(name)
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
    pub(crate) limits: Arc<std::sync::Mutex<LaunchLimits>>,
}

const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The connection, model and effort a subagent runs on. In order, each
/// overriding the one before: its parent's model, the session's default
/// for subagents, a named agent's model, and what the `Task` call asks for.
/// Only `connections` (those the session's realm may use), the head-authorized
/// default target or the parent's own may be used. Credentials still require
/// the head's current realm authorization. One on Claude Code runs as a Claude Code session of its
/// own; any other in carbon6's own loop.
pub fn resolve_model(
    parent: &ModelSpec,
    default: &SubagentDefault,
    agent_model: &str,
    choice: &ModelChoice,
    connections: &[SubagentConnection],
) -> Result<ModelSpec, String> {
    let target = default.target.as_ref().map(|m| SubagentConnection {
        id: m.connection_id.clone(),
        name: m.connection_id.clone(),
        kind: m.connection_kind.clone(),
        default_model: m.model.clone(),
    });
    let names = || {
        if connections.is_empty() { "none".to_owned() } else { connections.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ") }
    };
    let find = |want: &str| -> Option<&SubagentConnection> {
        let want = want.trim();
        connections
            .iter()
            .find(|c| c.id == want)
            .or_else(|| connections.iter().find(|c| c.name.eq_ignore_ascii_case(want)))
            .or_else(|| target.as_ref().filter(|c| c.id == want))
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
        compact_at_percent: if c.id == m.connection_id { m.compact_at_percent } else { 0 },
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
        // The head can change the default during a turn. Its authorized
        // target need not be in the parent's start-time connection catalog.
        None if default.target.as_ref().is_some_and(|t| t.connection_id == m.connection_id) => {}
        // The parent's own (a Claude Code parent's subagents run on Claude Code too).
        None if m.connection_id == parent.connection_id => {}
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

/// A local subagent's durable named-agent policy and tools hidden by depth
/// at launch. Neither a deeper limit nor a reload widens its tool policy.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct AgentJson {
    #[serde(default)]
    name: String,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hidden_delegation_tools: Vec<String>,
}

/// The tools a subagent has, of its parent's: those its named agent
/// allows. Depth limits apply at turn and invocation time, not to the stored inventory.
fn filter_tools(tools: Vec<Arc<dyn Tool>>, parent: Option<(&[Arc<dyn Tool>], bool)>, agent: Option<&AgentDef>) -> Vec<Arc<dyn Tool>> {
    tools
        .into_iter()
        .filter(|t| {
            let name = t.spec().name;
            // What a subagent finds goes back to its parent, not into task cards.
            !c6_tools::knowledge::is_task_card(&name)
                // Nor into messages to other sessions, sent in its top-level
                // session's name (docs/background.md §6).
                && !c6_tools::messages::is_message_tool(&name)
                && agent.is_none_or(|a| a.allows(&name))
                && parent.is_none_or(|(tools, search)| {
                    tools.iter().any(|t| c6_tools::tool_allowed(&[t.spec().name], &name))
                        || (search && c6_tools::tool_allowed(&["web_search".into()], &name))
                })
        })
        .collect()
}

/// The tools a subagent's named agent may use (`None`: any), from its
/// delegation. A Claude Code session narrows its own tools and carbon6's
/// by it, as `filter_tools` does in carbon6's own loop.
pub(crate) fn agent_tools(spec: &CreateSession) -> Option<Vec<String>> {
    let d = spec.delegation.as_ref()?;
    serde_json::from_str::<AgentJson>(&d.agent_json).ok().map(|a| a.tools).filter(|t| !t.is_empty())
}

/// Augment Started.tools only with tools the child was authorized to use at
/// creation. Old journals recorded no hidden tools; keep those conservative.
pub(crate) fn saved_tools(spec: &CreateSession, started: &SessionStarted) -> Vec<String> {
    let mut tools = started.tools.clone();
    if let Some(agent) = spec.delegation.as_ref().and_then(|d| serde_json::from_str::<AgentJson>(&d.agent_json).ok()) {
        tools.extend(agent.hidden_delegation_tools.into_iter().filter(|name| delegation_tool(name)));
    }
    tools
}

/// Both lists' tools (an empty list allows any); `None` when no tool is
/// left.
fn both(a: &[String], b: &[String]) -> Option<Vec<String>> {
    let out: Vec<String> = match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_vec(),
        (_, true) => a.to_vec(),
        _ => a.iter().filter(|t| c6_tools::tool_allowed(b, t)).cloned().collect(),
    };
    (out.is_empty() == (a.is_empty() && b.is_empty())).then_some(out)
}

/// A provider's hosted search is subject to the same lists as other tools.
fn web_search_allowed(spec: &CreateSession, started: &SessionStarted, agent: Option<&AgentDef>) -> bool {
    let parent_agent = spec.delegation.as_ref().and_then(|d| serde_json::from_str::<AgentJson>(&d.agent_json).ok());
    !started.hosted_search_off
        && c6_tools::tool_allowed(&spec.tools, "web_search")
        && parent_agent.is_none_or(|a| c6_tools::tool_allowed(&a.tools, "web_search"))
        && agent.is_none_or(|a| a.allows("web_search"))
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
        group: Option<&TaskGroup>,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let child_id = uuid::Uuid::now_v7().to_string();
        let p = &self.parent;
        self.check_depth()?;
        let depth = depth_of(&p.spec) + 1;
        let root = p.spec.delegation.as_ref().map_or(p.session_id.as_str(), |d| d.root_session_id.as_str()).to_owned();
        let default = p.default.lock().unwrap().clone();
        let parent_model = p.spec.model.clone().unwrap_or_default();
        let connections = crate::session::subagent_connections(&p.spec);
        let model_spec = resolve_model(&parent_model, &default, agent.map_or("", |a| a.model.as_str()), choice, &connections)?;
        if !p.spec.subagent_connections_json.is_empty() && !connections.iter().any(|c| c.id == model_spec.connection_id) {
            return Err("that connection is no longer available to this session's subagents".into());
        }
        // On Claude Code: a Claude Code session of its own, with Claude Code's
        // tools and prompt (carbon6's tools reach it over the bridge).
        let on_claude_code = model_spec.connection_kind == "claude_code";
        let admission = self.wait_for_headroom(&cancel, true).await?;
        let limits = self.check_depth()?;
        let _working = self.permit(&root, limits.running as usize)?;
        // An alias retains its stable ID in the child spec. Its provider
        // family is fixed, but "alias" itself is not a provider kind: ask
        // the head rather than silently building OpenAI tools for Claude.
        // LinkedClient still resolves the account afresh on every model call.
        let kind = if model_spec.connection_kind == "alias" {
            p.manager
                .link
                .session_credential(&root, &model_spec.connection_id)
                .await
                .map_err(|e| format!("subagent connection: {e:#}"))?
                .kind
        } else {
            model_spec.connection_kind.clone()
        };
        let provider = if on_claude_code {
            // No provider loop; the native runtime supplies its own tools.
            Provider::OpenAiChat
        } else {
            Provider::from_kind(&kind).ok_or_else(|| format!("unsupported subagent connection kind: {kind}"))?
        };
        let info = model_info(provider, &model_spec.model);
        let profile = info.tool_profile;
        let context_window = p.manager.link.context_window(&root, &model_spec).await;
        let delegation = Delegation {
            parent_session_id: p.session_id.clone(),
            parent_call_id: parent_call_id.to_owned(),
            root_session_id: root,
            agent_json: String::new(),
            depth,
            observed: false,
        };
        // Subagents start fresh: a side chat's seed is not theirs. Their
        // own subagents get the same default.
        let mut spec = CreateSession {
            seed: Vec::new(),
            model: Some(model_spec.clone()),
            delegation: Some(delegation),
            subagent_target: default.target.clone(),
            subagent_model: default.model.clone(),
            ..p.spec.clone()
        };
        let claude_code_parent = p.started.tool_profile == crate::claude_code::PROFILE;
        let search_allowed = web_search_allowed(&p.spec, &p.started, agent);
        if on_claude_code {
            // Its own agent's list narrows it through its delegation. What
            // narrows the parent goes into its tool list: a named parent's
            // list, and (in carbon6's own loop) the parent's tools, as
            // `filter_tools` keeps a child to them. No hosted search where
            // the parent has none.
            let mut parent: Vec<String> = Vec::new();
            if !claude_code_parent {
                parent = self.tools.iter().map(|t| t.spec().name).collect();
                if search_allowed {
                    parent.push("web_search".into());
                }
            }
            let none = || "its parent may use no tool it could have on Claude Code".to_owned();
            spec.tools = both(&spec.tools, &agent_tools(&p.spec).unwrap_or_default()).ok_or_else(none)?;
            spec.tools = both(&spec.tools, &parent).ok_or_else(none)?;
            if !search_allowed && let Some(c) = spec.claude_code.as_mut() {
                c.native_all = false;
            }
        }
        // The parent's tools where its model takes the same profile; else
        // (another model family, or a Claude Code parent, whose tools
        // carbon6 does not run) tools of its own in its model's profile.
        let (tools, skills) = if on_claude_code {
            (Vec::new(), None)
        } else if p.started.tool_profile == profile.as_str() {
            (filter_tools(self.tools.clone(), None, agent), None)
        } else {
            let built = p.manager.session_tool_set(&child_id, &spec, &p.started, profile).await;
            for e in &built.errors {
                tracing::warn!(child = %child_id, "subagent setup: {e}");
            }
            // A named parent may already have narrowed its tools.
            // Changing profiles must not give its descendants those tools back.
            let parent = (!claude_code_parent).then_some((self.tools.as_slice(), search_allowed));
            (filter_tools(built.tools, parent, agent), Some(built.skills))
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
                    prompt.push_str(&crate::session::context_sections(&spec, &labels, &tools));
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
        if on_claude_code {
            system_prompt = crate::claude_code::SYSTEM_PROMPT.to_owned();
        } else {
            system_prompt = crate::session::prompt_for_tools(&spec, &system_prompt, &tools, profile);
        }
        // The parent's log links to the child before the child's exists.
        p.log
            .append(session_event::Kind::SubagentSpawned(SubagentSpawned {
                call_id: parent_call_id.to_owned(),
                child_session_id: child_id.clone(),
                agent: agent.map(|a| a.name.clone()).unwrap_or_default(),
                description: description.to_owned(),
                background_task_id: background.unwrap_or_default().to_owned(),
                model: Some(model_spec.clone()),
                remote: None,
                group: group.map_or_else(String::new, |g| g.group.clone()),
                wake: group.map_or_else(String::new, |g| g.wake.clone()),
            }))
            .await
            .map_err(|e| format!("{e:#}"))?;

        // Setup may have awaited I/O. Inherit from the running actor,
        // not from the spec that was copied when this turn began.
        p.limits.lock().unwrap().inherit(&mut spec);
        let visible_names = crate::session::visible_tool_names(&spec, &tools);
        let hidden_delegation_tools: Vec<_> = tools.iter().map(|t| t.spec().name).filter(|name| !visible_names.contains(name)).collect();
        if agent.is_some() || !hidden_delegation_tools.is_empty() {
            let metadata = AgentJson {
                name: agent.map(|a| a.name.clone()).unwrap_or_default(),
                tools: agent.map(|a| a.tools.clone()).unwrap_or_default(),
                hidden_delegation_tools,
            };
            spec.delegation.as_mut().unwrap().agent_json = serde_json::to_string(&metadata).unwrap_or_default();
        }
        let started = SessionStarted {
            title: match agent {
                Some(a) => format!("{description} ({})", a.name),
                None => description.to_owned(),
            },
            parent_session_id: p.session_id.clone(),
            parent_call_id: parent_call_id.to_owned(),
            tools: visible_names,
            hosted_search_off: !search_allowed,
            system_prompt,
            model: Some(model_spec.clone()),
            tool_profile: if on_claude_code { crate::claude_code::PROFILE.to_owned() } else { profile.as_str().to_owned() },
            context_window,
            agent: agent.map(|a| a.name.clone()).unwrap_or_default(),
            depth,
            ..p.started.clone()
        };

        let child = p.manager.start_subagent(&child_id, spec, started, tools).await.map_err(|e| format!("{e:#}"))?;
        info!(parent = %p.session_id, child = %child_id, %description, "subagent started");

        let note = crate::session::spec_prompt(&p.spec, "session.subagent", c6_agent::SUBAGENT_NOTE);
        // A named agent's instructions: its system prompt in carbon6's own
        // loop; on Claude Code (whose prompt is its own), ahead of the task.
        let instructions = match agent {
            Some(a) if on_claude_code && !a.prompt.trim().is_empty() => format!("{}\n\n---\n", a.prompt.trim()),
            _ => String::new(),
        };
        let input = PendingInput {
            command_id: delegation_command_id(background),
            text: prompt.to_owned(),
            prompt: format!("{instructions}{prompt}\n\n---\n{note}"),
            delegated_by: parent_call_id.to_owned(),
            ..Default::default()
        };
        self.delegate(&child_id, &child, input, cancel, admission).await
    }

    async fn running(&self, to: &str) -> Result<bool, String> {
        let (child, remote) = self.resolve(to).await?;
        if remote {
            let v = self.head("session.subagent_state", serde_json::json!({"to": child})).await?;
            return Ok(v["running"].as_bool().unwrap_or(false));
        }
        Ok(self.parent.manager.is_running(&child))
    }

    async fn run_template(
        &self,
        parent_call_id: &str,
        description: &str,
        prompt: &str,
        template: &str,
        on: &str,
        background: Option<&str>,
        group: Option<&TaskGroup>,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let p = &self.parent;
        let depth = depth_of(&p.spec);
        self.check_depth()?;
        // Waiting before it starts: its first turn may end at once.
        let rx = p.manager.wait_remote(&p.session_id, parent_call_id);
        let params = serde_json::json!({"call_id": parent_call_id, "template": template, "on": on, "prompt": prompt,
                                        "description": description, "depth": depth});
        let started = match self.head("session.subagent_start", params).await {
            Ok(v) => v,
            Err(e) => {
                p.manager.take_remote_wait(&p.session_id, parent_call_id);
                return Err(e);
            }
        };
        let s = |k: &str| started[k].as_str().unwrap_or_default().to_owned();
        let child_id = s("child_session_id");
        let remote: c6_api::RemoteSubagent = serde_json::from_value(started["remote"].clone()).unwrap_or_default();
        let model = ModelSpec { connection_id: s("connection_id"), model: s("model"), effort: s("effort"), ..Default::default() };
        p.log
            .append(session_event::Kind::SubagentSpawned(SubagentSpawned {
                call_id: parent_call_id.to_owned(),
                child_session_id: child_id.clone(),
                agent: String::new(),
                description: description.to_owned(),
                background_task_id: background.unwrap_or_default().to_owned(),
                model: Some(model),
                remote: Some(c6_proto::RemoteSubagent {
                    template: remote.template,
                    worker_id: remote.worker_id,
                    worker_name: remote.worker_name.clone(),
                    target: remote.target,
                    cwd: remote.cwd,
                }),
                group: group.map_or_else(String::new, |g| g.group.clone()),
                wake: group.map_or_else(String::new, |g| g.wake.clone()),
            }))
            .await
            .map_err(|e| format!("{e:#}"))?;
        info!(parent = %p.session_id, child = %child_id, worker = %remote.worker_name, %template, "remote subagent started");
        self.wait_remote(&child_id, parent_call_id, rx, cancel).await
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
        self.check_depth()?;
        let spawn = self.resolve_spawn(to).await?;
        let child_id = &spawn.child_session_id;
        if spawn.remote.is_some() {
            // The head sends it; an idle subagent's turn reports to this call.
            let rx = p.manager.wait_remote(&p.session_id, call_id);
            let params = serde_json::json!({"to": child_id, "message": message, "call_id": call_id, "background": background});
            let sent = match self.head("session.subagent_message", params).await {
                Ok(v) => v,
                Err(e) => {
                    p.manager.take_remote_wait(&p.session_id, call_id);
                    return Err(e);
                }
            };
            if sent["running"].as_bool() == Some(true) {
                p.manager.take_remote_wait(&p.session_id, call_id);
                return Ok(None);
            }
            // The head has accepted a new run, not a steer (or a refused
            // input). Its RPC enqueues before replying, unlike local Delegate.
            if let Err(e) = self.link_continuation(&spawn, call_id, background).await {
                p.manager.take_remote_wait(&p.session_id, call_id);
                return Err(e);
            }
            return self.wait_remote(child_id, call_id, rx, cancel).await.map(Some);
        }
        let child = match p.manager.actor_for(child_id).await {
            Ok(Some(c)) => c,
            Ok(None) => return Err(format!("subagent {to} is no longer on this worker")),
            Err(e) => return Err(format!("subagent {to} could not be loaded: {e:#}")),
        };
        let input = PendingInput {
            command_id: delegation_command_id(background),
            text: message.to_owned(),
            delegated_by: call_id.to_owned(),
            ..Default::default()
        };
        // Running: the message joins its turn; the report comes as it would.
        let running = p.manager.is_running(child_id);
        if running && background.is_none() {
            child.send(crate::session::ActorMsg::Delegate(Box::new(input), None, None)).map_err(|_| format!("subagent {to} stopped"))?;
            return Ok(None);
        }
        let root = p.spec.delegation.as_ref().map_or(p.session_id.as_str(), |d| d.root_session_id.as_str()).to_owned();
        let admission = self.wait_for_headroom(&cancel, false).await?;
        let limits = self.check_depth()?;
        let _working = self.permit(&root, limits.running as usize)?;
        // A background invocation has its own waiter even if the child
        // became busy since the tool checked. Link that waiter before input.
        self.link_continuation(&spawn, call_id, background).await?;
        self.delegate(child_id, &child, input, cancel, admission).await.map(Some)
    }

    async fn peek(&self, to: &str) -> Result<String, String> {
        let p = &self.parent;
        let (child_id, remote) = self.resolve(to).await?;
        if remote {
            let v = self.head("session.subagent_peek", serde_json::json!({"to": child_id})).await?;
            return Ok(v["text"].as_str().unwrap_or_default().to_owned());
        }
        let journal = p.manager.journal();
        let events = journal.events_after(&child_id, 0, u32::MAX).await.map_err(|e| format!("{e:#}"))?;
        let status = if p.manager.is_running(&child_id) { "running" } else { logged_state(&events) };
        Ok(digest_events(journal, events, &child_id, status, "").await)
    }
}

impl Runner {
    fn check_depth(&self) -> Result<LaunchLimits, String> {
        let limits = self.parent.limits.lock().unwrap().clone();
        if !limits.allows(depth_of(&self.parent.spec)) {
            return Err("this session is as deep as subagents nest; it may not start or continue subagents".into());
        }
        Ok(limits)
    }

    async fn wait_for_headroom(&self, cancel: &CancellationToken, new_work: bool) -> Result<crate::resource_pressure::Admission, String> {
        let p = &self.parent;
        let writer = p.log.as_ref();
        let gate = if p.started.ssh_target.is_empty() {
            p.manager.launch_pressure.scoped(std::path::Path::new(&p.started.cwd))
        } else {
            (*p.manager.launch_pressure).clone()
        };
        if new_work {
            gate.wait(cancel, move |pressure| pressure.announce(writer)).await
        } else {
            gate.wait_continue(cancel, move |pressure| pressure.announce(writer)).await
        }
    }

    /// Counts a delegation against its tree's limit while it runs.
    fn permit(&self, root: &str, max: usize) -> Result<crate::session::Working, String> {
        self.parent.manager.working(root, max).ok_or_else(|| {
            format!("{max} subagents are already working in this session; wait for one to finish (or stop one) before starting another")
        })
    }

    /// The session id of one of this session's subagents, named by its
    /// session id or background task id (it must have been started here),
    /// and whether it is remote.
    async fn resolve(&self, to: &str) -> Result<(String, bool), String> {
        let spawn = self.resolve_spawn(to).await?;
        Ok((spawn.child_session_id, spawn.remote.is_some()))
    }

    /// Resolve only durable identities, retaining the original delegation's
    /// metadata for a continuation (never the child's mutable title).
    async fn resolve_spawn(&self, to: &str) -> Result<SubagentSpawned, String> {
        let p = &self.parent;
        let events = p.manager.journal().events_after(&p.session_id, 0, u32::MAX).await.map_err(|e| format!("{e:#}"))?;
        events
            .iter()
            .find_map(|e| match &e.kind {
                Some(session_event::Kind::SubagentSpawned(sp))
                    if !to.is_empty() && (sp.child_session_id == to || sp.background_task_id == to) =>
                {
                    Some(sp.clone())
                }
                _ => None,
            })
            .ok_or_else(|| format!("no subagent {to:?} was started in this session"))
    }

    /// A new background run of the same child has a new task/call identity.
    /// Keep earlier links intact so either task id still resolves to it.
    async fn link_continuation(&self, spawn: &SubagentSpawned, call_id: &str, background: Option<&str>) -> Result<(), String> {
        let Some(background) = background else { return Ok(()) };
        let mut link = spawn.clone();
        link.call_id = call_id.to_owned();
        link.background_task_id = background.to_owned();
        // SendMessage is a new task, not another member of Task's fan-out.
        link.group.clear();
        link.wake.clear();
        self.parent.log.append(session_event::Kind::SubagentSpawned(link)).await.map_err(|e| format!("{e:#}"))?;
        Ok(())
    }

    /// Asks the head about this session's remote subagents.
    async fn head(&self, method: &str, mut params: serde_json::Value) -> Result<serde_json::Value, String> {
        params["session_id"] = serde_json::json!(self.parent.session_id);
        self.parent.manager.link.call(method, params).await.map_err(|e| format!("{e:#}"))
    }

    /// Waits for the remote subagent turn a call waits on (`rx`): its
    /// report, or (as `Err`) what it did. Stopping the call stops the
    /// subagent's turn through the head; one whose worker is not connected
    /// cannot be stopped, and its report comes as a notice instead.
    async fn wait_remote(
        &self,
        child_id: &str,
        call_id: &str,
        mut rx: tokio::sync::oneshot::Receiver<c6_proto::DelegationEnded>,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let p = &self.parent;
        let mut guard = RemoteInterruptOnDrop(Some((p.manager.link.clone(), p.session_id.clone(), child_id.to_owned())));
        let mut stopped_here = false;
        let ended = tokio::select! {
            r = &mut rx => r,
            _ = cancel.cancelled() => {
                stopped_here = true;
                let sent = self.head("session.subagent_interrupt", serde_json::json!({"to": child_id})).await;
                if !sent.is_ok_and(|v| v["sent"].as_bool() == Some(true)) {
                    guard.0 = None;
                    p.manager.take_remote_wait(&p.session_id, call_id);
                    return Err(partial(&p.spec, &format!(
                        "<subagent id=\"{child_id}\" status=\"running\">\nIts worker is not connected, so it could not be stopped; its report will come as a notice.\n</subagent>"
                    )));
                }
                (&mut rx).await
            }
        };
        guard.0 = None;
        match ended {
            Ok(e) => remote_outcome(&p.spec, &e, stopped_here),
            Err(_) => Err(format!("stopped waiting for subagent {child_id}; its report will come as a notice")),
        }
    }

    /// Gives the subagent `input` and waits for the turn that takes it:
    /// its report, or (as `Err`) a digest of what it did.
    async fn delegate(
        &self,
        child_id: &str,
        child: &tokio::sync::mpsc::UnboundedSender<crate::session::ActorMsg>,
        input: PendingInput,
        cancel: CancellationToken,
        admission: crate::resource_pressure::Admission,
    ) -> Result<String, String> {
        let p = &self.parent;
        let call_id = input.delegated_by.clone();
        // Normally the local child replies directly. Register the same call
        // on the head report path too, so a loss can settle this tool even
        // when the child's actor can no longer answer its mailbox.
        let loss = p.manager.wait_remote(&p.session_id, &call_id);
        let registration = LocalLossRegistration { manager: &p.manager, parent: &p.session_id, call: &call_id };
        // The parent journal knows the exact call before the child can take
        // the input. Replicate this marker on the usual persistence stream;
        // it does not require the new child's Started to reach the head first.
        p.log
            .append(session_event::Kind::SubagentReported(c6_proto::SubagentReported {
                command_id: format!("subagent:local-wait:{}", input.command_id),
                child_session_id: child_id.to_owned(),
                call_id: call_id.clone(),
                status: "waiting".into(),
            }))
            .await
            .map_err(|e| format!("could not record the local subagent wait: {e:#}"))?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        child
            .send(crate::session::ActorMsg::Delegate(Box::new(input), Some(tx), Some(admission)))
            .map_err(|_| "the subagent stopped before it started".to_owned())?;
        // Stopping the parent's turn (or this background task) stops the
        // subagent's turn; its log keeps what it did. The delegation's
        // outcome follows once the foreground and its counted work stop.
        let mut guard = InterruptOnDrop(Some(child.clone()));
        let mut stopped_here = false;
        let outcome = wait_local_outcome(child_id, rx, loss);
        tokio::pin!(outcome);
        let outcome = tokio::select! {
            biased;
            o = &mut outcome => o,
            _ = cancel.cancelled() => {
                stopped_here = true;
                let _ = child.send(crate::session::ActorMsg::InterruptDelegation);
                outcome.await
            }
        };
        // A liveness observation is not a request to stop surviving work.
        guard.0 = None;
        drop(registration);
        let journal = p.manager.journal();
        match outcome {
            Err(loss) => remote_outcome(&p.spec, &loss, stopped_here),
            Ok(Some(o)) if o.reason == TurnEndReason::Completed && !o.report.trim().is_empty() => {
                Ok(format!("{}\n\n[subagent session {child_id}]", o.report))
            }
            Ok(Some(o)) => {
                let status = status_of(o.reason, &o.report);
                // Stopped from elsewhere than this call: by you.
                let by = if o.reason == TurnEndReason::Interrupted && !stopped_here { "user" } else { "" };
                Err(o.with_failure_cause(&p.spec, partial(&p.spec, &digest(journal, child_id, status, by).await)))
            }
            Ok(None) => Err(partial(&p.spec, &digest(journal, child_id, "lost", "").await)),
        }
    }
}

struct LocalLossRegistration<'a> {
    manager: &'a SessionManager,
    parent: &'a str,
    call: &'a str,
}

impl Drop for LocalLossRegistration<'_> {
    fn drop(&mut self) {
        self.manager.take_remote_wait(self.parent, self.call);
    }
}

async fn wait_local_outcome(
    child_id: &str,
    outcome: tokio::sync::oneshot::Receiver<Outcome>,
    loss: tokio::sync::oneshot::Receiver<c6_proto::DelegationEnded>,
) -> Result<Option<Outcome>, c6_proto::DelegationEnded> {
    let loss = async {
        if let Ok(report) = loss.await
            && report.child_session_id == child_id
            && report.reason == TurnEndReason::WorkerRestarted as i32
        {
            return report;
        }
        // A stale/unrelated report or a removed registration cannot replace
        // the result from this particular local child.
        std::future::pending().await
    };
    tokio::select! {
        biased;
        direct = outcome => Ok(direct.ok()),
        report = loss => Err(report),
    }
}

pub(crate) fn counted_background(notify: bool, kind: &str) -> bool {
    notify || kind == "claude_code"
}

fn open_counted_tasks(events: &[SessionEvent]) -> BTreeMap<&str, &c6_proto::BackgroundTask> {
    let mut open = BTreeMap::new();
    for e in events {
        match &e.kind {
            Some(session_event::Kind::Started(_)) => open.clear(),
            Some(session_event::Kind::BackgroundTask(t)) if t.state == "started" && counted_background(t.notify, &t.kind) => {
                open.insert(t.task_id.as_str(), t);
            }
            Some(session_event::Kind::BackgroundTask(t)) if t.state != "started" => {
                open.remove(t.task_id.as_str());
            }
            _ => {}
        }
    }
    open
}

fn logged_state(events: &[SessionEvent]) -> &'static str {
    let latest = events.iter().rev().find_map(|e| match &e.kind {
        Some(session_event::Kind::TurnEnded(t)) => Some(status_of(TurnEndReason::try_from(t.reason).unwrap_or_default(), "x")),
        Some(session_event::Kind::TurnStarted(_)) => Some("running"),
        Some(session_event::Kind::Started(_)) => Some("not started"),
        _ => None,
    });
    match latest {
        Some("running") => "running",
        _ if !open_counted_tasks(events).is_empty() => "waiting",
        state => state.unwrap_or("not started"),
    }
}

/// A delegation reports at quiescence, not merely at the foreground turn
/// boundary. Replay the same boundaries after a restart; don't let a task's
/// final notice turn turn the original wait into a second detached report.
pub(crate) fn local_report_windows(
    events: &[SessionEvent],
    tracking_start: usize,
    received: impl Fn(u64) -> bool,
) -> Vec<(u64, usize, Vec<&UserMessage>)> {
    use session_event::Kind;
    let mut tasks = HashSet::new();
    let mut foreground = false;
    let mut pending = Vec::new();
    let mut inputs = Vec::new();
    let mut turn_inputs = Vec::new();
    let mut end = None;
    for (i, e) in events.iter().enumerate() {
        // A task's start and its foreground turn end come through different
        // callbacks. Allow a start written just after TurnEnded to keep the
        // report open, but close historical work before the next idle input.
        if !foreground
            && tasks.is_empty()
            && matches!(&e.kind, Some(Kind::User(_) | Kind::TurnStarted(_)))
            && let Some(seq) = end.take()
        {
            pending.push((seq, i - 1, std::mem::take(&mut inputs)));
        }
        match &e.kind {
            Some(Kind::Started(_)) => {
                tasks.clear();
                foreground = false;
            }
            Some(Kind::BackgroundTask(t)) if t.state == "started" && counted_background(t.notify, &t.kind) => {
                tasks.insert(t.task_id.as_str());
            }
            Some(Kind::BackgroundTask(t)) if t.state != "started" => {
                tasks.remove(t.task_id.as_str());
            }
            Some(Kind::User(u)) if i >= tracking_start && u.blocked.is_empty() && u.command != "compact" => turn_inputs.push(u),
            Some(Kind::TurnStarted(_)) => foreground = true,
            Some(Kind::TurnEnded(_)) => {
                foreground = false;
                inputs.append(&mut turn_inputs);
                if received(e.seq) {
                    // Also recognize receipts from before background-aware
                    // settlement: those inputs were already reported.
                    inputs.clear();
                    end = None;
                } else if !inputs.is_empty() {
                    end = Some(e.seq);
                }
            }
            _ => {}
        }
    }
    if !foreground
        && tasks.is_empty()
        && let Some(seq) = end
    {
        pending.push((seq, events.len() - 1, inputs));
    }
    pending
}

/// When no follow-up turn ran, append what the durable task records say to
/// the last foreground response (which may merely have announced a launch).
pub(crate) fn local_background_outcome(events: &[SessionEvent], outcome: &mut Outcome) {
    use session_event::Kind;
    let mut tasks = Vec::new();
    for e in events.iter().rev().take_while(|e| !matches!(e.kind, Some(Kind::TurnEnded(_)))) {
        let Some(Kind::BackgroundTask(t)) = &e.kind else { continue };
        if t.state == "started" || !counted_background(t.notify, &t.kind) {
            continue;
        }
        if outcome.reason == TurnEndReason::Completed {
            outcome.reason = match t.state.as_str() {
                "lost" => TurnEndReason::WorkerRestarted,
                "stopped" | "stopped_by_user" => TurnEndReason::Interrupted,
                "failed" => TurnEndReason::Error,
                _ if t.exit_code != 0 => TurnEndReason::Error,
                _ => outcome.reason,
            };
        }
        tasks.push(format!("{} ({}): {}", t.description, t.task_id, t.state));
    }
    if !tasks.is_empty() {
        tasks.reverse();
        if !outcome.report.is_empty() {
            outcome.report.push_str("\n\n");
        }
        outcome.report.push_str(&format!("Background work ended:\n{}", tasks.join("\n")));
    }
}

/// Preserve the exact background waiter on the child's durable User input.
/// A reused tool call id cannot identify a particular run of the same child.
fn delegation_command_id(background: Option<&str>) -> String {
    let id = uuid::Uuid::now_v7();
    match background {
        Some(task) => format!("delegation:{id}:{task}"),
        None => format!("delegation:{id}"),
    }
}

/// How a delegation's turn ended, as the digest names it.
pub fn status_of(reason: TurnEndReason, report: &str) -> &'static str {
    match reason {
        TurnEndReason::Completed if report.trim().is_empty() => "completed without a report",
        TurnEndReason::Completed => "completed",
        TurnEndReason::Interrupted => "interrupted",
        TurnEndReason::WorkerRestarted => "lost",
        _ => "failed",
    }
}

/// Display a report without the model-facing attribution or digest wrappers.
/// Callers use the child's title (or its delegation description), never its id.
pub fn task_report_notice(title: &str, sid: &str, reason: TurnEndReason, report: &str) -> c6_proto::TaskNotice {
    let status = status_of(reason, report);
    let markdown = if reason == TurnEndReason::Completed && !report.trim().is_empty() {
        report.to_owned()
    } else if report.trim().is_empty() {
        format!("Subagent {status}.")
    } else {
        format!("Subagent {status}.\n\n{report}")
    };
    c6_proto::TaskNotice { kind: "subagent".into(), title: title.into(), session_id: sid.into(), status: status.into(), markdown }
}

/// A subagent's depth in its tree: a local subagent's from its delegation,
/// a remote one's from the head (a top-level session is 0).
pub(crate) fn depth_of(spec: &CreateSession) -> u32 {
    spec.delegation.as_ref().map_or(spec.depth, |d| d.depth)
}

/// A report for a notice: the subagent's report, or (`digest` set) what it
/// did and how to continue it.
pub(crate) fn report_body(spec: &CreateSession, sid: &str, reason: TurnEndReason, report: &str, digest: &str) -> String {
    if reason == TurnEndReason::Completed && !report.trim().is_empty() {
        format!("<subagent id=\"{sid}\" status=\"completed\">\nReport:\n{}\n</subagent>", report.trim_end())
    } else {
        partial(spec, digest)
    }
}

/// Attribute an unasked input before its source is lost in `said` (also
/// used by the head for remote subagents). A resume is not a user message;
/// missing provenance (including older input) never implies a human.
pub fn report_input(
    sid: &str,
    command: &str,
    text: &str,
    from_id: &str,
    from_title: &str,
    api_origin: Option<&c6_proto::ApiMessageOrigin>,
    human_origin: bool,
) -> String {
    let sender = if from_title.is_empty() { from_id } else { from_title };
    if from_id == sid {
        format!("Subagent {sid} ({sender}) resumed itself: \"{text}\"")
    } else if !from_id.is_empty() {
        format!("Session \"{sender}\" ({from_id}) messaged subagent {sid}: \"{text}\"")
    } else if let Some(origin) = api_origin {
        let token = if origin.token_name.is_empty() { String::new() } else { format!(" (token {:?})", origin.token_name) };
        format!("Sent to subagent {sid} via the API{token}: \"{text}\"")
    } else if command == "resume" {
        format!("Subagent {sid} resumed: \"{text}\"")
    } else if human_origin {
        format!("The user messaged subagent {sid} directly: \"{text}\"")
    } else {
        format!("Sent to subagent {sid}: \"{text}\"")
    }
}

/// The notice of a subagent's turn that no call waited on, retaining the
/// input's attribution. Older workers' plain `said` text stays neutral.
pub(crate) fn unasked_report(sid: &str, said: &[String], body: &str) -> String {
    let said: Vec<&str> = said.iter().map(String::as_str).filter(|s| !s.trim().is_empty()).collect();
    if said.is_empty() {
        format!("Subagent {sid} worked on its own (no call of yours waited on it):\n{body}")
    } else {
        format!("{}\n{body}", said.join("\n"))
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
    digest_events(journal, events, child_id, status, by).await
}

async fn digest_events(journal: &Journal, mut events: Vec<SessionEvent>, child_id: &str, status: &str, by: &str) -> String {
    // The local outcome renders this cause through an editable prompt, not
    // by repeating the user-facing diagnostic in the digest.
    events
        .retain(|e| !matches!(&e.kind, Some(session_event::Kind::Error(err)) if err.message == crate::session::JOURNAL_FAILURE_DIAGNOSTIC));
    let mut states = std::collections::HashMap::new();
    for id in spawned_children(&events) {
        let ev = journal.events_after(&id, 0, u32::MAX).await.unwrap_or_default();
        states.insert(id, logged_state(&ev).to_owned());
    }
    digest_text(&events, child_id, status, by, &states)
}

/// The subagents a session's log says it started.
pub fn spawned_children(events: &[c6_proto::SessionEvent]) -> Vec<String> {
    let mut children = Vec::new();
    for e in events {
        if let Some(session_event::Kind::SubagentSpawned(sp)) = &e.kind
            && !children.contains(&sp.child_session_id)
        {
            children.push(sp.child_session_id.clone());
        }
    }
    children
}

/// A subagent's digest from its log (`events`), with its own subagents'
/// states (`states`, by session id; "not started" when missing). The head
/// writes the same for remote subagents (docs/remote-subagents.md).
pub fn digest_text(
    events: &[c6_proto::SessionEvent],
    child_id: &str,
    status: &str,
    by: &str,
    states: &std::collections::HashMap<String, String>,
) -> String {
    let mut turns = 0;
    let mut calls = 0;
    let mut last_text = String::new();
    let mut edits: Vec<(String, Vec<String>)> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut children: Vec<(String, String)> = Vec::new();
    for e in events {
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
            Some(session_event::Kind::SubagentSpawned(sp)) if !children.iter().any(|(id, _)| id == &sp.child_session_id) => {
                children.push((sp.child_session_id.clone(), sp.description.clone()));
            }
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
    if matches!(status, "waiting" | "running") {
        let tasks = open_counted_tasks(events);
        if !tasks.is_empty() {
            let work = tasks
                .into_iter()
                .map(|(id, task)| {
                    let description = first_line(&task.description);
                    if description.is_empty() { id.to_owned() } else { format!("{id} ({})", clip(&description, 200)) }
                })
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("Pending background work: {work}\n"));
        }
    }
    if !children.is_empty() {
        let list: Vec<String> = children
            .iter()
            .map(|(id, description)| {
                let state = states.get(id).map(String::as_str).unwrap_or("not started");
                format!("{id} \"{description}\" ({state})")
            })
            .collect();
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

/// A remote subagent's turn as its parent's call returns it: its report,
/// or (as `Err`) what it did and how to continue it.
fn remote_outcome(spec: &CreateSession, e: &c6_proto::DelegationEnded, stopped_here: bool) -> Result<String, String> {
    let reason = TurnEndReason::try_from(e.reason).unwrap_or(TurnEndReason::Unspecified);
    if reason == TurnEndReason::Completed && !e.report.trim().is_empty() {
        return Ok(format!("{}\n\n[subagent session {}]", e.report, e.child_session_id));
    }
    // Stopped from elsewhere than this call: by you.
    let digest = if reason == TurnEndReason::Interrupted && !stopped_here {
        e.digest.replacen("status=\"interrupted\"", "status=\"interrupted\" by=\"user\"", 1)
    } else {
        e.digest.clone()
    };
    Err(partial(spec, &digest))
}

/// Interrupts a remote subagent (through the head) if the call is dropped
/// before it returns.
struct RemoteInterruptOnDrop(Option<(Arc<crate::link::HeadLink>, String, String)>);

impl Drop for RemoteInterruptOnDrop {
    fn drop(&mut self) {
        if let Some((link, session_id, child)) = self.0.take()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            rt.spawn(async move {
                let _ = link.call("session.subagent_interrupt", serde_json::json!({"session_id": session_id, "to": child})).await;
            });
        }
    }
}

/// Interrupts the subagent if the call is dropped before it returns.
struct InterruptOnDrop(Option<tokio::sync::mpsc::UnboundedSender<crate::session::ActorMsg>>);

impl Drop for InterruptOnDrop {
    fn drop(&mut self) {
        if let Some(c) = &self.0 {
            let _ = c.send(crate::session::ActorMsg::InterruptDelegation);
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

    #[test]
    fn keep_warm_live_settings_reach_children_and_legacy_updates_preserve_them() {
        let mut spec = CreateSession { keep_warm_json: Some(r#"{"enabled":true}"#.into()), ..Default::default() };
        LaunchLimits::apply_notice_limits(&mut spec, &c6_proto::NoticeLimits::default());
        assert_eq!(spec.keep_warm_json.as_deref(), Some(r#"{"enabled":true}"#));
        let limits = c6_proto::NoticeLimits { keep_warm_json: Some(r#"{"enabled":false}"#.into()), ..Default::default() };
        let before = LaunchLimits::of(&spec);
        LaunchLimits::apply_notice_limits(&mut spec, &limits);
        assert!(before != LaunchLimits::of(&spec), "a keep-warm-only edit must trigger settings persistence");
        let mut child = CreateSession::default();
        LaunchLimits::of(&spec).inherit(&mut child);
        assert_eq!(child.keep_warm_json, limits.keep_warm_json);
    }

    #[tokio::test]
    async fn head_loss_settles_local_wait_while_the_child_cannot_reply() {
        let (child, outcome) = tokio::sync::oneshot::channel();
        let (head, loss) = tokio::sync::oneshot::channel();
        head.send(c6_proto::DelegationEnded {
            child_session_id: "child".into(),
            reason: TurnEndReason::WorkerRestarted as i32,
            digest: "Observed model timeout".into(),
            ..Default::default()
        })
        .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), wait_local_outcome("child", outcome, loss)).await;
        assert!(matches!(result, Ok(Err(report)) if report.digest == "Observed model timeout"));
        assert!(child.is_closed(), "the local reply path was released without waiting for the actor");
    }

    #[tokio::test]
    async fn another_childs_loss_cannot_settle_a_local_wait() {
        let (child, outcome) = tokio::sync::oneshot::channel();
        let (head, loss) = tokio::sync::oneshot::channel();
        head.send(c6_proto::DelegationEnded {
            child_session_id: "previous-child".into(),
            reason: TurnEndReason::WorkerRestarted as i32,
            ..Default::default()
        })
        .unwrap();
        let waiting = wait_local_outcome("child", outcome, loss);
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        drop(child);
        assert!(matches!(waiting.await, Ok(None)), "wait for the local actor to reply or disappear");
    }

    #[test]
    fn peek_describes_counted_background_work_as_waiting() {
        use session_event::Kind;
        let event = |kind| SessionEvent { kind: Some(kind), ..Default::default() };
        let work = c6_proto::BackgroundTask {
            task_id: "bg1".into(),
            description: "Run the tests".into(),
            state: "started".into(),
            notify: true,
            ..Default::default()
        };
        let mut events = vec![
            event(Kind::TurnStarted(Default::default())),
            event(Kind::BackgroundTask(work.clone())),
            event(Kind::BackgroundTask(c6_proto::BackgroundTask {
                task_id: "bg2".into(),
                description: "Start a server".into(),
                notify: false,
                ..work.clone()
            })),
        ];
        assert_eq!(logged_state(&events), "running");
        events.push(event(Kind::TurnEnded(c6_proto::TurnEnded { turn: 1, reason: TurnEndReason::Completed as i32 })));
        assert_eq!(logged_state(&events), "waiting");
        let text = digest_text(&events, "child", logged_state(&events), "", &Default::default());
        assert!(text.contains("status=\"waiting\""));
        assert!(text.contains("Pending background work: bg1 (Run the tests)"));
        assert!(!text.contains("Start a server"));
        events.push(event(Kind::BackgroundTask(c6_proto::BackgroundTask { state: "finished".into(), ..work })));
        assert_eq!(logged_state(&events), "completed");
    }

    #[test]
    fn local_delegation_waits_for_counted_work_and_the_open_turn() {
        use session_event::Kind;
        let user = |id: &str| Kind::User(UserMessage { command_id: id.into(), ..Default::default() });
        let task = |id: &str, state: &str, notify| {
            Kind::BackgroundTask(c6_proto::BackgroundTask {
                task_id: id.into(),
                kind: "shell".into(),
                state: state.into(),
                notify,
                ..Default::default()
            })
        };
        let end = |turn| Kind::TurnEnded(c6_proto::TurnEnded { turn, reason: TurnEndReason::Completed as i32 });
        let events: Vec<_> = [
            user("original"),
            Kind::TurnStarted(Default::default()),
            task("server", "started", false),
            end(1),
            task("work", "started", true), // Task callback can land after TurnEnded.
            user("continue"),
            Kind::TurnStarted(Default::default()),
            task("work", "finished", true),
            end(2),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, kind)| SessionEvent { seq: i as u64 + 1, kind: Some(kind), ..Default::default() })
        .collect();
        assert!(local_report_windows(&events[..5], 0, |_| false).is_empty(), "counted work still running");
        assert!(local_report_windows(&events[..8], 0, |_| false).is_empty(), "new foreground turn still running");
        let reports = local_report_windows(&events, 0, |_| false);
        assert_eq!(reports.len(), 1, "the uncounted server does not block reporting");
        assert_eq!(reports[0].0, 9);
        assert_eq!(reports[0].2.iter().map(|u| u.command_id.as_str()).collect::<Vec<_>>(), ["original", "continue"]);
        assert!(local_report_windows(&events, 0, |seq| seq == 9).is_empty(), "a durable receipt ends the dependency");
    }

    #[test]
    fn terminal_background_event_can_settle_idle_delegation() {
        use session_event::Kind;
        let events: Vec<_> = [
            Kind::User(UserMessage { command_id: "original".into(), ..Default::default() }),
            Kind::TurnStarted(Default::default()),
            Kind::BackgroundTask(c6_proto::BackgroundTask {
                task_id: "bg1".into(),
                notify: true,
                state: "started".into(),
                ..Default::default()
            }),
            Kind::TurnEnded(c6_proto::TurnEnded { turn: 1, reason: TurnEndReason::Completed as i32 }),
            Kind::BackgroundTask(c6_proto::BackgroundTask {
                task_id: "bg1".into(),
                description: "work".into(),
                notify: true,
                state: "lost".into(),
                ..Default::default()
            }),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, kind)| SessionEvent { seq: i as u64 + 1, kind: Some(kind), ..Default::default() })
        .collect();
        assert!(local_report_windows(&events[..4], 0, |_| false).is_empty());
        let reports = local_report_windows(&events, 0, |_| false);
        assert_eq!((reports[0].0, reports[0].1), (4, 4));
        let mut outcome = Outcome { reason: TurnEndReason::Completed, report: "Started work".into(), error: String::new() };
        local_background_outcome(&events, &mut outcome);
        assert_eq!(outcome.reason, TurnEndReason::WorkerRestarted);
        assert!(outcome.report.contains("work (bg1): lost"));
    }

    #[test]
    fn continuation_links_keep_one_child_in_the_tree_and_digest() {
        let spawn = SubagentSpawned { child_session_id: "child".into(), description: "same name".into(), ..Default::default() };
        let events: Vec<_> = [
            spawn.clone(),
            SubagentSpawned { call_id: "continue".into(), background_task_id: "bg2".into(), ..spawn.clone() },
            SubagentSpawned { child_session_id: "sibling".into(), ..spawn },
        ]
        .into_iter()
        .map(|sp| c6_proto::SessionEvent { kind: Some(session_event::Kind::SubagentSpawned(sp)), ..Default::default() })
        .collect();
        assert_eq!(spawned_children(&events), ["child", "sibling"]);
        let digest = digest_text(&events, "parent", "interrupted", "", &Default::default());
        assert_eq!(digest.matches("child \"same name\"").count(), 1, "{digest}");
        assert_eq!(digest.matches("sibling \"same name\"").count(), 1, "{digest}");
    }

    #[test]
    fn report_notice_uses_the_report_and_plain_outcome_not_the_model_digest() {
        let report = "## Result\n\n**Passed.**\n";
        let notice = task_report_notice("Review changes", "child", TurnEndReason::Completed, report);
        assert_eq!(
            (notice.kind.as_str(), notice.title.as_str(), notice.session_id.as_str(), notice.status.as_str()),
            ("subagent", "Review changes", "child", "completed")
        );
        assert_eq!(notice.markdown, report, "successful Markdown stays verbatim");
        let failed = task_report_notice("", "raw-id", TurnEndReason::Error, "Partial **findings**.");
        assert!(failed.title.is_empty(), "the UI supplies subagent, not the raw id");
        assert_eq!(failed.markdown, "Subagent failed.\n\nPartial **findings**.");
        for (reason, status) in [
            (TurnEndReason::Completed, "completed without a report"),
            (TurnEndReason::WorkerRestarted, "lost"),
            (TurnEndReason::Interrupted, "interrupted"),
        ] {
            let notice = task_report_notice("Review", "child", reason, "");
            assert_eq!(notice.status, status);
            assert_eq!(notice.markdown, format!("Subagent {status}."));
        }
    }

    #[test]
    fn unasked_reports_name_the_input_source() {
        let own = report_input("ops", "session-message", "Continue after restart.", "ops", "Deploy head", None, false);
        let text = unasked_report("ops", &[own], "Deployed.");
        assert!(text.starts_with("Subagent ops (Deploy head) resumed itself:"), "{text}");
        assert!(!text.contains("The user"));
        let resume = unasked_report("ops", &[report_input("ops", "resume", "Continue.", "", "", None, true)], "Done.");
        assert!(resume.starts_with("Subagent ops resumed:"), "{resume}");
        assert!(!resume.contains("The user"));
        let other = report_input("ops", "session-message", "Deploy.", "coordinator", "Coordinator", None, false);
        assert!(other.starts_with("Session \"Coordinator\" (coordinator) messaged subagent ops:"));
        let user = report_input("ops", "", "Status?", "", "", None, true);
        assert!(user.starts_with("The user messaged subagent ops directly:"));
        for command in ["", "review", "task-notification"] {
            let unknown = report_input("ops", command, "Status?", "", "", None, false);
            assert_eq!(unknown, "Sent to subagent ops: \"Status?\"");
        }
        assert!(!unasked_report("ops", &["an older worker's input".into()], "Done.").contains("The user"));
    }

    #[test]
    fn api_inputs_name_only_the_token_display_name() {
        for command in ["", "resume"] {
            let origin = c6_proto::ApiMessageOrigin { token_name: "ops-controller".into() };
            // API provenance wins even over an inconsistent human flag.
            let text = report_input("ops", command, "Continue.", "", "", Some(&origin), true);
            assert_eq!(text, "Sent to subagent ops via the API (token \"ops-controller\"): \"Continue.\"");
            let unnamed = report_input("ops", command, "Continue.", "", "", Some(&Default::default()), false);
            assert_eq!(unnamed, "Sent to subagent ops via the API: \"Continue.\"");
        }
        let origin = c6_proto::ApiMessageOrigin { token_name: "name\n\"quoted\"".into() };
        let text = report_input("ops", "", "Continue.", "", "", Some(&origin), false);
        assert!(text.contains(r#"name\n\"quoted\""#), "{text}");
    }

    #[test]
    fn older_wire_inputs_and_events_have_unknown_origin() {
        use prost::Message;
        // Only text, as an older head/worker encoded it: no origin field.
        let input = c6_proto::UserInput::decode(&b"\x0a\x07Status?"[..]).unwrap();
        let event = c6_proto::UserMessage::decode(&b"\x12\x07Status?"[..]).unwrap();
        assert!(!input.human_origin && input.api_origin.is_none());
        assert!(!event.human_origin && event.api_origin.is_none());
        let report = report_input(
            "ops",
            &event.command,
            &event.text,
            &event.from_session_id,
            &event.from_title,
            event.api_origin.as_ref(),
            event.human_origin,
        );
        assert_eq!(report, "Sent to subagent ops: \"Status?\"");
    }

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
            compact_at_percent: 0,
        }
    }

    fn choice(connection: &str, model: &str, effort: &str) -> ModelChoice {
        ModelChoice { connection: connection.into(), model: model.into(), effort: effort.into() }
    }

    /// Tool lists combine as both allow, and a combination that allows
    /// nothing says so (an empty list would allow everything).
    #[test]
    fn tool_lists_combine_as_both_allow() {
        let v = |t: &[&str]| t.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(both(&v(&[]), &v(&[])), Some(v(&[])));
        assert_eq!(both(&v(&["Read"]), &v(&[])), Some(v(&["Read"])));
        assert_eq!(both(&v(&[]), &v(&["shell"])), Some(v(&["shell"])));
        assert_eq!(both(&v(&["Read", "Bash"]), &v(&["shell", "read_file"])), Some(v(&["Read", "Bash"])));
        assert_eq!(both(&v(&["Read"]), &v(&["shell"])), None);
    }

    #[tokio::test]
    async fn running_delegate_reads_live_limits_and_reload_keeps_saved_policy() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(crate::journal::Journal::open(&dir.path().join("journal.db")).await.unwrap());
        let link = Arc::new(crate::link::HeadLink::default());
        let manager = SessionManager::new(journal.clone(), link.clone(), dir.path().join("worktrees"));
        let mut spec = CreateSession {
            max_subagent_depth: 1,
            max_running_subagents: 1,
            delegation: Some(Delegation {
                depth: 1,
                agent_json: r#"{"name":"finder","tools":["Read","Task"],"hidden_delegation_tools":["Task"]}"#.into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let started = SessionStarted { tools: vec!["Read".into()], ..Default::default() };
        assert_eq!(saved_tools(&spec, &started), ["Read", "Task"]);
        assert_eq!(agent_tools(&spec).unwrap(), ["Read", "Task"]);
        let limits = Arc::new(std::sync::Mutex::new(LaunchLimits::of(&spec)));
        let runner = Runner::new(
            Parent {
                session_id: "child".into(),
                spec: spec.clone(),
                started: started.clone(),
                log: Arc::new(Writer::new("child".into(), journal, link, 2)),
                manager,
                default: SharedDefault::default(),
                limits: limits.clone(),
            },
            &[],
        );
        assert!(runner.check_depth().is_err());
        let mut edited = spec.clone();
        let update = c6_proto::NoticeLimits {
            notice_quiet_seconds: 7,
            notice_max_wait_seconds: 42,
            notice_no_progress_limit: 13,
            notice_failure_relaunch_limit: 9,
            notice_tokens_per_hour: 10_000_000_000,
            notice_loop_backoff_rounds: 2,
            notice_loop_backoff_max_seconds: 120,
            max_subagent_depth: Some(2),
            max_running_subagents: Some(0),
            claude_code_trim_enabled: Some(false),
            claude_code_trim_at: Some(123_456),
            keep_warm_json: Some(r#"{"enabled":false}"#.into()),
        };
        LaunchLimits::apply_notice_limits(&mut edited, &update);
        *limits.lock().unwrap() = LaunchLimits::of(&edited);
        assert_eq!(runner.check_depth().unwrap().running, 0);
        assert_eq!(runner.parent.spec.max_running_subagents, 1, "the turn still carries the old spawn spec");
        let mut inherited = runner.parent.spec.clone();
        limits.lock().unwrap().inherit(&mut inherited);
        assert_eq!(LaunchLimits::of(&inherited).notices, update);
        assert_eq!(inherited.claude_code_trim_enabled, Some(false));
        assert_eq!(inherited.claude_code_trim_at, 123_456);
        *limits.lock().unwrap() = LaunchLimits::of(&spec);
        assert!(runner.check_depth().is_err());
        spec.delegation.as_mut().unwrap().agent_json = r#"{"name":"finder","tools":["Read","Task"]}"#.into();
        assert_eq!(saved_tools(&spec, &started), ["Read"], "old journals do not gain tools whose launch authorization is unknown");
    }

    #[test]
    fn older_notice_limits_preserve_trim_settings_and_explicit_zero_resets_threshold() {
        let mut spec = CreateSession { claude_code_trim_enabled: Some(false), claude_code_trim_at: 123_456, ..Default::default() };
        LaunchLimits::apply_notice_limits(&mut spec, &c6_proto::NoticeLimits::default());
        assert_eq!(spec.claude_code_trim_enabled, Some(false));
        assert_eq!(spec.claude_code_trim_at, 123_456);
        LaunchLimits::apply_notice_limits(
            &mut spec,
            &c6_proto::NoticeLimits { claude_code_trim_enabled: Some(true), claude_code_trim_at: Some(0), ..Default::default() },
        );
        assert_eq!(spec.claude_code_trim_enabled, Some(true));
        assert_eq!(spec.claude_code_trim_at, 0);
    }

    #[test]
    fn hosted_search_respects_parent_and_named_agent_tools() {
        let mut parent = CreateSession::default();
        let mut started = SessionStarted { tools: vec!["Read".into(), "Task".into()], ..Default::default() };
        let mut agent = AgentDef {
            name: "finder".into(),
            description: String::new(),
            model: String::new(),
            prompt: String::new(),
            tools: vec!["Read".into(), "Task".into()],
        };
        for kind in ["openai_compat", "anthropic", "openai", "claude_code"] {
            parent.model = Some(spec("c1", kind, "m", ""));
            assert!(web_search_allowed(&parent, &started, None), "permission does not depend on the parent's provider or search tools");
            assert!(!web_search_allowed(&parent, &started, Some(&agent)));
        }
        for name in ["web_search", "WebSearch"] {
            agent.tools = vec![name.into()];
            assert!(web_search_allowed(&parent, &started, Some(&agent)));
        }
        parent.tools = vec!["Read".into()];
        assert!(!web_search_allowed(&parent, &started, Some(&agent)));
        parent.tools.clear();
        parent.delegation = Some(Delegation {
            agent_json: serde_json::to_string(&AgentJson {
                name: "finder".into(),
                tools: vec!["Read".into(), "Task".into()],
                ..Default::default()
            })
            .unwrap(),
            ..Default::default()
        });
        assert!(!web_search_allowed(&parent, &started, None), "an older named parent's list still applies");
        parent.delegation = None;
        started.hosted_search_off = true;
        assert!(!web_search_allowed(&parent, &started, None), "an unnamed child inherits the denial");
    }

    #[tokio::test]
    async fn parent_hosted_search_allows_a_childs_external_search_tool() {
        let dir = tempfile::tempdir().unwrap();
        let parent = CreateSession {
            model: Some(spec("native", "anthropic", "claude-sonnet-5-5", "")),
            web_search_json: r#"{"provider":"brave","always":false}"#.into(),
            ..Default::default()
        };
        let built = crate::session::build_tools(&parent, dir.path(), ToolProfile::Claude, &[], &[]).await;
        assert!(!crate::session::has_tool(&built.tools, "web_search"));
        let child = CreateSession { model: Some(spec("chat", "openai_compat", "m", "")), ..parent.clone() };
        let child_tools = crate::session::build_tools(&child, dir.path(), ToolProfile::Generic, &[], &[]).await.tools;
        assert!(crate::session::has_tool(&child_tools, "web_search"));
        let started = SessionStarted::default();
        let allowed = web_search_allowed(&parent, &started, None);
        let filtered = filter_tools(child_tools.clone(), Some((&built.tools, allowed)), None);
        assert!(crate::session::has_tool(&filtered, "web_search"));
        let filtered = filter_tools(child_tools.clone(), Some((&built.tools, false)), None);
        assert!(!crate::session::has_tool(&filtered, "web_search"));
        let filtered = filter_tools(child_tools.clone(), Some((&child_tools, false)), None);
        assert!(crate::session::has_tool(&filtered, "web_search"), "a parent's function search also passes the cap");
    }

    #[test]
    fn a_live_default_missing_from_the_old_catalog_is_authorized() {
        let parent = spec("cc", "claude_code", "opus", "");
        let target = spec("subscription", "alias", "gpt-6.2-sol", "medium");
        let default = SubagentDefault { target: Some(target.clone()), ..Default::default() };
        assert_eq!(resolve_model(&parent, &default, "", &ModelChoice::default(), &[]).unwrap(), target);
        assert_eq!(resolve_model(&parent, &default, "", &choice("subscription", "", ""), &[]).unwrap(), target);
        assert_eq!(
            resolve_model(&parent, &default, "", &choice("", "gpt-6.1-sol", "high"), &[]).unwrap(),
            spec("subscription", "alias", "gpt-6.1-sol", "high")
        );
        assert!(resolve_model(&parent, &default, "", &choice("unavailable", "m", ""), &[]).is_err());
    }

    #[test]
    fn resolves_where_a_subagent_runs() {
        let conns = vec![
            conn("c1", "work", "anthropic", "claude-sonnet-5-5"),
            conn("c2", "work ChatGPT", "chatgpt", "gpt-6.1"),
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
            resolve_model(&parent, &picked, "haiku", &choice("WORK chatgpt", "gpt-6.1-sol", ""), &conns).unwrap(),
            spec("c2", "chatgpt", "gpt-6.1-sol", "medium")
        );
        assert_eq!(
            resolve_model(&parent, &none, "", &choice("work chatgpt", "gpt-6.1-sol", ""), &conns).unwrap(),
            spec("c2", "chatgpt", "gpt-6.1-sol", "")
        );
        assert_eq!(
            resolve_model(&parent, &none, "", &choice("c2", "", "high"), &conns).unwrap(),
            spec("c2", "chatgpt", "gpt-6.1", "high"),
            "by id, on its default model"
        );
        assert_eq!(
            resolve_model(&parent, &none, "", &choice("", "work ChatGPT/gpt-6.1-sol", "xhigh"), &conns).unwrap(),
            spec("c2", "chatgpt", "gpt-6.1-sol", "xhigh")
        );
        assert_eq!(
            resolve_model(&parent, &none, "", &choice("", "claude-haiku-4-5", ""), &conns).unwrap(),
            spec("c1", "anthropic", "claude-haiku-4-5", "high"),
            "a model on the same connection"
        );
        // Refusals.
        let err = resolve_model(&parent, &none, "", &choice("nope", "", ""), &conns).unwrap_err();
        assert!(err.contains("no connection \"nope\"") && err.contains("work, work ChatGPT, router"), "{err}");
        assert!(resolve_model(&parent, &none, "", &choice("router", "", ""), &conns).unwrap_err().contains("no default model"));
        assert!(resolve_model(&parent, &none, "", &choice("", "", "huge"), &conns).unwrap_err().contains("effort"));
        // A head-authorized default can be newer than this catalog, but it
        // does not authorize an arbitrary connection supplied by the call.
        let unlisted = SubagentDefault { target: Some(spec("c9", "openai", "gpt-5", "")), model: String::new() };
        assert_eq!(resolve_model(&parent, &unlisted, "", &no, &conns).unwrap(), spec("c9", "openai", "gpt-5", ""));
        assert!(resolve_model(&parent, &unlisted, "", &choice("c10", "m", ""), &conns).unwrap_err().contains("no connection"));
        // A Claude Code parent: its subagents run on Claude Code too, unless
        // another connection is named.
        let cc = spec("cc", "claude_code", "opus", "");
        assert_eq!(resolve_model(&cc, &none, "", &no, &conns).unwrap(), cc);
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

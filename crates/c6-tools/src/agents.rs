//! Subagents (Task), named agents and skills (Skill).

use std::sync::Arc;

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

/// Runs a subagent: a child session on the same worker with a fresh
/// conversation. Provided by the runtime for each turn.
#[async_trait]
pub trait Subagents: Send + Sync {
    /// Returns the subagent's final answer. `agent`: a named agent's
    /// instructions, tools and model; `None` for a general-purpose one.
    /// `background`: the background task it runs as, if it does.
    async fn run(
        &self,
        parent_call_id: &str,
        description: &str,
        prompt: &str,
        agent: Option<&AgentDef>,
        background: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<String, String>;

    /// Whether `to` (one of this session's subagents, by session id or
    /// background task id) is running now. `Err`: it is not one of them.
    async fn running(&self, to: &str) -> Result<bool, String>;

    /// Sends `message` to the subagent `to`. If it is running, the message
    /// joins its turn and `Ok(None)` returns at once. Otherwise it starts a
    /// turn, and the turn's report comes back as from `run` (`background`:
    /// the background task it runs as, if it does).
    async fn send(
        &self,
        call_id: &str,
        to: &str,
        message: &str,
        background: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<Option<String>, String>;

    /// What the subagent `to` has done so far.
    async fn peek(&self, to: &str) -> Result<String, String>;
}

/// A named subagent (from the head's settings): its instructions, the tools
/// it may use and the model it asks for.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// Tool names it may use (Claude Code's names work in every profile);
    /// empty for all of them.
    pub tools: Vec<String>,
    /// `inherit` (or empty), an alias (`opus`, `sonnet`, `haiku`) or a model id.
    pub model: String,
    pub prompt: String,
}

impl AgentDef {
    /// Whether it may use the tool called `name` (in any profile's naming).
    pub fn allows(&self, name: &str) -> bool {
        fn canonical(n: &str) -> String {
            let n = n.to_ascii_lowercase().replace(['_', '-'], "");
            match n.as_str() {
                "readfile" => "read".into(),
                "shell" => "bash".into(),
                "applypatch" | "multiedit" => "edit".into(),
                "updateplan" => "todowrite".into(),
                "loadskill" => "skill".into(),
                _ => n,
            }
        }
        self.tools.is_empty() || self.tools.iter().any(|t| canonical(t) == canonical(name))
    }
}

pub struct Task {
    claude: bool,
    agents: Arc<Vec<AgentDef>>,
}

impl Task {
    pub fn generic() -> Self {
        Self { claude: false, agents: Arc::default() }
    }

    pub fn claude() -> Self {
        Self { claude: true, agents: Arc::default() }
    }

    /// The same tool, offering these named agents too.
    pub fn with_agents(claude: bool, agents: Vec<AgentDef>) -> Self {
        Self { claude, agents: Arc::new(agents) }
    }

    pub fn is_task(name: &str) -> bool {
        name == "Task" || name == "task"
    }
}

#[async_trait]
impl Tool for Task {
    fn spec(&self) -> ToolSpec {
        let mut description = "Launches a subagent with its own context to handle a self-contained task: broad searches, \
                          investigations, or independent pieces of work. It has the same tools (except launching \
                          further subagents) and returns a single final report. Give it a complete, standalone prompt; \
                          it cannot see this conversation. Launch several in one message to run them in parallel."
            .to_owned();
        let mut properties = json!({
            "description": {"type": "string", "description": "A short (3-5 word) description of the task"},
            "prompt": {"type": "string", "description": "The complete task for the subagent"},
            "run_in_background": {"type": "boolean", "description": "Start it in the background and continue; its report arrives as a notice when it finishes (stop it with the kill tool)"}
        });
        if !self.agents.is_empty() {
            description.push_str("\n\nSpecialized agents (set subagent_type; omit it, or use general-purpose, for a general one):\n");
            for a in self.agents.iter() {
                description.push_str(&format!("- {}: {}\n", a.name, a.description));
            }
            let mut names = vec!["general-purpose".to_owned()];
            names.extend(self.agents.iter().map(|a| a.name.clone()));
            properties["subagent_type"] = json!({"type": "string", "enum": names, "description": "Which agent to launch"});
        }
        ToolSpec {
            name: if self.claude { "Task" } else { "task" }.into(),
            description,
            parameters: json!({"type": "object", "properties": properties, "required": ["description", "prompt"], "additionalProperties": true}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let Some(agents) = ctx.session.subagents() else { return ToolOutput::err("subagents are not available here") };
        let description = input["description"].as_str().unwrap_or("subtask");
        let Some(prompt) = input["prompt"].as_str().filter(|p| !p.trim().is_empty()) else {
            return ToolOutput::err("prompt is required");
        };
        let agent = match input["subagent_type"].as_str().filter(|t| !t.is_empty() && *t != "general-purpose") {
            None => None,
            Some(t) => match self.agents.iter().find(|a| a.name == t) {
                Some(a) => Some(a),
                None => {
                    let names: Vec<&str> = self.agents.iter().map(|a| a.name.as_str()).collect();
                    return ToolOutput::err(format!("no agent named {t:?}; available: general-purpose, {}", names.join(", ")));
                }
            },
        };
        if input["run_in_background"].as_bool() == Some(true) {
            let (id, cancel) = ctx.session.background.start_agent(description, true);
            let (session, call_id, description, prompt, agent, task) =
                (ctx.session.clone(), ctx.call_id.clone(), description.to_owned(), prompt.to_owned(), agent.cloned(), id.clone());
            tokio::spawn(async move {
                let result = agents.run(&call_id, &description, &prompt, agent.as_ref(), Some(&task), cancel).await;
                session.background.finish_agent(&task, result);
            });
            return ToolOutput::ok(format!(
                "Started the subagent in the background as {id}. Its report will arrive as a notice when it finishes; continue with other work meanwhile."
            ));
        }
        match agents.run(&ctx.call_id, description, prompt, agent, None, ctx.cancel.clone()).await {
            Ok(answer) => ToolOutput::limited(answer, false),
            // What it did and how it ended.
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// Continues one of this session's subagents: a follow-up for one that is
/// idle (with its whole conversation), or a message that joins the turn
/// of one that is running.
pub struct SendMessage {
    claude: bool,
}

impl SendMessage {
    pub fn generic() -> Self {
        Self { claude: false }
    }

    pub fn claude() -> Self {
        Self { claude: true }
    }

    pub fn is_send(name: &str) -> bool {
        name == "SendMessage" || name == "send_message"
    }
}

#[async_trait]
impl Tool for SendMessage {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "SendMessage" } else { "send_message" }.into(),
            description: "Sends a message to a subagent you started, which keeps its whole conversation: to give one that \
                          finished a follow-up task, to continue one that was stopped or lost, or to steer one running in the \
                          background. An idle subagent starts working on it and its report comes back like Task's (or as a \
                          notice, with run_in_background); one that is running sees it at its next step and its report comes \
                          when it finishes."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "to": {"type": "string", "description": "The subagent's id (from Task's result or notice), or its background task id"},
                "message": {"type": "string", "description": "What to tell it"},
                "run_in_background": {"type": "boolean", "description": "Continue it in the background; its report arrives as a notice"}
            }, "required": ["to", "message"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let Some(agents) = ctx.session.subagents() else { return ToolOutput::err("subagents are not available here") };
        let to = input["to"].as_str().unwrap_or_default().trim();
        let Some(message) = input["message"].as_str().filter(|m| !m.trim().is_empty()) else {
            return ToolOutput::err("message is required");
        };
        let running = match agents.running(to).await {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(e),
        };
        if running {
            return match agents.send(&ctx.call_id, to, message, None, ctx.cancel.clone()).await {
                Ok(_) => ToolOutput::ok(format!(
                    "Sent: subagent {to} is running and will see your message at its next step. Its report comes when it finishes."
                )),
                Err(e) => ToolOutput::err(e),
            };
        }
        if input["run_in_background"].as_bool() == Some(true) {
            let (id, cancel) = ctx.session.background.start_agent(&format!("continue {to}"), true);
            let (session, call_id, to, message, task) = (ctx.session.clone(), ctx.call_id.clone(), to.to_owned(), message.to_owned(), id.clone());
            tokio::spawn(async move {
                let result = agents.send(&call_id, &to, &message, Some(&task), cancel).await.map(Option::unwrap_or_default);
                session.background.finish_agent(&task, result);
            });
            return ToolOutput::ok(format!(
                "Continuing the subagent in the background as {id}. Its report will arrive as a notice when it finishes."
            ));
        }
        match agents.send(&ctx.call_id, to, message, None, ctx.cancel.clone()).await {
            Ok(answer) => ToolOutput::limited(answer.unwrap_or_default(), false),
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// A skill: instructions loaded on demand.
#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    /// Where its supporting files live, if it came from disk.
    pub dir: Option<String>,
}

impl Skill {
    /// The index placed in the system prompt.
    pub fn index(skills: &[Skill], tool_name: &str) -> String {
        let mut s = format!(
            "# Skills\n\nThese skills hold instructions for specific kinds of work. When one applies, load it with the {tool_name} tool before starting.\n"
        );
        for k in skills {
            s.push_str(&format!("- {}: {}\n", k.name, k.description));
        }
        s
    }
}

pub struct SkillTool {
    claude: bool,
    skills: Arc<Vec<Skill>>,
}

impl SkillTool {
    pub fn new(claude: bool, skills: Arc<Vec<Skill>>) -> Self {
        Self { claude, skills }
    }

    pub fn tool_name(claude: bool) -> &'static str {
        if claude { "Skill" } else { "load_skill" }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn spec(&self) -> ToolSpec {
        let key = if self.claude { "skill" } else { "name" };
        ToolSpec {
            name: Self::tool_name(self.claude).into(),
            description: "Loads a skill's full instructions by name (see the Skills section of the system prompt).".into(),
            parameters: json!({"type": "object", "properties": {key: {"type": "string", "description": "The skill name"}},
                "required": [key], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, _ctx: &ToolCtx, input: Value) -> ToolOutput {
        let name = input["skill"].as_str().or(input["name"].as_str()).unwrap_or_default();
        match self.skills.iter().find(|s| s.name == name) {
            Some(s) => {
                let mut out = format!("# Skill: {}\n\n{}", s.name, s.body);
                if let Some(d) = &s.dir {
                    out.push_str(&format!("\n\n(This skill's files are in {d}.)"));
                }
                ToolOutput::limited(out, false)
            }
            None => ToolOutput::err(format!("no skill named {name:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_index() {
        let skills = vec![Skill { name: "release".into(), description: "Cut a release".into(), body: "1. Bump.".into(), dir: None }];
        assert!(Skill::index(&skills, "Skill").contains("- release: Cut a release"));
    }

    #[test]
    fn named_agents() {
        let r = AgentDef {
            name: "code-reviewer".into(),
            description: "Reviews code".into(),
            tools: vec!["Read".into(), "Grep".into(), "Glob".into()],
            model: "sonnet".into(),
            prompt: "You review code.".into(),
        };
        assert!(r.allows("Read") && r.allows("read") && r.allows("read_file") && r.allows("grep"));
        assert!(!r.allows("Bash") && !r.allows("shell") && !r.allows("Write"));
        let spec = Task::with_agents(true, vec![r]).spec();
        assert!(spec.description.contains("- code-reviewer: Reviews code"));
        assert_eq!(spec.parameters["properties"]["subagent_type"]["enum"], json!(["general-purpose", "code-reviewer"]));
    }
}

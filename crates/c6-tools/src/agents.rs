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
    async fn run(
        &self,
        parent_call_id: &str,
        description: &str,
        prompt: &str,
        agent: Option<&AgentDef>,
        cancel: CancellationToken,
    ) -> Result<String, String>;
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
            "prompt": {"type": "string", "description": "The complete task for the subagent"}
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
        match agents.run(&ctx.call_id, description, prompt, agent, ctx.cancel.clone()).await {
            Ok(answer) => ToolOutput::limited(answer, false),
            Err(e) => ToolOutput::err(format!("subagent failed: {e}")),
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

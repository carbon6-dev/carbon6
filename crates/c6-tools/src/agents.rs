//! Subagents (Task) and skills (Skill).

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
    /// Returns the subagent's final answer.
    async fn run(&self, parent_call_id: &str, description: &str, prompt: &str, cancel: CancellationToken) -> Result<String, String>;
}

pub struct Task {
    claude: bool,
}

impl Task {
    pub fn generic() -> Self {
        Self { claude: false }
    }

    pub fn claude() -> Self {
        Self { claude: true }
    }

    pub fn is_task(name: &str) -> bool {
        name == "Task" || name == "task"
    }
}

#[async_trait]
impl Tool for Task {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "Task" } else { "task" }.into(),
            description: "Launches a subagent with its own context to handle a self-contained task: broad searches, \
                          investigations, or independent pieces of work. It has the same tools (except launching \
                          further subagents) and returns a single final report. Give it a complete, standalone prompt; \
                          it cannot see this conversation. Launch several in one message to run them in parallel."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "description": {"type": "string", "description": "A short (3-5 word) description of the task"},
                "prompt": {"type": "string", "description": "The complete task for the subagent"}
            }, "required": ["description", "prompt"], "additionalProperties": true}),
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
        match agents.run(&ctx.call_id, description, prompt, ctx.cancel.clone()).await {
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
    /// Parses `SKILL.md` with `name:` / `description:` frontmatter.
    pub fn parse(text: &str, fallback_name: &str, dir: Option<String>) -> Option<Self> {
        let rest = text.strip_prefix("---")?;
        let (front, body) = rest.split_once("\n---")?;
        let field = |k: &str| {
            front.lines().find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix(':')).map(|v| v.trim().trim_matches('"').to_owned()))
        };
        let name = field("name").filter(|n| !n.is_empty()).unwrap_or_else(|| fallback_name.to_owned());
        let description = field("description").unwrap_or_default();
        Some(Self { name, description, body: body.trim_start_matches(['-', '\n', '\r']).trim().to_owned(), dir })
    }

    /// Skills from `.claude/skills/*/SKILL.md` and `.agents/skills/*/SKILL.md`
    /// between the repo root and `cwd`.
    pub fn discover(cwd: &std::path::Path) -> Vec<Skill> {
        let root = cwd.ancestors().find(|d| d.join(".git").exists()).unwrap_or(cwd);
        let mut out = Vec::new();
        for dir in cwd.ancestors().take_while(|d| d.starts_with(root)) {
            for sub in [".claude/skills", ".agents/skills"] {
                let Ok(rd) = std::fs::read_dir(dir.join(sub)) else { continue };
                for e in rd.flatten() {
                    let path = e.path().join("SKILL.md");
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        let fallback = e.file_name().to_string_lossy().into_owned();
                        if let Some(s) = Skill::parse(&text, &fallback, Some(e.path().display().to_string())) {
                            out.push(s);
                        }
                    }
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out.dedup_by(|a, b| a.name == b.name);
        out
    }

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
    fn parses_and_discovers_skills() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let s = dir.path().join(".claude/skills/release");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("SKILL.md"), "---\nname: release\ndescription: \"Cut a release\"\n---\n\n1. Bump the version.\n").unwrap();
        let found = Skill::discover(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].name.as_str(), found[0].description.as_str(), found[0].body.as_str()),
            ("release", "Cut a release", "1. Bump the version.")
        );
        assert!(Skill::index(&found, "Skill").contains("- release: Cut a release"));
    }
}

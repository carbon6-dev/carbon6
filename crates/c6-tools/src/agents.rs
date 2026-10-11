//! Subagents (Task), named agents (`.claude/agents`) and skills (Skill).

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

/// The frontmatter (between `---` lines) and body of a Markdown file.
pub fn frontmatter(text: &str) -> (&str, &str) {
    match text.strip_prefix("---").and_then(|r| r.split_once("\n---")) {
        Some((front, body)) => (front, body.trim_start_matches(['-', '\n', '\r'])),
        None => ("", text),
    }
}

/// A `key: value` from frontmatter, unquoted.
pub fn front_field(front: &str, key: &str) -> Option<String> {
    front.lines().find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix(':')).map(|v| v.trim().trim_matches('"').to_owned()))
}

/// A named subagent, in Claude Code's format: `.claude/agents/<name>.md`
/// with `name`, `description`, optional `tools` (comma-separated; all
/// when absent) and `model` frontmatter, and its instructions as the body.
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
    pub fn parse(text: &str, fallback_name: &str) -> Option<Self> {
        let (front, body) = frontmatter(text);
        let name = front_field(front, "name").filter(|n| !n.is_empty()).unwrap_or_else(|| fallback_name.to_owned());
        let tools = front_field(front, "tools")
            .map(|t| {
                t.trim_matches(['[', ']'])
                    .split(',')
                    .map(|s| s.trim().trim_matches(['"', '\'']).to_owned())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let prompt = body.trim().to_owned();
        (!prompt.is_empty()).then(|| Self {
            name,
            description: front_field(front, "description").unwrap_or_default(),
            tools,
            model: front_field(front, "model").unwrap_or_default(),
            prompt,
        })
    }

    /// Agents in `~/.claude/agents`, then `.claude/agents` from the repo
    /// root down to `cwd` (later ones replace earlier ones of the same name).
    pub fn discover(cwd: &std::path::Path) -> Vec<AgentDef> {
        let mut dirs = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(std::path::PathBuf::from(home).join(".claude/agents"));
        }
        let root = cwd.ancestors().find(|d| d.join(".git").exists()).unwrap_or(cwd);
        let mut repo: Vec<_> = cwd.ancestors().take_while(|d| d.starts_with(root)).map(|d| d.join(".claude/agents")).collect();
        repo.reverse();
        dirs.extend(repo);
        let mut out: Vec<AgentDef> = Vec::new();
        for dir in dirs {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            let mut files: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "md")).collect();
            files.sort();
            for path in files {
                let fallback = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                if let Some(a) = std::fs::read_to_string(&path).ok().and_then(|t| AgentDef::parse(&t, &fallback)) {
                    out.retain(|k| k.name != a.name);
                    out.push(a);
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

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

    #[test]
    fn parses_and_discovers_agents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let a = dir.path().join(".claude/agents");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(
            a.join("reviewer.md"),
            "---\nname: code-reviewer\ndescription: Reviews code\ntools: Read, Grep, Glob\nmodel: sonnet\n---\nYou review code.\n",
        )
        .unwrap();
        std::fs::write(a.join("empty.md"), "---\nname: nothing\n---\n").unwrap();
        let found: Vec<AgentDef> =
            AgentDef::discover(dir.path()).into_iter().filter(|a| a.name == "code-reviewer" || a.name == "nothing").collect();
        assert_eq!(found.len(), 1, "an agent needs instructions");
        let r = &found[0];
        assert_eq!((r.description.as_str(), r.model.as_str(), r.prompt.as_str()), ("Reviews code", "sonnet", "You review code."));
        assert!(r.allows("Read") && r.allows("read") && r.allows("read_file") && r.allows("grep"));
        assert!(!r.allows("Bash") && !r.allows("shell") && !r.allows("Write"));
        let spec = Task::with_agents(true, found.clone()).spec();
        assert!(spec.description.contains("- code-reviewer: Reviews code"));
        assert_eq!(spec.parameters["properties"]["subagent_type"]["enum"], json!(["general-purpose", "code-reviewer"]));
    }
}

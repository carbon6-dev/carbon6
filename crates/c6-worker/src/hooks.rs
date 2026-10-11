//! Hooks in Claude Code's `settings.json` format, from `~/.claude/settings.json`
//! and the project's `.claude/settings.json` and `.claude/settings.local.json`:
//!
//! ```json
//! {"hooks": {"PreToolUse": [{"matcher": "Bash|Edit", "hooks": [{"type": "command", "command": "./check.sh", "timeout": 30}]}]}}
//! ```
//!
//! Each command runs with `sh -c` in the session's folder, gets the event as
//! JSON on stdin and `CLAUDE_PROJECT_DIR` in its environment. Exit code 2
//! blocks (PreToolUse: the call; UserPromptSubmit: the message; Stop: the
//! end of the turn) with stderr as the reason; for PostToolUse stderr goes
//! to the model. Exit 0 with JSON on stdout can do the same with
//! `{"decision": "block", "reason": …}` (or PreToolUse's
//! `hookSpecificOutput.permissionDecision: "deny"`), and add context with
//! `hookSpecificOutput.additionalContext`; plain stdout from
//! UserPromptSubmit and SessionStart is added as context. Other exit codes
//! are errors that block nothing.
//!
//! Supported events: PreToolUse, PostToolUse, UserPromptSubmit, Stop,
//! SessionStart. Matchers (tool events) are regular expressions matched
//! against the whole tool name, ours or its Claude Code name (`shell` is
//! also `Bash`); empty or `*` matches every tool.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tracing::warn;

const DEFAULT_TIMEOUT: u64 = 60;

#[derive(Clone, Debug, Default, Deserialize)]
struct Group {
    #[serde(default)]
    matcher: String,
    #[serde(default)]
    hooks: Vec<Command>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Command {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    command: String,
    timeout: Option<u64>,
}

#[derive(Default, Deserialize)]
struct Settings {
    #[serde(default)]
    hooks: HashMap<String, Vec<Group>>,
}

pub struct HookRunner {
    session_id: String,
    cwd: PathBuf,
    project_dir: PathBuf,
    events: HashMap<String, Vec<Group>>,
}

/// What a set of hooks decided.
#[derive(Debug, Default, PartialEq)]
pub struct Verdict {
    /// Why it is blocked, if it is.
    pub blocked: Option<String>,
    /// Context to add (hook output), if any.
    pub context: Option<String>,
    /// stderr of hooks that exited 2 (PostToolUse feedback).
    pub feedback: Option<String>,
}

impl HookRunner {
    /// The hooks configured for `cwd`; `None` when there are none.
    pub fn load(session_id: &str, cwd: &Path) -> Option<Self> {
        let project_dir = cwd.ancestors().find(|d| d.join(".git").exists()).unwrap_or(cwd).to_owned();
        let mut files = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            files.push(PathBuf::from(home).join(".claude/settings.json"));
        }
        files.push(project_dir.join(".claude/settings.json"));
        files.push(project_dir.join(".claude/settings.local.json"));
        let mut events: HashMap<String, Vec<Group>> = HashMap::new();
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else { continue };
            match serde_json::from_str::<Settings>(&text) {
                Ok(s) => {
                    for (event, groups) in s.hooks {
                        events.entry(event).or_default().extend(groups);
                    }
                }
                Err(e) => warn!(file = %f.display(), "ignoring unreadable settings: {e}"),
            }
        }
        events.retain(|_, g| g.iter().any(|g| g.hooks.iter().any(|h| !h.command.trim().is_empty())));
        (!events.is_empty()).then(|| Self { session_id: session_id.to_owned(), cwd: cwd.to_owned(), project_dir, events })
    }

    /// Runs the hooks for `event` (matching `tool`, for tool events) in
    /// parallel and combines what they decided.
    pub async fn run(&self, event: &str, tool: Option<&str>, mut payload: Value) -> Verdict {
        let commands: Vec<&Command> = self
            .events
            .get(event)
            .into_iter()
            .flatten()
            .filter(|g| tool.is_none_or(|t| matches(&g.matcher, t)))
            .flat_map(|g| g.hooks.iter())
            .filter(|c| (c.kind.is_empty() || c.kind == "command") && !c.command.trim().is_empty())
            .collect();
        if commands.is_empty() {
            return Verdict::default();
        }
        payload["session_id"] = json!(self.session_id);
        payload["transcript_path"] = json!("");
        payload["cwd"] = json!(self.cwd.display().to_string());
        payload["hook_event_name"] = json!(event);
        let input = payload.to_string();
        let outcomes = futures::future::join_all(commands.iter().map(|c| self.exec(c, &input))).await;

        let mut v = Verdict::default();
        let mut blocked = Vec::new();
        let mut context = Vec::new();
        let mut feedback = Vec::new();
        for (code, stdout, stderr) in outcomes {
            match code {
                Some(2) => {
                    let why = if stderr.trim().is_empty() { "(no reason given)".to_owned() } else { stderr.trim().to_owned() };
                    if event == "PostToolUse" { feedback.push(why) } else { blocked.push(why) }
                }
                Some(0) => match serde_json::from_str::<Value>(stdout.trim()) {
                    Ok(out) if out.is_object() => {
                        let specific = &out["hookSpecificOutput"];
                        let reason =
                            out["reason"].as_str().or(specific["permissionDecisionReason"].as_str()).unwrap_or("blocked by a hook");
                        if out["decision"] == "block" || specific["permissionDecision"] == "deny" {
                            if event == "PostToolUse" { feedback.push(reason.to_owned()) } else { blocked.push(reason.to_owned()) }
                        }
                        if let Some(c) = specific["additionalContext"].as_str().filter(|c| !c.trim().is_empty()) {
                            context.push(c.trim().to_owned());
                        }
                    }
                    _ if matches!(event, "UserPromptSubmit" | "SessionStart") && !stdout.trim().is_empty() => {
                        context.push(stdout.trim().to_owned())
                    }
                    _ => {}
                },
                other => warn!(event, code = ?other, "hook failed: {}", stderr.trim()),
            }
        }
        let join = |v: Vec<String>| (!v.is_empty()).then(|| v.join("\n"));
        v.blocked = join(blocked);
        v.context = join(context);
        v.feedback = join(feedback);
        v
    }

    /// (exit code, stdout, stderr); no code on timeout or failure to start.
    async fn exec(&self, c: &Command, input: &str) -> (Option<i32>, String, String) {
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&c.command)
            .current_dir(&self.cwd)
            .env("CLAUDE_PROJECT_DIR", &self.project_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => return (None, String::new(), format!("could not run `{}`: {e}", c.command)),
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input.as_bytes()).await;
        }
        let timeout = Duration::from_secs(c.timeout.unwrap_or(DEFAULT_TIMEOUT));
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => {
                (o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
            }
            Ok(Err(e)) => (None, String::new(), e.to_string()),
            Err(_) => (None, String::new(), format!("`{}` timed out after {}s", c.command, timeout.as_secs())),
        }
    }
}

/// Claude Code's name for one of our tools, so its matchers work in every
/// tool profile.
fn claude_name(tool: &str) -> &str {
    match tool {
        "read" | "read_file" => "Read",
        "write" => "Write",
        "edit" | "apply_patch" => "Edit",
        "bash" | "shell" => "Bash",
        "grep" => "Grep",
        "glob" => "Glob",
        "web_fetch" => "WebFetch",
        "update_plan" => "TodoWrite",
        "task" => "Task",
        "load_skill" => "Skill",
        "bash_output" => "BashOutput",
        "kill_shell" => "KillShell",
        other => other,
    }
}

fn matches(matcher: &str, tool: &str) -> bool {
    let m = matcher.trim();
    if m.is_empty() || m == "*" {
        return true;
    }
    match regex::Regex::new(&format!("^(?:{m})$")) {
        Ok(re) => re.is_match(tool) || re.is_match(claude_name(tool)),
        Err(_) => m == tool || m == claude_name(tool),
    }
}

#[async_trait]
impl c6_tools::Hooks for HookRunner {
    async fn pre_tool_use(&self, tool: &str, input: &Value) -> Option<String> {
        self.run("PreToolUse", Some(tool), json!({"tool_name": tool, "tool_input": input})).await.blocked
    }

    async fn post_tool_use(&self, tool: &str, input: &Value, output: &str, is_error: bool) -> Option<String> {
        let v = self
            .run(
                "PostToolUse",
                Some(tool),
                json!({"tool_name": tool, "tool_input": input, "tool_response": {"output": output, "is_error": is_error}}),
            )
            .await;
        match (v.feedback, v.context) {
            (Some(f), Some(c)) => Some(format!("{f}\n{c}")),
            (f, c) => f.or(c),
        }
    }

    async fn stop(&self, active: bool) -> Option<String> {
        self.run("Stop", None, json!({"stop_hook_active": active})).await.blocked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c6_tools::Hooks;

    #[tokio::test]
    async fn runs_settings_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::create_dir_all(d.join(".claude")).unwrap();
        std::fs::write(
            d.join(".claude/settings.json"),
            r#"{"hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "grep -q 'rm -rf' && { echo 'no rm -rf' >&2; exit 2; }; exit 0"}]}],
                "PostToolUse": [{"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "echo '{\"decision\":\"block\",\"reason\":\"run the formatter\"}'"}]}],
                "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "echo \"branch: main ($CLAUDE_PROJECT_DIR)\""}]}]
            }}"#,
        )
        .unwrap();
        std::fs::write(d.join(".claude/settings.local.json"), r#"{"hooks": {"Stop": [{"hooks": [{"command": "exit 3"}]}]}}"#).unwrap();
        let h = HookRunner::load("s1", d).unwrap();

        // `shell` (Codex) is Bash too.
        assert_eq!(h.pre_tool_use("shell", &json!({"command": "rm -rf /"})).await.as_deref(), Some("no rm -rf"));
        assert_eq!(h.pre_tool_use("bash", &json!({"command": "ls"})).await, None);
        assert_eq!(h.pre_tool_use("read", &json!({"path": "x"})).await, None, "the matcher does not apply");
        assert_eq!(h.post_tool_use("apply_patch", &json!({}), "ok", false).await.as_deref(), Some("run the formatter"));
        let v = h.run("UserPromptSubmit", None, json!({"prompt": "hi"})).await;
        assert_eq!(v.context, Some(format!("branch: main ({})", d.display())));
        // A failing hook (not exit 2) blocks nothing.
        assert_eq!(h.stop(false).await, None);
        assert!(HookRunner::load("s1", tempfile::tempdir().unwrap().path()).is_none_or(|h| !h.events.contains_key("PreToolUse")));
    }
}

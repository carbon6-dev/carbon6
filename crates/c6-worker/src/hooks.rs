//! Hooks: shell commands the head's settings attach to points in every
//! session (`CreateSession.hooks_json`, a snapshot taken when the session
//! is created).
//!
//! Each command runs with `sh -c` in the session's folder, gets the event
//! as JSON on stdin (the same fields as Claude Code's hooks) and
//! `C6_PROJECT_DIR` (also `CLAUDE_PROJECT_DIR`) in its environment. Exit
//! code 2 blocks (PreToolUse: the call; UserPromptSubmit: the message;
//! Stop: the end of the turn) with stderr as the reason; for PostToolUse
//! stderr goes to the model. Exit 0 with JSON on stdout can do the same
//! with `{"decision": "block", "reason": …}` (or PreToolUse's
//! `hookSpecificOutput.permissionDecision: "deny"`), and add context with
//! `hookSpecificOutput.additionalContext`; plain stdout from
//! UserPromptSubmit and SessionStart is added as context. Other exit codes
//! are errors that block nothing.
//!
//! Matchers (tool events) are regular expressions matched against the whole
//! tool name, ours or its Claude Code name (`shell` is also `Bash`); empty
//! or `*` matches every tool. A hook applies only to sessions with all the
//! labels in its `applies_to` (see `c6_api::labels`).

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

/// A hook as the head stores it (`c6_api::Hook`).
#[derive(Clone, Debug, Default, Deserialize)]
struct Command {
    event: String,
    #[serde(default)]
    matcher: String,
    #[serde(default)]
    folder: String,
    command: String,
    timeout_s: Option<u64>,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    applies_to: Vec<String>,
}

fn yes() -> bool {
    true
}

pub struct HookRunner {
    session_id: String,
    cwd: PathBuf,
    project_dir: PathBuf,
    events: HashMap<String, Vec<Command>>,
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
    /// The enabled hooks in `hooks_json` that apply to a session with
    /// `labels` in `cwd`; `None` when there are none.
    pub fn new(session_id: &str, cwd: &Path, hooks_json: &str, labels: &[String]) -> Option<Self> {
        let project_dir = cwd.ancestors().find(|d| d.join(".git").exists()).unwrap_or(cwd).to_owned();
        let hooks: Vec<Command> = serde_json::from_str(hooks_json).unwrap_or_else(|e| {
            if !hooks_json.trim().is_empty() {
                warn!("ignoring unreadable hooks: {e}");
            }
            Vec::new()
        });
        let mut events: HashMap<String, Vec<Command>> = HashMap::new();
        for h in hooks {
            // `folder` is the older form of a `folder:` label.
            let mut selector = h.applies_to.clone();
            if !h.folder.trim().is_empty() {
                selector.push(format!("folder:{}", h.folder.trim()));
            }
            let applies = c6_api::labels::applies(&selector, labels);
            if h.enabled && applies && !h.command.trim().is_empty() {
                events.entry(h.event.clone()).or_default().push(h);
            }
        }
        (!events.is_empty()).then(|| Self { session_id: session_id.to_owned(), cwd: cwd.to_owned(), project_dir, events })
    }

    /// The hooks as Claude Code settings (`--settings`), for Claude Code
    /// sessions, which run hooks themselves.
    pub fn claude_settings(&self) -> String {
        let mut hooks = serde_json::Map::new();
        for (event, cmds) in &self.events {
            let groups: Vec<Value> = cmds
                .iter()
                .map(|c| {
                    let mut cmd = json!({"type": "command", "command": c.command});
                    if let Some(t) = c.timeout_s {
                        cmd["timeout"] = json!(t);
                    }
                    json!({"matcher": c.matcher, "hooks": [cmd]})
                })
                .collect();
            hooks.insert(event.clone(), Value::Array(groups));
        }
        json!({"hooks": hooks}).to_string()
    }

    /// Runs the hooks for `event` (matching `tool`, for tool events) in
    /// parallel and combines what they decided.
    pub async fn run(&self, event: &str, tool: Option<&str>, mut payload: Value) -> Verdict {
        let commands: Vec<&Command> =
            self.events.get(event).into_iter().flatten().filter(|c| tool.is_none_or(|t| matches(&c.matcher, t))).collect();
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
            .env("C6_PROJECT_DIR", &self.project_dir)
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
        let timeout = Duration::from_secs(c.timeout_s.unwrap_or(DEFAULT_TIMEOUT));
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

pub(crate) fn matches(matcher: &str, tool: &str) -> bool {
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
    async fn pre_tool_use(&self, _call_id: &str, tool: &str, input: &Value) -> Option<String> {
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
    async fn runs_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join(".git")).unwrap();
        let hooks = json!([
            {"event": "PreToolUse", "matcher": "Bash", "command": "grep -q 'rm -rf' && { echo 'no rm -rf' >&2; exit 2; }; exit 0"},
            {"event": "PostToolUse", "matcher": "Write|Edit", "command": "echo '{\"decision\":\"block\",\"reason\":\"run the formatter\"}'"},
            {"event": "UserPromptSubmit", "command": "echo \"branch: main ($C6_PROJECT_DIR)\""},
            {"event": "Stop", "command": "exit 3"},
            {"event": "Stop", "command": "echo off >&2; exit 2", "enabled": false},
            {"event": "PreToolUse", "command": "exit 2", "folder": "/somewhere/else"},
            {"event": "PreToolUse", "command": "exit 2", "applies_to": ["python"]},
            {"event": "PostToolUse", "command": "echo rusty >&2; exit 2", "applies_to": ["rust", "repo:github.com/acme"]}
        ])
        .to_string();
        let mut labels = c6_api::labels::auto_labels("w", "github.com/acme/app", &d.display().to_string(), "", "m", "");
        labels.push("rust".into());
        let h = HookRunner::new("s1", d, &hooks, &labels).unwrap();

        // `shell` (Codex) is Bash too.
        assert_eq!(h.pre_tool_use("c", "shell", &json!({"command": "rm -rf /"})).await.as_deref(), Some("no rm -rf"));
        assert_eq!(h.pre_tool_use("c", "bash", &json!({"command": "ls"})).await, None, "hooks for another folder or label do not apply");
        assert_eq!(h.pre_tool_use("c", "read", &json!({"path": "x"})).await, None, "the matcher does not apply");
        assert_eq!(h.post_tool_use("apply_patch", &json!({}), "ok", false).await.as_deref(), Some("run the formatter\nrusty"));
        let v = h.run("UserPromptSubmit", None, json!({"prompt": "hi"})).await;
        assert_eq!(v.context, Some(format!("branch: main ({})", d.display())));
        // A failing hook (not exit 2) blocks nothing; a disabled one is not run.
        assert_eq!(h.stop(false).await, None);
        // Claude Code gets them as its own settings.
        let cc: Value = serde_json::from_str(&h.claude_settings()).unwrap();
        assert_eq!(cc["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert!(HookRunner::new("s1", d, "[]", &labels).is_none());
    }
}

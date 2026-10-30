//! Tool profiles: the same implementations presented with the names and
//! input shapes each model family was trained on.

use std::sync::Arc;

use async_trait::async_trait;
use c6_llm::{ToolProfile, ToolSpec};
use serde_json::{Map, Value, json};

use crate::{
    ApplyPatch, Bash, BashOutput, Concurrency, Edit, Glob, Grep, KillShell, Plan, Read, SendMessage, Task, Tool, ToolCtx, ToolOutput,
    WebFetch, Write,
};

pub fn tools_for(profile: ToolProfile) -> Vec<Arc<dyn Tool>> {
    match profile {
        ToolProfile::Generic => vec![
            Arc::new(Read),
            Arc::new(Write),
            Arc::new(Edit),
            Arc::new(Bash),
            Arc::new(BashOutput::generic()),
            Arc::new(KillShell::generic()),
            Arc::new(Grep),
            Arc::new(Glob),
            Arc::new(WebFetch::generic()),
            Arc::new(Plan::update_plan()),
            Arc::new(Task::generic()),
            Arc::new(SendMessage::generic()),
        ],
        ToolProfile::Claude => claude(),
        ToolProfile::Codex => vec![
            Arc::new(Shell),
            Arc::new(ApplyPatch),
            Arc::new(Plan::update_plan()),
            Arc::new(WebFetch::generic()),
            Arc::new(BashOutput::generic()),
            Arc::new(KillShell::generic()),
        ],
    }
}

/// Wraps a tool under another name and schema, translating inputs.
struct Adapted {
    spec: ToolSpec,
    inner: Arc<dyn Tool>,
    map: fn(Map<String, Value>) -> Map<String, Value>,
}

#[async_trait]
impl Tool for Adapted {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn concurrency(&self) -> Concurrency {
        self.inner.concurrency()
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        match input {
            Value::Object(m) => self.inner.call(ctx, Value::Object((self.map)(m))).await,
            _ => ToolOutput::err("input must be a JSON object"),
        }
    }
}

fn rename(mut m: Map<String, Value>, from: &str, to: &str) -> Map<String, Value> {
    if let Some(v) = m.remove(from) {
        m.insert(to.to_owned(), v);
    }
    m
}

fn claude() -> Vec<Arc<dyn Tool>> {
    let file_path = json!({"type": "string", "description": "The absolute path to the file"});
    vec![
        Arc::new(Adapted {
            spec: ToolSpec {
                name: "Read".into(),
                description: "Reads a file from the local filesystem. Returns up to 2000 lines by default with line numbers \
                              (cat -n format); use offset and limit for long files. Always read a file before editing it."
                    .into(),
                parameters: json!({"type": "object", "properties": {
                    "file_path": file_path,
                    "offset": {"type": "integer", "description": "The line number to start reading from"},
                    "limit": {"type": "integer", "description": "The number of lines to read"}
                }, "required": ["file_path"], "additionalProperties": false}),
            },
            inner: Arc::new(Read),
            map: |m| rename(m, "file_path", "path"),
        }),
        Arc::new(Adapted {
            spec: ToolSpec {
                name: "Write".into(),
                description: "Writes a file, overwriting it if it exists. An existing file must be read first. Prefer Edit for changes \
                              to existing files."
                    .into(),
                parameters: json!({"type": "object", "properties": {
                    "file_path": file_path,
                    "content": {"type": "string", "description": "The content to write"}
                }, "required": ["file_path", "content"], "additionalProperties": false}),
            },
            inner: Arc::new(Write),
            map: |m| rename(m, "file_path", "path"),
        }),
        Arc::new(Adapted {
            spec: ToolSpec {
                name: "Edit".into(),
                description: "Performs an exact string replacement in a file. old_string must match exactly, including indentation, \
                              and be unique unless replace_all is set. The file must have been read first."
                    .into(),
                parameters: json!({"type": "object", "properties": {
                    "file_path": file_path,
                    "old_string": {"type": "string", "description": "The text to replace"},
                    "new_string": {"type": "string", "description": "The text to replace it with"},
                    "replace_all": {"type": "boolean", "description": "Replace all occurrences (default false)"}
                }, "required": ["file_path", "old_string", "new_string"], "additionalProperties": false}),
            },
            inner: Arc::new(Edit),
            map: |m| rename(m, "file_path", "path"),
        }),
        Arc::new(Adapted {
            spec: ToolSpec {
                name: "Bash".into(),
                description: "Executes a bash command and returns combined stdout and stderr. The working directory persists between \
                              calls. stdin is closed. Default timeout 120000 ms, max 600000 ms. Prefer Read, Grep and Glob over \
                              cat, grep and find."
                    .into(),
                parameters: json!({"type": "object", "properties": {
                    "command": {"type": "string", "description": "The command to execute"},
                    "timeout": {"type": "number", "description": "Optional timeout in milliseconds (max 600000)"},
                    "description": {"type": "string", "description": "Clear, concise description of what this command does in 5-10 words"},
                    "run_in_background": {"type": "boolean", "description": "Run in the background; inspect output with BashOutput. Completion is pushed automatically by default; do not poll just to wait."},
                    "notify": {"type": "boolean", "description": "With run_in_background: be told when it finishes (default true). Set false for servers and watchers meant to keep running."}
                }, "required": ["command"], "additionalProperties": false}),
            },
            inner: Arc::new(Bash),
            map: |m| rename(m, "timeout", "timeout_ms"),
        }),
        Arc::new(Adapted {
            spec: ToolSpec {
                name: "Grep".into(),
                description: "Searches file contents with ripgrep-compatible regular expressions, respecting .gitignore. \
                              output_mode: files_with_matches (default), content, or count."
                    .into(),
                parameters: json!({"type": "object", "properties": {
                    "pattern": {"type": "string", "description": "The regular expression to search for"},
                    "path": {"type": "string", "description": "File or directory to search in (default: working directory)"},
                    "glob": {"type": "string", "description": "Glob to filter files, e.g. \"*.js\""},
                    "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"]},
                    "-i": {"type": "boolean", "description": "Case insensitive search"},
                    "-C": {"type": "number", "description": "Lines of context around each match (content mode)"},
                    "head_limit": {"type": "number", "description": "Limit output to the first N lines or entries"}
                }, "required": ["pattern"], "additionalProperties": false}),
            },
            inner: Arc::new(Grep),
            map: |m| rename(rename(m, "-i", "case_insensitive"), "-C", "context"),
        }),
        Arc::new(Adapted {
            spec: ToolSpec {
                name: "Glob".into(),
                description: "Fast file pattern matching, e.g. \"**/*.ts\". Returns matching paths sorted by modification time.".into(),
                parameters: json!({"type": "object", "properties": {
                    "pattern": {"type": "string", "description": "The glob pattern to match files against"},
                    "path": {"type": "string", "description": "The directory to search in (default: working directory)"}
                }, "required": ["pattern"], "additionalProperties": false}),
            },
            inner: Arc::new(Glob),
            map: |m| m,
        }),
        Arc::new(BashOutput::claude()),
        Arc::new(KillShell::claude()),
        Arc::new(WebFetch::claude()),
        Arc::new(Plan::todo_write()),
        Arc::new(Task::claude()),
        Arc::new(SendMessage::claude()),
    ]
}

/// Codex `shell`: argv array, optional workdir.
pub struct Shell;

#[async_trait]
impl Tool for Shell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shell".into(),
            description: "Runs a shell command and returns its output. `command` is an argv array, typically \
                          [\"bash\", \"-lc\", \"<script>\"]. Use rg for searching and sed -n or cat for reading files; make \
                          edits with apply_patch. Background completion is pushed automatically by default; do not poll just to wait."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "command": {"type": "array", "items": {"type": "string"}, "description": "The command to execute"},
                "workdir": {"type": "string", "description": "The working directory to execute the command in"},
                "timeout_ms": {"type": "number", "description": "The timeout for the command in milliseconds"},
                "run_in_background": {"type": "boolean", "description": "Start in the background and return an id for bash_output / kill_shell. Completion is pushed automatically by default; do not poll just to wait."},
                "notify": {"type": "boolean", "description": "With run_in_background: push completion automatically (default true). Set false for servers and watchers meant to keep running."}
            }, "required": ["command"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let Some(argv) = input["command"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()) else {
            return ToolOutput::err("command must be an array of strings");
        };
        let script = match argv.as_slice() {
            [] => return ToolOutput::err("command is empty"),
            // The common form: run the script itself.
            [sh, flag, script]
                if matches!(*sh, "bash" | "sh" | "zsh" | "/bin/bash" | "/bin/sh") && flag.starts_with('-') && flag.ends_with('c') =>
            {
                (*script).to_owned()
            }
            args => args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" "),
        };
        let script = match input["workdir"].as_str() {
            Some(dir) => format!("cd {} && {script}", shell_quote(&ctx.session.resolve(dir).display().to_string())),
            None => script,
        };
        let mut bash_input = json!({"command": script});
        if let Some(t) = input["timeout_ms"].as_u64() {
            bash_input["timeout_ms"] = t.into();
        }
        for key in ["run_in_background", "notify"] {
            if let Some(value) = input.get(key) {
                bash_input[key] = value.clone();
            }
        }
        Bash.call(ctx, bash_input).await
    }
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionTools;

    #[tokio::test]
    async fn codex_background_flags_and_tools_reach_bash() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx {
            session: Arc::new(SessionTools::new(dir.path().to_owned())),
            cancel: Default::default(),
            output: Arc::new(|_: &str| {}),
            call_id: String::new(),
        };
        let tools = tools_for(ToolProfile::Codex);
        let get = |name: &str| tools.iter().find(|t| t.spec().name == name).unwrap().clone();
        let shell = get("shell");
        assert_eq!(shell.spec().parameters["properties"]["run_in_background"]["type"], "boolean");
        assert_eq!(shell.spec().parameters["properties"]["notify"]["type"], "boolean");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.session.background.set_sink(Some(tx));
        for (index, notify) in [None, Some(false)].into_iter().enumerate() {
            let mut input = json!({"command": ["bash", "-lc", "echo ready; sleep 30"], "run_in_background": true});
            if let Some(notify) = notify {
                input["notify"] = notify.into();
            }
            let r = shell.call(&ctx, input).await;
            let id = format!("bg{}", index + 1);
            assert!(r.content.contains(&id), "{r:?}");
            let event = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
            assert_eq!(event.state, crate::TaskState::Started);
            assert_eq!(event.notify, notify.unwrap_or(true));
            let r = get("kill_shell").call(&ctx, json!({"id": id})).await;
            assert!(r.content.contains("Stopped"), "{r:?}");
            let event = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
            assert_eq!(event.state, crate::TaskState::Stopped);
            assert_eq!(event.notify, notify.unwrap_or(true));
            let r = get("bash_output").call(&ctx, json!({"id": id})).await;
            assert!(r.content.contains("exited with code"), "{r:?}");
        }
    }

    #[tokio::test]
    async fn claude_and_codex_shapes_reach_the_same_tools() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "alpha\nbeta\n").unwrap();
        let ctx = ToolCtx {
            session: Arc::new(SessionTools::new(dir.path().to_owned())),
            cancel: Default::default(),
            output: Arc::new(|_: &str| {}),
            call_id: String::new(),
        };

        let claude = tools_for(ToolProfile::Claude);
        let get = |n: &str| claude.iter().find(|t| t.spec().name == n).unwrap().clone();
        let r = get("Read").call(&ctx, json!({"file_path": "f.txt"})).await;
        assert!(r.content.contains("alpha"), "{r:?}");
        let r = get("Grep").call(&ctx, json!({"pattern": "BETA", "-i": true, "output_mode": "content"})).await;
        assert!(r.content.contains("f.txt:2:beta"), "{r:?}");

        let r = Shell.call(&ctx, json!({"command": ["bash", "-lc", "echo $((1+2))"]})).await;
        assert_eq!(r.content, "3\n");
        let r = Shell.call(&ctx, json!({"command": ["echo", "it's"], "workdir": "."})).await;
        assert_eq!(r.content, "it's\n");

        let names: Vec<String> = tools_for(ToolProfile::Codex).iter().map(|t| t.spec().name).collect();
        assert_eq!(names, ["shell", "apply_patch", "update_plan", "web_fetch", "bash_output", "kill_shell"]);
    }
}

//! Plan / todo tracking. The tool only acknowledges; the plan itself lives
//! in the recorded tool input, which the UI renders.

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

pub struct Plan {
    spec: ToolSpec,
    list_key: &'static str,
}

impl Plan {
    /// Codex shape: `{explanation?, plan: [{step, status}]}`.
    pub fn update_plan() -> Self {
        Self {
            spec: ToolSpec {
                name: "update_plan".into(),
                description: "Updates the task plan. Provide the full list of steps with their status each time; keep exactly one \
                              step in_progress while working. Use it for multi-step work, not for trivial tasks."
                    .into(),
                parameters: json!({"type": "object", "properties": {
                    "explanation": {"type": "string"},
                    "plan": {"type": "array", "items": {"type": "object", "properties": {
                        "step": {"type": "string"},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                    }, "required": ["step", "status"], "additionalProperties": false}}
                }, "required": ["plan"], "additionalProperties": false}),
            },
            list_key: "plan",
        }
    }

    /// Claude Code shape: `{todos: [{content, status, activeForm}]}`.
    pub fn todo_write() -> Self {
        Self {
            spec: ToolSpec {
                name: "TodoWrite".into(),
                description: "Creates and updates a structured task list for the current session. Send the complete list each time. \
                              Mark a task in_progress before starting it and completed as soon as it is done; keep one task \
                              in_progress at a time. Use it for multi-step work, not for single trivial tasks."
                    .into(),
                parameters: json!({"type": "object", "properties": {
                    "todos": {"type": "array", "items": {"type": "object", "properties": {
                        "content": {"type": "string"},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                        "activeForm": {"type": "string"}
                    }, "required": ["content", "status", "activeForm"], "additionalProperties": false}}
                }, "required": ["todos"], "additionalProperties": false}),
            },
            list_key: "todos",
        }
    }
}

#[async_trait]
impl Tool for Plan {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, _ctx: &ToolCtx, input: Value) -> ToolOutput {
        match input[self.list_key].as_array() {
            Some(items) => {
                let done = items.iter().filter(|i| i["status"] == "completed").count();
                ToolOutput::ok(format!("Plan updated ({done}/{} done).", items.len()))
            }
            None => ToolOutput::err(format!("`{}` must be an array", self.list_key)),
        }
    }
}

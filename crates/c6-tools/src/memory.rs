//! remember / forget: the agent keeps durable notes across sessions. Each
//! note has a scope (one label, e.g. host:db-2; empty for every session);
//! sessions get the notes for their labels in their system prompt.

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

/// A change to a note, for the runtime to record.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryChange {
    /// "remember" or "forget".
    pub op: String,
    pub id: String,
    pub scope: String,
    pub text: String,
}

pub type MemorySink = tokio::sync::mpsc::UnboundedSender<MemoryChange>;

const MAX_TEXT: usize = 2000;

fn new_id() -> String {
    let mut b = [0u8; 6];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub struct Remember {
    claude: bool,
}

impl Remember {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

#[async_trait]
impl Tool for Remember {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "Remember" } else { "remember" }.into(),
            description: "Saves a note to memory, kept across sessions: durable facts worth knowing next time (how a machine is set \
                          up, where things are, conventions, the user's preferences), not this task's progress. Give it a scope: \
                          one of this session's labels (e.g. host:web1, repo:github.com/acme/app) to keep it to matching sessions, \
                          or empty for every session. To correct a note, pass its id as replaces."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "text": {"type": "string", "description": "The note: one fact, self-contained"},
                "scope": {"type": "string", "description": "A label, or empty for every session"},
                "replaces": {"type": "string", "description": "The id of a note this one replaces"}
            }, "required": ["text"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let Some(sink) = ctx.session.memory() else { return ToolOutput::err("memory is not available here") };
        let text = input["text"].as_str().unwrap_or("").trim();
        if text.is_empty() || text.len() > MAX_TEXT {
            return ToolOutput::err(format!("a note has 1 to {MAX_TEXT} characters"));
        }
        let scope = input["scope"].as_str().unwrap_or("").trim().to_owned();
        if scope.contains(char::is_whitespace) || scope.contains(',') {
            return ToolOutput::err("scope is a single label (or empty)");
        }
        let id = input["replaces"].as_str().map(str::trim).filter(|r| !r.is_empty()).map(str::to_owned).unwrap_or_else(new_id);
        let change = MemoryChange { op: "remember".into(), id: id.clone(), scope: scope.clone(), text: text.to_owned() };
        if sink.send(change).is_err() {
            return ToolOutput::err("memory is not available here");
        }
        ToolOutput::ok(format!("Saved note {id} ({}).", if scope.is_empty() { "every session".to_owned() } else { scope }))
    }
}

pub struct Forget {
    claude: bool,
}

impl Forget {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

#[async_trait]
impl Tool for Forget {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "Forget" } else { "forget" }.into(),
            description: "Removes a memory note that is wrong or no longer true, by its id (shown in brackets in the Memory section)."
                .into(),
            parameters: json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let Some(sink) = ctx.session.memory() else { return ToolOutput::err("memory is not available here") };
        let Some(id) = input["id"].as_str().map(str::trim).filter(|i| !i.is_empty()) else { return ToolOutput::err("id is required") };
        let change = MemoryChange { op: "forget".into(), id: id.to_owned(), scope: String::new(), text: String::new() };
        if sink.send(change).is_err() {
            return ToolOutput::err("memory is not available here");
        }
        ToolOutput::ok(format!("Forgot note {id}."))
    }
}

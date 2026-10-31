//! Messages between sessions (docs/background.md §6): an agent sends
//! another session in its realm a message, on any worker, and goes on; the
//! other session's agent gets it as a notice. Fire and forget: a reply, if
//! any, comes the same way.

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

async fn call(ctx: &ToolCtx, method: &str, params: Value) -> Result<Value, String> {
    let head = ctx.session.head().ok_or("carbon6's head is not reachable from here")?;
    head.call(method, params).await
}

/// Whether `name` is one of these tools (subagents don't get them: their
/// head calls go as their top-level session's, and what they find goes
/// back to their parent).
pub fn is_message_tool(name: &str) -> bool {
    matches!(name, "message_session" | "MessageSession" | "list_sessions" | "ListSessions")
}

/// Sends another session a message.
pub struct MessageSession {
    claude: bool,
}

impl MessageSession {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

#[async_trait]
impl Tool for MessageSession {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "MessageSession" } else { "message_session" }.into(),
            description: "Sends a message to another session's agent (find sessions with the list-sessions tool), on whatever machine it \
                          runs, and returns at once: it does not wait for an answer. That agent gets it at its next step, or starts a turn \
                          for it if idle; a reply, if any, arrives the same way, as a message from that session. Say who you are and what \
                          you need; it cannot see this conversation. Not for your own subagents: use their tools."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "to": {"type": "string", "description": "The session's id"},
                "message": {"type": "string", "description": "What to tell it, standing on its own"}
            }, "required": ["to", "message"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let params = json!({"to": input["to"], "message": input["message"], "hops": ctx.session.message_hops() + 1});
        match call(ctx, "session.message", params).await {
            Ok(v) => {
                let title = v["title"].as_str().unwrap_or_default();
                if v["queued"].as_bool() == Some(true) {
                    ToolOutput::ok(format!("Queued for \"{title}\": its worker is offline; it gets the message when it is back."))
                } else {
                    ToolOutput::ok(format!("Sent to \"{title}\". Carry on; a reply, if any, comes as a message."))
                }
            }
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// Lists the sessions of this realm, to find one to message.
pub struct ListSessions {
    claude: bool,
}

impl ListSessions {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

#[async_trait]
impl Tool for ListSessions {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "ListSessions" } else { "list_sessions" }.into(),
            description: "Lists other sessions you can message (this realm's, not archived), most recently active first, each \
                          session's side chats under it: id, title, whether it is running, idle or offline, its worker and labels."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "query": {"type": "string", "description": "Only sessions whose title, labels or folder contain this"}
            }, "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        match call(ctx, "session.sessions", json!({"query": input["query"]})).await {
            Ok(v) => {
                let rows: Vec<String> = v["sessions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| {
                        let labels = s["labels"].as_array().map(|l| l.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
                        let side = s["side_chat_of"]["title"].as_str().map(|p| format!(" · side chat of \"{p}\"")).unwrap_or_default();
                        format!(
                            "{}- {} \"{}\"{side} · {} · worker {}{}",
                            if side.is_empty() { "" } else { "  " },
                            s["id"].as_str().unwrap_or_default(),
                            s["title"].as_str().unwrap_or_default(),
                            s["state"].as_str().unwrap_or_default(),
                            s["worker"].as_str().unwrap_or_default(),
                            labels.filter(|l| !l.is_empty()).map(|l| format!(" · {l}")).unwrap_or_default()
                        )
                    })
                    .collect();
                if rows.is_empty() { ToolOutput::ok("No other sessions match.") } else { ToolOutput::ok(rows.join("\n")) }
            }
            Err(e) => ToolOutput::err(e),
        }
    }
}

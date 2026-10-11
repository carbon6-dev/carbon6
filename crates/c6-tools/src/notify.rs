//! notify: the agent tells the user something worth their attention now
//! (a finding, a problem, that a long job is done). The head sends it to
//! the user's notification channels (phone, chat, email).

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

/// A notice, for the runtime to record.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub title: String,
    pub message: String,
    pub priority: String,
}

pub type NoticeSink = tokio::sync::mpsc::UnboundedSender<Notice>;

const MAX_TITLE: usize = 200;
const MAX_MESSAGE: usize = 4000;

pub struct Notify {
    claude: bool,
}

impl Notify {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

#[async_trait]
impl Tool for Notify {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "Notify" } else { "notify" }.into(),
            description: "Sends the user a notification (to their phone, chat or email): for something they should know about now, \
                          like a problem found, a result they asked to be told about, or a long job finished. Not for progress \
                          updates; the transcript has those. Keep it short and self-contained."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "title": {"type": "string", "description": "A few words, e.g. Disk almost full on web1"},
                "message": {"type": "string", "description": "The details, a few lines at most"},
                "priority": {"type": "string", "enum": ["low", "normal", "high"], "description": "high: needs attention soon"}
            }, "required": ["title"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let Some(sink) = ctx.session.notices() else { return ToolOutput::err("notifications are not available here") };
        let title = input["title"].as_str().unwrap_or("").trim();
        let message = input["message"].as_str().unwrap_or("").trim();
        if title.is_empty() || title.len() > MAX_TITLE {
            return ToolOutput::err(format!("a title has 1 to {MAX_TITLE} characters"));
        }
        if message.len() > MAX_MESSAGE {
            return ToolOutput::err(format!("a message has at most {MAX_MESSAGE} characters"));
        }
        let priority = match input["priority"].as_str().unwrap_or("normal") {
            p @ ("low" | "normal" | "high") => p.to_owned(),
            _ => return ToolOutput::err("priority is low, normal or high"),
        };
        if sink.send(Notice { title: title.to_owned(), message: message.to_owned(), priority }).is_err() {
            return ToolOutput::err("notifications are not available here");
        }
        ToolOutput::ok("Sent to the user's notification channels.")
    }
}

//! Next steps and trackers (docs/follow-through.md): the agent offers the
//! user a next step, and reads what its session's trackers (CI runs and the
//! like) are doing.

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

async fn call(ctx: &ToolCtx, method: &str, params: Value) -> Result<Value, String> {
    let head = ctx.session.head().ok_or("carbon6's head is not reachable from here")?;
    head.call(method, params).await
}

pub struct Suggest {
    claude: bool,
}

impl Suggest {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

#[async_trait]
impl Tool for Suggest {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "Suggest" } else { "suggest" }.into(),
            description: "Offers the user a next step as a button under this turn (for example \"Open a PR for feature-x\" after a push, \
                          or \"Push the fix branch\"). Clicking it sends `prompt` (default: the text) as their message. Use it for the \
                          obvious next thing to do, not for every option."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "text": {"type": "string", "description": "A few words, e.g. Open a PR for feature-x"},
                "prompt": {"type": "string", "description": "What the user's message will say if they take it"}
            }, "required": ["text"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let text = input["text"].as_str().unwrap_or("").trim();
        if text.is_empty() || text.len() > 200 {
            return ToolOutput::err("a next step has 1 to 200 characters");
        }
        match call(ctx, "session.suggest", json!({"text": text, "prompt": input["prompt"]})).await {
            Ok(v) if v["offered"] == true => ToolOutput::ok("Offered to the user."),
            Ok(_) => ToolOutput::ok("That next step is already offered."),
            Err(e) => ToolOutput::err(e),
        }
    }
}

pub struct Followups {
    claude: bool,
}

impl Followups {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

#[async_trait]
impl Tool for Followups {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "Followups" } else { "followups" }.into(),
            description: "What this session is following outside carbon6 (its trackers: CI runs and the like, with their phase, \
                          progress, link and any investigation) and the next steps offered to the user. Check it before saying \
                          whether CI passed."
                .into(),
            parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, _input: Value) -> ToolOutput {
        match call(ctx, "session.followups", json!({})).await {
            Ok(v) => {
                let mut out = String::new();
                for t in v["trackers"].as_array().into_iter().flatten() {
                    out.push_str(&format!(
                        "Tracker {} ({}): {} — {}{}{}\n",
                        t["title"].as_str().unwrap_or(""),
                        t["kit"].as_str().unwrap_or(""),
                        t["phase"].as_str().unwrap_or(""),
                        t["line"].as_str().unwrap_or(""),
                        t["url"].as_str().filter(|u| !u.is_empty()).map(|u| format!(" ({u})")).unwrap_or_default(),
                        t["investigation"].as_str().map(|s| format!(" · investigation: session {s}")).unwrap_or_default(),
                    ));
                }
                for s in v["suggestions"].as_array().into_iter().flatten().filter(|s| s["state"] == "open" && s["kind"] == "next") {
                    out.push_str(&format!("Next step offered: {}\n", s["text"].as_str().unwrap_or("")));
                }
                ToolOutput::ok(if out.is_empty() { "Nothing is being followed in this session.".to_owned() } else { out })
            }
            Err(e) => ToolOutput::err(e),
        }
    }
}

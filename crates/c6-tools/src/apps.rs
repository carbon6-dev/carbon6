//! Calling apps' operations (docs/apps.md §7): how an agent changes what an
//! app owns, in its session's realm or in another realm through a grant.

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

pub struct AppCall {
    claude: bool,
}

impl AppCall {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

#[async_trait]
impl Tool for AppCall {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "AppCall" } else { "app_call" }.into(),
            description: "Calls an operation of an app installed in this realm: the way to change what an app owns (its collections \
                          are written only by it). With no instance and operation, lists what there is to call, with each input's \
                          schema. A write runs at once; a template operation starts the app's own session."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "instance": {"type": "string", "description": "The app instance's name in the realm"},
                "operation": {"type": "string"},
                "input": {"type": "object", "description": "The operation's input, matching its schema"},
                "realm": {"type": "string", "description": "Another realm whose app a grant lets this one call; this session's when left out"}
            }, "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let Some(head) = ctx.session.head() else {
            return ToolOutput::err("carbon6's head is not reachable from here");
        };
        match head.call("session.app_call", input).await {
            Ok(v) => ToolOutput::ok(serde_json::to_string_pretty(&v).unwrap_or_default()),
            Err(e) => ToolOutput::err(e),
        }
    }
}

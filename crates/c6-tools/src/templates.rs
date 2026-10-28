//! Starting templates (docs/packages.md §6): in the session's realm, or in
//! another realm whose app exports the template to this one.

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

pub struct TemplateStart {
    claude: bool,
}

impl TemplateStart {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

#[async_trait]
impl Tool for TemplateStart {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "TemplateStart" } else { "template_start" }.into(),
            description: "Starts a template: new sessions that do its work, in this realm or in another realm whose app offers the \
                          template to this one (an app this realm's app requires). Its sessions run on this session's worker; \
                          a template naming other workers, hosts or SSH targets needs the user's grant first (grant_request, \
                          kind workers). Returns the started sessions' ids; they run on their own."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "template": {"type": "string"},
                "input": {"type": "string", "description": "What the template is asked to do"},
                "realm": {"type": "string", "description": "The realm the template belongs to; this session's when left out"}
            }, "required": ["template"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let Some(head) = ctx.session.head() else {
            return ToolOutput::err("carbon6's head is not reachable from here");
        };
        match head.call("session.template_start", input).await {
            Ok(v) => ToolOutput::ok(serde_json::to_string_pretty(&v).unwrap_or_default()),
            Err(e) => ToolOutput::err(e),
        }
    }
}

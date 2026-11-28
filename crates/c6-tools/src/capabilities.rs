//! Running capabilities declared in the session's project contract.

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

pub struct Capability {
    claude: bool,
}

impl Capability {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

#[async_trait]
impl Tool for Capability {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "Capability" } else { "capability" }.into(),
            description: "Runs a capability declared by the session's project by launching its configured template as a subagent. Pass \
                          its name and any input it requires. Use action 'list' to discover declared capabilities. The run may be queued; \
                          use its run_id to check status when needed rather than repeatedly polling or starting it again."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "name": {"type": "string", "description": "The declared capability to run; required when starting a run"},
                "input": {"type": "object", "description": "The input object matching the capability's declared input schema"},
                "run_id": {"type": "string", "description": "Check this existing run instead of starting a new one"},
                "action": {"type": "string", "enum": ["list"], "description": "List declared capabilities instead of starting a run"}
            }, "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let Some(head) = ctx.session.head() else {
            return ToolOutput::err("The head is not reachable from here");
        };
        match head.call("session.capability", input).await {
            Ok(v) => ToolOutput::ok(serde_json::to_string_pretty(&v).unwrap_or_default()),
            Err(e) => ToolOutput::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{HeadCaller, SessionTools};

    struct ExpectedCall(Value, Result<Value, String>);

    #[async_trait]
    impl HeadCaller for ExpectedCall {
        async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            assert_eq!(method, "session.capability");
            assert_eq!(params, self.0);
            self.1.clone()
        }
    }

    #[test]
    fn profile_names_match_tool_lists() {
        let generic = Capability::new(false).spec();
        let claude = Capability::new(true).spec();
        assert_eq!(generic.name, "capability");
        assert_eq!(claude.name, "Capability");
        assert!(crate::tool_allowed(&[generic.name], &claude.name));
        assert_eq!(generic.parameters, claude.parameters);
    }

    #[tokio::test]
    async fn forwards_run_status_and_list_without_rewriting() {
        let ctx = ToolCtx {
            session: Arc::new(SessionTools::new(".".into())),
            cancel: CancellationToken::new(),
            output: Arc::new(|_| {}),
            call_id: "call-a".into(),
        };
        let tool = Capability::new(false);
        let run = json!({"name": "widget", "input": {"parts": ["a", "b"], "count": 2}});
        for (params, response) in [
            (run.clone(), json!({"status": "queued", "details": {"run_id": "run-a", "sessions": ["session-a"], "errors": []}})),
            (json!({"run_id": "run-a"}), json!({"status": "running", "details": {"run_id": "run-a", "session": "session-a"}})),
            (json!({"action": "list"}), json!({"version": 1, "capabilities": [{"value": {"name": "widget"}}]})),
        ] {
            ctx.session.set_head(Some(Arc::new(ExpectedCall(params.clone(), Ok(response.clone())))));
            let result = tool.call(&ctx, params).await;
            assert!(!result.is_error);
            assert_eq!(serde_json::from_str::<Value>(&result.content).unwrap(), response);
        }

        ctx.session.set_head(Some(Arc::new(ExpectedCall(run.clone(), Err("not allowed".into())))));
        assert_eq!(tool.call(&ctx, run.clone()).await, ToolOutput::err("not allowed"));
        ctx.session.set_head(None);
        assert!(tool.call(&ctx, run).await.is_error);
    }
}

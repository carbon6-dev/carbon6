//! carbon6's operations, for sessions (docs/follow-through.md §5): find,
//! read and run what the UI can do, within the rules for sessions (reads,
//! kits that are dry-run before they act, action items, knowledge; nothing
//! destructive, nothing that changes a session's powers or reveals a
//! secret).

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

async fn call(ctx: &ToolCtx, method: &str, params: Value) -> Result<Value, String> {
    let head = ctx.session.head().ok_or("carbon6's head is not reachable from here")?;
    head.call(method, params).await
}

pub struct C6Discover;

#[async_trait]
impl Tool for C6Discover {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "c6_discover".into(),
            description: "Finds carbon6 operations (everything its UI does: workers, connections, kits, trackers, action items, \
                          settings…) and documentation. No query: the areas; a query: the best matches."
                .into(),
            parameters: json!({"type": "object", "properties": {"query": {"type": "string"}, "area": {"type": "string"}}, "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        match call(ctx, "c6.discover", input).await {
            Ok(v) => ToolOutput::limited(v.as_str().unwrap_or_default().to_owned(), false),
            Err(e) => ToolOutput::err(e),
        }
    }
}

pub struct C6Describe;

#[async_trait]
impl Tool for C6Describe {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "c6_describe".into(),
            description: "A carbon6 operation in full: its input schema, effect and where its choices come from.".into(),
            parameters: json!({"type": "object", "properties": {"op": {"type": "string"}}, "required": ["op"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        match call(ctx, "c6.describe", input).await {
            Ok(v) => ToolOutput::limited(serde_json::to_string_pretty(&v).unwrap_or_default(), false),
            Err(e) => ToolOutput::err(e),
        }
    }
}

pub struct C6Call;

#[async_trait]
impl Tool for C6Call {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "c6_call".into(),
            description: "Runs a carbon6 operation. A session may read anything; it may write project kits (settings.kits.put: send every \
                          kit back; what you change is dry-run, then active or marked broken), action items, knowledge and notes. It \
                          cannot delete or revoke, see secrets, or change tools, permissions or credentials: raise those for the user \
                          with todo_add."
                .into(),
            parameters: json!({"type": "object", "properties": {"op": {"type": "string"}, "input": {"type": "object"}}, "required": ["op"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        match call(ctx, "c6.call", input).await {
            Ok(v) => ToolOutput::limited(serde_json::to_string_pretty(&v).unwrap_or_default(), false),
            Err(e) => ToolOutput::err(e),
        }
    }
}

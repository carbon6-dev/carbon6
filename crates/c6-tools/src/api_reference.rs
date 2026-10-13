//! carbon6's API reference, for agents writing code that works with
//! carbon6 (scripts, importers, integrations). It reads the head's
//! operation catalog; it never calls carbon6 itself.

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

pub struct ApiReference {
    claude: bool,
}

impl ApiReference {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

#[async_trait]
impl Tool for ApiReference {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "Carbon6Api" } else { "carbon6_api" }.into(),
            description: "The reference for carbon6's HTTP API, for writing code that works with carbon6 (scripts, importers, \
                          integrations): the base URL, auth with an API token, errors, and each operation's method, path and input \
                          schema. With no arguments: the basics and the areas. query: operations matching words. op: one operation in \
                          full. save_openapi: writes an OpenAPI 3.1 document to a file, for code generation. It only reads the \
                          reference; the code you write calls carbon6 with the user's token."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "query": {"type": "string", "description": "Words, e.g. \"upload a document\" or \"start a session\""},
                "op": {"type": "string", "description": "An operation's name, e.g. sessions.create"},
                "save_openapi": {"type": "string", "description": "A file path (relative to the working directory) for the OpenAPI document"}
            }, "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let Some(head) = ctx.session.head() else { return ToolOutput::err("carbon6's head is not reachable from here") };
        if let Some(path) = input["save_openapi"].as_str().filter(|p| !p.trim().is_empty()) {
            let spec = match head.call("session.openapi", json!({})).await {
                Ok(s) => s,
                Err(e) => return ToolOutput::err(e),
            };
            let file = ctx.session.resolve(path.trim());
            let text = serde_json::to_string_pretty(&spec).unwrap_or_default();
            if let Some(dir) = file.parent()
                && let Err(e) = tokio::fs::create_dir_all(dir).await
            {
                return ToolOutput::err(format!("creating {}: {e}", dir.display()));
            }
            if let Err(e) = tokio::fs::write(&file, &text).await {
                return ToolOutput::err(format!("writing {}: {e}", file.display()));
            }
            let ops = spec["paths"].as_object().map_or(0, |p| p.values().map(|i| i.as_object().map_or(0, |m| m.len())).sum::<usize>());
            let rpc = spec["paths"]["/api/v1/memex/rpc"]["post"]["requestBody"]["content"]["application/json"]["schema"]["oneOf"]
                .as_array()
                .map_or(0, Vec::len);
            return ToolOutput::ok(format!(
                "Wrote the OpenAPI 3.1 document to {} ({} KB): {ops} HTTP operations, and {rpc} memex methods as variants of \
                 POST /api/v1/memex/rpc.",
                file.display(),
                text.len() / 1024
            ));
        }
        let params = json!({"query": input["query"].as_str().unwrap_or(""), "op": input["op"].as_str().unwrap_or("")});
        match head.call("session.api", params).await {
            Ok(v) => ToolOutput::limited(v["text"].as_str().unwrap_or("").to_owned(), false),
            Err(e) => ToolOutput::err(e),
        }
    }
}

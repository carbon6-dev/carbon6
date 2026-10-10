//! MCP client integration: connects to configured MCP servers (stdio
//! processes on the worker, or streamable HTTP) and exposes their tools as
//! carbon6 tools named `mcp__<server>__<tool>`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use c6_llm::ToolSpec;
use c6_tools::{Concurrency, Tool, ToolCtx, ToolOutput};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(600);

/// A configured server, as stored on the head.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    #[serde(flatten)]
    pub transport: Transport,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum Transport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    Http {
        url: String,
    },
}

type Client = RunningService<RoleClient, ()>;

/// Connected servers for one session.
pub struct McpSet {
    clients: Vec<(String, Arc<Client>)>,
}

impl McpSet {
    /// Connects to every server; failures are logged and skipped so one bad
    /// server does not take down the session.
    pub async fn connect(servers: &[ServerConfig], cwd: &std::path::Path) -> (Self, Vec<String>) {
        let mut clients = Vec::new();
        let mut errors = Vec::new();
        for s in servers {
            match tokio::time::timeout(CONNECT_TIMEOUT, connect_one(s, cwd)).await {
                Ok(Ok(c)) => {
                    info!(server = %s.name, "MCP server connected");
                    clients.push((s.name.clone(), Arc::new(c)));
                }
                Ok(Err(e)) => errors.push(format!("{}: {e:#}", s.name)),
                Err(_) => errors.push(format!("{}: timed out connecting", s.name)),
            }
        }
        for e in &errors {
            warn!("MCP server unavailable: {e}");
        }
        (Self { clients }, errors)
    }

    /// The servers' tools, sorted for a stable prompt prefix.
    pub async fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for (server, client) in &self.clients {
            match client.list_all_tools().await {
                Ok(tools) => {
                    for t in tools {
                        let name = format!("mcp__{}__{}", sanitize(server), sanitize(&t.name));
                        let spec = ToolSpec {
                            name,
                            description: t.description.map(|d| d.into_owned()).unwrap_or_default(),
                            parameters: Value::Object((*t.input_schema).clone()),
                        };
                        let read_only = t.annotations.as_ref().and_then(|a| a.read_only_hint).unwrap_or(false);
                        out.push(Arc::new(McpTool { spec, remote_name: t.name.into_owned(), client: client.clone(), read_only }));
                    }
                }
                Err(e) => warn!(%server, "listing MCP tools failed: {e}"),
            }
        }
        out.sort_by(|a, b| a.spec().name.cmp(&b.spec().name));
        out
    }
}

async fn connect_one(s: &ServerConfig, cwd: &std::path::Path) -> anyhow::Result<Client> {
    match &s.transport {
        Transport::Stdio { command, args, env } => {
            if command.trim().is_empty() {
                bail!("empty command");
            }
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args).envs(env).current_dir(cwd);
            let transport = TokioChildProcess::new(cmd).with_context(|| format!("starting {command}"))?;
            Ok(().serve(transport).await.context("MCP handshake")?)
        }
        Transport::Http { url } => {
            c6_llm::install_crypto_provider();
            let transport = StreamableHttpClientTransport::from_uri(url.clone());
            Ok(().serve(transport).await.context("MCP handshake")?)
        }
    }
}

/// Tool names must be `[A-Za-z0-9_-]` for model APIs.
fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

struct McpTool {
    spec: ToolSpec,
    remote_name: String,
    client: Arc<Client>,
    read_only: bool,
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn concurrency(&self) -> Concurrency {
        if self.read_only { Concurrency::Shared } else { Concurrency::Exclusive }
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let mut params = CallToolRequestParams::new(self.remote_name.clone());
        params.arguments = match input {
            Value::Object(m) => Some(m),
            Value::Null => None,
            _ => return ToolOutput::err("input must be a JSON object"),
        };
        let result = tokio::select! {
            _ = ctx.cancel.cancelled() => return ToolOutput::err("interrupted"),
            r = tokio::time::timeout(CALL_TIMEOUT, self.client.call_tool(params)) => r,
        };
        match result {
            Err(_) => ToolOutput::err("MCP tool call timed out"),
            Ok(Err(e)) => ToolOutput::err(format!("MCP error: {e}")),
            Ok(Ok(r)) => {
                let mut parts: Vec<String> = r
                    .content
                    .iter()
                    .map(|c| match c {
                        ContentBlock::Text(t) => t.text.clone(),
                        ContentBlock::Image(i) => format!("[image: {}]", i.mime_type),
                        ContentBlock::Resource(r) => format!("[resource: {}]", serde_json::to_string(&r.resource).unwrap_or_default()),
                        ContentBlock::ResourceLink(l) => format!("[resource link: {}]", l.uri),
                        ContentBlock::Audio(_) => "[audio]".to_owned(),
                        _ => "[unsupported content]".to_owned(),
                    })
                    .collect();
                if parts.is_empty()
                    && let Some(s) = &r.structured_content
                {
                    parts.push(s.to_string());
                }
                ToolOutput::limited(parts.join("\n"), r.is_error.unwrap_or(false))
            }
        }
    }
}

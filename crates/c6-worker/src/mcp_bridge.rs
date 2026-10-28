//! carbon6's subagents for Claude Code sessions (docs/subagents.md §14).
//! Claude Code runs its own tools, so it never sees carbon6's `Task`. During
//! each of its turns the worker serves `Task` and `SendMessage` to it as a
//! local MCP server (streamable HTTP, JSON responses, one secret path per
//! turn), backed by the same subagent runner as native sessions: the
//! subagents run in carbon6's own loop, on the connection and model asked
//! for.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use c6_proto::{content_block, session_event};
use c6_tools::{SessionTools, Tool, ToolCtx};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::journal::Journal;

/// The name Claude Code knows the server by: its tools are
/// `mcp__carbon6__Task` and `mcp__carbon6__SendMessage`.
pub const SERVER: &str = "carbon6";

/// What one Claude Code turn may call.
pub(crate) struct Entry {
    pub session_id: String,
    /// The session's tool state, with its subagent runner set for the turn.
    pub session: Arc<SessionTools>,
    pub tools: Vec<Arc<dyn Tool>>,
    /// The turn's: stopping it stops the subagents it started.
    pub cancel: CancellationToken,
    pub journal: Arc<Journal>,
    /// Tool calls of the session already matched to an MCP call.
    pub matched: Mutex<HashSet<String>>,
}

type Entries = Arc<Mutex<HashMap<String, Arc<Entry>>>>;

/// The worker's MCP server for Claude Code turns (one, started on first use).
pub(crate) struct Bridge {
    port: u16,
    entries: Entries,
}

/// A turn's registration; dropping it ends the turn's access.
pub(crate) struct Registration {
    pub url: String,
    token: String,
    entries: Entries,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.entries.lock().unwrap().remove(&self.token);
    }
}

impl Bridge {
    pub async fn start() -> anyhow::Result<Self> {
        let entries: Entries = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let state = entries.clone();
        let app =
            axum::Router::new().route("/mcp/{token}", axum::routing::post(handle).get(no_stream).delete(end_session)).with_state(state);
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                warn!("the MCP bridge for Claude Code stopped: {e}");
            }
        });
        info!(port, "MCP bridge for Claude Code listening");
        Ok(Self { port, entries })
    }

    pub fn register(&self, entry: Entry) -> Registration {
        let token = format!("{:032x}", rand::random::<u128>());
        self.entries.lock().unwrap().insert(token.clone(), Arc::new(entry));
        Registration { url: format!("http://127.0.0.1:{}/mcp/{token}", self.port), token, entries: self.entries.clone() }
    }
}

impl Registration {
    /// Claude Code's `--mcp-config` for it.
    pub fn mcp_config(&self) -> String {
        json!({"mcpServers": {SERVER: {"type": "http", "url": self.url}}}).to_string()
    }
}

async fn no_stream() -> axum::http::StatusCode {
    axum::http::StatusCode::METHOD_NOT_ALLOWED
}

async fn end_session() -> axum::http::StatusCode {
    axum::http::StatusCode::OK
}

async fn handle(
    axum::extract::State(entries): axum::extract::State<Entries>,
    axum::extract::Path(token): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(entry) = entries.lock().unwrap().get(&token).cloned() else {
        return (axum::http::StatusCode::NOT_FOUND, "no such turn").into_response();
    };
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return axum::Json(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))).into_response(),
    };
    // Notifications and responses get no answer.
    let Some(id) = msg.get("id").cloned().filter(|_| msg.get("method").is_some()) else {
        return axum::http::StatusCode::ACCEPTED.into_response();
    };
    let method = msg["method"].as_str().unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let reply = match method {
        "initialize" => {
            let version = params["protocolVersion"].as_str().unwrap_or("2025-06-18");
            rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": SERVER, "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "carbon6 runs these subagents in its own loop, on any connection and model the session may use."
                }),
            )
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => {
            let tools: Vec<Value> = entry
                .tools
                .iter()
                .map(|t| {
                    let s = t.spec();
                    json!({"name": s.name, "description": s.description, "inputSchema": s.parameters})
                })
                .collect();
            rpc_result(id, json!({"tools": tools}))
        }
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default().to_owned();
            let input = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match entry.tools.iter().find(|t| t.spec().name == name).cloned() {
                None => rpc_error(id, -32602, &format!("no tool {name:?}")),
                Some(tool) => {
                    let call_id = call_id_for(&entry, &name, &input).await;
                    let ctx = ToolCtx {
                        session: entry.session.clone(),
                        cancel: entry.cancel.child_token(),
                        output: Arc::new(|_: &str| {}),
                        call_id,
                    };
                    let out = tool.call(&ctx, input).await;
                    rpc_result(id, json!({"content": [{"type": "text", "text": out.content}], "isError": out.is_error}))
                }
            }
        }
        _ => rpc_error(id, -32601, &format!("method {method:?} not found")),
    };
    axum::Json(reply).into_response()
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The id of Claude Code's tool call this MCP call answers, from the
/// session's log (so the subagent shows under that call): the earliest
/// unmatched call of `mcp__carbon6__<tool>` with these arguments. Claude
/// Code streams the call before it runs it, but the worker may not have
/// logged it yet, so it waits a little; else a fresh id.
async fn call_id_for(entry: &Entry, tool: &str, input: &Value) -> String {
    let full = format!("mcp__{SERVER}__{tool}");
    for _ in 0..20 {
        let events = entry.journal.events_after(&entry.session_id, 0, u32::MAX).await.unwrap_or_default();
        let found = events.iter().find_map(|e| match &e.kind {
            Some(session_event::Kind::Assistant(a)) => a.blocks.iter().find_map(|b| match &b.block {
                Some(content_block::Block::ToolUse(u))
                    if u.name == full
                        && serde_json::from_str::<Value>(&u.input_json).ok().as_ref() == Some(input)
                        && !entry.matched.lock().unwrap().contains(&u.id) =>
                {
                    Some(u.id.clone())
                }
                _ => None,
            }),
            _ => None,
        });
        if let Some(id) = found {
            entry.matched.lock().unwrap().insert(id.clone());
            return id;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    format!("mcp-{}", uuid::Uuid::now_v7())
}

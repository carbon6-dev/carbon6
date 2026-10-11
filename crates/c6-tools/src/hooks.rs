//! Hooks around tool calls and the end of a turn, provided by the runtime
//! (Claude Code-style `settings.json` hooks on the worker).

use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait Hooks: Send + Sync {
    /// Before a tool runs: `Some(reason)` blocks the call, and the model
    /// gets the reason instead of a result.
    async fn pre_tool_use(&self, tool: &str, input: &Value) -> Option<String>;

    /// After a tool ran: `Some(feedback)` is added to the result the model
    /// sees.
    async fn post_tool_use(&self, tool: &str, input: &Value, output: &str, is_error: bool) -> Option<String>;

    /// When the model would end its turn: `Some(reason)` makes it continue,
    /// with the reason as feedback. `active` is set when it is already
    /// continuing because of a Stop hook.
    async fn stop(&self, active: bool) -> Option<String>;
}

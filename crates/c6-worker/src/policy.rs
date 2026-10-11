//! Policies: label-scoped rules for tool calls. A matching call is denied,
//! or held until you approve it in the UI (the request and your answer are
//! session events).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

/// How long a call waits for an answer.
const WAIT: Duration = Duration::from_secs(3600);

#[derive(Deserialize)]
struct Def {
    name: String,
    #[serde(default)]
    applies_to: Vec<String>,
    #[serde(default)]
    tools: String,
    #[serde(default)]
    pattern: String,
    action: String,
    #[serde(default)]
    message: String,
    #[serde(default = "yes")]
    enabled: bool,
}

fn yes() -> bool {
    true
}

struct Rule {
    name: String,
    tools: String,
    pattern: Option<Regex>,
    deny: bool,
    message: String,
}

/// An approval request or outcome, for the session's log.
#[derive(Clone, Debug)]
pub struct ApprovalEvent {
    pub call_id: String,
    pub state: &'static str,
    pub tool: String,
    pub input: String,
    pub policy: String,
    pub message: String,
}

pub struct Policies {
    rules: Vec<Rule>,
    pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    log: mpsc::UnboundedSender<ApprovalEvent>,
}

impl Policies {
    /// The enabled policies in `json` that apply to a session with
    /// `labels`; `None` if there are none.
    pub fn new(json: &str, labels: &[String], log: mpsc::UnboundedSender<ApprovalEvent>) -> Option<Arc<Self>> {
        let rules: Vec<Rule> = serde_json::from_str::<Vec<Def>>(json)
            .unwrap_or_default()
            .into_iter()
            .filter(|d| d.enabled && c6_api::labels::applies(&d.applies_to, labels))
            .map(|d| Rule {
                name: d.name,
                tools: d.tools.trim().to_owned(),
                pattern: Some(d.pattern.trim()).filter(|p| !p.is_empty()).and_then(|p| Regex::new(p).ok()),
                deny: d.action == "deny",
                message: d.message,
            })
            .collect();
        (!rules.is_empty()).then(|| Arc::new(Self { rules, pending: Mutex::default(), log }))
    }

    /// Your answer for a waiting call; false if nothing waits for it.
    pub fn decide(&self, call_id: &str, allow: bool) -> bool {
        match self.pending.lock().unwrap().remove(call_id) {
            Some(tx) => tx.send(allow).is_ok(),
            None => false,
        }
    }

    fn rule_for(&self, tool: &str, input: &Value) -> Option<&Rule> {
        let text = input.to_string();
        self.rules
            .iter()
            .find(|r| (r.tools.is_empty() || crate::hooks::matches(&r.tools, tool)) && r.pattern.as_ref().is_none_or(|p| p.is_match(&text)))
    }
}

/// Clears a wait however it ends; one that ends unanswered is logged as
/// expired.
struct Waiting<'a> {
    policies: &'a Policies,
    event: ApprovalEvent,
    answered: bool,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.policies.pending.lock().unwrap().remove(&self.event.call_id);
        if !self.answered {
            let _ = self.policies.log.send(ApprovalEvent { state: "expired", ..self.event.clone() });
        }
    }
}

#[async_trait]
impl c6_tools::Hooks for Policies {
    async fn pre_tool_use(&self, call_id: &str, tool: &str, input: &Value) -> Option<String> {
        let rule = self.rule_for(tool, input)?;
        let why = if rule.message.is_empty() { String::new() } else { format!(": {}", rule.message) };
        if rule.deny {
            return Some(format!("Denied by policy \"{}\"{why}", rule.name));
        }
        let mut shown = input.to_string();
        if shown.len() > 4000 {
            let mut cut = 4000;
            while !shown.is_char_boundary(cut) {
                cut -= 1;
            }
            shown.truncate(cut);
            shown.push('…');
        }
        let event = ApprovalEvent {
            call_id: call_id.to_owned(),
            state: "requested",
            tool: tool.to_owned(),
            input: shown,
            policy: rule.name.clone(),
            message: rule.message.clone(),
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(call_id.to_owned(), tx);
        let _ = self.log.send(event.clone());
        let mut waiting = Waiting { policies: self, event, answered: false };
        match tokio::time::timeout(WAIT, rx).await {
            Ok(Ok(allow)) => {
                waiting.answered = true;
                (!allow).then(|| format!("Not run: you denied it (policy \"{}\"{why}). Ask the user how to proceed.", rule.name))
            }
            _ => Some(format!("Not run: it needs your approval (policy \"{}\"), and none came in time.", rule.name)),
        }
    }

    async fn post_tool_use(&self, _: &str, _: &Value, _: &str, _: bool) -> Option<String> {
        None
    }

    async fn stop(&self, _: bool) -> Option<String> {
        None
    }
}

/// Several hooks as one: the first block wins; feedback is joined.
pub struct Chain(pub Vec<Arc<dyn c6_tools::Hooks>>);

#[async_trait]
impl c6_tools::Hooks for Chain {
    async fn pre_tool_use(&self, call_id: &str, tool: &str, input: &Value) -> Option<String> {
        for h in &self.0 {
            if let Some(r) = h.pre_tool_use(call_id, tool, input).await {
                return Some(r);
            }
        }
        None
    }

    async fn post_tool_use(&self, tool: &str, input: &Value, output: &str, is_error: bool) -> Option<String> {
        let mut all = Vec::new();
        for h in &self.0 {
            all.extend(h.post_tool_use(tool, input, output, is_error).await);
        }
        (!all.is_empty()).then(|| all.join("\n"))
    }

    async fn stop(&self, active: bool) -> Option<String> {
        for h in &self.0 {
            if let Some(r) = h.stop(active).await {
                return Some(r);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use c6_tools::Hooks;
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn ask_deny_and_scope() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let json = json!([
            {"name": "no rm", "applies_to": ["env:prod"], "tools": "Bash", "pattern": "rm\\s+-rf", "action": "deny", "message": "never on prod"},
            {"name": "ask prod", "applies_to": ["env:prod"], "tools": "Bash|Edit|Write", "action": "ask"},
            {"name": "elsewhere", "applies_to": ["env:dev"], "action": "deny"}
        ])
        .to_string();
        assert!(Policies::new(&json, &["env:staging".into()], tx.clone()).is_none(), "none apply");
        let p = Policies::new(&json, &["env:prod".into()], tx).unwrap();
        // shell is Bash too.
        let d = p.pre_tool_use("c1", "shell", &json!({"command": "rm -rf /var/x"})).await;
        assert_eq!(d.as_deref(), Some("Denied by policy \"no rm\": never on prod"));
        assert_eq!(p.pre_tool_use("c2", "read", &json!({"path": "x"})).await, None, "reads are fine");

        // Ask: waits for the answer.
        let p2 = p.clone();
        let wait = tokio::spawn(async move { p2.pre_tool_use("c3", "bash", &json!({"command": "systemctl restart nginx"})).await });
        let req = rx.recv().await.unwrap();
        assert_eq!((req.call_id.as_str(), req.state, req.policy.as_str()), ("c3", "requested", "ask prod"));
        assert!(!wait.is_finished());
        assert!(p.decide("c3", true));
        assert_eq!(wait.await.unwrap(), None, "allowed: it runs");

        let p3 = p.clone();
        let wait = tokio::spawn(async move { p3.pre_tool_use("c4", "edit", &json!({"path": "/etc/x"})).await });
        rx.recv().await.unwrap();
        p.decide("c4", false);
        assert!(wait.await.unwrap().unwrap().starts_with("Not run: you denied it"));

        // Given up (an interrupt): logged as expired.
        let p4 = p.clone();
        let wait = tokio::spawn(async move { p4.pre_tool_use("c5", "write", &json!({})).await });
        rx.recv().await.unwrap();
        wait.abort();
        let _ = wait.await;
        assert_eq!(rx.recv().await.unwrap().state, "expired");
        assert!(!p.decide("c5", true), "nothing waits for it any more");
    }
}

//! Native capability reminders through the documented command-hook protocol.
//! No transcript inspection, CLI internals, network or model calls.
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use c6_api::project_contract::ProjectContract;
use c6_proto::CreateSession;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The process's private, lifetime-managed output root owns this state file.
pub const STATE_ENV: &str = "C6_CAPABILITY_HOOK_STATE";

#[derive(Serialize, Deserialize)]
pub struct Policy {
    pub capabilities: Vec<Triggers>,
    pub reminder: String,
}

#[derive(Serialize, Deserialize)]
pub struct Triggers {
    pub name: String,
    pub trigger_phrases: Vec<String>,
    pub command_patterns: Vec<String>,
}

impl Policy {
    fn matches(&self, text: &str, commands: bool) -> Vec<String> {
        let text = text.to_lowercase();
        self.capabilities
            .iter()
            .filter_map(|cap| {
                let mut patterns = cap.trigger_phrases.iter().chain(cap.command_patterns.iter().filter(|_| commands));
                patterns.any(|p| !p.trim().is_empty() && text.contains(&p.to_lowercase())).then(|| cap.name.clone())
            })
            .collect()
    }
}

/// Append independent entries; never replace the head's or other guards' hooks.
pub(crate) fn configure(settings: &mut Value, spec: &CreateSession, reminder: String) {
    let Some(contract) = spec.project_contract_json.as_deref().and_then(|s| serde_json::from_str::<ProjectContract>(s).ok()) else {
        return;
    };
    if contract.content.capabilities.is_empty() {
        return;
    }
    // Only capability definitions are needed by this local matcher. Do not put
    // the project's environment, commands, decisions or history in argv.
    let policy = Policy {
        capabilities: contract
            .content
            .capabilities
            .into_iter()
            .map(|item| Triggers {
                name: item.value.name,
                trigger_phrases: item.value.trigger_phrases,
                command_patterns: item.value.command_patterns,
            })
            .collect(),
        reminder,
    };
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "c6w".into());
    let exe = exe.strip_suffix(" (deleted)").unwrap_or(&exe);
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let command = format!("{} claude-capability {}", quote(exe), quote(&serde_json::to_string(&policy).expect("policy serializes")));
    if !settings.get("hooks").is_some_and(Value::is_object) {
        settings["hooks"] = json!({});
    }
    for event in ["PreToolUse", "UserPromptSubmit"] {
        let handler = json!({"type":"command","command":command,"timeout":10,"onFailure":"block"});
        let group = if event == "PreToolUse" { json!({"matcher":"*","hooks":[handler]}) } else { json!({"hooks":[handler]}) };
        match settings["hooks"][event].as_array_mut() {
            Some(groups) => groups.push(group),
            None => settings["hooks"][event] = json!([group]),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct TurnState {
    // Documented session/agent IDs separate native subagents from the parent.
    agents: HashMap<String, AgentTurn>,
}

#[derive(Default, Serialize, Deserialize)]
struct AgentTurn {
    prompt_id: Option<String>,
    seen: HashSet<String>,
}

/// CLI boundary kept here so the worker binary needs no JSON dependency.
pub fn command(policy: &str, state_path: &Path, input: &str) -> anyhow::Result<Option<String>> {
    handle(&serde_json::from_str(policy)?, state_path, input)?.map(|output| serde_json::to_string(&output).map_err(Into::into)).transpose()
}

/// None means no verdict, *not* approval. A separate lease or permission hook
/// still applies, including after this hook's once-per-turn reminder.
pub fn handle(policy: &Policy, state_path: &Path, input: &str) -> anyhow::Result<Option<Value>> {
    let input: Value = serde_json::from_str(input)?;
    let event = input["hook_event_name"].as_str().unwrap_or_default();
    if !matches!(event, "PreToolUse" | "UserPromptSubmit") {
        return Ok(None);
    }
    let tool = input["tool_name"].as_str().unwrap_or_default();
    if event == "PreToolUse" && matches!(tool, "mcp__carbon6__Capability" | "mcp__carbon6__capability" | "Capability" | "capability") {
        return Ok(None);
    }
    let names = if event == "UserPromptSubmit" {
        // Prompt reminders use trigger phrases, not shell-command fragments.
        policy.matches(input["prompt"].as_str().unwrap_or_default(), false)
    } else {
        fn strings(value: &Value, policy: &Policy, names: &mut Vec<String>) {
            match value {
                Value::String(text) => {
                    for name in policy.matches(text, true) {
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        strings(value, policy, names);
                    }
                }
                Value::Object(values) => {
                    for value in values.values() {
                        strings(value, policy, names);
                    }
                }
                _ => {}
            }
        }
        let mut names = policy.matches(tool, true);
        strings(&input["tool_input"], policy, &mut names);
        names
    };
    // File::lock is released on every exit, including a killed hook. Keep the
    // same inode while rewriting so parallel tool hooks cannot both claim it.
    let mut file = std::fs::File::options().read(true).write(true).create(true).truncate(false).open(state_path)?;
    file.lock()?;
    let mut stored = String::new();
    file.read_to_string(&mut stored)?;
    let mut state = if stored.is_empty() {
        TurnState::default()
    } else if event == "UserPromptSubmit" {
        // A new prompt can recover an interrupted state rewrite; a tool in the
        // same turn still fails closed rather than guessing it was reminded.
        serde_json::from_str(&stored).unwrap_or_default()
    } else {
        serde_json::from_str(&stored)?
    };
    let key = serde_json::to_string(&(input["session_id"].as_str().unwrap_or_default(), input["agent_id"].as_str().unwrap_or_default()))?;
    let agent = state.agents.entry(key).or_default();
    let prompt_id = input["prompt_id"].as_str().map(str::to_owned);
    if prompt_id.as_ref().is_some_and(|id| agent.prompt_id.as_ref() != Some(id)) || (event == "UserPromptSubmit" && prompt_id.is_none()) {
        agent.seen.clear();
        agent.prompt_id = prompt_id;
    }
    let fresh =
        if event == "UserPromptSubmit" { names } else { names.into_iter().filter(|name| agent.seen.insert(name.clone())).collect() };
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(&serde_json::to_vec(&state)?)?;
    if fresh.is_empty() {
        return Ok(None);
    }
    let reminder = policy.reminder.replace("{{capabilities}}", &fresh.join(", "));
    Ok(Some(if event == "PreToolUse" {
        json!({"hookSpecificOutput":{"hookEventName":event,"permissionDecision":"deny","permissionDecisionReason":reminder}})
    } else {
        json!({"hookSpecificOutput":{"hookEventName":event,"additionalContext":reminder}})
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        serde_json::from_value(json!({"capabilities":[{"name":"inspect widget",
            "trigger_phrases":["widget status"],"command_patterns":["widget-check"]}],
            "reminder":"Use capability {{capabilities}}."}))
        .unwrap()
    }

    #[test]
    fn tool_denial_is_once_per_turn_and_prompt_context_resets_it() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let p = policy();
        let tool = json!({"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"widget-check"}}).to_string();
        let first = handle(&p, &state, &tool).unwrap().unwrap();
        assert_eq!(first["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(first["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap().contains("inspect widget"));
        assert!(handle(&p, &state, &tool).unwrap().is_none());
        let prompt = json!({"hook_event_name":"UserPromptSubmit","prompt":"WIDGET STATUS please"}).to_string();
        let context = handle(&p, &state, &prompt).unwrap().unwrap();
        assert_eq!(context["hookSpecificOutput"]["additionalContext"], "Use capability inspect widget.");
        assert!(context["hookSpecificOutput"].get("permissionDecision").is_none());
        assert!(handle(&p, &state, &tool).unwrap().is_some());
    }

    #[test]
    fn unrelated_tools_and_the_capability_bridge_have_no_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        for (tool, input) in [("Bash", json!({"command":"other"})), ("mcp__carbon6__Capability", json!({"input":"widget status"}))] {
            assert!(
                handle(&policy(), &state, &json!({"hook_event_name":"PreToolUse","tool_name":tool,"tool_input":input}).to_string())
                    .unwrap()
                    .is_none()
            );
        }
        let fetch = json!({"hook_event_name":"PreToolUse","tool_name":"WebFetch","tool_input":{"prompt":"widget status"}});
        assert!(handle(&policy(), &state, &fetch.to_string()).unwrap().is_some());
    }

    #[test]
    fn raw_command_values_and_prompt_phrases_use_their_declared_triggers() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let mut policy = policy();
        policy.capabilities[0].command_patterns = vec!["widget-check \"ready\"".into()];
        let prompt = json!({"hook_event_name":"UserPromptSubmit","prompt":"widget-check \"ready\""});
        assert!(handle(&policy, &state, &prompt.to_string()).unwrap().is_none());
        let call = json!({"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"widget-check \"ready\""}});
        assert!(handle(&policy, &state, &call.to_string()).unwrap().is_some());
    }

    #[test]
    fn documented_prompt_ids_reset_deduplication_without_resetting_duplicate_submit_events() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let policy = policy();
        let mut call = json!({"hook_event_name":"PreToolUse","session_id":"widget","prompt_id":"prompt-a","tool_name":"Bash","tool_input":{"command":"widget-check"}});
        assert!(handle(&policy, &state, &call.to_string()).unwrap().is_some());
        let prompt = json!({"hook_event_name":"UserPromptSubmit","session_id":"widget","prompt_id":"prompt-a","prompt":"widget status"});
        assert!(handle(&policy, &state, &prompt.to_string()).unwrap().is_some());
        assert!(handle(&policy, &state, &call.to_string()).unwrap().is_none());
        call["prompt_id"] = json!("prompt-b");
        assert!(handle(&policy, &state, &call.to_string()).unwrap().is_some());
    }

    #[test]
    fn parallel_calls_claim_a_capability_once_and_native_agents_are_separate() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let policy = std::sync::Arc::new(policy());
        let call = json!({"hook_event_name":"PreToolUse","session_id":"widget","tool_name":"Bash","tool_input":{"command":"widget-check"}})
            .to_string();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (policy, state, call) = (policy.clone(), state.clone(), call.clone());
                std::thread::spawn(move || handle(&policy, &state, &call).unwrap().is_some())
            })
            .collect();
        assert_eq!(threads.into_iter().filter_map(|t| t.join().unwrap().then_some(())).count(), 1);
        let mut child: Value = serde_json::from_str(&call).unwrap();
        child["agent_id"] = json!("widget-child");
        assert!(handle(&policy, &state, &child.to_string()).unwrap().is_some());
        assert!(handle(&policy, &state, &call).unwrap().is_none());
    }

    #[test]
    fn configuration_preserves_other_hook_groups_and_has_no_contract_noop() {
        let mut settings = json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"other-guard"}]}]}});
        let original = settings.clone();
        configure(&mut settings, &CreateSession::default(), "reminder".into());
        assert_eq!(settings, original);
        let contract = json!({"version":1,"provenance":{"by":"you","at":1},"content":{
            "capabilities":[{"value":{"name":"inspect widget","description":"inspect","location":{"kind":"template","name":"inspect"},
                "inputs":{},"result_schema":{},"trigger_phrases":["widget status"],"command_patterns":["widget-check"]}}]}});
        let spec = CreateSession { project_contract_json: Some(contract.to_string()), ..Default::default() };
        configure(&mut settings, &spec, "reminder".into());
        assert_eq!(settings["hooks"]["PreToolUse"][0], original["hooks"]["PreToolUse"][0]);
        assert_eq!(settings["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(settings["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 1);
        let command = settings["hooks"]["PreToolUse"][1]["hooks"][0]["command"].as_str().unwrap();
        assert!(command.contains(" claude-capability "));
    }
}

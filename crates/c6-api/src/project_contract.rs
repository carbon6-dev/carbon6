//! Project-owned operating instructions. No project policy is built into the schema.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub by: String,
    pub at: crate::UnixMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_by: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ContractItem<T> {
    pub value: T,
    #[serde(default)]
    pub provenance: Provenance,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ValidationCommand {
    pub name: String,
    pub command: String,
    pub when_allowed: String,
    pub roles: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct EnvironmentVariable {
    pub name: String,
    /// A value or a secret reference, never a required credential.
    pub value: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct NamespaceHints {
    pub target: String,
    pub cache: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct BuildPolicy {
    #[serde(default)]
    pub validation: Vec<ContractItem<ValidationCommand>>,
    #[serde(default)]
    pub environment: Vec<ContractItem<EnvironmentVariable>>,
    #[serde(default)]
    pub namespaces: Vec<ContractItem<NamespaceHints>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityLocation {
    Template { name: String },
    Worker { worker: String, folder: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub name: String,
    pub description: String,
    pub location: CapabilityLocation,
    /// A JSON schema describing the accepted input object.
    pub inputs: Value,
    /// Schema for the capability's result details; status uses CapabilityStatus.
    pub result_schema: Value,
    #[serde(default)]
    pub trigger_phrases: Vec<String>,
    /// Literal command fragments (not regular expressions or shell code).
    #[serde(default)]
    pub command_patterns: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct IntegrationRules {
    pub branch: String,
    pub rebase: String,
    pub ordering: String,
    pub approval_required: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ContractContent {
    #[serde(default)]
    pub build_policy: BuildPolicy,
    #[serde(default)]
    pub capabilities: Vec<ContractItem<Capability>>,
    #[serde(default)]
    pub integration: Vec<ContractItem<IntegrationRules>>,
    #[serde(default)]
    pub architecture_rules: Vec<ContractItem<String>>,
    #[serde(default)]
    pub decisions: Vec<ContractItem<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ProjectContract {
    pub version: u64,
    pub provenance: Provenance,
    pub content: ContractContent,
}

impl ProjectContract {
    /// Deterministic, bounded by the author-provided list; no model or regex.
    pub fn reminders(&self, text: &str) -> Vec<String> {
        let text = text.to_lowercase();
        self.content
            .capabilities
            .iter()
            .filter_map(|item| {
                let cap = &item.value;
                cap.trigger_phrases
                    .iter()
                    .chain(&cap.command_patterns)
                    .any(|p| !p.trim().is_empty() && text.contains(&p.to_lowercase()))
                    .then(|| cap.name.clone())
            })
            .collect()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ContractSet {
    /// Zero creates the first version; stale edits are rejected.
    pub expected_version: u64,
    pub contract: ContractContent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityStatus {
    Queued,
    Running,
    InfrastructureFailed,
    ProductFailed,
    Skipped,
    Unknown,
    Succeeded,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CapabilityResult {
    pub status: CapabilityStatus,
    pub details: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn contract_round_trips_provenance_and_matches_only_declared_triggers() {
        let contract: ProjectContract = serde_json::from_value(json!({
            "version": 7, "provenance": {"by":"you", "at":123, "suggested_by":"session:widget"},
            "content":{"capabilities":[{"value":{"name":"inspect widget","description":"Inspect the widget",
                "location":{"kind":"template","name":"inspect"}, "inputs":{"type":"object"},
                "result_schema":{"type":"object"}, "trigger_phrases":["widget status"],
                "command_patterns":["widget-check"]}, "provenance":{"by":"you","at":123}}]}
        }))
        .unwrap();
        assert_eq!(contract, serde_json::from_str(&serde_json::to_string(&contract).unwrap()).unwrap());
        assert_eq!(contract.reminders("WIDGET STATUS"), ["inspect widget"]);
        assert_eq!(contract.reminders("run widget-check --all"), ["inspect widget"]);
        assert!(contract.reminders("unrelated task").is_empty());
        assert_eq!(contract.content.capabilities[0].provenance.by, "you");
    }
}

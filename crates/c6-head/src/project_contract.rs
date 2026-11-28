//! Accepted project policy and proposals. Only a human-origin request, or
//! accepting a decision card, writes a contract revision.
use c6_api::project_contract::*;
use c6_core::now_ms;
use serde_json::{Value, json};

use crate::realms::Caller;
use crate::{HeadState, bad_request, forbidden, not_found};

pub async fn history(st: &HeadState, id: &str, caller: &Caller) -> anyhow::Result<Vec<ProjectContract>> {
    let p = crate::projects::load(st, id).await?.filter(|p| caller.sees(&p.realm)).ok_or_else(|| not_found("no project"))?;
    Ok(p.fields.contract_history)
}

fn stamp<T: PartialEq>(items: &mut [ContractItem<T>], old: &[ContractItem<T>], provenance: &Provenance) {
    for item in items {
        item.provenance = old.iter().find(|o| o.value == item.value).map(|o| &o.provenance).unwrap_or(provenance).clone();
    }
}

fn revision(change: &ContractSet, previous: Option<&ProjectContract>, suggestion: Option<&str>) -> anyhow::Result<ProjectContract> {
    if previous.map_or(0, |c| c.version) != change.expected_version {
        return Err(bad_request("project contract changed; reload before saving"));
    }
    let mut names = std::collections::HashSet::new();
    for cap in &change.contract.capabilities {
        if cap.value.name.trim().is_empty() || !names.insert(&cap.value.name) {
            return Err(bad_request("capabilities need distinct, nonempty names"));
        }
        match &cap.value.location {
            CapabilityLocation::Template { name } if name.trim().is_empty() => {
                return Err(bad_request("a capability needs a template name"));
            }
            CapabilityLocation::Worker { worker, folder } if worker.trim().is_empty() || worker == "*" || folder.trim().is_empty() => {
                return Err(bad_request("a worker capability needs a worker and folder"));
            }
            _ => {}
        }
        if !cap.value.inputs.is_object() || !cap.value.result_schema.is_object() {
            return Err(bad_request("a capability needs a template and object input/result schemas"));
        }
        check_capability_schema(&cap.value.inputs)?;
        check_capability_schema(&cap.value.result_schema)?;
    }
    let provenance = Provenance { by: "you".into(), at: now_ms(), suggested_by: suggestion.map(str::to_owned) };
    let empty = ContractContent::default();
    let old = previous.map(|c| &c.content).unwrap_or(&empty);
    let mut content = change.contract.clone();
    stamp(&mut content.build_policy.validation, &old.build_policy.validation, &provenance);
    stamp(&mut content.build_policy.environment, &old.build_policy.environment, &provenance);
    stamp(&mut content.build_policy.namespaces, &old.build_policy.namespaces, &provenance);
    stamp(&mut content.capabilities, &old.capabilities, &provenance);
    stamp(&mut content.integration, &old.integration, &provenance);
    stamp(&mut content.architecture_rules, &old.architecture_rules, &provenance);
    stamp(&mut content.decisions, &old.decisions, &provenance);
    Ok(ProjectContract { version: change.expected_version + 1, provenance, content })
}

fn check_capability_schema(schema: &Value) -> anyhow::Result<()> {
    fn walk(schema: &Value, depth: usize) -> anyhow::Result<()> {
        if depth > 64 {
            return Err(bad_request("capability schema nests too deeply"));
        }
        if schema.is_boolean() {
            return Ok(());
        }
        let object = schema.as_object().ok_or_else(|| bad_request("a schema must be an object or boolean"))?;
        for (key, value) in object {
            let valid = match key.as_str() {
                "$ref" | "$schema" | "$id" | "title" | "description" | "pattern" => value.is_string(),
                "type" => {
                    let allowed = |v: &Value| {
                        v.as_str().is_some_and(|t| matches!(t, "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"))
                    };
                    allowed(value) || value.as_array().is_some_and(|v| !v.is_empty() && v.iter().all(allowed))
                }
                "properties" | "$defs" | "definitions" => {
                    if let Some(properties) = value.as_object() {
                        for child in properties.values() {
                            walk(child, depth + 1)?;
                        }
                        true
                    } else {
                        false
                    }
                }
                "items" | "additionalProperties" => {
                    walk(value, depth + 1)?;
                    true
                }
                "anyOf" | "oneOf" | "allOf" => {
                    if let Some(choices) = value.as_array().filter(|v| !v.is_empty()) {
                        for child in choices {
                            walk(child, depth + 1)?;
                        }
                        true
                    } else {
                        false
                    }
                }
                "required" => value.as_array().is_some_and(|v| v.iter().all(Value::is_string)),
                "enum" => value.as_array().is_some_and(|v| !v.is_empty()),
                "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" => value.is_number(),
                "minLength" | "maxLength" | "minItems" | "maxItems" => value.as_u64().is_some(),
                "const" | "default" | "examples" => true,
                _ => return Err(bad_request(format!("unsupported capability schema keyword {key:?}"))),
            };
            if !valid {
                return Err(bad_request(format!("invalid capability schema keyword {key:?}")));
            }
        }
        Ok(())
    }
    walk(schema, 0)?;
    crate::schema::check_schema(schema).map_err(bad_request)
}

async fn accept(st: &HeadState, id: &str, change: &ContractSet, suggestion: Option<&str>) -> anyhow::Result<ProjectContract> {
    let mut accepted = None;
    crate::projects::modify(st, id, "you", None, |fields| {
        let next = revision(change, fields.contract_history.last(), suggestion)?;
        fields.contract_history.push(next.clone());
        accepted = Some(next);
        Ok(true)
    })
    .await?;
    // The durable edit succeeds even when a worker is disconnected.
    if let Err(error) = crate::sessions::refresh_specs(st, None).await {
        tracing::warn!("refreshing project contract: {error:#}");
    }
    Ok(accepted.expect("the project modification accepted a revision"))
}

pub async fn set(st: &HeadState, id: &str, change: &ContractSet, caller: &Caller) -> anyhow::Result<ProjectContract> {
    history(st, id, caller).await?;
    if !caller.human_origin(st.auth.required) {
        return Err(forbidden("only the user may edit the project contract; agents propose a change"));
    }
    accept(st, id, change, None).await
}

/// Resolve work and quick-session membership, then inherit from ancestors.
/// Realm checks prevent a moved session from inheriting another realm's policy.
pub async fn for_session(st: &HeadState, session: &str) -> anyhow::Result<Option<(String, ProjectContract)>> {
    let Some(origin) = st.store.session(session).await? else { return Ok(None) };
    for_row(st, &origin).await
}

pub async fn for_row(st: &HeadState, origin: &crate::store::SessionRow) -> anyhow::Result<Option<(String, ProjectContract)>> {
    let mut current = Some(origin.clone());
    let mut seen = std::collections::HashSet::new();
    while let Some(row) = current {
        let id = row.id.clone();
        if !seen.insert(id.clone()) {
            break;
        }
        if row.realm != origin.realm {
            break;
        }
        let mut project = crate::projects::of_session(st, &id).await;
        if project.is_none() {
            for work in crate::work::holding(st, &format!("session:{id}")).await? {
                if let Some(pid) = crate::work::project_of(st, &work).await {
                    project = Some(pid);
                    break;
                }
            }
        }
        if let Some(pid) = project
            && let Some(p) = crate::projects::load(st, &pid).await?
            && p.realm == origin.realm
        {
            return Ok(p.fields.contract_history.last().cloned().map(|c| (pid, c)));
        }
        current = match row.parent_id {
            Some(parent) => st.store.session(&parent).await?,
            None => None,
        };
    }
    Ok(None)
}

pub async fn for_work(st: &HeadState, work: &str, realm: &str) -> anyhow::Result<Option<ProjectContract>> {
    let pid = match work.strip_prefix("project:") {
        Some(id) => Some(id.to_owned()),
        None => crate::work::project_of(st, work).await,
    };
    let Some(pid) = pid else { return Ok(None) };
    Ok(crate::projects::load(st, &pid).await?.filter(|p| p.realm == realm).and_then(|p| p.fields.contract_history.last().cloned()))
}

/// Failure evidence only suggests a clearer instruction; accepting the card
/// remains the only path from an observation to project policy.
pub async fn failure(st: &HeadState, session: &crate::store::SessionRow, p: &Value) -> anyhow::Result<Value> {
    let Some((pid, contract)) = for_row(st, session).await? else { return Ok(json!({"proposed":false})) };
    let requested = p["capabilities"].as_array().cloned().unwrap_or_default();
    let names: Vec<&str> = contract
        .content
        .capabilities
        .iter()
        .filter_map(|c| requested.iter().any(|v| v.as_str() == Some(c.value.name.as_str())).then_some(c.value.name.as_str()))
        .collect();
    if names.is_empty() {
        return Ok(json!({"proposed":false}));
    }
    let template = crate::prompts::get(st, "project.contract_failure", &session.realm).await;
    let note = c6_api::prompts::fill(&template, &[("capabilities", &names.join(", "))]);
    if contract.content.decisions.iter().any(|i| i.value == note) {
        return Ok(json!({"proposed":false}));
    }
    let mut content = contract.content;
    content.decisions.push(ContractItem { value: note, ..Default::default() });
    let change = ContractSet { expected_version: contract.version, contract: content };
    let caller = Caller::in_realm(&session.realm);
    let card = propose(st, &pid, &change, &caller, &format!("session:{}", session.id)).await?;
    Ok(json!({"proposed":true,"decision":card.id}))
}

pub async fn propose(st: &HeadState, id: &str, change: &ContractSet, caller: &Caller, source: &str) -> anyhow::Result<c6_api::Decision> {
    let previous = history(st, id, caller).await?;
    revision(change, previous.last(), Some(source))?;
    let p = crate::projects::load(st, id).await?.ok_or_else(|| not_found("no project"))?;
    let d = c6_api::DecisionIn {
        kind: "confirm".into(),
        title: "Update project contract?".into(),
        document: serde_json::to_string_pretty(&change.contract)?,
        context: json!({"project":id,"contract_change":change,"suggested_by":source}),
        source: source.into(),
        options: vec![
            c6_api::DecisionOption { label: "Accept".into(), ..Default::default() },
            c6_api::DecisionOption { label: "Keep current".into(), ..Default::default() },
        ],
        ..Default::default()
    };
    crate::decisions::open(st, &d, &p.realm, &format!("contract:{id}:{}", uuid::Uuid::now_v7())).await
}

pub async fn decided(st: &HeadState, d: &c6_api::Decision, option: Option<&str>) -> anyhow::Result<()> {
    if option != Some("Accept") {
        return Ok(());
    }
    let id = d.context["project"].as_str().ok_or_else(|| bad_request("missing proposal project"))?;
    let change: ContractSet = serde_json::from_value(d.context["contract_change"].clone())?;
    accept(st, id, &change, d.context["suggested_by"].as_str()).await?;
    Ok(())
}

pub async fn render(st: &HeadState, realm: &str, contract: &ProjectContract) -> String {
    let template = crate::prompts::get(st, "project.contract", realm).await;
    c6_api::prompts::fill(
        &template,
        &[("version", &contract.version.to_string()), ("contract", &serde_json::to_string_pretty(&contract.content).unwrap_or_default())],
    )
}

/// Starts only the declared procedure. Missing targets and transport failures
/// are infrastructure outcomes, never evidence that the product failed.
pub async fn capability(st: &HeadState, session: &crate::store::SessionRow, p: &Value) -> anyhow::Result<Value> {
    if let Some(run) = p["run_id"].as_str() {
        return capability_result(st, session, run).await;
    }
    let Some((pid, contract)) = for_session(st, &session.id).await? else {
        return Err(bad_request("this session has no project contract"));
    };
    if p["action"] == "list" || p["name"].as_str().is_none() {
        return Ok(json!({"version":contract.version,"capabilities":contract.content.capabilities}));
    }
    let name = p["name"].as_str().unwrap_or_default();
    let cap = contract.content.capabilities.iter().find(|c| c.value.name == name).ok_or_else(|| not_found("no such project capability"))?;
    let direct = match &cap.value.location {
        CapabilityLocation::Worker { worker, folder } => Some(c6_api::Template {
            name: "project-capability".into(),
            workers: vec![worker.clone()],
            cwd: folder.clone(),
            connection: session.connection_id.clone(),
            model: session.model.clone(),
            enabled: true,
            agents: true,
            ..Default::default()
        }),
        _ => None,
    };
    let template = match &cap.value.location {
        CapabilityLocation::Template { name } => name.as_str(),
        _ => "project-capability",
    };
    let input_value = p.get("input").cloned().unwrap_or(json!({}));
    crate::schema::validate(&cap.value.inputs, &input_value).map_err(|e| bad_request(e.join("; ")))?;
    let input = json!({"capability":name,"input":p.get("input").cloned().unwrap_or(json!({})),
        "result_schema":cap.value.result_schema,
        "result_statuses":["queued","running","infrastructure-failed","product-failed","skipped","unknown","succeeded"]});
    let run_id = uuid::Uuid::now_v7().to_string();
    let template_prompt = crate::prompts::get(st, "project.capability", &session.realm).await;
    let prompt = c6_api::prompts::fill(&template_prompt, &[("invocation", &input.to_string())]);
    let on = match &cap.value.location {
        CapabilityLocation::Worker { worker, .. } => worker.as_str(),
        _ => "",
    };
    let mut depth = 0;
    let mut ancestor = session.parent_id.clone();
    while let Some(id) = ancestor {
        depth += 1;
        if depth > 64 {
            return Err(bad_request("session ancestry exceeds the delegation limit"));
        }
        ancestor = st.store.session(&id).await?.and_then(|s| s.parent_id);
    }
    let request = crate::templates::SubagentStart { template, on, prompt: &prompt, description: name, call_id: &run_id, depth };
    let started = match direct {
        Some(ref template) => crate::templates::start_subagent_with(st, session, &request, template, "").await,
        None => crate::templates::start_subagent(st, session, &request).await,
    };
    match started {
        Ok((child, _)) => {
            sqlx::query("INSERT INTO project_capability_runs (id, parent_id, child_id, project_id, capability, contract, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
                .bind(&run_id).bind(&session.id).bind(&child.id).bind(&pid).bind(name)
                .bind(serde_json::to_string(&contract)?).bind(now_ms()).execute(&st.store.pool).await?;
            Ok(serde_json::to_value(CapabilityResult {
                status: CapabilityStatus::Queued,
                details: json!({"run_id":run_id,"sessions":[child.id],"errors":[]}),
            })?)
        }
        Err(error) => Ok(serde_json::to_value(CapabilityResult {
            status: CapabilityStatus::InfrastructureFailed,
            details: json!({"error":error.to_string()}),
        })?),
    }
}

async fn capability_result(st: &HeadState, parent: &crate::store::SessionRow, run: &str) -> anyhow::Result<Value> {
    type RunRow = (String, String, String, Option<String>, Option<i64>);
    let record: Option<RunRow> = sqlx::query_as(
        "SELECT child_id, capability, contract, result, end_seq FROM project_capability_runs WHERE id = ? AND parent_id = ?",
    )
    .bind(run)
    .bind(&parent.id)
    .fetch_optional(&st.store.pool)
    .await?;
    let (child_id, name, snapshot, saved, end_seq) = record.ok_or_else(|| not_found("no capability run owned by this session"))?;
    let child = st.store.session(&child_id).await?.filter(|s| s.realm == parent.realm).ok_or_else(|| not_found("no capability session"))?;
    if let Some(saved) = saved {
        return Ok(serde_json::from_str(&saved)?);
    }
    let contract: ProjectContract = serde_json::from_str(&snapshot)?;
    let cap =
        contract.content.capabilities.iter().find(|c| c.value.name == name).ok_or_else(|| bad_request("missing capability snapshot"))?;
    let events = st.store.effective_events(&child_id).await?;
    let boundary = events.iter().find_map(|event| match &event.kind {
        Some(c6_proto::session_event::Kind::TurnEnded(ended)) if end_seq.is_none_or(|seq| event.seq == seq as u64) => {
            Some((event.seq, ended.reason))
        }
        _ => None,
    });
    if let Some((seq, _)) = boundary {
        sqlx::query("UPDATE project_capability_runs SET end_seq = ? WHERE id = ? AND end_seq IS NULL")
            .bind(seq as i64)
            .bind(run)
            .execute(&st.store.pool)
            .await?;
    }
    let background: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM running_tasks WHERE session_id = ? AND counted = 1)")
        .bind(&child_id)
        .fetch_one(&st.store.pool)
        .await?;
    let result = if (child.status == "running" && boundary.is_none()) || background {
        CapabilityResult { status: CapabilityStatus::Running, details: json!({"run_id":run,"session":child_id}) }
    } else if boundary.is_none() && child.end_reason.is_empty() {
        CapabilityResult { status: CapabilityStatus::Queued, details: json!({"run_id":run,"session":child_id}) }
    } else if boundary.is_some_and(|(_, reason)| reason != c6_proto::TurnEndReason::Completed as i32)
        || (boundary.is_none() && child.end_reason != "completed")
    {
        CapabilityResult {
            status: CapabilityStatus::InfrastructureFailed,
            details: json!({"run_id":run,"reason":child.end_reason,"error":child.error}),
        }
    } else {
        let text = events
            .iter()
            .filter(|event| boundary.is_none_or(|(seq, _)| event.seq <= seq))
            .rev()
            .find_map(|event| match &event.kind {
                Some(c6_proto::session_event::Kind::Assistant(message)) => Some(
                    message
                        .blocks
                        .iter()
                        .filter_map(|block| match &block.block {
                            Some(c6_proto::content_block::Block::Text(text)) => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                ),
                _ => None,
            })
            .unwrap_or_default();
        parse_result(&text, &cap.value.result_schema)
    };
    let value = serde_json::to_value(&result)?;
    if !matches!(result.status, CapabilityStatus::Running | CapabilityStatus::Queued) {
        sqlx::query("UPDATE project_capability_runs SET result = ? WHERE id = ? AND result IS NULL")
            .bind(value.to_string())
            .bind(run)
            .execute(&st.store.pool)
            .await?;
        // Concurrent readers must observe the same first terminal result.
        let saved: String =
            sqlx::query_scalar("SELECT result FROM project_capability_runs WHERE id = ?").bind(run).fetch_one(&st.store.pool).await?;
        return Ok(serde_json::from_str(&saved)?);
    }
    Ok(value)
}

/// Capture results at the durable end hook, before later messages can change
/// the child's latest reply. Status reads recover an interrupted hook too.
pub async fn ended(st: &HeadState, child: &str) -> anyhow::Result<()> {
    let runs: Vec<(String, String)> =
        sqlx::query_as("SELECT id, parent_id FROM project_capability_runs WHERE child_id = ? AND result IS NULL")
            .bind(child)
            .fetch_all(&st.store.pool)
            .await?;
    for (run, parent) in runs {
        if let Some(parent) = st.store.session(&parent).await? {
            capability_result(st, &parent, &run).await?;
        }
    }
    Ok(())
}

fn parse_result(text: &str, schema: &Value) -> CapabilityResult {
    match serde_json::from_str::<CapabilityResult>(text) {
        Ok(result)
            if !matches!(result.status, CapabilityStatus::Queued | CapabilityStatus::Running)
                && crate::schema::validate(schema, &result.details).is_ok() =>
        {
            result
        }
        _ => CapabilityResult {
            status: CapabilityStatus::Unknown,
            details: json!({"error":"the capability ended without a valid terminal structured result","reply":text}),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_results_do_not_infer_success_from_completion() {
        let schema = json!({"type":"object","properties":{"observation":{"type":"string"}},"required":["observation"]});
        let good = json!({"status":"succeeded","details":{"observation":"widget ready"}});
        assert_eq!(parse_result(&good.to_string(), &schema).status, CapabilityStatus::Succeeded);
        for text in [
            "finished",
            "{}",
            r#"{"status":"succeeded","details":{}}"#,
            r#"{"status":"running","details":{"observation":"pending"}}"#,
            r#"{"status":"queued","details":{"observation":"pending"}}"#,
        ] {
            assert_eq!(parse_result(text, &schema).status, CapabilityStatus::Unknown);
        }
    }

    #[test]
    fn capability_schemas_reject_silent_validation_gaps() {
        assert!(check_capability_schema(&json!({"type":"object","properties":{}})).is_ok());
        for schema in [
            json!({"type":"object","properties":{},"required":"widget"}),
            json!({"type":"object","properties":{},"not":{"type":"object"}}),
            json!({"type":"object","properties":{"widget":{"$ref":"#/$defs/missing"}}}),
        ] {
            assert!(check_capability_schema(&schema).is_err());
        }
    }

    #[test]
    fn revisions_stamp_provenance_retain_unchanged_items_and_reject_stale_edits() {
        let mut change = ContractSet::default();
        change.contract.architecture_rules.push(ContractItem { value: "Keep interfaces small".into(), ..Default::default() });
        let first = revision(&change, None, Some("session:widget")).unwrap();
        assert_eq!(first.version, 1);
        assert_eq!(first.content.architecture_rules[0].provenance.suggested_by.as_deref(), Some("session:widget"));
        assert!(revision(&change, Some(&first), None).is_err());
        change.expected_version = 1;
        change.contract.decisions.push(ContractItem { value: "Keep the existing interface".into(), ..Default::default() });
        let second = revision(&change, Some(&first), None).unwrap();
        assert_eq!(second.version, 2);
        assert_eq!(second.content.architecture_rules, first.content.architecture_rules);
        assert_eq!(second.content.decisions[0].provenance.by, "you");
        assert!(second.content.decisions[0].provenance.suggested_by.is_none());
    }
}

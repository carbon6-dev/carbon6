//! Operations (docs/apps.md §7): how anyone but an app changes what it owns.
//! An app's package declares them, with typed input:
//!
//! - **write:** a record update (or a new record, or a deletion) mapped from
//!   the input, run at once in the head and checked against the schema; it
//!   touches only the fields it names, and can require a condition on the
//!   record first.
//! - **template:** one of the app's templates, started as the app with the
//!   input as `{{input}}`, for changes that need judgment.
//! - **kit:** a kit tool, run at once with arguments mapped from the input
//!   (docs/knowledge.md §6.2).
//!
//! Callers: you and the realm's tokens (the API), agents (`app_call`),
//! triggers (the `operation` action) and views' actions; another realm
//! through a `call` grant on `op:<instance>.<name>`.

use c6_api as api;
use serde_json::{Value, json};

use crate::{HeadState, bad_request, not_found};

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// What is wrong with a package's operations.
pub fn check(p: &api::AppPackage) -> Vec<String> {
    let mut out = Vec::new();
    let mut names = std::collections::HashSet::new();
    let templates: Vec<&str> = p.contents.lists.get("templates").into_iter().flatten().filter_map(|t| t["name"].as_str()).collect();
    for op in &p.operations {
        let n = op.name.as_str();
        if !valid_name(n) {
            out.push(format!("operation {n:?}: a name is letters, digits, - and _"));
        }
        if !names.insert(n) {
            out.push(format!("two operations are named {n}"));
        }
        let kinds = [op.write.is_some(), !op.template.is_empty(), op.kit.is_some()].iter().filter(|x| **x).count();
        if kinds != 1 {
            out.push(format!("operation {n}: exactly one of write, template and kit"));
        }
        if !op.input.is_null() && (op.input["type"] != "object" || crate::schema::check_schema(&op.input).is_err()) {
            out.push(format!("operation {n}: its input is a JSON Schema of an object"));
        }
        if let Some(w) = &op.write
            && !p.contents.collections.iter().any(|c| c.name == w.collection)
        {
            out.push(format!("operation {n} writes {}, which the package does not own", w.collection));
        }
        if !op.template.is_empty() && !templates.contains(&op.template.as_str()) {
            out.push(format!("operation {n} starts {}, which is not among the package's templates", op.template));
        }
        if op.kit.is_some() {
            out.push(format!("operation {n}: kit operations are not built yet"));
        }
        if !matches!(op.stability.as_str(), "" | "published" | "internal") {
            out.push(format!("operation {n}: stability is published or internal"));
        }
    }
    out
}

/// An instance's operations, with its settings filled in.
pub async fn list(st: &HeadState, realm: &str, instance: &str) -> anyhow::Result<Vec<api::Operation>> {
    let i = crate::instances::get(&st.store.pool, realm, instance)
        .await?
        .ok_or_else(|| not_found(format!("no app {instance} in realm {realm}")))?;
    Ok(i.package
        .operations
        .iter()
        .filter_map(|o| serde_json::from_value(crate::instances::fill_settings(&json!(o), &i.settings)).ok())
        .collect())
}

/// The operations that change a collection (for its forms in the UI).
pub async fn for_collection(st: &HeadState, realm: &str, owner: &str, collection: &str) -> anyhow::Result<Vec<api::Operation>> {
    Ok(list(st, realm, owner)
        .await?
        .into_iter()
        .filter(|o| o.write.as_ref().is_some_and(|w| w.collection == collection) || o.write.is_none())
        .collect())
}

/// Calls an operation of an instance as `by` (who asks: you, a token, a
/// session, a trigger). The work is done as the instance.
pub async fn call(st: &HeadState, realm: &str, instance: &str, name: &str, input: &Value, by: &str) -> anyhow::Result<Value> {
    let op = list(st, realm, instance)
        .await?
        .into_iter()
        .find(|o| o.name == name)
        .ok_or_else(|| not_found(format!("the app {instance} has no operation {name:?}")))?;
    let input = if input.is_null() { json!({}) } else { input.clone() };
    if !op.input.is_null()
        && let Err(problems) = crate::schema::validate(&op.input, &input)
    {
        return Err(bad_request(format!("{instance}.{name}: the input does not match: {}", problems.join("; "))));
    }
    // The app does the work; who asked is kept with it.
    let acting = format!("app:{instance} op:{name} {by}");
    if let Some(w) = &op.write {
        return write(st, realm, w, &input, &acting).await;
    }
    if !op.template.is_empty() {
        let text = serde_json::to_string_pretty(&input)?;
        let run = crate::templates::run_as(st, &op.template, &text, &[format!("op:{instance}.{name}")], realm, Some(instance)).await?;
        return Ok(json!({"run": run}));
    }
    Err(bad_request(format!("{instance}.{name}: kit operations are not built yet")))
}

async fn write(st: &HeadState, realm: &str, w: &api::OperationWrite, input: &Value, by: &str) -> anyhow::Result<Value> {
    let fields = crate::triggers::render(&Value::Object(w.fields.clone()), input);
    let id = match crate::triggers::render(&w.id, input) {
        Value::String(s) if !s.is_empty() => Some(s),
        Value::Null => None,
        v => return Err(bad_request(format!("the record to change is not an id: {v}"))),
    };
    let Some(id) = id else {
        if w.delete {
            return Err(bad_request("a deletion names the record"));
        }
        let rec = api::RecordIn { fields, links: None, labels: None, version: None };
        return Ok(json!({"record": crate::collections::put(st, realm, &w.collection, None, &rec, by).await?}));
    };
    let cur = crate::collections::record(st, realm, &w.collection, &id).await?;
    if !w.when.is_empty() {
        let when = crate::triggers::render(&Value::Object(w.when.clone()), input);
        let mut filter = when.as_object().cloned().unwrap_or_default();
        filter.insert("id".into(), json!(id));
        let q = api::RecordQuery { filter, ..Default::default() };
        if crate::collections::query(st, realm, &w.collection, &q).await?.total == 0 {
            return Err(bad_request(format!("record {id} is not in a state this operation applies to ({when})")));
        }
    }
    if w.delete {
        crate::collections::delete(st, realm, &w.collection, &id, by).await?;
        return Ok(json!({"deleted": id}));
    }
    let mut merged = cur.fields.as_object().cloned().unwrap_or_default();
    merged.extend(fields.as_object().cloned().unwrap_or_default());
    let rec = api::RecordIn { fields: Value::Object(merged), links: None, labels: None, version: Some(cur.version) };
    Ok(json!({"record": crate::collections::put(st, realm, &w.collection, Some(&id), &rec, by).await?}))
}

/// An agent's `app_call`: an operation of an instance in its session's
/// realm, or in another realm through a `call` grant on
/// `op:<instance>.<name>`.
pub async fn for_session(st: &HeadState, session_id: &str, p: &Value) -> anyhow::Result<Value> {
    let own = crate::realms::of_session(st, session_id).await?.unwrap_or_else(|| crate::realms::HOME.to_owned());
    let realm = p["realm"].as_str().filter(|r| !r.is_empty()).unwrap_or(&own).to_owned();
    let (instance, name) = (p["instance"].as_str().unwrap_or_default(), p["operation"].as_str().unwrap_or_default());
    if instance.is_empty() && name.is_empty() {
        // What there is to call here.
        let mut out = Vec::new();
        for i in crate::instances::list(&st.store.pool, Some(&realm)).await? {
            for o in list(st, &realm, &i.name).await? {
                out.push(json!({"instance": i.name, "operation": o.name, "description": o.description, "input": o.input}));
            }
        }
        return Ok(json!(out));
    }
    let by = format!("session:{session_id}");
    if realm != own && !crate::grants::may_call(st, &realm, &own, &format!("op:{instance}.{name}"), &by).await {
        return Err(bad_request(format!("realm {own} may not call {realm}'s {instance}.{name}; no grant allows it")));
    }
    call(st, &realm, instance, name, &p["input"], &by).await
}

//! Views (docs/os.md §4.5): configured screens of a realm (a table or
//! board of records, a record, a chart of a series, a feed of decisions
//! or sessions, a page embedding others), kept as the `views` system list
//! and rendered by the UI. Their actions run here.

use c6_api as api;
use serde_json::{Value, json};

use crate::{HeadState, bad_request, not_found};

pub const KEY: &str = "views";
pub const KINDS: [&str; 6] = ["table", "board", "record", "chart", "feed", "page"];

pub async fn views(st: &HeadState) -> anyhow::Result<Vec<api::View>> {
    Ok(st.store.setting(KEY).await?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
}

/// The views a realm shows: root's and its own.
pub async fn effective(st: &HeadState, realm: &str) -> anyhow::Result<Vec<api::View>> {
    Ok(st.store.setting_for(KEY, realm).await?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
}

pub fn check_views(views: &[api::View]) -> anyhow::Result<()> {
    let mut names = std::collections::HashSet::new();
    for v in views {
        let n = v.name.as_str();
        if n.is_empty() || n.len() > 64 || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(bad_request(format!("{n:?}: a view's name is letters, digits, - and _")));
        }
        if !names.insert(n) {
            return Err(bad_request(format!("two views are named {n}")));
        }
        if !KINDS.contains(&v.kind.as_str()) {
            return Err(bad_request(format!("{n}: a view's kind is {}", KINDS.join(", "))));
        }
        let s = &v.source;
        let ok = match v.kind.as_str() {
            "table" | "board" | "record" => s["collection"].as_str().is_some_and(|c| !c.is_empty()),
            "chart" => s["series"].as_str().is_some_and(|c| !c.is_empty()),
            "feed" => s.get("decisions").is_some() || s.get("sessions").is_some(),
            _ => true,
        };
        if !ok {
            return Err(bad_request(format!(
                "{n}: a {} view's source is {}",
                v.kind,
                match v.kind.as_str() {
                    "chart" => "{\"series\": name, \"agg\"?, \"minutes\"?, \"bucket\"?, \"group_by\"?}",
                    "feed" => "{\"decisions\": \"open\"} or {\"sessions\": {\"labels\": [...]}}",
                    _ => "{\"collection\": name, \"where\"?, \"sort\"?, \"limit\"?}",
                }
            )));
        }
        if v.kind == "board" && v.group_by.is_empty() {
            return Err(bad_request(format!("{n}: a board names the field its columns are (group_by)")));
        }
        if let Some(a) = v.actions.iter().find(|a| a.label.trim().is_empty() || (a.start.is_empty() && a.set.is_empty())) {
            return Err(bad_request(format!("{n}: an action has a label, and starts a template or sets fields ({:?})", a.label)));
        }
    }
    Ok(())
}

/// Runs one of a view's actions on a record: sets its fields, then starts
/// the template with the record as {{input}}, its sessions labelled
/// record:<id>.
pub async fn act(st: &HeadState, realm: &str, name: &str, a: &api::ViewAct, by: &str) -> anyhow::Result<Value> {
    // A view an app brings acts as the app (docs/apps.md §6).
    let (view, layer) = crate::lists::effective_tagged(&st.store.pool, KEY, realm, None)
        .await?
        .into_iter()
        .filter_map(|(v, l)| serde_json::from_value::<api::View>(v).ok().map(|v| (v, l)))
        .find(|(v, _)| v.name == name)
        .ok_or_else(|| not_found(format!("no view {name}")))?;
    let app = match &layer {
        crate::lists::Layer::Instance(n) => Some(n.clone()),
        _ => None,
    };
    let by = &match &app {
        Some(n) => format!("app:{n} {by}"),
        None => by.to_owned(),
    };
    let action =
        view.actions.iter().find(|x| x.label == a.action).ok_or_else(|| bad_request(format!("{name} has no action {:?}", a.action)))?;
    let collection = view.source["collection"].as_str().ok_or_else(|| bad_request("actions are on records"))?;
    let mut rec = crate::collections::record(st, realm, collection, &a.record).await?;
    if !action.set.is_empty() {
        let mut fields = rec.fields.as_object().cloned().unwrap_or_default();
        fields.extend(action.set.clone());
        let r = api::RecordIn { fields: Value::Object(fields), links: None, labels: None, version: Some(rec.version) };
        rec = crate::collections::put(st, realm, collection, Some(&rec.id), &r, by).await?;
    }
    let mut out = json!({"record": rec});
    if !action.start.is_empty() {
        let input =
            serde_json::to_string_pretty(&json!({"collection": collection, "id": rec.id, "fields": rec.fields, "links": rec.links}))?;
        let run = crate::templates::run_as(st, &action.start, &input, &[format!("record:{}", rec.id)], realm, app.as_deref()).await?;
        out["run"] = json!(run);
    }
    Ok(out)
}

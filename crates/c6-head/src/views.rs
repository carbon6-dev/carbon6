//! Views (docs/os.md §4.5): configured screens of a realm (a table or
//! board of records, a record, a chart of a series, a feed of decisions
//! or sessions, a page embedding others), kept as the `views` system list
//! and rendered by the UI. Their actions run here.
//!
//! Items of a shared kind (docs/knowledge.md §6.2, #7) have views of their
//! own: `messages`, `calendar` and `gallery`. An app binds one to its items
//! and its operations; the UI that draws them is carbon6's.

use c6_api as api;
use serde_json::{Value, json};

use crate::{HeadState, bad_request, not_found};

pub const KEY: &str = "views";
pub const KINDS: [&str; 9] = ["table", "board", "record", "chart", "feed", "page", "messages", "calendar", "gallery"];
/// The kinds that show items, and the item kinds each is for.
pub const ITEM_VIEWS: [(&str, &[&str]); 3] = [("messages", &["email"]), ("calendar", &["event"]), ("gallery", &["photo", "video"])];

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
            k => match ITEM_VIEWS.iter().find(|(v, _)| *v == k) {
                Some((_, kinds)) => s["items"].as_str().is_some_and(|i| kinds.contains(&i)),
                None => true,
            },
        };
        if !ok {
            return Err(bad_request(format!(
                "{n}: a {} view's source is {}",
                v.kind,
                match v.kind.as_str() {
                    "chart" => "{\"series\": name, \"agg\"?, \"minutes\"?, \"bucket\"?, \"group_by\"?}",
                    "feed" => "{\"decisions\": \"open\"} or {\"sessions\": {\"labels\": [...]}}",
                    "messages" => "{\"items\": \"email\", \"owner\"?: app}",
                    "calendar" => "{\"items\": \"event\", \"owner\"?: app}",
                    "gallery" => "{\"items\": \"photo\" or \"video\", \"owner\"?: app}",
                    _ => "{\"collection\": name, \"where\"?, \"sort\"?, \"limit\"?}",
                }
            )));
        }
        if v.kind == "board" && v.group_by.is_empty() {
            return Err(bad_request(format!("{n}: a board names the field its columns are (group_by)")));
        }
        if let Some(a) =
            v.actions.iter().find(|a| a.label.trim().is_empty() || (a.start.is_empty() && a.set.is_empty() && a.operation.is_empty()))
        {
            return Err(bad_request(format!(
                "{n}: an action has a label, and starts a template, sets fields or calls an operation ({:?})",
                a.label
            )));
        }
    }
    Ok(())
}

/// A view of the realm, and the app that brings it (if one does).
async fn view_of(st: &HeadState, realm: &str, name: &str) -> anyhow::Result<(api::View, Option<String>)> {
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
    Ok((view, app))
}

/// Whose items an items view shows: its `owner`, else the app that brings
/// it, else anyone's ("").
fn owner_of(view: &api::View, app: Option<&str>) -> String {
    view.source["owner"].as_str().filter(|o| !o.is_empty()).or(app).unwrap_or_default().to_owned()
}

/// What an items view shows: items of its kind (newest first, or from
/// `from` on, oldest first, for a calendar).
pub async fn items(st: &HeadState, realm: &str, name: &str, q: &api::ViewItemsQuery) -> anyhow::Result<Vec<api::ViewItem>> {
    let (view, app) = view_of(st, realm, name).await?;
    let kind = view.source["items"].as_str().filter(|_| ITEM_VIEWS.iter().any(|(k, _)| *k == view.kind));
    let kind = kind.ok_or_else(|| bad_request(format!("{name} does not show items")))?;
    let kinds = ITEM_VIEWS.iter().find(|(k, _)| *k == view.kind).map(|(_, ks)| *ks).unwrap_or_default();
    let kind = match q.kind.as_str() {
        "" => kind.to_owned(),
        k if kinds.contains(&k) => k.to_owned(),
        k => return Err(bad_request(format!("a {} view shows {}, not {k}", view.kind, kinds.join(" and ")))),
    };
    let owner = owner_of(&view, app.as_deref());
    let params = json!({
        "kind": kind,
        "source": if owner.is_empty() { String::new() } else { format!("app:{owner}") },
        "from": q.from, "to": q.to.or(q.before), "limit": q.limit.unwrap_or(100),
        "order": if q.from.is_some() { "asc" } else { "desc" },
    });
    Ok(serde_json::from_value(crate::memex::call_in(st, realm, "memex.items.list", params).await?)?)
}

/// An item's picture, in an items view.
pub async fn thumb(st: &HeadState, realm: &str, name: &str, id: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    let (view, _) = view_of(st, realm, name).await?;
    if !ITEM_VIEWS.iter().any(|(k, _)| *k == view.kind) {
        return Err(bad_request(format!("{name} does not show items")));
    }
    let v = crate::memex::call_in(st, realm, "memex.items.thumb", json!({"id": id})).await?;
    Ok(base64::engine::general_purpose::STANDARD.decode(v["data"].as_str().unwrap_or_default())?)
}

/// Runs one of a view's actions on a record (sets its fields, then starts
/// the template with the record as {{input}}, its sessions labelled
/// record:<id>), or on an item of an items view (calls the owner's
/// operation, or starts the template with the item).
pub async fn act(st: &HeadState, realm: &str, name: &str, a: &api::ViewAct, by: &str) -> anyhow::Result<Value> {
    // A view an app brings acts as the app (docs/apps.md §6).
    let (view, app) = view_of(st, realm, name).await?;
    if !a.item.is_empty() {
        return act_on_item(st, realm, &view, app.as_deref(), a, by).await;
    }
    let by = &match &app {
        Some(n) => format!("app:{n} {by}"),
        None => by.to_owned(),
    };
    let action =
        view.actions.iter().find(|x| x.label == a.action).ok_or_else(|| bad_request(format!("{name} has no action {:?}", a.action)))?;
    let collection = view.source["collection"].as_str().ok_or_else(|| bad_request("actions are on records"))?;
    let mut rec = crate::collections::record(st, realm, collection, &a.record).await?;
    // An operation of the app that owns the collection (docs/apps.md §7):
    // `<app>.<operation>`, or the operation's name alone.
    if !action.operation.is_empty() {
        let owner = crate::collections::get(st, realm, collection).await?.owner;
        let (inst, op) = match action.operation.split_once('.') {
            Some((i, o)) => (i.to_owned(), o.to_owned()),
            None => (owner.clone(), action.operation.clone()),
        };
        let input = crate::triggers::render(&Value::Object(action.input.clone()), &json!({"id": rec.id, "fields": rec.fields}));
        let out = crate::operations::call(st, realm, &inst, &op, &input, by).await?;
        if let Ok(r) = crate::collections::record(st, realm, collection, &rec.id).await {
            rec = r;
        }
        let mut v = json!({"record": rec, "operation": out});
        if let Some(run) = out_run(&v) {
            v["run"] = run;
        }
        return Ok(v);
    }
    if !action.set.is_empty() {
        let mut fields = rec.fields.as_object().cloned().unwrap_or_default();
        fields.extend(action.set.clone());
        let r = api::RecordIn { fields: Value::Object(fields), version: Some(rec.version), ..Default::default() };
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

async fn act_on_item(
    st: &HeadState,
    realm: &str,
    view: &api::View,
    app: Option<&str>,
    a: &api::ViewAct,
    by: &str,
) -> anyhow::Result<Value> {
    let by = &match app {
        Some(n) => format!("app:{n} {by}"),
        None => by.to_owned(),
    };
    let action = view
        .actions
        .iter()
        .find(|x| x.label == a.action)
        .ok_or_else(|| bad_request(format!("{} has no action {:?}", view.name, a.action)))?;
    let got = crate::memex::call_in(st, realm, "memex.get", json!({"id": a.item})).await?;
    let item: api::MemexItem = serde_json::from_value(got["item"].clone()).map_err(|_| not_found(format!("no item {}", a.item)))?;
    if !view.source["items"]
        .as_str()
        .is_some_and(|k| k == item.kind || (view.kind == "gallery" && matches!(item.kind.as_str(), "photo" | "video")))
    {
        return Err(bad_request(format!("{} is not an item of this view", a.item)));
    }
    let data = json!({"id": item.id, "kind": item.kind, "title": item.title, "meta": item.meta, "source": item.source});
    if !action.operation.is_empty() {
        // The operation of the app whose item it is: `<app>.<operation>`, or
        // the operation's name alone (the view's owner, else the item's).
        let owner = owner_of(view, app);
        let owner = if owner.is_empty() { item.source.strip_prefix("app:").unwrap_or_default().to_owned() } else { owner };
        let (inst, op) = match action.operation.split_once('.') {
            Some((i, o)) => (i.to_owned(), o.to_owned()),
            None => (owner, action.operation.clone()),
        };
        if inst.is_empty() {
            return Err(bad_request(format!("{}: no app owns {}; name it (app.operation)", action.label, a.item)));
        }
        let input = crate::triggers::render(&Value::Object(action.input.clone()), &data);
        let out = crate::operations::call(st, realm, &inst, &op, &input, by).await?;
        let mut v = json!({"item": item.id, "operation": out});
        if let Some(run) = out_run(&v) {
            v["run"] = run;
        }
        return Ok(v);
    }
    if action.start.is_empty() {
        return Err(bad_request("an action on items calls an operation or starts a template"));
    }
    let run =
        crate::templates::run_as(st, &action.start, &serde_json::to_string_pretty(&data)?, &[format!("item:{}", item.id)], realm, app)
            .await?;
    Ok(json!({"item": item.id, "run": run}))
}

fn out_run(v: &Value) -> Option<Value> {
    v["operation"].get("run").cloned()
}

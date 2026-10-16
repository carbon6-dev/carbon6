//! App instances (docs/apps.md §5): a package version installed in a realm
//! under a name. An instance's code is its package's, read from the
//! registry by digest, so nothing can change it but an upgrade, a
//! downgrade or a switch to a fork. What it offers (skills, templates,
//! views, …) joins the settings lists its realm's sessions use; its own
//! sessions use its entries over the realm's. Its settings' values fill
//! `{{settings.<name>}}` in its code.

use c6_api as api;
use serde_json::{Value, json};
use sqlx::Row;

/// An installed app or library in a realm.
#[derive(Clone, Debug)]
pub struct Instance {
    pub id: i64,
    pub realm: String,
    pub name: String,
    pub package: api::AppPackage,
    /// The values of its package's settings, defaults filled in.
    pub settings: Value,
    pub state: String,
}

impl Instance {
    /// Its entries of a settings list, with its settings filled in.
    pub fn entries(&self, key: &str) -> Vec<Value> {
        self.package.contents.lists.get(key).into_iter().flatten().map(|e| fill_settings(e, &self.settings)).collect()
    }

    /// The identity its sessions and writes act as.
    pub fn principal(&self) -> String {
        format!("app:{}", self.name)
    }
}

/// The instances in a realm (or every realm), in the order they were
/// installed. Instances still waiting for your decision to install are
/// not there yet.
pub async fn list(pool: &sqlx::SqlitePool, realm: Option<&str>) -> anyhow::Result<Vec<Instance>> {
    let rows = sqlx::query(
        "SELECT i.id, i.realm, i.name, i.state, i.settings, p.manifest FROM installs i JOIN packages p ON p.digest = i.digest \
         WHERE i.realm != '' AND i.state != 'installing' AND (?1 IS NULL OR i.realm = ?1) ORDER BY i.id",
    )
    .bind(realm)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let package: api::AppPackage = serde_json::from_str(&r.get::<String, _>("manifest")).ok()?;
            let values: Value = serde_json::from_str(&r.get::<String, _>("settings")).unwrap_or(json!({}));
            Some(Instance {
                id: r.get("id"),
                realm: r.get("realm"),
                name: r.get("name"),
                settings: with_defaults(&package.settings, &values),
                package,
                state: r.get("state"),
            })
        })
        .collect())
}

pub async fn get(pool: &sqlx::SqlitePool, realm: &str, name: &str) -> anyhow::Result<Option<Instance>> {
    Ok(list(pool, Some(realm)).await?.into_iter().find(|i| i.name == name))
}

/// The instance a session acts as, if any.
pub async fn of_session(pool: &sqlx::SqlitePool, session_id: &str) -> anyhow::Result<Option<Instance>> {
    let Some(r) = sqlx::query("SELECT realm, instance FROM sessions WHERE id = ?").bind(session_id).fetch_optional(pool).await? else {
        return Ok(None);
    };
    let instance: String = r.get("instance");
    if instance.is_empty() {
        return Ok(None);
    }
    get(pool, &r.get::<String, _>("realm"), &instance).await
}

/// A settings schema's defaults, with the values given over them.
pub fn with_defaults(schema: &Value, values: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for (k, p) in schema["properties"].as_object().into_iter().flatten() {
        if let Some(d) = p.get("default") {
            out.insert(k.clone(), d.clone());
        }
    }
    for (k, v) in values.as_object().into_iter().flatten() {
        out.insert(k.clone(), v.clone());
    }
    Value::Object(out)
}

/// `{{settings.<name>}}` in every string of `v`, replaced by the value
/// (text as it is, anything else as JSON).
pub fn fill_settings(v: &Value, settings: &Value) -> Value {
    match v {
        Value::String(s) if s.contains("{{settings.") => Value::String(fill_text(s, settings)),
        Value::Array(a) => Value::Array(a.iter().map(|x| fill_settings(x, settings)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), fill_settings(x, settings))).collect()),
        other => other.clone(),
    }
}

pub fn fill_text(s: &str, settings: &Value) -> String {
    let mut out = s.to_owned();
    for (k, v) in settings.as_object().into_iter().flatten() {
        let with = v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string());
        out = out.replace(&format!("{{{{settings.{k}}}}}"), &with);
    }
    out
}

/// Whether an instance's approved reads let it read collection `name`,
/// owned by `owner` ("" for the realm). Its own are always readable.
pub fn may_read_collection(reads: &api::Reads, me: &str, name: &str, owner: &str) -> bool {
    if owner == me {
        return true;
    }
    reads.collections.iter().any(|pattern| match pattern.strip_suffix(".*") {
        Some(inst) => !owner.is_empty() && owner == inst,
        None => pattern == name,
    })
}

/// The reads approved for an instance (as installed).
pub async fn approved_reads(pool: &sqlx::SqlitePool, id: i64) -> anyhow::Result<api::Reads> {
    let raw: Option<String> = sqlx::query_scalar("SELECT approved FROM installs WHERE id = ?").bind(id).fetch_optional(pool).await?;
    let v: Value = raw.and_then(|r| serde_json::from_str(&r).ok()).unwrap_or(Value::Null);
    Ok(serde_json::from_value(v["reads"].clone()).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_fill_code_and_reads_match() {
        let schema = json!({"type": "object", "properties": {"weeks": {"type": "integer", "default": 6}, "voice": {"type": "string"}}});
        let s = with_defaults(&schema, &json!({"voice": "warm"}));
        assert_eq!(s, json!({"weeks": 6, "voice": "warm"}));
        let e = fill_settings(&json!({"body": "Every {{settings.weeks}} weeks, {{settings.voice}}.", "n": 1}), &s);
        assert_eq!(e, json!({"body": "Every 6 weeks, warm.", "n": 1}));
        let reads = api::Reads { collections: vec!["contacts".into(), "gmail.*".into()], ..Default::default() };
        assert!(may_read_collection(&reads, "crm", "touchpoints", "crm"), "its own");
        assert!(may_read_collection(&reads, "crm", "contacts", ""));
        assert!(may_read_collection(&reads, "crm", "threads", "gmail"));
        assert!(!may_read_collection(&reads, "crm", "notes", ""));
        assert!(!may_read_collection(&reads, "crm", "threads", "other"));
    }
}

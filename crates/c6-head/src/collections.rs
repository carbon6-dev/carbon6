//! Collections and records (docs/os.md §4.1): typed tables in a realm, for
//! anything apps and agents keep. Records always match their collection's
//! schema (strictly: see `schema`), unique keys hold, writes give the
//! version they read, every change is kept with who made it, and records
//! of non-system collections are mirrored into the memex for search and
//! briefs.

use c6_api as api;
use c6_core::now_ms;
use serde_json::{Value, json};
use sqlx::Row;

use crate::{HeadState, bad_request, not_found};

/// Records returned by a query at most.
const MAX_LIMIT: u32 = 500;
/// A record's fields, serialized, at most.
const MAX_RECORD: usize = 256 * 1024;

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn valid_field(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn collection_of(r: &sqlx::sqlite::SqliteRow, records: i64) -> api::Collection {
    api::Collection {
        realm: r.get("realm"),
        name: r.get("name"),
        description: r.get("description"),
        schema: serde_json::from_str(&r.get::<String, _>("schema")).unwrap_or(Value::Null),
        schema_version: r.get("schema_version"),
        keys: serde_json::from_str(&r.get::<String, _>("keys")).unwrap_or_default(),
        system: r.get::<i64, _>("system") != 0,
        records,
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        owner: r.try_get("owner").unwrap_or_default(),
        card: r.try_get::<String, _>("card").ok().and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default(),
    }
}

fn record_of(r: &sqlx::sqlite::SqliteRow) -> api::Record {
    api::Record {
        id: r.get("id"),
        realm: r.get("realm"),
        collection: r.get("collection"),
        fields: serde_json::from_str(&r.get::<String, _>("fields")).unwrap_or(Value::Null),
        links: serde_json::from_str(&r.get::<String, _>("links")).unwrap_or_default(),
        labels: serde_json::from_str(&r.get::<String, _>("labels")).unwrap_or_default(),
        version: r.get("version"),
        created_by: r.get("created_by"),
        updated_by: r.get("updated_by"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        derived_from: r.try_get::<String, _>("derived_from").ok().and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default(),
        provenance: r
            .try_get::<String, _>("provenance")
            .ok()
            .and_then(|c| serde_json::from_str::<Value>(&c).ok())
            .filter(|v| v.as_object().is_some_and(|o| !o.is_empty()))
            .unwrap_or(Value::Null),
    }
}

/// An app's collection's card, with what the head knows: the app's
/// version, and the operations that change it.
async fn with_card(st: &HeadState, mut c: api::Collection) -> api::Collection {
    if c.owner.is_empty() {
        return c;
    }
    if let Ok(Some(i)) = crate::instances::get(&st.store.pool, &c.realm, &c.owner).await {
        c.card.version = i.package.version.clone();
        c.card.operations = i
            .package
            .operations
            .iter()
            .filter(|o| o.write.as_ref().is_none_or(|w| w.collection == c.name))
            .map(|o| o.name.clone())
            .collect();
    }
    c
}

/// A card, in words, for an agent reading the collection for the first time.
pub fn card_text(c: &api::Collection) -> String {
    let mut out = format!("{}: {}", c.name, if c.description.is_empty() { "(no description)" } else { &c.description });
    let fields: Vec<String> = c.schema["properties"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| v["description"].as_str().map(|d| format!("{k}: {d}")))
        .collect();
    if !fields.is_empty() {
        out.push_str(&format!("\nFields: {}", fields.join("; ")));
    }
    if c.owner.is_empty() {
        out.push_str("\nOwner: the realm (anything in it may write it).");
    } else {
        out.push_str(&format!(
            "\nOwner: the app {}{} ({}); written only by it{}.",
            c.owner,
            if c.card.version.is_empty() { String::new() } else { format!(" {}", c.card.version) },
            if c.card.internal() {
                "internal: it may change without notice; build on its published collections instead"
            } else {
                "published"
            },
            if c.card.operations.is_empty() {
                String::new()
            } else {
                format!(", changed through its operations {}", c.card.operations.join(", "))
            }
        ));
    }
    if !c.card.sources.is_empty() {
        out.push_str(&format!("\nWhere it comes from: {}", c.card.sources));
    }
    if !c.card.invariants.is_empty() {
        out.push_str(&format!("\nWhat stays true: {}", c.card.invariants.join("; ")));
    }
    if !c.card.freshness.is_empty() {
        out.push_str(&format!("\nFreshness: {}", c.card.freshness));
    }
    out
}

/// The note on an internal collection's records, for readers other than its app.
pub fn internal_note(c: &api::Collection) -> Option<String> {
    (c.card.internal() && !c.owner.is_empty())
        .then(|| format!("internal to {}: may change without notice; build on its published collections instead", c.owner))
}

pub async fn list(st: &HeadState, realm: &str) -> anyhow::Result<Vec<api::Collection>> {
    let rows = sqlx::query(
        "SELECT c.*, (SELECT count(*) FROM records r WHERE r.realm = c.realm AND r.collection = c.name) AS n \
         FROM collections c WHERE realm = ? ORDER BY system, name",
    )
    .bind(realm)
    .fetch_all(&st.store.pool)
    .await?;
    let mut out = Vec::new();
    for r in &rows {
        out.push(with_card(st, collection_of(r, r.get("n"))).await);
    }
    Ok(out)
}

pub async fn get(st: &HeadState, realm: &str, name: &str) -> anyhow::Result<api::Collection> {
    let r = sqlx::query(
        "SELECT c.*, (SELECT count(*) FROM records r WHERE r.realm = c.realm AND r.collection = c.name) AS n \
         FROM collections c WHERE realm = ? AND name = ?",
    )
    .bind(realm)
    .bind(name)
    .fetch_optional(&st.store.pool)
    .await?
    .ok_or_else(|| not_found(format!("no collection {name:?} in realm {realm}")))?;
    Ok(with_card(st, collection_of(&r, r.get("n"))).await)
}

fn check_card(card: &api::DataCard) -> anyhow::Result<()> {
    if !matches!(card.stability.as_str(), "" | "published" | "internal") {
        return Err(bad_request("a card's stability is published or internal"));
    }
    Ok(())
}

/// What a card keeps (what the head fills in is not stored).
fn stored_card(card: &api::DataCard) -> String {
    let c = api::DataCard { version: String::new(), operations: Vec::new(), ..card.clone() };
    serde_json::to_string(&c).unwrap_or_else(|_| "{}".into())
}

fn check_keys(schema: &Value, keys: &[String]) -> anyhow::Result<()> {
    for k in keys {
        if !valid_field(k) {
            return Err(bad_request(format!("key {k:?} is not a field name (letters, digits and _)")));
        }
        if schema["properties"].get(k).is_none() {
            return Err(bad_request(format!("key {k:?} is not a field of the schema")));
        }
    }
    Ok(())
}

/// A new collection in `realm`.
pub async fn create(st: &HeadState, realm: &str, c: &api::CollectionIn, by: &str, system: bool) -> anyhow::Result<api::Collection> {
    create_owned(st, realm, c, by, system, "").await
}

/// A new collection owned by an app instance (`owner`), or by the realm
/// (`""`): docs/apps.md §6.
pub async fn create_owned(
    st: &HeadState,
    realm: &str,
    c: &api::CollectionIn,
    by: &str,
    system: bool,
    owner: &str,
) -> anyhow::Result<api::Collection> {
    let name = c.name.trim();
    if !valid_name(name) {
        return Err(bad_request("a collection's name is letters, digits, - and _ (up to 64)"));
    }
    let schema = c.schema.clone().ok_or_else(|| bad_request("a collection needs a schema"))?;
    crate::schema::check_schema(&schema).map_err(bad_request)?;
    let keys = c.keys.clone().unwrap_or_default();
    check_keys(&schema, &keys)?;
    let card = c.card.clone().unwrap_or_default();
    check_card(&card)?;
    let now = now_ms();
    let done = sqlx::query(
        "INSERT OR IGNORE INTO collections (realm, name, description, schema, keys, system, created_by, created_at, updated_at, owner, card) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(realm)
    .bind(name)
    .bind(c.description.clone().unwrap_or_default().trim())
    .bind(schema.to_string())
    .bind(serde_json::to_string(&keys)?)
    .bind(system as i64)
    .bind(by)
    .bind(now)
    .bind(now)
    .bind(owner)
    .bind(stored_card(&card))
    .execute(&st.store.pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(bad_request(format!("realm {realm} already has a collection {name:?}")));
    }
    get(st, realm, name).await
}

/// Changes a collection's description, keys or schema. A new schema must
/// accept every record already in it, and keys must already be unique.
pub async fn update(st: &HeadState, realm: &str, name: &str, c: &api::CollectionIn) -> anyhow::Result<api::Collection> {
    let current = get(st, realm, name).await?;
    let schema = c.schema.clone().unwrap_or(current.schema.clone());
    let keys = c.keys.clone().unwrap_or(current.keys.clone());
    let new_schema = schema != current.schema;
    if new_schema {
        crate::schema::check_schema(&schema).map_err(bad_request)?;
    }
    check_keys(&schema, &keys)?;
    let records = all(st, realm, name).await?;
    if new_schema {
        let misfits: Vec<String> = records
            .iter()
            .filter_map(|r| crate::schema::validate(&schema, &r.fields).err().map(|e| format!("{}: {}", r.id, e.join("; "))))
            .collect();
        if !misfits.is_empty() {
            return Err(bad_request(format!(
                "the new schema rejects {} existing record(s), so it needs a migration: {}",
                misfits.len(),
                misfits.iter().take(3).cloned().collect::<Vec<_>>().join(" | ")
            )));
        }
    }
    for k in &keys {
        let mut seen = std::collections::HashSet::new();
        if records.iter().filter_map(|r| r.fields.get(k)).any(|v| !seen.insert(v.to_string())) {
            return Err(bad_request(format!("{k:?} is not unique among the records already there")));
        }
    }
    let card = c.card.clone().unwrap_or(current.card.clone());
    check_card(&card)?;
    sqlx::query(
        "UPDATE collections SET description = ?, schema = ?, keys = ?, card = ?, schema_version = schema_version + ?, updated_at = ? \
         WHERE realm = ? AND name = ?",
    )
    .bind(c.description.clone().unwrap_or(current.description).trim())
    .bind(schema.to_string())
    .bind(serde_json::to_string(&keys)?)
    .bind(stored_card(&card))
    .bind(new_schema as i64)
    .bind(now_ms())
    .bind(realm)
    .bind(name)
    .execute(&st.store.pool)
    .await?;
    get(st, realm, name).await
}

/// Deletes a collection and its records (their history stays).
pub async fn drop(st: &HeadState, realm: &str, name: &str) -> anyhow::Result<()> {
    let c = get(st, realm, name).await?;
    if c.system {
        return Err(bad_request(format!("{name} is carbon6's own; it cannot be deleted")));
    }
    let records = all(st, realm, name).await?;
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM records WHERE realm = ? AND collection = ?").bind(realm).bind(name).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM collections WHERE realm = ? AND name = ?").bind(realm).bind(name).execute(&mut *tx).await?;
    tx.commit().await?;
    for r in &records {
        unmirror(st, realm, name, &r.id).await;
    }
    Ok(())
}

async fn all(st: &HeadState, realm: &str, name: &str) -> anyhow::Result<Vec<api::Record>> {
    let rows =
        sqlx::query("SELECT * FROM records WHERE realm = ? AND collection = ?").bind(realm).bind(name).fetch_all(&st.store.pool).await?;
    Ok(rows.iter().map(record_of).collect())
}

pub async fn record(st: &HeadState, realm: &str, collection: &str, id: &str) -> anyhow::Result<api::Record> {
    sqlx::query("SELECT * FROM records WHERE id = ? AND realm = ? AND collection = ?")
        .bind(id)
        .bind(realm)
        .bind(collection)
        .fetch_optional(&st.store.pool)
        .await?
        .as_ref()
        .map(record_of)
        .ok_or_else(|| not_found(format!("no record {id} in {collection}")))
}

/// Creates (`id: None`) or replaces a record. Its fields must match the
/// schema and keep the keys unique; an update must give the version it read.
pub async fn put(
    st: &HeadState,
    realm: &str,
    collection: &str,
    id: Option<&str>,
    r: &api::RecordIn,
    by: &str,
) -> anyhow::Result<api::Record> {
    let c = get(st, realm, collection).await?;
    if c.system {
        return Err(system_list(collection));
    }
    check_owner(st, &c, by).await?;
    if !r.fields.is_object() {
        return Err(bad_request("a record's fields are an object"));
    }
    if r.fields.to_string().len() > MAX_RECORD {
        return Err(bad_request(format!("a record is at most {} KB", MAX_RECORD / 1024)));
    }
    if let Err(problems) = crate::schema::validate(&c.schema, &r.fields) {
        return Err(bad_request(format!("the record does not match the schema of {collection}: {}", problems.join("; "))));
    }
    let labels = crate::sessions::check_labels(r.labels.as_deref().unwrap_or_default())?;
    if let Some(d) = &r.derived_from
        && let Some(bad) = d.iter().find(|x| !(x.starts_with("item:") || x.starts_with("record:") || x.starts_with("http")))
    {
        return Err(bad_request(format!("{bad:?}: derived_from names items (item:<id>) and records (record:<collection>/<id>)")));
    }
    let provenance = provenance_of(st, &c, by).await;
    let now = now_ms();
    let mut before: Option<Value> = None;
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    for k in &c.keys {
        if let Some(v) = r.fields.get(k) {
            let clash: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT id FROM records WHERE realm = ? AND collection = ? AND json_extract(fields, '$.{k}') = json_extract(?, '$') AND id != ?"
            )))
            .bind(realm)
            .bind(collection)
            .bind(v.to_string())
            .bind(id.unwrap_or(""))
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(other) = clash {
                return Err(bad_request(format!("{k} {v} is already used by record {other}")));
            }
        }
    }
    let (id, version, op) = match id {
        None => {
            let id = uuid::Uuid::now_v7().to_string();
            sqlx::query(
                "INSERT INTO records (id, realm, collection, fields, links, labels, version, created_by, updated_by, created_at, updated_at, \
                 derived_from, provenance) VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(realm)
            .bind(collection)
            .bind(r.fields.to_string())
            .bind(serde_json::to_string(&r.links.clone().unwrap_or_default())?)
            .bind(serde_json::to_string(&labels)?)
            .bind(by)
            .bind(by)
            .bind(now)
            .bind(now)
            .bind(serde_json::to_string(&r.derived_from.clone().unwrap_or_default())?)
            .bind(provenance.to_string())
            .execute(&mut *tx)
            .await?;
            (id, 1, "create")
        }
        Some(id) => {
            let current: Option<(i64, String, String, String)> =
                sqlx::query_as("SELECT version, links, labels, fields FROM records WHERE id = ? AND realm = ? AND collection = ?")
                    .bind(id)
                    .bind(realm)
                    .bind(collection)
                    .fetch_optional(&mut *tx)
                    .await?;
            let (version, links, old_labels, old_fields) = current.ok_or_else(|| not_found(format!("no record {id} in {collection}")))?;
            before = serde_json::from_str(&old_fields).ok();
            match r.version {
                Some(v) if v != version => {
                    return Err(bad_request(format!(
                        "record {id} changed since you read it (you have version {v}; it is at {version}): read it again"
                    )));
                }
                None => return Err(bad_request("an update gives the version it read (version)")),
                _ => {}
            }
            let links = match &r.links {
                Some(l) => serde_json::to_string(l)?,
                None => links,
            };
            let labels = if r.labels.is_some() { serde_json::to_string(&labels)? } else { old_labels };
            sqlx::query(
                "UPDATE records SET fields = ?, links = ?, labels = ?, version = ?, updated_by = ?, updated_at = ?, provenance = ?, \
                 derived_from = coalesce(?, derived_from) WHERE id = ?",
            )
            .bind(r.fields.to_string())
            .bind(links)
            .bind(labels)
            .bind(version + 1)
            .bind(by)
            .bind(now)
            .bind(provenance.to_string())
            .bind(r.derived_from.as_ref().map(|d| serde_json::to_string(d).unwrap_or_default()))
            .bind(id)
            .execute(&mut *tx)
            .await?;
            (id.to_owned(), version + 1, "update")
        }
    };
    history(&mut tx, &id, version, op, &r.fields, by, now).await?;
    tx.commit().await?;
    let rec = record(st, realm, collection, &id).await?;
    if !c.system {
        mirror(st, &c, &rec).await;
    }
    st.registry.bump();
    // Triggers waiting for it (docs/os.md §4.3).
    crate::triggers::on_record(st, realm, if op == "create" { "created" } else { "updated" }, &rec, before.as_ref()).await;
    Ok(rec)
}

async fn history(
    tx: &mut sqlx::SqliteConnection,
    id: &str,
    version: i64,
    op: &str,
    fields: &Value,
    by: &str,
    at: i64,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO record_history (record_id, version, op, fields, changed_by, changed_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(id)
        .bind(version)
        .bind(op)
        .bind(fields.to_string())
        .bind(by)
        .bind(at)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Why a system collection's records are not written here: they change
/// through Settings (and agents' remember/forget), which check them.
fn system_list(collection: &str) -> anyhow::Error {
    bad_request(format!("{collection} is carbon6's own configuration: change it under Settings"))
}

/// Deletes a record; its history stays.
pub async fn delete(st: &HeadState, realm: &str, collection: &str, id: &str, by: &str) -> anyhow::Result<()> {
    let c = get(st, realm, collection).await?;
    if c.system {
        return Err(system_list(collection));
    }
    check_owner(st, &c, by).await?;
    let r = record(st, realm, collection, id).await?;
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM records WHERE id = ?").bind(id).execute(&mut *tx).await?;
    history(&mut tx, id, r.version + 1, "delete", &r.fields, by, now_ms()).await?;
    tx.commit().await?;
    unmirror(st, realm, collection, id).await;
    st.registry.bump();
    crate::triggers::on_record(st, realm, "deleted", &r, Some(&r.fields)).await;
    Ok(())
}

pub async fn changes(st: &HeadState, realm: &str, collection: &str, id: &str) -> anyhow::Result<Vec<api::RecordChange>> {
    // The record may be deleted; its history is still its realm's.
    let known: Option<String> = sqlx::query_scalar(
        "SELECT h.record_id FROM record_history h WHERE h.record_id = ? AND (EXISTS (SELECT 1 FROM records r WHERE r.id = h.record_id \
         AND r.realm = ? AND r.collection = ?) OR NOT EXISTS (SELECT 1 FROM records r WHERE r.id = h.record_id)) LIMIT 1",
    )
    .bind(id)
    .bind(realm)
    .bind(collection)
    .fetch_optional(&st.store.pool)
    .await?;
    if known.is_none() {
        return Err(not_found(format!("no record {id} in {collection}")));
    }
    let rows = sqlx::query("SELECT * FROM record_history WHERE record_id = ? ORDER BY version").bind(id).fetch_all(&st.store.pool).await?;
    Ok(rows
        .iter()
        .map(|r| api::RecordChange {
            version: r.get("version"),
            op: r.get("op"),
            fields: serde_json::from_str(&r.get::<String, _>("fields")).unwrap_or(Value::Null),
            changed_by: r.get("changed_by"),
            changed_at: r.get("changed_at"),
        })
        .collect())
}

/// A field reference for SQL: a field of the record, or created_at /
/// updated_at.
fn column(field: &str) -> anyhow::Result<String> {
    match field {
        "created_at" | "updated_at" | "id" => Ok(field.to_owned()),
        f if valid_field(f) => Ok(format!("json_extract(fields, '$.{f}')")),
        f => Err(bad_request(format!("{f:?} is not a field name"))),
    }
}

/// Records matching a query.
pub async fn query(st: &HeadState, realm: &str, collection: &str, q: &api::RecordQuery) -> anyhow::Result<api::RecordPage> {
    let note = internal_note(&get(st, realm, collection).await?);
    let mut sql = String::from(" FROM records WHERE realm = ? AND collection = ?");
    let mut binds: Vec<Value> = vec![json!(realm), json!(collection)];
    for (field, cond) in q.filter.iter().chain(&q.within) {
        let col = column(field)?;
        let conds: Vec<(String, Value)> = match cond {
            Value::Object(ops) if ops.keys().all(|k| k.starts_with('$')) => ops.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            v => vec![("$eq".into(), v.clone())],
        };
        for (op, v) in conds {
            match op.as_str() {
                "$eq" | "$ne" | "$gt" | "$gte" | "$lt" | "$lte" => {
                    let sym = match op.as_str() {
                        "$eq" => "=",
                        "$ne" => "IS NOT",
                        "$gt" => ">",
                        "$gte" => ">=",
                        "$lt" => "<",
                        _ => "<=",
                    };
                    sql.push_str(&format!(" AND {col} {sym} json_extract(?, '$')"));
                    binds.push(json!(v.to_string()));
                }
                "$in" | "$nin" => {
                    let list = v.as_array().ok_or_else(|| bad_request(format!("{field}: {op} takes a list")))?;
                    if list.is_empty() {
                        sql.push_str(if op == "$in" { " AND 0" } else { "" });
                        continue;
                    }
                    let marks = vec!["json_extract(?, '$')"; list.len()].join(", ");
                    sql.push_str(&format!(" AND {col} {} ({marks})", if op == "$in" { "IN" } else { "NOT IN" }));
                    binds.extend(list.iter().map(|x| json!(x.to_string())));
                }
                "$contains" => {
                    // Text containing it, or a list with it as an item.
                    sql.push_str(&format!(
                        " AND (instr(lower(CAST({col} AS TEXT)), lower(?)) > 0 OR EXISTS (SELECT 1 FROM json_each(fields, '$.{field}') e \
                         WHERE e.value = json_extract(?, '$')))"
                    ));
                    binds.push(json!(v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())));
                    binds.push(json!(v.to_string()));
                }
                other => return Err(bad_request(format!("{field}: unknown comparison {other}"))),
            }
        }
    }
    for word in q.text.split_whitespace() {
        sql.push_str(" AND instr(lower(fields), lower(?)) > 0");
        binds.push(json!(word));
    }
    let bind_all = |mut qy: sqlx::query::Query<'static, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>| {
        for b in binds.clone() {
            qy = match b {
                Value::String(s) => qy.bind(s),
                other => qy.bind(other.to_string()),
            };
        }
        qy
    };
    let total: i64 =
        bind_all(sqlx::query(sqlx::AssertSqlSafe(format!("SELECT count(*) AS n{sql}")))).fetch_one(&st.store.pool).await?.get("n");
    if !q.group_by.is_empty() {
        let col = column(&q.group_by)?;
        let rows = bind_all(sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT json_quote({col}) AS v, count(*) AS n{sql} GROUP BY {col} ORDER BY n DESC, v LIMIT {MAX_LIMIT}"
        ))))
        .fetch_all(&st.store.pool)
        .await?;
        let groups = rows
            .iter()
            .map(|r| {
                (serde_json::from_str(&r.get::<Option<String>, _>("v").unwrap_or_else(|| "null".into())).unwrap_or(Value::Null), r.get("n"))
            })
            .collect();
        return Ok(api::RecordPage { records: Vec::new(), total, groups, note });
    }
    let (field, desc) = match q.sort.trim() {
        "" => ("updated_at", true),
        s => s.strip_prefix('-').map_or((s, false), |f| (f, true)),
    };
    let order = format!("{} {}, id", column(field)?, if desc { "DESC" } else { "ASC" });
    let limit = q.limit.unwrap_or(100).min(MAX_LIMIT);
    let offset = q.offset.unwrap_or(0);
    let rows = bind_all(sqlx::query(sqlx::AssertSqlSafe(format!("SELECT *{sql} ORDER BY {order} LIMIT {limit} OFFSET {offset}"))))
        .fetch_all(&st.store.pool)
        .await?;
    Ok(api::RecordPage { records: rows.iter().map(record_of).collect(), total, groups: Vec::new(), note })
}

/// The record as a memex item, for search and briefs.
async fn mirror(st: &HeadState, c: &api::Collection, r: &api::Record) {
    if !crate::memex::has_memex(&r.realm) {
        return;
    }
    let title = ["title", "name", "subject", "claim"]
        .iter()
        .find_map(|k| r.fields.get(*k).and_then(Value::as_str))
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{} {}", c.name, &r.id[r.id.len().saturating_sub(6)..]));
    let text = r
        .fields
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| format!("{k}: {}", v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    // Search results carry what the collection says about itself.
    let text = format!("{text}\n\nAbout {}", card_text(c));
    let mut labels = r.labels.clone();
    labels.extend(r.links.iter().filter(|l| l.contains(':')).cloned());
    labels.push(format!("collection:{}", c.name));
    let item = api::MemexItemIn {
        kind: "record".into(),
        source: format!("collection:{}", c.name),
        external_id: r.id.clone(),
        title,
        text,
        labels,
        meta: json!({"collection": c.name, "record": r.id, "version": r.version}),
        ..Default::default()
    };
    crate::memex::enqueue(st, "memex.ingest", crate::memex::in_realm(json!(item), &r.realm)).await;
}

async fn unmirror(st: &HeadState, realm: &str, collection: &str, id: &str) {
    if crate::memex::has_memex(realm) {
        let params = json!({"source": format!("collection:{collection}"), "external_id": id});
        crate::memex::enqueue(st, "memex.delete", crate::memex::in_realm(params, realm)).await;
    }
}

/// Collections each session has read (their cards come once).
static READ: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::LazyLock::new(Default::default);

fn first_read(session: &str, realm: &str, name: &str) -> bool {
    READ.lock()
        .map(|mut r| {
            if r.len() > 100_000 {
                r.clear();
            }
            r.insert(format!("{session}/{realm}/{name}"))
        })
        .unwrap_or(false)
}

/// Who wrote a record's current version (docs/apps.md §8): the app and its
/// package version when an app did, and who asked.
async fn provenance_of(st: &HeadState, c: &api::Collection, by: &str) -> Value {
    let (instance, forced) = writer_of(st, by).await;
    let mut p = json!({"by": by});
    if let Some(name) = instance
        && let Ok(Some(i)) = crate::instances::get(&st.store.pool, &c.realm, &name).await
    {
        p["app"] = json!(name);
        p["package"] = json!(i.package.app);
        p["version"] = json!(i.package.version);
    }
    if forced {
        p["forced"] = json!(true);
    }
    for w in by.split_whitespace() {
        if let Some(s) = w.strip_prefix("session:") {
            p["session"] = json!(s);
        } else if let Some(o) = w.strip_prefix("op:") {
            p["operation"] = json!(match p["app"].as_str() {
                Some(app) => format!("{app}.{o}"),
                None => o.to_owned(),
            });
        }
    }
    p
}

/// Who a write is from, as far as ownership goes: the app instance it acts
/// as (`app:<name>`, or a session acting as one), and whether you forced
/// it (docs/apps.md §6).
pub async fn writer_of(st: &HeadState, by: &str) -> (Option<String>, bool) {
    let mut instance = None;
    let mut forced = false;
    for word in by.split_whitespace() {
        if let Some(name) = word.strip_prefix("app:") {
            instance = Some(name.to_owned());
        } else if let Some(session) = word.strip_prefix("session:")
            && instance.is_none()
        {
            instance = crate::instances::of_session(&st.store.pool, session).await.ok().flatten().map(|i| i.name);
        } else if word == "forced" {
            forced = true;
        }
    }
    (instance, forced)
}

/// An app instance's collection is written only by that instance (its
/// sessions, triggers, views and operations), or by you, forced.
pub async fn check_owner(st: &HeadState, c: &api::Collection, by: &str) -> anyhow::Result<()> {
    if c.owner.is_empty() {
        return Ok(());
    }
    let (instance, forced) = writer_of(st, by).await;
    if forced || instance.as_deref() == Some(c.owner.as_str()) {
        return Ok(());
    }
    Err(bad_request(format!(
        "{} belongs to the app {}, which keeps it consistent: change it through {}'s operations{}",
        c.name,
        c.owner,
        c.owner,
        if by == "you" { ", or force the write (it asks you first)" } else { "" }
    )))
}

/// An app instance's code (its collections' schemas, its triggers and
/// series) changes only by upgrading it (docs/apps.md §5.5).
pub fn check_code(c: &api::Collection) -> anyhow::Result<()> {
    if c.owner.is_empty() {
        return Ok(());
    }
    Err(bad_request(format!("{} is the app {}'s: its schema changes only when the app is upgraded (or forked)", c.name, c.owner)))
}

/// You, writing to an app's collection anyway: a decision first, naming
/// what the app keeps true (docs/apps.md §6). `id`: the record (none:
/// a new one); `rec` none: deleting it.
pub async fn ask_force(
    st: &HeadState,
    realm: &str,
    collection: &str,
    id: Option<&str>,
    rec: Option<&api::RecordIn>,
) -> anyhow::Result<api::Decision> {
    let c = get(st, realm, collection).await?;
    if c.owner.is_empty() {
        return Err(bad_request(format!("{collection} is the realm's; write it directly")));
    }
    let what = match (id, rec) {
        (Some(id), None) => format!("delete record {id}"),
        (Some(id), Some(_)) => format!("change record {id}"),
        (None, _) => "add a record".to_owned(),
    };
    let mut doc = format!(
        "**{collection}** belongs to the app **{}**, which keeps it consistent through its operations. Writing it directly can break what \
         it keeps true.",
        c.owner
    );
    if !c.description.is_empty() {
        doc.push_str(&format!("\n\n{}", c.description));
    }
    if let Some(r) = rec {
        doc.push_str(&format!("\n\nThe fields:\n\n```json\n{}\n```", serde_json::to_string_pretty(&r.fields).unwrap_or_default()));
    }
    let d = api::DecisionIn {
        kind: "approve".into(),
        title: format!("Force: {what} in {collection}?"),
        document: doc,
        options: ["Write it", "Cancel"].iter().map(|o| api::DecisionOption { label: (*o).to_owned(), does: String::new() }).collect(),
        source: format!("collection:{collection}"),
        context: json!({"realm": realm, "collection": collection, "id": id, "record": rec}),
        ..Default::default()
    };
    crate::decisions::open(st, &d, realm, &format!("force:{}", uuid::Uuid::now_v7())).await
}

/// Your answer to a forced write.
pub async fn answer_force(st: &HeadState, d: &api::Decision, option: Option<&str>) -> anyhow::Result<()> {
    if option != Some("Write it") {
        return Ok(());
    }
    let ctx = &d.context;
    let (realm, collection) = (ctx["realm"].as_str().unwrap_or_default(), ctx["collection"].as_str().unwrap_or_default());
    let id = ctx["id"].as_str();
    match serde_json::from_value::<Option<api::RecordIn>>(ctx["record"].clone()).ok().flatten() {
        Some(rec) => {
            put(st, realm, collection, id, &rec, "you forced").await?;
        }
        None => delete(st, realm, collection, id.unwrap_or_default(), "you forced").await?,
    }
    Ok(())
}

/// An agent's call on collections, in its session's realm.
pub async fn for_session(st: &HeadState, realm: &str, session_id: &str, what: &str, p: &Value) -> anyhow::Result<Value> {
    let mut by = format!("session:{session_id}");
    let name = p["collection"].as_str().unwrap_or_default();
    let id = p["id"].as_str().unwrap_or_default();
    // Another realm's collection, through a grant.
    let mode = match what {
        "records_put" if id.is_empty() => "append",
        "records_put" | "records_delete" => "write",
        _ => "read",
    };
    let to = if what == "collections" {
        crate::grants::Reach { realm: realm.to_owned(), within: None, grant: None }
    } else {
        crate::grants::reach(st, realm, p["realm"].as_str(), name, mode, &by).await?
    };
    if let Some(g) = &to.grant {
        by = format!("realm:{realm} {by} grant:{g}");
    }
    let realm = to.realm.as_str();
    // An app instance's session reads what its install approved (docs/apps.md §5.2).
    if matches!(what, "records_query" | "records_get")
        && to.grant.is_none()
        && let Some(me) = crate::instances::of_session(&st.store.pool, session_id).await?
    {
        let c = get(st, realm, name).await?;
        let reads = crate::instances::approved_reads(&st.store.pool, me.id).await?;
        if !crate::instances::may_read_collection(&reads, &me.name, name, &c.owner) {
            return Err(bad_request(format!(
                "the app {} may not read {name}: its install did not ask to (it reads {})",
                me.name,
                if reads.collections.is_empty() { "only its own collections".to_owned() } else { reads.collections.join(", ") }
            )));
        }
    }
    // What a collection says about itself, the first time a session reads
    // it (docs/apps.md §8); an internal one's note for anyone but its app.
    let explain = async |out: &mut Value| -> anyhow::Result<()> {
        let c = get(st, realm, name).await?;
        let me = crate::instances::of_session(&st.store.pool, session_id).await?.map(|i| i.name);
        if me.as_deref() != Some(c.owner.as_str())
            && let Some(n) = internal_note(&c)
        {
            out["note"] = json!(n);
        } else if let Some(o) = out.as_object_mut() {
            o.remove("note");
        }
        if first_read(session_id, realm, name) {
            out["about"] = json!(card_text(&c));
        }
        Ok(())
    };
    match what {
        "collections" => Ok(json!(list(st, realm).await?)),
        "records_query" => {
            let mut q: api::RecordQuery = serde_json::from_value(p["query"].clone()).unwrap_or_default();
            if let Some(w) = &to.within {
                crate::grants::narrow(&mut q, w);
            }
            let mut out = json!(query(st, realm, name, &q).await?);
            explain(&mut out).await?;
            Ok(out)
        }
        "records_get" => {
            crate::grants::check_within(st, &to, name, id).await?;
            let mut out = json!(record(st, realm, name, id).await?);
            explain(&mut out).await?;
            Ok(out)
        }
        "records_put" => {
            let r: api::RecordIn = serde_json::from_value(json!({
                "fields": p["fields"], "links": p.get("links"), "labels": p.get("labels"), "version": p.get("version"),
                "derived_from": p.get("derived_from")
            }))
            .map_err(|e| bad_request(format!("{e}")))?;
            // A record for commons waits for the user's review.
            if realm == crate::realms::COMMONS && to.grant.is_some() && id.is_empty() {
                let own = crate::realms::of_session(st, session_id).await?.unwrap_or_default();
                let d = crate::commons::propose_record(st, &own, &format!("session:{session_id}"), name, &r).await?;
                return Ok(json!({"proposed": true, "decision": d.id, "note": "it waits for the user's review"}));
            }
            Ok(json!(put(st, realm, name, (!id.is_empty()).then_some(id), &r, &by).await?))
        }
        "records_delete" => {
            delete(st, realm, name, id, &by).await?;
            Ok(json!({"deleted": id}))
        }
        other => Err(bad_request(format!("unknown collections call {other}"))),
    }
}

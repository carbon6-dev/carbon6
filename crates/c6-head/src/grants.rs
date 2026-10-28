//! Grants (docs/os.md §3.7): the only way data crosses realms. A grant says
//! "realm A's data may flow into realm B" — B may read A's collection (or
//! some of its records), add to it, or take a copy once; read A's
//! knowledge of some kinds; or call some of A's operations (drafting mail
//! through home's accounts). Every use is logged with who made it; what
//! crosses carries where it came from. Only you create grants: an agent
//! may request one, which comes to you as a decision.

use c6_api as api;
use c6_core::now_ms;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::warn;

use crate::realms::{COMMONS, ROOT};
use crate::{HeadState, bad_request, not_found};

const MODES: [(&str, &[&str]); 4] = [
    ("collection", &["read", "append", "snapshot"]),
    ("memex", &["read", "append"]),
    ("operations", &["call"]),
    // A session starting `from`'s template where it says, not only on its own worker.
    ("workers", &["start"]),
];

/// What every realm that reads commons has without a grant row.
pub const COMMONS_READ: &str = "commons";

fn grant_of(r: &sqlx::sqlite::SqliteRow) -> api::Grant {
    api::Grant {
        id: r.get("id"),
        from_realm: r.get("from_realm"),
        to_realm: r.get("to_realm"),
        kind: r.get("kind"),
        scope: serde_json::from_str(&r.get::<String, _>("scope")).unwrap_or(json!({})),
        mode: r.get("mode"),
        state: r.get("state"),
        expires_at: r.get("expires_at"),
        note: r.get("note"),
        created_by: r.get("created_by"),
        created_at: r.get("created_at"),
        revoked_at: r.get("revoked_at"),
        uses: r.try_get("uses").unwrap_or(0),
    }
}

async fn check(st: &HeadState, g: &api::GrantIn) -> anyhow::Result<()> {
    for r in [&g.from_realm, &g.to_realm] {
        if r == ROOT || !crate::realms::exists(st, r).await? {
            return Err(bad_request(format!("no realm {r:?} that holds data")));
        }
    }
    // Reaching other workers is a grant within a realm too.
    if g.from_realm == g.to_realm && g.kind != "workers" {
        return Err(bad_request("a grant is between two realms"));
    }
    if g.to_realm == COMMONS {
        return Err(bad_request(
            "nothing flows into commons except what a realm publishes, with your review: grant the realm append to commons",
        ));
    }
    if g.from_realm == COMMONS && g.kind == "operations" {
        return Err(bad_request("commons has no operations"));
    }
    // Adding to knowledge is publishing to commons (each item reviewed).
    if g.kind == "memex" && g.mode == "append" && g.from_realm != COMMONS {
        return Err(bad_request("a realm's knowledge is its own: append to knowledge is only publishing to commons"));
    }
    let Some((_, modes)) = MODES.iter().find(|(k, _)| *k == g.kind) else {
        return Err(bad_request("a grant's kind is collection, memex or operations"));
    };
    if !modes.contains(&g.mode.as_str()) {
        return Err(bad_request(format!("a {} grant's mode is {}", g.kind, modes.join(" or "))));
    }
    match g.kind.as_str() {
        "collection" if g.scope["collection"].as_str().is_none_or(str::is_empty) => {
            Err(bad_request("name the collection: {\"collection\": name}"))
        }
        "operations" if g.scope["methods"].as_array().is_none_or(Vec::is_empty) => {
            Err(bad_request("name the operations: {\"methods\": [\"memex.mail.draft\", …]}"))
        }
        "workers" if g.scope["template"].as_str().is_none_or(str::is_empty) => Err(bad_request("name the template: {\"template\": name}")),
        _ => Ok(()),
    }
}

/// A grant you make (`state: active`), or an agent's request for one
/// (`requested`, then answered as a decision).
pub async fn create(st: &HeadState, g: &api::GrantIn, by: &str, state: &str) -> anyhow::Result<api::Grant> {
    check(st, g).await?;
    let id = uuid::Uuid::now_v7().simple().to_string()[16..].to_owned();
    sqlx::query(
        "INSERT INTO grants (id, from_realm, to_realm, kind, scope, mode, state, expires_at, note, created_by, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&g.from_realm)
    .bind(&g.to_realm)
    .bind(&g.kind)
    .bind(if g.scope.is_null() { "{}".to_owned() } else { g.scope.to_string() })
    .bind(&g.mode)
    .bind(state)
    .bind(g.expires_at)
    .bind(g.note.trim())
    .bind(by)
    .bind(now_ms())
    .execute(&st.store.pool)
    .await?;
    let grant = get(st, &id).await?.ok_or_else(|| anyhow::anyhow!("grant gone"))?;
    if state == "active" && grant.mode == "snapshot" {
        snapshot(st, &grant).await?;
    }
    st.registry.bump();
    Ok(grant)
}

pub async fn get(st: &HeadState, id: &str) -> anyhow::Result<Option<api::Grant>> {
    Ok(sqlx::query("SELECT g.*, (SELECT count(*) FROM grant_uses u WHERE u.grant_id = g.id) AS uses FROM grants g WHERE id = ?")
        .bind(id)
        .fetch_optional(&st.store.pool)
        .await?
        .as_ref()
        .map(grant_of))
}

/// Grants touching `realm` (either side), or all.
pub async fn list(st: &HeadState, realm: Option<&str>) -> anyhow::Result<Vec<api::Grant>> {
    let rows = sqlx::query(
        "SELECT g.*, (SELECT count(*) FROM grant_uses u WHERE u.grant_id = g.id) AS uses FROM grants g \
         WHERE (?1 IS NULL OR from_realm = ?1 OR to_realm = ?1) ORDER BY created_at DESC",
    )
    .bind(realm)
    .fetch_all(&st.store.pool)
    .await?;
    Ok(rows.iter().map(grant_of).collect())
}

/// Ends a grant: nothing more flows (what already crossed stays).
pub async fn revoke(st: &HeadState, id: &str) -> anyhow::Result<()> {
    let done = sqlx::query("UPDATE grants SET state = 'revoked', revoked_at = ? WHERE id = ? AND state IN ('active', 'requested')")
        .bind(now_ms())
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    if done.rows_affected() == 0 {
        return Err(not_found("no such grant in force"));
    }
    // A request revoked before it was answered: its decision follows.
    crate::decisions::settle_ref(st, &format!("grant:{id}"), "answered", json!({"option": "Deny"}), "Grants").await;
    st.registry.bump();
    Ok(())
}

pub async fn uses(st: &HeadState, id: &str) -> anyhow::Result<Vec<api::GrantUse>> {
    let rows = sqlx::query("SELECT at, by, what FROM grant_uses WHERE grant_id = ? ORDER BY at DESC LIMIT 500")
        .bind(id)
        .fetch_all(&st.store.pool)
        .await?;
    Ok(rows.iter().map(|r| api::GrantUse { at: r.get("at"), by: r.get("by"), what: r.get("what") }).collect())
}

/// The grants in force from realm `from` (or any realm) into `to`, of a
/// kind and mode.
async fn in_force(st: &HeadState, from: Option<&str>, to: &str, kind: &str, mode: &str) -> Vec<api::Grant> {
    sqlx::query(
        "SELECT g.*, 0 AS uses FROM grants g WHERE (?1 IS NULL OR from_realm = ?1) AND to_realm = ?2 AND kind = ?3 AND mode = ?4 \
         AND state = 'active' AND (expires_at IS NULL OR expires_at > ?5)",
    )
    .bind(from)
    .bind(to)
    .bind(kind)
    .bind(mode)
    .bind(now_ms())
    .fetch_all(&st.store.pool)
    .await
    .map(|rows| rows.iter().map(grant_of).collect())
    .unwrap_or_default()
}

async fn record_use(st: &HeadState, grant: &str, by: &str, what: &str) {
    if grant == COMMONS_READ {
        return;
    }
    if let Err(e) = sqlx::query("INSERT INTO grant_uses (grant_id, at, by, what) VALUES (?, ?, ?, ?)")
        .bind(grant)
        .bind(now_ms())
        .bind(by)
        .bind(what)
        .execute(&st.store.pool)
        .await
    {
        warn!("logging a grant's use: {e:#}");
    }
}

/// May `to` use realm `from`'s collection `name` this way? The grant (its
/// id and the filter it narrows reads to), and the use logged.
pub async fn collection(st: &HeadState, from: &str, to: &str, name: &str, mode: &str, by: &str) -> anyhow::Result<(String, Option<Value>)> {
    // Every realm reads commons, unless you turned that off for it.
    if from == COMMONS && mode == "read" && reads_commons(st, to).await {
        return Ok((COMMONS_READ.to_owned(), None));
    }
    let g = in_force(st, Some(from), to, "collection", mode).await.into_iter().find(|g| g.scope["collection"] == name);
    let Some(g) = g else {
        return Err(crate::forbidden(format!("realm {to} has no {mode} grant on {from}'s collection {name}")));
    };
    record_use(st, &g.id, by, &format!("{mode} {name}")).await;
    Ok((g.id, g.scope.get("where").filter(|w| w.is_object()).cloned()))
}

/// Realm `to`'s read grants on others' knowledge: (realm, kinds; empty:
/// every kind but spam).
pub async fn memex_reads(st: &HeadState, to: &str) -> Vec<(String, String, Vec<String>)> {
    let mut reads: Vec<(String, String, Vec<String>)> = in_force(st, None, to, "memex", "read")
        .await
        .into_iter()
        .map(|g| {
            let kinds =
                g.scope["kinds"].as_array().map(|a| a.iter().filter_map(|k| k.as_str().map(str::to_owned)).collect()).unwrap_or_default();
            (g.id, g.from_realm, kinds)
        })
        .collect();
    if to != COMMONS && !reads.iter().any(|(_, from, _)| from == COMMONS) && reads_commons(st, to).await {
        reads.push((COMMONS_READ.to_owned(), COMMONS.to_owned(), Vec::new()));
    }
    reads
}

/// The realms `from` lets read its knowledge of `kind`: (grant, realm).
pub async fn memex_readers(st: &HeadState, from: &str, kind: &str) -> Vec<(String, String)> {
    let rows = sqlx::query(
        "SELECT g.*, 0 AS uses FROM grants g WHERE from_realm = ? AND kind = 'memex' AND mode = 'read' AND state = 'active' \
         AND (expires_at IS NULL OR expires_at > ?)",
    )
    .bind(from)
    .bind(now_ms())
    .fetch_all(&st.store.pool)
    .await
    .unwrap_or_default();
    rows.iter()
        .map(grant_of)
        .filter(|g| g.scope["kinds"].as_array().is_none_or(|k| k.is_empty() || k.iter().any(|x| x == kind)))
        .map(|g| (g.id, g.to_realm))
        .collect()
}

/// Whether `realm` reads commons (you can turn it off per realm).
pub async fn reads_commons(st: &HeadState, realm: &str) -> bool {
    realm != COMMONS
        && realm != ROOT
        && sqlx::query_scalar::<_, i64>("SELECT reads_commons FROM realms WHERE id = ?")
            .bind(realm)
            .fetch_optional(&st.store.pool)
            .await
            .ok()
            .flatten()
            .is_some_and(|r| r != 0)
}

/// Whether realm `to` may publish to commons (records of `collection`, or
/// knowledge with `None`).
pub async fn may_publish(st: &HeadState, to: &str, collection: Option<&str>) -> Option<String> {
    let kind = if collection.is_some() { "collection" } else { "memex" };
    in_force(st, Some(COMMONS), to, kind, "append")
        .await
        .into_iter()
        .find(|g| collection.is_none_or(|c| g.scope["collection"] == c))
        .map(|g| g.id)
}

/// The realm whose operation `method` realm `to` may call through a grant.
pub async fn caller_of(st: &HeadState, to: &str, method: &str, by: &str) -> Option<String> {
    let g = in_force(st, None, to, "operations", "call")
        .await
        .into_iter()
        .find(|g| g.scope["methods"].as_array().is_some_and(|m| m.iter().any(|x| x == method)))?;
    record_use(st, &g.id, by, method).await;
    Some(g.from_realm)
}

/// Whether realm `to` may start realm `from`'s template (granted when an
/// app requiring `from`'s app was installed, docs/packages.md §3).
pub async fn may_start(st: &HeadState, from: &str, to: &str, template: &str, by: &str) -> bool {
    may_call(st, from, to, &format!("template:{template}"), by).await
}

/// Whether realm `to` may call `method` of realm `from` (an operations
/// grant naming it), logged on the grant when it may: `template:<name>`,
/// `op:<instance>.<name>` (docs/apps.md §7).
pub async fn may_call(st: &HeadState, from: &str, to: &str, method: &str, by: &str) -> bool {
    let method = method.to_owned();
    let Some(g) = in_force(st, Some(from), to, "operations", "call")
        .await
        .into_iter()
        .find(|g| g.scope["methods"].as_array().is_some_and(|m| m.iter().any(|x| x == method.as_str())))
    else {
        return false;
    };
    record_use(st, &g.id, by, &method).await;
    true
}

/// Whether realm `to`'s sessions may start realm `from`'s template where
/// it says (its workers, hosts and SSH targets), not only on their own
/// worker; logged on the grant when they may.
pub async fn may_reach(st: &HeadState, from: &str, to: &str, template: &str, by: &str) -> bool {
    let Some(g) = in_force(st, Some(from), to, "workers", "start").await.into_iter().find(|g| g.scope["template"] == template) else {
        return false;
    };
    record_use(st, &g.id, by, &format!("template:{template}")).await;
    true
}

pub async fn log_memex_read(st: &HeadState, grant: &str, by: &str, what: &str) {
    record_use(st, grant, by, what).await;
}

/// A snapshot: a copy of the records into the receiving realm's
/// collection of the same name (made like the source's if it has none),
/// each carrying where it came from.
async fn snapshot(st: &HeadState, g: &api::Grant) -> anyhow::Result<()> {
    let name = g.scope["collection"].as_str().unwrap_or_default();
    let source = crate::collections::get(st, &g.from_realm, name).await?;
    if crate::collections::get(st, &g.to_realm, name).await.is_err() {
        let c = api::CollectionIn {
            name: name.to_owned(),
            description: Some(format!("{} (a copy from realm {})", source.description, g.from_realm)),
            schema: Some(source.schema.clone()),
            keys: Some(Vec::new()),
            card: Some(source.card.clone()),
            handoff: false,
        };
        crate::collections::create(st, &g.to_realm, &c, &format!("grant:{}", g.id), false).await?;
    }
    let filter = g.scope.get("where").and_then(Value::as_object).cloned().unwrap_or_default();
    let page =
        crate::collections::query(st, &g.from_realm, name, &api::RecordQuery { filter, limit: Some(500), ..Default::default() }).await?;
    for r in &page.records {
        let mut links = r.links.clone();
        links.push(format!("record:{}/{}", g.from_realm, r.id));
        let mut labels = r.labels.clone();
        labels.extend([format!("from:{}", g.from_realm), format!("grant:{}", g.id)]);
        let copy = api::RecordIn {
            fields: r.fields.clone(),
            links: Some(links),
            labels: Some(labels),
            version: None,
            derived_from: Some(vec![format!("record:{}/{name}/{}", g.from_realm, r.id)]),
        };
        crate::collections::put(st, &g.to_realm, name, None, &copy, &format!("grant:{}", g.id)).await?;
    }
    record_use(st, &g.id, "you", &format!("snapshot of {} records of {name}", page.records.len())).await;
    Ok(())
}

/// An agent asks for a grant: it waits as `requested`, and comes to you as
/// a decision; allowing it puts it in force.
pub async fn request(st: &HeadState, session_id: &str, realm: &str, p: &Value) -> anyhow::Result<Value> {
    let g = api::GrantIn {
        from_realm: p["from_realm"].as_str().unwrap_or_default().to_owned(),
        to_realm: realm.to_owned(),
        kind: p["kind"].as_str().unwrap_or_default().to_owned(),
        scope: p["scope"].clone(),
        mode: p["mode"].as_str().unwrap_or_default().to_owned(),
        expires_at: None,
        note: p["why"].as_str().unwrap_or_default().to_owned(),
    };
    ask(st, &g, &format!("session:{session_id}")).await
}

/// A grant asked for by `source` (a session, an app): requested, with a
/// decision for you.
pub async fn ask(st: &HeadState, g: &api::GrantIn, source: &str) -> anyhow::Result<Value> {
    let grant = create(st, g, source, "requested").await?;
    let d = api::DecisionIn {
        kind: "approve".into(),
        title: format!("Let realm {} {} {}'s {}?", grant.to_realm, grant.mode, grant.from_realm, describe(&grant)),
        document: format!(
            "{} in realm **{}** asks for a grant: **{}** of realm **{}**'s {}.\n\n{}\n\nAllowing it lets that data flow into {}; \
             every use is logged, and you can revoke it under Settings → System → Grants.",
            if source.starts_with("app:") { "The app" } else { "A session" },
            grant.to_realm,
            grant.mode,
            grant.from_realm,
            describe(&grant),
            if grant.note.is_empty() { "(no reason given)".to_owned() } else { format!("Why: {}", grant.note) },
            grant.to_realm
        ),
        context: json!({"grant": grant.id, "session": source.strip_prefix("session:")}),
        source: source.to_owned(),
        priority: "high".into(),
        options: vec![
            api::DecisionOption { label: "Allow".into(), does: "The grant is in force.".into() },
            api::DecisionOption { label: "Deny".into(), does: "Nothing flows.".into() },
        ],
        ..Default::default()
    };
    let decision = crate::decisions::open(st, &d, &grant.to_realm, &format!("grant:{}", grant.id)).await?;
    Ok(json!({"grant": grant.id, "decision": decision.id}))
}

/// The answer to a grant request.
pub async fn answer_request(st: &HeadState, id: &str, allow: bool) -> anyhow::Result<()> {
    let g = get(st, id).await?.filter(|g| g.state == "requested").ok_or_else(|| bad_request("that grant is not waiting for an answer"))?;
    sqlx::query("UPDATE grants SET state = ? WHERE id = ?")
        .bind(if allow { "active" } else { "denied" })
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    if allow && g.mode == "snapshot" {
        snapshot(st, &api::Grant { state: "active".into(), ..g }).await?;
    }
    st.registry.bump();
    Ok(())
}

/// What a grant covers, in a few words.
pub fn describe(g: &api::Grant) -> String {
    match g.kind.as_str() {
        "collection" => format!("collection {}", g.scope["collection"].as_str().unwrap_or("?")),
        "memex" => match g.scope["kinds"].as_array().filter(|k| !k.is_empty()) {
            Some(k) => format!("knowledge ({})", k.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")),
            None => "knowledge".into(),
        },
        "workers" => format!("template {} on its workers, hosts and SSH targets", g.scope["template"].as_str().unwrap_or("?")),
        _ => format!(
            "operations {}",
            g.scope["methods"].as_array().map(|m| m.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default()
        ),
    }
}

/// Where a collection call lands: the realm, the filter its reads stay
/// within (a grant on some records), and the grant it went through.
pub struct Reach {
    pub realm: String,
    pub within: Option<serde_json::Map<String, Value>>,
    pub grant: Option<String>,
}

/// A collection call from realm `own` on `asked`'s collection `name`: in
/// its own realm freely, in another only through a grant for `mode`.
pub async fn reach(st: &HeadState, own: &str, asked: Option<&str>, name: &str, mode: &str, by: &str) -> anyhow::Result<Reach> {
    match asked.map(str::trim).filter(|a| !a.is_empty() && *a != own) {
        None => Ok(Reach { realm: own.to_owned(), within: None, grant: None }),
        Some(other) => {
            let (grant, within) = collection(st, other, own, name, mode, by).await?;
            Ok(Reach { realm: other.to_owned(), within: within.and_then(|w| w.as_object().cloned()), grant: Some(grant) })
        }
    }
}

/// A query kept within a grant's filter (both must hold).
pub fn narrow(q: &mut api::RecordQuery, within: &serde_json::Map<String, Value>) {
    q.within = within.clone();
}

/// A record read through a grant on some records: refused unless it is one
/// of them.
pub async fn check_within(st: &HeadState, r: &Reach, name: &str, id: &str) -> anyhow::Result<()> {
    let Some(within) = &r.within else { return Ok(()) };
    let mut q = api::RecordQuery { limit: Some(1), ..Default::default() };
    q.filter.insert("id".into(), json!(id));
    narrow(&mut q, within);
    if crate::collections::query(st, &r.realm, name, &q).await?.records.is_empty() {
        return Err(not_found(format!("no record {id} in {name}")));
    }
    Ok(())
}

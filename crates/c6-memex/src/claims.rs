//! Claims (docs/apps.md §9): every edge of the graph is a claim with an
//! owner. Anyone may add their own about anything they can read; only the
//! owner retracts its claims (into history, not away); disagreeing is a
//! claim of your own. The graph doesn't judge: a reader's policy says,
//! per label, whose claims to believe (the head passes the realm's).
//!
//! Identity is claims too: `same_as` and `not_same_as` between things,
//! followed by readers that ask to.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use c6_api as api;
use serde_json::{Value, json};
use sqlx::Row;

use crate::Memex;

/// Claims one owner may add in an hour, so a faulty app can't flood the graph.
const PER_HOUR: i64 = 20_000;

/// Without a policy for a label: yours first, then everyone's.
const DEFAULT_BELIEVE: [&str; 2] = ["you", "*"];

/// One end of a claim: a thing (`thing:<key>`, a key, or an id) or an item
/// (`item:<id>`).
enum End {
    Thing(String),
    Item(String),
}

async fn end(m: &Memex, s: &str) -> anyhow::Result<End> {
    if let Some(id) = s.strip_prefix("item:") {
        let found: Option<String> = sqlx::query_scalar("SELECT id FROM items WHERE id = ?").bind(id).fetch_optional(&m.store.pool).await?;
        return found.map(End::Item).ok_or_else(|| anyhow::anyhow!("no item {id}"));
    }
    let key = s.strip_prefix("thing:").unwrap_or(s);
    m.store
        .thing(key)
        .await?
        .map(|t| End::Thing(t.id))
        .ok_or_else(|| anyhow::anyhow!("no thing {key}: make it first (memex.things.create)"))
}

/// Adds a claim of `owner`'s.
pub async fn add(m: &Memex, p: &Value) -> anyhow::Result<Value> {
    let owner = p["owner"].as_str().filter(|o| !o.is_empty()).unwrap_or("you").to_owned();
    let made_by = p["made_by"].as_str().unwrap_or(&owner).to_owned();
    let label = p["label"].as_str().unwrap_or_default().trim().to_owned();
    anyhow::ensure!(
        !label.is_empty() && label.len() <= 64 && label.chars().all(|c| c.is_ascii_alphanumeric() || "_-:.".contains(c)),
        "a claim's label is letters, digits, _ - : and ."
    );
    let recent: i64 = sqlx::query_scalar("SELECT count(*) FROM links WHERE owner = ? AND created_at > ?")
        .bind(&owner)
        .bind(c6_core::now_ms() - 3_600_000)
        .fetch_one(&m.store.pool)
        .await?;
    anyhow::ensure!(recent < PER_HOUR, "{owner} made {recent} claims in the last hour, its limit");
    let (from, to) = (end(m, p["from"].as_str().unwrap_or_default()).await?, end(m, p["to"].as_str().unwrap_or_default()).await?);
    let (src_thing, src_item) = match &from {
        End::Thing(t) => (Some(t.clone()), None),
        End::Item(i) => (None, Some(i.clone())),
    };
    let (dst_thing, dst_item) = match &to {
        End::Thing(t) => (Some(t.clone()), None),
        End::Item(i) => (None, Some(i.clone())),
    };
    let confidence = p["confidence"].as_f64().unwrap_or(1.0).clamp(0.0, 1.0);
    let basis = if confidence < 1.0 || p["basis"] == "inferred" { "inferred" } else { "observed" };
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO links (src_thing, src_item, rel, dst_thing, dst_item, basis, confidence, made_by, owner, created_at, realm) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(&src_thing)
    .bind(&src_item)
    .bind(&label)
    .bind(&dst_thing)
    .bind(&dst_item)
    .bind(basis)
    .bind(confidence)
    .bind(&made_by)
    .bind(&owner)
    .bind(c6_core::now_ms())
    .bind(&m.realm)
    .fetch_one(&m.store.pool)
    .await?;
    for e in p["evidence"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let item = e.strip_prefix("item:").unwrap_or(e);
        sqlx::query("INSERT INTO evidence (link_id, item_id) VALUES (?, ?)").bind(id).bind(item).execute(&m.store.pool).await?;
    }
    Ok(json!(one(m, id).await?))
}

/// Retracts a claim: only its owner may. It moves into history.
pub async fn retract(m: &Memex, p: &Value) -> anyhow::Result<Value> {
    let id = p["id"].as_i64().ok_or_else(|| anyhow::anyhow!("which claim (id)"))?;
    let by = p["owner"].as_str().filter(|o| !o.is_empty()).unwrap_or("you");
    let owner: Option<String> = sqlx::query_scalar("SELECT owner FROM links WHERE id = ?").bind(id).fetch_optional(&m.store.pool).await?;
    let owner = owner.ok_or_else(|| anyhow::anyhow!("no claim {id}"))?;
    anyhow::ensure!(owner == by, "claim {id} is {owner}'s: only its owner retracts it (add a claim of your own instead)");
    move_out(m, "id = ?", &json!(id), by).await?;
    Ok(json!({"retracted": id}))
}

/// Retracts every claim an owner made (an app instance removed).
pub async fn retract_owner(m: &Memex, owner: &str) -> anyhow::Result<Value> {
    anyhow::ensure!(!owner.is_empty(), "whose claims");
    let n = move_out(m, "owner = ?", &json!(owner), owner).await?;
    Ok(json!({"retracted": n}))
}

async fn move_out(m: &Memex, cond: &str, arg: &Value, by: &str) -> anyhow::Result<u64> {
    let bind = |q: sqlx::query::Query<'static, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>| match arg {
        Value::Number(n) => q.bind(n.as_i64()),
        v => q.bind(v.as_str().unwrap_or_default().to_owned()),
    };
    let mut tx = m.store.pool.begin().await?;
    bind(sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO retracted_links (id, src_thing, src_item, rel, dst_thing, dst_item, basis, confidence, made_by, owner, created_at, \
         retracted_by, retracted_at) SELECT id, src_thing, src_item, rel, dst_thing, dst_item, basis, confidence, made_by, owner, created_at, \
         '{}', {} FROM links WHERE {cond}",
        by.replace('\'', "''"),
        c6_core::now_ms()
    ))))
    .execute(&mut *tx)
    .await?;
    bind(sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM evidence WHERE link_id IN (SELECT id FROM links WHERE {cond})"))))
        .execute(&mut *tx)
        .await?;
    let n = bind(sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM links WHERE {cond}")))).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(n)
}

/// A thing anyone may make: a person, a place, a topic.
pub async fn create_thing(m: &Memex, p: &Value) -> anyhow::Result<Value> {
    let kind = p["kind"].as_str().unwrap_or("topic").trim();
    let name = p["name"].as_str().unwrap_or_default().trim();
    let key = p["key"]
        .as_str()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{kind}:{}", name.to_lowercase().split_whitespace().collect::<Vec<_>>().join("-")));
    anyhow::ensure!(!name.is_empty() || key.contains(':'), "a thing has a name or a key");
    let id = m.store.ensure_thing(&key, kind, name).await?;
    Ok(json!(m.store.thing(&id).await?))
}

fn end_name(thing: Option<(String, String)>, item: Option<(String, String)>) -> (String, String) {
    match (thing, item) {
        (Some((key, name)), _) => (format!("thing:{key}"), name),
        (_, Some((id, title))) => (format!("item:{id}"), title),
        _ => (String::new(), String::new()),
    }
}

const SELECT: &str = "SELECT l.id, l.rel, l.owner, l.made_by, l.basis, l.confidence, l.created_at, \
     st.key AS st_key, st.name AS st_name, si.id AS si_id, si.title AS si_title, \
     dt.key AS dt_key, dt.name AS dt_name, di.id AS di_id, di.title AS di_title, \
     (SELECT json_group_array(item_id) FROM evidence e WHERE e.link_id = l.id) AS ev \
     FROM links l LEFT JOIN things st ON st.id = l.src_thing LEFT JOIN items si ON si.id = l.src_item \
     LEFT JOIN things dt ON dt.id = l.dst_thing LEFT JOIN items di ON di.id = l.dst_item";

fn claim_of(r: &sqlx::sqlite::SqliteRow) -> api::Claim {
    let pair = |a: &str, b: &str| r.get::<Option<String>, _>(a).map(|x| (x, r.get::<Option<String>, _>(b).unwrap_or_default()));
    let (from, from_name) = end_name(pair("st_key", "st_name"), pair("si_id", "si_title"));
    let (to, to_name) = end_name(pair("dt_key", "dt_name"), pair("di_id", "di_title"));
    api::Claim {
        id: r.get("id"),
        from,
        from_name,
        label: r.get("rel"),
        to,
        to_name,
        owner: r.get("owner"),
        made_by: r.get("made_by"),
        basis: r.get("basis"),
        confidence: r.get("confidence"),
        evidence: serde_json::from_str(&r.get::<String, _>("ev")).unwrap_or_default(),
        created_at: r.get("created_at"),
        means: String::new(),
        believed: true,
    }
}

async fn one(m: &Memex, id: i64) -> anyhow::Result<api::Claim> {
    let r = sqlx::query(sqlx::AssertSqlSafe(format!("{SELECT} WHERE l.id = ?"))).bind(id).fetch_one(&m.store.pool).await?;
    Ok(claim_of(&r))
}

/// Whether `owner` matches a believe entry (`app:*` matches every app).
fn matches(entry: &str, owner: &str) -> bool {
    entry == "*" || entry == owner || entry.strip_suffix('*').is_some_and(|p| owner.starts_with(p))
}

/// Marks what a policy believes: per (from, label), the claims of the
/// first entry of `believe` that has any (all claims for `*`).
pub fn apply(claims: &mut [api::Claim], policies: &[api::ClaimPolicy]) {
    let mut groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for (i, c) in claims.iter().enumerate() {
        groups.entry((c.from.clone(), c.label.clone())).or_default().push(i);
    }
    for ((_, label), idx) in groups {
        let believe: Vec<String> = policies
            .iter()
            .find(|p| p.label == label && p.enabled != Some(false))
            .map(|p| p.believe.clone())
            .unwrap_or_else(|| DEFAULT_BELIEVE.iter().map(|s| (*s).to_owned()).collect());
        let mut chosen: Option<Vec<usize>> = None;
        let mut taken: HashSet<usize> = HashSet::new();
        for entry in &believe {
            if entry == "*" {
                chosen = Some(idx.iter().copied().filter(|i| !taken.contains(i)).collect());
                break;
            }
            let these: Vec<usize> = idx.iter().copied().filter(|i| matches(entry, &claims[*i].owner)).collect();
            if !these.is_empty() {
                chosen = Some(these);
                break;
            }
            taken.extend(idx.iter().copied().filter(|i| matches(entry, &claims[*i].owner)));
        }
        let chosen: HashSet<usize> = chosen.unwrap_or_default().into_iter().collect();
        for i in idx {
            claims[i].believed = chosen.contains(&i);
        }
    }
}

/// Things a reader takes to be the same as `thing`, by believed `same_as`
/// claims (both ways, transitively), less believed `not_same_as`.
async fn same_as(m: &Memex, thing: &str, policies: &[api::ClaimPolicy]) -> anyhow::Result<Vec<String>> {
    let mut seen: BTreeSet<String> = BTreeSet::from([thing.to_owned()]);
    let mut queue = vec![thing.to_owned()];
    while let Some(t) = queue.pop() {
        if seen.len() > 50 {
            break;
        }
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{SELECT} WHERE l.rel IN ('same_as', 'not_same_as') AND (l.src_thing = ?1 OR l.dst_thing = ?1)"
        )))
        .bind(&t)
        .fetch_all(&m.store.pool)
        .await?;
        let mut claims: Vec<api::Claim> = rows.iter().map(claim_of).collect();
        apply(&mut claims, policies);
        let pair = |a: &str, b: &str| if a < b { (a.to_owned(), b.to_owned()) } else { (b.to_owned(), a.to_owned()) };
        let not: HashSet<(String, String)> =
            claims.iter().filter(|c| c.believed && c.label == "not_same_as").map(|c| pair(&c.from, &c.to)).collect();
        for c in claims.iter().filter(|c| c.believed && c.label == "same_as") {
            if not.contains(&pair(&c.from, &c.to)) {
                continue;
            }
            for e in [&c.from, &c.to] {
                if let Some(key) = e.strip_prefix("thing:")
                    && let Some(t) = m.store.thing(key).await?
                    && seen.insert(t.id.clone())
                {
                    queue.push(t.id);
                }
            }
        }
    }
    Ok(seen.into_iter().collect())
}

/// Claims about a thing or an item (from it, or to it), with whether the
/// reader's policies believe each: `policy` `default` (only what they
/// believe) or `all`; `label`, `owner` to narrow; `same_as` to include the
/// things taken to be the same.
pub async fn list(m: &Memex, p: &Value) -> anyhow::Result<Value> {
    let policies: Vec<api::ClaimPolicy> = serde_json::from_value(p["policies"].clone()).unwrap_or_default();
    let about = p["about"].as_str().unwrap_or_default();
    let mut things: Vec<String> = Vec::new();
    let mut items: Vec<String> = Vec::new();
    match end(m, about).await? {
        End::Thing(t) => {
            things = if p["same_as"].as_bool().unwrap_or(false) { same_as(m, &t, &policies).await? } else { vec![t] };
        }
        End::Item(i) => items.push(i),
    }
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE (l.src_thing IN (SELECT value FROM json_each(?1)) OR l.dst_thing IN (SELECT value FROM json_each(?1)) \
         OR l.src_item IN (SELECT value FROM json_each(?2)) OR l.dst_item IN (SELECT value FROM json_each(?2))) \
         AND (?3 = '' OR l.rel = ?3) AND (?4 = '' OR l.owner = ?4) ORDER BY l.rel, l.created_at DESC LIMIT 500"
    )))
    .bind(json!(things).to_string())
    .bind(json!(items).to_string())
    .bind(p["label"].as_str().unwrap_or(""))
    .bind(p["owner"].as_str().unwrap_or(""))
    .fetch_all(&m.store.pool)
    .await?;
    let mut claims: Vec<api::Claim> = rows.iter().map(claim_of).collect();
    apply(&mut claims, &policies);
    if p["policy"].as_str() != Some("all") {
        claims.retain(|c| c.believed);
    }
    Ok(json!(claims))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: i64, from: &str, label: &str, owner: &str) -> api::Claim {
        api::Claim { id, from: from.into(), label: label.into(), owner: owner.into(), ..Default::default() }
    }

    #[test]
    fn policies_choose_whose_claims_to_believe() {
        // By default yours win; without yours, everyone's stand.
        let mut v = vec![c(1, "item:a", "sent_by", "gmail"), c(2, "item:a", "sent_by", "you"), c(3, "item:b", "sent_by", "gmail")];
        apply(&mut v, &[]);
        assert_eq!(v.iter().map(|c| c.believed).collect::<Vec<_>>(), vec![false, true, true]);
        // A realm's policy: gmail's first, then any app's.
        let p = vec![api::ClaimPolicy { label: "sent_by".into(), believe: vec!["gmail".into(), "app:*".into()], enabled: None }];
        let mut v = vec![
            c(1, "item:a", "sent_by", "app:crm"),
            c(2, "item:a", "sent_by", "gmail"),
            c(3, "item:b", "sent_by", "app:crm"),
            c(4, "item:c", "sent_by", "realm"),
        ];
        apply(&mut v, &p);
        assert_eq!(v.iter().map(|c| c.believed).collect::<Vec<_>>(), vec![false, true, true, false]);
        // Other labels keep the default.
        let mut v = vec![c(1, "item:a", "about", "app:crm"), c(2, "item:a", "about", "realm")];
        apply(&mut v, &p);
        assert!(v.iter().all(|c| c.believed));
    }
}

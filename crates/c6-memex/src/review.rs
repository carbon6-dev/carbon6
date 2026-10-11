//! Things for you to decide, and the graph around a thing.
//!
//! The memex never merges people on its own: two things with the same name
//! (an author on a paper and someone you write to, an address you used to
//! have) become a suggestion, and you merge or dismiss it. Faces to name
//! come here too (see media).

use c6_api as api;
use serde_json::json;
use sqlx::Row;

use crate::store::Store;

/// Suggests merging people with the same name (made since the last look).
pub async fn suggest(store: &Store) -> anyhow::Result<u64> {
    let r = sqlx::query(
        "INSERT OR IGNORE INTO review (kind, a, b, reason, created_at) \
         SELECT 'merge', a.id, b.id, 'same name: ' || a.name, ? FROM things a JOIN things b \
         ON b.kind = a.kind AND b.name = a.name COLLATE NOCASE AND a.id < b.id \
         WHERE a.kind = 'person' AND a.name NOT LIKE '%@%' AND length(a.name) > 3 AND instr(a.name, ' ') > 0",
    )
    .bind(c6_core::now_ms())
    .execute(&store.pool)
    .await?;
    Ok(r.rows_affected())
}

/// Adds a suggestion (another part of the memex found one).
pub async fn add(store: &Store, kind: &str, a: &str, b: &str, reason: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT OR IGNORE INTO review (kind, a, b, reason, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(kind)
        .bind(a)
        .bind(b)
        .bind(reason)
        .bind(c6_core::now_ms())
        .execute(&store.pool)
        .await?;
    Ok(())
}

pub async fn open_count(store: &Store) -> anyhow::Result<u64> {
    Ok(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM review WHERE status = 'open'").fetch_one(&store.pool).await? as u64)
}

/// Open suggestions, oldest first.
pub async fn list(store: &Store, limit: u32) -> anyhow::Result<Vec<api::MemexReview>> {
    let rows = sqlx::query("SELECT * FROM review WHERE status = 'open' ORDER BY created_at, id LIMIT ?")
        .bind(limit as i64)
        .fetch_all(&store.pool)
        .await?;
    let mut out = Vec::new();
    for r in rows {
        let kind: String = r.get("kind");
        let (a, b): (String, String) = (r.get("a"), r.get("b"));
        let (thing_a, thing_b, picture) = if kind == "face" {
            // a: the face cluster's thing; b: a person it may be ("" for none).
            let picture = Some(format!("face/{a}"));
            (store.thing(&a).await?, if b.is_empty() { None } else { store.thing(&b).await? }, picture)
        } else {
            (store.thing(&a).await?, store.thing(&b).await?, None)
        };
        let Some(thing_a) = thing_a else {
            // Gone since (merged away): no longer a question.
            sqlx::query("UPDATE review SET status = 'stale' WHERE id = ?").bind(r.get::<i64, _>("id")).execute(&store.pool).await?;
            continue;
        };
        if kind == "merge" && thing_b.is_none() {
            sqlx::query("UPDATE review SET status = 'stale' WHERE id = ?").bind(r.get::<i64, _>("id")).execute(&store.pool).await?;
            continue;
        }
        out.push(api::MemexReview {
            id: r.get("id"),
            kind,
            a: thing_a,
            b: thing_b,
            reason: r.get("reason"),
            picture,
            created_at: r.get("created_at"),
        });
    }
    Ok(out)
}

/// Decides a suggestion: `accept` merges (a into b) or, for a face, names
/// it (`name`, or as b); anything else dismisses it.
pub async fn decide(store: &Store, id: i64, accept: bool, name: Option<&str>) -> anyhow::Result<()> {
    let r = sqlx::query("SELECT * FROM review WHERE id = ? AND status = 'open'")
        .bind(id)
        .fetch_optional(&store.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no open suggestion {id}"))?;
    let (kind, a, b): (String, String, String) = (r.get("kind"), r.get("a"), r.get("b"));
    if accept {
        match kind.as_str() {
            "merge" => merge(store, &a, &b).await?,
            "face" => {
                let name = name.map(str::trim).filter(|n| !n.is_empty());
                let person = match (name, b.is_empty()) {
                    // A name: the person called that (made if need be).
                    (Some(n), _) => match person_named(store, n).await? {
                        Some(p) => p,
                        None => store.ensure_thing(&format!("person:name:{}", n.to_lowercase()), "person", n).await?,
                    },
                    (None, false) => b.clone(),
                    (None, true) => anyhow::bail!("name the face"),
                };
                merge(store, &a, &person).await?;
            }
            other => anyhow::bail!("unknown suggestion kind {other}"),
        }
    }
    sqlx::query("UPDATE review SET status = ? WHERE id = ?")
        .bind(if accept { "accepted" } else { "dismissed" })
        .bind(id)
        .execute(&store.pool)
        .await?;
    Ok(())
}

/// The person with this name, if there is just one.
async fn person_named(store: &Store, name: &str) -> anyhow::Result<Option<String>> {
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM things WHERE kind = 'person' AND lower(name) = lower(?) LIMIT 2")
        .bind(name)
        .fetch_all(&store.pool)
        .await?;
    Ok(if ids.len() == 1 { ids.into_iter().next() } else { None })
}

/// Makes `from` part of `into`: its links, keys and name (as an alias) move,
/// and `from` is gone.
pub async fn merge(store: &Store, from: &str, into: &str) -> anyhow::Result<()> {
    anyhow::ensure!(from != into, "a thing cannot merge into itself");
    let mut tx = store.pool.begin().await?;
    let (name, key): (String, Option<String>) = sqlx::query_as("SELECT name, key FROM things WHERE id = ?")
        .bind(from)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no thing {from}"))?;
    let into_kind: Option<String> = sqlx::query_scalar("SELECT kind FROM things WHERE id = ?").bind(into).fetch_optional(&mut *tx).await?;
    anyhow::ensure!(into_kind.is_some(), "no thing {into}");
    sqlx::query("UPDATE links SET dst_thing = ?1 WHERE dst_thing = ?2").bind(into).bind(from).execute(&mut *tx).await?;
    sqlx::query("UPDATE links SET src_thing = ?1 WHERE src_thing = ?2").bind(into).bind(from).execute(&mut *tx).await?;
    // A thing does not link to itself.
    sqlx::query("DELETE FROM links WHERE src_thing = ?1 AND dst_thing = ?1").bind(into).execute(&mut *tx).await?;
    sqlx::query("UPDATE thing_keys SET thing_id = ?1 WHERE thing_id = ?2").bind(into).bind(from).execute(&mut *tx).await?;
    if let Some(k) = key {
        sqlx::query("INSERT OR REPLACE INTO thing_keys (key, thing_id) VALUES (?, ?)").bind(k).bind(into).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE trail_steps SET target = ?1 WHERE target = ?2").bind(into).bind(from).execute(&mut *tx).await?;
    // Its name, as an alias (and the name, if `into` had none but an address).
    sqlx::query(
        "UPDATE things SET aliases = json_insert(aliases, '$[#]', ?1), \
         name = CASE WHEN name LIKE '%@%' AND ?1 NOT LIKE '%@%' THEN ?1 ELSE name END WHERE id = ?2",
    )
    .bind(&name)
    .bind(into)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE review SET status = 'stale' WHERE status = 'open' AND (a = ?1 OR b = ?1) AND kind = 'merge'")
        .bind(from)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM things WHERE id = ?").bind(from).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// The things around a thing: those it shares items with (of `kinds`) and
/// those it links to directly, with the links among them.
pub async fn graph(store: &Store, key: &str, kinds: &[String], limit: u32) -> anyhow::Result<api::MemexGraph> {
    let center = store.thing(key).await?.ok_or_else(|| anyhow::anyhow!("no thing {key:?}"))?;
    let kinds_json = serde_json::to_string(kinds)?;
    let shared = sqlx::query(
        "SELECT t.*, COUNT(DISTINCT a.src_item) AS shared, MAX(i.created_at) AS last FROM links a JOIN links b ON b.src_item = a.src_item \
         JOIN things t ON t.id = b.dst_thing JOIN items i ON i.id = a.src_item \
         WHERE a.dst_thing = ?1 AND b.dst_thing != ?1 AND b.rel != 'cites' AND t.kind != 'claim' AND i.kind IN (SELECT value FROM json_each(?2)) \
         GROUP BY t.id ORDER BY shared DESC LIMIT ?3",
    )
    .bind(&center.id)
    .bind(&kinds_json)
    .bind(limit as i64)
    .fetch_all(&store.pool)
    .await?;
    let mut nodes: Vec<api::MemexGraphNode> = shared
        .iter()
        .map(|r| api::MemexGraphNode {
            thing: api::MemexThingRef {
                id: r.get("id"),
                kind: r.get("kind"),
                name: r.get("name"),
                label: r.get::<Option<String>, _>("key").unwrap_or_default(),
            },
            rels: Vec::new(),
            shared: r.get::<i64, _>("shared") as u32,
            last: r.get("last"),
        })
        .collect();
    let direct = sqlx::query(
        "SELECT t.*, l.rel, l.src_thing = ?1 AS outgoing FROM links l JOIN things t ON t.id = CASE WHEN l.src_thing = ?1 THEN l.dst_thing ELSE l.src_thing END \
         WHERE (l.src_thing = ?1 AND l.dst_thing IS NOT NULL) OR l.dst_thing = ?1 AND l.src_thing IS NOT NULL LIMIT 200",
    )
    .bind(&center.id)
    .fetch_all(&store.pool)
    .await?;
    for r in &direct {
        let id: String = r.get("id");
        let rel: String = r.get("rel");
        let rel = if r.get::<bool, _>("outgoing") { rel } else { format!("{rel} ←") };
        match nodes.iter_mut().find(|n| n.thing.id == id) {
            Some(n) if !n.rels.contains(&rel) => n.rels.push(rel),
            Some(_) => {}
            None => nodes.push(api::MemexGraphNode {
                thing: api::MemexThingRef {
                    id,
                    kind: r.get("kind"),
                    name: r.get("name"),
                    label: r.get::<Option<String>, _>("key").unwrap_or_default(),
                },
                rels: vec![rel],
                shared: 0,
                last: None,
            }),
        }
    }
    let ids = serde_json::to_string(&nodes.iter().map(|n| n.thing.id.clone()).collect::<Vec<_>>())?;
    let edges = sqlx::query(
        "SELECT a.dst_thing AS x, b.dst_thing AS y, COUNT(DISTINCT a.src_item) AS n FROM links a JOIN links b ON b.src_item = a.src_item \
         AND a.dst_thing < b.dst_thing JOIN items i ON i.id = a.src_item \
         WHERE a.dst_thing IN (SELECT value FROM json_each(?1)) AND b.dst_thing IN (SELECT value FROM json_each(?1)) \
         AND i.kind IN (SELECT value FROM json_each(?2)) GROUP BY 1, 2 HAVING n > 1 ORDER BY n DESC LIMIT 300",
    )
    .bind(&ids)
    .bind(&kinds_json)
    .fetch_all(&store.pool)
    .await?;
    Ok(api::MemexGraph { center, nodes, edges: edges.iter().map(|r| (r.get("x"), r.get("y"), r.get::<i64, _>("n") as u32)).collect() })
}

/// A thing's items by month: (YYYY-MM, kind, count), for its timeline.
pub async fn histogram(store: &Store, key: &str, kinds: &[String]) -> anyhow::Result<serde_json::Value> {
    let thing = store.thing(key).await?.ok_or_else(|| anyhow::anyhow!("no thing {key:?}"))?;
    let rows = sqlx::query(
        "SELECT strftime('%Y-%m', i.created_at / 1000, 'unixepoch') AS month, i.kind, COUNT(DISTINCT i.id) AS n FROM items i \
         JOIN links l ON l.src_item = i.id WHERE l.dst_thing = ? AND i.kind IN (SELECT value FROM json_each(?)) GROUP BY 1, 2 ORDER BY 1",
    )
    .bind(&thing.id)
    .bind(serde_json::to_string(kinds)?)
    .fetch_all(&store.pool)
    .await?;
    Ok(json!(
        rows.iter()
            .map(|r| json!({"month": r.get::<String, _>("month"), "kind": r.get::<String, _>("kind"), "n": r.get::<i64, _>("n")}))
            .collect::<Vec<_>>()
    ))
}

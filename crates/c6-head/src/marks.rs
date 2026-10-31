//! Marks (docs/marks.md): the colour, plain or in a pattern, that each
//! value of a session's aspects wears wherever it shows. Workers and open
//! work and projects get one when first seen, the one their kind uses least
//! (so your few workers never share one); realms, repos and models keep the one
//! worked out from their name until you pick one.

use c6_api::marks::{self, Marks};
use c6_core::now_ms;
use sqlx::Row;

use crate::realms::Caller;
use crate::{HeadState, bad_request, not_found};

pub fn not_a_mark(mark: &str) -> String {
    format!(
        "{mark:?} is not a mark: a colour ({}), alone or as <colour>/<pattern> ({})",
        marks::COLOURS.join(", "),
        marks::PATTERNS.join(", ")
    )
}

/// Every mark the caller may see, after giving workers, open work and
/// projects theirs. A realm sees its own work's and projects', realms its
/// caller may name, and the rest.
pub async fn book(st: &HeadState, caller: &Caller) -> anyhow::Result<Marks> {
    assign_missing(st).await?;
    let rows = sqlx::query("SELECT key, mark FROM marks").fetch_all(&st.store.pool).await?;
    let mut out = Marks::new();
    let mine: Option<std::collections::HashSet<String>> = match &caller.realm {
        None => None,
        Some(r) => Some(
            sqlx::query_scalar::<_, String>(
                "SELECT CASE collection WHEN 'work' THEN 'work:' ELSE 'project:' END || id FROM records \
                 WHERE collection IN ('work', 'projects') AND realm = ?",
            )
            .bind(r)
            .fetch_all(&st.store.pool)
            .await?
            .into_iter()
            .collect(),
        ),
    };
    for r in rows {
        let (key, mark): (String, String) = (r.get("key"), r.get("mark"));
        let shown = match (key.split_once(':'), &caller.realm, &mine) {
            // Your browser names the other realms too.
            (Some(("realm", realm)), Some(own), _) => realm == own || caller.sees_others(),
            (Some(("work" | "project", _)), _, Some(mine)) => mine.contains(&key),
            _ => true,
        };
        if shown {
            out.insert(key, mark);
        }
    }
    Ok(out)
}

/// Gives workers, open work and live projects without a mark the one their
/// kind uses least: workers (that run sessions) among workers, work among
/// open work, projects among those not done or dropped.
async fn assign_missing(st: &HeadState) -> anyhow::Result<()> {
    // Each kind's keys in the order they came, with the mark each has.
    const KINDS: [&str; 3] = [
        "SELECT k.key, m.mark FROM (SELECT 'worker:' || id AS key, enrolled_at AS at FROM workers WHERE role NOT IN ('head', 'memex')) k \
         LEFT JOIN marks m ON m.key = k.key ORDER BY k.at, k.key",
        "SELECT k.key, m.mark FROM (SELECT 'work:' || id AS key, created_at AS at FROM records \
         WHERE collection = 'work' AND json_extract(fields, '$.status') = 'open') k LEFT JOIN marks m ON m.key = k.key ORDER BY k.at, k.key",
        "SELECT k.key, m.mark FROM (SELECT 'project:' || id AS key, created_at AS at FROM records \
         WHERE collection = 'projects' AND coalesce(json_extract(fields, '$.status'), 'active') IN ('active', 'paused')) k \
         LEFT JOIN marks m ON m.key = k.key ORDER BY k.at, k.key",
    ];
    for query in KINDS {
        let rows = sqlx::query(query).fetch_all(&st.store.pool).await?;
        let mut taken = Vec::new();
        let mut missing = Vec::new();
        for r in rows {
            match r.get::<Option<String>, _>("mark") {
                Some(m) => taken.push(m),
                None => missing.push(r.get::<String, _>("key")),
            }
        }
        for k in missing {
            let m = marks::pick(&taken);
            sqlx::query("INSERT OR IGNORE INTO marks (key, mark, updated_at) VALUES (?, ?, ?)")
                .bind(&k)
                .bind(&m)
                .bind(now_ms())
                .execute(&st.store.pool)
                .await?;
            taken.push(m);
        }
    }
    Ok(())
}

/// Gives a value its mark, or (an empty mark) takes it away. A realm marks
/// its own realm and work, and the shared things it uses.
pub async fn set(st: &HeadState, caller: &Caller, m: &marks::SetMark) -> anyhow::Result<Marks> {
    let key = m.key.trim();
    if !marks::is_key(key) {
        return Err(bad_request(format!("{key:?} is not a mark's key: <aspect>:<value>, an aspect one of {}", marks::ASPECTS.join(", "))));
    }
    if !m.mark.is_empty() && !marks::is_mark(&m.mark) {
        return Err(bad_request(not_a_mark(&m.mark)));
    }
    match key.split_once(':') {
        Some(("realm", r)) if !caller.sees(r) => return Err(not_found(format!("no realm {r}"))),
        Some((aspect @ ("work" | "project"), id)) => {
            let collection = if aspect == "work" { crate::work::WORK } else { crate::projects::PROJECTS };
            let realm: Option<String> = sqlx::query_scalar("SELECT realm FROM records WHERE collection = ? AND id = ?")
                .bind(collection)
                .bind(id)
                .fetch_optional(&st.store.pool)
                .await?;
            if !realm.is_some_and(|r| caller.sees(&r)) {
                return Err(not_found(format!("no {aspect} {id}")));
            }
        }
        _ => {}
    }
    if m.mark.is_empty() {
        sqlx::query("DELETE FROM marks WHERE key = ?").bind(key).execute(&st.store.pool).await?;
    } else {
        sqlx::query("INSERT INTO marks (key, mark, updated_at) VALUES (?, ?, ?) ON CONFLICT (key) DO UPDATE SET mark = excluded.mark, updated_at = excluded.updated_at")
            .bind(key)
            .bind(&m.mark)
            .bind(now_ms())
            .execute(&st.store.pool)
            .await?;
    }
    st.registry.bump();
    book(st, caller).await
}

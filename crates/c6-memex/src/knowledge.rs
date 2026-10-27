//! Environment knowledge (docs/follow-through.md §6): what agents learn
//! about the systems around the code, kept as facts with how each is known
//! (status), how long it holds (volatility), where it applies (scope), how
//! to re-check it, and the evidence. Agents write everything they figure
//! out; nothing waits for the user: disputes become action items on the
//! head.

use c6_api as api;
use serde_json::{Value, json};
use sqlx::Row;

use crate::Memex;
use crate::store::clip;

/// Slow facts are due for a re-check after this long.
const SLOW_DAYS: i64 = 30;
const DAY_MS: i64 = 86_400_000;
const STATUSES: [&str; 6] = ["reviewed", "verified", "observed", "inferred", "disputed", "retired"];

/// A claim, normalized for spotting duplicates.
pub fn key_of(claim: &str) -> String {
    claim
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '.' && c != '/' && c != '-' && c != '_')
        .map(|w| w.trim_matches('.'))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.trim().to_owned())).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

fn fact_of(r: &sqlx::sqlite::SqliteRow, now: i64) -> api::Fact {
    let volatility: String = r.get("volatility");
    let status: String = r.get("status");
    let last: Option<i64> = r.get("last_verified");
    let created: i64 = r.get("created_at");
    let check_cmd: String = r.get("check_cmd");
    let valid_to: Option<i64> = r.get("valid_to");
    let due = valid_to.is_none()
        && volatility == "slow"
        && !matches!(status.as_str(), "disputed" | "retired")
        && now - last.unwrap_or(created) > SLOW_DAYS * DAY_MS;
    api::Fact {
        id: r.get("id"),
        claim: r.get("claim"),
        about: serde_json::from_str(&r.get::<String, _>("about")).unwrap_or_default(),
        status,
        volatility,
        labels: serde_json::from_str(&r.get::<String, _>("labels")).unwrap_or_default(),
        conditions: r.get("conditions"),
        negative: r.get::<i64, _>("negative") != 0,
        check: (!check_cmd.is_empty()).then(|| api::Probe {
            command: check_cmd,
            on: r.get("check_on"),
            worker: r.get("check_worker"),
            ..Default::default()
        }),
        evidence: serde_json::from_str(&r.get::<String, _>("evidence")).unwrap_or_default(),
        supports: serde_json::from_str(&r.get::<String, _>("supports")).unwrap_or_default(),
        replaced_by: r.get("replaced_by"),
        valid_from: r.get("valid_from"),
        valid_to,
        last_verified: last,
        due,
        made_by: r.get("made_by"),
        created_at: created,
        updated_at: r.get("updated_at"),
    }
}

pub async fn get(m: &Memex, id: &str) -> anyhow::Result<Option<api::Fact>> {
    let r = sqlx::query("SELECT * FROM facts WHERE id = ?").bind(id).fetch_optional(&m.store.pool).await?;
    Ok(r.map(|r| fact_of(&r, c6_core::now_ms())))
}

/// Facts about any of these things (keys or ids), newest first; retired
/// ones only when asked.
pub async fn about(m: &Memex, keys: &[String], include_retired: bool, limit: u32) -> anyhow::Result<Vec<api::Fact>> {
    let now = c6_core::now_ms();
    let rows = if keys.is_empty() {
        sqlx::query("SELECT * FROM facts WHERE (?1 OR status != 'retired') ORDER BY updated_at DESC LIMIT ?2")
            .bind(include_retired)
            .bind(limit as i64)
            .fetch_all(&m.store.pool)
            .await?
    } else {
        sqlx::query(
            "SELECT DISTINCT f.* FROM facts f, json_each(f.about) a WHERE a.value IN (SELECT value FROM json_each(?1)) \
             AND (?2 OR f.status != 'retired') ORDER BY f.updated_at DESC LIMIT ?3",
        )
        .bind(serde_json::to_string(keys)?)
        .bind(include_retired)
        .bind(limit as i64)
        .fetch_all(&m.store.pool)
        .await?
    };
    Ok(rows.iter().map(|r| fact_of(r, now)).collect())
}

/// The searchable item that stands for a fact.
async fn put_item(m: &Memex, f: &api::Fact) -> anyhow::Result<String> {
    let mut text = format!("{}\nAbout: {}\nStatus: {} ({})", f.claim, f.about.join(", "), f.status, f.volatility);
    if !f.labels.is_empty() || !f.conditions.is_empty() {
        text.push_str(&format!("\nApplies: {} {}", f.labels.join(", "), f.conditions));
    }
    if f.negative {
        text.push_str("\n(This says what does NOT apply.)");
    }
    if let Some(c) = &f.check {
        text.push_str(&format!("\nCheck: {}", c.command));
    }
    let item = api::MemexItemIn {
        kind: "fact".into(),
        source: "knowledge".into(),
        external_id: f.id.clone(),
        title: clip(&f.claim, 120),
        text,
        created_at: Some(f.created_at),
        labels: f.about.clone(),
        meta: json!({"status": f.status, "volatility": f.volatility}),
        ..Default::default()
    };
    m.store.put(&item, "knowledge").await
}

async fn save(m: &Memex, f: &api::Fact) -> anyhow::Result<()> {
    let item_id = put_item(m, f).await?;
    let check = f.check.clone().unwrap_or_default();
    sqlx::query(
        "INSERT OR REPLACE INTO facts (id, item_id, claim, key, about, status, volatility, labels, conditions, negative, check_cmd, check_on, \
         check_worker, evidence, supports, replaced_by, valid_from, valid_to, last_verified, made_by, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&f.id)
    .bind(&item_id)
    .bind(&f.claim)
    .bind(key_of(&f.claim))
    .bind(serde_json::to_string(&f.about)?)
    .bind(&f.status)
    .bind(&f.volatility)
    .bind(serde_json::to_string(&f.labels)?)
    .bind(&f.conditions)
    .bind(f.negative as i64)
    .bind(&check.command)
    .bind(&check.on)
    .bind(&check.worker)
    .bind(serde_json::to_string(&f.evidence)?)
    .bind(serde_json::to_string(&f.supports)?)
    .bind(&f.replaced_by)
    .bind(f.valid_from)
    .bind(f.valid_to)
    .bind(f.last_verified)
    .bind(&f.made_by)
    .bind(f.created_at)
    .bind(f.updated_at)
    .execute(&m.store.pool)
    .await?;
    // A changed fact means its systems' pages are due.
    for k in &f.about {
        sqlx::query("UPDATE things SET page_at = NULL WHERE key = ?").bind(k).execute(&m.store.pool).await?;
    }
    Ok(())
}

fn evidence_of(params: &Value, made_by: &str) -> Option<Value> {
    let e = &params["evidence"];
    let has = ["command", "excerpt", "source"].iter().any(|k| e[*k].as_str().is_some_and(|s| !s.trim().is_empty()));
    has.then(|| {
        json!({
            "at": c6_core::now_ms(), "by": made_by,
            "where": e["where"].as_str().unwrap_or(""),
            "command": e["command"].as_str().unwrap_or(""),
            "excerpt": clip(e["excerpt"].as_str().unwrap_or(""), 1500),
            "source": e["source"].as_str().unwrap_or(""),
        })
    })
}

/// `memex.knowledge.record`: a fact a session figured out. The same claim
/// about the same things again adds its evidence instead of a duplicate;
/// `replaces` ends an older fact; `contradicts` disputes one (an action
/// item for you).
pub async fn record(m: &Memex, params: &Value) -> anyhow::Result<Value> {
    let claim = params["claim"].as_str().unwrap_or("").trim().to_owned();
    anyhow::ensure!(!claim.is_empty() && claim.len() <= 2000, "a fact needs a claim (up to 2000 characters)");
    let mut about = strings(&params["about"]);
    anyhow::ensure!(!about.is_empty(), "say which systems it is about (thing keys like ci:argo or repo:github.com/you/app)");
    about.sort();
    about.dedup();
    let status = params["status"].as_str().unwrap_or("observed");
    anyhow::ensure!(
        matches!(status, "observed" | "inferred"),
        "record what you observed or inferred; confirmation comes from checks and the user"
    );
    let volatility = params["volatility"].as_str().unwrap_or("stable");
    anyhow::ensure!(
        matches!(volatility, "stable" | "slow"),
        "volatility is stable (structural) or slow (addresses, versions, reachability); a current state (volatile) is not a fact"
    );
    let made_by = params["made_by"].as_str().unwrap_or("you").to_owned();
    let evidence = evidence_of(params, &made_by);
    anyhow::ensure!(
        status == "inferred" || evidence.is_some(),
        "an observed fact needs evidence: the command you ran and an excerpt of what it showed (or the source you read)"
    );
    let now = c6_core::now_ms();
    // The same claim about the same things: one fact, more evidence.
    let key = key_of(&claim);
    let existing = sqlx::query("SELECT * FROM facts WHERE key = ? AND about = ? AND valid_to IS NULL AND status != 'retired'")
        .bind(&key)
        .bind(serde_json::to_string(&about)?)
        .fetch_optional(&m.store.pool)
        .await?;
    if let Some(r) = existing {
        let mut f = fact_of(&r, now);
        if let Some(e) = evidence {
            f.evidence.push(e);
            let n = f.evidence.len();
            if n > 10 {
                f.evidence.drain(..n - 10);
            }
        }
        // Seen where it was only inferred: now observed.
        if f.status == "inferred" && status == "observed" {
            f.status = "observed".into();
        }
        f.updated_at = now;
        save(m, &f).await?;
        return Ok(json!({"id": f.id, "merged": true, "status": f.status}));
    }
    let check = params["check"]["command"].as_str().filter(|c| !c.trim().is_empty()).map(|c| api::Probe {
        command: c.trim().to_owned(),
        on: params["check"]["on"].as_str().unwrap_or("").to_owned(),
        worker: params["check"]["worker"].as_str().unwrap_or("").to_owned(),
        ..Default::default()
    });
    let f = api::Fact {
        id: uuid::Uuid::now_v7().to_string(),
        claim,
        about,
        status: status.into(),
        volatility: volatility.into(),
        labels: strings(&params["labels"]),
        conditions: params["conditions"].as_str().unwrap_or("").trim().to_owned(),
        negative: params["negative"].as_bool().unwrap_or(false),
        check,
        evidence: evidence.into_iter().collect(),
        supports: strings(&params["supports"]),
        replaced_by: None,
        valid_from: now,
        valid_to: None,
        last_verified: None,
        due: false,
        made_by: made_by.clone(),
        created_at: now,
        updated_at: now,
    };
    save(m, &f).await?;
    let mut note = Vec::new();
    if let Some(old) = params["replaces"].as_str().filter(|s| !s.is_empty()) {
        end(m, old, "retired", Some(&f.id)).await?;
        note.push(format!("replaced {old}"));
    }
    if let Some(old) = params["contradicts"].as_str().filter(|s| !s.is_empty()) {
        dispute(m, old, &f, &made_by).await?;
        note.push(format!("disputed {old} (an action item asks the user to settle it)"));
    }
    Ok(json!({"id": f.id, "merged": false, "status": f.status, "note": note.join("; ")}))
}

/// Ends a fact (retired, or replaced by another).
async fn end(m: &Memex, id: &str, status: &str, replaced_by: Option<&str>) -> anyhow::Result<()> {
    let Some(mut f) = get(m, id).await? else { anyhow::bail!("no fact {id}") };
    let now = c6_core::now_ms();
    f.status = status.into();
    f.valid_to = Some(now);
    f.replaced_by = replaced_by.map(str::to_owned);
    f.updated_at = now;
    save(m, &f).await
}

/// A newer observation contradicts a fact: it is disputed, and you get an
/// action item to settle it.
async fn dispute(m: &Memex, id: &str, newer: &api::Fact, by: &str) -> anyhow::Result<()> {
    let Some(mut old) = get(m, id).await? else { anyhow::bail!("no fact {id}") };
    old.status = "disputed".into();
    old.updated_at = c6_core::now_ms();
    save(m, &old).await?;
    todo(
        m,
        "dispute",
        &format!("Which is true: \"{}\" or \"{}\"?", clip(&old.claim, 120), clip(&newer.claim, 120)),
        &format!("{by} observed something that contradicts an earlier fact. Settle it: keep one, correct it, or say where each applies."),
        json!({"facts": [old.id, newer.id], "things": old.about}),
        &format!("dispute:{}", old.id),
        "normal",
    )
    .await;
    Ok(())
}

/// Raises an action item on the head (best effort: knowledge never waits).
pub async fn todo(m: &Memex, kind: &str, title: &str, detail: &str, context: Value, group: &str, priority: &str) {
    let p = json!({"kind": kind, "title": title, "detail": detail, "context": context, "source": "memex", "group": group, "priority": priority, "realm": m.realm});
    if let Err(e) = m.link.call("todo.add", p).await {
        tracing::warn!("raising an action item: {e:#}");
    }
}

/// `memex.knowledge.verify`: a session re-checked a fact. True: verified
/// (and when); false: disputed, with an action item.
pub async fn verify(m: &Memex, params: &Value) -> anyhow::Result<Value> {
    let id = params["id"].as_str().unwrap_or_default();
    let Some(mut f) = get(m, id).await? else { anyhow::bail!("no fact {id}") };
    anyhow::ensure!(f.valid_to.is_none(), "that fact is no longer current (status {})", f.status);
    let made_by = params["made_by"].as_str().unwrap_or("you").to_owned();
    let now = c6_core::now_ms();
    if let Some(e) = evidence_of(params, &made_by) {
        f.evidence.push(e);
    }
    let holds = params["holds"].as_bool().unwrap_or(true);
    if holds {
        if f.status != "reviewed" {
            f.status = "verified".into();
        }
        f.last_verified = Some(now);
        f.updated_at = now;
        save(m, &f).await?;
        Ok(json!({"id": f.id, "status": f.status}))
    } else {
        f.status = "disputed".into();
        f.updated_at = now;
        save(m, &f).await?;
        todo(
            m,
            "dispute",
            &format!("No longer true? \"{}\"", clip(&f.claim, 140)),
            &format!("{made_by} re-checked it and it did not hold. {}", params["note"].as_str().unwrap_or("")),
            json!({"facts": [f.id], "things": f.about}),
            &format!("dispute:{}", f.id),
            "normal",
        )
        .await;
        Ok(json!({"id": f.id, "status": "disputed"}))
    }
}

/// `memex.knowledge.set`: you settle a fact: reviewed (confirmed), retired
/// (no longer true), or corrected (a new reviewed fact replaces it).
pub async fn set(m: &Memex, params: &Value) -> anyhow::Result<Value> {
    let id = params["id"].as_str().unwrap_or_default();
    let status = params["status"].as_str().unwrap_or("");
    let Some(mut f) = get(m, id).await? else { anyhow::bail!("no fact {id}") };
    // A fact is a claim (docs/apps.md §9): only its owner (or you) retires or
    // replaces it; anyone else records a fact of their own.
    let caller = params["owner"].as_str().filter(|o| !o.is_empty()).unwrap_or("you");
    let owner: String = sqlx::query_scalar("SELECT owner FROM facts WHERE id = ?").bind(&f.id).fetch_one(&m.store.pool).await?;
    if caller != "you" && caller != owner && (status == "retired" || params["claim"].as_str().is_some_and(|c| !c.trim().is_empty())) {
        anyhow::bail!("fact {} is {owner}'s: record a fact of your own instead (it may dispute this one)", f.id);
    }
    let now = c6_core::now_ms();
    let by = params["made_by"].as_str().unwrap_or("you").to_owned();
    if let Some(correct) = params["claim"].as_str().filter(|c| !c.trim().is_empty()) {
        let mut new = f.clone();
        new.id = uuid::Uuid::now_v7().to_string();
        new.claim = correct.trim().to_owned();
        new.status = "reviewed".into();
        new.evidence = vec![json!({"at": now, "by": by, "source": "corrected by the user"})];
        new.last_verified = Some(now);
        new.valid_from = now;
        new.valid_to = None;
        new.replaced_by = None;
        new.created_at = now;
        new.updated_at = now;
        if let Some(l) = params["labels"].as_array() {
            new.labels = strings(&Value::Array(l.clone()));
        }
        if let Some(c) = params["conditions"].as_str() {
            new.conditions = c.to_owned();
        }
        save(m, &new).await?;
        end(m, &f.id, "retired", Some(&new.id)).await?;
        return Ok(json!({"id": new.id, "status": "reviewed", "replaced": f.id}));
    }
    anyhow::ensure!(STATUSES.contains(&status) && status != "verified", "status is reviewed, observed, inferred, disputed or retired");
    if status == "retired" {
        end(m, &f.id, "retired", None).await?;
        return Ok(json!({"id": f.id, "status": "retired"}));
    }
    f.status = status.into();
    if status == "reviewed" {
        f.last_verified = Some(now);
        f.evidence.push(json!({"at": now, "by": by, "source": "confirmed by the user"}));
    }
    if let Some(l) = params["labels"].as_array() {
        f.labels = strings(&Value::Array(l.clone()));
    }
    if let Some(c) = params["conditions"].as_str() {
        f.conditions = c.to_owned();
    }
    f.updated_at = now;
    save(m, &f).await?;
    Ok(json!({"id": f.id, "status": f.status}))
}

/// `memex.knowledge.list`: facts about things (or all), with filters.
pub async fn list(m: &Memex, params: &Value) -> anyhow::Result<Value> {
    let keys = strings(&params["about"]);
    let mut facts =
        about(m, &keys, params["include_retired"].as_bool().unwrap_or(false), params["limit"].as_u64().unwrap_or(200) as u32).await?;
    if let Some(s) = params["status"].as_str().filter(|s| !s.is_empty()) {
        facts.retain(|f| f.status == s);
    }
    if params["due"].as_bool() == Some(true) {
        facts.retain(|f| f.due);
    }
    Ok(json!(facts))
}

/// `memex.knowledge.page`: a system: its page, facts and procedures.
pub async fn page(m: &Memex, key: &str) -> anyhow::Result<Value> {
    let t = m.store.thing(key).await?.ok_or_else(|| anyhow::anyhow!("no system {key:?}"))?;
    let page: String = sqlx::query_scalar("SELECT page FROM things WHERE id = ?").bind(&t.id).fetch_one(&m.store.pool).await?;
    let facts = about(m, std::slice::from_ref(&t.label), true, 200).await?;
    let procedures = m.store.about(Some(&t.id), &["note".to_owned()], 30).await?;
    Ok(json!({"thing": t, "page": page, "facts": facts, "procedures": procedures}))
}

/// What a fact looks like in a brief: its status, age, scope and check.
pub fn brief_line(f: &api::Fact, now: i64) -> String {
    let age_days = (now - f.last_verified.unwrap_or(f.created_at)) / DAY_MS;
    let age = match age_days {
        0 => "today".to_owned(),
        1 => "1 day".to_owned(),
        d => format!("{d} days"),
    };
    let mut meta = vec![f.status.clone(), age];
    if !f.labels.is_empty() {
        meta.push(format!("only {}", f.labels.join(",")));
    }
    if !f.conditions.is_empty() {
        meta.push(f.conditions.clone());
    }
    let mut line = format!("- [{}] {}{}", meta.join(" · "), if f.negative { "NOT: " } else { "" }, f.claim);
    if let Some(c) = &f.check {
        line.push_str(&format!(" (check: `{}`{})", c.command, if f.due { "; due — re-check before relying on it" } else { "" }));
    }
    line.push_str(&format!(" [fact {}]", f.id));
    line
}

/// The environment knowledge for a brief: facts about these things, and
/// about the systems those facts mention (one step further).
pub async fn for_brief(m: &Memex, keys: &[String], labels: &[String], max: usize) -> anyhow::Result<Vec<api::Fact>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let mut facts = about(m, keys, false, 100).await?;
    let mut next: Vec<String> = facts.iter().flat_map(|f| f.about.clone()).filter(|k| !keys.contains(k)).collect();
    next.sort();
    next.dedup();
    if !next.is_empty() {
        let more = about(m, &next, false, 100).await?;
        facts.extend(more);
    }
    let mut seen = std::collections::HashSet::new();
    facts.retain(|f| seen.insert(f.id.clone()));
    // Facts limited to labels this session does not have still show, so the
    // agent learns where they do not apply; rank them last.
    let rank = |f: &api::Fact| -> (u8, u8) {
        let applies = f.labels.is_empty() || api::labels::applies(&f.labels, labels);
        let status = match f.status.as_str() {
            "reviewed" => 0,
            "verified" => 1,
            "observed" => 2,
            "inferred" => 3,
            _ => 4,
        };
        (u8::from(!applies), status)
    };
    facts.sort_by_key(rank);
    facts.truncate(max);
    Ok(facts)
}

/// Writes system pages from their facts (with the understanding model),
/// one per call. Whether it did one.
pub async fn page_step(m: &Memex) -> anyhow::Result<bool> {
    let Some((task, per_day)) = m.understanding() else { return Ok(false) };
    if crate::understand::budget_left(m, per_day).await? == 0 {
        return Ok(false);
    }
    let r = sqlx::query(
        "SELECT t.id, t.key, t.name FROM things t WHERE t.page_at IS NULL AND EXISTS \
         (SELECT 1 FROM facts f, json_each(f.about) a WHERE a.value = t.key AND f.status != 'retired') LIMIT 1",
    )
    .fetch_optional(&m.store.pool)
    .await?;
    let Some(r) = r else { return Ok(false) };
    let (id, key, name): (String, String, String) = (r.get("id"), r.get("key"), r.get("name"));
    let facts = about(m, std::slice::from_ref(&key), false, 80).await?;
    let now = c6_core::now_ms();
    let lines: Vec<String> = facts.iter().map(|f| brief_line(f, now)).collect();
    crate::understand::count_call(m).await?;
    let template = m.prompt("memex.page", api::prompts::PAGE);
    let prompt = api::prompts::fill(&template, &[("name", &name), ("key", &key), ("facts", &lines.join("\n"))]);
    let system = m.prompt("memex.page.system", api::prompts::PAGE_SYSTEM);
    let page = crate::llm::ask(m, &task, "page", &name, &system, vec![c6_llm::Block::Text(prompt)], 3000).await?;
    sqlx::query("UPDATE things SET page = ?, page_at = ? WHERE id = ?")
        .bind(page.trim())
        .bind(now)
        .bind(&id)
        .execute(&m.store.pool)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn keys_ignore_case_and_punctuation() {
        assert_eq!(
            super::key_of("Workflows carry the commit as label `git-sha`."),
            super::key_of("workflows carry the commit as label git-sha")
        );
        assert_ne!(super::key_of("CI is Argo"), super::key_of("CI is Jenkins"));
    }
}

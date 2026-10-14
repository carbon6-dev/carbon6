//! Apps (docs/os.md §5): packages of configuration (collections, series,
//! triggers, views, skills, templates and the other settings lists, memory
//! notes, and optionally records to start with), with no code.
//!
//! - **Install** shows you the plan as a decision, then creates the realm
//!   and its contents, asks for the grants it needs, and tells you which
//!   secrets to set.
//! - **Upgrade** compares the installed version, your edits since, and the
//!   new version: your edits are kept, and conflicts are yours to settle.
//! - **Export** turns a realm's configuration (and, if asked, its records)
//!   into a package. **Remove** deletes the realm and everything in it,
//!   after your decision.
//!
//! Contents are written through the head's own API, so they are checked
//! exactly as when you write them in Settings.

use std::collections::{BTreeMap, BTreeSet};

use c6_api as api;
use c6_core::now_ms;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::warn;

use crate::realms::{COMMONS, HOME, ROOT};
use crate::{HeadState, bad_request, not_found};

/// The settings lists an app may bring, and their API paths.
pub const LISTS: [(&str, &str); 12] = [
    ("views", "views"),
    ("skills", "skills"),
    ("agents", "agents"),
    ("commands", "commands"),
    ("hooks", "hooks"),
    ("mcp_servers", "mcp-servers"),
    ("templates", "templates"),
    ("workflows", "workflows"),
    ("policies", "policies"),
    ("routes", "routes"),
    ("prices", "prices"),
    ("budgets", "budgets"),
];

fn install_of(r: &sqlx::sqlite::SqliteRow) -> api::AppInstall {
    api::AppInstall {
        realm: r.get("realm"),
        app: r.get("app"),
        version: r.get("version"),
        description: r.get("description"),
        state: r.get("state"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        decision: r.get("decision"),
    }
}

pub async fn list(st: &HeadState) -> anyhow::Result<Vec<api::AppInstall>> {
    let rows = sqlx::query("SELECT * FROM apps ORDER BY app").fetch_all(&st.store.pool).await?;
    Ok(rows.iter().map(install_of).collect())
}

async fn row(st: &HeadState, realm: &str) -> anyhow::Result<Option<sqlx::sqlite::SqliteRow>> {
    Ok(sqlx::query("SELECT * FROM apps WHERE realm = ?").bind(realm).fetch_optional(&st.store.pool).await?)
}

/// A request the head makes to its own API, as you.
async fn internal(st: &HeadState, method: &str, uri: &str, body: Option<&Value>) -> anyhow::Result<Value> {
    use tower::ServiceExt;
    let router = st.assistant.router().ok_or_else(|| anyhow::anyhow!("the head is starting"))?;
    let mut req =
        axum::http::Request::builder().method(method).uri(uri).header(axum::http::header::CONTENT_TYPE, "application/json").body(
            match body {
                Some(b) => axum::body::Body::from(serde_json::to_vec(b)?),
                None => axum::body::Body::empty(),
            },
        )?;
    req.extensions_mut().insert(crate::assistant::InternalCall::default());
    let resp = router.oneshot(req).await?;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024).await?;
    let v: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
    if !status.is_success() {
        return Err(bad_request(format!("{method} {uri}: {}", v["error"].as_str().unwrap_or(status.as_str()))));
    }
    Ok(v)
}

fn realm_of(p: &api::AppPackage) -> String {
    if p.realm.trim().is_empty() { p.app.trim().to_owned() } else { p.realm.trim().to_owned() }
}

fn check_package(p: &api::AppPackage) -> Vec<String> {
    let mut problems = Vec::new();
    let valid = |s: &str| !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid(p.app.trim()) {
        problems.push("an app's name is lowercase letters, digits and - (up to 32)".into());
    }
    let realm = realm_of(p);
    if [ROOT, HOME, COMMONS].contains(&realm.as_str()) {
        problems.push(format!("an app gets a realm of its own, not {realm}"));
    }
    for key in p.contents.lists.keys() {
        if !LISTS.iter().any(|(k, _)| k == key) {
            problems
                .push(format!("{key:?} is not something an app can bring ({})", LISTS.iter().map(|l| l.0).collect::<Vec<_>>().join(", ")));
        }
    }
    for c in p.contents.records.keys() {
        if !p.contents.collections.iter().any(|x| &x.name == c) {
            problems.push(format!("records for {c}, which the app does not define"));
        }
    }
    problems
}

fn count(what: &str, n: usize) -> Option<String> {
    (n > 0).then(|| format!("{n} {what}"))
}

/// What installing a package would do.
pub async fn plan_install(st: &HeadState, p: &api::AppPackage) -> anyhow::Result<api::AppPlan> {
    let realm = realm_of(p);
    let mut problems = check_package(p);
    if crate::realms::exists(st, &realm).await? {
        problems.push(format!("realm {realm} already exists (upgrade the app installed there, or give the package another realm)"));
    }
    let c = &p.contents;
    let mut lines = vec![format!("Install **{}** {} into a new realm **{realm}**.", p.app, p.version), String::new()];
    if !p.description.is_empty() {
        lines.splice(1..1, [String::new(), p.description.clone()]);
    }
    let mut what: Vec<String> = [
        count("collections", c.collections.len()),
        count("series", c.series.len()),
        count("triggers", c.triggers.len()),
        count("memory notes", c.memory.len()),
        count("records", c.records.values().map(Vec::len).sum()),
    ]
    .into_iter()
    .flatten()
    .collect();
    what.extend(c.lists.iter().filter_map(|(k, v)| count(k, v.len())));
    lines.push(format!("It creates: {}.", if what.is_empty() { "nothing".to_owned() } else { what.join(", ") }));
    for (title, items) in [
        ("Collections", c.collections.iter().map(|x| x.name.clone()).collect::<Vec<_>>()),
        ("Triggers", c.triggers.iter().map(|t| t.name.clone().unwrap_or_default()).collect()),
    ] {
        if !items.is_empty() {
            lines.push(format!("- {title}: {}", items.join(", ")));
        }
    }
    for (k, v) in &c.lists {
        let names: Vec<String> = v.iter().map(|e| e["name"].as_str().unwrap_or("?").to_owned()).collect();
        lines.push(format!("- {k}: {}", names.join(", ")));
    }
    if !p.needs.secrets.is_empty() {
        lines.push(format!(
            "\nIt needs these secrets, which you set afterwards (Settings → Secrets, in realm {realm}): {}.",
            p.needs.secrets.join(", ")
        ));
    }
    if p.needs.workers == "dedicated" {
        lines.push(format!("\nIt wants a worker of its own: dedicate one to realm {realm} under Settings → System → Realms."));
    }
    for g in &p.needs.grants {
        lines.push(format!(
            "\nIt asks for a grant: {} {} of realm {} ({}), which comes to you as a decision of its own.",
            g.mode, g.kind, g.from_realm, g.scope
        ));
    }
    Ok(api::AppPlan { realm, plan: lines.join("\n"), problems, conflicts: Vec::new() })
}

/// Asks to install a package: the plan waits as a decision.
pub async fn install(st: &HeadState, p: &api::AppPackage) -> anyhow::Result<api::AppInstall> {
    let plan = plan_install(st, p).await?;
    if !plan.problems.is_empty() {
        return Err(bad_request(plan.problems.join("; ")));
    }
    if row(st, &plan.realm).await?.is_some() {
        return Err(bad_request(format!("an app is already being installed into {}", plan.realm)));
    }
    let now = now_ms();
    sqlx::query("INSERT INTO apps (realm, app, version, description, state, pending, created_at, updated_at) VALUES (?, ?, ?, ?, 'installing', ?, ?, ?)")
        .bind(&plan.realm)
        .bind(p.app.trim())
        .bind(&p.version)
        .bind(&p.description)
        .bind(serde_json::to_string(p)?)
        .bind(now)
        .bind(now)
        .execute(&st.store.pool)
        .await?;
    ask(st, &plan.realm, "install", &format!("Install the app {}?", p.app), &plan.plan, &["Install", "Cancel"]).await
}

async fn ask(st: &HeadState, realm: &str, what: &str, title: &str, document: &str, options: &[&str]) -> anyhow::Result<api::AppInstall> {
    let d = api::DecisionIn {
        kind: "approve".into(),
        title: title.to_owned(),
        document: document.to_owned(),
        options: options.iter().map(|o| api::DecisionOption { label: (*o).to_owned(), does: String::new() }).collect(),
        priority: "normal".into(),
        source: "apps".into(),
        context: json!({"app_realm": realm, "what": what}),
        ..Default::default()
    };
    let d = crate::decisions::open(st, &d, ROOT, &format!("app:{what}:{realm}")).await?;
    sqlx::query("UPDATE apps SET decision = ?, updated_at = ? WHERE realm = ?")
        .bind(d.id)
        .bind(now_ms())
        .bind(realm)
        .execute(&st.store.pool)
        .await?;
    Ok(install_of(&row(st, realm).await?.ok_or_else(|| anyhow::anyhow!("app gone"))?))
}

/// Your answer to an install, upgrade or removal.
pub async fn answer(st: &HeadState, what: &str, realm: &str, option: Option<&str>) -> anyhow::Result<()> {
    let r = row(st, realm).await?.ok_or_else(|| not_found(format!("no app in {realm}")))?;
    let pending: Option<api::AppPackage> = r.get::<Option<String>, _>("pending").and_then(|p| serde_json::from_str(&p).ok());
    let go = matches!(option, Some("Install" | "Upgrade, keeping my edits" | "Upgrade, taking the new version" | "Remove"));
    if !go {
        // Cancelled: an install that never happened leaves nothing.
        if what == "install" {
            sqlx::query("DELETE FROM apps WHERE realm = ?").bind(realm).execute(&st.store.pool).await?;
        } else {
            sqlx::query("UPDATE apps SET state = 'installed', pending = NULL, decision = NULL, updated_at = ? WHERE realm = ?")
                .bind(now_ms())
                .bind(realm)
                .execute(&st.store.pool)
                .await?;
        }
        return Ok(());
    }
    match what {
        "install" => {
            let p = pending.ok_or_else(|| anyhow::anyhow!("the package is gone"))?;
            apply_install(st, realm, &p).await?;
            finished(st, realm, &p).await?;
            next_steps(st, realm, &p).await;
        }
        "upgrade" => {
            let p = pending.ok_or_else(|| anyhow::anyhow!("the package is gone"))?;
            let base: api::AppPackage = serde_json::from_str(&r.get::<String, _>("installed")).unwrap_or_default();
            apply_upgrade(st, realm, &base, &p, option == Some("Upgrade, taking the new version")).await?;
            finished(st, realm, &p).await?;
        }
        "remove" => remove_now(st, realm).await?,
        _ => {}
    }
    Ok(())
}

async fn finished(st: &HeadState, realm: &str, p: &api::AppPackage) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE apps SET state = 'installed', installed = ?, pending = NULL, decision = NULL, version = ?, description = ?, updated_at = ? WHERE realm = ?",
    )
    .bind(serde_json::to_string(p)?)
    .bind(&p.version)
    .bind(&p.description)
    .bind(now_ms())
    .bind(realm)
    .execute(&st.store.pool)
    .await?;
    sqlx::query("UPDATE realms SET app = ? WHERE id = ?").bind(p.app.trim()).bind(realm).execute(&st.store.pool).await?;
    st.registry.bump();
    Ok(())
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

async fn apply_install(st: &HeadState, realm: &str, p: &api::AppPackage) -> anyhow::Result<()> {
    let r = api::RealmIn { id: realm.to_owned(), name: p.app.clone(), description: p.description.clone(), reads_commons: None };
    crate::realms::create(st, &r, &format!("app:{}", p.app)).await?;
    let c = &p.contents;
    let q = format!("?realm={}", enc(realm));
    for col in &c.collections {
        internal(st, "POST", &format!("/api/v1/collections{q}"), Some(&json!(col))).await?;
    }
    for s in &c.series {
        internal(st, "POST", &format!("/api/v1/series{q}"), Some(&json!(s))).await?;
    }
    for (key, entries) in &c.lists {
        write_list(st, realm, key, entries.clone()).await?;
    }
    for t in &c.triggers {
        crate::triggers::create(st, t.clone(), realm).await?;
    }
    for m in &c.memory {
        let note = api::Memory {
            scope: m["scope"].as_str().unwrap_or("").to_owned(),
            text: m["text"].as_str().unwrap_or("").to_owned(),
            ..Default::default()
        };
        crate::memory::put(st, note, realm).await?;
    }
    for (col, recs) in &c.records {
        for fields in recs {
            internal(st, "POST", &format!("/api/v1/collections/{}/records{q}", enc(col)), Some(&json!({"fields": fields}))).await?;
        }
    }
    Ok(())
}

/// Replaces a realm's own entries of a list, through Settings.
async fn write_list(st: &HeadState, realm: &str, key: &str, entries: Vec<Value>) -> anyhow::Result<()> {
    let path =
        LISTS.iter().find(|(k, _)| *k == key).map(|l| l.1).ok_or_else(|| bad_request(format!("{key} is not a list an app brings")))?;
    internal(st, "PUT", &format!("/api/v1/settings/{path}?realm={}", enc(realm)), Some(&Value::Array(entries))).await?;
    Ok(())
}

/// After installing: the grants it needs (asked as decisions), and what is
/// left for you (secrets, a worker of its own), as a follow-up.
async fn next_steps(st: &HeadState, realm: &str, p: &api::AppPackage) {
    for g in &p.needs.grants {
        let gi = api::GrantIn {
            from_realm: g.from_realm.clone(),
            to_realm: realm.to_owned(),
            kind: g.kind.clone(),
            scope: g.scope.clone(),
            mode: g.mode.clone(),
            expires_at: None,
            note: if g.note.is_empty() { format!("for the app {}", p.app) } else { g.note.clone() },
        };
        if let Err(e) = crate::grants::ask(st, &gi, &format!("app:{}", p.app)).await {
            warn!(app = %p.app, "asking for a grant: {e:#}");
        }
    }
    let have: Vec<String> =
        sqlx::query_scalar("SELECT name FROM secrets WHERE realm = ?").bind(realm).fetch_all(&st.store.pool).await.unwrap_or_default();
    let missing: Vec<&String> = p.needs.secrets.iter().filter(|s| !have.contains(s)).collect();
    let mut todo = Vec::new();
    if !missing.is_empty() {
        todo.push(format!(
            "- Set its secrets in realm **{realm}** (Settings → Secrets, with {realm} picked in the header): {}",
            missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    if p.needs.workers == "dedicated" {
        todo.push(format!("- Dedicate a worker to realm **{realm}** (Settings → System → Realms)."));
    }
    if p.contents.triggers.iter().any(|t| t.trigger.as_deref() == Some("webhook")) {
        todo.push("- Its webhook triggers have URLs you get with New URL (Settings → Automation).".into());
    }
    if todo.is_empty() {
        return;
    }
    let d = api::DecisionIn {
        kind: "follow_up".into(),
        title: format!("Finish setting up {}", p.app),
        document: todo.join("\n"),
        source: "apps".into(),
        ..Default::default()
    };
    if let Err(e) = crate::decisions::open(st, &d, realm, "").await {
        warn!(app = %p.app, "{e:#}");
    }
}

/// A realm's configuration as a package; with `data`, its records too.
pub async fn export(st: &HeadState, realm: &str, data: bool) -> anyhow::Result<api::AppPackage> {
    if !crate::realms::exists(st, realm).await? || realm == ROOT {
        return Err(bad_request(format!("no realm {realm:?} to export")));
    }
    let installed = row(st, realm).await?;
    let mut p = api::AppPackage {
        app: installed.as_ref().map(|r| r.get::<String, _>("app")).unwrap_or_else(|| realm.to_owned()),
        version: installed.as_ref().map(|r| r.get::<String, _>("version")).unwrap_or_default(),
        description: installed.as_ref().map(|r| r.get::<String, _>("description")).unwrap_or_default(),
        realm: realm.to_owned(),
        ..Default::default()
    };
    for c in crate::collections::list(st, realm).await?.into_iter().filter(|c| !c.system) {
        if data {
            let page = crate::collections::query(
                st,
                realm,
                &c.name,
                &api::RecordQuery { limit: Some(100_000), sort: "created_at".into(), ..Default::default() },
            )
            .await?;
            if !page.records.is_empty() {
                p.contents.records.insert(c.name.clone(), page.records.into_iter().map(|r| r.fields).collect());
            }
        }
        p.contents.collections.push(api::CollectionIn {
            name: c.name,
            description: Some(c.description),
            schema: Some(c.schema),
            keys: Some(c.keys),
        });
    }
    for s in crate::series::list(st, realm).await?.into_iter().filter(|s| !s.system) {
        p.contents.series.push(api::SeriesIn {
            name: s.name,
            unit: Some(s.unit),
            description: Some(s.description),
            dimensions: Some(s.dimensions),
            retention_days: Some(s.retention_days),
        });
    }
    for (key, _) in LISTS {
        let entries = crate::lists::read(&st.store.pool, key, realm).await?;
        if !entries.is_empty() {
            p.contents.lists.insert(key.to_owned(), entries);
        }
    }
    for t in crate::triggers::list(st, &crate::realms::Caller::in_realm(realm)).await? {
        p.contents.triggers.push(api::AutomationEdit {
            name: Some(t.name),
            template: Some(t.template),
            trigger: Some(t.trigger),
            cron: Some(t.cron),
            input: Some(t.input),
            enabled: Some(t.enabled),
            disable_after: Some(t.disable_after),
            event_params: Some(t.event_params),
            filter: Some(t.filter),
            action: Some(t.action),
            action_params: Some(t.action_params),
            per_hour: Some(t.per_hour),
        });
    }
    for m in crate::memory::list(st, &crate::realms::Caller::in_realm(realm)).await? {
        p.contents.memory.push(json!({"scope": m.scope, "text": m.text}));
    }
    p.needs.secrets =
        sqlx::query_scalar("SELECT name FROM secrets WHERE realm = ? ORDER BY name").bind(realm).fetch_all(&st.store.pool).await?;
    p.needs.grants = crate::grants::list(st, Some(realm))
        .await?
        .into_iter()
        .filter(|g| g.to_realm == realm && matches!(g.state.as_str(), "active" | "requested"))
        .map(|g| api::AppGrant { from_realm: g.from_realm, kind: g.kind, mode: g.mode, scope: g.scope, note: g.note })
        .collect();
    let workers = crate::api::workers_snapshot(st).await.unwrap_or_default();
    p.needs.workers = if workers.iter().any(|w| w.realms == [realm.to_owned()]) { "dedicated".into() } else { "any".into() };
    Ok(p)
}

fn roundtrip<T: serde::de::DeserializeOwned + serde::Serialize>(v: &Value) -> Value {
    serde_json::from_value::<T>(v.clone()).ok().and_then(|t| serde_json::to_value(t).ok()).unwrap_or_else(|| v.clone())
}

/// A package as carbon6 stores it: each entry through its type, with the
/// defaults filled in, so that only real differences count.
fn normalize(p: &api::AppPackage) -> api::AppPackage {
    let mut p = p.clone();
    let c = &mut p.contents;
    for x in &mut c.collections {
        x.description = Some(x.description.clone().unwrap_or_default());
        x.keys = Some(x.keys.clone().unwrap_or_default());
    }
    for x in &mut c.series {
        x.unit = Some(x.unit.clone().unwrap_or_default());
        x.description = Some(x.description.clone().unwrap_or_default());
        x.dimensions = Some(x.dimensions.clone().unwrap_or_default());
        x.retention_days = Some(x.retention_days.unwrap_or(0));
    }
    for t in &mut c.triggers {
        let or = |v: &Option<Value>| Some(v.clone().filter(|v| !v.is_null()).unwrap_or(json!({})));
        *t = api::AutomationEdit {
            name: Some(t.name.clone().unwrap_or_default()),
            template: Some(t.template.clone().unwrap_or_default()),
            trigger: Some(t.trigger.clone().unwrap_or_else(|| "schedule".into())),
            cron: Some(t.cron.clone().unwrap_or_default()),
            input: Some(t.input.clone().unwrap_or_default()),
            enabled: Some(t.enabled.unwrap_or(true)),
            disable_after: Some(t.disable_after.unwrap_or(0)),
            event_params: or(&t.event_params),
            filter: or(&t.filter),
            action: Some(t.action.clone().unwrap_or_else(|| "start".into())),
            action_params: or(&t.action_params),
            per_hour: Some(t.per_hour.unwrap_or(60)),
        };
    }
    for m in &mut c.memory {
        *m = json!({"scope": m["scope"].as_str().unwrap_or(""), "text": m["text"].as_str().unwrap_or("")});
    }
    for (key, entries) in &mut c.lists {
        for e in entries.iter_mut() {
            *e = match key.as_str() {
                "views" => roundtrip::<api::View>(e),
                "skills" => roundtrip::<api::SkillDef>(e),
                "agents" => roundtrip::<api::AgentDef>(e),
                "commands" => roundtrip::<api::CommandDef>(e),
                "hooks" => roundtrip::<api::Hook>(e),
                "templates" => roundtrip::<api::Template>(e),
                "workflows" => roundtrip::<api::Workflow>(e),
                "policies" => roundtrip::<api::Policy>(e),
                "routes" => roundtrip::<api::Route>(e),
                "prices" => roundtrip::<api::Price>(e),
                "budgets" => roundtrip::<api::Budget>(e),
                _ => e.clone(),
            };
        }
    }
    p
}

/// One item's identity, by kind ("list:<key>", "collection", "series",
/// "trigger", "memory").
fn items(p: &api::AppPackage) -> BTreeMap<(String, String), Value> {
    let mut out = BTreeMap::new();
    let c = &p.contents;
    for x in &c.collections {
        out.insert(("collection".into(), x.name.clone()), json!(x));
    }
    for x in &c.series {
        out.insert(("series".into(), x.name.clone()), json!(x));
    }
    for x in &c.triggers {
        out.insert(("trigger".into(), x.name.clone().unwrap_or_default()), json!(x));
    }
    for x in &c.memory {
        out.insert(("memory".into(), x["text"].as_str().unwrap_or("").to_owned()), x.clone());
    }
    for (key, entries) in &c.lists {
        let l = crate::lists::list(key);
        for e in entries {
            let id = l.map(|l| crate::lists::identity(l, e)).unwrap_or_else(|| e["name"].to_string());
            out.insert((format!("list:{key}"), id), e.clone());
        }
    }
    out
}

fn name_of(kind: &str, id: &str) -> String {
    format!("{} {}", kind.trim_start_matches("list:"), id.split('\u{1f}').next().unwrap_or(id).trim_matches('"'))
}

/// What an upgrade does: per item, the installed version (base), yours
/// now (mine), and the new one (theirs).
struct Diff {
    apply: Vec<((String, String), Option<Value>)>,
    conflicts: Vec<((String, String), Option<Value>)>,
}

fn diff(base: &api::AppPackage, mine: &api::AppPackage, theirs: &api::AppPackage) -> Diff {
    let (b, m, t) = (items(&normalize(base)), items(&normalize(mine)), items(&normalize(theirs)));
    let keys: BTreeSet<&(String, String)> = b.keys().chain(t.keys()).collect();
    let mut d = Diff { apply: Vec::new(), conflicts: Vec::new() };
    for k in keys {
        let (bv, mv, tv) = (b.get(k), m.get(k), t.get(k));
        if tv == bv || mv == tv {
            continue;
        }
        if mv == bv {
            d.apply.push((k.clone(), tv.cloned()));
        } else {
            d.conflicts.push((k.clone(), tv.cloned()));
        }
    }
    d
}

pub async fn plan_upgrade(st: &HeadState, realm: &str, p: &api::AppPackage) -> anyhow::Result<api::AppPlan> {
    let r = row(st, realm).await?.ok_or_else(|| not_found(format!("no app installed in {realm}")))?;
    let mut problems = check_package(&api::AppPackage { realm: realm.to_owned(), ..p.clone() });
    if r.get::<String, _>("app") != p.app.trim() {
        problems.push(format!("realm {realm} has the app {}, not {}", r.get::<String, _>("app"), p.app));
    }
    let base: api::AppPackage = serde_json::from_str(&r.get::<String, _>("installed")).unwrap_or_default();
    let mine = export(st, realm, false).await?;
    let d = diff(&base, &mine, p);
    let describe =
        |(k, v): &((String, String), Option<Value>)| format!("- {} {}", if v.is_some() { "update" } else { "remove" }, name_of(&k.0, &k.1));
    let mut lines =
        vec![format!("Upgrade **{}** in realm **{realm}** from {} to {}.", p.app, r.get::<String, _>("version"), p.version), String::new()];
    if d.apply.is_empty() {
        lines.push("Nothing of yours changes.".into());
    } else {
        lines.push("Changes:".into());
        lines.extend(d.apply.iter().map(describe));
    }
    if !d.conflicts.is_empty() {
        lines.push("\nYou and the new version both changed these; by default yours are kept:".into());
        lines.extend(d.conflicts.iter().map(describe));
    }
    Ok(api::AppPlan {
        realm: realm.to_owned(),
        plan: lines.join("\n"),
        problems,
        conflicts: d.conflicts.iter().map(|(k, _)| name_of(&k.0, &k.1)).collect(),
    })
}

/// Asks to upgrade the app in `realm` to a new version.
pub async fn upgrade(st: &HeadState, realm: &str, p: &api::AppPackage) -> anyhow::Result<api::AppInstall> {
    let plan = plan_upgrade(st, realm, p).await?;
    if !plan.problems.is_empty() {
        return Err(bad_request(plan.problems.join("; ")));
    }
    sqlx::query("UPDATE apps SET state = 'upgrading', pending = ?, updated_at = ? WHERE realm = ? AND state = 'installed'")
        .bind(serde_json::to_string(p)?)
        .bind(now_ms())
        .bind(realm)
        .execute(&st.store.pool)
        .await?;
    let options: &[&str] = if plan.conflicts.is_empty() {
        &["Upgrade, keeping my edits", "Cancel"]
    } else {
        &["Upgrade, keeping my edits", "Upgrade, taking the new version", "Cancel"]
    };
    ask(st, realm, "upgrade", &format!("Upgrade {} to {}?", p.app, p.version), &plan.plan, options).await
}

async fn apply_upgrade(st: &HeadState, realm: &str, base: &api::AppPackage, p: &api::AppPackage, take_theirs: bool) -> anyhow::Result<()> {
    let mine = export(st, realm, false).await?;
    let d = diff(base, &mine, p);
    let changes: Vec<_> = d.apply.into_iter().chain(if take_theirs { d.conflicts } else { Vec::new() }).collect();
    let q = format!("?realm={}", enc(realm));
    // Lists are replaced whole: yours, with the changed entries swapped.
    let mut lists: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for ((kind, id), new) in changes {
        match kind.as_str() {
            // A collection the new version drops keeps your records.
            "collection" => {
                if let Some(v) = new {
                    if mine.contents.collections.iter().any(|c| c.name == id) {
                        internal(st, "PATCH", &format!("/api/v1/collections/{}{q}", enc(&id)), Some(&v)).await?;
                    } else {
                        internal(st, "POST", &format!("/api/v1/collections{q}"), Some(&v)).await?;
                    }
                }
            }
            "series" => match new {
                Some(v) if mine.contents.series.iter().any(|s| s.name == id) => {
                    internal(st, "PATCH", &format!("/api/v1/series/{}{q}", enc(&id)), Some(&v)).await?;
                }
                Some(v) => {
                    internal(st, "POST", &format!("/api/v1/series{q}"), Some(&v)).await?;
                }
                None => {}
            },
            "trigger" => {
                let current = crate::triggers::list(st, &crate::realms::Caller::in_realm(realm)).await?.into_iter().find(|t| t.name == id);
                match (current, new) {
                    (Some(t), Some(v)) => {
                        crate::triggers::update(st, &t.id, serde_json::from_value(v)?).await?;
                    }
                    (None, Some(v)) => {
                        crate::triggers::create(st, serde_json::from_value(v)?, realm).await?;
                    }
                    (Some(t), None) => crate::triggers::delete(st, &t.id).await?,
                    (None, None) => {}
                }
            }
            "memory" => {
                if let Some(v) = new {
                    let note = api::Memory { scope: v["scope"].as_str().unwrap_or("").to_owned(), text: id.clone(), ..Default::default() };
                    crate::memory::put(st, note, realm).await?;
                }
            }
            k if k.starts_with("list:") => {
                let key = &k[5..];
                let entries = match lists.get_mut(key) {
                    Some(e) => e,
                    None => lists.entry(key.to_owned()).or_insert(crate::lists::read(&st.store.pool, key, realm).await?),
                };
                let l = crate::lists::list(key).ok_or_else(|| bad_request(format!("{key} is not a list")))?;
                let at = entries.iter().position(|e| crate::lists::identity(l, e) == id);
                match (at, new) {
                    (Some(i), Some(v)) => entries[i] = v,
                    (None, Some(v)) => entries.push(v),
                    (Some(i), None) => {
                        entries.remove(i);
                    }
                    (None, None) => {}
                }
            }
            _ => {}
        }
    }
    for (key, entries) in lists {
        write_list(st, realm, &key, entries).await?;
    }
    Ok(())
}

/// Asks to remove the app in `realm`, with everything in the realm.
pub async fn remove(st: &HeadState, realm: &str) -> anyhow::Result<api::AppInstall> {
    let r = row(st, realm).await?.ok_or_else(|| not_found(format!("no app installed in {realm}")))?;
    let doc = format!(
        "Removing **{}** deletes realm **{realm}** and everything in it: its sessions and their history, collections and records, series, \
         triggers, views and other settings, decisions, secrets, memory notes and knowledge. Its grants end. Model usage stays in the usage \
         ledger. This cannot be undone.\n\nExport it first (Settings → System → Apps → Export, with data) to keep a copy you can install again.",
        r.get::<String, _>("app")
    );
    sqlx::query("UPDATE apps SET state = 'removing', updated_at = ? WHERE realm = ?")
        .bind(now_ms())
        .bind(realm)
        .execute(&st.store.pool)
        .await?;
    ask(st, realm, "remove", &format!("Remove the app {} and its realm?", r.get::<String, _>("app")), &doc, &["Remove", "Cancel"]).await
}

async fn remove_now(st: &HeadState, realm: &str) -> anyhow::Result<()> {
    let running: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE realm = ? AND status IN ('starting', 'running')")
        .bind(realm)
        .fetch_one(&st.store.pool)
        .await?;
    if running > 0 {
        return Err(bad_request(format!("{running} of its sessions are still running: stop them first")));
    }
    let now = now_ms();
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    let in_sessions = "(SELECT id FROM sessions WHERE realm = ?)";
    for sql in [
        format!("DELETE FROM events WHERE session_id IN {in_sessions}"),
        format!("DELETE FROM commands WHERE session_id IN {in_sessions}"),
        format!("DELETE FROM run_sessions WHERE session_id IN {in_sessions}"),
        format!("DELETE FROM search WHERE session_id IN {in_sessions}"),
        "DELETE FROM sessions WHERE realm = ?".into(),
        "DELETE FROM trigger_runs WHERE trigger_id IN (SELECT id FROM triggers WHERE realm = ?)".into(),
        "DELETE FROM series_alerts WHERE trigger_id IN (SELECT id FROM triggers WHERE realm = ?)".into(),
        "DELETE FROM triggers WHERE realm = ?".into(),
        "DELETE FROM workflow_runs WHERE realm = ?".into(),
        "DELETE FROM decisions WHERE realm = ?".into(),
        "DELETE FROM record_history WHERE record_id IN (SELECT id FROM records WHERE realm = ?)".into(),
        "DELETE FROM records WHERE realm = ?".into(),
        "DELETE FROM collections WHERE realm = ?".into(),
        "DELETE FROM points WHERE realm = ?".into(),
        "DELETE FROM series WHERE realm = ?".into(),
        "DELETE FROM secrets WHERE realm = ?".into(),
        "DELETE FROM api_tokens WHERE realm = ?".into(),
        "DELETE FROM apps WHERE realm = ?".into(),
        "DELETE FROM realms WHERE id = ?".into(),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(sql)).bind(realm).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE grants SET state = 'revoked', revoked_at = ?1 WHERE (from_realm = ?2 OR to_realm = ?2) AND state IN ('active', 'requested')")
        .bind(now)
        .bind(realm)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    // Workers and connections kept to it serve every realm again.
    for table in ["workers", "connections"] {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT id, realms FROM {table} WHERE realms LIKE ?")))
            .bind(format!("%\"{realm}\"%"))
            .fetch_all(&st.store.pool)
            .await?;
        for r in rows {
            let mut realms: Vec<String> = serde_json::from_str(&r.get::<String, _>("realms")).unwrap_or_default();
            realms.retain(|x| x != realm);
            let id: String = r.try_get::<String, _>("id").unwrap_or_default();
            sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE {table} SET realms = ? WHERE id = ?")))
                .bind(serde_json::to_string(&realms)?)
                .bind(id)
                .execute(&st.store.pool)
                .await?;
        }
    }
    crate::memex::enqueue(st, "memex.drop_realm", json!({"realm": realm})).await;
    st.registry.bump();
    Ok(())
}

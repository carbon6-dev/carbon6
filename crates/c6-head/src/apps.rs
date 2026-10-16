//! The realm half of packages (docs/apps.md §5): creating what an app
//! instance owns in its realm (collections, series, triggers, the records
//! it starts with), replacing its code with another version's (nothing
//! merges: its code is its package's), removing what it owns, adopting
//! apps installed before instances, and exporting a realm or an instance
//! as a package. Installs, approvals and the registry are `packages`.

use std::collections::{BTreeMap, BTreeSet};

use c6_api as api;
use serde_json::{Value, json};
use sqlx::Row;
use tracing::warn;

use crate::realms::{COMMONS, ROOT};
use crate::{HeadState, bad_request};

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

/// An app's realm (packages are normalized so an app always names it).
pub(crate) fn realm_of(p: &api::AppPackage) -> String {
    p.realm.trim().to_owned()
}

pub(crate) fn check_package(p: &api::AppPackage) -> Vec<String> {
    let mut problems = Vec::new();
    let valid = |s: &str| !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid(p.app.trim()) {
        problems.push("a package's name is lowercase letters, digits and - (up to 32)".into());
    }
    let realm = realm_of(p);
    if !realm.is_empty() && [ROOT, COMMONS].contains(&realm.as_str()) {
        problems.push(format!("apps are not installed in {realm}"));
    }
    for key in p.contents.lists.keys() {
        if !LISTS.iter().any(|(k, _)| k == key) {
            problems.push(format!(
                "{key:?} is not something a package can bring ({})",
                LISTS.iter().map(|l| l.0).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    for c in p.contents.records.keys() {
        if !p.contents.collections.iter().any(|x| &x.name == c) {
            problems.push(format!("records for {c}, which the package does not define"));
        }
    }
    problems
}

fn count(what: &str, n: usize) -> Option<String> {
    (n > 0).then(|| format!("{n} {what}"))
}

/// What installing a package's realm half creates, in lines of Markdown.
pub(crate) fn contents_lines(p: &api::AppPackage) -> Vec<String> {
    let c = &p.contents;
    let mut lines = Vec::new();
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
    if what.is_empty() {
        return lines;
    }
    lines.push(format!("It creates: {}.", what.join(", ")));
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
    lines
}

/// Creates what an instance owns in its realm (docs/apps.md §5.1): its
/// collections, series and triggers, and the records it starts with, all
/// as the instance. Its other code (skills, templates, views, …) is read
/// from its package wherever it applies, so nothing of it is copied.
pub(crate) async fn apply_instance(st: &HeadState, realm: &str, name: &str, p: &api::AppPackage) -> anyhow::Result<()> {
    let c = &p.contents;
    let by = format!("app:{name}");
    for col in &c.collections {
        match crate::collections::get(st, realm, &col.name).await {
            Ok(existing) if existing.owner == name => {}
            Ok(existing) => {
                return Err(bad_request(format!(
                    "realm {realm} already has a collection {} ({})",
                    col.name,
                    if existing.owner.is_empty() { "the realm's".to_owned() } else { format!("the app {}'s", existing.owner) }
                )));
            }
            Err(_) => {
                crate::collections::create_owned(st, realm, col, &by, false, name).await?;
            }
        }
    }
    for s in &c.series {
        if crate::series::get(st, realm, &s.name).await.is_err() {
            crate::series::create_owned(st, realm, s, &by, name).await?;
        }
    }
    for t in &c.triggers {
        let filled: api::AutomationEdit =
            serde_json::from_value(crate::instances::fill_settings(&json!(t), &crate::instances::with_defaults(&p.settings, &json!({}))))?;
        crate::triggers::create_owned(st, filled, realm, name).await?;
    }
    for (col, recs) in &c.records {
        for fields in recs {
            let rec = api::RecordIn { fields: fields.clone(), links: None, labels: None, version: None };
            crate::collections::put(st, realm, col, None, &rec, &by).await?;
        }
    }
    Ok(())
}

/// What in a realm would clash with what an instance brings.
pub(crate) async fn clashes(st: &HeadState, realm: &str, name: &str, p: &api::AppPackage) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    if realm.is_empty() || !crate::realms::exists(st, realm).await? {
        return Ok(out);
    }
    for col in &p.contents.collections {
        if let Ok(c) = crate::collections::get(st, realm, &col.name).await
            && c.owner != name
        {
            out.push(format!(
                "realm {realm} already has a collection {} ({}): install it in another realm",
                col.name,
                if c.owner.is_empty() { "the realm's".to_owned() } else { format!("the app {}'s", c.owner) }
            ));
        }
    }
    for s in &p.contents.series {
        if let Ok(x) = crate::series::get(st, realm, &s.name).await
            && x.owner != name
        {
            out.push(format!("realm {realm} already has a series {}", s.name));
        }
    }
    Ok(out)
}

/// What an upgrade changes in an instance's code, in lines of Markdown.
pub(crate) fn code_changes(old: &api::AppPackage, new: &api::AppPackage) -> Vec<String> {
    let (a, b) = (items(&normalize(old)), items(&normalize(new)));
    let keys: BTreeSet<&(String, String)> = a.keys().chain(b.keys()).collect();
    let mut out = Vec::new();
    for k in keys {
        let what = match (a.get(k), b.get(k)) {
            (None, Some(_)) => "add",
            (Some(_), None) if k.0 == "collection" || k.0 == "series" => "stop shipping (its records stay)",
            (Some(_), None) => "remove",
            (Some(x), Some(y)) if x != y => "change",
            _ => continue,
        };
        out.push(format!("- {what} {}", name_of(&k.0, &k.1)));
    }
    out
}

/// Why the new version's collections would not take the records already
/// there: those need a migration, so the upgrade can't go ahead as it is.
pub(crate) async fn migration_problems(st: &HeadState, realm: &str, name: &str, new: &api::AppPackage) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for col in &new.contents.collections {
        let Ok(cur) = crate::collections::get(st, realm, &col.name).await else { continue };
        if cur.owner != name {
            out.push(format!("realm {realm} has a collection {} that is not {name}'s", col.name));
            continue;
        }
        let Some(schema) = &col.schema else { continue };
        if *schema == cur.schema {
            continue;
        }
        if let Err(e) = crate::schema::check_schema(schema) {
            out.push(format!("{}: {e}", col.name));
            continue;
        }
        let page =
            crate::collections::query(st, realm, &col.name, &api::RecordQuery { limit: Some(100_000), ..Default::default() }).await?;
        let misfits = page.records.iter().filter(|r| crate::schema::validate(schema, &r.fields).is_err()).count();
        if misfits > 0 {
            out.push(format!(
                "{}: the new version's schema rejects {misfits} of its records, so the change needs a migration the package does not give",
                col.name
            ));
        }
    }
    Ok(out)
}

/// Applies a new version's code to an instance: what it owns follows the
/// package (collections and series it adds are created, changed ones
/// updated, ones it stops shipping keep their records; its triggers are
/// the new version's, by name, keeping their webhook URLs and whether
/// they are on).
pub(crate) async fn apply_instance_upgrade(
    st: &HeadState,
    realm: &str,
    name: &str,
    old: &api::AppPackage,
    new: &api::AppPackage,
) -> anyhow::Result<()> {
    let by = format!("app:{name}");
    for col in &new.contents.collections {
        match crate::collections::get(st, realm, &col.name).await {
            Ok(cur) if cur.owner == name => {
                let changed = col.schema.as_ref().is_some_and(|s| *s != cur.schema)
                    || col.description.as_ref().is_some_and(|d| *d != cur.description)
                    || col.keys.as_ref().is_some_and(|k| *k != cur.keys);
                if changed {
                    crate::collections::update(st, realm, &col.name, col).await?;
                }
            }
            Ok(_) => return Err(bad_request(format!("realm {realm} has a collection {} that is not {name}'s", col.name))),
            Err(_) => {
                crate::collections::create_owned(st, realm, col, &by, false, name).await?;
            }
        }
    }
    for s in &new.contents.series {
        match crate::series::get(st, realm, &s.name).await {
            Ok(cur) if cur.owner == name => {
                crate::series::update(st, realm, &s.name, s).await?;
            }
            Ok(_) => {}
            Err(_) => {
                crate::series::create_owned(st, realm, s, &by, name).await?;
            }
        }
    }
    let settings = crate::instances::with_defaults(&new.settings, &json!({}));
    let mine: Vec<api::Automation> =
        crate::triggers::list(st, &crate::realms::Caller::in_realm(realm)).await?.into_iter().filter(|t| t.owner == name).collect();
    for t in &new.contents.triggers {
        let mut e: api::AutomationEdit = serde_json::from_value(crate::instances::fill_settings(&json!(t), &settings))?;
        match mine.iter().find(|m| Some(&m.name) == t.name.as_ref()) {
            Some(cur) => {
                e.enabled = Some(cur.enabled);
                crate::triggers::update(st, &cur.id, e).await?;
            }
            None => {
                crate::triggers::create_owned(st, e, realm, name).await?;
            }
        }
    }
    let names: Vec<&str> = new.contents.triggers.iter().filter_map(|t| t.name.as_deref()).collect();
    for cur in &mine {
        if !names.contains(&cur.name.as_str()) {
            crate::triggers::delete(st, &cur.id).await?;
        }
    }
    let _ = old;
    Ok(())
}

/// What an instance owns, in words (for its removal).
pub(crate) async fn owned_lines(st: &HeadState, realm: &str, name: &str) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for c in crate::collections::list(st, realm).await?.into_iter().filter(|c| c.owner == name) {
        out.push(format!("collection {} ({} records)", c.name, c.records));
    }
    for s in crate::series::list(st, realm).await?.into_iter().filter(|s| s.owner == name) {
        out.push(format!("series {}", s.name));
    }
    let n = crate::triggers::list(st, &crate::realms::Caller::in_realm(realm)).await?.into_iter().filter(|t| t.owner == name).count();
    if n > 0 {
        out.push(format!("{n} triggers"));
    }
    Ok(out)
}

/// Removes an instance's owned data and triggers (once you decided).
pub(crate) async fn remove_instance(st: &HeadState, realm: &str, name: &str) -> anyhow::Result<()> {
    let running: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sessions WHERE realm = ? AND instance = ? AND status IN ('starting', 'running')")
            .bind(realm)
            .bind(name)
            .fetch_one(&st.store.pool)
            .await?;
    if running > 0 {
        return Err(bad_request(format!("{running} of its sessions are still running: stop them first")));
    }
    for t in crate::triggers::list(st, &crate::realms::Caller::in_realm(realm)).await? {
        if t.owner == name {
            crate::triggers::delete(st, &t.id).await?;
        }
    }
    for c in crate::collections::list(st, realm).await? {
        if c.owner == name {
            crate::collections::drop(st, realm, &c.name).await?;
        }
    }
    for s in crate::series::list(st, realm).await? {
        if s.owner == name {
            crate::series::delete(st, realm, &s.name).await?;
        }
    }
    st.registry.bump();
    Ok(())
}

/// After installing: the grants it needs from other realms (asked as
/// decisions), and what is left for you (secrets, a worker of its own), as
/// a follow-up.
pub(crate) async fn next_steps(st: &HeadState, realm: &str, name: &str, p: &api::AppPackage) {
    for g in p.permissions.grants.iter().filter(|g| g.from_realm != realm) {
        let gi = api::GrantIn {
            from_realm: g.from_realm.clone(),
            to_realm: realm.to_owned(),
            kind: g.kind.clone(),
            scope: g.scope.clone(),
            mode: g.mode.clone(),
            expires_at: None,
            note: if g.note.is_empty() { format!("for the app {name}") } else { g.note.clone() },
        };
        if let Err(e) = crate::grants::ask(st, &gi, &format!("app:{name}")).await {
            warn!(app = %name, "asking for a grant: {e:#}");
        }
    }
    let have: Vec<String> =
        sqlx::query_scalar("SELECT name FROM secrets WHERE realm = ?").bind(realm).fetch_all(&st.store.pool).await.unwrap_or_default();
    let missing: Vec<&String> = p.permissions.secrets.iter().filter(|s| !have.contains(s)).collect();
    let mut todo = Vec::new();
    if !missing.is_empty() {
        todo.push(format!(
            "- Set its secrets in realm **{realm}** (Settings → Secrets, with {realm} picked in the header): {}",
            missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    if p.permissions.workers == "dedicated" {
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
        title: format!("Finish setting up {name}"),
        document: todo.join("\n"),
        source: "apps".into(),
        ..Default::default()
    };
    if let Err(e) = crate::decisions::open(st, &d, realm, "").await {
        warn!(app = %name, "{e:#}");
    }
}

/// Apps installed before instances wrote their code into their realm's
/// settings lists. Now their code is read from their package, so the
/// copies go: entries still as installed are removed; entries you edited
/// stay as the realm's own (they override the app's for the realm's
/// sessions), and a decision asks whether to keep them (docs/apps.md §14).
pub async fn adopt(st: &HeadState) -> anyhow::Result<()> {
    let rows = sqlx::query("SELECT id, realm, name, digest FROM installs WHERE adopted = 0 AND realm != '' AND state = 'installed'")
        .fetch_all(&st.store.pool)
        .await?;
    for r in rows {
        let (id, realm, name, digest): (i64, String, String, String) = (r.get("id"), r.get("realm"), r.get("name"), r.get("digest"));
        let Some((p, _)) = crate::packages::by_digest(st, &digest).await? else { continue };
        let shipped = normalize(&p);
        let mut edited = Vec::new();
        for (key, entries) in &shipped.contents.lists {
            let Some(l) = crate::lists::list(key) else { continue };
            let mut own = crate::lists::read(&st.store.pool, key, &realm).await?;
            let before = own.len();
            let mut probe = api::AppPackage::default();
            own.retain(|e| {
                probe.contents.lists = BTreeMap::from([(key.clone(), vec![e.clone()])]);
                let e_norm = normalize(&probe).contents.lists.remove(key).and_then(|mut v| v.pop()).unwrap_or(Value::Null);
                match entries.iter().find(|s| crate::lists::identity(l, s) == crate::lists::identity(l, e)) {
                    Some(s) if *s == e_norm => false,
                    Some(_) => {
                        edited.push(name_of(&format!("list:{key}"), &crate::lists::identity(l, e)));
                        true
                    }
                    None => true,
                }
            });
            if own.len() != before {
                crate::lists::write(&st.store.pool, key, &realm, &own, "upgrade").await?;
            }
        }
        sqlx::query("UPDATE installs SET adopted = 1 WHERE id = ?").bind(id).execute(&st.store.pool).await?;
        if !edited.is_empty() {
            let d = api::DecisionIn {
                kind: "approve".into(),
                title: format!("Keep your edits to {name}?"),
                document: format!(
                    "**{name}** now reads its code from its package, which nothing changes but an upgrade. You had edited some of what it \
                     installed in realm **{realm}**; those stay as the realm's own, and override the app's for the realm's sessions:\n\n{}\n\n\
                     Keep them, or use the app's? To change the app itself, export it as a fork (Settings → System → Packages).",
                    edited.iter().map(|e| format!("- {e}")).collect::<Vec<_>>().join("\n")
                ),
                options: ["Keep my edits", "Use the app's"]
                    .iter()
                    .map(|o| api::DecisionOption { label: (*o).to_owned(), does: String::new() })
                    .collect(),
                source: "packages".into(),
                context: json!({"install": id, "edited": edited}),
                ..Default::default()
            };
            crate::decisions::open(st, &d, ROOT, &format!("package:adopt:{id}")).await?;
        }
    }
    Ok(())
}

/// Your answer to keeping edits made before instances.
pub(crate) async fn answer_adopt(st: &HeadState, id: i64, option: Option<&str>) -> anyhow::Result<()> {
    if option != Some("Use the app's") {
        return Ok(());
    }
    let i = crate::packages::install_by_id(st, id).await?;
    let (p, _) = crate::packages::by_digest(st, &i.digest).await?.unwrap_or_default();
    for key in p.contents.lists.keys() {
        let Some(l) = crate::lists::list(key) else { continue };
        let shipped: Vec<String> = p.contents.lists[key].iter().map(|e| crate::lists::identity(l, e)).collect();
        let mut own = crate::lists::read(&st.store.pool, key, &i.realm).await?;
        let before = own.len();
        own.retain(|e| !shipped.contains(&crate::lists::identity(l, e)));
        if own.len() != before {
            crate::lists::write(&st.store.pool, key, &i.realm, &own, "you").await?;
        }
    }
    st.registry.bump();
    Ok(())
}

/// An instance as a package (its package's manifest); `fork`: as a new
/// package of that name, forked from it (docs/apps.md §5.6); with `data`,
/// the records of the collections it owns.
pub async fn export_instance(st: &HeadState, realm: &str, name: &str, fork: &str, data: bool) -> anyhow::Result<api::AppPackage> {
    let i = crate::packages::installs(st)
        .await?
        .into_iter()
        .find(|i| i.realm == realm && i.name == name)
        .ok_or_else(|| crate::not_found(format!("no app {name} in realm {realm}")))?;
    let (mut p, _) = crate::packages::by_digest(st, &i.digest).await?.unwrap_or_default();
    if !fork.trim().is_empty() {
        p.forked_from = p.app.clone();
        p.app = fork.trim().to_owned();
        p.realm = String::new();
    }
    if data {
        for c in crate::collections::list(st, realm).await?.into_iter().filter(|c| c.owner == name) {
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
    }
    Ok(p)
}

/// A realm's configuration as a package; with `data`, its records too.
pub async fn export(st: &HeadState, realm: &str, data: bool) -> anyhow::Result<api::AppPackage> {
    if !crate::realms::exists(st, realm).await? || realm == ROOT {
        return Err(bad_request(format!("no realm {realm:?} to export")));
    }
    let installed = crate::packages::app_in(st, realm).await?;
    let mut p = api::AppPackage {
        app: installed.as_ref().map(|(i, _)| i.package.clone()).unwrap_or_else(|| realm.to_owned()),
        version: installed.as_ref().map(|(i, _)| i.version.clone()).unwrap_or_default(),
        description: installed.as_ref().map(|(_, m)| m.description.clone()).unwrap_or_default(),
        realm: realm.to_owned(),
        requires: installed.as_ref().map(|(_, m)| m.requires.clone()).unwrap_or_default(),
        exports: installed.as_ref().map(|(_, m)| m.exports.clone()).unwrap_or_default(),
        ..Default::default()
    };
    // The realm's own: what its apps own is theirs (export_instance).
    for c in crate::collections::list(st, realm).await?.into_iter().filter(|c| !c.system && c.owner.is_empty()) {
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
    for s in crate::series::list(st, realm).await?.into_iter().filter(|s| !s.system && s.owner.is_empty()) {
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
    for t in crate::triggers::list(st, &crate::realms::Caller::in_realm(realm)).await?.into_iter().filter(|t| t.owner.is_empty()) {
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
    p.permissions.secrets =
        sqlx::query_scalar("SELECT name FROM secrets WHERE realm = ? ORDER BY name").bind(realm).fetch_all(&st.store.pool).await?;
    p.permissions.grants = crate::grants::list(st, Some(realm))
        .await?
        .into_iter()
        .filter(|g| g.to_realm == realm && matches!(g.state.as_str(), "active" | "requested"))
        .map(|g| api::AppGrant { from_realm: g.from_realm, kind: g.kind, mode: g.mode, scope: g.scope, note: g.note })
        .collect();
    let workers = crate::api::workers_snapshot(st).await.unwrap_or_default();
    p.permissions.workers = if workers.iter().any(|w| w.realms == [realm.to_owned()]) { "dedicated".into() } else { "any".into() };
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

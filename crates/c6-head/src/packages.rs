//! Packages (docs/packages.md): the local registry and what is installed.
//!
//! - **Registry:** package versions, each content-addressed by a digest of
//!   its manifest and files.
//! - **Installs:** installing a package resolves what it requires (and
//!   what those require), picking the newest version each range allows.
//!   Installs are shared, every requirement is locked to a digest, and
//!   cycles are refused.
//! - **Approval:** one decision lists everything the new packages ask for;
//!   once you approve, apps get their realms, libraries join the requiring
//!   realm, kits become available to workers, and apps that require apps
//!   may start their exported templates.
//! - **Upgrades** merge an app's or a library's configuration three ways
//!   and ask only for what grew. **Removing** an app deletes its realm.

use std::collections::BTreeMap;

use base64::Engine;
use c6_api as api;
use c6_core::now_ms;
use serde_json::{Value, json};
use sqlx::Row;

use crate::realms::{COMMONS, HOME, ROOT};
use crate::{HeadState, bad_request, not_found};

// --- Manifests -------------------------------------------------------------------

/// A manifest as carbon6 keeps it: `package` (an `app` is a package with a
/// realm of its own, its name unless given), and `needs` folded into
/// `permissions`.
pub fn normalize_manifest(mut v: Value) -> anyhow::Result<api::AppPackage> {
    if let Some(o) = v.as_object_mut()
        && let Some(name) = o.remove("app")
    {
        if o.get("realm").and_then(Value::as_str).is_none_or(str::is_empty) {
            o.insert("realm".into(), name.clone());
        }
        o.entry("package").or_insert(name);
    }
    let mut p: api::AppPackage = serde_json::from_value(v).map_err(|e| bad_request(format!("the manifest: {e}")))?;
    p.app = p.app.trim().to_owned();
    p.version = p.version.trim().to_owned();
    if p.version.is_empty() {
        p.version = "0.0.0".into();
    }
    let needs = std::mem::take(&mut p.needs);
    p.permissions.secrets.extend(needs.secrets);
    if !needs.workers.is_empty() {
        p.permissions.workers = needs.workers;
    }
    p.permissions.grants.extend(needs.grants);
    p.permissions.secrets.sort();
    p.permissions.secrets.dedup();
    Ok(p)
}

/// What a package is: an app (it owns data: collections, series,
/// triggers, settings, or it names a realm as packages first did), a kit
/// (an environment), a library (configuration that owns nothing). Apps and
/// libraries are installed as instances in a realm (docs/apps.md §5).
pub fn roles(p: &api::AppPackage) -> Vec<String> {
    let mut r = Vec::new();
    let c = &p.contents;
    let owns =
        !c.collections.is_empty() || !c.series.is_empty() || !c.triggers.is_empty() || !p.settings.is_null() || !p.operations.is_empty()
        || !p.providers.is_empty();
    if owns || !p.realm.is_empty() {
        r.push("app".to_owned());
    }
    if p.kit.is_some() {
        r.push("kit".to_owned());
    }
    if !owns && p.realm.is_empty() && !p.contents.is_empty() {
        r.push("library".to_owned());
    }
    r
}

/// The content address of a package: its manifest and its files.
pub fn digest_of(p: &api::AppPackage, files: &BTreeMap<String, Vec<u8>>) -> String {
    // serde_json keeps object keys sorted, so this text is canonical.
    let mut text = serde_json::to_string(&serde_json::to_value(p).unwrap_or(Value::Null)).unwrap_or_default();
    for (path, data) in files {
        text.push_str(&format!("\n{path} {}", c6_core::sha256_hex(data)));
    }
    format!("sha256:{}", c6_core::sha256_hex(text.as_bytes()))
}

/// A version range: `^1.2`, `~1.2`, `>=1.0`, `*`; a bare version is exact.
pub fn range(req: &str) -> anyhow::Result<semver::VersionReq> {
    let req = req.trim();
    if req.is_empty() || req == "*" {
        return Ok(semver::VersionReq::STAR);
    }
    if let Ok(v) = semver::Version::parse(req) {
        return Ok(semver::VersionReq::parse(&format!("={v}"))?);
    }
    semver::VersionReq::parse(req).map_err(|e| bad_request(format!("{req:?} is not a version range: {e}")))
}

/// The kits a Dockerfile builds from (`kit:<name>`).
pub fn kit_refs(dockerfile: &str) -> Vec<String> {
    let mut out: Vec<String> = dockerfile
        .split(|c: char| c.is_whitespace())
        .filter_map(|w| w.strip_prefix("kit:"))
        .map(|n| n.split([':', '@']).next().unwrap_or(n).to_owned())
        .filter(|n| !n.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

fn check_manifest(p: &api::AppPackage, files: &BTreeMap<String, Vec<u8>>) -> Vec<String> {
    let mut problems = crate::apps::check_package(p);
    if semver::Version::parse(&p.version).is_err() {
        problems.push(format!("{:?} is not a version (like 1.2.3)", p.version));
    }
    for (name, req) in &p.requires {
        if name == &p.app {
            problems.push("a package cannot require itself".into());
        }
        if let Err(e) = range(req) {
            problems.push(format!("{name}: {e}"));
        }
    }
    let perm = &p.permissions;
    if !matches!(perm.files.as_str(), "session" | "session-ro") {
        problems.push("files is session or session-ro".into());
    }
    if !matches!(perm.network.as_str(), "none" | "any" | "hosts") {
        problems.push("network is none, any, or hosts (with the hosts it may reach)".into());
    }
    if perm.network == "hosts"
        && (perm.hosts.is_empty()
            || perm.hosts.iter().any(|h| h.is_empty() || !h.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')))
    {
        problems.push("network hosts lists the host names its runs may reach (like gmail.googleapis.com)".into());
    }
    if !matches!(perm.workers.as_str(), "" | "any" | "dedicated") {
        problems.push("workers is any or dedicated".into());
    }
    if !(perm.cpus > 0.0 && perm.cpus <= 64.0) || perm.memory_mb < 64 || perm.timeout_s == 0 || perm.timeout_s > 24 * 3600 {
        problems.push("cpus, memory_mb and timeout_s must be sensible (0–64 CPUs, at least 64 MB, up to a day)".into());
    }
    if let Some(k) = &p.kit {
        match (&k.image, files.get(&k.build.dockerfile)) {
            (Some(_), _) => problems.push("prebuilt images come with remote registries; give a Dockerfile".into()),
            (None, None) => problems.push(format!("the kit's {} is not among the package's files", k.build.dockerfile)),
            (None, Some(df)) => {
                for r in kit_refs(&String::from_utf8_lossy(df)) {
                    if !p.requires.contains_key(&r) {
                        problems.push(format!("the Dockerfile builds from kit:{r}, which the package does not require"));
                    }
                }
            }
        }
        for f in &k.build.files {
            if !files.contains_key(f) {
                problems.push(format!("the kit's file {f} is not among the package's files"));
            }
        }
        for t in &k.tools {
            if t.name.is_empty() || !t.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') || t.command.is_empty() {
                problems.push(format!("tool {:?}: a name (letters, digits, - and _) and a command", t.name));
            }
            for (a, kind) in &t.args {
                if !matches!(kind.as_str(), "path" | "text") {
                    problems.push(format!("tool {}: argument {a} is path or text", t.name));
                }
            }
        }
    }
    problems.extend(crate::operations::check(p));
    problems.extend(crate::providers::check(p));
    if roles(p).is_empty() {
        problems.push("a package has contents (an app or a library), settings, or a kit".into());
    }
    if !p.settings.is_null() && (p.settings["type"] != "object" || crate::schema::check_schema(&p.settings).is_err()) {
        problems.push("settings is a JSON Schema of an object".into());
    }
    problems
}

// --- The registry ------------------------------------------------------------------

fn info_of(r: &sqlx::sqlite::SqliteRow) -> api::PackageInfo {
    let m: api::AppPackage = serde_json::from_str(&r.get::<String, _>("manifest")).unwrap_or_default();
    api::PackageInfo {
        name: r.get("name"),
        version: r.get("version"),
        digest: r.get("digest"),
        roles: serde_json::from_str(&r.get::<String, _>("roles")).unwrap_or_default(),
        description: m.description,
        source: r.get("source"),
        added_at: r.get("added_at"),
        installed_in: Vec::new(),
        requires: m.requires,
        files: Vec::new(),
    }
}

/// Adds a package version to the registry (the same version again with the
/// same content is fine; with other content, refused).
pub async fn add(st: &HeadState, manifest: Value, files: BTreeMap<String, Vec<u8>>, source: &str) -> anyhow::Result<api::PackageInfo> {
    let p = normalize_manifest(manifest)?;
    let problems = check_manifest(&p, &files);
    if !problems.is_empty() {
        return Err(bad_request(problems.join("; ")));
    }
    let digest = digest_of(&p, &files);
    if let Some(existing) = sqlx::query_scalar::<_, String>("SELECT digest FROM packages WHERE name = ? AND version = ?")
        .bind(&p.app)
        .bind(&p.version)
        .fetch_optional(&st.store.pool)
        .await?
    {
        if existing != digest {
            return Err(bad_request(format!(
                "{} {} is already in the registry with other content: give it a new version",
                p.app, p.version
            )));
        }
        return get(st, &p.app, &p.version).await.map(|x| x.1);
    }
    let mut tx = st.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("INSERT INTO packages (digest, name, version, manifest, roles, source, added_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(&digest)
        .bind(&p.app)
        .bind(&p.version)
        .bind(serde_json::to_string(&p)?)
        .bind(serde_json::to_string(&roles(&p))?)
        .bind(source)
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
    for (path, data) in &files {
        sqlx::query("INSERT INTO package_files (digest, path, data) VALUES (?, ?, ?)")
            .bind(&digest)
            .bind(path)
            .bind(data)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    st.registry.bump();
    get(st, &p.app, &p.version).await.map(|x| x.1)
}

/// `POST /api/v1/packages`: files come as base64.
pub async fn add_in(st: &HeadState, p: api::PackageIn) -> anyhow::Result<api::PackageInfo> {
    let mut files = BTreeMap::new();
    for (path, b64) in p.files {
        if path.is_empty() || path.starts_with('/') || path.split('/').any(|s| s == "..") {
            return Err(bad_request(format!("{path:?} is not a file path inside the package")));
        }
        let data = base64::engine::general_purpose::STANDARD.decode(b64.trim()).map_err(|e| bad_request(format!("{path}: {e}")))?;
        files.insert(path, data);
    }
    for (path, text) in p.text_files {
        if path.is_empty() || path.starts_with('/') || path.split('/').any(|s| s == "..") {
            return Err(bad_request(format!("{path:?} is not a file path inside the package")));
        }
        if files.insert(path.clone(), text.into_bytes()).is_some() {
            return Err(bad_request(format!("{path} is given twice")));
        }
    }
    add(st, p.manifest, files, "local").await
}

pub async fn list(st: &HeadState) -> anyhow::Result<Vec<api::PackageInfo>> {
    let rows = sqlx::query("SELECT * FROM packages ORDER BY name, added_at").fetch_all(&st.store.pool).await?;
    let installs = installs(st).await?;
    Ok(rows
        .iter()
        .map(|r| {
            let mut i = info_of(r);
            i.installed_in = installs.iter().filter(|x| x.digest == i.digest).map(|x| x.realm.clone()).collect();
            i
        })
        .collect())
}

/// A package version: its manifest and its registry entry (with its files).
pub async fn get(st: &HeadState, name: &str, version: &str) -> anyhow::Result<(api::AppPackage, api::PackageInfo)> {
    let r = sqlx::query("SELECT * FROM packages WHERE name = ? AND version = ?")
        .bind(name)
        .bind(version)
        .fetch_optional(&st.store.pool)
        .await?
        .ok_or_else(|| not_found(format!("no package {name} {version} in the registry")))?;
    let digest: String = r.get("digest");
    by_digest(st, &digest).await?.ok_or_else(|| not_found("no such package"))
}

pub async fn by_digest(st: &HeadState, digest: &str) -> anyhow::Result<Option<(api::AppPackage, api::PackageInfo)>> {
    let Some(r) = sqlx::query("SELECT * FROM packages WHERE digest = ?").bind(digest).fetch_optional(&st.store.pool).await? else {
        return Ok(None);
    };
    let mut info = info_of(&r);
    info.files =
        sqlx::query_scalar("SELECT path FROM package_files WHERE digest = ? ORDER BY path").bind(digest).fetch_all(&st.store.pool).await?;
    Ok(Some((serde_json::from_str(&r.get::<String, _>("manifest"))?, info)))
}

pub async fn file(st: &HeadState, digest: &str, path: &str) -> anyhow::Result<Vec<u8>> {
    sqlx::query_scalar("SELECT data FROM package_files WHERE digest = ? AND path = ?")
        .bind(digest)
        .bind(path)
        .fetch_optional(&st.store.pool)
        .await?
        .ok_or_else(|| not_found(format!("no file {path}")))
}

/// Removes a version nothing has installed.
pub async fn delete(st: &HeadState, name: &str, version: &str) -> anyhow::Result<()> {
    let (_, info) = get(st, name, version).await?;
    if sqlx::query_scalar::<_, i64>("SELECT count(*) FROM installs WHERE digest = ?").bind(&info.digest).fetch_one(&st.store.pool).await?
        > 0
    {
        return Err(bad_request(format!("{name} {version} is installed; remove it first")));
    }
    sqlx::query("DELETE FROM package_files WHERE digest = ?").bind(&info.digest).execute(&st.store.pool).await?;
    sqlx::query("DELETE FROM packages WHERE digest = ?").bind(&info.digest).execute(&st.store.pool).await?;
    st.registry.bump();
    Ok(())
}

/// Apps installed before packages came: their digests, now that carbon6
/// can compute them (the migration left `legacy:<realm>`).
pub async fn fix_legacy(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let rows = sqlx::query("SELECT digest, manifest FROM packages WHERE digest LIKE 'legacy:%'").fetch_all(pool).await?;
    for r in rows {
        let old: String = r.get("digest");
        let Ok(p) = normalize_manifest(serde_json::from_str(&r.get::<String, _>("manifest")).unwrap_or(json!({}))) else { continue };
        let digest = digest_of(&p, &BTreeMap::new());
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE packages SET digest = ?, manifest = ?, version = ? WHERE digest = ?")
            .bind(&digest)
            .bind(serde_json::to_string(&p)?)
            .bind(&p.version)
            .bind(&old)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE installs SET digest = ?, version = ?, approved = ? WHERE digest = ?")
            .bind(&digest)
            .bind(&p.version)
            .bind(serde_json::to_string(&p.permissions)?)
            .bind(&old)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(())
}

// --- Installs ----------------------------------------------------------------------

/// Packages by digest: their roles and manifests.
async fn packages_by_digest(st: &HeadState) -> anyhow::Result<BTreeMap<String, (Vec<String>, api::AppPackage)>> {
    Ok(sqlx::query("SELECT digest, roles, manifest FROM packages")
        .fetch_all(&st.store.pool)
        .await?
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("digest"),
                (
                    serde_json::from_str(&r.get::<String, _>("roles")).unwrap_or_default(),
                    serde_json::from_str(&r.get::<String, _>("manifest")).unwrap_or_default(),
                ),
            )
        })
        .collect())
}

/// Digests checked against their content since the head started: a
/// package's code can't change under its digest (docs/apps.md §5.5).
static VERIFIED: std::sync::LazyLock<std::sync::Mutex<BTreeMap<String, bool>>> = std::sync::LazyLock::new(Default::default);

/// Whether a package's manifest and files still hash to its digest.
async fn verified(st: &HeadState, digest: &str) -> bool {
    if let Some(ok) = VERIFIED.lock().ok().and_then(|v| v.get(digest).copied()) {
        return ok;
    }
    let ok = async {
        let (p, info) = by_digest(st, digest).await.ok()??;
        let mut files = BTreeMap::new();
        for f in &info.files {
            files.insert(f.clone(), file(st, digest, f).await.ok()?);
        }
        Some(digest_of(&p, &files) == digest)
    }
    .await
    .unwrap_or(false);
    if let Ok(mut v) = VERIFIED.lock() {
        v.insert(digest.to_owned(), ok);
    }
    ok
}

fn install_of(r: &sqlx::sqlite::SqliteRow, packages: &BTreeMap<String, (Vec<String>, api::AppPackage)>) -> api::Install {
    let digest: String = r.get("digest");
    let (roles, manifest) = packages.get(&digest).cloned().unwrap_or_default();
    let approved: Value = serde_json::from_str(&r.get::<String, _>("approved")).unwrap_or(Value::Null);
    let values: Value = serde_json::from_str(&r.get::<String, _>("settings")).unwrap_or(json!({}));
    api::Install {
        id: r.get("id"),
        package: r.get("package"),
        version: r.get("version"),
        roles,
        digest,
        realm: r.get("realm"),
        name: r.get("name"),
        state: r.get("state"),
        settings: crate::instances::with_defaults(&manifest.settings, &values),
        reads: serde_json::from_value(approved["reads"].clone()).unwrap_or_default(),
        problems: Vec::new(),
        providers: manifest.providers.iter().map(|p| p.name.clone()).collect(),
        locks: serde_json::from_str(&r.get::<String, _>("locks")).unwrap_or_default(),
        required_by: serde_json::from_str(&r.get::<String, _>("required_by")).unwrap_or_default(),
        decision: r.get("decision"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

pub async fn installs(st: &HeadState) -> anyhow::Result<Vec<api::Install>> {
    let packages = packages_by_digest(st).await?;
    let rows = sqlx::query("SELECT * FROM installs ORDER BY package, realm, name").fetch_all(&st.store.pool).await?;
    let mut out: Vec<api::Install> = rows.iter().map(|r| install_of(r, &packages)).collect();
    for i in &mut out {
        if !verified(st, &i.digest).await {
            i.problems.push("its code no longer matches the package it installed (the digest differs): reinstall it".into());
        }
    }
    Ok(out)
}

pub async fn install_by_id(st: &HeadState, id: i64) -> anyhow::Result<api::Install> {
    installs(st).await?.into_iter().find(|i| i.id == id).ok_or_else(|| not_found("no such install"))
}

async fn row(st: &HeadState, id: i64) -> anyhow::Result<sqlx::sqlite::SqliteRow> {
    sqlx::query("SELECT * FROM installs WHERE id = ?")
        .bind(id)
        .fetch_optional(&st.store.pool)
        .await?
        .ok_or_else(|| not_found("no such install"))
}

/// An app installed in a realm (the one named after the realm, else the
/// first), with its manifest.
pub async fn app_in(st: &HeadState, realm: &str) -> anyhow::Result<Option<(api::Install, api::AppPackage)>> {
    let mut apps: Vec<api::Install> =
        installs(st).await?.into_iter().filter(|i| i.realm == realm && i.roles.iter().any(|r| r == "app")).collect();
    apps.sort_by_key(|i| (i.name != realm, i.id));
    let Some(i) = apps.into_iter().next() else { return Ok(None) };
    let m = by_digest(st, &i.digest).await?.map(|x| x.0).unwrap_or_default();
    Ok(Some((i, m)))
}

/// One package to install (or already installed), found while resolving.
#[derive(Clone, Debug)]
struct Node {
    pkg: api::AppPackage,
    digest: String,
    roles: Vec<String>,
    /// The realm it is installed in (apps, libraries), or "" (kits).
    target: String,
    /// Its instance's name there.
    name: String,
    /// Already installed: that install.
    existing: Option<i64>,
    /// What it requires: name → digest.
    locks: BTreeMap<String, String>,
    /// The nodes that require it.
    parents: Vec<usize>,
}

/// Whether a package is installed as an instance in a realm (an app or a
/// library) rather than for workers (a kit).
fn in_realm(roles: &[String]) -> bool {
    roles.iter().any(|r| r == "app" || r == "library")
}

/// The newest version in the registry a range allows.
async fn newest(st: &HeadState, name: &str, req: &str) -> anyhow::Result<Option<(api::AppPackage, String)>> {
    let req = range(req)?;
    let rows = sqlx::query("SELECT digest, version, manifest FROM packages WHERE name = ?").bind(name).fetch_all(&st.store.pool).await?;
    let mut best: Option<(semver::Version, String, String)> = None;
    for r in rows {
        let Ok(v) = semver::Version::parse(&r.get::<String, _>("version")) else { continue };
        if req.matches(&v) && best.as_ref().is_none_or(|b| v > b.0) {
            best = Some((v, r.get("digest"), r.get("manifest")));
        }
    }
    Ok(best.map(|(_, d, m)| (serde_json::from_str(&m).unwrap_or_default(), d)))
}

fn valid_instance_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 32 && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && !n.starts_with('-')
}

/// The realm an app or library goes in: the one asked for, else the one
/// the package suggests if it exists, else home. Installing never makes a
/// realm (docs/apps.md §4).
async fn target_realm(st: &HeadState, p: &api::AppPackage, asked: &str) -> anyhow::Result<String> {
    if !in_realm(&roles(p)) {
        return Ok(String::new());
    }
    let asked = asked.trim();
    let suggested = p.realm.trim();
    let realm = if !asked.is_empty() {
        asked.to_owned()
    } else if !suggested.is_empty() && crate::realms::exists(st, suggested).await? && ![ROOT, COMMONS].contains(&suggested) {
        suggested.to_owned()
    } else {
        HOME.to_owned()
    };
    if realm == ROOT || realm == COMMONS {
        return Err(bad_request(format!("apps are not installed in {realm}; pick a realm that holds data")));
    }
    if !crate::realms::exists(st, &realm).await? {
        return Err(bad_request(format!(
            "there is no realm {realm:?}: create it first (Settings → System → Realms), then install into it"
        )));
    }
    Ok(realm)
}

/// Resolves a package and what it requires: nodes in install order
/// (requirements first), and what stands in the way. Apps and libraries a
/// package requires go in the same realm.
async fn resolve(
    st: &HeadState,
    root: api::AppPackage,
    root_digest: String,
    realm: &str,
    name: &str,
) -> anyhow::Result<(Vec<Node>, Vec<String>)> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut problems = Vec::new();
    let existing = installs(st).await?;
    // Depth first, children before parents.
    #[allow(clippy::too_many_arguments)]
    async fn visit(
        st: &HeadState,
        pkg: api::AppPackage,
        digest: String,
        target: String,
        name: String,
        path: &mut Vec<String>,
        nodes: &mut Vec<Node>,
        problems: &mut Vec<String>,
        existing: &[api::Install],
    ) -> anyhow::Result<usize> {
        let pkg_roles = roles(&pkg);
        // Already in this plan for the same place.
        if let Some(i) = nodes.iter().position(|n| n.pkg.app == pkg.app && n.target == target) {
            if nodes[i].digest != digest {
                problems.push(format!("{} is required in two versions ({} and {})", pkg.app, nodes[i].pkg.version, pkg.version));
            }
            return Ok(i);
        }
        if path.contains(&pkg.app) {
            problems.push(format!("a cycle: {} → {}", path.join(" → "), pkg.app));
            return Ok(usize::MAX);
        }
        path.push(pkg.app.clone());
        let mut locks = BTreeMap::new();
        let mut children = Vec::new();
        for (req_name, req) in pkg.requires.clone() {
            let Some((child, cdigest)) = newest(st, &req_name, &req).await? else {
                problems.push(format!("{} requires {req_name} {req}, which is not in the registry", pkg.app));
                continue;
            };
            let ctarget = if in_realm(&roles(&child)) {
                if target.is_empty() {
                    problems.push(format!("{} (a kit) cannot require {req_name}, which is installed in a realm", pkg.app));
                    continue;
                }
                target.clone()
            } else {
                String::new()
            };
            // An installed version that meets the range is used as it is.
            if let Some(inst) = existing.iter().find(|i| i.package == req_name && i.realm == ctarget && i.state == "installed") {
                let ok = semver::Version::parse(&inst.version).ok().is_some_and(|v| range(&req).map(|r| r.matches(&v)).unwrap_or(false));
                if !ok {
                    problems.push(format!(
                        "{} requires {req_name} {req}, but {} is installed{}; upgrade it first",
                        pkg.app,
                        inst.version,
                        where_(&ctarget)
                    ));
                    continue;
                }
                locks.insert(req_name.clone(), inst.digest.clone());
                let i = match nodes.iter().position(|n| n.existing == Some(inst.id)) {
                    Some(i) => i,
                    None => {
                        let (m, _) = by_digest(st, &inst.digest).await?.unwrap_or_default();
                        nodes.push(Node {
                            roles: roles(&m),
                            pkg: m,
                            digest: inst.digest.clone(),
                            target: ctarget,
                            name: inst.name.clone(),
                            existing: Some(inst.id),
                            locks: inst.locks.clone(),
                            parents: Vec::new(),
                        });
                        nodes.len() - 1
                    }
                };
                children.push(i);
                continue;
            }
            locks.insert(req_name.clone(), cdigest.clone());
            let cname = req_name.clone();
            let i = Box::pin(visit(st, child, cdigest, ctarget, cname, path, nodes, problems, existing)).await?;
            if i != usize::MAX {
                children.push(i);
            }
        }
        path.pop();
        let i = nodes.len();
        let existing_here = existing.iter().find(|x| x.realm == target && x.name == name).map(|x| x.id);
        nodes.push(Node { pkg, digest, roles: pkg_roles, target, name, existing: existing_here, locks, parents: Vec::new() });
        for c in children {
            nodes[c].parents.push(i);
        }
        Ok(i)
    }
    let target = if in_realm(&roles(&root)) { realm.to_owned() } else { String::new() };
    let name = if name.trim().is_empty() { root.app.clone() } else { name.trim().to_owned() };
    if !valid_instance_name(&name) {
        problems.push("an instance's name is lowercase letters, digits and - (up to 32)".into());
    }
    visit(st, root, root_digest, target, name, &mut Vec::new(), &mut nodes, &mut problems, &existing).await?;
    Ok((nodes, problems))
}

fn where_(target: &str) -> String {
    if target.is_empty() { String::new() } else { format!(" in {target}") }
}

/// What a package asks for, in plain words.
pub fn permission_lines(p: &api::AppPackage) -> Vec<String> {
    let mut out = Vec::new();
    let perm = &p.permissions;
    if p.kit.is_some() {
        let files =
            if perm.files == "session-ro" { "Read files in the session's folder" } else { "Read and write files in the session's folder" };
        let hosts = format!("Reach only {}", perm.hosts.join(", "));
        let net = match perm.network.as_str() {
            "any" => "Reach the network",
            "hosts" => hosts.as_str(),
            _ => "No network",
        };
        let mem =
            if perm.memory_mb >= 1024 { format!("{:.1} GB", perm.memory_mb as f64 / 1024.0) } else { format!("{} MB", perm.memory_mb) };
        let time = if perm.timeout_s >= 60 { format!("{} minutes", perm.timeout_s / 60) } else { format!("{} seconds", perm.timeout_s) };
        out.push(format!("{files}. {net}. Up to {} CPUs and {mem} of memory, {time} a run.", perm.cpus));
        if perm.shell {
            out.push("Agents may run any command in it.".into());
        }
        if let Some(k) = &p.kit {
            let tools: Vec<&str> = k.tools.iter().map(|t| t.name.as_str()).collect();
            if !tools.is_empty() {
                out.push(format!("Tools: {}.", tools.join(", ")));
            }
        }
        let from: Vec<String> = p.requires.keys().cloned().collect();
        if !from.is_empty() {
            out.push(format!("Built from or with: {}. Builds reach the network to fetch what they need.", from.join(", ")));
        } else {
            out.push("Builds reach the network to fetch what they need.".into());
        }
    }
    out.extend(read_lines(&p.reads));
    if !perm.secrets.is_empty() {
        out.push(format!("Needs secrets you set: {}.", perm.secrets.join(", ")));
    }
    if perm.workers == "dedicated" {
        out.push("Wants a worker of its own.".into());
    }
    for g in &perm.grants {
        out.push(format!("Asks for a grant: {} {} of realm {} ({}).", g.mode, g.kind, g.from_realm, g.scope));
    }
    out
}

/// What an instance reads in its realm, in plain words (docs/apps.md §5.2).
pub fn read_lines(r: &api::Reads) -> Vec<String> {
    let mut out = Vec::new();
    if !r.items.kinds.is_empty() {
        out.push(format!("Reads knowledge in its realm: {}.", r.items.kinds.join(", ")));
    }
    if !r.collections.is_empty() {
        out.push(format!("Reads collections in its realm: {}.", r.collections.join(", ")));
    }
    out
}

/// What grew from one version's permissions to another's.
fn grown(old: &api::AppPackage, new: &api::AppPackage) -> Vec<String> {
    let (o, n) = (&old.permissions, &new.permissions);
    let mut out = Vec::new();
    if o.network != n.network && n.network == "any" {
        out.push("network access".to_owned());
    }
    let more_hosts: Vec<&String> = n.hosts.iter().filter(|h| o.network != "any" && !o.hosts.contains(h)).collect();
    if n.network == "hosts" && !more_hosts.is_empty() {
        out.push(format!("reaching {}", more_hosts.iter().map(|h| h.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if o.files == "session-ro" && n.files == "session" {
        out.push("writing files".to_owned());
    }
    if n.cpus > o.cpus || n.memory_mb > o.memory_mb || n.timeout_s > o.timeout_s {
        out.push("more CPU, memory or time".to_owned());
    }
    if n.shell && !o.shell {
        out.push("a shell".to_owned());
    }
    let more: Vec<&String> = n.secrets.iter().filter(|s| !o.secrets.contains(s)).collect();
    if !more.is_empty() {
        out.push(format!("secrets {}", more.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if n.grants.len() > o.grants.len() {
        out.push("more grants".to_owned());
    }
    if n.workers == "dedicated" && o.workers != "dedicated" {
        out.push("a worker of its own".to_owned());
    }
    let kinds: Vec<&String> = new.reads.items.kinds.iter().filter(|k| !old.reads.items.kinds.contains(k)).collect();
    let cols: Vec<&String> = new.reads.collections.iter().filter(|c| !old.reads.collections.contains(c)).collect();
    if !kinds.is_empty() || !cols.is_empty() {
        out.push(format!("reading more ({})", kinds.iter().chain(cols.iter()).map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
    }
    let triggers: Vec<String> = new
        .contents
        .triggers
        .iter()
        .filter_map(|t| t.name.clone())
        .filter(|n| !old.contents.triggers.iter().any(|t| t.name.as_deref() == Some(n.as_str())))
        .collect();
    if !triggers.is_empty() {
        out.push(format!("new triggers ({})", triggers.join(", ")));
    }
    out
}

/// The plan of installing a package version: Markdown, and what stands in
/// the way.
async fn plan_of(st: &HeadState, nodes: &[Node], mut problems: Vec<String>) -> anyhow::Result<api::AppPlan> {
    let root = nodes.last().ok_or_else(|| anyhow::anyhow!("nothing to install"))?;
    let new: Vec<&Node> = nodes.iter().filter(|n| n.existing.is_none()).collect();
    if root.existing.is_some() {
        problems.push(format!(
            "{} is already installed{} as {}: upgrade it, or give this one another name",
            root.pkg.app,
            where_(&root.target),
            root.name
        ));
    }
    let mut lines = Vec::new();
    for n in &new {
        if in_realm(&n.roles) {
            problems.extend(crate::apps::clashes(st, &n.target, &n.name, &n.pkg).await?);
        }
        let role = n.roles.join(" and ");
        let place = match n.target.as_str() {
            "" => "for workers to build".to_owned(),
            t if n.name == n.pkg.app => format!("in realm **{t}**"),
            t => format!("in realm **{t}** as **{}**", n.name),
        };
        let first = if std::ptr::eq(*n, root) { "Install" } else { "With it, install" };
        lines.push(format!("{first} **{}** {} ({role}) {place}.", n.pkg.app, n.pkg.version));
        if !n.pkg.description.is_empty() {
            lines.push(String::new());
            lines.push(n.pkg.description.clone());
        }
        lines.push(String::new());
        lines.extend(crate::apps::contents_lines(&n.pkg));
        let perms = permission_lines(&n.pkg);
        if !perms.is_empty() {
            lines.push(String::new());
            lines.extend(perms.into_iter().map(|p| format!("- {p}")));
        }
        let settings: Vec<&String> = n.pkg.settings["properties"].as_object().map(|o| o.keys().collect()).unwrap_or_default();
        if !settings.is_empty() {
            lines.push(format!("- Settings you can change later: {}.", settings.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
        }
        lines.push(String::new());
    }
    for n in nodes.iter().filter(|n| n.existing.is_some() && !std::ptr::eq(*n, root)) {
        lines.push(format!("Uses **{}** {}, already installed{}.", n.pkg.app, n.pkg.version, where_(&n.target)));
    }
    Ok(api::AppPlan { realm: root.target.clone(), plan: lines.join("\n").trim().to_owned(), problems, conflicts: Vec::new() })
}

/// What installing a package version (from the registry) would do.
pub async fn plan(st: &HeadState, req: &api::InstallIn) -> anyhow::Result<api::AppPlan> {
    let (pkg, digest) = newest(st, &req.package, &req.version)
        .await?
        .ok_or_else(|| not_found(format!("no package {} {} in the registry", req.package, req.version)))?;
    let realm = match target_realm(st, &pkg, &req.realm).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(api::AppPlan {
                realm: req.realm.clone(),
                plan: String::new(),
                problems: vec![format!("{e:#}")],
                conflicts: Vec::new(),
            });
        }
    };
    let (nodes, problems) = resolve(st, pkg, digest, &realm, &req.name).await?;
    plan_of(st, &nodes, problems).await
}

/// What installing a package not yet in the registry would do.
pub async fn plan_manifest(st: &HeadState, manifest: Value, realm: &str) -> anyhow::Result<api::AppPlan> {
    let p = normalize_manifest(manifest)?;
    let mut problems = check_manifest(&p, &BTreeMap::new());
    problems.retain(|x| !x.contains("among the package's files"));
    let digest = digest_of(&p, &BTreeMap::new());
    let realm = match target_realm(st, &p, realm).await {
        Ok(r) => r,
        Err(e) => {
            problems.push(format!("{e:#}"));
            return Ok(api::AppPlan { realm: realm.to_owned(), plan: String::new(), problems, conflicts: Vec::new() });
        }
    };
    let (nodes, more) = resolve(st, p, digest, &realm, "").await?;
    problems.extend(more);
    plan_of(st, &nodes, problems).await
}

/// Asks to install a package version and what it requires: one decision.
pub async fn install(st: &HeadState, req: &api::InstallIn) -> anyhow::Result<api::Install> {
    let (pkg, digest) = newest(st, &req.package, &req.version)
        .await?
        .ok_or_else(|| not_found(format!("no package {} {} in the registry", req.package, req.version)))?;
    let realm = target_realm(st, &pkg, &req.realm).await?;
    let (nodes, problems) = resolve(st, pkg, digest, &realm, &req.name).await?;
    let plan = plan_of(st, &nodes, problems).await?;
    if !plan.problems.is_empty() {
        return Err(bad_request(plan.problems.join("; ")));
    }
    // A row per new package, waiting; the root's says what to do.
    let now = now_ms();
    let mut ids: Vec<Option<i64>> = Vec::new();
    for n in &nodes {
        if let Some(e) = n.existing {
            ids.push(Some(e));
            continue;
        }
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO installs (package, version, digest, realm, name, state, locks, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, 'installing', ?, ?, ?) RETURNING id",
        )
        .bind(&n.pkg.app)
        .bind(&n.pkg.version)
        .bind(&n.digest)
        .bind(&n.target)
        .bind(if n.target.is_empty() { &n.pkg.app } else { &n.name })
        .bind(serde_json::to_string(&n.locks)?)
        .bind(now)
        .bind(now)
        .fetch_one(&st.store.pool)
        .await
        .map_err(|e| bad_request(format!("{} is already being installed{} ({e})", n.pkg.app, where_(&n.target))))?;
        ids.push(Some(id));
    }
    let order: Vec<Value> = nodes
        .iter()
        .zip(&ids)
        .map(|(n, id)| json!({"id": id, "new": n.existing.is_none(), "parents": n.parents.iter().map(|p| ids[*p]).collect::<Vec<_>>()}))
        .collect();
    let root = nodes.last().expect("a root");
    let root_id = ids.last().copied().flatten().expect("a new root");
    sqlx::query("UPDATE installs SET pending = ? WHERE id = ?")
        .bind(json!({"install": order}).to_string())
        .bind(root_id)
        .execute(&st.store.pool)
        .await?;
    let title = if root.roles.iter().any(|r| r == "app") {
        format!("Install the app {} in {}?", root.pkg.app, root.target)
    } else if root.target.is_empty() {
        format!("Install the {} {}?", root.roles.join(" and "), root.pkg.app)
    } else {
        format!("Install the {} {} in {}?", root.roles.join(" and "), root.pkg.app, root.target)
    };
    ask(st, root_id, "install", &title, &plan.plan, &["Install", "Cancel"]).await
}

async fn ask(st: &HeadState, id: i64, what: &str, title: &str, document: &str, options: &[&str]) -> anyhow::Result<api::Install> {
    let d = api::DecisionIn {
        kind: "approve".into(),
        title: title.to_owned(),
        document: document.to_owned(),
        options: options.iter().map(|o| api::DecisionOption { label: (*o).to_owned(), does: String::new() }).collect(),
        priority: "normal".into(),
        source: "packages".into(),
        context: json!({"install": id, "what": what}),
        ..Default::default()
    };
    let d = crate::decisions::open(st, &d, ROOT, &format!("package:{what}:{id}")).await?;
    sqlx::query("UPDATE installs SET decision = ?, updated_at = ? WHERE id = ?")
        .bind(d.id)
        .bind(now_ms())
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    install_by_id(st, id).await
}

/// Your answer to an install, upgrade or removal (decision `package:<what>:<id>`,
/// or `app:<what>:<realm>` from before packages).
pub async fn answer(st: &HeadState, reference: &str, option: Option<&str>, values: &serde_json::Map<String, Value>) -> anyhow::Result<()> {
    let mut parts = reference.splitn(3, ':');
    let (kind, what, key) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let id = match kind {
        "package" => key.parse::<i64>().map_err(|_| bad_request("no such install"))?,
        // An app's decision from before packages: by its realm.
        _ => app_in(st, key).await?.map(|(i, _)| i.id).ok_or_else(|| not_found(format!("no app in {key}")))?,
    };
    if what == "adopt" {
        return crate::apps::answer_adopt(st, id, option).await;
    }
    if what == "move" {
        return crate::apps::answer_move(st, id, option, values).await;
    }
    let r = row(st, id).await?;
    let pending: Value = r.get::<Option<String>, _>("pending").and_then(|p| serde_json::from_str(&p).ok()).unwrap_or(Value::Null);
    let go = matches!(
        option,
        Some(
            "Install"
                | "Upgrade"
                | "Downgrade"
                | "Switch"
                | "Remove"
                // Decisions asked before upgrades stopped merging.
                | "Upgrade, keeping my edits"
                | "Upgrade, taking the new version"
        )
    );
    if !go {
        if what == "install" {
            // Cancelled: nothing was installed.
            for step in pending["install"].as_array().into_iter().flatten().filter(|s| s["new"] == true) {
                sqlx::query("DELETE FROM installs WHERE id = ? AND state = 'installing'")
                    .bind(step["id"].as_i64())
                    .execute(&st.store.pool)
                    .await?;
            }
            sqlx::query("DELETE FROM installs WHERE id = ? AND state = 'installing'").bind(id).execute(&st.store.pool).await?;
        } else {
            // New requirements an upgrade brought are not installed.
            for step in pending["install"].as_array().into_iter().flatten().filter(|s| s["new"] == true) {
                sqlx::query("DELETE FROM installs WHERE id = ? AND state = 'installing'")
                    .bind(step["id"].as_i64())
                    .execute(&st.store.pool)
                    .await?;
            }
            sqlx::query("UPDATE installs SET state = 'installed', pending = NULL, decision = NULL, updated_at = ? WHERE id = ?")
                .bind(now_ms())
                .bind(id)
                .execute(&st.store.pool)
                .await?;
        }
        st.registry.bump();
        return Ok(());
    }
    match what {
        "install" => apply_install(st, &pending).await?,
        "upgrade" => apply_upgrade(st, id, &pending).await?,
        "remove" => remove_now(st, id).await?,
        _ => {}
    }
    st.registry.bump();
    Ok(())
}

/// What an install approves: its permissions, and what it reads.
fn approved_of(p: &api::AppPackage) -> anyhow::Result<String> {
    let mut v = serde_json::to_value(&p.permissions)?;
    if let Some(o) = v.as_object_mut() {
        o.insert("reads".into(), serde_json::to_value(&p.reads)?);
    }
    Ok(v.to_string())
}

async fn apply_install(st: &HeadState, pending: &Value) -> anyhow::Result<()> {
    let steps = pending["install"].as_array().cloned().unwrap_or_default();
    for step in &steps {
        let Some(id) = step["id"].as_i64() else { continue };
        if step["new"] != true {
            continue;
        }
        let r = row(st, id).await?;
        let digest: String = r.get("digest");
        let (realm, name): (String, String) = (r.get("realm"), r.get("name"));
        let (p, _) = by_digest(st, &digest).await?.ok_or_else(|| anyhow::anyhow!("package {digest} is gone"))?;
        let roles = roles(&p);
        // Installed first, so what it brings sees its own code (its triggers
        // start its templates).
        sqlx::query(
            "UPDATE installs SET state = 'installed', approved = ?, installed = ?, pending = NULL, decision = NULL, adopted = 1, updated_at = ? \
             WHERE id = ?",
        )
        .bind(approved_of(&p)?)
        .bind(serde_json::to_string(&p)?)
        .bind(now_ms())
        .bind(id)
        .execute(&st.store.pool)
        .await?;
        if in_realm(&roles)
            && let Err(e) = crate::apps::apply_instance(st, &realm, &name, &p).await
        {
            // Nothing half-done: what it made goes, and so does the install.
            let _ = crate::apps::remove_instance(st, &realm, &name).await;
            sqlx::query("DELETE FROM installs WHERE id = ?").bind(id).execute(&st.store.pool).await?;
            return Err(e);
        }
        if in_realm(&roles) {
            crate::apps::next_steps(st, &realm, &name, &p).await;
        }
        if p.app == crate::extract::KIT {
            crate::extract::approved(st).await;
        }
    }
    // Then who requires what, and what an app requiring an app in another
    // realm may start.
    for step in &steps {
        let Some(id) = step["id"].as_i64() else { continue };
        for parent in step["parents"].as_array().into_iter().flatten().filter_map(Value::as_i64) {
            add_required_by(st, id, parent).await?;
            export_templates(st, id, parent).await?;
        }
    }
    Ok(())
}

async fn add_required_by(st: &HeadState, id: i64, by: i64) -> anyhow::Result<()> {
    let r = row(st, id).await?;
    let mut list: Vec<i64> = serde_json::from_str(&r.get::<String, _>("required_by")).unwrap_or_default();
    if !list.contains(&by) {
        list.push(by);
    }
    sqlx::query("UPDATE installs SET required_by = ? WHERE id = ?")
        .bind(serde_json::to_string(&list)?)
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    Ok(())
}

/// An app requiring an app in another realm may start the templates it
/// exports: a grant from the required app's realm. (In the same realm, the
/// realm's sessions already have them.)
async fn export_templates(st: &HeadState, child: i64, parent: i64) -> anyhow::Result<()> {
    let (c, p) = (install_by_id(st, child).await?, install_by_id(st, parent).await?);
    if !c.roles.iter().any(|r| r == "app") || p.realm.is_empty() || p.realm == c.realm {
        return Ok(());
    }
    let (m, _) = by_digest(st, &c.digest).await?.unwrap_or_default();
    if m.exports.templates.is_empty() {
        return Ok(());
    }
    let methods: Vec<String> = m.exports.templates.iter().map(|t| format!("template:{t}")).collect();
    let g = api::GrantIn {
        from_realm: c.realm.clone(),
        to_realm: p.realm.clone(),
        kind: "operations".into(),
        scope: json!({"methods": methods}),
        mode: "call".into(),
        expires_at: None,
        note: format!("{} requires {}", p.package, c.package),
    };
    crate::grants::create(st, &g, &format!("package:{}", p.package), "active").await?;
    Ok(())
}

/// Asks to upgrade (or downgrade) an install to the newest version a range
/// allows.
pub async fn upgrade(st: &HeadState, id: i64, version: &str) -> anyhow::Result<api::Install> {
    upgrade_to(st, id, "", version).await
}

/// Asks to replace an instance's code with another version of its package,
/// or of a fork of it (`package`; docs/apps.md §5.6, §12). Nothing merges:
/// its code becomes the new version's, its settings and data stay.
pub async fn upgrade_to(st: &HeadState, id: i64, package: &str, version: &str) -> anyhow::Result<api::Install> {
    let cur = install_by_id(st, id).await?;
    if cur.state != "installed" {
        return Err(bad_request(format!("{} is {}; wait for that first", cur.package, cur.state)));
    }
    let package = if package.trim().is_empty() { cur.package.clone() } else { package.trim().to_owned() };
    let (new, digest) = newest(st, &package, version).await?.ok_or_else(|| not_found(format!("no {package} {version} in the registry")))?;
    if digest == cur.digest {
        return Err(bad_request(format!("{} {} is what is installed", cur.package, new.version)));
    }
    let (old, _) = by_digest(st, &cur.digest).await?.unwrap_or_default();
    if package != cur.package && new.forked_from != cur.package && old.forked_from != package {
        return Err(bad_request(format!(
            "{package} is not a fork of {} (its forked_from says {:?}): install it as another instance instead",
            cur.package, new.forked_from
        )));
    }
    // New requirements come with it.
    let (nodes, mut problems) = resolve(st, new.clone(), digest.clone(), &cur.realm, &cur.name).await?;
    problems.retain(|p| !p.contains("is already installed"));
    let verb = if package != cur.package {
        "Switch"
    } else {
        match (semver::Version::parse(&cur.version), semver::Version::parse(&new.version)) {
            (Ok(a), Ok(b)) if b < a => "Downgrade",
            _ => "Upgrade",
        }
    };
    let from = if package != cur.package { format!("{} {}", cur.package, cur.version) } else { cur.version.clone() };
    let to = if package != cur.package { format!("{package} {}", new.version) } else { new.version.clone() };
    let mut lines = vec![format!("{verb} **{}**{} from {from} to {to}.", cur.name, where_(&cur.realm)), String::new()];
    if !cur.realm.is_empty() {
        lines.push("Its code becomes the new version's; its settings and data stay.".into());
        let changes = crate::apps::code_changes(&old, &new);
        if changes.is_empty() {
            lines.push("No collections, triggers or settings lists change.".into());
        } else {
            lines.push(String::new());
            lines.extend(changes);
        }
        problems.extend(crate::apps::migration_problems(st, &cur.realm, &cur.name, &new).await?);
        problems.extend(crate::apps::contract_problems(&old, &new));
        let deps = crate::apps::dependents(st, &cur.realm, &cur.name, &old).await?;
        if !deps.is_empty() {
            lines.push(format!("\nWhat in {} uses what it publishes: {}.", cur.realm, deps.join(", ")));
        }
    }
    let grew = grown(&old, &new);
    if !grew.is_empty() {
        lines.push(format!("\nIt asks for more than before: {}.", grew.join(", ")));
        lines.extend(permission_lines(&new).into_iter().map(|p| format!("- {p}")));
    }
    let extra: Vec<&Node> = nodes.iter().filter(|n| n.existing.is_none() && n.pkg.app != new.app).collect();
    for n in &extra {
        lines.push(format!("\nWith it, install **{}** {} ({}).", n.pkg.app, n.pkg.version, n.roles.join(" and ")));
        lines.extend(permission_lines(&n.pkg).into_iter().map(|p| format!("- {p}")));
    }
    if !problems.is_empty() {
        return Err(bad_request(problems.join("; ")));
    }
    // Rows for new requirements, waiting with it.
    let now = now_ms();
    let mut steps = Vec::new();
    let mut ids: Vec<Option<i64>> = Vec::new();
    for n in &nodes {
        let id_here = if n.target == cur.realm && n.name == cur.name {
            Some(id)
        } else if let Some(e) = n.existing {
            Some(e)
        } else {
            Some(
                sqlx::query_scalar(
                    "INSERT INTO installs (package, version, digest, realm, name, state, locks, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, 'installing', ?, ?, ?) RETURNING id",
                )
                .bind(&n.pkg.app)
                .bind(&n.pkg.version)
                .bind(&n.digest)
                .bind(&n.target)
                .bind(if n.target.is_empty() { &n.pkg.app } else { &n.name })
                .bind(serde_json::to_string(&n.locks)?)
                .bind(now)
                .bind(now)
                .fetch_one(&st.store.pool)
                .await?,
            )
        };
        ids.push(id_here);
    }
    for (n, i) in nodes.iter().zip(&ids) {
        let is_root = *i == Some(id);
        steps.push(
            json!({"id": i, "new": n.existing.is_none() && !is_root, "parents": n.parents.iter().map(|p| ids[*p]).collect::<Vec<_>>()}),
        );
    }
    let locks = nodes.last().map(|n| n.locks.clone()).unwrap_or_default();
    sqlx::query("UPDATE installs SET state = 'upgrading', pending = ?, updated_at = ? WHERE id = ?")
        .bind(json!({"install": steps, "digest": digest, "version": new.version, "package": package, "locks": locks}).to_string())
        .bind(now)
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    ask(st, id, "upgrade", &format!("{verb} {} to {to}?", cur.name), &lines.join("\n"), &[verb, "Cancel"]).await
}

async fn apply_upgrade(st: &HeadState, id: i64, pending: &Value) -> anyhow::Result<()> {
    let cur = install_by_id(st, id).await?;
    let digest = pending["digest"].as_str().unwrap_or_default();
    let (new, _) = by_digest(st, digest).await?.ok_or_else(|| anyhow::anyhow!("the new version is gone"))?;
    let (old, _) = by_digest(st, &cur.digest).await?.unwrap_or_default();
    // Its new requirements first.
    let steps: Vec<Value> = pending["install"].as_array().cloned().unwrap_or_default();
    let others: Vec<Value> = steps.iter().filter(|s| s["id"].as_i64() != Some(id)).cloned().collect();
    apply_install(st, &json!({"install": others})).await?;
    if !cur.realm.is_empty() {
        crate::apps::apply_instance_upgrade(st, &cur.realm, &cur.name, &old, &new).await?;
    }
    // Settings the new version no longer has lose their values.
    let values: Value = serde_json::from_str(&row(st, id).await?.get::<String, _>("settings")).unwrap_or(json!({}));
    let props = new.settings["properties"].as_object();
    let kept: serde_json::Map<String, Value> = values
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| props.is_some_and(|p| p.contains_key(*k)))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    sqlx::query(
        "UPDATE installs SET state = 'installed', package = ?, digest = ?, version = ?, locks = ?, approved = ?, installed = ?, settings = ?, \
         pending = NULL, decision = NULL, updated_at = ? WHERE id = ?",
    )
    .bind(pending["package"].as_str().unwrap_or(&cur.package))
    .bind(digest)
    .bind(&new.version)
    .bind(pending["locks"].to_string())
    .bind(approved_of(&new)?)
    .bind(serde_json::to_string(&new)?)
    .bind(Value::Object(kept).to_string())
    .bind(now_ms())
    .bind(id)
    .execute(&st.store.pool)
    .await?;
    Ok(())
}

/// Sets an instance's settings (docs/apps.md §5.4): checked against the
/// schema its package declares.
pub async fn set_settings(st: &HeadState, id: i64, values: &Value) -> anyhow::Result<api::Install> {
    let cur = install_by_id(st, id).await?;
    let (p, _) = by_digest(st, &cur.digest).await?.unwrap_or_default();
    if p.settings.is_null() {
        return Err(bad_request(format!("{} has no settings", cur.package)));
    }
    if !values.is_object() {
        return Err(bad_request("settings are an object"));
    }
    let all = crate::instances::with_defaults(&p.settings, values);
    if let Err(problems) = crate::schema::validate(&p.settings, &all) {
        return Err(bad_request(format!("the settings do not match {}'s: {}", cur.package, problems.join("; "))));
    }
    sqlx::query("UPDATE installs SET settings = ?, updated_at = ? WHERE id = ?")
        .bind(values.to_string())
        .bind(now_ms())
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    st.registry.bump();
    install_by_id(st, id).await
}

/// Asks to remove an install: an app or library takes what it owns with
/// it; a kit stops its tools.
pub async fn remove(st: &HeadState, id: i64) -> anyhow::Result<api::Install> {
    let cur = install_by_id(st, id).await?;
    let users: Vec<String> = {
        let all = installs(st).await?;
        cur.required_by
            .iter()
            .filter_map(|b| all.iter().find(|i| i.id == *b))
            .map(|i| format!("{}{}", i.package, where_(&i.realm)))
            .collect()
    };
    if !users.is_empty() {
        return Err(bad_request(format!("{} is required by {}; remove those first", cur.package, users.join(", "))));
    }
    let (doc, title) = if in_realm(&cur.roles) {
        let owned = crate::apps::owned_lines(st, &cur.realm, &cur.name).await?;
        (
            format!(
                "Removing **{}** from realm **{}** deletes what it owns: {}, and retracts its claims in the graph. Its sessions stay in the \
                 realm's history, and the realm stays.\
                 \n\nExport its data first (Settings → System → Packages → Export) to keep a copy.",
                cur.name,
                cur.realm,
                if owned.is_empty() { "nothing but its code".to_owned() } else { owned.join(", ") }
            ),
            format!("Remove {} from {}?", cur.name, cur.realm),
        )
    } else {
        (
            format!("Removing **{}** stops its tools. Images built on workers stay until their runtime cleans them up.", cur.package),
            format!("Remove the kit {}?", cur.package),
        )
    };
    sqlx::query("UPDATE installs SET state = 'removing', updated_at = ? WHERE id = ?")
        .bind(now_ms())
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    ask(st, id, "remove", &title, &doc, &["Remove", "Cancel"]).await
}

async fn remove_now(st: &HeadState, id: i64) -> anyhow::Result<()> {
    let cur = install_by_id(st, id).await?;
    if in_realm(&cur.roles) {
        crate::apps::remove_instance(st, &cur.realm, &cur.name).await?;
    }
    sqlx::query("DELETE FROM installs WHERE id = ?").bind(id).execute(&st.store.pool).await?;
    // It no longer requires what it required.
    for other in installs(st).await? {
        if other.required_by.contains(&id) {
            let left: Vec<i64> = other.required_by.into_iter().filter(|x| *x != id).collect();
            sqlx::query("UPDATE installs SET required_by = ? WHERE id = ?")
                .bind(serde_json::to_string(&left)?)
                .bind(other.id)
                .execute(&st.store.pool)
                .await?;
        }
    }
    Ok(())
}

// --- Apps, as the API had them before packages -----------------------------------

pub fn app_install(i: &api::Install, description: &str) -> api::AppInstall {
    api::AppInstall {
        realm: i.realm.clone(),
        app: i.package.clone(),
        version: i.version.clone(),
        description: description.to_owned(),
        state: i.state.clone(),
        created_at: i.created_at,
        updated_at: i.updated_at,
        decision: i.decision,
    }
}

/// Installed apps (and apps being installed).
pub async fn apps(st: &HeadState) -> anyhow::Result<Vec<api::AppInstall>> {
    let mut out = Vec::new();
    for i in installs(st).await?.into_iter().filter(|i| i.roles.iter().any(|r| r == "app")) {
        let d = by_digest(st, &i.digest).await?.map(|x| x.0.description).unwrap_or_default();
        out.push(app_install(&i, &d));
    }
    Ok(out)
}

/// `POST /api/v1/apps`: the package into the registry, then installed in
/// `realm` (default: the one it suggests if it exists, else home).
pub async fn install_manifest(st: &HeadState, manifest: Value, realm: &str) -> anyhow::Result<api::AppInstall> {
    let info = add(st, manifest, BTreeMap::new(), "local").await?;
    let i = install(
        st,
        &api::InstallIn { package: info.name.clone(), version: info.version.clone(), realm: realm.to_owned(), name: String::new() },
    )
    .await?;
    Ok(app_install(&i, &info.description))
}

/// `POST /api/v1/apps/{realm}/upgrade`: the new version into the registry,
/// then that app in that realm upgraded to it.
pub async fn upgrade_manifest(st: &HeadState, realm: &str, manifest: Value) -> anyhow::Result<api::AppInstall> {
    let p = normalize_manifest(manifest.clone())?;
    let cur = installs(st)
        .await?
        .into_iter()
        .find(|i| i.realm == realm && i.package == p.app)
        .ok_or_else(|| not_found(format!("no app {} installed in {realm}", p.app)))?;
    let info = add(st, manifest, BTreeMap::new(), "local").await?;
    let i = upgrade(st, cur.id, &info.version).await?;
    Ok(app_install(&i, &info.description))
}

pub async fn remove_app(st: &HeadState, realm: &str) -> anyhow::Result<api::AppInstall> {
    let (cur, m) = app_in(st, realm).await?.ok_or_else(|| not_found(format!("no app installed in {realm}")))?;
    let i = remove(st, cur.id).await?;
    Ok(app_install(&i, &m.description))
}

// --- Kits, for workers ---------------------------------------------------------------

/// The local image tag of a kit version, given its dependencies' tags: it
/// changes when the kit or anything it is built from changes.
fn image_tag(name: &str, digest: &str, dep_tags: &[String]) -> String {
    let mut text = digest.to_owned();
    for t in dep_tags {
        text.push('\n');
        text.push_str(t);
    }
    format!("c6kit/{name}:{}", &c6_core::sha256_hex(text.as_bytes())[..16])
}

/// The kit installs a realm may use: what its apps and libraries require,
/// directly or through what they require (kits they are built from too).
pub async fn kits_for_realm(st: &HeadState, realm: &str) -> anyhow::Result<Vec<api::Install>> {
    let all = installs(st).await?;
    let mut want: Vec<String> = Vec::new();
    let mut queue: Vec<&api::Install> = all.iter().filter(|i| i.realm == realm && i.state == "installed").collect();
    while let Some(i) = queue.pop() {
        for d in i.locks.values() {
            if let Some(dep) = all.iter().find(|x| &x.digest == d && x.state == "installed")
                && !want.contains(&dep.digest)
            {
                if dep.roles.iter().any(|r| r == "kit") {
                    want.push(dep.digest.clone());
                }
                queue.push(dep);
            }
        }
    }
    Ok(all.into_iter().filter(|i| i.roles.iter().any(|r| r == "kit") && want.contains(&i.digest)).collect())
}

/// What a worker needs to build and run a kit a realm may use: the builds
/// in order (each with its Dockerfile, `kit:` resolved, and files), the
/// image to run, its tools, and the permissions approved.
pub async fn kit_spec(st: &HeadState, realm: &str, name: &str) -> anyhow::Result<Value> {
    let kits = kits_for_realm(st, realm).await?;
    let Some(kit) = kits.iter().find(|k| k.package == name) else {
        let names: Vec<&str> = kits.iter().map(|k| k.package.as_str()).collect();
        return Err(bad_request(if names.is_empty() {
            format!("no kits are installed for realm {realm}")
        } else {
            format!("no kit {name} here (this realm has: {})", names.join(", "))
        }));
    };
    kit_spec_of(st, kit).await
}

/// What a worker needs to build and run an installed kit.
pub async fn kit_spec_of(st: &HeadState, kit: &api::Install) -> anyhow::Result<Value> {
    let all = installs(st).await?;
    let mut builds: Vec<Value> = Vec::new();
    let mut tags: BTreeMap<String, String> = BTreeMap::new();
    // Depth first: what a kit is built from, then the kit.
    async fn build_of(
        st: &HeadState,
        all: &[api::Install],
        digest: &str,
        builds: &mut Vec<Value>,
        tags: &mut BTreeMap<String, String>,
        depth: usize,
    ) -> anyhow::Result<String> {
        if let Some(t) = tags.get(digest) {
            return Ok(t.clone());
        }
        anyhow::ensure!(depth < 32, "kits built from kits too deep");
        let (p, _) = by_digest(st, digest).await?.ok_or_else(|| anyhow::anyhow!("package {digest} is gone"))?;
        let k = p.kit.clone().ok_or_else(|| bad_request(format!("{} is not a kit", p.app)))?;
        let inst = all.iter().find(|i| i.digest == digest);
        let mut dockerfile = String::from_utf8_lossy(&file(st, digest, &k.build.dockerfile).await?).into_owned();
        let mut dep_tags = Vec::new();
        for r in kit_refs(&dockerfile) {
            let dep = inst
                .and_then(|i| i.locks.get(&r))
                .cloned()
                .ok_or_else(|| bad_request(format!("{} builds from kit:{r}, which is not installed with it", p.app)))?;
            let t = Box::pin(build_of(st, all, &dep, builds, tags, depth + 1)).await?;
            dockerfile = dockerfile.replace(&format!("kit:{r}"), &t);
            dep_tags.push(t);
        }
        let tag = image_tag(&p.app, digest, &dep_tags);
        let mut files = serde_json::Map::new();
        for f in &k.build.files {
            files.insert(f.clone(), json!(base64::engine::general_purpose::STANDARD.encode(file(st, digest, f).await?)));
        }
        builds.push(json!({"name": p.app, "version": p.version, "tag": tag, "dockerfile": dockerfile, "files": files}));
        tags.insert(digest.to_owned(), tag.clone());
        Ok(tag)
    }
    let tag = build_of(st, &all, &kit.digest, &mut builds, &mut tags, 0).await?;
    let (p, _) = by_digest(st, &kit.digest).await?.unwrap_or_default();
    let approved: api::Permissions = serde_json::from_str(&row(st, kit.id).await?.get::<String, _>("approved")).unwrap_or_default();
    Ok(
        json!({"name": p.app, "version": p.version, "image": tag, "builds": builds, "tools": p.kit.map(|k| k.tools).unwrap_or_default(), "permissions": approved}),
    )
}

/// The kits a realm may use, and their tools, in words for an agent.
pub async fn kits_text(st: &HeadState, realm: &str) -> anyhow::Result<Value> {
    let mut out = Vec::new();
    for k in kits_for_realm(st, realm).await? {
        let (p, _) = by_digest(st, &k.digest).await?.unwrap_or_default();
        let approved: api::Permissions = serde_json::from_str(&row(st, k.id).await?.get::<String, _>("approved")).unwrap_or_default();
        // Kits only built from offer nothing to run.
        if p.kit.as_ref().is_none_or(|k| k.tools.is_empty()) && !approved.shell {
            continue;
        }
        out.push(json!({"kit": p.app, "version": p.version, "description": p.description, "shell": approved.shell,
                        "tools": p.kit.map(|k| k.tools).unwrap_or_default()}));
    }
    Ok(json!(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_normalize_and_ranges_match() {
        let p = normalize_manifest(json!({"app": "crm", "version": "1.0.0", "needs": {"secrets": ["K"], "workers": "dedicated"}})).unwrap();
        assert_eq!((p.app.as_str(), p.realm.as_str()), ("crm", "crm"));
        assert_eq!((p.permissions.secrets.clone(), p.permissions.workers.as_str()), (vec!["K".to_owned()], "dedicated"));
        assert_eq!(roles(&p), vec!["app"]);
        let lib = normalize_manifest(json!({"package": "style", "version": "2.0.0", "contents": {"skills": [{"name": "s", "body": "b"}]}}))
            .unwrap();
        assert_eq!(roles(&lib), vec!["library"]);
        let v = |s: &str| semver::Version::parse(s).unwrap();
        assert!(range("^1.2").unwrap().matches(&v("1.9.0")) && !range("^1.2").unwrap().matches(&v("2.0.0")));
        assert!(range("1.2.3").unwrap().matches(&v("1.2.3")) && !range("1.2.3").unwrap().matches(&v("1.2.4")), "a bare version is exact");
        assert!(range("*").unwrap().matches(&v("0.1.0")));
        assert_eq!(kit_refs("FROM kit:python-base AS b\nFROM kit:rust@x\nRUN echo kit:"), vec!["python-base", "rust"]);
        // The digest covers files.
        let mut f = BTreeMap::new();
        let a = digest_of(&p, &f);
        f.insert("Dockerfile".into(), b"FROM scratch".to_vec());
        assert_ne!(a, digest_of(&p, &f));
    }
}

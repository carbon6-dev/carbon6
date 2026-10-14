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

use crate::realms::ROOT;
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

/// What a package is: app (a realm), kit (an environment), library
/// (configuration without a realm).
pub fn roles(p: &api::AppPackage) -> Vec<String> {
    let mut r = Vec::new();
    if !p.realm.is_empty() {
        r.push("app".to_owned());
    }
    if p.kit.is_some() {
        r.push("kit".to_owned());
    }
    if p.realm.is_empty() && !p.contents.is_empty() {
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
    if !matches!(perm.network.as_str(), "none" | "any") {
        problems.push("network is none or any (allow-lists by host come later)".into());
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
        if k.tools.is_empty() && !p.permissions.shell {
            problems.push("a kit offers tools (or a shell)".into());
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
    if p.realm.is_empty() && p.kit.is_none() && p.contents.is_empty() {
        problems.push("a package has a realm (an app), a kit, or contents (a library)".into());
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

fn install_of(r: &sqlx::sqlite::SqliteRow, roles_by_digest: &BTreeMap<String, Vec<String>>) -> api::Install {
    let digest: String = r.get("digest");
    api::Install {
        id: r.get("id"),
        package: r.get("package"),
        version: r.get("version"),
        roles: roles_by_digest.get(&digest).cloned().unwrap_or_default(),
        digest,
        realm: r.get("realm"),
        state: r.get("state"),
        locks: serde_json::from_str(&r.get::<String, _>("locks")).unwrap_or_default(),
        required_by: serde_json::from_str(&r.get::<String, _>("required_by")).unwrap_or_default(),
        decision: r.get("decision"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

async fn roles_by_digest(st: &HeadState) -> anyhow::Result<BTreeMap<String, Vec<String>>> {
    Ok(sqlx::query("SELECT digest, roles FROM packages")
        .fetch_all(&st.store.pool)
        .await?
        .iter()
        .map(|r| (r.get::<String, _>("digest"), serde_json::from_str(&r.get::<String, _>("roles")).unwrap_or_default()))
        .collect())
}

pub async fn installs(st: &HeadState) -> anyhow::Result<Vec<api::Install>> {
    let roles = roles_by_digest(st).await?;
    let rows = sqlx::query("SELECT * FROM installs ORDER BY package, realm").fetch_all(&st.store.pool).await?;
    Ok(rows.iter().map(|r| install_of(r, &roles)).collect())
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

/// The app installed in a realm, with its manifest as installed.
pub async fn app_in(st: &HeadState, realm: &str) -> anyhow::Result<Option<(api::Install, api::AppPackage)>> {
    let Some(i) = installs(st).await?.into_iter().find(|i| i.realm == realm && i.roles.iter().any(|r| r == "app")) else {
        return Ok(None);
    };
    let m: api::AppPackage = serde_json::from_str(&row(st, i.id).await?.get::<String, _>("installed")).unwrap_or_default();
    let m = if m.app.is_empty() { by_digest(st, &i.digest).await?.map(|x| x.0).unwrap_or_default() } else { m };
    Ok(Some((i, m)))
}

/// One package to install (or already installed), found while resolving.
#[derive(Clone, Debug)]
struct Node {
    pkg: api::AppPackage,
    digest: String,
    roles: Vec<String>,
    /// Its realm (app), the realm it joins (library), "" (kit).
    target: String,
    /// Already installed: that install.
    existing: Option<i64>,
    /// What it requires: name → digest.
    locks: BTreeMap<String, String>,
    /// The nodes that require it.
    parents: Vec<usize>,
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

/// Resolves a package and what it requires: nodes in install order
/// (requirements first), and what stands in the way.
async fn resolve(
    st: &HeadState,
    root: api::AppPackage,
    root_digest: String,
    library_realm: &str,
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
        for (name, req) in pkg.requires.clone() {
            // An installed version that meets the range is used as it is.
            let child_target_of = |child: &api::AppPackage| -> Option<String> {
                let r = roles(child);
                if r.iter().any(|x| x == "app") {
                    Some(child.realm.clone())
                } else if r.iter().any(|x| x == "library") {
                    (!target.is_empty()).then(|| target.clone())
                } else {
                    Some(String::new())
                }
            };
            let Some((child, cdigest)) = newest(st, &name, &req).await? else {
                problems.push(format!("{} requires {name} {req}, which is not in the registry", pkg.app));
                continue;
            };
            let Some(ctarget) = child_target_of(&child) else {
                problems.push(format!("{} (a kit) cannot require the library {name}", pkg.app));
                continue;
            };
            if let Some(inst) = existing.iter().find(|i| i.package == name && i.realm == ctarget && i.state == "installed") {
                let ok = semver::Version::parse(&inst.version).ok().is_some_and(|v| range(&req).map(|r| r.matches(&v)).unwrap_or(false));
                if !ok {
                    problems.push(format!(
                        "{} requires {name} {req}, but {} is installed{}; upgrade it first",
                        pkg.app,
                        inst.version,
                        where_(&ctarget)
                    ));
                    continue;
                }
                locks.insert(name.clone(), inst.digest.clone());
                let i = match nodes.iter().position(|n| n.existing == Some(inst.id)) {
                    Some(i) => i,
                    None => {
                        let (m, _) = by_digest(st, &inst.digest).await?.unwrap_or_default();
                        nodes.push(Node {
                            roles: roles(&m),
                            pkg: m,
                            digest: inst.digest.clone(),
                            target: ctarget,
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
            locks.insert(name.clone(), cdigest.clone());
            let i = Box::pin(visit(st, child, cdigest, ctarget, path, nodes, problems, existing)).await?;
            if i != usize::MAX {
                children.push(i);
            }
        }
        path.pop();
        let i = nodes.len();
        let existing_here = existing.iter().find(|x| x.package == pkg.app && x.realm == target).map(|x| x.id);
        nodes.push(Node { pkg, digest, roles: pkg_roles, target, existing: existing_here, locks, parents: Vec::new() });
        for c in children {
            nodes[c].parents.push(i);
        }
        Ok(i)
    }
    let root_roles = roles(&root);
    let target = if root_roles.iter().any(|r| r == "app") {
        root.realm.clone()
    } else if root_roles.iter().any(|r| r == "library") {
        if library_realm.is_empty() {
            problems.push(format!("{} is a library: say which realm it joins", root.app));
        }
        library_realm.to_owned()
    } else {
        String::new()
    };
    visit(st, root, root_digest, target, &mut Vec::new(), &mut nodes, &mut problems, &existing).await?;
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
        let net = if perm.network == "any" { "Reach the network" } else { "No network" };
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

/// What grew from one version's permissions to another's.
fn grown(old: &api::Permissions, new: &api::Permissions) -> Vec<String> {
    let mut out = Vec::new();
    if old.network != new.network && new.network == "any" {
        out.push("network access".to_owned());
    }
    if old.files == "session-ro" && new.files == "session" {
        out.push("writing files".to_owned());
    }
    if new.cpus > old.cpus || new.memory_mb > old.memory_mb || new.timeout_s > old.timeout_s {
        out.push("more CPU, memory or time".to_owned());
    }
    if new.shell && !old.shell {
        out.push("a shell".to_owned());
    }
    let more: Vec<&String> = new.secrets.iter().filter(|s| !old.secrets.contains(s)).collect();
    if !more.is_empty() {
        out.push(format!("secrets {}", more.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if new.grants.len() > old.grants.len() {
        out.push("more grants".to_owned());
    }
    if new.workers == "dedicated" && old.workers != "dedicated" {
        out.push("a worker of its own".to_owned());
    }
    out
}

/// The plan of installing a package version: Markdown, and what stands in
/// the way.
async fn plan_of(st: &HeadState, nodes: &[Node], mut problems: Vec<String>) -> anyhow::Result<api::AppPlan> {
    let root = nodes.last().ok_or_else(|| anyhow::anyhow!("nothing to install"))?;
    let new: Vec<&Node> = nodes.iter().filter(|n| n.existing.is_none()).collect();
    if root.existing.is_some() {
        problems.push(format!("{} is already installed{}", root.pkg.app, where_(&root.target)));
    }
    let mut lines = Vec::new();
    for n in &new {
        if n.roles.iter().any(|r| r == "app") && crate::realms::exists(st, &n.target).await? {
            problems
                .push(format!("realm {} already exists (upgrade the app installed there, or give the package another realm)", n.target));
        }
        let role = n.roles.join(" and ");
        let place = match n.target.as_str() {
            "" => "for workers to build".to_owned(),
            t if n.roles.iter().any(|r| r == "app") => format!("into a new realm **{t}**"),
            t => format!("into realm **{t}**"),
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
        for name in n.pkg.requires.keys() {
            if let Some(c) = nodes.iter().find(|c| &c.pkg.app == name && c.roles.iter().any(|r| r == "app"))
                && !c.pkg.exports.templates.is_empty() {
                    lines.push(format!("- May start {}'s templates: {}.", c.pkg.app, c.pkg.exports.templates.join(", ")));
                }
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
    let (nodes, problems) = resolve(st, pkg, digest, &req.realm).await?;
    plan_of(st, &nodes, problems).await
}

/// What installing a package not yet in the registry would do.
pub async fn plan_manifest(st: &HeadState, manifest: Value) -> anyhow::Result<api::AppPlan> {
    let p = normalize_manifest(manifest)?;
    let mut problems = check_manifest(&p, &BTreeMap::new());
    problems.retain(|x| !x.contains("among the package's files"));
    let digest = digest_of(&p, &BTreeMap::new());
    let (nodes, more) = resolve(st, p, digest, "").await?;
    problems.extend(more);
    plan_of(st, &nodes, problems).await
}

/// Asks to install a package version and what it requires: one decision.
pub async fn install(st: &HeadState, req: &api::InstallIn) -> anyhow::Result<api::Install> {
    let (pkg, digest) = newest(st, &req.package, &req.version)
        .await?
        .ok_or_else(|| not_found(format!("no package {} {} in the registry", req.package, req.version)))?;
    let (nodes, problems) = resolve(st, pkg, digest, &req.realm).await?;
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
            "INSERT INTO installs (package, version, digest, realm, state, locks, created_at, updated_at) VALUES (?, ?, ?, ?, 'installing', ?, ?, ?) RETURNING id",
        )
        .bind(&n.pkg.app)
        .bind(&n.pkg.version)
        .bind(&n.digest)
        .bind(&n.target)
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
        format!("Install the app {}?", root.pkg.app)
    } else {
        format!("Install the {} {}?", root.roles.join(" and "), root.pkg.app)
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
pub async fn answer(st: &HeadState, reference: &str, option: Option<&str>) -> anyhow::Result<()> {
    let mut parts = reference.splitn(3, ':');
    let (kind, what, key) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let id = match kind {
        "package" => key.parse::<i64>().map_err(|_| bad_request("no such install"))?,
        // An app's decision from before packages: by its realm.
        _ => app_in(st, key).await?.map(|(i, _)| i.id).ok_or_else(|| not_found(format!("no app in {key}")))?,
    };
    let r = row(st, id).await?;
    let pending: Value = r.get::<Option<String>, _>("pending").and_then(|p| serde_json::from_str(&p).ok()).unwrap_or(Value::Null);
    let go = matches!(option, Some("Install" | "Upgrade, keeping my edits" | "Upgrade, taking the new version" | "Upgrade" | "Remove"));
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
        "upgrade" => apply_upgrade(st, id, &pending, option == Some("Upgrade, taking the new version")).await?,
        "remove" => remove_now(st, id).await?,
        _ => {}
    }
    st.registry.bump();
    Ok(())
}

async fn apply_install(st: &HeadState, pending: &Value) -> anyhow::Result<()> {
    let steps = pending["install"].as_array().cloned().unwrap_or_default();
    // Every new app's realm first: libraries join them.
    for step in steps.iter().filter(|s| s["new"] == true) {
        let r = row(st, step["id"].as_i64().unwrap_or_default()).await?;
        let (p, _) = by_digest(st, &r.get::<String, _>("digest")).await?.unwrap_or_default();
        if !p.realm.is_empty() && !crate::realms::exists(st, &p.realm).await? {
            let ri = api::RealmIn { id: p.realm.clone(), name: p.app.clone(), description: p.description.clone(), reads_commons: None };
            crate::realms::create(st, &ri, &format!("app:{}", p.app)).await?;
        }
    }
    for step in &steps {
        let Some(id) = step["id"].as_i64() else { continue };
        if step["new"] == true {
            let r = row(st, id).await?;
            let digest: String = r.get("digest");
            let realm: String = r.get("realm");
            let (p, _) = by_digest(st, &digest).await?.ok_or_else(|| anyhow::anyhow!("package {digest} is gone"))?;
            let roles = roles(&p);
            if roles.iter().any(|r| r == "app") {
                crate::apps::apply_contents(st, &realm, &p, true).await?;
                sqlx::query("UPDATE realms SET app = ? WHERE id = ?").bind(&p.app).bind(&realm).execute(&st.store.pool).await?;
            } else if roles.iter().any(|r| r == "library") {
                crate::apps::apply_contents(st, &realm, &p, false).await?;
            }
            sqlx::query("UPDATE installs SET state = 'installed', approved = ?, installed = ?, pending = NULL, decision = NULL, updated_at = ? WHERE id = ?")
                .bind(serde_json::to_string(&p.permissions)?)
                .bind(serde_json::to_string(&p)?)
                .bind(now_ms())
                .bind(id)
                .execute(&st.store.pool)
                .await?;
            if roles.iter().any(|r| r == "app") {
                crate::apps::next_steps(st, &realm, &p).await;
            }
        }
    }
    // Then who requires what, and what an app requiring an app may start
    // (once every realm exists).
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

/// An app requiring an app may start the templates it exports: a grant
/// from the required app's realm.
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

/// Asks to upgrade an install to the newest version a range allows (or to
/// a version just added).
pub async fn upgrade(st: &HeadState, id: i64, version: &str) -> anyhow::Result<api::Install> {
    let cur = install_by_id(st, id).await?;
    if cur.state != "installed" {
        return Err(bad_request(format!("{} is {}; wait for that first", cur.package, cur.state)));
    }
    let (new, digest) =
        newest(st, &cur.package, version).await?.ok_or_else(|| not_found(format!("no {} {version} in the registry", cur.package)))?;
    if digest == cur.digest {
        return Err(bad_request(format!("{} {} is what is installed", cur.package, new.version)));
    }
    let base: api::AppPackage = serde_json::from_str(&row(st, id).await?.get::<String, _>("installed")).unwrap_or_default();
    let (old, _) = by_digest(st, &cur.digest).await?.unwrap_or_default();
    let base = if base.app.is_empty() { old.clone() } else { base };
    // New requirements come with it.
    let (nodes, mut problems) = resolve(st, new.clone(), digest.clone(), &cur.realm).await?;
    problems.retain(|p| !p.contains("is already installed"));
    let mut lines =
        vec![format!("Upgrade **{}**{} from {} to {}.", cur.package, where_(&cur.realm), cur.version, new.version), String::new()];
    let mut conflicts = Vec::new();
    if !cur.realm.is_empty() {
        let (more, c) = crate::apps::upgrade_lines(st, &cur.realm, &base, &new).await?;
        lines.extend(more);
        conflicts = c;
    }
    let grew = grown(&old.permissions, &new.permissions);
    if !grew.is_empty() {
        lines.push(format!("\nIt asks for more than before: {}.", grew.join(", ")));
        lines.extend(permission_lines(&new).into_iter().map(|p| format!("- {p}")));
    }
    let extra: Vec<&Node> = nodes.iter().filter(|n| n.existing.is_none() && n.pkg.app != cur.package).collect();
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
        let id_here = if n.pkg.app == cur.package && n.target == cur.realm {
            Some(id)
        } else if let Some(e) = n.existing {
            Some(e)
        } else {
            Some(
                sqlx::query_scalar(
                    "INSERT INTO installs (package, version, digest, realm, state, locks, created_at, updated_at) VALUES (?, ?, ?, ?, 'installing', ?, ?, ?) RETURNING id",
                )
                .bind(&n.pkg.app)
                .bind(&n.pkg.version)
                .bind(&n.digest)
                .bind(&n.target)
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
        .bind(json!({"install": steps, "digest": digest, "version": new.version, "locks": locks}).to_string())
        .bind(now)
        .bind(id)
        .execute(&st.store.pool)
        .await?;
    let options: &[&str] = if conflicts.is_empty() {
        &["Upgrade, keeping my edits", "Cancel"]
    } else {
        &["Upgrade, keeping my edits", "Upgrade, taking the new version", "Cancel"]
    };
    ask(st, id, "upgrade", &format!("Upgrade {} to {}?", cur.package, new.version), &lines.join("\n"), options).await
}

async fn apply_upgrade(st: &HeadState, id: i64, pending: &Value, take_theirs: bool) -> anyhow::Result<()> {
    let cur = install_by_id(st, id).await?;
    let digest = pending["digest"].as_str().unwrap_or_default();
    let (new, _) = by_digest(st, digest).await?.ok_or_else(|| anyhow::anyhow!("the new version is gone"))?;
    // Its new requirements first.
    let steps: Vec<Value> = pending["install"].as_array().cloned().unwrap_or_default();
    let others: Vec<Value> = steps.iter().filter(|s| s["id"].as_i64() != Some(id)).cloned().collect();
    apply_install(st, &json!({"install": others})).await?;
    if !cur.realm.is_empty() {
        let base: api::AppPackage = serde_json::from_str(&row(st, id).await?.get::<String, _>("installed")).unwrap_or_default();
        crate::apps::apply_upgrade(st, &cur.realm, &base, &new, take_theirs).await?;
    }
    sqlx::query(
        "UPDATE installs SET state = 'installed', digest = ?, version = ?, locks = ?, approved = ?, installed = ?, pending = NULL, decision = NULL, updated_at = ? WHERE id = ?",
    )
    .bind(digest)
    .bind(&new.version)
    .bind(pending["locks"].to_string())
    .bind(serde_json::to_string(&new.permissions)?)
    .bind(serde_json::to_string(&new)?)
    .bind(now_ms())
    .bind(id)
    .execute(&st.store.pool)
    .await?;
    Ok(())
}

/// Asks to remove an install (an app: with its realm and everything in it).
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
    let (doc, title) = if cur.roles.iter().any(|r| r == "app") {
        (
            format!(
                "Removing **{}** deletes realm **{}** and everything in it: its sessions and their history, collections and records, series, \
                 triggers, views and other settings, decisions, secrets, memory notes and knowledge. Its grants end. Model usage stays in the \
                 usage ledger. This cannot be undone.\n\nExport it first (Settings → System → Packages → Export, with data) to keep a copy you can \
                 install again.",
                cur.package, cur.realm
            ),
            format!("Remove the app {} and its realm?", cur.package),
        )
    } else if cur.roles.iter().any(|r| r == "library") {
        (
            format!(
                "Removing **{}** from realm **{}** takes out the settings entries and triggers it added (as they are now). Collections and their records stay.",
                cur.package, cur.realm
            ),
            format!("Remove {} from {}?", cur.package, cur.realm),
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
    let (p, _) = by_digest(st, &cur.digest).await?.unwrap_or_default();
    if cur.roles.iter().any(|r| r == "app") {
        crate::apps::remove_now(st, &cur.realm).await?;
    } else if cur.roles.iter().any(|r| r == "library") {
        // Its list entries and triggers leave the realm.
        for (key, entries) in &p.contents.lists {
            let Some(l) = crate::lists::list(key) else { continue };
            let ids: Vec<String> = entries.iter().map(|e| crate::lists::identity(l, e)).collect();
            let mut all = crate::lists::read(&st.store.pool, key, &cur.realm).await?;
            all.retain(|e| !ids.contains(&crate::lists::identity(l, e)));
            crate::apps::write_list(st, &cur.realm, key, all).await?;
        }
        let names: Vec<String> = p.contents.triggers.iter().filter_map(|t| t.name.clone()).collect();
        for t in crate::triggers::list(st, &crate::realms::Caller::in_realm(&cur.realm)).await? {
            if names.contains(&t.name) {
                crate::triggers::delete(st, &t.id).await?;
            }
        }
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

/// `POST /api/v1/apps`: the package into the registry, then installed.
pub async fn install_manifest(st: &HeadState, manifest: Value) -> anyhow::Result<api::AppInstall> {
    let info = add(st, manifest, BTreeMap::new(), "local").await?;
    let i = install(st, &api::InstallIn { package: info.name.clone(), version: info.version.clone(), realm: String::new() }).await?;
    Ok(app_install(&i, &info.description))
}

/// `POST /api/v1/apps/{realm}/upgrade`: the new version into the registry,
/// then the app in that realm upgraded to it.
pub async fn upgrade_manifest(st: &HeadState, realm: &str, manifest: Value) -> anyhow::Result<api::AppInstall> {
    let (cur, _) = app_in(st, realm).await?.ok_or_else(|| not_found(format!("no app installed in {realm}")))?;
    let p = normalize_manifest(manifest.clone())?;
    if p.app != cur.package {
        return Err(bad_request(format!("realm {realm} has the app {}, not {}", cur.package, p.app)));
    }
    let info = add(st, manifest, BTreeMap::new(), "local").await?;
    let i = upgrade(st, cur.id, &info.version).await?;
    Ok(app_install(&i, &info.description))
}

pub async fn remove_app(st: &HeadState, realm: &str) -> anyhow::Result<api::AppInstall> {
    let (cur, m) = app_in(st, realm).await?.ok_or_else(|| not_found(format!("no app installed in {realm}")))?;
    let i = remove(st, cur.id).await?;
    Ok(app_install(&i, &m.description))
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

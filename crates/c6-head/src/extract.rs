//! Reading files for the knowledge store (docs/knowledge.md §4): the head
//! never parses mail, documents, photos or videos itself. The c6-extract
//! kit (its source is part of carbon6, built on the worker) reads them on
//! a worker with a container runtime: the store's own files are sent with
//! the job, a watched folder's are mounted read-only where they are.
//!
//! The kit is installed with your approval: the first time something needs
//! reading, its package is added and you are asked.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use c6_api as api;
use c6_core::WorkerId;
use c6_memex::extract::{Extractor, File, Request, Response};
use serde_json::{Value, json};

use crate::HeadState;

pub const KIT: &str = "c6-extract";

/// The kit's package: its manifest, and the source the worker builds.
pub fn package() -> (Value, BTreeMap<String, Vec<u8>>) {
    macro_rules! files {
        ($($path:literal),*) => {
            BTreeMap::from([$(($path.to_owned(), include_bytes!(concat!("../../c6-extract/", $path)).to_vec())),*])
        };
    }
    let files = files!(
        "Dockerfile",
        "Cargo.toml",
        "Cargo.lock",
        "src/lib.rs",
        "src/main.rs",
        "src/docs.rs",
        "src/mail.rs",
        "src/media.rs",
        "src/faces.rs"
    );
    (serde_json::from_str(include_str!("../../c6-extract/kit.json")).expect("the kit's manifest"), files)
}

pub struct KitExtractor {
    pub st: Weak<HeadState>,
}

/// Calls to workers for folders: listing one can take a while.
const WALK: Duration = Duration::from_secs(300);

impl KitExtractor {
    fn st(&self) -> anyhow::Result<Arc<HeadState>> {
        self.st.upgrade().ok_or_else(|| anyhow::anyhow!("the head is stopping"))
    }

    /// The kit's install, once you approved it; else asks you (once).
    /// A newer kit (this carbon6's) is offered as an upgrade.
    async fn kit(st: &HeadState) -> anyhow::Result<api::Install> {
        let installs = crate::packages::installs(st).await?;
        if let Some(i) = installs.iter().find(|i| i.package == KIT && i.state == "installed") {
            let (manifest, files) = package();
            let ours = manifest["version"].as_str().unwrap_or_default().to_owned();
            let newer = semver::Version::parse(&ours).ok() > semver::Version::parse(&i.version).ok();
            if newer && i.decision.is_none() {
                crate::packages::add(st, manifest, files, "system").await?;
                if let Err(e) = crate::packages::upgrade(st, i.id, &ours).await {
                    tracing::warn!("offering c6-extract {ours}: {e:#}");
                }
            }
            return Ok(i.clone());
        }
        if !installs.iter().any(|i| i.package == KIT && i.state == "installing") {
            let (manifest, files) = package();
            crate::packages::add(st, manifest, files, "system").await?;
            crate::packages::install(st, &api::InstallIn { package: KIT.into(), ..Default::default() }).await?;
        }
        anyhow::bail!("reading files waits for you to approve the c6-extract kit (Decisions)")
    }
}

/// You approved the kit: what waits to be read goes now.
pub async fn approved(st: &HeadState) {
    if let Some(m) = crate::knowledge::get(st) {
        m.extract_now().await;
    }
}

/// The worker a folder is on: by name (or id); "" is the one on the
/// head's machine.
pub async fn worker(st: &HeadState, name: &str) -> anyhow::Result<WorkerId> {
    let rows = st.store.workers().await?;
    let host = gethostname::gethostname().into_string().unwrap_or_default().to_lowercase();
    let short = |h: &str| h.trim_end_matches(".local").to_lowercase();
    let candidates: Vec<&crate::store::WorkerRow> = if name.is_empty() {
        rows.iter().filter(|r| r.revoked_at.is_none() && r.role != "memex" && short(&r.hostname) == short(&host)).collect()
    } else {
        rows.iter().filter(|r| r.revoked_at.is_none() && (r.name == name || r.id == name)).collect()
    };
    // One that is connected, if any is (a machine can have several).
    let connected = |r: &&&crate::store::WorkerRow| r.id.parse().is_ok_and(|id| st.registry.get(WorkerId(id)).is_some());
    let row = candidates.iter().find(connected).or(candidates.first()).copied();
    let row = row.ok_or_else(|| {
        anyhow::anyhow!(if name.is_empty() {
            "no worker runs on the head's machine (name the folder's worker in Settings → Knowledge)".to_owned()
        } else {
            format!("no worker {name:?}")
        })
    })?;
    Ok(WorkerId(row.id.parse()?))
}

#[async_trait::async_trait]
impl Extractor for KitExtractor {
    async fn extract(&self, req: Request) -> anyhow::Result<Response> {
        let st = self.st()?;
        let kit = Self::kit(&st).await?;
        let spec = crate::packages::kit_spec_of(&st, &kit).await?;
        let mut inputs = BTreeMap::new();
        let mut mounts = Vec::new();
        let mut on: Option<String> = None;
        let mut jobs = Vec::new();
        for (n, j) in req.jobs.iter().enumerate() {
            let path = match &j.file {
                File::Bytes(b) => {
                    inputs.insert(format!("in/{}", j.id), b.clone());
                    format!("/work/in/{}", j.id)
                }
                File::At { worker, path } => {
                    on = Some(worker.clone());
                    let at = format!("/mnt/{n}");
                    mounts.push((path.clone(), at.clone()));
                    at
                }
            };
            jobs.push(json!({"id": j.id, "name": j.name, "path": path, "index": j.index, "sound": j.sound}));
        }
        let mut request = json!({"tool": req.tool, "jobs": jobs});
        if let Some((detector, embedder)) = req.models {
            inputs.insert("models/detector.onnx".into(), detector);
            inputs.insert("models/embedder.onnx".into(), embedder);
            request["detector"] = json!("/work/models/detector.onnx");
            request["embedder"] = json!("/work/models/embedder.onnx");
        }
        inputs.insert("jobs.json".into(), serde_json::to_vec(&request)?);
        let worker = match &on {
            Some(w) => Some(worker(&st, w).await?),
            None => None,
        };
        let job = crate::kitjobs::Job { spec, tool: "extract".into(), inputs, worker, mounts, ..Default::default() };
        let out = crate::kitjobs::run(&st, job).await?;
        let result: Value = match out.files.get("result.json") {
            Some(r) => serde_json::from_slice(r)?,
            None => anyhow::bail!("c6-extract exited {:?}: {}", out.exit, out.stderr.trim()),
        };
        if let Some(e) = result["error"].as_str() {
            anyhow::bail!("{e}");
        }
        let files = out.files.into_iter().filter(|(k, _)| k != "result.json").collect();
        Ok(Response { results: result["results"].as_object().cloned().unwrap_or_default(), files })
    }

    async fn walk(&self, worker_name: &str, root: &str) -> anyhow::Result<(String, Vec<(String, u64, i64)>)> {
        let st = self.st()?;
        let w = worker(&st, worker_name).await?;
        let v = st.registry.call(w, "fs.walk", json!({"path": root}), WALK).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let files = serde_json::from_value(v["files"].clone())?;
        Ok((v["root"].as_str().unwrap_or(root).to_owned(), files))
    }

    async fn read(&self, worker_name: &str, path: &str, max: u64) -> anyhow::Result<Vec<u8>> {
        use base64::Engine;
        let st = self.st()?;
        let w = worker(&st, worker_name).await?;
        let v = st.registry.call(w, "fs.read_file", json!({"path": path, "max": max}), WALK).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(base64::engine::general_purpose::STANDARD.decode(v["data"].as_str().unwrap_or_default())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kit's package is content-addressed: its source can't change
    /// under the same version (the registry refuses it on heads that have
    /// it). Change the source, and bump the version in kit.json, and the
    /// digest recorded in kit.digest.
    #[test]
    fn the_kit_version_follows_its_source() {
        let (manifest, files) = package();
        let mut h = String::new();
        for (path, data) in &files {
            h.push_str(&format!("{path} {}\n", c6_core::sha256_hex(data)));
        }
        let digest = c6_core::sha256_hex(h.as_bytes());
        let recorded = include_str!("../../c6-extract/kit.digest").trim();
        let now = format!("{} {digest}", manifest["version"].as_str().unwrap_or_default());
        assert_eq!(
            recorded, now,
            "c6-extract's source changed: bump the version in crates/c6-extract/kit.json, and write `{now}` to kit.digest"
        );
    }
}

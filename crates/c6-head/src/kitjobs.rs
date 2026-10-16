//! Kit jobs (docs/knowledge.md §6.2): the head runs a kit tool on a worker
//! that has a container runtime, with no session and no model, in the
//! sandbox the kit's permissions describe. What the run leaves in `out/`
//! comes back; an `out/ingest.json` becomes data the asking app owns.
//! Triggers (the `kit` action) and operations (kind `kit`) start jobs, and
//! the knowledge store's extraction runs through them.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine;
use c6_api as api;
use serde_json::{Value, json};

use crate::{HeadState, bad_request};

/// A first run may build the kit's image (up to half an hour) before its
/// own time limit.
const BUILD_ALLOWANCE: Duration = Duration::from_secs(35 * 60);

/// A tool run to do.
#[derive(Default)]
pub struct Job {
    /// What `packages::kit_spec` gives: builds, image, tools, permissions.
    pub spec: Value,
    pub tool: String,
    /// The tool's arguments: a `path` argument names a file in the job's
    /// folder (an input, or one under `out/`), a `text` argument is as given.
    pub args: serde_json::Map<String, Value>,
    /// Files written into the job's folder before the run.
    pub inputs: BTreeMap<String, Vec<u8>>,
    /// Environment for this run only (access tokens, approved secrets).
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Default)]
pub struct Output {
    pub exit: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// What it left in `out/`, by path.
    pub files: BTreeMap<String, Vec<u8>>,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.exit == Some(0)
    }
}

/// The command a tool runs, its `{arg}`s filled in.
fn command(spec: &Value, tool: &str, args: &serde_json::Map<String, Value>) -> anyhow::Result<Vec<String>> {
    let t = spec["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["name"] == tool)
        .ok_or_else(|| bad_request(format!("{} has no tool {tool:?}", spec["name"].as_str().unwrap_or("the kit"))))?;
    let kinds = t["args"].as_object().cloned().unwrap_or_default();
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    for (name, kind) in &kinds {
        let v = args.get(name).map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).unwrap_or_default();
        let v = if kind == "path" {
            if v.starts_with('/') || v.split('/').any(|s| s == "..") {
                return Err(bad_request(format!("{name}: a path inside the job's folder")));
            }
            format!("/work/{v}")
        } else {
            v
        };
        values.insert(name.clone(), v);
    }
    Ok(t["command"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|part| values.iter().fold(part.to_owned(), |acc, (k, v)| acc.replace(&format!("{{{k}}}"), v)))
        .collect())
}

/// Runs a job on a worker that can, and returns what it made.
pub async fn run(st: &HeadState, job: Job) -> anyhow::Result<Output> {
    let workers = st.registry.with_capability("kits");
    let worker = *workers
        .first()
        .ok_or_else(|| bad_request("no worker with a container runtime (Docker or Podman) is connected: kit jobs run on one"))?;
    let cmd = command(&job.spec, &job.tool, &job.args)?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let inputs: serde_json::Map<String, Value> = job.inputs.iter().map(|(k, v)| (k.clone(), json!(b64.encode(v)))).collect();
    let limit = Duration::from_secs(job.spec["permissions"]["timeout_s"].as_u64().unwrap_or(120)) + BUILD_ALLOWANCE;
    let params = json!({"spec": job.spec, "command": cmd, "inputs": inputs, "env": job.env});
    let v = st.registry.call(worker, "kit.job", params, limit).await.map_err(|e| anyhow::anyhow!("kit job: {e}"))?;
    let mut files = BTreeMap::new();
    for (k, data) in v["files"].as_object().into_iter().flatten() {
        files.insert(k.clone(), b64.decode(data.as_str().unwrap_or_default())?);
    }
    Ok(Output {
        exit: v["exit"].as_i64().map(|c| c as i32),
        stdout: v["stdout"].as_str().unwrap_or_default().to_owned(),
        stderr: v["stderr"].as_str().unwrap_or_default().to_owned(),
        files,
    })
}

/// What a run's `out/ingest.json` asks to keep, as the app that ran it
/// (docs/knowledge.md §6.2, #4): records of its collections, knowledge
/// items, things and claims. Each is checked like any write of that owner.
pub async fn ingest(st: &HeadState, realm: &str, instance: &str, by: &str, batch: &Value) -> anyhow::Result<Value> {
    let acting = format!("app:{instance} {by}");
    let mut done = json!({"records": 0, "items": 0, "things": 0, "claims": 0, "errors": []});
    let mut errors: Vec<String> = Vec::new();
    for r in batch["records"].as_array().into_iter().flatten() {
        let collection = r["collection"].as_str().unwrap_or_default();
        let rec = api::RecordIn {
            fields: r["fields"].clone(),
            version: r["version"].as_i64(),
            derived_from: serde_json::from_value(r["derived_from"].clone()).ok(),
            links: serde_json::from_value(r["links"].clone()).ok(),
            labels: None,
        };
        let id = r["id"].as_str().filter(|i| !i.is_empty());
        // An update by key: the record with that key's value, if any.
        let id = match (id, r["key"].as_str()) {
            (Some(i), _) => Some(i.to_owned()),
            (None, Some(k)) => {
                let mut filter = serde_json::Map::new();
                filter.insert(k.to_owned(), r["fields"][k].clone());
                let q = api::RecordQuery { filter, limit: Some(1), ..Default::default() };
                crate::collections::query(st, realm, collection, &q).await?.records.first().map(|x| x.id.clone())
            }
            _ => None,
        };
        let rec = match &id {
            Some(i) => {
                let cur = crate::collections::record(st, realm, collection, i).await?;
                api::RecordIn { version: Some(cur.version), ..rec }
            }
            None => rec,
        };
        match crate::collections::put(st, realm, collection, id.as_deref(), &rec, &acting).await {
            Ok(_) => done["records"] = json!(done["records"].as_i64().unwrap_or(0) + 1),
            Err(e) => errors.push(format!("record in {collection}: {e:#}")),
        }
    }
    for t in batch["things"].as_array().into_iter().flatten() {
        match crate::claims::call(st, realm, "memex.things.create", t.clone(), &format!("app:{instance}"), &acting).await {
            Ok(_) => done["things"] = json!(done["things"].as_i64().unwrap_or(0) + 1),
            Err(e) => errors.push(format!("thing: {e:#}")),
        }
    }
    for it in batch["items"].as_array().into_iter().flatten() {
        let mut item = it.clone();
        item["source"] = json!(format!("app:{instance}"));
        match crate::memex::call_in(st, realm, "memex.ingest", item).await {
            Ok(_) => done["items"] = json!(done["items"].as_i64().unwrap_or(0) + 1),
            Err(e) => errors.push(format!("item: {e:#}")),
        }
    }
    for c in batch["claims"].as_array().into_iter().flatten() {
        match crate::claims::call(st, realm, "memex.claims.add", c.clone(), &format!("app:{instance}"), &acting).await {
            Ok(_) => done["claims"] = json!(done["claims"].as_i64().unwrap_or(0) + 1),
            Err(e) => errors.push(format!("claim: {e:#}")),
        }
    }
    done["errors"] = json!(errors);
    Ok(done)
}

/// Runs a kit tool for an app instance (an operation's or a trigger's):
/// the kit must be one its package requires; the job gets the kit's
/// approved secrets of the realm, and the app's access tokens; an
/// `out/ingest.json` is kept as the app's.
#[allow(clippy::too_many_arguments)]
pub async fn for_instance(
    st: &HeadState,
    realm: &str,
    instance: &str,
    kit: &str,
    tool: &str,
    args: serde_json::Map<String, Value>,
    inputs: BTreeMap<String, Vec<u8>>,
    by: &str,
) -> anyhow::Result<Value> {
    let i = crate::instances::get(&st.store.pool, realm, instance)
        .await?
        .ok_or_else(|| bad_request(format!("no app {instance} in realm {realm}")))?;
    let install = crate::packages::install_by_id(st, i.id).await?;
    let digest = install.locks.get(kit).cloned().ok_or_else(|| bad_request(format!("{instance} does not require the kit {kit}")))?;
    let kit_install = crate::packages::installs(st)
        .await?
        .into_iter()
        .find(|k| k.digest == digest && k.state == "installed")
        .ok_or_else(|| bad_request(format!("the kit {kit} is not installed")))?;
    let spec = crate::packages::kit_spec_of(st, &kit_install).await?;
    let mut env = Vec::new();
    for s in spec["permissions"]["secrets"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if let Some(v) = crate::secrets::value_in(st, realm, s).await? {
            env.push((s.to_owned(), v));
        }
    }
    env.extend(crate::providers::tokens_for(st, realm, instance).await?);
    let out = run(st, Job { spec, tool: tool.to_owned(), args, inputs, env }).await?;
    let mut result = json!({"exit": out.exit, "stdout": out.stdout, "stderr": out.stderr,
        "files": out.files.keys().cloned().collect::<Vec<_>>()});
    if let Some(batch) = out.files.get("ingest.json") {
        let batch: Value = serde_json::from_slice(batch).map_err(|e| bad_request(format!("{tool}: out/ingest.json is not JSON: {e}")))?;
        result["ingested"] = ingest(st, realm, instance, &format!("kit:{kit}.{tool} {by}"), &batch).await?;
    }
    if !out.ok() {
        return Err(bad_request(format!("{kit}.{tool} exited {:?}: {}", out.exit, out.stderr.trim())));
    }
    Ok(result)
}

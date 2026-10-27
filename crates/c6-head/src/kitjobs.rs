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
    /// The worker to run on (default: any that can).
    pub worker: Option<c6_core::WorkerId>,
    /// The worker's own files the run reads: (its path, where under /mnt).
    pub mounts: Vec<(String, String)>,
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

/// A kit run in this process instead of a container (tests: the kit's own
/// code, against stand-ins for what it talks to).
pub struct LocalRun {
    pub tool: String,
    pub command: Vec<String>,
    /// The job's folder's files before the run.
    pub inputs: BTreeMap<String, Vec<u8>>,
    pub env: Vec<(String, String)>,
}

pub type LocalKit = std::sync::Arc<dyn Fn(LocalRun) -> futures::future::BoxFuture<'static, anyhow::Result<Output>> + Send + Sync>;

static LOCAL: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, LocalKit>>> =
    std::sync::LazyLock::new(Default::default);

/// Runs this kit's jobs in this process, for every head here (tests).
pub fn run_locally(kit: &str, f: LocalKit) {
    LOCAL.lock().unwrap().insert(kit.to_owned(), f);
}

/// Runs a job on a worker that can, and returns what it made.
pub async fn run(st: &HeadState, job: Job) -> anyhow::Result<Output> {
    let local = LOCAL.lock().unwrap().get(job.spec["name"].as_str().unwrap_or_default()).cloned();
    if let Some(f) = local {
        let command = command(&job.spec, &job.tool, &job.args)?;
        return f(LocalRun { tool: job.tool, command, inputs: job.inputs, env: job.env }).await;
    }
    let workers = st.registry.with_capability("kits");
    let worker = match job.worker {
        Some(w) if workers.contains(&w) => w,
        Some(_) => return Err(bad_request("that worker is not connected, or has no container runtime (Docker or Podman)")),
        None => *workers
            .first()
            .ok_or_else(|| bad_request("no worker with a container runtime (Docker or Podman) is connected: kit jobs run on one"))?,
    };
    let cmd = command(&job.spec, &job.tool, &job.args)?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let inputs: serde_json::Map<String, Value> = job.inputs.iter().map(|(k, v)| (k.clone(), json!(b64.encode(v)))).collect();
    let limit = Duration::from_secs(job.spec["permissions"]["timeout_s"].as_u64().unwrap_or(120)) + BUILD_ALLOWANCE;
    let mounts: Vec<Value> = job.mounts.iter().map(|(path, at)| json!({"path": path, "at": at})).collect();
    let params = json!({"spec": job.spec, "command": cmd, "inputs": inputs, "env": job.env, "mounts": mounts});
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
    ingest_with(st, realm, instance, by, batch, &BTreeMap::new()).await
}

/// `ingest`, with the files the run made (typed items' bytes).
pub async fn ingest_with(
    st: &HeadState,
    realm: &str,
    instance: &str,
    by: &str,
    batch: &Value,
    files: &BTreeMap<String, Vec<u8>>,
) -> anyhow::Result<Value> {
    let acting = format!("app:{instance} {by}");
    let mut done = json!({"records": 0, "items": 0, "things": 0, "claims": 0, "errors": []});
    let mut errors: Vec<String> = Vec::new();
    for t in batch["things"].as_array().into_iter().flatten() {
        match crate::claims::call(st, realm, "memex.things.create", t.clone(), &format!("app:{instance}"), &acting).await {
            Ok(_) => done["things"] = json!(done["things"].as_i64().unwrap_or(0) + 1),
            Err(e) => errors.push(format!("thing: {e:#}")),
        }
    }
    // Typed items (a message, an event, a contact) go to the knowledge
    // store's handlers, written as the app; their bytes are files it made.
    let mut typed = Vec::new();
    for it in batch["items"].as_array().into_iter().flatten().filter(|i| i["type"].is_string()) {
        let mut item = it.clone();
        if let Some(f) = it["file"].as_str() {
            match files.get(f) {
                Some(b) => item["data"] = json!(base64::engine::general_purpose::STANDARD.encode(b)),
                None => {
                    errors.push(format!("item {}: no file {f}", it["external_id"]));
                    continue;
                }
            }
        }
        if let Some(o) = item.as_object_mut() {
            o.remove("file");
            o.insert("source".into(), json!({"kind": "app"}));
        }
        typed.push(item);
    }
    for chunk in typed.chunks(200) {
        let params = json!({"items": chunk, "as": format!("app:{instance}"), "wait": false});
        match crate::memex::call_in(st, realm, "memex.ingest", params).await {
            Ok(v) => {
                for r in v["results"].as_array().into_iter().flatten() {
                    match r["status"].as_str() {
                        Some("rejected") => errors.push(format!("item: {}", r["error"].as_str().unwrap_or_default())),
                        _ => done["items"] = json!(done["items"].as_i64().unwrap_or(0) + 1),
                    }
                }
            }
            Err(e) => errors.push(format!("items: {e:#}")),
        }
    }
    for d in batch["deleted"].as_array().into_iter().flatten() {
        let params = json!({"source": format!("app:{instance}"), "external_id": d["external_id"]});
        if let Err(e) = crate::memex::call_in(st, realm, "memex.delete", params).await {
            errors.push(format!("deleting {}: {e:#}", d["external_id"]));
        }
    }
    errors.extend(batch["errors"].as_array().into_iter().flatten().filter_map(|e| e.as_str().map(str::to_owned)));
    for it in batch["items"].as_array().into_iter().flatten().filter(|i| !i["type"].is_string()) {
        // An item of a shared kind has its kind's shape (docs/knowledge.md §6.2, #6).
        if let Some(schema) = crate::claims::kind_schema(st, it["kind"].as_str().unwrap_or_default()).await {
            let meta = if it["meta"].is_null() { json!({}) } else { it["meta"].clone() };
            if let Err(why) = crate::schema::validate(&schema, &meta) {
                errors.push(format!("item {:?} ({}): {}", it["title"].as_str().unwrap_or_default(), it["kind"], why.join("; ")));
                continue;
            }
        }
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
    // Records last: a sync's place moves on only once what it brought is
    // kept (a run stopped partway leaves it where it was).
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
    done["errors"] = json!(errors);
    Ok(done)
}

/// Runs a kit tool for an app instance (an operation's or a trigger's):
/// the kit must be one its package requires; the job gets the kit's
/// approved secrets of the realm, and the app's access tokens; an
/// `out/ingest.json` is kept as the app's.
///
/// The run also gets the app's collections it asks for, as
/// `state/<collection>.json` (where a sync is, say), and an operation's
/// input with its arguments as `input.json`. What it says in `result.json`
/// is the answer; `again` in its batch asks for another run soon.
#[allow(clippy::too_many_arguments)]
pub async fn for_instance(
    st: &HeadState,
    realm: &str,
    instance: &str,
    kit: &str,
    tool: &str,
    args: serde_json::Map<String, Value>,
    mut inputs: BTreeMap<String, Vec<u8>>,
    by: &str,
    state: &[String],
    input: Option<&Value>,
) -> anyhow::Result<Value> {
    for c in state {
        let col = crate::collections::get(st, realm, c).await?;
        if col.owner != instance {
            return Err(bad_request(format!("{instance} does not own the collection {c}")));
        }
        let page = crate::collections::query(st, realm, c, &api::RecordQuery { limit: Some(10_000), ..Default::default() }).await?;
        let records: Vec<Value> = page.records.iter().map(|r| json!({"id": r.id, "fields": r.fields})).collect();
        inputs.insert(format!("state/{c}.json"), serde_json::to_vec(&records)?);
    }
    if let Some(i) = input {
        let mut merged = if i.is_object() { i.clone() } else { json!({}) };
        for (k, v) in &args {
            merged[k] = v.clone();
        }
        inputs.insert("input.json".into(), serde_json::to_vec(&merged)?);
    }
    let i = crate::instances::get(&st.store.pool, realm, instance)
        .await?
        .ok_or_else(|| bad_request(format!("no app {instance} in realm {realm}")))?;
    let install = crate::packages::install_by_id(st, i.id).await?;
    let digest = install.locks.get(kit).cloned().ok_or_else(|| bad_request(format!("{instance} does not require the kit {kit}")))?;
    let installs = crate::packages::installs(st).await?;
    let kit_install = match installs.iter().find(|k| k.digest == digest && k.state == "installed") {
        Some(k) => k.clone(),
        // Upgraded in place before upgrades moved what required it along:
        // the kit of that name, and the lock follows it.
        None => {
            let k = installs
                .iter()
                .find(|k| k.package == kit && k.realm.is_empty() && k.state == "installed")
                .cloned()
                .ok_or_else(|| bad_request(format!("the kit {kit} is not installed")))?;
            crate::packages::relock(st, &digest, &k.digest).await?;
            k
        }
    };
    let spec = crate::packages::kit_spec_of(st, &kit_install).await?;
    let mut env = Vec::new();
    for s in spec["permissions"]["secrets"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if let Some(v) = crate::secrets::value_in(st, realm, s).await? {
            env.push((s.to_owned(), v));
        }
    }
    env.extend(crate::providers::tokens_for(st, realm, instance).await?);
    let out = run(st, Job { spec, tool: tool.to_owned(), args, inputs, env, ..Default::default() }).await?;
    let mut result = json!({"exit": out.exit, "stdout": out.stdout, "stderr": out.stderr,
        "files": out.files.keys().cloned().collect::<Vec<_>>()});
    if let Some(batch) = out.files.get("ingest.json") {
        let batch: Value = serde_json::from_slice(batch).map_err(|e| bad_request(format!("{tool}: out/ingest.json is not JSON: {e}")))?;
        result["ingested"] = ingest_with(st, realm, instance, &format!("kit:{kit}.{tool} {by}"), &batch, &out.files).await?;
        result["again"] = json!(batch["again"].as_bool() == Some(true));
    }
    if let Some(r) = out.files.get("result.json") {
        result["result"] = serde_json::from_slice(r).unwrap_or(Value::Null);
    }
    if !out.ok() {
        return Err(bad_request(format!("{kit}.{tool} exited {:?}: {}", out.exit, out.stderr.trim())));
    }
    Ok(result)
}

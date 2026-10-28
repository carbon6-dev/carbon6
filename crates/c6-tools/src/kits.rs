//! Kits (docs/packages.md §5): software a session's realm may use,
//! packaged as images this worker builds from their definitions (kits
//! built from kits first) and runs, each tool call in a sandbox shaped by
//! the permissions you approved: the session's folder at /work and nothing
//! else of this machine, no network unless allowed, no privileges, bounded
//! CPU, memory and time.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use c6_llm::ToolSpec;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

/// Output kept from a run or a failed build.
const MAX_OUTPUT: usize = 30_000;
/// A build that takes longer than this is stopped.
const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Where container runtimes and their helpers (credential helpers, say)
/// are, beyond PATH: a service's PATH (launchd, systemd) is short.
const DIRS: [&str; 7] = [
    "/usr/local/bin",
    "/opt/homebrew/bin",
    "/Applications/Docker.app/Contents/Resources/bin",
    "~/.docker/bin",
    "~/.rd/bin",
    "/opt/podman/bin",
    "/usr/bin",
];

/// PATH, and the places runtimes live.
fn search_path() -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    for d in DIRS {
        let d = match d.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(d),
        };
        if d.is_dir() && !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

/// A command of the runtime, finding its helpers wherever they are.
pub fn runtime_command(rt: &str) -> Command {
    let mut c = Command::new(rt);
    c.env("PATH", search_path());
    c
}

/// The container runtime here: `docker` or `podman` (or what
/// C6_CONTAINER_RUNTIME names), if it answers. Found once; not found, it is
/// looked for again a minute later (Docker Desktop may still be starting).
pub async fn runtime() -> Option<String> {
    static FOUND: OnceLock<String> = OnceLock::new();
    static LOOKED: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    if let Some(r) = FOUND.get() {
        return Some(r.clone());
    }
    if LOOKED.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
        return None;
    }
    *LOOKED.lock().unwrap() = Some(std::time::Instant::now());
    let mut names: Vec<String> = Vec::new();
    if let Ok(r) = std::env::var("C6_CONTAINER_RUNTIME") {
        names.push(r);
    }
    names.extend(["docker".into(), "podman".into()]);
    let path = search_path();
    for name in names {
        // By its full path, so a short PATH doesn't hide it.
        let full = if name.contains('/') {
            PathBuf::from(&name)
        } else {
            match std::env::split_paths(&path).map(|d| d.join(&name)).find(|p| p.is_file()) {
                Some(p) => p,
                None => continue,
            }
        };
        let full = full.display().to_string();
        let ok = tokio::time::timeout(
            Duration::from_secs(15),
            runtime_command(&full).args(["version", "--format", "{{.Server.Version}}"]).output(),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .is_some_and(|o| o.status.success());
        if ok {
            let _ = FOUND.set(full.clone());
            return Some(full);
        }
    }
    None
}

async fn head_call(ctx: &ToolCtx, method: &str, params: Value) -> Result<Value, String> {
    let head = ctx.session.head().ok_or("carbon6's head is not reachable from here")?;
    head.call(method, params).await
}

fn clip(s: &str) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.to_owned();
    }
    let mut cut = s.len() - MAX_OUTPUT;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    format!("…{}", &s[cut..])
}

/// Builds what is missing: each image in order, from its Dockerfile and
/// files, in a context folder of its own.
async fn ensure_images(rt: &str, spec: &Value) -> Result<(), String> {
    for b in spec["builds"].as_array().into_iter().flatten() {
        let tag = b["tag"].as_str().unwrap_or_default();
        let have = runtime_command(rt).args(["image", "inspect", tag]).output().await.map(|o| o.status.success()).unwrap_or(false);
        if have {
            continue;
        }
        let dir = std::env::temp_dir().join(format!("c6kit-build-{}", tag.replace(['/', ':'], "-")));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.map_err(|e| format!("preparing the build: {e}"))?;
        tokio::fs::write(dir.join("Dockerfile.c6kit"), b["dockerfile"].as_str().unwrap_or_default()).await.map_err(|e| e.to_string())?;
        for (path, data) in b["files"].as_object().into_iter().flatten() {
            if path.starts_with('/') || path.split('/').any(|s| s == "..") {
                return Err(format!("{path}: not a path inside the kit"));
            }
            let data = base64::engine::general_purpose::STANDARD.decode(data.as_str().unwrap_or_default()).map_err(|e| e.to_string())?;
            let full = dir.join(path);
            if let Some(parent) = full.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
            }
            tokio::fs::write(&full, data).await.map_err(|e| e.to_string())?;
        }
        let build =
            runtime_command(rt).args(["build", "-t", tag, "-f"]).arg(dir.join("Dockerfile.c6kit")).arg(&dir).kill_on_drop(true).output();
        let out = tokio::time::timeout(BUILD_TIMEOUT, build).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let out =
            out.map_err(|_| format!("building {} took too long", b["name"].as_str().unwrap_or("the kit")))?.map_err(|e| e.to_string())?;
        if !out.status.success() {
            let log = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            return Err(format!(
                "building {} {} failed:\n{}",
                b["name"].as_str().unwrap_or(""),
                b["version"].as_str().unwrap_or(""),
                clip(&log)
            ));
        }
    }
    Ok(())
}

/// Where a path argument is inside the session's folder, as seen in the
/// sandbox (/work/…).
fn inside(root: &Path, given: &str) -> Result<String, String> {
    let p = if Path::new(given).is_absolute() { PathBuf::from(given) } else { root.join(given) };
    // Normalize without touching the disk (the file may not exist yet).
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(x) => parts.push(x.to_owned()),
            _ => {}
        }
    }
    let norm: PathBuf = std::iter::once(PathBuf::from("/")).chain(parts.into_iter().map(PathBuf::from)).collect();
    let rel = norm.strip_prefix(root).map_err(|_| format!("{given} is outside the session's folder"))?;
    Ok(format!("/work/{}", rel.display()).trim_end_matches('/').to_owned())
}

/// Runs a command in the kit's image, in the sandbox its permissions
/// describe, with the session's folder at /work.
async fn run(ctx: &ToolCtx, rt: &str, spec: &Value, command: Vec<String>) -> ToolOutput {
    if let Err(e) = ensure_images(rt, spec).await {
        return ToolOutput::err(e);
    }
    let root = ctx.session.cwd();
    // The secrets it was approved for, for this run only.
    let env: Vec<(String, String)> = spec["permissions"]["secrets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|s| ctx.session.secrets().get(s).map(|v| (s.to_owned(), v)))
        .collect();
    let out = match sandboxed(rt, spec, &root, &[], command, &env, Some(&ctx.cancel)).await {
        Ok(o) => o,
        Err(e) => return ToolOutput::err(e),
    };
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let text = ctx.session.secrets().redact(&clip(&text));
    match out.status.code() {
        Some(0) => ToolOutput::limited(if text.trim().is_empty() { "(done, no output)".into() } else { text }, false),
        code => {
            ToolOutput::limited(format!("exit status {}\n{text}", code.map(|c| c.to_string()).unwrap_or_else(|| "(killed)".into())), true)
        }
    }
}

/// The egress proxy's image: tinyproxy, denying every host but the
/// filter's (docs/knowledge.md §6.2).
const EGRESS_DOCKERFILE: &str = "FROM alpine:3.20\nRUN apk add --no-cache tinyproxy\n";

fn egress_spec() -> Value {
    json!({"builds": [{"name": "c6-egress", "version": "1", "tag": "c6kit/egress:tinyproxy-1", "dockerfile": EGRESS_DOCKERFILE, "files": {}}]})
}

/// The label on what a kit run makes on the runtime (its container, and
/// its network and egress proxy), naming the worker it's of. A sweep takes
/// only its own worker's, and never anything without the label.
const LABEL: &str = "dev.carbon6.kit-run";

/// Whose runs this process's are (`set_owner`), and the runs alive here,
/// by id: a sweep leaves what they made.
static OWNER: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static LIVE: std::sync::Mutex<std::collections::BTreeSet<String>> = std::sync::Mutex::new(std::collections::BTreeSet::new());

/// This worker's id, for the label: other workers on the same runtime keep
/// what's theirs.
pub fn set_owner(worker: &str) {
    *OWNER.lock().unwrap() = worker.to_owned();
}

fn label() -> String {
    let o = OWNER.lock().unwrap().clone();
    format!("{LABEL}={}", if o.is_empty() { "c6" } else { &o })
}

/// What a run makes on the runtime, under its id: gone when the run ends,
/// however it ends. On the way out it's awaited (`clean`). A run dropped
/// midway (cancelled, its caller gone) removes it from Drop, on a task of
/// its own. A worker stopped midway, or a runtime restarting under a run,
/// leaves it to the next sweep. Till then its id is live, so a sweep leaves
/// it be.
struct Made {
    rt: String,
    id: String,
    /// The run's container was started (`--rm` takes it once it ends by
    /// itself; killed or cut off, it stays).
    run: bool,
    /// Its network and egress proxy (and the proxy's settings folder).
    egress: bool,
    cleaned: bool,
}

impl Made {
    fn new(rt: &str) -> Self {
        let id = uuid_like();
        LIVE.lock().unwrap().insert(id.clone());
        Made { rt: rt.to_owned(), id, run: false, egress: false, cleaned: false }
    }

    fn container(&self) -> String {
        format!("c6kit-run-{}", self.id)
    }

    fn net(&self) -> String {
        format!("c6net-{}", self.id)
    }

    fn proxy(&self) -> String {
        format!("c6egress-{}", self.id)
    }

    fn dir(&self) -> PathBuf {
        std::env::temp_dir().join(format!("c6egress-{}", self.id))
    }

    fn leftovers(&self) -> Leftovers {
        let mut containers = Vec::new();
        if self.run {
            containers.push(self.container());
        }
        if self.egress {
            containers.push(self.proxy());
        }
        Leftovers { rt: self.rt.clone(), containers, net: self.egress.then(|| self.net()), dir: self.egress.then(|| self.dir()) }
    }

    async fn clean(mut self) {
        self.leftovers().remove().await;
        self.cleaned = true;
        LIVE.lock().unwrap().remove(&self.id);
    }
}

impl Drop for Made {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        let (left, id) = (self.leftovers(), self.id.clone());
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(async move {
                    left.remove().await;
                    LIVE.lock().unwrap().remove(&id);
                });
            }
            // Nowhere to run it: the next sweep takes it.
            Err(_) => {
                LIVE.lock().unwrap().remove(&id);
            }
        }
    }
}

struct Leftovers {
    rt: String,
    containers: Vec<String>,
    net: Option<String>,
    dir: Option<PathBuf>,
}

impl Leftovers {
    /// Containers first: a network with one still on it can't go.
    async fn remove(self) {
        if !self.containers.is_empty() {
            let _ = runtime_command(&self.rt).args(["rm", "-f"]).args(&self.containers).output().await;
        }
        if let Some(n) = &self.net {
            let _ = runtime_command(&self.rt).args(["network", "rm", n]).output().await;
        }
        if let Some(d) = &self.dir {
            let _ = tokio::fs::remove_dir_all(d).await;
        }
    }
}

/// Removes what this worker's kit runs left on the runtime that no live
/// run owns (the worker stopped midway, or the runtime restarted under a
/// run): containers, then networks, only those with this worker's label.
/// How many went.
pub async fn sweep(rt: &str) -> usize {
    let filter = format!("label={}", label());
    let orphan = |name: &String| name.rsplit('-').next().is_none_or(|id| !LIVE.lock().unwrap().contains(id));
    let containers: Vec<String> =
        names(rt, &["ps", "-a", "--filter", &filter, "--format", "{{.Names}}"]).await.into_iter().filter(orphan).collect();
    let mut gone = 0;
    if !containers.is_empty() && runtime_command(rt).args(["rm", "-f"]).args(&containers).output().await.is_ok_and(|o| o.status.success()) {
        gone += containers.len();
    }
    for n in names(rt, &["network", "ls", "--filter", &filter, "--format", "{{.Name}}"]).await.into_iter().filter(orphan) {
        if runtime_command(rt).args(["network", "rm", &n]).output().await.is_ok_and(|o| o.status.success()) {
            gone += 1;
        }
    }
    gone
}

/// What a listing of the runtime's names says (none if it fails).
async fn names(rt: &str, args: &[&str]) -> Vec<String> {
    match runtime_command(rt).args(args).output().await {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()
        }
        _ => Vec::new(),
    }
}

/// The runtime's own name: Docker, Podman.
fn runtime_name(rt: &str) -> String {
    let name = Path::new(rt).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| rt.to_owned());
    let mut c = name.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or(name)
}

/// The run's internal network. Out of address ranges (networks left
/// behind take them), it sweeps and tries once more; still out, it says
/// why.
async fn make_network(rt: &str, net: &str) -> Result<(), String> {
    let create = || runtime_command(rt).args(["network", "create", "--internal", "--label", &label(), net]).output();
    let failed = |o: &std::process::Output| (!o.status.success()).then(|| String::from_utf8_lossy(&o.stderr).trim().to_owned());
    let full = |why: &str| ["non-overlapping", "address pool", "free subnet"].iter().any(|k| why.to_lowercase().contains(k));
    let mut o = create().await.map_err(|x| format!("making the run's network: {x}"))?;
    if failed(&o).is_some_and(|why| full(&why)) {
        sweep(rt).await;
        o = create().await.map_err(|x| format!("making the run's network: {x}"))?;
    }
    let Some(why) = failed(&o) else { return Ok(()) };
    if !full(&why) {
        return Err(format!("making the run's network: {why}"));
    }
    let live = LIVE.lock().unwrap().clone();
    let left = names(rt, &["network", "ls", "--filter", "name=c6net-", "--format", "{{.Name}}"])
        .await
        .into_iter()
        .filter(|n| n.rsplit('-').next().is_none_or(|id| !live.contains(id)))
        .count();
    let rt_name = runtime_name(rt);
    Err(if left > 0 {
        format!(
            "{rt_name} has no free network ranges: {left} carbon6 networks are left behind (c6net-…) that this worker can't remove (an \
             older carbon6's, or another worker's). `{} network rm $({} network ls -q --filter name=c6net-)` removes the unused ones.",
            rt_name.to_lowercase(),
            rt_name.to_lowercase()
        )
    } else {
        format!("{rt_name} has no free network ranges for the run's network: its other networks take them all ({why})")
    })
}

/// Makes the run's network to reach only the given hosts through: an
/// internal network (no route out) with a proxy on it that also sits on
/// the default bridge and lets through only those hosts. Under `made`'s
/// names, so whatever of it was made goes with the run. The proxy's
/// address, for the run's environment.
async fn egress(made: &mut Made, hosts: &[String]) -> Result<Vec<(String, String)>, String> {
    let rt = made.rt.clone();
    ensure_images(&rt, &egress_spec()).await?;
    made.egress = true;
    let (net, proxy, dir) = (made.net(), made.proxy(), made.dir());
    tokio::fs::create_dir_all(&dir).await.map_err(|x| x.to_string())?;
    let filter: String = hosts.iter().map(|h| format!("^{}$\n", h.replace('.', "\\."))).collect();
    tokio::fs::write(dir.join("filter"), filter).await.map_err(|x| x.to_string())?;
    let conf = "Port 8888\nListen 0.0.0.0\nTimeout 600\nMaxClients 64\nFilterDefaultDeny Yes\nFilter \"/etc/c6/filter\"\n\
                ConnectPort 443\nConnectPort 80\nLogLevel Warning\n";
    tokio::fs::write(dir.join("tinyproxy.conf"), conf).await.map_err(|x| x.to_string())?;
    let ok = |o: std::io::Result<std::process::Output>, what: &str| -> Result<(), String> {
        match o {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(format!("{what}: {}", String::from_utf8_lossy(&o.stderr).trim())),
            Err(x) => Err(format!("{what}: {x}")),
        }
    };
    make_network(&rt, &net).await?;
    let r = runtime_command(&rt)
        .args(["run", "-d", "--rm", "--name", &proxy, "--label", &label(), "--network", &net, "--user", "65534:65534", "--cap-drop", "ALL"])
        .args(["--security-opt", "no-new-privileges", "--read-only", "--tmpfs", "/tmp", "-v"])
        .arg(format!("{}:/etc/c6:ro", dir.display()))
        .args(["c6kit/egress:tinyproxy-1", "tinyproxy", "-d", "-c", "/etc/c6/tinyproxy.conf"])
        .output()
        .await;
    ok(r, "starting the egress proxy")?;
    ok(runtime_command(&rt).args(["network", "connect", "bridge", &proxy]).output().await, "connecting the egress proxy")?;
    // Until it listens.
    for _ in 0..50 {
        let up =
            runtime_command(&rt).args(["exec", &proxy, "nc", "-z", "127.0.0.1", "8888"]).output().await.is_ok_and(|o| o.status.success());
        if up {
            let url = format!("http://{proxy}:8888");
            return Ok(["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"].iter().map(|k| ((*k).to_owned(), url.clone())).collect());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("the egress proxy did not start".into())
}

/// Runs `command` in the kit's image (built already), in the sandbox its
/// permissions describe: `folder` at /work and nothing else of this
/// machine, no privileges, bounded processes, CPU, memory and time, and the
/// network the kit may use: none, any, or only its hosts (through an
/// egress proxy). `env` is set for this run only. What the run made on the
/// runtime is gone when it returns, or when it's dropped (`Made`).
pub async fn sandboxed(
    rt: &str,
    spec: &Value,
    folder: &Path,
    mounts: &[(PathBuf, String)],
    command: Vec<String>,
    env: &[(String, String)],
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<std::process::Output, String> {
    let mut made = Made::new(rt);
    let out = sandboxed_in(&mut made, spec, folder, mounts, command, env, cancel).await;
    made.clean().await;
    out
}

async fn sandboxed_in(
    made: &mut Made,
    spec: &Value,
    folder: &Path,
    mounts: &[(PathBuf, String)],
    command: Vec<String>,
    env: &[(String, String)],
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<std::process::Output, String> {
    let rt = made.rt.clone();
    let p = &spec["permissions"];
    let ro = p["files"] == "session-ro";
    let mut args: Vec<String> = vec!["run".into(), "--rm".into()];
    args.extend(["--name".into(), made.container(), "--label".into(), label()]);
    let hosts: Vec<String> = p["hosts"].as_array().into_iter().flatten().filter_map(|h| h.as_str().map(str::to_owned)).collect();
    let proxy = match p["network"].as_str() {
        Some("hosts") if !hosts.is_empty() => Some(egress(made, &hosts).await?),
        _ => None,
    };
    let network = match (&proxy, p["network"].as_str()) {
        (Some(_), _) => made.net(),
        (None, Some("any")) => "bridge".into(),
        _ => "none".into(),
    };
    args.extend(["--network".into(), network]);
    args.extend(["--read-only".into(), "--tmpfs".into(), "/tmp:rw,size=512m".into()]);
    args.extend(["--cap-drop".into(), "ALL".into(), "--security-opt".into(), "no-new-privileges".into()]);
    args.extend(["--pids-limit".into(), "512".into()]);
    args.extend(["--cpus".into(), format!("{}", p["cpus"].as_f64().unwrap_or(1.0))]);
    args.extend(["--memory".into(), format!("{}m", p["memory_mb"].as_u64().unwrap_or(1024))]);
    // As the owner of the folder, never root.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(m) = std::fs::metadata(folder) {
            let (uid, gid) = if m.uid() == 0 { (65534, 65534) } else { (m.uid(), m.gid()) };
            args.extend(["--user".into(), format!("{uid}:{gid}")]);
        }
    }
    args.extend(["-e".into(), "HOME=/tmp".into()]);
    for (k, v) in env.iter().chain(proxy.iter().flatten()) {
        args.extend(["-e".into(), format!("{k}={v}")]);
    }
    args.extend(["-v".into(), format!("{}:/work{}", folder.display(), if ro { ":ro" } else { "" }), "-w".into(), "/work".into()]);
    // Files of the worker's the run reads, never writes.
    for (from, at) in mounts {
        args.extend(["-v".into(), format!("{}:{at}:ro", from.display())]);
    }
    args.push(spec["image"].as_str().unwrap_or_default().to_owned());
    args.extend(command);
    let timeout = Duration::from_secs(p["timeout_s"].as_u64().unwrap_or(120));
    made.run = true;
    let child = runtime_command(&rt).args(&args).kill_on_drop(true).output();
    let never = tokio_util::sync::CancellationToken::new();
    let cancelled = cancel.unwrap_or(&never);
    tokio::select! {
        r = tokio::time::timeout(timeout, child) => match r {
            Err(_) => Err(format!("stopped after {} seconds (the kit's limit)", timeout.as_secs())),
            Ok(Err(e)) => Err(format!("running the kit: {e}")),
            Ok(Ok(o)) => Ok(o),
        },
        _ = cancelled.cancelled() => Err("interrupted".into()),
    }
}

/// Builds a kit's images if they are missing (for runs the head asks for).
pub async fn build(rt: &str, spec: &Value) -> Result<(), String> {
    ensure_images(rt, spec).await
}

fn uuid_like() -> String {
    format!("{:016x}", rand_u64())
}

fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    h.finish()
}

async fn prepare(ctx: &ToolCtx, kit: &str) -> Result<(String, Value), ToolOutput> {
    if ctx.session.is_remote() {
        return Err(ToolOutput::err("kits run on the worker's own files; this session works on another machine over SSH"));
    }
    let rt = runtime().await.ok_or_else(|| {
        ToolOutput::err("this worker has no container runtime (Docker or Podman), so kits cannot run here; ask the user to install one or use a worker that has one")
    })?;
    let spec = head_call(ctx, "session.kit_spec", json!({"kit": kit})).await.map_err(ToolOutput::err)?;
    Ok((rt, spec))
}

pub struct KitRun {
    claude: bool,
}

impl KitRun {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

#[async_trait]
impl Tool for KitRun {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "KitRun" } else { "kit_run" }.into(),
            description: "Runs a tool from a kit: software this realm's packages provide, in a sandbox (the session's folder at /work, \
                          no network unless the kit is allowed, limited CPU, memory and time). With no kit: lists the kits you may \
                          use and their tools. Path arguments are files in the session's folder; files a tool writes appear there \
                          (read an image to look at it). The first run of a kit builds it, which can take a few minutes."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "kit": {"type": "string"},
                "tool": {"type": "string"},
                "args": {"type": "object", "description": "The tool's arguments by name"}
            }, "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let kit = input["kit"].as_str().unwrap_or_default();
        if kit.is_empty() {
            return match head_call(ctx, "session.kits", json!({})).await {
                Ok(v) if v.as_array().is_some_and(Vec::is_empty) => ToolOutput::ok("No kits are installed for this realm."),
                Ok(v) => ToolOutput::limited(serde_json::to_string_pretty(&v).unwrap_or_default(), false),
                Err(e) => ToolOutput::err(e),
            };
        }
        let (rt, spec) = match prepare(ctx, kit).await {
            Ok(x) => x,
            Err(e) => return e,
        };
        let name = input["tool"].as_str().unwrap_or_default();
        let Some(tool) = spec["tools"].as_array().into_iter().flatten().find(|t| t["name"] == name) else {
            let names: Vec<&str> = spec["tools"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str()).collect();
            return ToolOutput::err(format!("{kit} has no tool {name:?} (it has: {})", names.join(", ")));
        };
        let root = ctx.session.cwd();
        let mut values: Vec<(String, String)> = Vec::new();
        for (arg, kind) in tool["args"].as_object().into_iter().flatten() {
            let Some(v) = input["args"].get(arg).map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())) else {
                return ToolOutput::err(format!("{name} needs {arg}"));
            };
            let v = if kind == "path" {
                match inside(&root, &v) {
                    Ok(p) => p,
                    Err(e) => return ToolOutput::err(e),
                }
            } else {
                v
            };
            values.push((arg.clone(), v));
        }
        let command: Vec<String> = tool["command"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|part| values.iter().fold(part.to_owned(), |acc, (k, v)| acc.replace(&format!("{{{k}}}"), v)))
            .collect();
        run(ctx, &rt, &spec, command).await
    }
}

pub struct KitShell {
    claude: bool,
}

impl KitShell {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

#[async_trait]
impl Tool for KitShell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "KitShell" } else { "kit_shell" }.into(),
            description: "Runs a shell command inside a kit whose permissions allow a shell, in the same sandbox as its tools (the \
                          session's folder at /work)."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "kit": {"type": "string"},
                "command": {"type": "string"}
            }, "required": ["kit", "command"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let (rt, spec) = match prepare(ctx, input["kit"].as_str().unwrap_or_default()).await {
            Ok(x) => x,
            Err(e) => return e,
        };
        if spec["permissions"]["shell"] != true {
            return ToolOutput::err(format!(
                "{} does not allow a shell; use its tools (kit_run)",
                spec["name"].as_str().unwrap_or("this kit")
            ));
        }
        run(ctx, &rt, &spec, vec!["sh".into(), "-c".into(), input["command"].as_str().unwrap_or_default().to_owned()]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_session() {
        let root = Path::new("/home/a/proj");
        assert_eq!(inside(root, "deck/deck.html").unwrap(), "/work/deck/deck.html");
        assert_eq!(inside(root, "/home/a/proj/x.png").unwrap(), "/work/x.png");
        assert_eq!(inside(root, ".").unwrap(), "/work");
        assert!(inside(root, "../other/x").is_err());
        assert!(inside(root, "/etc/passwd").is_err());
        assert!(inside(root, "a/../../b").is_err());
    }

    /// A stand-in for docker: what it has (networks and containers, each a
    /// file holding its label) in @STATE@, and which network each
    /// container is on (a network with one on it can't be removed). A
    /// run's command runs here, and one that hangs ends once its container
    /// is removed, as with docker. With a `limit`, it has room for that
    /// many networks.
    #[cfg(unix)]
    const FAKE: &str = r#"#!/bin/sh
S='@STATE@'
cmd="$1"; shift
case "$cmd" in
rm)
  [ "$1" = "-f" ] && shift
  for c in "$@"; do rm -f "$S/ctrs/$c" "$S/on/$c"; done ;;
ps)
  f=""
  while [ $# -gt 0 ]; do case "$1" in --filter) f="$2"; shift 2 ;; *) shift ;; esac; done
  for c in $(ls "$S/ctrs"); do [ "label=$(cat "$S/ctrs/$c")" = "$f" ] && echo "$c"; done ;;
network)
  sub="$1"; shift
  case "$sub" in
  create)
    label=""
    while [ $# -gt 1 ]; do case "$1" in --label) label="$2"; shift 2 ;; *) shift ;; esac; done
    if [ -f "$S/limit" ] && [ "$(ls "$S/nets" | wc -l | tr -d ' ')" -ge "$(cat "$S/limit")" ]; then
      echo "Error response from daemon: could not find an available, non-overlapping IPv4 address pool among the defaults to assign to the network" >&2
      exit 1
    fi
    printf '%s' "$label" > "$S/nets/$1" ;;
  rm)
    [ -f "$S/nets/$1" ] || exit 1
    for c in $(ls "$S/on"); do [ "$(cat "$S/on/$c")" = "$1" ] && { echo "network $1 has active endpoints" >&2; exit 1; }; done
    rm -f "$S/nets/$1" ;;
  ls)
    f=""
    while [ $# -gt 0 ]; do case "$1" in --filter) f="$2"; shift 2 ;; *) shift ;; esac; done
    for n in $(ls "$S/nets"); do
      case "$f" in
      name=*) case "$n" in *"${f#name=}"*) echo "$n" ;; esac ;;
      *) [ "label=$(cat "$S/nets/$n")" = "$f" ] && echo "$n" ;;
      esac
    done ;;
  esac ;;
run)
  detach=""; name=""; label=""; net=""
  while [ $# -gt 0 ]; do
    case "$1" in
    -d) detach=1; shift ;;
    --rm|--read-only) shift ;;
    --name) name="$2"; shift 2 ;;
    --label) label="$2"; shift 2 ;;
    --network) net="$2"; shift 2 ;;
    -*) shift 2 ;;
    *) break ;;
    esac
  done
  printf '%s' "$label" > "$S/ctrs/$name"
  printf '%s' "$net" > "$S/on/$name"
  [ -n "$detach" ] && exit 0
  shift
  C6_FAKE_CTR="$S/ctrs/$name" "$@"
  code=$?
  rm -f "$S/ctrs/$name" "$S/on/$name"
  exit $code ;;
esac
exit 0
"#;

    #[cfg(unix)]
    struct Fake {
        rt: String,
        state: PathBuf,
        work: PathBuf,
        _dir: tempfile::TempDir,
    }

    #[cfg(unix)]
    impl Fake {
        async fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let (state, work) = (dir.path().join("state"), dir.path().join("work"));
            for d in [state.join("nets"), state.join("ctrs"), state.join("on"), work.clone()] {
                std::fs::create_dir_all(d).unwrap();
            }
            let rt = dir.path().join("docker");
            std::fs::write(&rt, FAKE.replace("@STATE@", &state.display().to_string())).unwrap();
            std::fs::set_permissions(&rt, std::fs::Permissions::from_mode(0o755)).unwrap();
            // Till then a sweep could take "Text file busy" for nothing listed.
            c6_testkit::runnable(&rt, &["version"]).await;
            Fake { rt: rt.display().to_string(), state, work, _dir: dir }
        }

        /// Its networks and containers, by name, with their labels.
        fn has(&self) -> Vec<(String, String)> {
            let mut out = Vec::new();
            for kind in ["nets", "ctrs"] {
                for e in std::fs::read_dir(self.state.join(kind)).unwrap().flatten() {
                    out.push((e.file_name().to_string_lossy().into_owned(), std::fs::read_to_string(e.path()).unwrap()));
                }
            }
            out.sort();
            out
        }

        fn names(&self) -> Vec<String> {
            self.has().into_iter().map(|(n, _)| n).collect()
        }

        fn put(&self, kind: &str, name: &str, label: &str) {
            std::fs::write(self.state.join(kind).join(name), label).unwrap();
        }

        /// A run of a kit that may reach example.com (through its egress
        /// proxy), stopped after `timeout` seconds.
        async fn run(
            &self,
            command: &str,
            timeout: u64,
            cancel: Option<&tokio_util::sync::CancellationToken>,
        ) -> Result<std::process::Output, String> {
            let spec = json!({"image": "kit", "permissions": {"network": "hosts", "hosts": ["example.com"], "timeout_s": timeout}});
            sandboxed(&self.rt, &spec, &self.work, &[], vec!["sh".into(), "-c".into(), command.into()], &[], cancel).await
        }
    }

    /// A run's command that runs until its container is removed.
    #[cfg(unix)]
    const HANG: &str = r#"while [ -f "$C6_FAKE_CTR" ]; do sleep 0.05; done"#;
    #[cfg(unix)]
    const MINE: &str = "dev.carbon6.kit-run=c6";

    /// A run removes its container, network and egress proxy however it
    /// ends: its command failing, cancelled, past its time limit, or
    /// dropped midway. Meanwhile they carry the worker's label.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_run_leaves_nothing_behind_however_it_ends() {
        let f = Fake::new().await;
        let out = f.run("exit 3", 600, None).await.unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(f.has(), [], "its command failed");

        let cancel = tokio_util::sync::CancellationToken::new();
        let watch = async {
            let made = c6_testkit::eventually("the run going", async || (f.has().len() == 3).then(|| f.has())).await;
            for (name, label) in &made {
                assert!(["c6net-", "c6egress-", "c6kit-run-"].iter().any(|p| name.starts_with(p)) && label == MINE, "{made:?}");
            }
            cancel.cancel();
        };
        let (out, ()) = tokio::join!(f.run(HANG, 600, Some(&cancel)), watch);
        assert_eq!(out.unwrap_err(), "interrupted");
        assert_eq!(f.has(), [], "cancelled");

        let out = f.run(HANG, 1, None).await;
        assert!(out.unwrap_err().starts_with("stopped after 1 seconds"));
        assert_eq!(f.has(), [], "past its time limit");

        let job = tokio::spawn({
            let f = Fake { rt: f.rt.clone(), state: f.state.clone(), work: f.work.clone(), _dir: tempfile::tempdir().unwrap() };
            async move {
                let _ = f.run(HANG, 600, None).await;
            }
        });
        c6_testkit::eventually("the run going", async || (f.has().len() == 3).then_some(())).await;
        job.abort();
        c6_testkit::eventually("what the dropped run made removed", async || f.has().is_empty().then_some(())).await;
    }

    /// A sweep removes what this worker's runs left and no live run owns,
    /// containers and networks; nothing without its label (an older
    /// carbon6's, someone else's), another worker's, or a live run's.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_sweep_takes_only_this_workers_leftovers() {
        let f = Fake::new().await;
        let live = Made::new(&f.rt);
        let (net, proxy) = (format!("c6net-{}", live.id), format!("c6egress-{}", live.id));
        for (kind, name, label) in [
            ("nets", "c6net-00000000000000a1", MINE),
            ("ctrs", "c6egress-00000000000000a1", MINE),
            ("ctrs", "c6kit-run-00000000000000a1", MINE),
            ("nets", &net, MINE),
            ("ctrs", &proxy, MINE),
            ("nets", "c6net-00000000000000b2", ""),
            ("ctrs", "c6egress-00000000000000b2", ""),
            ("nets", "c6net-00000000000000c3", "dev.carbon6.kit-run=another-worker"),
            ("nets", "someones-network", ""),
        ] {
            f.put(kind, name, label);
        }
        assert_eq!(sweep(&f.rt).await, 3);
        let mut kept =
            vec![net, proxy, "c6net-00000000000000b2".into(), "c6egress-00000000000000b2".into(), "c6net-00000000000000c3".into()];
        kept.push("someones-network".into());
        kept.sort();
        assert_eq!(f.names(), kept);
        live.clean().await;
    }

    /// Out of network ranges, a run sweeps and tries again; still out
    /// (networks it may not remove), it says why.
    #[cfg(unix)]
    #[tokio::test]
    async fn out_of_network_ranges_a_run_sweeps_then_says_why() {
        let f = Fake::new().await;
        std::fs::write(f.state.join("limit"), "2").unwrap();
        f.put("nets", "c6net-00000000000000a1", MINE);
        f.put("nets", "c6net-00000000000000a2", MINE);
        let out = f.run("exit 0", 600, None).await.unwrap();
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(f.has(), [], "what a stopped worker left went, and so did the run's");

        f.put("nets", "c6net-00000000000000b1", "");
        f.put("nets", "c6net-00000000000000b2", "");
        let e = f.run("exit 0", 600, None).await.unwrap_err();
        assert!(e.starts_with("Docker has no free network ranges: 2 carbon6 networks are left behind"), "{e}");
        assert_eq!(f.names(), ["c6net-00000000000000b1", "c6net-00000000000000b2"]);
    }
}

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

/// A network a run reaches only the given hosts through: an internal
/// network (no route out) with a proxy on it that also sits on the default
/// bridge and lets through only those hosts.
struct Egress {
    rt: String,
    net: String,
    proxy: String,
    dir: PathBuf,
}

impl Egress {
    async fn start(rt: &str, hosts: &[String]) -> Result<Self, String> {
        ensure_images(rt, &egress_spec()).await?;
        let id = uuid_like();
        let e = Egress {
            rt: rt.to_owned(),
            net: format!("c6net-{id}"),
            proxy: format!("c6egress-{id}"),
            dir: std::env::temp_dir().join(format!("c6egress-{id}")),
        };
        tokio::fs::create_dir_all(&e.dir).await.map_err(|x| x.to_string())?;
        let filter: String = hosts.iter().map(|h| format!("^{}$\n", h.replace('.', "\\."))).collect();
        tokio::fs::write(e.dir.join("filter"), filter).await.map_err(|x| x.to_string())?;
        let conf = "Port 8888\nListen 0.0.0.0\nTimeout 600\nMaxClients 64\nFilterDefaultDeny Yes\nFilter \"/etc/c6/filter\"\n\
                    ConnectPort 443\nConnectPort 80\nLogLevel Warning\n";
        tokio::fs::write(e.dir.join("tinyproxy.conf"), conf).await.map_err(|x| x.to_string())?;
        let ok = |o: std::io::Result<std::process::Output>, what: &str| -> Result<(), String> {
            match o {
                Ok(o) if o.status.success() => Ok(()),
                Ok(o) => Err(format!("{what}: {}", String::from_utf8_lossy(&o.stderr).trim())),
                Err(x) => Err(format!("{what}: {x}")),
            }
        };
        ok(runtime_command(rt).args(["network", "create", "--internal", &e.net]).output().await, "making the run's network")?;
        let r = runtime_command(rt)
            .args(["run", "-d", "--rm", "--name", &e.proxy, "--network", &e.net, "--user", "65534:65534", "--cap-drop", "ALL"])
            .args(["--security-opt", "no-new-privileges", "--read-only", "--tmpfs", "/tmp", "-v"])
            .arg(format!("{}:/etc/c6:ro", e.dir.display()))
            .args(["c6kit/egress:tinyproxy-1", "tinyproxy", "-d", "-c", "/etc/c6/tinyproxy.conf"])
            .output()
            .await;
        if let Err(x) = ok(r, "starting the egress proxy") {
            e.stop().await;
            return Err(x);
        }
        if let Err(x) =
            ok(runtime_command(rt).args(["network", "connect", "bridge", &e.proxy]).output().await, "connecting the egress proxy")
        {
            e.stop().await;
            return Err(x);
        }
        // Until it listens.
        for _ in 0..50 {
            let up = runtime_command(rt)
                .args(["exec", &e.proxy, "nc", "-z", "127.0.0.1", "8888"])
                .output()
                .await
                .is_ok_and(|o| o.status.success());
            if up {
                return Ok(e);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        e.stop().await;
        Err("the egress proxy did not start".into())
    }

    fn env(&self) -> Vec<(String, String)> {
        let url = format!("http://{}:8888", self.proxy);
        ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"].iter().map(|k| ((*k).to_owned(), url.clone())).collect()
    }

    async fn stop(&self) {
        let _ = runtime_command(&self.rt).args(["rm", "-f", &self.proxy]).output().await;
        let _ = runtime_command(&self.rt).args(["network", "rm", &self.net]).output().await;
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
    }
}

/// Runs `command` in the kit's image (built already), in the sandbox its
/// permissions describe: `folder` at /work and nothing else of this
/// machine, no privileges, bounded processes, CPU, memory and time, and the
/// network the kit may use: none, any, or only its hosts (through an
/// egress proxy). `env` is set for this run only.
pub async fn sandboxed(
    rt: &str,
    spec: &Value,
    folder: &Path,
    mounts: &[(PathBuf, String)],
    command: Vec<String>,
    env: &[(String, String)],
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<std::process::Output, String> {
    let p = &spec["permissions"];
    let ro = p["files"] == "session-ro";
    let mut args: Vec<String> = vec!["run".into(), "--rm".into()];
    let name = format!("c6kit-run-{}", uuid_like());
    args.extend(["--name".into(), name.clone()]);
    let hosts: Vec<String> = p["hosts"].as_array().into_iter().flatten().filter_map(|h| h.as_str().map(str::to_owned)).collect();
    let egress = match p["network"].as_str() {
        Some("hosts") if !hosts.is_empty() => Some(Egress::start(rt, &hosts).await?),
        _ => None,
    };
    let network = match (&egress, p["network"].as_str()) {
        (Some(e), _) => e.net.clone(),
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
    for (k, v) in env.iter().chain(egress.as_ref().map(|e| e.env()).unwrap_or_default().iter()) {
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
    let child = runtime_command(rt).args(&args).kill_on_drop(true).output();
    let never = tokio_util::sync::CancellationToken::new();
    let cancelled = cancel.unwrap_or(&never);
    let out = tokio::select! {
        r = tokio::time::timeout(timeout, child) => r,
        _ = cancelled.cancelled() => {
            let _ = runtime_command(rt).args(["kill", &name]).output().await;
            if let Some(e) = &egress { e.stop().await; }
            return Err("interrupted".into());
        }
    };
    if let Some(e) = &egress {
        e.stop().await;
    }
    match out {
        Err(_) => {
            let _ = runtime_command(rt).args(["kill", &name]).output().await;
            Err(format!("stopped after {} seconds (the kit's limit)", timeout.as_secs()))
        }
        Ok(Err(e)) => Err(format!("running the kit: {e}")),
        Ok(Ok(o)) => Ok(o),
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
}

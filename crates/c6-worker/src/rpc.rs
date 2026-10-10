//! Head → worker requests: file browsing, git, terminals.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::Worker;

/// Largest file `fs.read` returns.
const MAX_READ: u64 = 2 * 1024 * 1024;
const MAX_LIST: usize = 5000;
const MAX_DIFF: usize = 4 * 1024 * 1024;

pub async fn handle(worker: &Arc<Worker>, method: &str, params: Value) -> anyhow::Result<Value> {
    match method {
        "fs.list" => fs_list(serde_json::from_value(params)?).await,
        "fs.read" => fs_read(serde_json::from_value(params)?).await,
        "git.status" => git_status(&path_param(&params)?).await,
        "git.diff" => git_diff(&path_param(&params)?, params["file"].as_str()).await,
        "git.log" => git_log(&path_param(&params)?).await,
        "pty.open" => Ok(serde_json::to_value(worker.terminals.open(serde_json::from_value(params)?)?)?),
        "pty.list" => Ok(serde_json::to_value(worker.terminals.list())?),
        "pty.close" => Ok(json!({ "closed": worker.terminals.close(params["terminal_id"].as_str().unwrap_or_default()) })),
        "pty.attach" => {
            let id = params["terminal_id"].as_str().context("terminal_id required")?.to_owned();
            let token = params["token"].as_str().context("token required")?.to_owned();
            let channel = worker.channel().ok_or_else(|| anyhow!("not connected"))?;
            let w = worker.clone();
            // The stream lives as long as the viewer; reply right away.
            tokio::spawn(async move {
                if let Err(e) = w.terminals.attach(channel, &id, token).await {
                    tracing::warn!(terminal = %id, "terminal attach ended: {e:#}");
                }
            });
            Ok(json!({}))
        }
        _ => bail!("unknown method {method}"),
    }
}

fn path_param(params: &Value) -> anyhow::Result<PathBuf> {
    let p = params["path"].as_str().filter(|p| !p.is_empty()).context("path required")?;
    Ok(crate::expand_home(p))
}

#[derive(Deserialize)]
struct ListParams {
    path: Option<String>,
    #[serde(default)]
    hidden: bool,
}

async fn fs_list(p: ListParams) -> anyhow::Result<Value> {
    let dir = p.path.filter(|p| !p.is_empty()).map(|p| crate::expand_home(&p)).or_else(dirs::home_dir).context("no path")?;
    let dir = tokio::fs::canonicalize(&dir).await.with_context(|| format!("{} not found", dir.display()))?;
    let mut rd = tokio::fs::read_dir(&dir).await.with_context(|| format!("cannot list {}", dir.display()))?;
    let mut entries = Vec::new();
    while let Some(e) = rd.next_entry().await? {
        let name = e.file_name().to_string_lossy().into_owned();
        if !p.hidden && name.starts_with('.') {
            continue;
        }
        let meta = match tokio::fs::metadata(e.path()).await {
            Ok(m) => m,
            Err(_) => e.metadata().await?, // dangling symlink
        };
        let kind = if meta.is_dir() { "dir" } else { "file" };
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64);
        entries.push(json!({"name": name, "kind": kind, "size": meta.len(), "mtime": mtime}));
        if entries.len() >= MAX_LIST {
            break;
        }
    }
    entries.sort_by(|a, b| (a["kind"] != "dir", a["name"].as_str()).cmp(&(b["kind"] != "dir", b["name"].as_str())));
    let git_root = dir.ancestors().find(|d| d.join(".git").exists()).map(|d| d.display().to_string());
    Ok(json!({"path": dir.display().to_string(), "entries": entries, "git_root": git_root}))
}

#[derive(Deserialize)]
struct ReadParams {
    path: String,
}

async fn fs_read(p: ReadParams) -> anyhow::Result<Value> {
    let path = crate::expand_home(&p.path);
    let meta = tokio::fs::metadata(&path).await.with_context(|| format!("{} not found", path.display()))?;
    if meta.is_dir() {
        bail!("{} is a directory", path.display());
    }
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(&path).await?;
    let mut buf = Vec::with_capacity(meta.len().min(MAX_READ) as usize);
    (&mut f).take(MAX_READ).read_to_end(&mut buf).await?;
    let binary = buf[..buf.len().min(8192)].contains(&0);
    let content = if binary { Value::Null } else { Value::String(String::from_utf8_lossy(&buf).into_owned()) };
    Ok(
        json!({"path": path.display().to_string(), "size": meta.len(), "truncated": meta.len() > MAX_READ, "binary": binary, "content": content}),
    )
}

async fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0") // never take the index lock for reads
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .context("running git")?;
    if !out.status.success() {
        bail!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `git status --porcelain=v2 --branch`, parsed.
async fn git_status(dir: &Path) -> anyhow::Result<Value> {
    let out = git(dir, &["status", "--porcelain=v2", "--branch", "--untracked-files=all"]).await?;
    let root = git(dir, &["rev-parse", "--show-toplevel"]).await?.trim().to_owned();
    let (mut branch, mut upstream, mut ahead, mut behind) = (None, None, 0i64, 0i64);
    let mut files = Vec::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            branch = Some(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix("# branch.upstream ") {
            upstream = Some(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            let mut it = rest.split_whitespace();
            ahead = it.next().and_then(|a| a.trim_start_matches('+').parse().ok()).unwrap_or(0);
            behind = it.next().and_then(|b| b.trim_start_matches('-').parse().ok()).unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("1 ") {
            // XY sub mH mI mW hH hI path
            let fields: Vec<&str> = rest.splitn(8, ' ').collect();
            if let (Some(xy), Some(path)) = (fields.first(), fields.get(7)) {
                files.push(json!({"path": path, "index": &xy[..1], "worktree": &xy[1..2]}));
            }
        } else if let Some(rest) = line.strip_prefix("2 ") {
            // XY sub mH mI mW hH hI Rscore path\torig
            let fields: Vec<&str> = rest.splitn(9, ' ').collect();
            if let (Some(xy), Some(paths)) = (fields.first(), fields.get(8)) {
                let (path, orig) = paths.split_once('\t').unwrap_or((paths, ""));
                files.push(json!({"path": path, "from": orig, "index": &xy[..1], "worktree": &xy[1..2]}));
            }
        } else if let Some(path) = line.strip_prefix("? ") {
            files.push(json!({"path": path, "index": "?", "worktree": "?"}));
        } else if let Some(rest) = line.strip_prefix("u ") {
            let path = rest.rsplit(' ').next().unwrap_or(rest);
            files.push(json!({"path": path, "index": "U", "worktree": "U"}));
        }
    }
    Ok(json!({"root": root, "branch": branch, "upstream": upstream, "ahead": ahead, "behind": behind, "files": files}))
}

/// Unified diff of the working tree against HEAD, including untracked files.
async fn git_diff(dir: &Path, file: Option<&str>) -> anyhow::Result<Value> {
    let has_head = git(dir, &["rev-parse", "--verify", "-q", "HEAD"]).await.is_ok();
    let mut args = vec!["diff", "--no-color", "--no-ext-diff", "-M"];
    if has_head {
        args.push("HEAD");
    }
    args.push("--");
    if let Some(f) = file {
        args.push(f);
    }
    let mut diff = git(dir, &args).await?;
    // Untracked files show up as additions.
    let mut untracked_args = vec!["ls-files", "--others", "--exclude-standard", "--"];
    if let Some(f) = file {
        untracked_args.push(f);
    }
    for path in git(dir, &untracked_args).await?.lines().take(200) {
        let out =
            Command::new("git").arg("-C").arg(dir).args(["diff", "--no-color", "--no-index", "--", "/dev/null", path]).output().await?;
        diff.push_str(&String::from_utf8_lossy(&out.stdout));
        if diff.len() > MAX_DIFF {
            break;
        }
    }
    let truncated = diff.len() > MAX_DIFF;
    if truncated {
        let mut end = MAX_DIFF;
        while !diff.is_char_boundary(end) {
            end -= 1;
        }
        diff.truncate(end);
    }
    Ok(json!({"diff": diff, "truncated": truncated}))
}

async fn git_log(dir: &Path) -> anyhow::Result<Value> {
    let out = git(dir, &["log", "-n", "50", "--pretty=format:%H%x1f%h%x1f%an%x1f%at%x1f%s"]).await?;
    let commits: Vec<Value> = out
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\x1f').collect();
            (f.len() == 5).then(
                || json!({"hash": f[0], "short": f[1], "author": f[2], "time": f[3].parse::<i64>().unwrap_or(0) * 1000, "subject": f[4]}),
            )
        })
        .collect();
    Ok(json!({"commits": commits}))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn sh(dir: &Path, cmd: &str) {
        let ok = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).status().await.unwrap().success();
        assert!(ok, "{cmd}");
    }

    #[tokio::test]
    async fn status_and_diff_include_untracked_and_renames() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        sh(d, "git init -q -b main && git config user.email t@t && git config user.name t && printf 'a\\n' > a.txt && printf 'keep\\n' > old.txt && git add . && git commit -qm init").await;
        sh(d, "printf 'a\\nb\\n' > a.txt && git mv old.txt new.txt && printf 'n\\n' > untracked.txt").await;

        let st = git_status(d).await.unwrap();
        assert_eq!(st["branch"], "main");
        let files: Vec<(String, String)> = st["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (f["path"].as_str().unwrap().to_owned(), format!("{}{}", f["index"].as_str().unwrap(), f["worktree"].as_str().unwrap()))
            })
            .collect();
        assert!(files.contains(&("a.txt".into(), ".M".into())), "{files:?}");
        assert!(files.contains(&("new.txt".into(), "R.".into())), "{files:?}");
        assert!(files.contains(&("untracked.txt".into(), "??".into())), "{files:?}");

        let diff = git_diff(d, None).await.unwrap()["diff"].as_str().unwrap().to_owned();
        assert!(diff.contains("+b"));
        assert!(diff.contains("rename to new.txt"));
        assert!(diff.contains("+++ b/untracked.txt"));

        let listing = fs_list(ListParams { path: Some(d.display().to_string()), hidden: false }).await.unwrap();
        let names: Vec<&str> = listing["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["a.txt", "new.txt", "untracked.txt"]);
        assert!(listing["git_root"].as_str().is_some());
    }
}

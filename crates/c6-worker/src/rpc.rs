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
        "fs.mkdir" => fs_mkdir(&path_param(&params)?).await,
        "fs.find" => fs_find(serde_json::from_value(params)?).await,
        // Outside a repository, git methods answer `{"repo": false}` rather
        // than failing, so the UI can say so plainly.
        "git.status" | "git.diff" | "git.log" => {
            let dir = path_param(&params)?;
            if !in_repo(&dir).await? {
                return Ok(json!({"repo": false}));
            }
            match method {
                "git.status" => git_status(&dir).await,
                "git.diff" => git_diff(&dir, params["file"].as_str()).await,
                _ => git_log(&dir).await,
            }
        }
        "ssh.check" => ssh_check(params["target"].as_str().unwrap_or_default()).await,
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
        _ => match worker.extension() {
            Some(ext) => match ext.handle(method, params).await {
                Some(r) => r,
                None => bail!("unknown method {method}"),
            },
            None => bail!("unknown method {method}"),
        },
    }
}

fn path_param(params: &Value) -> anyhow::Result<PathBuf> {
    let p = params["path"].as_str().filter(|p| !p.is_empty()).context("path required")?;
    Ok(crate::expand_home(p))
}

#[derive(Deserialize)]
struct FindParams {
    root: String,
    #[serde(default)]
    q: String,
}

/// Entries walked per search, and results returned.
const FIND_WALK: usize = 50_000;
const FIND_RESULTS: usize = 30;

/// Files and folders under `root` matching `q` (characters in order, case
/// ignored), best first: for @-mentions. Respects .gitignore; skips hidden
/// entries.
async fn fs_find(p: FindParams) -> anyhow::Result<Value> {
    let root = crate::expand_home(&p.root);
    let q = p.q.to_lowercase();
    tokio::task::spawn_blocking(move || {
        let mut hits: Vec<(i64, String, bool)> = Vec::new();
        for entry in ignore::WalkBuilder::new(&root).max_depth(Some(16)).build().filter_map(Result::ok).skip(1).take(FIND_WALK) {
            let Ok(rel) = entry.path().strip_prefix(&root) else { continue };
            let rel = rel.to_string_lossy().replace('\\', "/");
            let dir = entry.file_type().is_some_and(|t| t.is_dir());
            if let Some(score) = fuzzy_score(&q, &rel) {
                hits.push((score, rel, dir));
            }
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())).then(a.1.cmp(&b.1)));
        let paths: Vec<Value> = hits.into_iter().take(FIND_RESULTS).map(|(_, path, dir)| json!({"path": path, "dir": dir})).collect();
        json!({"paths": paths})
    })
    .await
    .context("searching files")
}

/// How well `q` matches `path` (None: not at all): its characters must
/// appear in order; matches in the file name, at word starts and in a row
/// score higher, long paths lower.
fn fuzzy_score(q: &str, path: &str) -> Option<i64> {
    if q.is_empty() {
        return Some(-(path.len() as i64));
    }
    let lower = path.to_lowercase();
    let name_at = lower.rfind('/').map_or(0, |i| i + 1);
    let name = &lower[name_at..];
    if name.starts_with(q) {
        return Some(10_000 - path.len() as i64);
    }
    if name.contains(q) {
        return Some(8_000 - path.len() as i64);
    }
    if lower.contains(q) {
        return Some(6_000 - path.len() as i64);
    }
    let (chars, mut score, mut last) = (lower.as_bytes(), 0i64, None::<usize>);
    let mut from = 0;
    for qc in q.bytes() {
        let i = (from..chars.len()).find(|&i| chars[i] == qc)?;
        score += match last {
            Some(l) if l + 1 == i => 15,
            _ => 0,
        };
        if i == 0 || matches!(chars[i - 1], b'/' | b'_' | b'-' | b'.') {
            score += 10;
        }
        if i >= name_at {
            score += 5;
        }
        last = Some(i);
        from = i + 1;
    }
    Some(score * 10 - path.len() as i64)
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

/// Creates one new directory (its parent must exist).
async fn fs_mkdir(path: &Path) -> anyhow::Result<Value> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if name.is_empty() || name == "." || name == ".." {
        bail!("invalid folder name");
    }
    match tokio::fs::create_dir(path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!("{} already exists", path.display()),
        Err(e) => return Err(e).with_context(|| format!("cannot create {}", path.display())),
    }
    let path = tokio::fs::canonicalize(path).await?;
    Ok(json!({"path": path.display().to_string()}))
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

/// Whether `dir` is inside a git work tree. Errors only if `dir` is missing
/// or git cannot run.
async fn in_repo(dir: &Path) -> anyhow::Result<bool> {
    if !tokio::fs::metadata(dir).await.map(|m| m.is_dir()).unwrap_or(false) {
        bail!("{} is not a directory", dir.display());
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .await
        .context("running git (is it installed on this worker?)")?;
    Ok(out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true")
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

        let made = fs_mkdir(&d.join("newdir")).await.unwrap();
        assert!(made["path"].as_str().unwrap().ends_with("/newdir"));
        assert!(fs_mkdir(&d.join("newdir")).await.unwrap_err().to_string().contains("already exists"));
        std::fs::remove_dir(d.join("newdir")).unwrap();

        let plain = tempfile::tempdir().unwrap();
        assert!(!in_repo(plain.path()).await.unwrap());
        assert!(in_repo(d).await.unwrap());

        let listing = fs_list(ListParams { path: Some(d.display().to_string()), hidden: false }).await.unwrap();
        let names: Vec<&str> = listing["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["a.txt", "new.txt", "untracked.txt"]);
        assert!(listing["git_root"].as_str().is_some());
    }
}

#[cfg(test)]
mod find_tests {
    use super::fuzzy_score;

    #[test]
    fn file_names_first() {
        let mut paths = ["src/main.rs", "docs/maintenance.md", "crates/x/src/domain.rs", "README.md"];
        paths.sort_by_key(|p| std::cmp::Reverse(fuzzy_score("main", p).unwrap_or(i64::MIN)));
        assert_eq!(&paths[..2], ["src/main.rs", "docs/maintenance.md"]);
        assert!(fuzzy_score("srmn", "src/main.rs").is_some(), "characters in order");
        assert!(fuzzy_score("zz", "src/main.rs").is_none());
    }
}

/// Whether an SSH target answers: {ok, system} or {ok: false, error}.
async fn ssh_check(target: &str) -> anyhow::Result<Value> {
    let target = target.trim();
    if target.is_empty() || target.starts_with('-') || target.contains(char::is_whitespace) {
        bail!("an SSH target is user@host or a host alias");
    }
    let host = c6_tools::remote::SshHost::new(target, "");
    Ok(match tokio::time::timeout(std::time::Duration::from_secs(20), host.probe("~")).await {
        Ok(Ok((_, _, system))) => json!({"ok": true, "system": system}),
        Ok(Err(e)) => json!({"ok": false, "error": e.trim()}),
        Err(_) => json!({"ok": false, "error": format!("no answer from {target} in 20 seconds")}),
    })
}

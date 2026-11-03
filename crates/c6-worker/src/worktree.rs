//! A session's git worktree (docs/dev-starts.md): made as its template says
//! (branch, base, fetch, tracking, setup commands), and removed when its
//! work ends, only when nothing would be lost.

use std::path::{Path, PathBuf};
use std::time::Duration;

use c6_api::starts::{WorktreeEnd, WorktreeEnded};
use c6_proto::WorktreeSpec;
use tracing::info;

/// How long one setup command may run.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// The most of a setup command's output kept for the transcript.
const OUTPUT_CHARS: usize = 4000;

/// A worktree made for a session.
#[derive(Debug)]
pub struct Made {
    /// The session's folder: the same place in the worktree as `cwd` in its
    /// repository.
    pub cwd: PathBuf,
    /// The worktree's own top folder.
    pub top: PathBuf,
    pub branch: String,
    pub root: String,
    /// The branch was made for it (not checked out again).
    pub new_branch: bool,
}

/// git in `dir`: its stdout, or its stderr as the error.
async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new("git").arg("-C").arg(dir).args(args).output().await.map_err(|e| format!("running git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(format!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// The last 8 hex digits of a session id.
pub fn short_id(session_id: &str) -> String {
    let mut v: Vec<char> = session_id.chars().filter(|c| c.is_ascii_hexdigit()).rev().take(8).collect();
    v.reverse();
    v.into_iter().collect()
}

/// Makes a worktree of the repository holding `cwd`, under `worktrees`.
/// A branch that exists already is checked out again; one pushed to a
/// remote (and not here) starts from there; else a new branch from the
/// base.
pub async fn create(worktrees: &Path, session_id: &str, cwd: &Path, spec: &WorktreeSpec) -> Result<Made, String> {
    let root = cwd
        .ancestors()
        .find(|d| d.join(".git").exists())
        .ok_or_else(|| format!("{} is not inside a git repository, so a worktree cannot be created", cwd.display()))?
        .to_owned();
    let repo = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    let short = short_id(session_id);
    let pattern = if spec.branch.trim().is_empty() { c6_api::starts::DEFAULT_BRANCH } else { spec.branch.trim() };
    let branch = pattern.replace("{{id}}", &short);
    git(&root, &["check-ref-format", "--branch", &branch]).await.map_err(|_| format!("{branch:?} is not a valid git branch name"))?;
    let base = if spec.base.trim().is_empty() { "HEAD".to_owned() } else { spec.base.trim().to_owned() };
    if spec.fetch {
        // The base's remote (origin of origin/main), else every remote.
        let remotes = git(&root, &["remote"]).await?;
        let remote = base.split_once('/').map(|(r, _)| r).filter(|r| remotes.lines().any(|l| l == *r));
        match remote {
            Some(r) => git(&root, &["fetch", "--quiet", r]).await?,
            None => git(&root, &["fetch", "--quiet", "--all"]).await?,
        };
    }
    let path = worktrees.join(&repo).join(&short);
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| format!("creating {}: {e}", worktrees.display()))?;
    let path_s = path.display().to_string();
    let local = git(&root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).await.is_ok();
    let track = if spec.track { "--track" } else { "--no-track" };
    if local {
        git(&root, &["worktree", "add", &path_s, &branch]).await?;
    } else {
        let pushed = git(&root, &["for-each-ref", "--format=%(refname:short)", &format!("refs/remotes/*/{branch}")]).await?;
        let from = pushed.lines().next().map(str::to_owned).unwrap_or(base);
        git(&root, &["worktree", "add", "-b", &branch, track, &path_s, &from]).await?;
    }
    let rel = cwd.strip_prefix(&root).unwrap_or(Path::new(""));
    info!(session = %session_id, %branch, path = %path.display(), "worktree created");
    Ok(Made { cwd: path.join(rel), top: path, branch, root: root.display().to_string(), new_branch: !local })
}

/// Runs the setup commands in the new worktree, one after another. Returns
/// what they printed; the first that fails is the error, with its output.
pub async fn setup(made: &Made, commands: &[String]) -> Result<String, String> {
    let mut log = String::new();
    for line in commands.iter().map(|c| c.trim()).filter(|c| !c.is_empty()) {
        let run = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(line)
            .current_dir(&made.top)
            .env("C6_WORKTREE", &made.top)
            .env("C6_BRANCH", &made.branch)
            .env("C6_REPO", &made.root)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = match tokio::time::timeout(SETUP_TIMEOUT, run).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(format!("setup `{line}` could not run: {e}")),
            Err(_) => return Err(format!("setup `{line}` took longer than {} minutes", SETUP_TIMEOUT.as_secs() / 60)),
        };
        let mut said = String::from_utf8_lossy(&out.stdout).into_owned();
        said.push_str(&String::from_utf8_lossy(&out.stderr));
        let said = tail(said.trim());
        if !out.status.success() {
            let code = out.status.code().map_or("a signal".to_owned(), |c| format!("exit {c}"));
            return Err(format!("setup `{line}` failed ({code}){}", if said.is_empty() { String::new() } else { format!(":\n{said}") }));
        }
        log.push_str(&format!("$ {line}\n"));
        if !said.is_empty() {
            log.push_str(&said);
            log.push('\n');
        }
    }
    Ok(log)
}

/// Takes away a worktree whose setup failed, and the branch if it was new.
pub async fn discard(made: &Made) {
    let root = Path::new(&made.root);
    let _ = git(root, &["worktree", "remove", "--force", &made.top.display().to_string()]).await;
    if made.new_branch {
        let _ = git(root, &["branch", "-D", &made.branch]).await;
    }
}

fn tail(s: &str) -> String {
    let n = s.chars().count();
    if n <= OUTPUT_CHARS { s.to_owned() } else { format!("…{}", s.chars().skip(n - OUTPUT_CHARS).collect::<String>()) }
}

/// What would be lost by removing the worktree at `top` and its branch:
/// uncommitted changes, and commits on no remote and in no other branch.
pub async fn at_risk(top: &Path, branch: &str) -> Result<Vec<String>, String> {
    let mut kept = Vec::new();
    let status = git(top, &["status", "--porcelain"]).await?;
    let changes = status.lines().filter(|l| !l.trim().is_empty()).count();
    if changes > 0 {
        kept.push(format!("{changes} uncommitted change{} in {}", if changes == 1 { "" } else { "s" }, top.display()));
    }
    let exclude = format!("--exclude={branch}");
    let only = git(top, &["rev-list", "--count", branch, "--not", &exclude, "--branches", "--remotes"]).await?;
    let only: usize = only.parse().unwrap_or(0);
    if only > 0 {
        kept.push(format!(
            "{only} commit{} on {branch} {} on no remote and in no other branch",
            if only == 1 { "" } else { "s" },
            if only == 1 { "is" } else { "are" }
        ));
    }
    Ok(kept)
}

/// The work ended (`worktree.end`): removes the worktree and its branch
/// when asked and nothing would be lost, then runs the WorkEnded hooks.
pub async fn end(req: &WorktreeEnd, labels: &[String]) -> WorktreeEnded {
    let mut out = WorktreeEnded::default();
    let path = PathBuf::from(&req.path);
    let top = match git(&path, &["rev-parse", "--show-toplevel"]).await {
        Ok(t) => Some(PathBuf::from(t)),
        Err(_) => None,
    };
    let root = if !req.repo_root.is_empty() { PathBuf::from(&req.repo_root) } else { top.clone().unwrap_or_else(|| path.clone()) };
    match &top {
        None => out.kept.push(format!("{} is gone already", req.path)),
        Some(top) if req.remove => match at_risk(top, &req.branch).await {
            Ok(risks) if risks.is_empty() => {
                // Clean (submodules included), so --force only gets past a
                // worktree with submodules, which git won't remove otherwise.
                match git(&root, &["worktree", "remove", "--force", &top.display().to_string()]).await {
                    Ok(_) => {
                        out.removed = true;
                        if let Err(e) = git(&root, &["branch", "-D", &req.branch]).await {
                            out.kept.push(e);
                        }
                        info!(worktree = %top.display(), branch = %req.branch, "worktree removed");
                    }
                    Err(e) => out.kept.push(e),
                }
            }
            Ok(risks) => out.kept = risks,
            Err(e) => out.kept.push(e),
        },
        Some(_) => {}
    }
    let worktree = top.as_ref().unwrap_or(&path).display().to_string();
    let cwd = if root.is_dir() { root.clone() } else { std::env::temp_dir() };
    if let Some(hooks) = crate::hooks::HookRunner::new("", &cwd, &req.hooks_json, labels) {
        let removed = if out.removed { "1" } else { "0" };
        let repo = root.display().to_string();
        let hooks = hooks.with_env(&[("C6_WORKTREE", &worktree), ("C6_BRANCH", &req.branch), ("C6_REPO", &repo), ("C6_REMOVED", removed)]);
        let payload =
            serde_json::json!({"worktree": worktree, "branch": req.branch, "repo": repo, "removed": out.removed, "kept": out.kept});
        out.hooks = hooks.run_reporting("WorkEnded", payload).await;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, script: &str) {
        let out = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn sh_out(dir: &Path, script: &str) -> String {
        let out = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    const GIT: &str = "git -c user.name=t -c user.email=t@t -c protocol.file.allow=always";

    /// An "upstream" repository with a submodule, and a clone of it whose
    /// main line upstream has since moved on.
    fn repos(tmp: &Path) -> PathBuf {
        let lib = tmp.join("lib");
        let up = tmp.join("upstream");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::create_dir_all(&up).unwrap();
        sh(&lib, &format!("git init -q -b main && echo lib > lib.txt && git add . && {GIT} commit -qm lib"));
        sh(&up, &format!("git init -q -b master && echo one > a.txt && git add . && {GIT} commit -qm one"));
        sh(&up, &format!("{GIT} submodule -q add {} lib && {GIT} commit -qm sub", lib.display()));
        sh(tmp, &format!("{GIT} clone -q {} clone", up.display()));
        sh(&up, &format!("echo two > b.txt && git add . && {GIT} commit -qm two"));
        tmp.join("clone")
    }

    #[tokio::test]
    async fn a_template_s_worktree_from_the_latest_base_with_its_submodules() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let spec = WorktreeSpec {
            branch: "me/kafka_ingest".into(),
            base: "origin/master".into(),
            fetch: true,
            track: false,
            setup: vec![format!("{GIT} submodule update --init --recursive"), "echo ready".into()],
        };
        let wt = tmp.path().join("wt");
        let made = create(&wt, "0190-aaaa-bbbb-1234abcd", &clone, &spec).await.unwrap();
        assert_eq!(made.branch, "me/kafka_ingest");
        assert_eq!(made.top, wt.join("clone").join("1234abcd"));
        // Fetched first: the upstream's second commit is there.
        assert!(made.top.join("b.txt").exists());
        // Not tracking origin/master.
        assert_eq!(sh_out(&made.top, "git config branch.me/kafka_ingest.merge || echo none"), "none");
        let log = setup(&made, &spec.setup).await.unwrap();
        assert!(log.contains("$ echo ready\nready"), "{log}");
        assert!(made.top.join("lib/lib.txt").exists(), "the submodule is checked out");
    }

    #[tokio::test]
    async fn defaults_branch_from_head_and_tracking_is_a_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let wt = tmp.path().join("wt");
        let made = create(&wt, "s-00000001", &clone.join("lib"), &WorktreeSpec::default()).await.unwrap();
        assert_eq!(made.branch, "c6/00000001");
        // The same place in the worktree as the folder was in the repository.
        assert_eq!(made.cwd, made.top.join("lib"));
        assert!(!made.top.join("b.txt").exists(), "HEAD, not fetched");
        let spec = WorktreeSpec { branch: "t/{{id}}".into(), base: "origin/master".into(), track: true, ..Default::default() };
        let made = create(&wt, "s-00000002", &clone, &spec).await.unwrap();
        assert_eq!(made.branch, "t/00000002");
        assert_eq!(sh_out(&made.top, "git config branch.t/00000002.merge"), "refs/heads/master");
        let bad = WorktreeSpec { branch: "a..b".into(), ..Default::default() };
        assert!(create(&wt, "s-00000003", &clone, &bad).await.unwrap_err().contains("not a valid git branch name"));
    }

    #[tokio::test]
    async fn a_failing_setup_command_stops_the_start_with_its_output() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let made = create(&tmp.path().join("wt"), "s-00000004", &clone, &WorktreeSpec::default()).await.unwrap();
        let err = setup(&made, &["echo first".into(), "echo broken >&2; exit 3".into(), "touch never".into()]).await.unwrap_err();
        assert!(err.contains("`echo broken >&2; exit 3` failed (exit 3):\nbroken"), "{err}");
        assert!(!made.top.join("never").exists());
        discard(&made).await;
        assert!(!made.top.exists());
        assert_eq!(sh_out(&clone, "git branch --list c6/00000004"), "");
    }

    #[tokio::test]
    async fn a_pushed_branch_starts_a_fresh_worktree_from_its_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        sh(
            &tmp.path().join("upstream"),
            &format!("git checkout -qb me/elsewhere && echo x > x.txt && git add . && {GIT} commit -qm x && git checkout -q master"),
        );
        let spec = WorktreeSpec { branch: "me/elsewhere".into(), base: "origin/master".into(), fetch: true, ..Default::default() };
        let made = create(&tmp.path().join("wt"), "s-00000005", &clone, &spec).await.unwrap();
        assert!(made.top.join("x.txt").exists(), "from the pushed branch, not the base");
    }

    #[tokio::test]
    async fn the_end_removes_only_what_is_safe_and_runs_the_hook_with_its_place() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = repos(tmp.path());
        let wt = tmp.path().join("wt");
        let end_of = |made: &Made, remove: bool, hooks: &str| WorktreeEnd {
            path: made.cwd.display().to_string(),
            branch: made.branch.clone(),
            repo_root: made.root.clone(),
            remove,
            hooks_json: hooks.to_owned(),
        };
        let branch_exists = |b: &str| !sh_out(&clone, &format!("git branch --list {b}")).is_empty();

        // Uncommitted changes: kept.
        let dirty = create(&wt, "s-0000000a", &clone, &WorktreeSpec { branch: "w/dirty".into(), ..Default::default() }).await.unwrap();
        sh(&dirty.top, "echo change >> a.txt");
        let r = end(&end_of(&dirty, true, ""), &[]).await;
        assert!(!r.removed && r.kept[0].contains("1 uncommitted change"), "{r:?}");
        assert!(dirty.top.exists() && branch_exists("w/dirty"));

        // A commit on no remote: kept.
        let ahead = create(&wt, "s-0000000b", &clone, &WorktreeSpec { branch: "w/ahead".into(), ..Default::default() }).await.unwrap();
        sh(&ahead.top, &format!("echo c > c.txt && git add . && {GIT} commit -qm c"));
        let r = end(&end_of(&ahead, true, ""), &[]).await;
        assert!(!r.removed && r.kept[0].contains("1 commit on w/ahead is on no remote"), "{r:?}");

        // Pushed: removed, branch and all, and the hook sees where it was.
        sh(&ahead.top, "git push -q origin w/ahead");
        let ahead_top = ahead.top.canonicalize().unwrap();
        let out = tmp.path().join("hook.out");
        let hooks = serde_json::json!([{"event": "WorkEnded", "command": format!(
            "echo \"$C6_WORKTREE|$C6_BRANCH|$C6_REPO|$C6_REMOVED\" > {}; cat > {}.json", out.display(), out.display())}]);
        let r = end(&end_of(&ahead, true, &hooks.to_string()), &[]).await;
        assert!(r.removed && r.kept.is_empty(), "{r:?}");
        assert!(!ahead.top.exists() && !branch_exists("w/ahead"));
        let said = std::fs::read_to_string(&out).unwrap();
        assert_eq!(said.trim(), format!("{}|w/ahead|{}|1", ahead_top.display(), ahead.root));
        let payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(format!("{}.json", out.display())).unwrap()).unwrap();
        assert_eq!(payload["hook_event_name"], "WorkEnded");
        assert_eq!(r.hooks.len(), 1);

        // Merged into another branch (not pushed): removed.
        let merged = create(&wt, "s-0000000c", &clone, &WorktreeSpec { branch: "w/merged".into(), ..Default::default() }).await.unwrap();
        sh(&merged.top, &format!("echo m > m.txt && git add . && {GIT} commit -qm m"));
        sh(&clone, &format!("{GIT} merge -q --ff-only w/merged"));
        let r = end(&end_of(&merged, true, ""), &[]).await;
        assert!(r.removed, "{r:?}");

        // Not asked to remove: kept, and the hook says so.
        let keep = create(&wt, "s-0000000d", &clone, &WorktreeSpec { branch: "w/keep".into(), ..Default::default() }).await.unwrap();
        let r = end(&end_of(&keep, false, &hooks.to_string()), &[]).await;
        assert!(!r.removed && keep.top.exists());
        assert!(std::fs::read_to_string(&out).unwrap().trim().ends_with(&format!("|w/keep|{}|0", keep.root)));
    }
}

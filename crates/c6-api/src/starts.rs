//! How dev work starts (docs/dev-starts.md): how a template makes a
//! worktree, where a work item's work lives, and what the session form is
//! filled with.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The branch a worktree gets when its template names none.
pub const DEFAULT_BRANCH: &str = "c6/{{id}}";

/// What happens to a worktree when its work ends (§5).
pub const CLEANUP_MODES: [&str; 3] = ["off", "ask", "auto"];

/// How a session's new git worktree is made (§2). Every empty value is
/// carbon6's default: a `c6/{{id}}` branch from `HEAD`, not tracking it, no
/// setup commands, nothing removed when the work ends.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorktreeOptions {
    /// The branch: in a template a pattern, where {{name}} is the work's
    /// title in snake_case and {{id}} the session's short id; for a session,
    /// the name ({{id}} may still be in it). Empty: `c6/{{id}}`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub branch: String,
    /// What the branch starts from, e.g. `origin/main`; empty: `HEAD`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base: String,
    /// Fetch the base's remote first, so it is the latest.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fetch: bool,
    /// The branch tracks the base (`--track`); off: `--no-track`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub track: bool,
    /// Shell lines run in the new worktree before the first turn; one that
    /// fails stops the start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup: Vec<String>,
    /// When the work ends: off (or empty), ask or auto.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cleanup: String,
}

impl WorktreeOptions {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The cleanup mode, `off` when unset.
    pub fn cleanup_mode(&self) -> &str {
        if self.cleanup.is_empty() { "off" } else { &self.cleanup }
    }
}

/// A title in snake_case, for a branch: letters and digits kept
/// (lowercased), everything else one `_`, at most 48 characters.
pub fn snake_name(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
        if out.len() >= 48 {
            break;
        }
    }
    out.trim_end_matches('_').to_owned()
}

/// A branch pattern filled in: {{name}} from `title` (else `work`), and
/// {{id}} from `id` when it is known (else left for the worker).
pub fn branch_name(pattern: &str, title: &str, id: Option<&str>) -> String {
    let pattern = if pattern.trim().is_empty() { DEFAULT_BRANCH } else { pattern.trim() };
    let name = snake_name(title);
    let mut b = pattern.replace("{{name}}", if name.is_empty() { "work" } else { &name });
    if let Some(id) = id {
        b = b.replace("{{id}}", id);
    }
    b
}

/// Whether `name` is a valid git branch name (`git check-ref-format
/// --branch`), with any {{id}} still in it counted as a word.
pub fn check_branch(name: &str) -> Result<(), String> {
    let name = name.replace("{{id}}", "id");
    let bad = |why: &str| Err(format!("{name:?} is not a git branch name: {why}"));
    if name.is_empty() {
        return bad("it is empty");
    }
    if name == "@" || name.contains("@{") {
        return bad("it has @{ in it");
    }
    if name.starts_with('-') {
        return bad("it starts with -");
    }
    if let Some(c) = name.chars().find(|c| c.is_ascii_control() || " ~^:?*[\\".contains(*c)) {
        return bad(&format!("it has {c:?} in it"));
    }
    if name.contains("..") || name.contains("//") {
        return bad("it has .. or // in it");
    }
    if name.starts_with('/') || name.ends_with('/') || name.ends_with('.') || name.ends_with(".lock") {
        return bad("it begins or ends badly");
    }
    if name.split('/').any(|part| part.starts_with('.') || part.ends_with(".lock")) {
        return bad("a part of it starts with . or ends with .lock");
    }
    Ok(())
}

/// Checks a template's (or a session's) worktree options.
pub fn check_options(o: &WorktreeOptions) -> Result<(), String> {
    check_branch(&branch_name(&o.branch, "name", None))?;
    let base = o.base.trim();
    if !base.is_empty() && (base.starts_with('-') || base.contains(char::is_whitespace)) {
        return Err(format!("{base:?} is not a base to branch from (a branch, tag or commit, e.g. origin/main)"));
    }
    if !o.cleanup.is_empty() && !CLEANUP_MODES.contains(&o.cleanup.as_str()) {
        return Err(format!("when the work ends is off, ask or auto, not {:?}", o.cleanup));
    }
    Ok(())
}

/// Where a work item's work lives (§4): recorded by its first session in a
/// new worktree, forgotten when the work ends.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct WorkPlace {
    pub worker_id: String,
    pub worker: String,
    /// The folder sessions work in (inside the worktree).
    pub path: String,
    pub branch: String,
    /// The repository the worktree belongs to.
    #[serde(default)]
    pub repo_root: String,
    /// What happens when the work ends: off, ask or auto.
    #[serde(default)]
    pub cleanup: String,
    /// The template it was started with, and its first session.
    #[serde(default)]
    pub template: String,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub at: crate::UnixMs,
}

/// `GET /api/v1/starts`: what a new session for some work starts with.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StartQuery {
    /// A work item's id, or `project:<id>`.
    #[serde(default)]
    pub work: String,
    /// The to-do it starts on.
    #[serde(default)]
    pub todo: String,
    /// A template to use instead of the work's "start with".
    #[serde(default)]
    pub template: String,
    /// Another worker, instead of the one the work's place is on.
    #[serde(default)]
    pub worker: String,
    /// The first message, which names the branch when there's no work.
    #[serde(default)]
    pub text: String,
}

/// What the session form opens with for some work: the template's values,
/// the worker it picked and why.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StartPlan {
    /// The template used; empty: none applies (the form keeps its own).
    pub template: String,
    pub worker_id: Option<Uuid>,
    pub worker_name: String,
    pub cwd: String,
    /// A new worktree, made with `options` (its branch filled in).
    pub worktree: bool,
    pub options: WorktreeOptions,
    pub connection_id: String,
    pub model: String,
    pub effort: String,
    pub labels: Vec<String>,
    /// The first message's start, from the template.
    pub prompt: String,
    /// Why this worker: "rust-dev-02, where this work's worktree is".
    pub why: String,
    /// The worker the work's place is on is offline.
    pub offline: bool,
    /// It goes back to the work's worktree.
    pub reuse: bool,
}

/// `POST /api/v1/worktrees/end`: a decision's or an automatic removal of a
/// worktree, as the worker does it (`worktree.end`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorktreeEnd {
    /// The folder in the worktree.
    pub path: String,
    pub branch: String,
    #[serde(default)]
    pub repo_root: String,
    /// Remove it (when nothing would be lost); false: only the hooks run.
    pub remove: bool,
    /// The WorkEnded hooks that apply (JSON array of `Hook`).
    #[serde(default)]
    pub hooks_json: String,
}

/// What the worker did (`worktree.end`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorktreeEnded {
    pub removed: bool,
    /// Why it was kept: what would be lost.
    #[serde(default)]
    pub kept: Vec<String>,
    /// What the hooks said, one line each.
    #[serde(default)]
    pub hooks: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_make_branches() {
        assert_eq!(branch_name("", "x", None), "c6/{{id}}");
        assert_eq!(branch_name("", "x", Some("01ab")), "c6/01ab");
        assert_eq!(branch_name("vinayakb/{{name}}", "Kafka ingest: retry on 503!", None), "vinayakb/kafka_ingest_retry_on_503");
        assert_eq!(branch_name("{{name}}-{{id}}", "  ", Some("9f")), "work-9f");
        assert_eq!(snake_name("Ünïcode — and  spaces"), "n_code_and_spaces");
        assert!(snake_name(&"a".repeat(80)).len() <= 48);
    }

    #[test]
    fn branch_names_are_checked_as_git_does() {
        for ok in ["c6/{{id}}", "vinayakb/kafka_ingest", "feature/x.y", "a-b"] {
            assert!(check_branch(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-x", "a b", "a..b", "a//b", "/a", "a/", "a.", "a.lock", "a/.b", "a@{1}", "@", "a~1", "a:b", "a*"] {
            assert!(check_branch(bad).is_err(), "{bad}");
        }
        assert!(check_options(&WorktreeOptions { cleanup: "sometimes".into(), ..Default::default() }).is_err());
        assert!(check_options(&WorktreeOptions { base: "--upload-pack=x".into(), ..Default::default() }).is_err());
        assert!(
            check_options(&WorktreeOptions { branch: "me/{{name}}".into(), base: "origin/master".into(), ..Default::default() }).is_ok()
        );
    }
}

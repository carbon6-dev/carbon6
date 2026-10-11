//! Session labels and the "applies to" selectors of settings.
//!
//! A session's labels are the ones you attach (`client:acme`, `rust`) and
//! automatic ones: `worker:<name>`, `repo:<host/owner/name>` (from the git
//! remote, so a repository is the same on every machine), `folder:<path>`
//! (absolute, and `~/…` when under the home folder), `model:<model>`, and
//! `host:<host>` for a session on another machine over SSH.
//!
//! A setting (command, agent, skill, hook) applies to a session when every
//! label in its `applies_to` matches one of the session's: `folder:` and
//! `repo:` match a path prefix (`repo:github.com/acme` covers the whole
//! organization), the rest match exactly. An empty `applies_to` applies
//! everywhere.

/// Label keys the system sets; yours cannot use them.
pub const AUTO_KEYS: [&str; 5] = ["worker", "repo", "folder", "model", "host"];

/// A session's automatic labels (`ssh`: its SSH target, if any).
pub fn auto_labels(worker: &str, repo: &str, folder: &str, home: &str, model: &str, ssh: &str) -> Vec<String> {
    let mut out = Vec::new();
    if !worker.is_empty() {
        out.push(format!("worker:{worker}"));
    }
    if !ssh.is_empty() {
        // user@host:port → host
        let host = ssh.rsplit('@').next().unwrap_or(ssh);
        out.push(format!("host:{}", host.split(':').next().unwrap_or(host)));
    }
    if !repo.is_empty() {
        out.push(format!("repo:{repo}"));
    }
    if !folder.is_empty() {
        out.push(format!("folder:{folder}"));
        let home = home.trim_end_matches('/');
        if !home.is_empty()
            && let Some(rest) = folder.strip_prefix(home)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            out.push(format!("folder:~{rest}"));
        }
    }
    if !model.is_empty() {
        out.push(format!("model:{model}"));
    }
    out
}

/// Whether one selector label matches a session with `labels`. `key:`
/// alone matches any value (`host:`: every SSH session).
pub fn label_matches(selector: &str, labels: &[String]) -> bool {
    let selector = selector.trim();
    if selector.len() > 1 && selector.ends_with(':') && !selector[..selector.len() - 1].contains(':') {
        return labels.iter().any(|l| l.starts_with(selector));
    }
    match selector.split_once(':') {
        Some((key @ ("folder" | "repo"), want)) => {
            let want = want.trim_end_matches('/');
            labels
                .iter()
                .filter_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(':')))
                .any(|have| have == want || (have.starts_with(want) && have[want.len()..].starts_with('/')) || want.is_empty())
        }
        _ => labels.iter().any(|l| l == selector),
    }
}

/// Whether a setting with `applies_to` applies to a session with `labels`.
pub fn applies(applies_to: &[String], labels: &[String]) -> bool {
    applies_to.iter().filter(|s| !s.trim().is_empty()).all(|s| label_matches(s, labels))
}

/// Of the settings that share a name, the one for a session: the most
/// specific match (most labels), the later one in the list on a tie.
pub fn best<'a, T>(items: impl IntoIterator<Item = &'a T>, applies_to: impl Fn(&T) -> &[String], labels: &[String]) -> Option<&'a T>
where
    T: 'a,
{
    let mut best: Option<(&T, usize)> = None;
    for item in items {
        let sel = applies_to(item);
        if applies(sel, labels) {
            let n = sel.iter().filter(|s| !s.trim().is_empty()).count();
            if best.is_none_or(|(_, m)| n >= m) {
                best = Some((item, n));
            }
        }
    }
    best.map(|(i, _)| i)
}

/// Labels typed as text: separated by commas or spaces, deduplicated.
pub fn parse_labels(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in text.split([',', ' ', '\n', '\t']).map(str::trim).filter(|l| !l.is_empty()) {
        if !out.iter().any(|x| x == l) {
            out.push(l.to_owned());
        }
    }
    out
}

/// Why a label cannot be one of yours, if it cannot.
pub fn check_user_label(label: &str) -> Result<(), String> {
    if label.is_empty() || label.len() > 64 || label.chars().any(|c| c.is_whitespace() || c == ',') {
        return Err(format!("{label:?} is not a valid label (no spaces or commas, at most 64 characters)"));
    }
    if let Some((key, _)) = label.split_once(':')
        && AUTO_KEYS.contains(&key)
    {
        return Err(format!("{key}: labels are set automatically"));
    }
    Ok(())
}

/// The folder labels shown to people: the `~/` form when there is one.
pub fn display_labels(labels: &[String]) -> Vec<String> {
    let has_home_form = labels.iter().any(|l| l.starts_with("folder:~"));
    labels.iter().filter(|l| !(has_home_form && l.starts_with("folder:/"))).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        let mut labels = auto_labels("darkstar", "github.com/acme/app", "/Users/v/code/app/web", "/Users/v", "qwen3.6:27b", "");
        assert!(auto_labels("w", "", "/root", "/root", "m", "admin@web1:2222").contains(&"host:web1".to_owned()));
        assert!(label_matches("host:", &auto_labels("w", "", "", "", "", "web1")), "any host");
        assert!(!label_matches("host:", &auto_labels("w", "", "", "", "", "")));
        labels.push("rust".into());
        assert!(labels.contains(&"folder:~/code/app/web".to_owned()));
        let yes = |s: &[&str]| applies(&s.iter().map(|x| x.to_string()).collect::<Vec<_>>(), &labels);
        assert!(yes(&[]));
        assert!(yes(&["rust", "worker:darkstar"]));
        assert!(yes(&["repo:github.com/acme"]) && yes(&["repo:github.com/acme/app"]));
        assert!(!yes(&["repo:github.com/acm"]), "prefixes are whole path segments");
        assert!(yes(&["folder:~/code/app"]) && yes(&["folder:/Users/v/code"]) && yes(&["folder:~/code/app/"]));
        assert!(!yes(&["folder:~/code/ap"]));
        assert!(!yes(&["rust", "python"]), "every label must match");
        assert!(yes(&["model:qwen3.6:27b"]));

        let items = [(vec![], "everywhere"), (vec!["rust".to_owned()], "rust"), (vec!["python".to_owned()], "python")];
        assert_eq!(best(items.iter(), |i| &i.0, &labels).map(|i| i.1), Some("rust"));
        assert_eq!(display_labels(&labels).iter().filter(|l| l.starts_with("folder:")).count(), 1);
        assert_eq!(parse_labels("a, b c,a"), ["a", "b", "c"]);
        assert!(check_user_label("client:acme").is_ok() && check_user_label("repo:x").is_err() && check_user_label("a b").is_err());
    }
}

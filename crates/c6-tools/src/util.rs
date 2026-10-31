/// Keeps the head and tail of `s` within `max` chars, eliding the middle.
pub fn truncate_middle(s: &str, max: usize) -> (String, bool) {
    let count = s.chars().count();
    if count <= max {
        return (s.to_owned(), false);
    }
    let half = max / 2;
    let head: String = s.chars().take(half).collect();
    let tail: String = s.chars().skip(count - half).collect();
    (format!("{head}\n\n… [{} characters omitted] …\n\n{tail}", count - 2 * half), true)
}

/// Keeps a long command output in a file for the agent to read, under the
/// temporary folder's `c6-output`; files from earlier days are removed.
pub fn save_output(text: &str) -> Option<std::path::PathBuf> {
    save_output_in(&std::env::temp_dir().join("c6-output"), text)
}

pub(crate) fn save_output_in(dir: &std::path::Path, text: &str) -> Option<std::path::PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let day = std::time::Duration::from_secs(86_400);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > day);
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let path = dir.join(format!("{nanos}-{}.log", std::process::id()));
    std::fs::write(&path, text).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_short_and_elides_long() {
        assert_eq!(truncate_middle("abc", 10), ("abc".into(), false));
        let (t, cut) = truncate_middle("0123456789", 4);
        assert!(cut);
        assert!(t.starts_with("01") && t.ends_with("89") && t.contains("6 characters omitted"));
    }

    #[test]
    fn long_output_is_kept_in_a_file() {
        let dir = std::env::temp_dir().join(format!("c6-output-test-{}", std::process::id()));
        let path = save_output_in(&dir, "all of it").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "all of it");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

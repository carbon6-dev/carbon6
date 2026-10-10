//! System prompt assembly. Built once per session and recorded in
//! `SessionStarted`, so it stays a stable cache prefix for the whole session.

use std::path::{Path, PathBuf};

/// Instruction files, in priority order, read from each directory between the
/// repo root and the working directory.
const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];
const MAX_INSTRUCTION_BYTES: usize = 64 * 1024;

pub fn build_system_prompt(base: &str, cwd: &Path, date: &str) -> String {
    let git_root = find_git_root(cwd);
    let mut out = base.trim_end().to_owned();
    out.push_str("\n\n# Environment\n");
    out.push_str(&format!("- Working directory: {}\n", cwd.display()));
    out.push_str(&format!("- Platform: {} ({})\n", std::env::consts::OS, std::env::consts::ARCH));
    out.push_str("- Shell: bash\n");
    match &git_root {
        Some(root) => out.push_str(&format!("- Git repository root: {}\n", root.display())),
        None => out.push_str("- Not a git repository\n"),
    }
    out.push_str(&format!("- Session started: {date}\n"));

    for (path, text) in instruction_files(cwd, git_root.as_deref()) {
        out.push_str(&format!("\n# Project instructions ({})\n\n{}\n", path.display(), text.trim_end()));
    }
    out
}

fn find_git_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

/// Outermost first, so more specific instructions come later and win.
fn instruction_files(cwd: &Path, git_root: Option<&Path>) -> Vec<(PathBuf, String)> {
    let mut dirs: Vec<&Path> = Vec::new();
    for d in cwd.ancestors() {
        dirs.push(d);
        if Some(d) == git_root {
            break;
        }
    }
    if git_root.is_none() {
        dirs.truncate(1); // outside a repo, only the cwd itself
    }
    dirs.reverse();

    let mut out = Vec::new();
    for dir in dirs {
        for name in INSTRUCTION_FILES {
            let path = dir.join(name);
            if let Ok(text) = std::fs::read_to_string(&path) {
                let text = if text.len() > MAX_INSTRUCTION_BYTES {
                    let mut end = MAX_INSTRUCTION_BYTES;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}\n[truncated]", &text[..end])
                } else {
                    text
                };
                out.push((path, text));
                break; // AGENTS.md wins over CLAUDE.md in the same directory
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_instructions_from_repo_root_down() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "ignored: AGENTS.md wins").unwrap();
        std::fs::write(root.join("pkg/CLAUDE.md"), "pkg rules").unwrap();

        let p = build_system_prompt("BASE", &root.join("pkg"), "2026-10-09");
        assert!(p.starts_with("BASE\n\n# Environment"));
        let (r, k) = (p.find("root rules").unwrap(), p.find("pkg rules").unwrap());
        assert!(r < k);
        assert!(!p.contains("ignored"));
    }
}

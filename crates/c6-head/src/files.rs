//! The head's own files, for the UI's file picker (loading a package from
//! the head's disk): list a folder, read a text file. Only you reach these
//! (realm-scoped tokens are refused, and the assistant is not offered
//! them); the head's data directory (its database and keys) is not read.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};

use crate::{HeadState, bad_request};

/// Largest file `read` returns.
const MAX_READ: u64 = 8 * 1024 * 1024;
const MAX_LIST: usize = 5000;

fn expand(path: &str) -> Option<PathBuf> {
    let path = path.trim();
    if path.is_empty() || path == "~" {
        return dirs::home_dir();
    }
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map(|h| h.join(rest)),
        None => Some(PathBuf::from(path)),
    }
}

/// A folder's entries (folders first), as the workers' `fs.list` gives
/// them; its home folder when `path` is empty.
pub async fn list(path: &str, hidden: bool) -> anyhow::Result<Value> {
    let dir = expand(path).ok_or_else(|| bad_request("no home folder"))?;
    let dir = tokio::fs::canonicalize(&dir).await.map_err(|_| bad_request(format!("{} not found", dir.display())))?;
    let mut rd = tokio::fs::read_dir(&dir).await.map_err(|e| bad_request(format!("cannot list {}: {e}", dir.display())))?;
    let mut entries = Vec::new();
    while let Some(e) = rd.next_entry().await? {
        let name = e.file_name().to_string_lossy().into_owned();
        if !hidden && name.starts_with('.') {
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
    Ok(json!({"path": dir.display().to_string(), "entries": entries}))
}

/// A text file's content, as the workers' `fs.read` gives it.
pub async fn read(st: &HeadState, path: &str) -> anyhow::Result<Value> {
    let asked = expand(path).filter(|_| !path.trim().is_empty()).ok_or_else(|| bad_request("path required"))?;
    let path = tokio::fs::canonicalize(&asked).await.map_err(|_| bad_request(format!("{} not found", asked.display())))?;
    if inside(&path, &st.data_dir).await {
        return Err(bad_request("that is the head's own data (its database and keys), which is not read here"));
    }
    let meta = tokio::fs::metadata(&path).await?;
    if meta.is_dir() {
        return Err(bad_request(format!("{} is a folder", path.display())));
    }
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(&path).await.map_err(|e| bad_request(format!("cannot read {}: {e}", path.display())))?;
    let mut buf = Vec::with_capacity(meta.len().min(MAX_READ) as usize);
    (&mut f).take(MAX_READ).read_to_end(&mut buf).await?;
    let binary = buf[..buf.len().min(8192)].contains(&0);
    let content = if binary { Value::Null } else { Value::String(String::from_utf8_lossy(&buf).into_owned()) };
    Ok(
        json!({"path": path.display().to_string(), "size": meta.len(), "truncated": meta.len() > MAX_READ, "binary": binary, "content": content}),
    )
}

async fn inside(path: &Path, dir: &Path) -> bool {
    match tokio::fs::canonicalize(dir).await {
        Ok(d) => path.starts_with(d),
        Err(_) => path.starts_with(dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_folders_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("b")).unwrap();
        std::fs::write(dir.path().join("a.json"), "{}").unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();
        let l = list(&dir.path().display().to_string(), false).await.unwrap();
        let names: Vec<&str> = l["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["b", "a.json"]);
    }
}

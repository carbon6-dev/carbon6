//! read, write, edit.

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput, parse_input};

const DEFAULT_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;

pub struct Read;

#[derive(Deserialize)]
struct ReadInput {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for Read {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "Read a text file. Returns lines prefixed with their 1-based line numbers. Reads up to 2000 lines \
                          by default; use offset and limit for large files. Paths are relative to the working directory \
                          unless absolute. Read a file before editing or overwriting it. An image (PNG, JPEG, GIF, WebP) is \
                          shown to you as an image: use it to look at a chart, a rendered slide or a screenshot you made."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path"},
                    "offset": {"type": "integer", "minimum": 1, "description": "1-based line to start from"},
                    "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of lines to return"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let input: ReadInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let path = ctx.session.resolve(&input.path);
        if ctx.session.fs_stat(&path).await.0 == crate::remote::Kind::Dir {
            return ToolOutput::err(format!("{} is a directory; use glob or bash `ls` to list it", path.display()));
        }
        let bytes = match ctx.session.fs_read(&path).await {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
        };
        // An image: shown, not read as text.
        if let Some(mime) = image_type(&bytes) {
            ctx.session.mark_seen(&path).await;
            return ToolOutput::ok(format!("{} ({mime}, {} KB)", path.display(), bytes.len() / 1024)).with_image(mime, bytes);
        }
        if bytes[..bytes.len().min(8192)].contains(&0) {
            return ToolOutput::err(format!("{} looks like a binary file", path.display()));
        }
        ctx.session.mark_seen(&path).await;
        let text = String::from_utf8_lossy(&bytes);
        if text.is_empty() {
            return ToolOutput::ok("(empty file)");
        }

        let total = text.lines().count();
        let start = input.offset.unwrap_or(1).max(1);
        let limit = input.limit.unwrap_or(DEFAULT_LINES);
        if start > total {
            return ToolOutput::err(format!("offset {start} is past the end of the file ({total} lines)"));
        }
        let mut out = String::new();
        let mut last = start;
        for (i, line) in text.lines().enumerate().skip(start - 1).take(limit) {
            last = i + 1;
            let line = if line.chars().count() > MAX_LINE_CHARS {
                format!("{}… [line truncated]", line.chars().take(MAX_LINE_CHARS).collect::<String>())
            } else {
                line.to_owned()
            };
            out.push_str(&format!("{:>6}\t{line}\n", i + 1));
        }
        if last < total || start > 1 {
            out.push_str(&format!("\n(showing lines {start}-{last} of {total}; use offset/limit to see more)\n"));
        }
        ToolOutput::limited(out, false)
    }
}

/// An image's media type, from its first bytes.
pub fn image_type(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub struct Write;

#[derive(Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for Write {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "Create or overwrite a file with the given content, creating parent directories as needed. \
                          An existing file must be read first. Prefer edit for changes to existing files."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path"},
                    "content": {"type": "string", "description": "Full file content"}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let input: WriteInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let path = ctx.session.resolve(&input.path);
        if let Err(msg) = ctx.session.check_writable(&path).await {
            return ToolOutput::err(msg);
        }
        let existed = ctx.session.fs_stat(&path).await.0 != crate::remote::Kind::Missing;
        if let Err(e) = ctx.session.fs_write(&path, input.content.as_bytes()).await {
            return ToolOutput::err(format!("cannot write {}: {e}", path.display()));
        }
        ctx.session.mark_seen(&path).await;
        let lines = input.content.lines().count();
        ToolOutput::ok(format!("{} {} ({lines} lines)", if existed { "Overwrote" } else { "Created" }, path.display()))
    }
}

pub struct Edit;

#[derive(Deserialize)]
struct EditInput {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for Edit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: "Replace an exact string in a file. old_string must match the file exactly, including whitespace \
                          and indentation, and must be unique unless replace_all is true; include surrounding lines to \
                          make it unique. The file must be read first."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path"},
                    "old_string": {"type": "string", "description": "Exact text to replace"},
                    "new_string": {"type": "string", "description": "Replacement text"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let input: EditInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let path = ctx.session.resolve(&input.path);
        if ctx.session.fs_stat(&path).await.0 == crate::remote::Kind::Missing {
            return ToolOutput::err(format!("{} does not exist; use write to create it", path.display()));
        }
        if let Err(msg) = ctx.session.check_writable(&path).await {
            return ToolOutput::err(msg);
        }
        if input.old_string == input.new_string {
            return ToolOutput::err("old_string and new_string are identical");
        }
        if input.old_string.is_empty() {
            return ToolOutput::err("old_string must not be empty");
        }
        let text = match ctx.session.fs_read(&path).await.and_then(|b| String::from_utf8(b).map_err(|_| "not UTF-8 text".to_owned())) {
            Ok(t) => t,
            Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
        };
        let count = text.matches(&input.old_string).count();
        let updated = match (count, input.replace_all) {
            (0, _) => return ToolOutput::err(format!("old_string not found in {}", path.display())),
            (1, _) | (_, true) => text.replace(&input.old_string, &input.new_string),
            (n, false) => {
                return ToolOutput::err(format!(
                    "old_string occurs {n} times in {}; add surrounding context to make it unique, or set replace_all",
                    path.display()
                ));
            }
        };
        if let Err(e) = ctx.session.fs_write(&path, updated.as_bytes()).await {
            return ToolOutput::err(format!("cannot write {}: {e}", path.display()));
        }
        ctx.session.mark_seen(&path).await;
        ToolOutput::ok(format!("Edited {} ({count} replacement{})", path.display(), if count == 1 { "" } else { "s" }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::SessionTools;

    fn ctx(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            session: Arc::new(SessionTools::new(dir.to_owned())),
            cancel: Default::default(),
            output: Arc::new(|_: &str| {}),
            call_id: String::new(),
        }
    }

    #[tokio::test]
    async fn read_requires_before_overwrite_and_edit_is_exact() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\ntwo\n").unwrap();

        let r = Write.call(&ctx, json!({"path": "a.txt", "content": "x"})).await;
        assert!(r.is_error && r.content.contains("has not been read"), "{r:?}");

        let r = Read.call(&ctx, json!({"path": "a.txt", "offset": 2, "limit": 1})).await;
        assert!(r.content.starts_with("     2\ttwo\n"), "{r:?}");
        assert!(r.content.contains("lines 2-2 of 3"));

        let r = Edit.call(&ctx, json!({"path": "a.txt", "old_string": "two", "new_string": "2"})).await;
        assert!(r.is_error && r.content.contains("occurs 2 times"));
        let r = Edit.call(&ctx, json!({"path": "a.txt", "old_string": "two", "new_string": "2", "replace_all": true})).await;
        assert!(!r.is_error, "{r:?}");
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "one\n2\n2\n");

        // Changed behind our back: must re-read.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.path().join("a.txt"), "external\n").unwrap();
        let r = Edit.call(&ctx, json!({"path": "a.txt", "old_string": "external", "new_string": "x"})).await;
        assert!(r.is_error && r.content.contains("changed on disk"), "{r:?}");

        let r = Write.call(&ctx, json!({"path": "sub/new.txt", "content": "hi\n"})).await;
        assert!(r.content.starts_with("Created"), "{r:?}");
    }

    #[tokio::test]
    async fn read_shows_images() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let png = [b"\x89PNG\r\n\x1a\n".as_slice(), &[0u8; 40]].concat();
        std::fs::write(dir.path().join("slide.png"), &png).unwrap();
        let r = Read.call(&ctx, json!({"path": "slide.png"})).await;
        assert!(!r.is_error && r.content.contains("image/png"), "{r:?}");
        assert_eq!(r.images, vec![("image/png".to_owned(), png)]);
        std::fs::write(dir.path().join("big.jpg"), [vec![0xff, 0xd8, 0xff], vec![0; crate::MAX_IMAGE_BYTES]].concat()).unwrap();
        let r = Read.call(&ctx, json!({"path": "big.jpg"})).await;
        assert!(r.images.is_empty() && r.content.contains("too large to show"), "{}", r.content);
        std::fs::write(dir.path().join("blob.bin"), [0u8, 1, 2]).unwrap();
        assert!(Read.call(&ctx, json!({"path": "blob.bin"})).await.content.contains("binary"));
    }
}

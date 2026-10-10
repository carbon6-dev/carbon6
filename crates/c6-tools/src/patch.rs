//! apply_patch: the Codex patch format.
//!
//! ```text
//! *** Begin Patch
//! *** Add File: path
//! +line
//! *** Update File: path
//! *** Move to: new/path
//! @@ optional context line
//!  context
//! -old
//! +new
//! *** Delete File: path
//! *** End Patch
//! ```
//!
//! The whole patch is validated before any file is written.

use std::path::PathBuf;

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput, parse_input};

pub struct ApplyPatch;

#[derive(Deserialize)]
struct PatchInput {
    input: String,
}

#[async_trait]
impl Tool for ApplyPatch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".into(),
            description: "Edit files with a patch. The input is the full patch text:\n\
                          *** Begin Patch\n\
                          *** Add File: <path>          (then every line prefixed with +)\n\
                          *** Delete File: <path>\n\
                          *** Update File: <path>       (optionally followed by *** Move to: <new path>)\n\
                          @@ <optional line to anchor the hunk>\n\
                          <context lines prefixed with a space, removed lines with -, added lines with +>\n\
                          *** End Patch\n\
                          Include about three lines of context around each change. Paths are relative to the working directory."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"input": {"type": "string", "description": "The entire patch, from *** Begin Patch to *** End Patch"}},
                "required": ["input"],
                "additionalProperties": false
            }),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let input: PatchInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let ops = match parse(&input.input) {
            Ok(ops) => ops,
            Err(e) => return ToolOutput::err(format!("invalid patch: {e}")),
        };
        // Plan every change first so a bad hunk leaves the tree untouched.
        let mut writes: Vec<(PathBuf, Option<String>)> = Vec::new();
        let mut summary = Vec::new();
        for op in ops {
            match op {
                Op::Add { path, lines } => {
                    let p = ctx.session.resolve(&path);
                    if p.exists() {
                        return ToolOutput::err(format!("cannot add {path}: it already exists (use Update File)"));
                    }
                    writes.push((p, Some(join(&lines, true))));
                    summary.push(format!("A {path}"));
                }
                Op::Delete { path } => {
                    let p = ctx.session.resolve(&path);
                    if !p.is_file() {
                        return ToolOutput::err(format!("cannot delete {path}: no such file"));
                    }
                    writes.push((p, None));
                    summary.push(format!("D {path}"));
                }
                Op::Update { path, move_to, hunks } => {
                    let p = ctx.session.resolve(&path);
                    let text = match std::fs::read_to_string(&p) {
                        Ok(t) => t,
                        Err(e) => return ToolOutput::err(format!("cannot read {path}: {e}")),
                    };
                    let updated = match apply_hunks(&text, &hunks) {
                        Ok(u) => u,
                        Err(e) => return ToolOutput::err(format!("{path}: {e}")),
                    };
                    match move_to {
                        Some(dest) => {
                            writes.push((ctx.session.resolve(&dest), Some(updated)));
                            writes.push((p, None));
                            summary.push(format!("M {path} -> {dest}"));
                        }
                        None => {
                            writes.push((p, Some(updated)));
                            summary.push(format!("M {path}"));
                        }
                    }
                }
            }
        }
        for (path, contents) in &writes {
            let result = match contents {
                Some(c) => path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(path, c)),
                None => std::fs::remove_file(path),
            };
            if let Err(e) = result {
                return ToolOutput::err(format!(
                    "failed writing {}: {e} (earlier files in this patch were already written)",
                    path.display()
                ));
            }
            if contents.is_some() {
                ctx.session.mark_seen(path);
            }
        }
        ToolOutput::ok(format!("Success. Updated the following files:\n{}", summary.join("\n")))
    }
}

#[derive(Debug, PartialEq)]
enum Op {
    Add { path: String, lines: Vec<String> },
    Delete { path: String },
    Update { path: String, move_to: Option<String>, hunks: Vec<Hunk> },
}

#[derive(Debug, Default, PartialEq)]
struct Hunk {
    anchor: Option<String>,
    old: Vec<String>,
    new: Vec<String>,
    at_eof: bool,
}

fn parse(patch: &str) -> Result<Vec<Op>, String> {
    let lines: Vec<&str> = patch.trim().lines().collect();
    if lines.first().map(|l| l.trim()) != Some("*** Begin Patch") {
        return Err("must start with *** Begin Patch".into());
    }
    if lines.last().map(|l| l.trim()) != Some("*** End Patch") {
        return Err("must end with *** End Patch".into());
    }
    let body = &lines[1..lines.len() - 1];
    let mut ops = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            i += 1;
            let mut out = Vec::new();
            while i < body.len() && !body[i].starts_with("*** ") {
                out.push(body[i].strip_prefix('+').ok_or_else(|| format!("line {} of added file must start with +", i + 2))?.to_owned());
                i += 1;
            }
            ops.push(Op::Add { path: path.trim().to_owned(), lines: out });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(Op::Delete { path: path.trim().to_owned() });
            i += 1;
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            i += 1;
            let mut move_to = None;
            if let Some(dest) = body.get(i).and_then(|l| l.strip_prefix("*** Move to: ")) {
                move_to = Some(dest.trim().to_owned());
                i += 1;
            }
            let mut hunks: Vec<Hunk> = Vec::new();
            let mut cur: Option<Hunk> = None;
            while i < body.len() && !(body[i].starts_with("*** ") && body[i] != "*** End of File") {
                let l = body[i];
                if l == "*** End of File" {
                    if let Some(h) = cur.as_mut() {
                        h.at_eof = true;
                    }
                } else if let Some(rest) = l.strip_prefix("@@") {
                    if let Some(h) = cur.take() {
                        hunks.push(h);
                    }
                    let anchor = rest.trim();
                    cur = Some(Hunk { anchor: (!anchor.is_empty()).then(|| anchor.to_owned()), ..Default::default() });
                } else {
                    let h = cur.get_or_insert_with(Hunk::default);
                    match l.chars().next() {
                        Some('+') => h.new.push(l[1..].to_owned()),
                        Some('-') => h.old.push(l[1..].to_owned()),
                        Some(' ') => {
                            h.old.push(l[1..].to_owned());
                            h.new.push(l[1..].to_owned());
                        }
                        // A blank line inside a hunk is a blank context line.
                        None => {
                            h.old.push(String::new());
                            h.new.push(String::new());
                        }
                        _ => return Err(format!("unexpected line in hunk: {l:?} (prefix lines with ' ', '-' or '+')")),
                    }
                }
                i += 1;
            }
            if let Some(h) = cur.take() {
                hunks.push(h);
            }
            if hunks.is_empty() && move_to.is_none() {
                return Err(format!("Update File {path} has no hunks"));
            }
            ops.push(Op::Update { path: path.trim().to_owned(), move_to, hunks });
        } else if line.trim().is_empty() {
            i += 1;
        } else {
            return Err(format!("unexpected line: {line:?}"));
        }
    }
    if ops.is_empty() {
        return Err("patch contains no operations".into());
    }
    Ok(ops)
}

fn apply_hunks(text: &str, hunks: &[Hunk]) -> Result<String, String> {
    let trailing_newline = text.ends_with('\n') || text.is_empty();
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut cursor = 0;
    for (n, h) in hunks.iter().enumerate() {
        if let Some(anchor) = &h.anchor {
            // Anchors are hints; the context lines below are what must match.
            if let Some(at) = find(&lines, std::slice::from_ref(anchor), cursor) {
                cursor = at + 1;
            }
        }
        if h.old.is_empty() {
            let at = if h.at_eof || h.anchor.is_none() { lines.len() } else { cursor };
            lines.splice(at..at, h.new.iter().cloned());
            cursor = at + h.new.len();
            continue;
        }
        let at = if h.at_eof {
            let start = lines.len().checked_sub(h.old.len()).ok_or("hunk is longer than the file")?;
            find(&lines, &h.old, start).filter(|&a| a == start)
        } else {
            find(&lines, &h.old, cursor).or_else(|| find(&lines, &h.old, 0))
        };
        let Some(at) = at else {
            return Err(format!(
                "hunk {} did not match the file. Expected these lines:\n{}\nRead the file again and retry with exact context.",
                n + 1,
                h.old.join("\n")
            ));
        };
        lines.splice(at..at + h.old.len(), h.new.iter().cloned());
        cursor = at + h.new.len();
    }
    Ok(join(&lines, trailing_newline))
}

/// Finds `needle` in `hay` at or after `from`: exact, then ignoring trailing
/// whitespace, then ignoring surrounding whitespace.
fn find(hay: &[String], needle: &[String], from: usize) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    let passes: [fn(&str, &str) -> bool; 3] = [|a, b| a == b, |a, b| a.trim_end() == b.trim_end(), |a, b| a.trim() == b.trim()];
    for eq in passes {
        for start in from..=hay.len() - needle.len() {
            if needle.iter().zip(&hay[start..]).all(|(n, h)| eq(h, n)) {
                return Some(start);
            }
        }
    }
    None
}

fn join(lines: &[String], trailing_newline: bool) -> String {
    let mut s = lines.join("\n");
    if trailing_newline && !lines.is_empty() {
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::SessionTools;

    #[tokio::test]
    async fn add_update_move_delete() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("a.py"), "def f():\n    return 1\n\ndef g():\n    return 2\n").unwrap();
        std::fs::write(p.join("old.txt"), "bye\n").unwrap();
        let ctx = ToolCtx {
            session: Arc::new(SessionTools::new(p.to_owned())),
            cancel: Default::default(),
            output: Arc::new(|_: &str| {}),
            call_id: String::new(),
        };

        let patch = "*** Begin Patch\n\
*** Add File: new/b.txt\n+hello\n+world\n\
*** Update File: a.py\n*** Move to: c.py\n@@ def g():\n-    return 2\n+    return 3\n\
*** Delete File: old.txt\n\
*** End Patch";
        let r = ApplyPatch.call(&ctx, json!({"input": patch})).await;
        assert!(!r.is_error, "{r:?}");
        assert_eq!(std::fs::read_to_string(p.join("new/b.txt")).unwrap(), "hello\nworld\n");
        assert_eq!(std::fs::read_to_string(p.join("c.py")).unwrap(), "def f():\n    return 1\n\ndef g():\n    return 3\n");
        assert!(!p.join("a.py").exists() && !p.join("old.txt").exists());
    }

    #[tokio::test]
    async fn mismatch_leaves_files_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("x.txt"), "one\ntwo\n").unwrap();
        let ctx = ToolCtx {
            session: Arc::new(SessionTools::new(p.to_owned())),
            cancel: Default::default(),
            output: Arc::new(|_: &str| {}),
            call_id: String::new(),
        };
        let patch = "*** Begin Patch\n*** Add File: y.txt\n+y\n*** Update File: x.txt\n-three\n+3\n*** End Patch";
        let r = ApplyPatch.call(&ctx, json!({"input": patch})).await;
        assert!(r.is_error && r.content.contains("did not match"), "{r:?}");
        assert!(!p.join("y.txt").exists());
    }

    #[test]
    fn whitespace_tolerant_matching_and_eof() {
        let text = "a  \nb\nc\n";
        let hunks = vec![Hunk { old: vec!["a".into(), "b".into()], new: vec!["A".into(), "b".into()], ..Default::default() }];
        assert_eq!(apply_hunks(text, &hunks).unwrap(), "A\nb\nc\n");
        let eof = vec![Hunk { old: vec!["c".into()], new: vec!["c".into(), "d".into()], at_eof: true, ..Default::default() }];
        assert_eq!(apply_hunks(text, &eof).unwrap(), "a  \nb\nc\nd\n");
    }
}

//! grep and glob, built on ripgrep's crates (no external `rg` needed).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use c6_llm::ToolSpec;
use globset::{Glob as GlobPattern, GlobMatcher};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{Concurrency, Tool, ToolCtx, ToolOutput, parse_input};

const DEFAULT_HEAD_LIMIT: usize = 250;
const GLOB_LIMIT: usize = 200;

pub struct Grep;

#[derive(Deserialize)]
struct GrepInput {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    output_mode: OutputMode,
    context: Option<usize>,
    head_limit: Option<usize>,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    #[default]
    FilesWithMatches,
    Content,
    Count,
}

#[async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Search file contents with a regular expression (Rust regex syntax), respecting .gitignore and \
                          skipping hidden and binary files. output_mode: files_with_matches (default) lists matching \
                          files, content shows matching lines with line numbers, count shows matches per file."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression"},
                    "path": {"type": "string", "description": "File or directory to search (default: working directory)"},
                    "glob": {"type": "string", "description": "Only search files matching this glob, e.g. \"*.rs\" or \"src/**/*.ts\""},
                    "case_insensitive": {"type": "boolean"},
                    "output_mode": {"type": "string", "enum": ["files_with_matches", "content", "count"]},
                    "context": {"type": "integer", "minimum": 0, "description": "Lines of context around matches (content mode)"},
                    "head_limit": {"type": "integer", "minimum": 1, "description": "Maximum lines or files to return (default 250)"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let input: GrepInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let cwd = ctx.session.cwd();
        let root = input.path.as_deref().map(|p| ctx.session.resolve(p)).unwrap_or_else(|| cwd.clone());
        let cancel = ctx.cancel.clone();
        tokio::task::spawn_blocking(move || grep(&input, &root, &cwd, &cancel))
            .await
            .unwrap_or_else(|e| ToolOutput::err(format!("grep failed: {e}")))
    }
}

fn grep(input: &GrepInput, root: &Path, cwd: &Path, cancel: &CancellationToken) -> ToolOutput {
    let matcher =
        match RegexMatcherBuilder::new().case_insensitive(input.case_insensitive).line_terminator(Some(b'\n')).build(&input.pattern) {
            Ok(m) => m,
            Err(e) => return ToolOutput::err(format!("invalid regex: {e}")),
        };
    let filter = match input.glob.as_deref().map(glob_matcher).transpose() {
        Ok(f) => f,
        Err(e) => return ToolOutput::err(e),
    };
    let context = if input.output_mode == OutputMode::Content { input.context.unwrap_or(0) } else { 0 };
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(0))
        .before_context(context)
        .after_context(context)
        .build();
    let limit = input.head_limit.unwrap_or(DEFAULT_HEAD_LIMIT);

    let mut lines: Vec<String> = Vec::new();
    let mut files = 0usize;
    let mut hit_limit = false;
    for entry in WalkBuilder::new(root).build().flatten() {
        if cancel.is_cancelled() {
            return ToolOutput::err("interrupted");
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Some(f) = &filter
            && !f.is_match(path.strip_prefix(root).unwrap_or(path))
            && !path.file_name().is_some_and(|n| f.is_match(n))
        {
            continue;
        }
        let shown = display_path(path, cwd);
        let mut sink = Collect { mode: input.output_mode, path: &shown, out: Vec::new(), count: 0, last_line: None };
        if searcher.search_path(&matcher, path, &mut sink).is_err() || sink.count == 0 {
            continue;
        }
        files += 1;
        match input.output_mode {
            OutputMode::FilesWithMatches => lines.push(shown),
            OutputMode::Count => lines.push(format!("{shown}:{}", sink.count)),
            OutputMode::Content => lines.extend(sink.out),
        }
        if lines.len() >= limit {
            lines.truncate(limit);
            hit_limit = true;
            break;
        }
    }
    if lines.is_empty() {
        return ToolOutput::ok("No matches found");
    }
    let mut out = lines.join("\n");
    if hit_limit {
        out.push_str(&format!("\n\n(results truncated at {limit}; narrow the search or raise head_limit)"));
    } else if input.output_mode != OutputMode::FilesWithMatches {
        out.push_str(&format!("\n\n({files} file{})", if files == 1 { "" } else { "s" }));
    }
    ToolOutput::limited(out, false)
}

struct Collect<'a> {
    mode: OutputMode,
    path: &'a str,
    out: Vec<String>,
    count: usize,
    last_line: Option<u64>,
}

impl Collect<'_> {
    fn line(&mut self, n: Option<u64>, sep: char, bytes: &[u8]) {
        if let (Some(last), Some(n)) = (self.last_line, n)
            && n > last + 1
        {
            self.out.push("--".into());
        }
        self.last_line = n;
        let text = String::from_utf8_lossy(bytes);
        self.out.push(format!("{}{sep}{}{sep}{}", self.path, n.unwrap_or(0), text.trim_end_matches(['\n', '\r'])));
    }
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.count += 1;
        match self.mode {
            OutputMode::FilesWithMatches => return Ok(false), // one hit is enough
            OutputMode::Content => self.line(m.line_number(), ':', m.bytes()),
            OutputMode::Count => {}
        }
        Ok(true)
    }

    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> Result<bool, Self::Error> {
        self.line(c.line_number(), '-', c.bytes());
        Ok(true)
    }
}

pub struct Glob;

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
    path: Option<String>,
}

#[async_trait]
impl Tool for Glob {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "Find files by glob pattern (e.g. \"**/*.rs\", \"src/**/test_*.py\"), respecting .gitignore. \
                          Returns paths sorted by modification time, newest first."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern relative to path"},
                    "path": {"type": "string", "description": "Directory to search (default: working directory)"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let input: GlobInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let cwd = ctx.session.cwd();
        let root = input.path.as_deref().map(|p| ctx.session.resolve(p)).unwrap_or_else(|| cwd.clone());
        let cancel = ctx.cancel.clone();
        tokio::task::spawn_blocking(move || glob(&input.pattern, &root, &cwd, &cancel))
            .await
            .unwrap_or_else(|e| ToolOutput::err(format!("glob failed: {e}")))
    }
}

fn glob(pattern: &str, root: &Path, cwd: &Path, cancel: &CancellationToken) -> ToolOutput {
    let matcher = match glob_matcher(pattern) {
        Ok(m) => m,
        Err(e) => return ToolOutput::err(e),
    };
    let mut hits: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in WalkBuilder::new(root).build().flatten() {
        if cancel.is_cancelled() {
            return ToolOutput::err("interrupted");
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if matcher.is_match(rel) {
            let mtime = entry.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
            hits.push((mtime, entry.into_path()));
        }
    }
    if hits.is_empty() {
        return ToolOutput::ok("No files found");
    }
    hits.sort_by_key(|h| std::cmp::Reverse(h.0));
    let total = hits.len();
    let mut out: Vec<String> = hits.iter().take(GLOB_LIMIT).map(|(_, p)| display_path(p, cwd)).collect();
    if total > GLOB_LIMIT {
        out.push(format!("\n({total} files matched; showing the {GLOB_LIMIT} most recently modified)"));
    }
    ToolOutput::ok(out.join("\n"))
}

fn glob_matcher(pattern: &str) -> Result<GlobMatcher, String> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .or_else(|_| GlobPattern::new(pattern))
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("invalid glob: {e}"))
}

/// Paths under the cwd are shown relative to it; others absolute.
fn display_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::SessionTools;

    fn setup() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join("src/deep")).unwrap();
        std::fs::write(p.join("src/a.rs"), "fn main() {\n    let x = 1;\n    println!(\"Hello\");\n}\n").unwrap();
        std::fs::write(p.join("src/deep/b.rs"), "// hello there\n").unwrap();
        std::fs::write(p.join("notes.md"), "hello md\n").unwrap();
        std::fs::write(p.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir(p.join("ignored")).unwrap();
        std::fs::write(p.join("ignored/c.rs"), "hello\n").unwrap();
        // ignore only honours .gitignore inside a git repo.
        std::fs::create_dir(p.join(".git")).unwrap();
        let ctx = ToolCtx {
            session: Arc::new(SessionTools::new(p.to_owned())),
            cancel: Default::default(),
            output: Arc::new(|_: &str| {}),
            call_id: String::new(),
        };
        (dir, ctx)
    }

    #[tokio::test]
    async fn grep_modes_and_filters() {
        let (_d, ctx) = setup();
        let r = Grep.call(&ctx, json!({"pattern": "hello", "case_insensitive": true})).await;
        let mut files: Vec<&str> = r.content.lines().collect();
        files.sort();
        assert_eq!(files, vec!["notes.md", "src/a.rs", "src/deep/b.rs"]);

        let r = Grep.call(&ctx, json!({"pattern": "Hello", "glob": "*.rs", "output_mode": "content", "context": 1})).await;
        assert!(r.content.starts_with("src/a.rs-2-    let x = 1;\nsrc/a.rs:3:    println!(\"Hello\");\nsrc/a.rs-4-}"), "{}", r.content);

        let r = Grep.call(&ctx, json!({"pattern": "nomatch"})).await;
        assert_eq!(r.content, "No matches found");
        let r = Grep.call(&ctx, json!({"pattern": "("})).await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn glob_respects_gitignore() {
        let (_d, ctx) = setup();
        let r = Glob.call(&ctx, json!({"pattern": "**/*.rs"})).await;
        let mut files: Vec<&str> = r.content.lines().collect();
        files.sort();
        assert_eq!(files, vec!["src/a.rs", "src/deep/b.rs"]);
        let r = Glob.call(&ctx, json!({"pattern": "*.rs", "path": "src"})).await;
        assert_eq!(r.content, "src/a.rs");
    }
}

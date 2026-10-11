//! Slash commands: a message starting with `/name` runs a command.
//!
//! Custom commands are Markdown files, as in Claude Code: `.claude/commands`
//! in the repository (from its root down to the session's folder; deeper
//! ones win) and in `~/.claude/commands`. A file's path names it
//! (`frontend/component.md` is `/frontend:component`), optional frontmatter
//! gives `description` and `argument-hint`, and the body is the prompt:
//! `$ARGUMENTS` and `$1`…`$9` are replaced by the arguments, `` !`cmd` ``
//! by that shell command's output, and `@path` attaches the file.
//!
//! Built in: `/compact [focus]` summarizes the conversation, `/init` writes
//! AGENTS.md, `/review` reviews the changes. (`/clear` and `/export` are
//! the UI's.) Expansion happens when the message is received, so the log
//! records what the model was given.

use std::path::{Path, PathBuf};
use std::time::Duration;

use c6_agent::PendingInput;
use serde::Serialize;

const SHELL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SHELL_OUTPUT: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
    /// "built-in", "project" or "user".
    pub source: &'static str,
    #[serde(skip)]
    pub body: String,
}

const INIT: &str = "Analyze this codebase and write an AGENTS.md file at the repository root for future coding agents working in it. \
If an AGENTS.md or CLAUDE.md already exists, improve it rather than starting over.

Include:
- The commands that build, lint and test the project, including how to run a single test.
- The high-level architecture: the big picture that takes reading several files to understand, not a listing of every file.
- Conventions and gotchas that are not obvious from the code (and nothing that is).

Base it on the project's actual files (README, build configuration, CI, existing agent or editor rules). Keep it short; do not \
invent practices that the project does not follow. $ARGUMENTS";

const REVIEW: &str = "Review the code changes. $ARGUMENTS

If no changes were named above, review the uncommitted changes (`git status`, `git diff HEAD`); if there are none, the commits on \
this branch that are not on the default branch.

Look for bugs, security problems, missing error handling, missing tests and confusing code. Read enough of the surrounding code \
to be sure of each finding. Report findings most severe first, each with the file and line, what goes wrong and when, and a \
suggested fix. Do not change any files.";

fn built_ins() -> Vec<Command> {
    let c = |name: &str, description: &str, argument_hint: &str, body: &str| Command {
        name: name.into(),
        description: description.into(),
        argument_hint: argument_hint.into(),
        source: "built-in",
        body: body.into(),
    };
    vec![
        c("compact", "Summarize the conversation so far to free context", "[what to focus on]", ""),
        c("init", "Write an AGENTS.md describing this codebase", "", INIT),
        c("review", "Review the changes (uncommitted, or a branch or PR)", "[what to review]", REVIEW),
    ]
}

/// The commands available in `cwd`: built-ins, then the user's, then the
/// project's (each replacing earlier ones of the same name).
pub fn discover(cwd: &Path) -> Vec<Command> {
    let mut out = built_ins();
    let mut add = |found: Vec<Command>| {
        for c in found {
            out.retain(|k| k.name != c.name);
            out.push(c);
        }
    };
    if let Some(home) = std::env::var_os("HOME") {
        add(from_dir(&PathBuf::from(home).join(".claude/commands"), "user"));
    }
    let root = cwd.ancestors().find(|d| d.join(".git").exists()).unwrap_or(cwd);
    let mut dirs: Vec<&Path> = cwd.ancestors().take_while(|d| d.starts_with(root)).collect();
    dirs.reverse(); // outermost first, so deeper folders win
    for d in dirs {
        add(from_dir(&d.join(".claude/commands"), "project"));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn from_dir(dir: &Path, source: &'static str) -> Vec<Command> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(dir).standard_filters(false).max_depth(Some(4)).build();
    for e in walker.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "md") || !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = path.strip_prefix(dir) else { continue };
        let name = rel.with_extension("").components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join(":");
        if let Ok(text) = std::fs::read_to_string(path) {
            out.push(parse(&name, &text, source));
        }
    }
    out
}

fn parse(name: &str, text: &str, source: &'static str) -> Command {
    let (front, body) = match text.strip_prefix("---").and_then(|r| r.split_once("\n---")) {
        Some((front, body)) => (front, body.trim_start_matches(['-', '\n', '\r'])),
        None => ("", text),
    };
    let field = |k: &str| {
        front.lines().find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix(':')).map(|v| v.trim().trim_matches('"').to_owned()))
    };
    let body = body.trim().to_owned();
    let description = field("description").filter(|d| !d.is_empty()).unwrap_or_else(|| {
        body.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim_start_matches('#').trim().chars().take(100).collect()
    });
    Command { name: name.into(), description, argument_hint: field("argument-hint").unwrap_or_default(), source, body }
}

/// If the message invokes a known command, records which and sets what the
/// model gets. Unknown commands are left as text.
pub async fn expand(cwd: &Path, input: &mut PendingInput) {
    let Some(rest) = input.text.strip_prefix('/') else { return };
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let args = args.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || "_-:.".contains(c)) {
        return;
    }
    let Some(cmd) = discover(cwd).into_iter().find(|c| c.name == name) else { return };
    input.command = cmd.name.clone();
    if cmd.source == "built-in" && cmd.name == "compact" {
        return; // the focus stays in the text
    }
    input.prompt = substitute(&cmd.body, args, cwd).await;
}

async fn substitute(body: &str, args: &str, cwd: &Path) -> String {
    let positional = split_args(args);
    let mut out = String::with_capacity(body.len());
    let mut chars = body.char_indices().peekable();
    let mut used_arguments = false;
    while let Some((i, c)) = chars.next() {
        if c == '$' {
            if body[i + 1..].starts_with("ARGUMENTS") {
                out.push_str(args);
                used_arguments = true;
                for _ in 0.."ARGUMENTS".len() {
                    chars.next();
                }
                continue;
            }
            if let Some(&(_, d)) = chars.peek()
                && let Some(n) = d.to_digit(10).filter(|n| *n > 0)
            {
                chars.next();
                out.push_str(positional.get(n as usize - 1).map(String::as_str).unwrap_or(""));
                used_arguments = true;
                continue;
            }
        }
        if c == '!' && body[i + 1..].starts_with('`') {
            let start = i + 2;
            if let Some(len) = body[start..].find('`') {
                out.push_str(&shell(&body[start..start + len], cwd).await);
                while chars.peek().is_some_and(|&(j, _)| j <= start + len) {
                    chars.next();
                }
                continue;
            }
        }
        out.push(c);
    }
    // As in Claude Code: arguments the body does not place are appended.
    if !used_arguments && !args.is_empty() {
        out.push_str(&format!("\n\nARGUMENTS: {args}"));
    }
    out.trim().to_owned()
}

/// Whitespace-separated arguments; double or single quotes group words.
fn split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    let mut any = false;
    for c in args.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            (_, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

async fn shell(cmd: &str, cwd: &Path) -> String {
    let run = tokio::process::Command::new("sh").arg("-c").arg(cmd).current_dir(cwd).kill_on_drop(true).output();
    let text = match tokio::time::timeout(SHELL_TIMEOUT, run).await {
        Err(_) => format!("(`{cmd}` timed out)"),
        Ok(Err(e)) => format!("(`{cmd}` could not run: {e})"),
        Ok(Ok(o)) => {
            let mut t = String::from_utf8_lossy(&o.stdout).into_owned();
            t.push_str(&String::from_utf8_lossy(&o.stderr));
            if !o.status.success() {
                t.push_str(&format!("\n(exit status {})", o.status.code().unwrap_or(-1)));
            }
            t
        }
    };
    let mut t = text.trim_end().to_owned();
    if t.len() > MAX_SHELL_OUTPUT {
        let mut cut = MAX_SHELL_OUTPUT;
        while !t.is_char_boundary(cut) {
            cut -= 1;
        }
        t.truncate(cut);
        t.push_str("\n… (truncated)");
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn discovers_and_expands_commands() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::create_dir_all(d.join(".claude/commands/frontend")).unwrap();
        std::fs::create_dir_all(d.join("pkg/.claude/commands")).unwrap();
        std::fs::write(
            d.join(".claude/commands/fix.md"),
            "---\ndescription: Fix an issue\nargument-hint: <issue> [priority]\n---\nFix issue #$1 with priority $2. Branch: !`echo main`",
        )
        .unwrap();
        std::fs::write(d.join(".claude/commands/frontend/component.md"), "# Make a component\nCreate it.").unwrap();
        std::fs::write(d.join(".claude/commands/review.md"), "Our own review.").unwrap();
        std::fs::write(d.join("pkg/.claude/commands/fix.md"), "Fix it the pkg way: $ARGUMENTS").unwrap();

        let found = discover(d);
        let get = |n: &str| found.iter().find(|c| c.name == n).unwrap().clone();
        assert_eq!((get("fix").description.as_str(), get("fix").argument_hint.as_str()), ("Fix an issue", "<issue> [priority]"));
        assert_eq!(get("frontend:component").description, "Make a component");
        assert_eq!(get("review").source, "project", "a project command replaces the built-in");
        assert_eq!(get("init").source, "built-in");

        let mut input = PendingInput { text: "/fix 123 \"very high\"".into(), ..Default::default() };
        expand(d, &mut input).await;
        assert_eq!((input.command.as_str(), input.prompt.as_str()), ("fix", "Fix issue #123 with priority very high. Branch: main"));

        // Deeper folders win.
        let mut input = PendingInput { text: "/fix 7".into(), ..Default::default() };
        expand(&d.join("pkg"), &mut input).await;
        assert_eq!(input.prompt, "Fix it the pkg way: 7");

        // Arguments a body does not place are appended.
        let mut input = PendingInput { text: "/frontend:component a button".into(), ..Default::default() };
        expand(d, &mut input).await;
        assert_eq!(input.prompt, "# Make a component\nCreate it.\n\nARGUMENTS: a button");

        // /compact keeps its focus in the text; unknown commands and paths
        // are left alone.
        let mut input = PendingInput { text: "/compact the API work".into(), ..Default::default() };
        expand(d, &mut input).await;
        assert!(input.is_compact() && input.prompt.is_empty());
        for text in ["/nope x", "/usr/bin/env is a path", "no command"] {
            let mut input = PendingInput { text: text.into(), ..Default::default() };
            expand(d, &mut input).await;
            assert_eq!((input.command.as_str(), input.prompt.as_str()), ("", ""), "{text}");
        }
    }
}

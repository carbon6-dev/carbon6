//! Slash commands: a message starting with `/name` runs a command.
//!
//! Custom commands are kept in the head's settings; the head attaches the
//! template to the message (`UserInput.command_template`) when it is sent,
//! and the worker fills it in here: `$ARGUMENTS` and `$1`…`$9` take the
//! arguments, `` !`cmd` `` a shell command's output (run in the session's
//! folder), and `@path` attaches the file (see `mentions`).
//!
//! Built in: `/compact [focus]` summarizes the conversation, `/init` writes
//! AGENTS.md, `/review` reviews the changes. (`/clear` and `/export` are
//! the UI's.) Expansion happens when the message is received, so the log
//! records what the model was given.

use std::path::Path;
use std::time::Duration;

use c6_agent::PendingInput;

const SHELL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SHELL_OUTPUT: usize = 64 * 1024;

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

/// If the message invokes a custom command (its template came with it) or
/// a built-in one, records which and sets what the model gets. Unknown
/// commands are left as text.
pub async fn expand(cwd: &Path, input: &mut PendingInput) {
    let Some(rest) = input.text.strip_prefix('/') else { return };
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let args = args.trim().to_owned();
    if !input.template.is_empty() {
        input.prompt = substitute(&std::mem::take(&mut input.template), &args, cwd).await;
        return;
    }
    let body = match name {
        "compact" => {
            input.command = "compact".into();
            return; // the focus stays in the text
        }
        "init" => INIT,
        "review" => REVIEW,
        _ => return,
    };
    input.command = name.to_owned();
    input.prompt = substitute(body, &args, cwd).await;
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
    async fn expands_commands() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let custom = |text: &str, template: &str| PendingInput {
            text: text.into(),
            command: text[1..].split(' ').next().unwrap().into(),
            template: template.into(),
            ..Default::default()
        };

        let mut input = custom("/fix 123 \"very high\"", "Fix issue #$1 with priority $2. Branch: !`echo main`");
        expand(d, &mut input).await;
        assert_eq!((input.command.as_str(), input.prompt.as_str()), ("fix", "Fix issue #123 with priority very high. Branch: main"));
        assert!(input.template.is_empty(), "the template is not kept");

        // Arguments a template does not place are appended.
        let mut input = custom("/component a button", "# Make a component\nCreate it.");
        expand(d, &mut input).await;
        assert_eq!(input.prompt, "# Make a component\nCreate it.\n\nARGUMENTS: a button");

        // A custom command replaces a built-in of the same name.
        let mut input = custom("/review", "Our own review.");
        expand(d, &mut input).await;
        assert_eq!(input.prompt, "Our own review.");

        // Built-ins: /review expands; /compact keeps its focus in the text.
        let mut input = PendingInput { text: "/review the parser".into(), ..Default::default() };
        expand(d, &mut input).await;
        assert!(input.command == "review" && input.prompt.contains("Review the code changes. the parser"));
        let mut input = PendingInput { text: "/compact the API work".into(), ..Default::default() };
        expand(d, &mut input).await;
        assert!(input.is_compact() && input.prompt.is_empty());
        // Unknown commands and paths are left alone.
        for text in ["/nope x", "/usr/bin/env is a path", "no command"] {
            let mut input = PendingInput { text: text.into(), ..Default::default() };
            expand(d, &mut input).await;
            assert_eq!((input.command.as_str(), input.prompt.as_str()), ("", ""), "{text}");
        }
    }
}

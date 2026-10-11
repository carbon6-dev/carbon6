//! A session as Markdown (`/export`): the conversation as the model last
//! saw it (rewound turns left out), with tool calls folded.

use c6_proto::{SessionEvent, content_block, session_event::Kind};

use crate::store::SessionRow;

const MAX_OUTPUT: usize = 4000;

pub fn markdown(session: &SessionRow, events: &[SessionEvent]) -> String {
    let mut out = format!("# {}\n\n", session.title);
    out.push_str(&format!("- Folder: `{}`\n- Model: `{}`\n- Session: `{}`\n", session.cwd, session.model, session.id));
    let mut tools = std::collections::HashMap::new();
    for ev in &c6_agent::effective_events(events) {
        match &ev.kind {
            Some(Kind::User(u)) if u.command == "task-notification" || u.command == "stop-hook" => {
                let what = if u.command == "stop-hook" { "Stop hook" } else { "Background task ended" };
                out.push_str(&format!("\n> **{what}**\n>\n{}\n", u.text.lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n")))
            }
            Some(Kind::User(u)) if !u.blocked.is_empty() => {
                out.push_str(&format!("\n## You\n\n~~{}~~\n\n> Blocked by a hook: {}\n", u.text, u.blocked))
            }
            Some(Kind::User(u)) if u.prompt.is_empty() => out.push_str(&format!("\n## You\n\n{}\n", u.text)),
            Some(Kind::User(u)) => {
                // A slash command's expansion, or context a hook added.
                let what = if u.command.is_empty() { "Sent to the model as".to_owned() } else { format!("Expanded /{}", u.command) };
                out.push_str(&format!("\n## You\n\n{}\n\n<details><summary>{what}</summary>\n\n{}\n\n</details>\n", u.text, u.prompt))
            }
            Some(Kind::Assistant(a)) => {
                let mut said = false;
                for b in &a.blocks {
                    match &b.block {
                        Some(content_block::Block::Text(t)) if !t.trim().is_empty() => {
                            if !said {
                                out.push_str("\n## Assistant\n");
                                said = true;
                            }
                            out.push_str(&format!("\n{}\n", t.trim_end()));
                        }
                        Some(content_block::Block::ToolUse(u)) => {
                            tools.insert(u.id.clone(), (u.name.clone(), u.input_json.clone()));
                        }
                        _ => {}
                    }
                }
            }
            Some(Kind::ToolResult(r)) => {
                let (name, input) = tools.remove(&r.call_id).unwrap_or_else(|| (r.tool.clone(), String::new()));
                let output = clip(&r.output);
                let fence = if output.contains("```") { "````" } else { "```" };
                out.push_str(&format!(
                    "\n<details><summary>{}{name}</summary>\n\n```json\n{input}\n```\n\n{fence}\n{output}\n{fence}\n\n</details>\n",
                    if r.is_error { "✗ " } else { "" },
                ));
            }
            Some(Kind::Compaction(c)) => {
                out.push_str(&format!("\n## Summary of the earlier conversation\n\n{}\n", c.summary.trim_end()));
            }
            Some(Kind::Error(e)) => out.push_str(&format!("\n> **Error:** {}\n", e.message)),
            _ => {}
        }
    }
    out
}

fn clip(s: &str) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.trim_end().to_owned();
    }
    let mut cut = MAX_OUTPUT;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… ({} more bytes)", &s[..cut], s.len() - cut)
}

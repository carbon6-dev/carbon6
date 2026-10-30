//! Tool lists shared by routing rules and named agents.

/// Whether a list allows a tool, in any profile's naming; empty: all.
pub fn tool_allowed(tools: &[String], name: &str) -> bool {
    tools.is_empty() || tools.iter().any(|t| canonical(t) == canonical(name))
}

fn canonical(name: &str) -> String {
    // MCP names identify a server and tool exactly, including case.
    if name.starts_with("mcp__") {
        return name.to_owned();
    }
    let name = name.to_ascii_lowercase().replace(['_', '-'], "");
    match name.as_str() {
        "readfile" => "read".into(),
        "shell" => "bash".into(),
        "applypatch" | "multiedit" => "edit".into(),
        "updateplan" => "todowrite".into(),
        "loadskill" => "skill".into(),
        _ => name,
    }
}

/// MCP tools are discovered at runtime; settings accept their full names.
pub fn is_mcp_tool_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("mcp__") else { return false };
    name.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        && rest.as_bytes().windows(2).enumerate().any(|(i, pair)| i > 0 && i + 2 < rest.len() && pair == b"__")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_lists_match_profiles_and_exact_mcp_names() {
        for aliases in [
            &["read", "Read", "read_file"][..],
            &["bash", "Bash", "shell"],
            &["edit", "Edit", "apply_patch", "MultiEdit"],
            &["update_plan", "TodoWrite"],
            &["skill", "Skill", "load_skill"],
            &["memex_search", "MemexSearch"],
        ] {
            for a in aliases {
                for b in aliases {
                    assert!(tool_allowed(&[(*a).into()], b), "{a} / {b}");
                }
            }
        }
        assert!(tool_allowed(&[], "anything"));
        assert!(!tool_allowed(&["read".into()], "write"));
        assert!(tool_allowed(&["mcp__Mail__read".into()], "mcp__Mail__read"));
        for other in ["mcp__mail__read", "mcp__Mail__Read", "mcp__Mail__r_ead"] {
            assert!(!tool_allowed(&["mcp__Mail__read".into()], other));
        }
        assert!(is_mcp_tool_name("mcp__mail__read_thread"));
        assert!(is_mcp_tool_name("mcp_____read_thread"));
        assert!(is_mcp_tool_name("mcp__mail____"));
        for invalid in ["mcp__mail", "mcp____read", "mcp__mail__", "mcp__mail__read thread"] {
            assert!(!is_mcp_tool_name(invalid), "{invalid}");
        }
    }
}

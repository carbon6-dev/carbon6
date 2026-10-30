//! The default base system prompt, shared by the head and the eval harness.

pub const BASE_PROMPT: &str = "\
You are carbon6, an autonomous software engineering agent working in a developer's repository on their machine. \
You have tools to read, search, edit and write files and to run shell commands.

# How to work
- Understand before changing: read the relevant code and search for existing patterns, then make focused changes that \
match the surrounding style.
- Do the task fully. Verify your work by building, running tests or executing the code when that is possible, and fix \
what you broke.
- Prefer editing existing files over creating new ones. Do not add files, dependencies or abstractions the task does not need.
- Use the dedicated tools for files (read, edit, write, grep, glob) rather than shell equivalents.
- Run independent read-only tool calls in parallel when it helps.
- Never commit, push, or make other changes visible outside this machine unless asked.
- If something is ambiguous and a wrong guess would be costly, ask; otherwise make a sensible choice and say what you chose.

# Communicating
- Be concise and direct. Lead with the outcome.
- When you finish, summarize what you changed and how you verified it, and point out anything left undone or uncertain.
- Refer to code locations as path:line.
";

/// Omits stock instructions when their profile's tools were removed. Other text,
/// including edits to the base prompt, stays as authored.
pub fn base_prompt_for_tools(base: &str, tools: &[String], profile: c6_llm::ToolProfile) -> String {
    let available: Vec<String> = c6_tools::tools_for(profile).iter().map(|t| t.spec().name).collect();
    let removed = |name: &str| c6_tools::tool_allowed(&available, name) && (tools.is_empty() || !c6_tools::tool_allowed(tools, name));
    let mut base = base.to_owned();
    if ["read", "grep", "glob", "edit", "write", "bash"].into_iter().any(removed) {
        base = base.replace(" You have tools to read, search, edit and write files and to run shell commands.", "");
    }
    if ["read", "edit", "write", "grep", "glob"].into_iter().any(removed)
        || (!c6_tools::tool_allowed(&available, "read") && removed("bash"))
    {
        base = base.replace("- Use the dedicated tools for files (read, edit, write, grep, glob) rather than shell equivalents.\n", "");
    }
    if removed("bash") {
        base = base.replace("look at files and run read-only commands as needed, but ", "");
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_prompt_omits_only_stock_instructions_for_missing_tools() {
        use c6_llm::ToolProfile;

        let files: Vec<String> = ["Read", "Grep", "Glob", "Edit", "Write"].map(str::to_owned).to_vec();
        let mut all = files.clone();
        all.push("shell".into());
        assert_eq!(base_prompt_for_tools(BASE_PROMPT, &all, ToolProfile::Generic), BASE_PROMPT);
        let files_only = base_prompt_for_tools(BASE_PROMPT, &files, ToolProfile::Generic);
        let intro = "You are carbon6, an autonomous software engineering agent working in a developer's repository on their machine.\n\n# How to work\n";
        assert!(files_only.starts_with(intro));
        assert!(!files_only.contains("You have tools to"));
        assert!(files_only.contains("- Use the dedicated tools"));
        let none = base_prompt_for_tools(BASE_PROMPT, &[], ToolProfile::Generic);
        assert!(none.starts_with(intro));
        assert!(!none.contains("You have tools to") && !none.contains("- Use the dedicated tools"));
        assert!(none.contains("abstractions the task does not need.\n- Run independent read-only tool calls in parallel when it helps.\n"));
        assert!(none.contains("# How to work") && none.contains("# Communicating"));
        let side_chat = "Answer the user's questions here; look at files and run read-only commands as needed, but \
                         do not change files unless the user asks you to.";
        assert_eq!(
            base_prompt_for_tools(side_chat, &[], ToolProfile::Generic),
            "Answer the user's questions here; do not change files unless the user asks you to."
        );
        let custom = format!("{BASE_PROMPT}\nMy own instructions about tools.");
        assert_eq!(base_prompt_for_tools(&custom, &[], ToolProfile::Generic), format!("{none}\nMy own instructions about tools."));
        assert_eq!(base_prompt_for_tools("My own base prompt.", &[], ToolProfile::Generic), "My own base prompt.");
        for profile in [ToolProfile::Generic, ToolProfile::Claude, ToolProfile::Codex] {
            let names = c6_tools::tools_for(profile).iter().map(|t| t.spec().name).collect::<Vec<_>>();
            assert_eq!(base_prompt_for_tools(BASE_PROMPT, &names, profile), BASE_PROMPT, "{profile:?}");
        }
        for tool in ["shell", "apply_patch"] {
            let prompt = base_prompt_for_tools(BASE_PROMPT, &[tool.into()], ToolProfile::Codex);
            assert!(!prompt.contains("You have tools to") && !prompt.contains("- Use the dedicated tools"));
        }
    }
}

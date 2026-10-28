//! What we know about models: context size, output cap, thinking style, and
//! which tool profile they were trained with. Matched by id prefix; unknown
//! models get conservative defaults.

use crate::Provider;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolProfile {
    /// Lowercase read/write/edit/bash/grep/glob.
    Generic,
    /// Claude Code tool names and shapes.
    Claude,
    /// Codex tool names and shapes (shell, apply_patch, update_plan).
    Codex,
}

impl ToolProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolProfile::Generic => "generic",
            ToolProfile::Claude => "claude",
            ToolProfile::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "claude" => ToolProfile::Claude,
            "codex" => ToolProfile::Codex,
            _ => ToolProfile::Generic,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Thinking {
    /// No thinking parameter is sent.
    None,
    /// `thinking: {type: "adaptive"}` with effort control.
    Adaptive,
    /// Older Anthropic models: `{type: "enabled", budget_tokens}`.
    Budget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub context_window: u64,
    pub max_output: u32,
    pub tool_profile: ToolProfile,
    pub thinking: Thinking,
    /// Effort to use when the session does not choose one.
    pub default_effort: Option<&'static str>,
}

pub fn model_info(provider: Provider, model: &str) -> ModelInfo {
    let m = model.to_ascii_lowercase();
    let m = m.rsplit('/').next().unwrap_or(&m); // gateway prefixes like "anthropic/"
    if m.starts_with("claude-haiku") {
        return ModelInfo {
            context_window: 200_000,
            max_output: 64_000,
            tool_profile: ToolProfile::Claude,
            thinking: Thinking::Budget,
            default_effort: None,
        };
    }
    if m.starts_with("claude-") {
        // Adaptive thinking (and the 1M window) starts with the 4.6 family;
        // anything not known to support it gets the conservative older settings.
        let adaptive = [
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-sonnet-4-6",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable",
            "claude-mythos",
        ]
        .iter()
        .any(|p| m.starts_with(p));
        let old = !adaptive;
        return ModelInfo {
            context_window: if old { 200_000 } else { 1_000_000 },
            max_output: if old { 64_000 } else { 128_000 },
            tool_profile: ToolProfile::Claude,
            thinking: if old { Thinking::Budget } else { Thinking::Adaptive },
            // Coding work benefits from high effort; some models default lower.
            default_effort: if old { None } else { Some("high") },
        };
    }
    // GPT-5 and later (gpt-6.1-sol, …): unknown, they'd get 128k and so
    // compact every few minutes.
    let gpt5_on = m.strip_prefix("gpt-").and_then(|v| v.split(['.', '-']).next()?.parse::<u32>().ok()).is_some_and(|v| v >= 5);
    if gpt5_on || m.starts_with("codex") || m.starts_with("o3") || m.starts_with("o4") {
        return ModelInfo {
            context_window: 272_000,
            max_output: 128_000,
            tool_profile: if provider == Provider::OpenAiResponses { ToolProfile::Codex } else { ToolProfile::Generic },
            thinking: Thinking::None,
            default_effort: Some("medium"),
        };
    }
    ModelInfo {
        context_window: 128_000,
        max_output: 32_000,
        tool_profile: ToolProfile::Generic,
        thinking: Thinking::None,
        default_effort: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families() {
        let opus = model_info(Provider::Anthropic, "claude-opus-5-5");
        assert_eq!((opus.tool_profile, opus.thinking, opus.context_window), (ToolProfile::Claude, Thinking::Adaptive, 1_000_000));
        assert_eq!(model_info(Provider::Anthropic, "claude-haiku-4-5").thinking, Thinking::Budget);
        assert_eq!(model_info(Provider::Anthropic, "claude-sonnet-4-5").thinking, Thinking::Budget);
        assert_eq!(model_info(Provider::Anthropic, "claude-fable-5-1").thinking, Thinking::Adaptive);
        assert_eq!(model_info(Provider::OpenAiResponses, "gpt-5.5-codex").tool_profile, ToolProfile::Codex);
        let sol = model_info(Provider::OpenAiResponses, "gpt-6.1-sol");
        assert_eq!((sol.context_window, sol.tool_profile), (272_000, ToolProfile::Codex));
        assert_eq!(model_info(Provider::OpenAiChat, "gpt-4o").context_window, 128_000);
        assert_eq!(model_info(Provider::OpenAiChat, "qwen3.6:27b").tool_profile, ToolProfile::Generic);
        assert_eq!(model_info(Provider::OpenAiChat, "anthropic/claude-sonnet-5-5").tool_profile, ToolProfile::Claude);
    }
}

//! Which harness-authored prompt surface a process sends.
//!
//! The lean surface is the default. `REBON_PROMPT_PROFILE=legacy` restores the
//! system prompt, runtime guidance, tool descriptions, tool schemas and tool
//! exposure that shipped before it, byte for byte, so the two can be compared
//! side by side. Any other value, or no value, is the lean surface.
//!
//! The variable is read on every call rather than cached: it is set once per
//! process by whoever launches it, and reading it again costs nothing next to
//! the request it shapes. Code that must not read the environment inside a
//! build (the prompt assembly) resolves it once up front and carries it.

/// The environment variable that selects the profile.
pub const PROMPT_PROFILE_ENV: &str = "REBON_PROMPT_PROFILE";

/// A prompt surface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PromptProfile {
    /// The rewritten surface: each rule once, terse tool definitions.
    #[default]
    Lean,
    /// The surface as it was before the rewrite.
    Legacy,
}

impl PromptProfile {
    /// The profile a raw variable value names. Only `legacy` (trimmed, any
    /// case) selects the old surface.
    pub fn from_value(value: Option<&str>) -> Self {
        match value {
            Some(value) if value.trim().eq_ignore_ascii_case("legacy") => Self::Legacy,
            _ => Self::Lean,
        }
    }

    /// The profile this process runs with.
    pub fn current() -> Self {
        Self::from_value(std::env::var(PROMPT_PROFILE_ENV).ok().as_deref())
    }

    pub fn is_legacy(self) -> bool {
        self == Self::Legacy
    }
}

/// `true` when this process sends the legacy prompt surface.
pub fn legacy_prompt_profile() -> bool {
    PromptProfile::current().is_legacy()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_legacy_selects_the_old_surface() {
        assert_eq!(PromptProfile::from_value(None), PromptProfile::Lean);
        assert_eq!(PromptProfile::from_value(Some("")), PromptProfile::Lean);
        assert_eq!(PromptProfile::from_value(Some("lean")), PromptProfile::Lean);
        assert_eq!(PromptProfile::from_value(Some("1")), PromptProfile::Lean);
        for spelling in ["legacy", " Legacy ", "LEGACY"] {
            assert_eq!(
                PromptProfile::from_value(Some(spelling)),
                PromptProfile::Legacy,
                "{spelling:?}"
            );
        }
    }
}

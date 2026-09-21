//! Agent validation rules.
//!
//! Validates custom agent identifiers, full agent definitions, and tool
//! references through two entry points:
//!
//! * [`validate_agent_type`] — returns `Some(error)` with the
//! first error encountered, or `None` if the type is valid.
//! * [`validate_agent`] — runs
//! every check, accumulates errors + warnings, and returns an
//! [`AgentValidationResult`].
//!
//! This module preserves the order in which errors are pushed:
//! the surface displays the first error first.

use crate::surface::types::{AgentSource, AgentSummary, AgentValidationResult};
use crate::surface::utils::{agent_source_display_name, AgentSourceFilter};

/// Reduced tool-resolution payload passed in by the consumer.
///
/// Only invalid tool names are needed by the validation rules modeled here.
/// Pinning this as a separate struct lets future fields be added without
/// changing the validate signature.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedTools {
    /// Tool names that were declared but don't exist in the registry.
    pub invalid_tools: Vec<String>,
}

/// The shape passed into [`validate_agent`].
/// Agent payload accepted by [`validate_agent`], excluding any storage location.
///
/// Differs from [`AgentSummary`] in that the `system_prompt` is what
/// the user is currently typing — it may not match an existing agent
/// on disk. The `source` is required so duplicate detection can
/// exclude self-edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDraft {
    /// Stable identifier (kebab-case).
    pub agent_type: String,
    /// Description of when to use the agent.
    pub when_to_use: String,
    /// Allowed tool names. `None` = "all tools".
    pub tools: Option<Vec<String>>,
    /// System prompt body (the resolved string, not a closure).
    pub system_prompt: String,
    /// Source the agent is being saved to. Used to exclude self-edits
    /// from the duplicate check.
    pub source: AgentSource,
}

/// Validate an agent type identifier. Returns `Some(error)` when invalid.
///
/// The format regex `^[a-zA-Z0-9][a-zA-Z0-9-]*[a-zA-Z0-9]$` is enforced literally
/// (note: this rules out single-character types). The length must be
/// at least 3 and at most 50 characters.
pub fn validate_agent_type(agent_type: &str) -> Option<&'static str> {
    if agent_type.is_empty() {
        return Some("Agent type is required");
    }

    if !is_valid_agent_type_format(agent_type) {
        return Some("Agent type must start and end with alphanumeric characters and contain only letters, numbers, and hyphens");
    }

    if agent_type.len() < 3 {
        return Some("Agent type must be at least 3 characters long");
    }

    if agent_type.len() > 50 {
        return Some("Agent type must be less than 50 characters");
    }

    None
}

/// Match `^[a-zA-Z0-9][a-zA-Z0-9-]*[a-zA-Z0-9]$`. Returns false for
/// single-character strings (the regex requires at least one anchor
/// alphanumeric + a final alphanumeric).
fn is_valid_agent_type_format(s: &str) -> bool {
    if s.len() < 2 {
        return false;
    }
    let bytes = s.as_bytes();
    if !is_alnum(bytes[0]) || !is_alnum(bytes[bytes.len() - 1]) {
        return false;
    }
    for &b in &bytes[1..bytes.len() - 1] {
        if !is_alnum(b) && b != b'-' {
            return false;
        }
    }
    true
}

fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

/// Run the full validation pass.
///
/// Validate a full custom agent definition. Errors and warnings are pushed in
/// deterministic order.
///
/// `existing_agents` is provided by the consumer for duplicate checks.
///
/// `resolved` is a pre-computed `ResolvedTools` struct containing tool
/// resolution results.
pub fn validate_agent(
    agent: &AgentDraft,
    resolved: &ResolvedTools,
    existing_agents: &[AgentSummary],
) -> AgentValidationResult {
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // 1. agent_type
    if agent.agent_type.is_empty() {
        errors.push("Agent type is required".to_string());
    } else {
        if let Some(type_err) = validate_agent_type(&agent.agent_type) {
            errors.push(type_err.to_string());
        }
        // Duplicate check (excludes self) — same `agent_type` and
        // different `source` ⇒ duplicate.
        if let Some(dup) = existing_agents
            .iter()
            .find(|a| a.agent_type == agent.agent_type && a.source != agent.source)
        {
            errors.push(format!(
                r#"Agent type "{}" already exists in {}"#,
                agent.agent_type,
                agent_source_display_name(&source_to_filter(&dup.source))
            ));
        }
    }

    // 2. when_to_use (description)
    if agent.when_to_use.is_empty() {
        errors.push("Description (description) is required".to_string());
    } else if agent.when_to_use.chars().count() < 10 {
        warnings
            .push("Description should be more descriptive (at least 10 characters)".to_string());
    } else if agent.when_to_use.chars().count() > 5000 {
        warnings.push("Description is very long (over 5000 characters)".to_string());
    }

    // 3. tools — `Option<Vec<String>>` is the only shape the type
    // allows, so the remaining warnings are:
    if agent.tools.is_none() {
        warnings.push("Agent has access to all tools".to_string());
    } else if let Some(t) = &agent.tools {
        if t.is_empty() {
            warnings
                .push("No tools selected - agent will have very limited capabilities".to_string());
        }
    }
    if !resolved.invalid_tools.is_empty() {
        errors.push(format!(
            "Invalid tools: {}",
            resolved.invalid_tools.join(", ")
        ));
    }

    // 4. system_prompt
    if agent.system_prompt.is_empty() {
        errors.push("System prompt is required".to_string());
    } else if agent.system_prompt.chars().count() < 20 {
        errors.push("System prompt is too short (minimum 20 characters)".to_string());
    } else if agent.system_prompt.chars().count() > 10_000 {
        warnings.push("System prompt is very long (over 10,000 characters)".to_string());
    }

    AgentValidationResult::new(errors, warnings)
}

/// Map an [`AgentSource`] to the matching display filter so the
/// duplicate-detection error uses the expected message exactly.
fn source_to_filter(s: &AgentSource) -> AgentSourceFilter {
    match s {
        AgentSource::BuiltIn => AgentSourceFilter::BuiltIn,
        AgentSource::Plugin { .. } => AgentSourceFilter::Plugin,
        AgentSource::Settings(ss) => AgentSourceFilter::Source(*ss),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::types::SettingSource;

    fn draft(agent_type: &str, when: &str, prompt: &str) -> AgentDraft {
        AgentDraft {
            agent_type: agent_type.to_string(),
            when_to_use: when.to_string(),
            tools: None,
            system_prompt: prompt.to_string(),
            source: AgentSource::Settings(SettingSource::UserSettings),
        }
    }

    // ---- validate_agent_type ----

    #[test]
    fn type_empty_is_required() {
        assert_eq!(validate_agent_type(""), Some("Agent type is required"));
    }

    #[test]
    fn type_with_underscore_rejected() {
        assert!(validate_agent_type("foo_bar")
            .unwrap()
            .contains("alphanumeric"));
    }

    #[test]
    fn type_starting_hyphen_rejected() {
        assert!(validate_agent_type("-foo")
            .unwrap()
            .contains("alphanumeric"));
    }

    #[test]
    fn type_ending_hyphen_rejected() {
        assert!(validate_agent_type("foo-")
            .unwrap()
            .contains("alphanumeric"));
    }

    #[test]
    fn type_uppercase_letters_allowed() {
        assert_eq!(validate_agent_type("FooBar"), None);
    }

    #[test]
    fn type_too_short_rejected() {
        assert_eq!(
            validate_agent_type("ab"),
            Some("Agent type must be at least 3 characters long")
        );
    }

    #[test]
    fn type_three_chars_ok() {
        assert_eq!(validate_agent_type("abc"), None);
    }

    #[test]
    fn type_fifty_chars_ok() {
        let s = "a".repeat(50);
        assert_eq!(validate_agent_type(&s), None);
    }

    #[test]
    fn type_fifty_one_chars_rejected() {
        let s = "a".repeat(51);
        assert_eq!(
            validate_agent_type(&s),
            Some("Agent type must be less than 50 characters")
        );
    }

    #[test]
    fn type_with_hyphen_in_middle_ok() {
        assert_eq!(validate_agent_type("foo-bar"), None);
    }

    #[test]
    fn type_with_double_hyphen_ok() {
        // The regex permits any number of consecutive hyphens in the middle.
        assert_eq!(validate_agent_type("foo--bar"), None);
    }

    #[test]
    fn type_single_char_rejected_by_regex() {
        // Single chars don't satisfy `start AND end alnum` of length >= 2.
        assert!(validate_agent_type("a").unwrap().contains("alphanumeric"));
    }

    // ---- validate_agent ----

    #[test]
    fn validate_happy_path() {
        let d = draft(
            "code-reviewer",
            "Use this agent when reviewing code carefully",
            "You are a code reviewer with deep expertise.",
        );
        let res = validate_agent(&d, &ResolvedTools::default(), &[]);
        assert!(res.is_valid);
        assert!(res.errors.is_empty());
        // The "all tools" warning is expected because tools is None.
        assert!(res
            .warnings
            .iter()
            .any(|w| w == "Agent has access to all tools"));
    }

    #[test]
    fn validate_missing_type() {
        let mut d = draft(
            "x",
            "Use this agent when reviewing code carefully",
            "You are an expert with twenty plus chars",
        );
        d.agent_type = String::new();
        let res = validate_agent(&d, &ResolvedTools::default(), &[]);
        assert!(!res.is_valid);
        assert!(res.errors.iter().any(|e| e == "Agent type is required"));
    }

    #[test]
    fn validate_invalid_type_format() {
        let d = draft(
            "bad_name",
            "Use this agent when reviewing code carefully",
            "You are an expert with twenty plus chars",
        );
        let res = validate_agent(&d, &ResolvedTools::default(), &[]);
        assert!(!res.is_valid);
        assert!(res.errors.iter().any(|e| e.contains("alphanumeric")));
    }

    #[test]
    fn validate_duplicate_in_other_source() {
        let d = draft(
            "code-reviewer",
            "Use this agent when reviewing code carefully",
            "You are an expert with twenty plus chars",
        );
        let existing = vec![AgentSummary::minimal(
            "code-reviewer",
            "old",
            "old prompt that is plenty long",
            AgentSource::Settings(SettingSource::ProjectSettings),
        )];
        let res = validate_agent(&d, &ResolvedTools::default(), &existing);
        assert!(!res.is_valid);
        assert!(res
            .errors
            .iter()
            .any(|e| e.contains("already exists in Project")));
    }

    #[test]
    fn validate_self_edit_not_duplicate() {
        let d = draft(
            "code-reviewer",
            "Use this agent when reviewing code carefully",
            "You are an expert with twenty plus chars",
        );
        let existing = vec![AgentSummary::minimal(
            "code-reviewer",
            "self",
            "self prompt that is plenty long",
            AgentSource::Settings(SettingSource::UserSettings),
        )];
        let res = validate_agent(&d, &ResolvedTools::default(), &existing);
        // Same type, same source ⇒ self edit, not flagged.
        assert!(!res.errors.iter().any(|e| e.contains("already exists")));
    }

    #[test]
    fn validate_missing_when_to_use() {
        let d = draft(
            "code-reviewer",
            "",
            "You are an expert with twenty plus chars",
        );
        let res = validate_agent(&d, &ResolvedTools::default(), &[]);
        assert!(res
            .errors
            .iter()
            .any(|e| e == "Description (description) is required"));
    }

    #[test]
    fn validate_short_when_to_use_warns() {
        let d = draft(
            "code-reviewer",
            "short",
            "You are an expert with twenty plus chars",
        );
        let res = validate_agent(&d, &ResolvedTools::default(), &[]);
        assert!(res.warnings.iter().any(|w| w.contains("more descriptive")));
    }

    #[test]
    fn validate_long_when_to_use_warns() {
        let long = "x".repeat(5001);
        let d = draft(
            "code-reviewer",
            &long,
            "You are an expert with twenty plus chars",
        );
        let res = validate_agent(&d, &ResolvedTools::default(), &[]);
        assert!(res.warnings.iter().any(|w| w.contains("very long")));
    }

    #[test]
    fn validate_empty_tools_list_warns() {
        let mut d = draft(
            "code-reviewer",
            "Use this agent when reviewing code carefully",
            "You are an expert with twenty plus chars",
        );
        d.tools = Some(vec![]);
        let res = validate_agent(&d, &ResolvedTools::default(), &[]);
        assert!(res
            .warnings
            .iter()
            .any(|w| w.contains("very limited capabilities")));
    }

    #[test]
    fn validate_invalid_tools_errors() {
        let mut d = draft(
            "code-reviewer",
            "Use this agent when reviewing code carefully",
            "You are an expert with twenty plus chars",
        );
        d.tools = Some(vec!["RealTool".to_string()]);
        let resolved = ResolvedTools {
            invalid_tools: vec!["NoSuchTool".into(), "Other".into()],
        };
        let res = validate_agent(&d, &resolved, &[]);
        assert!(res
            .errors
            .iter()
            .any(|e| e == "Invalid tools: NoSuchTool, Other"));
    }

    #[test]
    fn validate_missing_system_prompt() {
        let d = draft(
            "code-reviewer",
            "Use this agent when reviewing carefully",
            "",
        );
        let res = validate_agent(&d, &ResolvedTools::default(), &[]);
        assert!(res.errors.iter().any(|e| e == "System prompt is required"));
    }

    #[test]
    fn validate_short_system_prompt() {
        let d = draft(
            "code-reviewer",
            "Use this agent when reviewing carefully",
            "short",
        );
        let res = validate_agent(&d, &ResolvedTools::default(), &[]);
        assert!(res.errors.iter().any(|e| e.contains("too short")));
    }

    #[test]
    fn validate_long_system_prompt_warns() {
        let long = "x".repeat(10_001);
        let d = draft(
            "code-reviewer",
            "Use this agent when reviewing carefully",
            &long,
        );
        let res = validate_agent(&d, &ResolvedTools::default(), &[]);
        assert!(res.warnings.iter().any(|w| w.contains("very long")));
    }

    #[test]
    fn validate_error_order_is_deterministic() {
        // Empty everything: errors should appear in deterministic order:
        // 1. agent type required
        // 2. description required
        // 3. system prompt required
        let mut d = draft("", "", "");
        d.tools = Some(vec![]); // forces "no tools selected" warning
        let res = validate_agent(&d, &ResolvedTools::default(), &[]);
        assert_eq!(res.errors[0], "Agent type is required");
        assert_eq!(res.errors[1], "Description (description) is required");
        assert_eq!(res.errors[2], "System prompt is required");
    }

    #[test]
    fn validate_built_in_dup_message_uses_built_in_label() {
        let d = draft(
            "general-purpose",
            "Use this agent when general tasks needed",
            "You are a general purpose agent helper",
        );
        let existing = vec![AgentSummary::minimal(
            "general-purpose",
            "built in",
            "built in prompt that is plenty long",
            AgentSource::BuiltIn,
        )];
        let res = validate_agent(&d, &ResolvedTools::default(), &existing);
        assert!(res.errors.iter().any(|e| e.contains("Built-in agents")));
    }

    #[test]
    fn validate_plugin_dup_message_uses_plugin_label() {
        let d = draft(
            "plugin-agent",
            "Use this agent when plugin tasks happen",
            "You are a plugin assistant helper agent",
        );
        let existing = vec![AgentSummary::minimal(
            "plugin-agent",
            "from plugin",
            "from plugin prompt long enough",
            AgentSource::Plugin {
                plugin: "my".into(),
            },
        )];
        let res = validate_agent(&d, &ResolvedTools::default(), &existing);
        assert!(res.errors.iter().any(|e| e.contains("Plugin agents")));
    }
}

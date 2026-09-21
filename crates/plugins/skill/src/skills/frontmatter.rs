//! Parsed skill frontmatter types and field validation.
//!
//! This module works on **already-parsed** key-value maps (the YAML
//! parsing step lives in the I/O layer). It validates and coerces
//! raw frontmatter values into strongly-typed Rust structs.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Shell / effort enums
// ---------------------------------------------------------------------------

/// Shell to use for `!`cmd`` and ````! …```` blocks inside skill
/// markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontmatterShell {
    /// Bash (default on all platforms for skill portability).
    Bash,
    /// PowerShell.
    PowerShell,
}

/// Effort level for agent invocations — a named level *or* an integer
/// in 1..=10.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffortValue {
    /// Named level.
    Named(EffortLevel),
    /// Integer level (1–10).
    Integer(u8),
}

/// Named effort levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortLevel {
    /// Minimal effort.
    Low,
    /// Default.
    Medium,
    /// High quality.
    High,
    /// Extra-high effort.
    XHigh,
    /// Maximum effort (gpt-5.6+).
    Max,
}

// ---------------------------------------------------------------------------
// Parsed frontmatter
// ---------------------------------------------------------------------------

/// Validated, strongly-typed output of frontmatter parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSkillFrontmatter {
    /// Display name from `name` field (overrides directory name).
    pub display_name: Option<String>,
    /// Description (from `description` field or extracted from markdown).
    pub description: String,
    /// Whether the description was explicitly set in frontmatter.
    pub has_user_specified_description: bool,
    /// Tools the skill is allowed to call.
    pub allowed_tools: Vec<String>,
    /// Tools this turn must offer before the skill is worth naming to the
    /// model — a skill that only drives one tool is noise without it.
    pub required_tools: Vec<String>,
    /// Hint shown after the skill name.
    pub argument_hint: Option<String>,
    /// Named argument list parsed from `arguments` field.
    pub argument_names: Vec<String>,
    /// When the model should consider invoking this skill.
    pub when_to_use: Option<String>,
    /// Skill version.
    pub version: Option<String>,
    /// Model override. `None` means inherit from parent.
    pub model: Option<String>,
    /// Prevent model from auto-invoking this skill.
    pub disable_model_invocation: bool,
    /// Whether the user can type `/name` to invoke.
    pub user_invocable: bool,
    /// Execution context (`fork` or `None` for inline).
    pub execution_context: Option<super::bundled::ExecutionContext>,
    /// Agent type when execution_context is Fork.
    pub agent: Option<String>,
    /// Effort level for the agent.
    pub effort: Option<EffortValue>,
    /// Shell for inline `!` blocks.
    pub shell: Option<FrontmatterShell>,
    /// Opaque hooks configuration (transparent to this crate;
    /// the integration layer validates the structure).
    pub hooks: Option<HashMap<String, FrontmatterValue>>,
    /// Diagnostic warning when effort frontmatter is present but
    /// could not be parsed.
    pub effort_parse_warning: Option<String>,
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Raw frontmatter data — a string-keyed map of heterogeneous
/// values. The YAML layer produces this; we consume it.
pub type RawFrontmatter = HashMap<String, FrontmatterValue>;

/// A value that can appear in YAML frontmatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontmatterValue {
    /// A string.
    String(String),
    /// A boolean.
    Bool(bool),
    /// An integer.
    Integer(i64),
    /// A list of strings.
    StringList(Vec<String>),
    /// Null / absent.
    Null,
    /// Opaque structured data (e.g. hooks).
    Map(HashMap<String, FrontmatterValue>),
}

impl FrontmatterValue {
    /// Coerce to string if possible.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// Coerce to owned string.
    pub fn to_string_lossy(&self) -> Option<String> {
        match self {
            Self::String(s) => Some(s.clone()),
            Self::Bool(b) => Some(b.to_string()),
            Self::Integer(n) => Some(n.to_string()),
            _ => None,
        }
    }

    /// Coerce to string list (accepts both single string and list).
    pub fn to_string_list(&self) -> Vec<String> {
        match self {
            Self::String(s) => vec![s.clone()],
            Self::StringList(v) => v.clone(),
            _ => Vec::new(),
        }
    }
}

/// Parse a boolean-ish frontmatter value.
///
/// `true`/`yes`/`1` (case-insensitively) and any non-zero integer are
/// true; everything else is false.
pub fn parse_boolean(value: &FrontmatterValue) -> bool {
    match value {
        FrontmatterValue::Bool(b) => *b,
        FrontmatterValue::String(s) => {
            matches!(s.to_lowercase().as_str(), "true" | "yes" | "1")
        }
        FrontmatterValue::Integer(n) => *n != 0,
        _ => false,
    }
}

/// Parse shell from frontmatter: `bash`, or `powershell` / `pwsh`
/// (case-insensitively). Any other value yields `None`.
pub fn parse_shell(value: &FrontmatterValue) -> Option<FrontmatterShell> {
    let s = value.as_str()?;
    match s.to_lowercase().as_str() {
        "bash" => Some(FrontmatterShell::Bash),
        "powershell" | "pwsh" => Some(FrontmatterShell::PowerShell),
        _ => None,
    }
}

/// Parse effort value from frontmatter.
///
/// Named levels `low` / `medium` / `high` / `xhigh` / `max`
/// (case-insensitively), or an integer in 1..=10. Anything else is
/// `None`.
pub fn parse_effort(value: &FrontmatterValue) -> Option<EffortValue> {
    match value {
        FrontmatterValue::String(s) => match s.to_lowercase().as_str() {
            "low" => Some(EffortValue::Named(EffortLevel::Low)),
            "medium" => Some(EffortValue::Named(EffortLevel::Medium)),
            "high" => Some(EffortValue::Named(EffortLevel::High)),
            "xhigh" => Some(EffortValue::Named(EffortLevel::XHigh)),
            "max" => Some(EffortValue::Named(EffortLevel::Max)),
            other => other
                .parse::<u8>()
                .ok()
                .filter(|n| (1..=10).contains(n))
                .map(EffortValue::Integer),
        },
        FrontmatterValue::Integer(n) => {
            let n = *n as u8;
            if (1..=10).contains(&n) {
                Some(EffortValue::Integer(n))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Parse argument names from frontmatter `arguments` field.
///
/// Accepts either a space-separated string or a list of strings.
/// Filters out empty and numeric-only names.
pub fn parse_argument_names(value: &FrontmatterValue) -> Vec<String> {
    let raw = match value {
        FrontmatterValue::String(s) => s.split_whitespace().map(String::from).collect::<Vec<_>>(),
        FrontmatterValue::StringList(v) => v.clone(),
        _ => return Vec::new(),
    };

    raw.into_iter()
        .filter(|name| {
            let trimmed = name.trim();
            !trimmed.is_empty() && !trimmed.chars().all(|c| c.is_ascii_digit())
        })
        .collect()
}

/// Coerce a description value to a string, returning `None` if it
/// is null/absent/empty.
pub fn coerce_description(value: Option<&FrontmatterValue>) -> Option<String> {
    let v = value?;
    let s = v.to_string_lossy()?;
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Extract a short description from markdown content by taking the
/// first non-empty, non-heading line.
pub fn extract_description_from_markdown(content: &str, fallback_label: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Truncate at 200 chars to keep descriptions concise.
        let desc = if trimmed.chars().count() > 200 {
            let truncated: String = trimmed.chars().take(200).collect();
            format!("{truncated}…")
        } else {
            trimmed.to_string()
        };
        return desc;
    }
    format!("{fallback_label} command")
}

/// Parse `allowed-tools` from frontmatter.
///
/// Accepts a comma-separated string or a list of strings.
pub fn parse_allowed_tools(value: Option<&FrontmatterValue>) -> Vec<String> {
    let Some(v) = value else { return Vec::new() };
    match v {
        FrontmatterValue::String(s) => s
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        FrontmatterValue::StringList(list) => list
            .iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse `paths` frontmatter (glob patterns for conditional
/// activation).
///
/// A trailing `/**` is stripped and the result is `None` when there
/// are no patterns or when every pattern is `**`.
pub fn parse_skill_paths(value: Option<&FrontmatterValue>) -> Option<Vec<String>> {
    let v = value?;
    let raw: Vec<String> = match v {
        FrontmatterValue::String(s) => s.split(',').map(|p| p.trim().to_string()).collect(),
        FrontmatterValue::StringList(list) => list.clone(),
        _ => return None,
    };

    let patterns: Vec<String> = raw
        .into_iter()
        .map(|p| {
            if p.ends_with("/**") {
                p[..p.len() - 3].to_string()
            } else {
                p
            }
        })
        .filter(|p| !p.is_empty())
        .collect();

    // If all patterns are `**` (match-all), treat as no paths.
    if patterns.is_empty() || patterns.iter().all(|p| p == "**") {
        return None;
    }

    Some(patterns)
}

/// Validate and normalize a user-specified model name.
///
/// Returns `None` for empty or whitespace-only strings.
/// Trims leading/trailing whitespace and lower-cases the name
/// so downstream mapping to concrete model IDs is
/// case-insensitive.
pub fn parse_user_specified_model(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_lowercase())
}

/// Parse all shared frontmatter fields for a skill.
///
/// `resolved_name` is the final skill name used for logging.
/// `markdown_content` is the body after frontmatter stripping.
/// `description_fallback_label` is `"Skill"` or `"Custom command"`.
pub fn parse_skill_frontmatter_fields(
    frontmatter: &RawFrontmatter,
    markdown_content: &str,
    resolved_name: &str,
    description_fallback_label: &str,
) -> ParsedSkillFrontmatter {
    let validated_description = coerce_description(frontmatter.get("description"));
    let description = validated_description.clone().unwrap_or_else(|| {
        extract_description_from_markdown(markdown_content, description_fallback_label)
    });

    let user_invocable = frontmatter
        .get("user-invocable")
        .map(parse_boolean)
        .unwrap_or(true);

    let model = frontmatter
        .get("model")
        .and_then(|v| v.as_str())
        .and_then(|s| {
            if s == "inherit" {
                None
            } else {
                parse_user_specified_model(s)
            }
        });

    let effort_raw = frontmatter.get("effort");
    let effort = effort_raw.and_then(parse_effort);
    let effort_parse_warning = if effort_raw.is_some() && effort.is_none() {
        Some(format!(
            "Skill {} has invalid effort '{}'. Valid options: max, xhigh, high, medium, low or an integer 1-10",
            resolved_name,
            effort_raw.and_then(|v| v.to_string_lossy()).unwrap_or_default()
        ))
    } else {
        None
    };

    let execution_context = frontmatter.get("context").and_then(|v| {
        if v.as_str() == Some("fork") {
            Some(super::bundled::ExecutionContext::Fork)
        } else {
            None
        }
    });

    let shell = frontmatter.get("shell").and_then(parse_shell);

    // Hooks are kept opaque here; the integration layer validates the
    // structure.
    let hooks = frontmatter.get("hooks").and_then(|v| match v {
        FrontmatterValue::Map(map) => Some(map.clone()),
        _ => None,
    });

    ParsedSkillFrontmatter {
        display_name: frontmatter.get("name").and_then(|v| v.to_string_lossy()),
        description,
        has_user_specified_description: validated_description.is_some(),
        allowed_tools: parse_allowed_tools(frontmatter.get("allowed-tools")),
        required_tools: parse_allowed_tools(frontmatter.get("required-tools")),
        argument_hint: frontmatter
            .get("argument-hint")
            .and_then(|v| v.to_string_lossy()),
        argument_names: frontmatter
            .get("arguments")
            .map(parse_argument_names)
            .unwrap_or_default(),
        when_to_use: frontmatter
            .get("when_to_use")
            .and_then(|v| v.as_str())
            .map(String::from),
        version: frontmatter
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from),
        model,
        disable_model_invocation: frontmatter
            .get("disable-model-invocation")
            .map(parse_boolean)
            .unwrap_or(false),
        user_invocable,
        execution_context,
        agent: frontmatter
            .get("agent")
            .and_then(|v| v.as_str())
            .map(String::from),
        effort,
        shell,
        hooks,
        effort_parse_warning,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fm(pairs: &[(&str, FrontmatterValue)]) -> RawFrontmatter {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn parse_boolean_variants() {
        assert!(parse_boolean(&FrontmatterValue::Bool(true)));
        assert!(!parse_boolean(&FrontmatterValue::Bool(false)));
        assert!(parse_boolean(&FrontmatterValue::String("true".into())));
        assert!(parse_boolean(&FrontmatterValue::String("yes".into())));
        assert!(parse_boolean(&FrontmatterValue::String("1".into())));
        assert!(!parse_boolean(&FrontmatterValue::String("false".into())));
        assert!(!parse_boolean(&FrontmatterValue::String("no".into())));
        assert!(parse_boolean(&FrontmatterValue::Integer(1)));
        assert!(!parse_boolean(&FrontmatterValue::Integer(0)));
        assert!(!parse_boolean(&FrontmatterValue::Null));
    }

    #[test]
    fn parse_shell_variants() {
        assert_eq!(
            parse_shell(&FrontmatterValue::String("bash".into())),
            Some(FrontmatterShell::Bash),
        );
        assert_eq!(
            parse_shell(&FrontmatterValue::String("PowerShell".into())),
            Some(FrontmatterShell::PowerShell),
        );
        assert_eq!(
            parse_shell(&FrontmatterValue::String("pwsh".into())),
            Some(FrontmatterShell::PowerShell),
        );
        assert_eq!(parse_shell(&FrontmatterValue::String("zsh".into())), None,);
    }

    #[test]
    fn parse_effort_named_and_integer() {
        assert_eq!(
            parse_effort(&FrontmatterValue::String("low".into())),
            Some(EffortValue::Named(EffortLevel::Low)),
        );
        assert_eq!(
            parse_effort(&FrontmatterValue::String("xhigh".into())),
            Some(EffortValue::Named(EffortLevel::XHigh)),
        );
        assert_eq!(
            parse_effort(&FrontmatterValue::String("5".into())),
            Some(EffortValue::Integer(5)),
        );
        assert_eq!(
            parse_effort(&FrontmatterValue::Integer(7)),
            Some(EffortValue::Integer(7)),
        );
        // Out of range
        assert_eq!(parse_effort(&FrontmatterValue::String("0".into())), None,);
        assert_eq!(parse_effort(&FrontmatterValue::String("11".into())), None,);
    }

    #[test]
    fn parse_argument_names_string_and_list() {
        let from_string = parse_argument_names(&FrontmatterValue::String("foo bar baz".into()));
        assert_eq!(from_string, vec!["foo", "bar", "baz"]);

        let from_list = parse_argument_names(&FrontmatterValue::StringList(vec![
            "foo".into(),
            "bar".into(),
        ]));
        assert_eq!(from_list, vec!["foo", "bar"]);
    }

    #[test]
    fn parse_argument_names_filters_numeric_only() {
        let result = parse_argument_names(&FrontmatterValue::String("foo 42 bar".into()));
        assert_eq!(result, vec!["foo", "bar"]);
    }

    #[test]
    fn parse_skill_paths_removes_double_star_suffix() {
        let paths = parse_skill_paths(Some(&FrontmatterValue::StringList(vec![
            "src/**".into(),
            "tests".into(),
        ])));
        assert_eq!(paths, Some(vec!["src".into(), "tests".into()]));
    }

    #[test]
    fn parse_skill_paths_returns_none_for_all_match_all() {
        let paths = parse_skill_paths(Some(&FrontmatterValue::StringList(vec!["**".into()])));
        assert_eq!(paths, None);
    }

    #[test]
    fn parse_allowed_tools_comma_separated() {
        let tools =
            parse_allowed_tools(Some(&FrontmatterValue::String("Read, Write, Bash".into())));
        assert_eq!(tools, vec!["Read", "Write", "Bash"]);
    }

    #[test]
    fn coerce_description_returns_none_for_empty() {
        assert_eq!(
            coerce_description(Some(&FrontmatterValue::String("".into()))),
            None
        );
        assert_eq!(
            coerce_description(Some(&FrontmatterValue::String("  ".into()))),
            None
        );
        assert_eq!(coerce_description(Some(&FrontmatterValue::Null)), None);
        assert_eq!(coerce_description(None), None);
    }

    #[test]
    fn extract_description_skips_headings_and_blanks() {
        let md = "# Title\n\n## Subtitle\n\nActual description here.\n\nMore text.";
        assert_eq!(
            extract_description_from_markdown(md, "Skill"),
            "Actual description here.",
        );
    }

    #[test]
    fn extract_description_truncates_multibyte_on_char_boundary() {
        // Regression: byte-index slicing panicked when byte 200 fell
        // inside a multi-byte character.
        let long = "令".repeat(250);
        let desc = extract_description_from_markdown(&long, "Skill");
        assert_eq!(desc.chars().count(), 201);
        assert!(desc.ends_with('…'));
        assert!(desc.starts_with("令"));

        // 120 three-byte chars exceed 200 bytes but not 200 chars:
        // must stay untruncated (the old byte-based check panicked here).
        let short = "令".repeat(120);
        assert_eq!(extract_description_from_markdown(&short, "Skill"), short);
    }

    #[test]
    fn extract_description_fallback() {
        assert_eq!(
            extract_description_from_markdown("", "Custom command"),
            "Custom command command",
        );
    }

    #[test]
    fn parse_full_frontmatter_defaults() {
        let fm_data = fm(&[(
            "description",
            FrontmatterValue::String("A test skill".into()),
        )]);
        let parsed = parse_skill_frontmatter_fields(&fm_data, "", "test", "Skill");
        assert_eq!(parsed.description, "A test skill");
        assert!(parsed.has_user_specified_description);
        assert!(parsed.user_invocable);
        assert!(!parsed.disable_model_invocation);
        assert!(parsed.model.is_none());
        assert!(parsed.execution_context.is_none());
        assert!(parsed.hooks.is_none());
        assert!(parsed.effort_parse_warning.is_none());
    }

    #[test]
    fn parse_full_frontmatter_all_fields() {
        let fm_data = fm(&[
            ("name", FrontmatterValue::String("My Skill".into())),
            (
                "description",
                FrontmatterValue::String("Does things".into()),
            ),
            (
                "allowed-tools",
                FrontmatterValue::String("Read, Bash".into()),
            ),
            (
                "required-tools",
                FrontmatterValue::StringList(vec!["ImageGen".into(), " Read ".into()]),
            ),
            ("argument-hint", FrontmatterValue::String("<file>".into())),
            ("arguments", FrontmatterValue::String("file output".into())),
            (
                "when_to_use",
                FrontmatterValue::String("When files need review".into()),
            ),
            ("version", FrontmatterValue::String("1.0".into())),
            ("model", FrontmatterValue::String("haiku".into())),
            ("disable-model-invocation", FrontmatterValue::Bool(true)),
            ("user-invocable", FrontmatterValue::Bool(false)),
            ("context", FrontmatterValue::String("fork".into())),
            ("agent", FrontmatterValue::String("batch-worker".into())),
            ("effort", FrontmatterValue::String("high".into())),
            ("shell", FrontmatterValue::String("powershell".into())),
        ]);
        let parsed = parse_skill_frontmatter_fields(&fm_data, "body", "test-skill", "Skill");

        assert_eq!(parsed.display_name.as_deref(), Some("My Skill"));
        assert_eq!(parsed.description, "Does things");
        assert_eq!(parsed.allowed_tools, vec!["Read", "Bash"]);
        assert_eq!(parsed.required_tools, vec!["ImageGen", "Read"]);
        assert_eq!(parsed.argument_hint.as_deref(), Some("<file>"));
        assert_eq!(parsed.argument_names, vec!["file", "output"]);
        assert_eq!(
            parsed.when_to_use.as_deref(),
            Some("When files need review")
        );
        assert_eq!(parsed.version.as_deref(), Some("1.0"));
        assert_eq!(parsed.model.as_deref(), Some("haiku"));
        assert!(parsed.disable_model_invocation);
        assert!(!parsed.user_invocable);
        assert_eq!(
            parsed.execution_context,
            Some(super::super::bundled::ExecutionContext::Fork),
        );
        assert_eq!(parsed.agent.as_deref(), Some("batch-worker"));
        assert_eq!(parsed.effort, Some(EffortValue::Named(EffortLevel::High)));
        assert_eq!(parsed.shell, Some(FrontmatterShell::PowerShell));
        assert!(parsed.hooks.is_none());
        assert!(parsed.effort_parse_warning.is_none());
    }

    #[test]
    fn model_inherit_maps_to_none() {
        let fm_data = fm(&[("model", FrontmatterValue::String("inherit".into()))]);
        let parsed = parse_skill_frontmatter_fields(&fm_data, "body", "test", "Skill");
        assert!(parsed.model.is_none());
    }

    #[test]
    fn model_is_lowercased() {
        let fm_data = fm(&[("model", FrontmatterValue::String("Haiku".into()))]);
        let parsed = parse_skill_frontmatter_fields(&fm_data, "body", "test", "Skill");
        assert_eq!(parsed.model.as_deref(), Some("haiku"));
    }

    #[test]
    fn description_falls_back_to_markdown_extraction() {
        let fm_data = fm(&[]);
        let parsed = parse_skill_frontmatter_fields(
            &fm_data,
            "# Title\n\nFirst real line.",
            "test",
            "Skill",
        );
        assert_eq!(parsed.description, "First real line.");
        assert!(!parsed.has_user_specified_description);
    }

    #[test]
    fn effort_parse_warning_on_invalid_value() {
        let fm_data = fm(&[("effort", FrontmatterValue::String("banana".into()))]);
        let parsed = parse_skill_frontmatter_fields(&fm_data, "", "test-skill", "Skill");
        assert!(parsed.effort.is_none());
        assert!(parsed.effort_parse_warning.is_some());
        let warning = parsed.effort_parse_warning.unwrap();
        assert!(warning.contains("test-skill"));
        assert!(warning.contains("banana"));
    }

    #[test]
    fn effort_no_warning_when_valid() {
        let fm_data = fm(&[("effort", FrontmatterValue::String("high".into()))]);
        let parsed = parse_skill_frontmatter_fields(&fm_data, "", "test-skill", "Skill");
        assert!(parsed.effort.is_some());
        assert!(parsed.effort_parse_warning.is_none());
    }

    #[test]
    fn hooks_parsed_from_map() {
        let mut hooks_map = HashMap::new();
        hooks_map.insert(
            "PreToolUse".to_string(),
            FrontmatterValue::String("test".into()),
        );
        let fm_data = fm(&[("hooks", FrontmatterValue::Map(hooks_map.clone()))]);
        let parsed = parse_skill_frontmatter_fields(&fm_data, "", "test", "Skill");
        assert_eq!(parsed.hooks, Some(hooks_map));
    }

    #[test]
    fn hooks_none_for_non_map() {
        let fm_data = fm(&[("hooks", FrontmatterValue::String("invalid".into()))]);
        let parsed = parse_skill_frontmatter_fields(&fm_data, "", "test", "Skill");
        assert!(parsed.hooks.is_none());
    }

    #[test]
    fn parse_user_specified_model_trims_and_lowercases() {
        assert_eq!(
            super::parse_user_specified_model("  Haiku  "),
            Some("haiku".into()),
        );
        assert_eq!(super::parse_user_specified_model(""), None);
        assert_eq!(super::parse_user_specified_model("   "), None);
        assert_eq!(
            super::parse_user_specified_model("SONNET"),
            Some("sonnet".into()),
        );
    }
}

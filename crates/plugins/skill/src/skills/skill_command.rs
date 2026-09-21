//! Full skill command model, builder, and prompt-expansion logic.
//!
//! The *I/O-free* portion: argument substitution, variable
//! replacement, token estimation, and the command data model.
//! Actual shell execution and file I/O live in the integration
//! layer.

use std::collections::HashMap;

use super::bundled::ExecutionContext;
use super::frontmatter::{EffortValue, FrontmatterShell, FrontmatterValue, ParsedSkillFrontmatter};

// ---------------------------------------------------------------------------
// Loading source
// ---------------------------------------------------------------------------

/// Where a skill was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadedFrom {
    /// Modern `/skills/` directories.
    Skills,
    /// Legacy `/commands/` directories.
    CommandsDeprecated,
    /// Plugin-provided skills.
    Plugin,
    /// Policy-managed skills.
    Managed,
    /// Built-in bundled skills.
    Bundled,
    /// MCP server-provided skills.
    Mcp,
}

/// The setting source where a skill's configuration originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    /// `.rebon/skills/` in the project.
    ProjectSettings,
    /// `~/.rebon/skills/`.
    UserSettings,
    /// Policy-managed path.
    PolicySettings,
    /// Local settings.
    LocalSettings,
    /// Feature-flag settings.
    FlagSettings,
    /// Built-in to the CLI.
    Builtin,
    /// Plugin-provided.
    Plugin,
    /// Bundled skill.
    Bundled,
    /// MCP server.
    Mcp,
}

// ---------------------------------------------------------------------------
// PluginInfo
// ---------------------------------------------------------------------------

/// Plugin metadata attached to plugin-provided skills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    /// Plugin manifest name.
    pub plugin_manifest_name: String,
    /// Plugin repository URL.
    pub repository: String,
}

// ---------------------------------------------------------------------------
// SkillCommandDef — the full command model
// ---------------------------------------------------------------------------

/// Complete skill command definition.
///
/// Prompt generation is *not* embedded as a closure. Instead,
/// [`expand_prompt`] performs the pure text-transformation steps;
/// the caller handles shell execution and I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCommandDef {
    /// Canonical skill name.
    pub name: String,
    /// Display name (from frontmatter `name` field).
    pub display_name: Option<String>,
    /// Human-readable description.
    pub description: String,
    /// Whether the description was explicitly set.
    pub has_user_specified_description: bool,
    /// Tools the skill may use.
    pub allowed_tools: Vec<String>,
    /// Tools a turn must offer for the skill to be listed or invoked.
    pub required_tools: Vec<String>,
    /// Hint shown after the skill name.
    pub argument_hint: Option<String>,
    /// Named argument list.
    pub arg_names: Vec<String>,
    /// When the model should auto-invoke.
    pub when_to_use: Option<String>,
    /// Skill version.
    pub version: Option<String>,
    /// Model override.
    pub model: Option<String>,
    /// Block model from auto-invoking.
    pub disable_model_invocation: bool,
    /// User can type `/name`.
    pub user_invocable: bool,
    /// Execution context.
    pub context: Option<ExecutionContext>,
    /// Agent type for forked execution.
    pub agent: Option<String>,
    /// Effort level.
    pub effort: Option<EffortValue>,
    /// Glob patterns for conditional activation.
    pub paths: Option<Vec<String>>,
    /// Length of the raw markdown content.
    pub content_length: usize,
    /// Hidden from completion UIs when not user-invocable.
    pub is_hidden: bool,
    /// Configuration source.
    pub source: CommandSource,
    /// Loading source.
    pub loaded_from: LoadedFrom,
    /// Base directory for skill files (if directory-based).
    pub skill_root: Option<String>,
    /// Shell for inline `!` blocks.
    pub shell: Option<FrontmatterShell>,
    /// Hooks configuration.
    pub hooks: Option<HashMap<String, FrontmatterValue>>,
    /// When `true`, the skill cannot be used in non-interactive
    /// mode.
    pub disable_non_interactive: bool,
    /// Plugin metadata (only set for plugin-provided skills).
    pub plugin_info: Option<PluginInfo>,
    /// Progress message shown in the UI spinner.
    /// Always `"running"` for skills.
    pub progress_message: String,
    /// The raw markdown body (prompt template).
    pub markdown_content: String,
}

impl SkillCommandDef {
    /// The command type discriminant — always `"prompt"` for skills.
    pub const COMMAND_TYPE: &'static str = "prompt";

    /// The name shown to users.
    pub fn user_facing_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Create a [`SkillCommandDef`] from parsed frontmatter and context.
pub fn create_skill_command(
    skill_name: &str,
    parsed: &ParsedSkillFrontmatter,
    markdown_content: &str,
    source: CommandSource,
    loaded_from: LoadedFrom,
    base_dir: Option<&str>,
    paths: Option<Vec<String>>,
) -> SkillCommandDef {
    SkillCommandDef {
        name: skill_name.to_string(),
        display_name: parsed.display_name.clone(),
        description: parsed.description.clone(),
        has_user_specified_description: parsed.has_user_specified_description,
        allowed_tools: parsed.allowed_tools.clone(),
        required_tools: parsed.required_tools.clone(),
        argument_hint: parsed.argument_hint.clone(),
        arg_names: parsed.argument_names.clone(),
        when_to_use: parsed.when_to_use.clone(),
        version: parsed.version.clone(),
        model: parsed.model.clone(),
        disable_model_invocation: parsed.disable_model_invocation,
        user_invocable: parsed.user_invocable,
        context: parsed.execution_context,
        agent: parsed.agent.clone(),
        effort: parsed.effort.clone(),
        paths,
        content_length: markdown_content.len(),
        is_hidden: !parsed.user_invocable,
        source,
        loaded_from,
        skill_root: base_dir.map(String::from),
        shell: parsed.shell,
        hooks: parsed.hooks.clone(),
        disable_non_interactive: false,
        plugin_info: None,
        progress_message: "running".into(),
        markdown_content: markdown_content.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Prompt expansion (pure text transforms)
// ---------------------------------------------------------------------------

/// Parse a `/<skill> [args]` user invocation out of prompt text.
///
/// Returns the skill id and the trailing arguments (`None` when there are
/// none). The caller decides whether that id is a registered, enabled,
/// user-invocable skill — this is only the syntax.
///
/// Deliberately strict about the name: ASCII alphanumerics plus `:`, `_` and
/// `-`. Anything else (a path like `/usr/bin`, a date, a fraction) is not a
/// skill invocation and must stay ordinary prompt text.
pub fn parse_user_skill_invocation(text: &str) -> Option<(String, Option<String>)> {
    let trimmed = text.trim_end();
    let rest = trimmed.strip_prefix('/')?;
    let mut split_at = rest.len();
    for (idx, ch) in rest.char_indices() {
        if ch.is_whitespace() {
            split_at = idx;
            break;
        }
    }
    let skill = rest[..split_at].trim();
    if skill.is_empty() || !skill.chars().all(is_skill_name_char) {
        return None;
    }
    let args = rest[split_at..].trim();
    Some((
        skill.to_string(),
        (!args.is_empty()).then(|| args.to_string()),
    ))
}

fn is_skill_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, ':' | '_' | '-')
}

/// Expand a skill prompt by applying all pure text transformations.
///
/// The shell execution step (which requires I/O) is left to the
/// caller.
///
/// Steps:
/// 1. Prepend base directory if present.
/// 2. Substitute `$ARGUMENTS` / `$0` / named arguments.
/// 3. Replace `${CLAUDE_SKILL_DIR}`.
/// 4. Replace `${CLAUDE_SESSION_ID}`.
///
/// Returns the expanded prompt text.
pub fn expand_prompt(cmd: &SkillCommandDef, args: Option<&str>, session_id: &str) -> String {
    let mut content = if let Some(base_dir) = &cmd.skill_root {
        format!(
            "Base directory for this skill: {base_dir}\n\n{}",
            cmd.markdown_content
        )
    } else {
        cmd.markdown_content.clone()
    };

    // Argument substitution.
    content = substitute_arguments(&content, args, true, &cmd.arg_names);

    // Replace ${CLAUDE_SKILL_DIR} — normalize backslashes on Windows.
    if let Some(base_dir) = &cmd.skill_root {
        let skill_dir = base_dir.replace('\\', "/");
        content = content.replace("${CLAUDE_SKILL_DIR}", &skill_dir);
    }

    // Replace ${CLAUDE_SESSION_ID}.
    content = content.replace("${CLAUDE_SESSION_ID}", session_id);

    content
}

// ---------------------------------------------------------------------------
// Argument substitution
// ---------------------------------------------------------------------------

/// Parse a raw arguments string into individual args with
/// POSIX-shell-style quoting.
///
/// Handles single-quoted (`'…'`), double-quoted (`"…"`) strings,
/// and backslash escapes.
pub fn parse_arguments(args: &str) -> Vec<String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut result = Vec::new();
    let mut current = String::new();
    let mut chars = trimmed.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while let Some(c) = chars.next() {
        if in_single_quote {
            if c == '\'' {
                in_single_quote = false;
            } else {
                current.push(c);
            }
        } else if in_double_quote {
            if c == '"' {
                in_double_quote = false;
            } else if c == '\\' {
                // Inside double quotes, backslash escapes a small set.
                if let Some(&next) = chars.peek() {
                    if matches!(next, '"' | '\\' | '$' | '`') {
                        current.push(chars.next().unwrap());
                    } else {
                        current.push(c);
                    }
                } else {
                    current.push(c);
                }
            } else {
                current.push(c);
            }
        } else {
            match c {
                '\'' => in_single_quote = true,
                '"' => in_double_quote = true,
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                c if c.is_whitespace() => {
                    if !current.is_empty() {
                        result.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(c),
            }
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

/// Substitute `$ARGUMENTS` placeholders in content.
///
/// Supported placeholders:
/// - `$ARGUMENTS` — full arguments string
/// - `$ARGUMENTS[N]` — indexed argument
/// - `$N` — shorthand for `$ARGUMENTS[N]`
/// - `$name` — named argument (when `argument_names` is set)
pub fn substitute_arguments(
    content: &str,
    args: Option<&str>,
    append_if_no_placeholder: bool,
    argument_names: &[String],
) -> String {
    let Some(args) = args else {
        return content.to_string();
    };

    let parsed_args = parse_arguments(args);
    let original_content = content.to_string();
    let mut result = content.to_string();

    // Replace named arguments ($name but not $name[...] or $nameXyz).
    for (i, name) in argument_names.iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        let value = parsed_args.get(i).map(|s| s.as_str()).unwrap_or("");
        // Match $name at word boundary — simple approach: replace
        // $name when followed by non-alphanumeric/non-underscore or
        // end-of-string.
        let pattern = format!("${name}");
        let mut out = String::with_capacity(result.len());
        let mut rest = result.as_str();
        while let Some(pos) = rest.find(&pattern) {
            out.push_str(&rest[..pos]);
            let after = &rest[pos + pattern.len()..];
            // Check that next char is NOT alphanumeric, underscore, or `[`.
            let next_char = after.chars().next();
            let is_boundary = next_char
                .map(|c| !c.is_alphanumeric() && c != '_' && c != '[')
                .unwrap_or(true);
            if is_boundary {
                out.push_str(value);
            } else {
                out.push_str(&pattern);
            }
            rest = after;
        }
        out.push_str(rest);
        result = out;
    }

    // Replace $ARGUMENTS[N].
    result = replace_indexed_arguments(&result, "$ARGUMENTS[", "]", &parsed_args);

    // Replace $N shorthand (not followed by word chars).
    result = replace_shorthand_indexed(&result, &parsed_args);

    // Replace $ARGUMENTS with full string.
    result = result.replace("$ARGUMENTS", args);

    // Append if no placeholders were found.
    if result == original_content && append_if_no_placeholder && !args.is_empty() {
        result.push_str(&format!("\n\nARGUMENTS: {args}"));
    }

    result
}

/// Replace `$ARGUMENTS[0]`, `$ARGUMENTS[1]`, etc.
fn replace_indexed_arguments(content: &str, prefix: &str, suffix: &str, args: &[String]) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find(prefix) {
        out.push_str(&rest[..start]);
        let after_prefix = &rest[start + prefix.len()..];
        if let Some(end) = after_prefix.find(suffix) {
            let index_str = &after_prefix[..end];
            if let Ok(index) = index_str.parse::<usize>() {
                out.push_str(args.get(index).map(|s| s.as_str()).unwrap_or(""));
                rest = &after_prefix[end + suffix.len()..];
                continue;
            }
        }
        // Not a valid pattern — emit prefix literally and continue.
        out.push_str(prefix);
        rest = after_prefix;
    }
    out.push_str(rest);
    out
}

/// Replace `$0`, `$1`, etc. (not followed by word characters).
fn replace_shorthand_indexed(content: &str, args: &[String]) -> String {
    let mut out = String::with_capacity(content.len());
    let chars: Vec<char> = content.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit() {
            // Collect all digits.
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && chars[end].is_ascii_digit() {
                end += 1;
            }
            // Check that next char is NOT a word character.
            let next_is_word =
                end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_');
            if !next_is_word {
                let index_str: String = chars[start..end].iter().collect();
                if let Ok(index) = index_str.parse::<usize>() {
                    out.push_str(args.get(index).map(|s| s.as_str()).unwrap_or(""));
                    i = end;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Rough token count estimation from text length.
///
/// Uses the default 4 bytes-per-token ratio.
pub fn rough_token_count_estimation(content: &str) -> u64 {
    (content.len() as f64 / 4.0).round() as u64
}

/// Estimate token count for a skill's frontmatter-only metadata.
pub fn estimate_skill_frontmatter_tokens(cmd: &SkillCommandDef) -> u64 {
    let mut text = cmd.name.clone();
    text.push(' ');
    text.push_str(&cmd.description);
    if let Some(ref w) = cmd.when_to_use {
        text.push(' ');
        text.push_str(w);
    }
    rough_token_count_estimation(&text)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- user skill invocation parsing --

    #[test]
    fn parse_user_skill_invocation_splits_name_and_args() {
        assert_eq!(
            parse_user_skill_invocation("/release"),
            Some(("release".to_string(), None))
        );
        assert_eq!(
            parse_user_skill_invocation("/release Watcher 1.2.0"),
            Some(("release".to_string(), Some("Watcher 1.2.0".to_string())))
        );
        // Namespaced and punctuated ids stay intact.
        assert_eq!(
            parse_user_skill_invocation("/zcf:git-commit  --no-verify "),
            Some((
                "zcf:git-commit".to_string(),
                Some("--no-verify".to_string())
            ))
        );
    }

    /// Prompt text that merely starts with a slash is not an invocation —
    /// treating `/usr/bin/env` or `/3 of the tests` as a skill would swallow
    /// real prompts.
    #[test]
    fn parse_user_skill_invocation_rejects_non_skill_slashes() {
        assert_eq!(parse_user_skill_invocation("/usr/bin/env python"), None);
        assert_eq!(parse_user_skill_invocation("/"), None);
        assert_eq!(parse_user_skill_invocation("/ release"), None);
        assert_eq!(parse_user_skill_invocation("release"), None);
        assert_eq!(parse_user_skill_invocation("look at /release"), None);
        assert_eq!(parse_user_skill_invocation("/发布"), None);
    }

    // -- argument substitution --

    // -- argument quoting --

    #[test]
    fn parse_arguments_simple_whitespace() {
        assert_eq!(parse_arguments("a b c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_arguments_empty() {
        assert!(parse_arguments("").is_empty());
        assert!(parse_arguments("   ").is_empty());
    }

    #[test]
    fn parse_arguments_single_quotes() {
        assert_eq!(
            parse_arguments("'hello world' --flag"),
            vec!["hello world", "--flag"],
        );
    }

    #[test]
    fn parse_arguments_double_quotes() {
        assert_eq!(
            parse_arguments("\"hello world\" --flag"),
            vec!["hello world", "--flag"],
        );
    }

    #[test]
    fn parse_arguments_backslash_escape() {
        assert_eq!(parse_arguments("hello\\ world"), vec!["hello world"]);
    }

    #[test]
    fn parse_arguments_mixed_quotes() {
        assert_eq!(parse_arguments("'a b' \"c d\" e"), vec!["a b", "c d", "e"],);
    }

    #[test]
    fn parse_arguments_escape_in_double_quotes() {
        assert_eq!(
            parse_arguments("\"hello \\\"world\\\"\""),
            vec!["hello \"world\""],
        );
    }

    // -- argument substitution --

    #[test]
    fn substitute_arguments_replaces_dollar_arguments() {
        let result = substitute_arguments("Run $ARGUMENTS now", Some("test --verbose"), false, &[]);
        assert_eq!(result, "Run test --verbose now");
    }

    #[test]
    fn substitute_arguments_indexed() {
        let result = substitute_arguments(
            "File: $ARGUMENTS[0], Output: $ARGUMENTS[1]",
            Some("input.txt output.txt"),
            false,
            &[],
        );
        assert_eq!(result, "File: input.txt, Output: output.txt");
    }

    #[test]
    fn substitute_arguments_shorthand() {
        let result = substitute_arguments("First: $0, Second: $1", Some("alpha beta"), false, &[]);
        assert_eq!(result, "First: alpha, Second: beta");
    }

    #[test]
    fn substitute_arguments_named() {
        let result = substitute_arguments(
            "File: $file, Output: $output",
            Some("input.txt result.txt"),
            false,
            &["file".to_string(), "output".to_string()],
        );
        assert_eq!(result, "File: input.txt, Output: result.txt");
    }

    #[test]
    fn substitute_arguments_named_word_boundary() {
        // $file should NOT match inside $filename.
        let result = substitute_arguments(
            "Use $filename not $file",
            Some("test.txt"),
            false,
            &["file".to_string()],
        );
        assert_eq!(result, "Use $filename not test.txt");
    }

    #[test]
    fn substitute_arguments_appends_when_no_placeholder() {
        let result = substitute_arguments("Run the skill", Some("extra args"), true, &[]);
        assert_eq!(result, "Run the skill\n\nARGUMENTS: extra args");
    }

    #[test]
    fn substitute_arguments_no_append_for_empty_args() {
        let result = substitute_arguments("Run the skill", Some(""), true, &[]);
        assert_eq!(result, "Run the skill");
    }

    #[test]
    fn substitute_arguments_none_returns_unchanged() {
        let result = substitute_arguments("Hello $ARGUMENTS", None, true, &[]);
        assert_eq!(result, "Hello $ARGUMENTS");
    }

    #[test]
    fn substitute_arguments_missing_index_becomes_empty() {
        let result = substitute_arguments("A: $0, B: $1, C: $2", Some("only-one"), false, &[]);
        assert_eq!(result, "A: only-one, B: , C: ");
    }

    // -- token estimation --

    #[test]
    fn rough_token_estimation() {
        assert_eq!(rough_token_count_estimation(""), 0);
        assert_eq!(rough_token_count_estimation("abcd"), 1);
        assert_eq!(rough_token_count_estimation("abcdefgh"), 2);
        // 100 chars → 25 tokens
        let s = "a".repeat(100);
        assert_eq!(rough_token_count_estimation(&s), 25);
    }

    // -- expand_prompt --

    #[test]
    fn expand_prompt_with_base_dir_and_variables() {
        let cmd = SkillCommandDef {
            name: "test".into(),
            display_name: None,
            description: "test skill".into(),
            has_user_specified_description: true,
            allowed_tools: Vec::new(),
            required_tools: Vec::new(),
            argument_hint: None,
            arg_names: vec!["name".into()],
            when_to_use: None,
            version: None,
            model: None,
            disable_model_invocation: false,
            user_invocable: true,
            context: None,
            agent: None,
            effort: None,
            paths: None,
            content_length: 50,
            is_hidden: false,
            source: CommandSource::ProjectSettings,
            loaded_from: LoadedFrom::Skills,
            skill_root: Some("/tmp/skill".into()),
            shell: None,
            hooks: None,
            disable_non_interactive: false,
            plugin_info: None,
            progress_message: "running".into(),
            markdown_content:
                "Hello $name. Dir: ${CLAUDE_SKILL_DIR}. Session: ${CLAUDE_SESSION_ID}.".into(),
        };

        let result = expand_prompt(&cmd, Some("World"), "sess-123");
        assert!(result.starts_with("Base directory for this skill: /tmp/skill\n\n"));
        assert!(result.contains("Hello World."));
        assert!(result.contains("Dir: /tmp/skill."));
        assert!(result.contains("Session: sess-123."));
    }

    #[test]
    fn expand_prompt_no_base_dir() {
        let cmd = SkillCommandDef {
            name: "test".into(),
            display_name: None,
            description: String::new(),
            has_user_specified_description: false,
            allowed_tools: Vec::new(),
            required_tools: Vec::new(),
            argument_hint: None,
            arg_names: Vec::new(),
            when_to_use: None,
            version: None,
            model: None,
            disable_model_invocation: false,
            user_invocable: true,
            context: None,
            agent: None,
            effort: None,
            paths: None,
            content_length: 10,
            is_hidden: false,
            source: CommandSource::Bundled,
            loaded_from: LoadedFrom::Bundled,
            skill_root: None,
            shell: None,
            hooks: None,
            disable_non_interactive: false,
            plugin_info: None,
            progress_message: "running".into(),
            markdown_content: "Simple prompt".into(),
        };

        let result = expand_prompt(&cmd, None, "sess-0");
        assert_eq!(result, "Simple prompt");
    }

    #[test]
    fn user_facing_name_prefers_display_name() {
        let cmd = SkillCommandDef {
            name: "skill-id".into(),
            display_name: Some("Pretty Name".into()),
            description: String::new(),
            has_user_specified_description: false,
            allowed_tools: Vec::new(),
            required_tools: Vec::new(),
            argument_hint: None,
            arg_names: Vec::new(),
            when_to_use: None,
            version: None,
            model: None,
            disable_model_invocation: false,
            user_invocable: true,
            context: None,
            agent: None,
            effort: None,
            paths: None,
            content_length: 0,
            is_hidden: false,
            source: CommandSource::ProjectSettings,
            loaded_from: LoadedFrom::Skills,
            skill_root: None,
            shell: None,
            hooks: None,
            disable_non_interactive: false,
            plugin_info: None,
            progress_message: "running".into(),
            markdown_content: String::new(),
        };
        assert_eq!(cmd.user_facing_name(), "Pretty Name");
    }

    // -- create_skill_command --

    #[test]
    fn create_skill_command_wires_fields_correctly() {
        let parsed = ParsedSkillFrontmatter {
            display_name: Some("My Skill".into()),
            description: "A skill".into(),
            has_user_specified_description: true,
            allowed_tools: vec!["Read".into()],
            required_tools: Vec::new(),
            argument_hint: Some("<file>".into()),
            argument_names: vec!["file".into()],
            when_to_use: Some("When reviewing".into()),
            version: Some("1.0".into()),
            model: Some("haiku".into()),
            disable_model_invocation: true,
            user_invocable: false,
            execution_context: Some(ExecutionContext::Fork),
            agent: Some("batch-worker".into()),
            effort: Some(EffortValue::Named(
                crate::skills::frontmatter::EffortLevel::High,
            )),
            shell: Some(FrontmatterShell::Bash),
            hooks: None,
            effort_parse_warning: None,
        };

        let cmd = create_skill_command(
            "my-skill",
            &parsed,
            "# Prompt body",
            CommandSource::ProjectSettings,
            LoadedFrom::Skills,
            Some("/project/.rebon/skills/my-skill"),
            Some(vec!["src/**".into()]),
        );

        assert_eq!(cmd.name, "my-skill");
        assert_eq!(cmd.display_name.as_deref(), Some("My Skill"));
        assert_eq!(cmd.description, "A skill");
        assert_eq!(cmd.allowed_tools, vec!["Read"]);
        assert!(cmd.is_hidden); // user_invocable == false
        assert_eq!(cmd.content_length, "# Prompt body".len());
        assert_eq!(cmd.source, CommandSource::ProjectSettings);
        assert_eq!(cmd.loaded_from, LoadedFrom::Skills);
        assert_eq!(
            cmd.skill_root.as_deref(),
            Some("/project/.rebon/skills/my-skill")
        );
        assert_eq!(cmd.paths, Some(vec!["src/**".into()]));
        assert!(cmd.hooks.is_none()); // from parsed.hooks
        assert!(!cmd.disable_non_interactive);
        assert!(cmd.plugin_info.is_none());
        assert_eq!(cmd.progress_message, "running");
        assert_eq!(SkillCommandDef::COMMAND_TYPE, "prompt");
    }
}

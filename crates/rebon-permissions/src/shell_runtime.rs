//! Runtime-ish pure state helpers for Bash/PowerShell permission prompts.
//!
//! Implements:
//! * simple-command and first-word prefix extraction.
//! * Bash editable-prefix seed/refine logic and the
//!   classifier-was-checking transition.
//! * PowerShell editable-prefix seed/refine logic.

use crate::rule_value::permission_rule_value_from_string;
use crate::types::{
    PermissionDecisionReason, PermissionResult, PermissionRuleValue, PermissionUpdate,
};
pub use rebon_shell_policy::lexer::{parse_bash_shape, BashShape};

use rebon_shell_policy::lexer::{self, simple_command_segments};
use std::collections::BTreeSet;

const BARE_SHELL_PREFIXES: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "fish",
    "csh",
    "tcsh",
    "ksh",
    "dash",
    "cmd",
    "powershell",
    "pwsh",
    "env",
    "xargs",
    "nice",
    "stdbuf",
    "nohup",
    "timeout",
    "time",
    "sudo",
    "doas",
    "pkexec",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashPrefixContext<'a> {
    pub command: &'a str,
    pub permission_result: &'a PermissionResult,
    pub safe_env_vars: &'a BTreeSet<String>,
    pub internal_only_safe_env_vars: &'a BTreeSet<String>,
    pub internal_build: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashEditablePrefixState {
    pub value: Option<String>,
    pub has_user_edited: bool,
    pub is_compound: bool,
    pub classifier_was_checking: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerShellEditablePrefixState {
    pub value: Option<String>,
    pub has_user_edited: bool,
}

pub fn bash_editable_prefix_state(
    context: BashPrefixContext<'_>,
    classifier_feature_enabled: bool,
    classifier_check_in_progress: bool,
) -> BashEditablePrefixState {
    let is_compound = matches!(
        context.permission_result.decision_reason(),
        Some(PermissionDecisionReason::SubcommandResults { .. })
    );

    let value = if is_compound {
        seed_bash_prefix_from_backend(context.permission_result.suggestions())
    } else {
        get_simple_command_prefix(
            context.command,
            context.safe_env_vars,
            context.internal_only_safe_env_vars,
            context.internal_build,
        )
        .map(|prefix| format!("{prefix}:*"))
        .or_else(|| {
            get_first_word_prefix(
                context.command,
                context.safe_env_vars,
                context.internal_only_safe_env_vars,
                context.internal_build,
            )
            .map(|prefix| format!("{prefix}:*"))
        })
        .or_else(|| Some(context.command.to_string()))
    };

    BashEditablePrefixState {
        value,
        has_user_edited: false,
        is_compound,
        classifier_was_checking: classifier_feature_enabled && classifier_check_in_progress,
    }
}

pub fn bash_editable_prefix_changed(state: &mut BashEditablePrefixState, value: String) {
    state.has_user_edited = true;
    state.value = Some(value);
}

pub fn refine_bash_editable_prefix(state: &mut BashEditablePrefixState, prefixes: &[String]) {
    if state.is_compound || state.has_user_edited || prefixes.is_empty() {
        return;
    }
    state.value = Some(format!("{}:*", prefixes[0]));
}

pub fn powershell_editable_prefix_state(command: &str) -> PowerShellEditablePrefixState {
    PowerShellEditablePrefixState {
        value: (!command.contains('\n')).then_some(command.to_string()),
        has_user_edited: false,
    }
}

pub fn powershell_editable_prefix_changed(
    state: &mut PowerShellEditablePrefixState,
    value: String,
) {
    state.has_user_edited = true;
    state.value = Some(value);
}

pub fn refine_powershell_editable_prefix(
    state: &mut PowerShellEditablePrefixState,
    prefixes: &[String],
) {
    if state.has_user_edited || prefixes.is_empty() {
        return;
    }
    state.value = Some(format!("{}:*", prefixes[0]));
}

pub fn toggle_permission_debug(current: bool) -> bool {
    !current
}

pub fn get_simple_command_prefix(
    command: &str,
    safe_env_vars: &BTreeSet<String>,
    internal_only_safe_env_vars: &BTreeSet<String>,
    internal_build: bool,
) -> Option<String> {
    let tokens = command.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() {
        return None;
    }

    let mut i = 0usize;
    while i < tokens.len() && is_env_assignment(tokens[i]) {
        let var_name = tokens[i].split('=').next()?;
        let is_internal_only_safe =
            internal_build && internal_only_safe_env_vars.contains(var_name);
        if !safe_env_vars.contains(var_name) && !is_internal_only_safe {
            return None;
        }
        i += 1;
    }

    let remaining = &tokens[i..];
    if remaining.len() < 2 {
        return None;
    }
    let subcmd = remaining[1];
    if !looks_like_subcommand(subcmd) {
        return None;
    }
    Some(format!("{} {}", remaining[0], subcmd))
}

pub fn get_first_word_prefix(
    command: &str,
    safe_env_vars: &BTreeSet<String>,
    internal_only_safe_env_vars: &BTreeSet<String>,
    internal_build: bool,
) -> Option<String> {
    let tokens = command.split_whitespace().collect::<Vec<_>>();
    let mut i = 0usize;
    while i < tokens.len() && is_env_assignment(tokens[i]) {
        let var_name = tokens[i].split('=').next()?;
        let is_internal_only_safe =
            internal_build && internal_only_safe_env_vars.contains(var_name);
        if !safe_env_vars.contains(var_name) && !is_internal_only_safe {
            return None;
        }
        i += 1;
    }

    let cmd = *tokens.get(i)?;
    if !looks_like_subcommand(cmd) {
        return None;
    }
    if is_bare_shell_prefix(cmd) {
        return None;
    }
    Some(cmd.to_string())
}

pub fn is_bare_shell_prefix(cmd: &str) -> bool {
    let cmd = cmd
        .trim_matches(|ch| ch == '"' || ch == '\'')
        .replace('\\', "/");
    let basename = cmd.rsplit('/').next().unwrap_or(&cmd);
    let basename = strip_ascii_suffix(basename, ".exe");

    BARE_SHELL_PREFIXES
        .iter()
        .any(|prefix| basename.eq_ignore_ascii_case(prefix))
}

pub fn first_shell_word(command: &str) -> Option<&str> {
    let command = command.trim_start();
    let mut chars = command.char_indices();
    let (_, first) = chars.next()?;
    if first == '"' || first == '\'' {
        for (idx, ch) in chars {
            if ch == first {
                return Some(&command[..idx + ch.len_utf8()]);
            }
        }
        return Some(command);
    }

    command.split_ascii_whitespace().next()
}

fn strip_ascii_suffix<'a>(value: &'a str, suffix: &str) -> &'a str {
    if value.len() >= suffix.len()
        && value[value.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    {
        &value[..value.len() - suffix.len()]
    } else {
        value
    }
}

/// Whether the command contains anything a shell would act on beyond running
/// one command with arguments.
///
/// This is the same scan [`parse_bash_shape`] runs, asked for less: only
/// whether it hit a metacharacter. An unterminated quote or a dangling
/// backslash is *not* one — those are typos in a prefix the user is still
/// editing, and answering "yes, complex" to them would withdraw the editable
/// prefix mid-keystroke.
pub fn has_unquoted_shell_metacharacter(command: &str) -> bool {
    matches!(
        simple_command_segments(command, &lexer::ANY_METACHARACTER),
        Err(lexer::Bail::Metacharacter)
    )
}

fn seed_bash_prefix_from_backend(suggestions: &[PermissionUpdate]) -> Option<String> {
    let backend_bash_rules = suggestions
        .iter()
        .flat_map(|suggestion| match suggestion {
            PermissionUpdate::AddRules { rules, .. } => rules.clone(),
            _ => vec![],
        })
        .map(|rule| {
            permission_rule_value_from_string(&permission_rule_value_to_string_lossy(&rule))
        })
        .filter(|rule| rule.tool_name == "Bash" && rule.rule_content.is_some())
        .collect::<Vec<_>>();

    if backend_bash_rules.len() == 1 {
        backend_bash_rules[0].rule_content.clone()
    } else {
        None
    }
}

fn permission_rule_value_to_string_lossy(rule: &PermissionRuleValue) -> String {
    match &rule.rule_content {
        Some(rule_content) => format!("{}({rule_content})", rule.tool_name),
        None => rule.tool_name.clone(),
    }
}

fn is_env_assignment(token: &str) -> bool {
    let mut parts = token.splitn(2, '=');
    let name = parts.next().unwrap_or_default();
    let value = parts.next();
    if value.is_none() || name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn looks_like_subcommand(token: &str) -> bool {
    let mut parts = token.split('-');
    let first = parts.next().unwrap_or_default();
    if first.is_empty()
        || !first
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        || !first
            .chars()
            .next()
            .unwrap_or_default()
            .is_ascii_lowercase()
    {
        return false;
    }
    parts.all(|part| {
        !part.is_empty()
            && part
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
    })
}

/// Strip a leading `cd <cwd> && ` from a command when the cd
/// target matches the session working directory.
///
/// Returns the tail command when a match is found, or `None` when
/// the prefix doesn't match (or there is no cd prefix at all).
///
/// Both forward and back slashes are normalized before comparison,
/// and the cd target may be bare or quoted (`"path"` / `'path'`).
///
/// Rule evaluation and rule *authoring* must agree on the command text,
/// otherwise an `allow_always` rule recorded from the raw command can
/// never match the stripped command the evaluator sees. Both sides call
/// this function.
pub fn strip_cwd_prefix<'a>(command: &'a str, cwd: &str) -> Option<&'a str> {
    let command = command.trim();
    // Must start with `cd ` (case-insensitive).
    let after_cd = command
        .strip_prefix("cd ")
        .or_else(|| command.strip_prefix("CD "))?;
    // Find the ` && ` separator.
    let sep_pos = after_cd.find(" && ")?;
    let cd_target = after_cd[..sep_pos].trim();
    // Strip surrounding quotes if present.
    let cd_target = cd_target
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            cd_target
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .unwrap_or(cd_target);
    if !target_matches_cwd(cd_target, cwd) {
        return None;
    }
    let tail = &after_cd[sep_pos + 4..]; // skip ` && `
    Some(tail)
}

/// Strip a leading `Set-Location <cwd>;` from a PowerShell command.
/// The target must be a literal path matching the session working
/// directory; variables, expressions, and other directories are left
/// intact for normal permission matching.
pub fn strip_powershell_cwd_prefix<'a>(command: &'a str, cwd: &str) -> Option<&'a str> {
    let command = command.trim();
    let command_name_len = ["Set-Location", "cd"]
        .into_iter()
        .find(|name| {
            command
                .get(..name.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(name))
                && command[name.len()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace)
        })?
        .len();
    let mut arguments = command[command_name_len..].trim_start();
    for parameter in ["-LiteralPath", "-Path"] {
        if arguments
            .get(..parameter.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(parameter))
            && arguments[parameter.len()..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
        {
            arguments = arguments[parameter.len()..].trim_start();
            break;
        }
    }

    let mut quote = None;
    let mut escaped = false;
    let mut separator = None;
    for (offset, ch) in arguments.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '`' && quote == Some('"') {
            escaped = true;
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            }
            continue;
        }
        if ch == ';' && quote.is_none() {
            separator = Some(offset);
            break;
        }
    }
    let separator = separator?;
    let target = arguments[..separator].trim();
    if !target_matches_cwd(target, cwd) {
        return None;
    }
    Some(arguments[separator + 1..].trim_start())
}

fn target_matches_cwd(target: &str, cwd: &str) -> bool {
    let target = target.trim();
    let target = target
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            target
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(target);
    target
        .replace('\\', "/")
        .eq_ignore_ascii_case(&cwd.replace('\\', "/"))
}

/// Strip whichever working-directory wrapper `tool_name` can carry, so
/// callers that author permission rules see the same command text the
/// policy evaluator matches against. Returns the command unchanged when
/// there is no wrapper, when `cwd` is empty, or for non-shell tools.
pub fn command_without_cwd_prefix<'a>(tool_name: &str, command: &'a str, cwd: &str) -> &'a str {
    if cwd.is_empty() {
        return command;
    }
    let stripped = match tool_name {
        "Bash" | "BashTool" => strip_cwd_prefix(command, cwd),
        "PowerShell" | "PowerShellTool" => {
            strip_powershell_cwd_prefix(command, cwd).or_else(|| strip_cwd_prefix(command, cwd))
        }
        _ => None,
    };
    stripped.unwrap_or(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PermissionBehavior, PermissionRuleValue, PermissionUpdateDestination};

    fn safe_envs() -> BTreeSet<String> {
        BTreeSet::from(["NODE_ENV".to_string(), "CI".to_string()])
    }

    #[test]
    fn simple_command_prefix_skips_safe_env_assignments() {
        assert_eq!(
            get_simple_command_prefix(
                "NODE_ENV=prod npm run build",
                &safe_envs(),
                &BTreeSet::new(),
                false
            ),
            Some("npm run".to_string())
        );
        assert_eq!(
            get_simple_command_prefix(
                "RUN=/tmp npm run build",
                &safe_envs(),
                &BTreeSet::new(),
                false
            ),
            None
        );
    }

    #[test]
    fn first_word_prefix_rejects_bare_shells_and_paths() {
        assert_eq!(
            get_first_word_prefix("bash -lc foo", &safe_envs(), &BTreeSet::new(), false),
            None
        );
        assert_eq!(
            get_first_word_prefix("./script.sh", &safe_envs(), &BTreeSet::new(), false),
            None
        );
        assert_eq!(
            get_first_word_prefix("python3 file.py", &safe_envs(), &BTreeSet::new(), false),
            Some("python3".to_string())
        );
    }

    #[test]
    fn bash_state_uses_backend_rule_for_compound_commands() {
        let state = bash_editable_prefix_state(
            BashPrefixContext {
                command: "cd src && npm test",
                permission_result: &PermissionResult::Ask {
                    message: "Need approval".to_string(),
                    decision_reason: Some(PermissionDecisionReason::SubcommandResults {
                        reasons: vec![],
                    }),
                    suggestions: vec![PermissionUpdate::AddRules {
                        destination: PermissionUpdateDestination::LocalSettings,
                        rules: vec![PermissionRuleValue::new("Bash", Some("npm test:*"))],
                        behavior: PermissionBehavior::Allow,
                    }],
                },
                safe_env_vars: &safe_envs(),
                internal_only_safe_env_vars: &BTreeSet::new(),
                internal_build: false,
            },
            true,
            true,
        );
        assert_eq!(state.value, Some("npm test:*".to_string()));
        assert!(state.is_compound);
        assert!(state.classifier_was_checking);
    }

    #[test]
    fn bash_state_falls_back_to_simple_then_first_word_then_full_command() {
        let result = PermissionResult::Allow {
            decision_reason: None,
        };
        let simple = bash_editable_prefix_state(
            BashPrefixContext {
                command: "npm run build",
                permission_result: &result,
                safe_env_vars: &safe_envs(),
                internal_only_safe_env_vars: &BTreeSet::new(),
                internal_build: false,
            },
            false,
            false,
        );
        assert_eq!(simple.value, Some("npm run:*".to_string()));

        let first = bash_editable_prefix_state(
            BashPrefixContext {
                command: "python3 file.py 2>&1 | tail -20",
                permission_result: &result,
                safe_env_vars: &safe_envs(),
                internal_only_safe_env_vars: &BTreeSet::new(),
                internal_build: false,
            },
            false,
            false,
        );
        assert_eq!(first.value, Some("python3:*".to_string()));

        let full = bash_editable_prefix_state(
            BashPrefixContext {
                command: "./script.sh && npm test",
                permission_result: &result,
                safe_env_vars: &safe_envs(),
                internal_only_safe_env_vars: &BTreeSet::new(),
                internal_build: false,
            },
            false,
            false,
        );
        assert_eq!(full.value, Some("./script.sh && npm test".to_string()));
    }

    #[test]
    fn refine_bash_prefix_respects_user_edits_and_compound_guard() {
        let result = PermissionResult::Allow {
            decision_reason: None,
        };
        let mut state = bash_editable_prefix_state(
            BashPrefixContext {
                command: "npm run build",
                permission_result: &result,
                safe_env_vars: &safe_envs(),
                internal_only_safe_env_vars: &BTreeSet::new(),
                internal_build: false,
            },
            false,
            false,
        );
        bash_editable_prefix_changed(&mut state, "custom".to_string());
        refine_bash_editable_prefix(&mut state, &[String::from("npm test")]);
        assert_eq!(state.value, Some("custom".to_string()));
    }

    #[test]
    fn powershell_state_uses_multiline_guard_and_refine() {
        let mut state = powershell_editable_prefix_state("Get-Process");
        assert_eq!(state.value, Some("Get-Process".to_string()));
        refine_powershell_editable_prefix(&mut state, &[String::from("Get-Process")]);
        assert_eq!(state.value, Some("Get-Process:*".to_string()));

        let multiline = powershell_editable_prefix_state("# comment\nGet-Process");
        assert_eq!(multiline.value, None);
    }

    #[test]
    fn bare_shell_prefix_matches_path_qualified_shells() {
        assert!(is_bare_shell_prefix("/bin/bash"));
        assert!(is_bare_shell_prefix("/usr/bin/env"));
        assert!(is_bare_shell_prefix("powershell.exe"));
        assert!(is_bare_shell_prefix(
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
        ));
        assert!(!is_bare_shell_prefix("cargo"));
    }

    #[test]
    fn first_shell_word_keeps_quoted_paths_together() {
        assert_eq!(
            first_shell_word(r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoProfile"#),
            Some(r#""C:\Program Files\PowerShell\7\pwsh.exe""#)
        );
        assert_eq!(
            first_shell_word("'/usr/local/bin/bash' -lc whoami"),
            Some("'/usr/local/bin/bash'")
        );
        assert_eq!(first_shell_word("cargo test"), Some("cargo"));
    }

    #[test]
    fn parses_simple_bash_commands() {
        assert_eq!(
            parse_bash_shape("cargo test -p rebon-core"),
            BashShape::Simple(vec![
                "cargo".to_string(),
                "test".to_string(),
                "-p".to_string(),
                "rebon-core".to_string()
            ])
        );
        assert_eq!(
            parse_bash_shape("cargo test 'foo && bar'"),
            BashShape::Simple(vec![
                "cargo".to_string(),
                "test".to_string(),
                "foo && bar".to_string()
            ])
        );
        assert_eq!(
            parse_bash_shape(r#"cargo test "foo && bar""#),
            BashShape::Simple(vec![
                "cargo".to_string(),
                "test".to_string(),
                "foo && bar".to_string()
            ])
        );
    }

    #[test]
    fn parses_safe_and_chains() {
        assert_eq!(
            parse_bash_shape("cargo test -p a && cargo test -p b"),
            BashShape::SafeAndChain(vec![
                vec![
                    "cargo".to_string(),
                    "test".to_string(),
                    "-p".to_string(),
                    "a".to_string()
                ],
                vec![
                    "cargo".to_string(),
                    "test".to_string(),
                    "-p".to_string(),
                    "b".to_string()
                ]
            ])
        );
        assert_eq!(
            parse_bash_shape(r"cargo test foo\&\&bar && cargo test baz"),
            BashShape::SafeAndChain(vec![
                vec![
                    "cargo".to_string(),
                    "test".to_string(),
                    "foo&&bar".to_string()
                ],
                vec!["cargo".to_string(), "test".to_string(), "baz".to_string()]
            ])
        );
    }

    #[test]
    fn rejects_unsafe_complex_bash_shapes() {
        for command in [
            "cargo test ; cargo test",
            "cargo test || cargo test",
            "cargo test | sh",
            "cargo test > out.log",
            "cargo test < in.log",
            "cargo test &",
            "cargo test\ncargo test",
            "cargo test `echo foo`",
            "cargo test $(echo foo)",
            "cargo test &&",
            "&& cargo test",
            "cargo test && && cargo test",
            "cargo test (echo foo)",
            "cargo test <(echo foo)",
        ] {
            assert_eq!(
                parse_bash_shape(command),
                BashShape::UnsafeComplex,
                "{command}"
            );
        }
    }

    #[test]
    fn detects_unquoted_shell_metacharacters() {
        assert!(has_unquoted_shell_metacharacter(
            "cargo test && rm -rf target"
        ));
        assert!(has_unquoted_shell_metacharacter(
            "cargo test ; rm -rf target"
        ));
        assert!(has_unquoted_shell_metacharacter("cargo test | tee out.log"));
        assert!(has_unquoted_shell_metacharacter("cargo test > out.log"));
        assert!(has_unquoted_shell_metacharacter("cargo test $(whoami)"));
        assert!(!has_unquoted_shell_metacharacter(
            "cargo test -- --exact 'name with && chars'"
        ));
        assert!(!has_unquoted_shell_metacharacter(
            r#"cargo test -- --exact "name with | chars""#
        ));
    }

    #[test]
    fn debug_toggle_flips_boolean() {
        assert!(toggle_permission_debug(false));
        assert!(!toggle_permission_debug(true));
    }

    #[test]
    fn strips_matching_cd_prefix_across_slash_and_quote_styles() {
        assert_eq!(
            strip_cwd_prefix(r#"cd "F:\dev\project" && cargo test"#, "F:/dev/project"),
            Some("cargo test")
        );
        assert_eq!(
            strip_cwd_prefix("cd '/home/user/project' && ls -la", "/home/user/project"),
            Some("ls -la")
        );
        assert_eq!(
            strip_cwd_prefix("cd /home/user/project && ls", "/home/user/project"),
            Some("ls")
        );
    }

    #[test]
    fn leaves_other_directories_and_bare_commands_alone() {
        assert_eq!(strip_cwd_prefix("cd /tmp && rm -rf *", "/home/user"), None);
        assert_eq!(strip_cwd_prefix("cargo test -p foo", "/home/user"), None);
    }

    #[test]
    fn command_without_cwd_prefix_picks_the_wrapper_the_tool_can_carry() {
        assert_eq!(
            command_without_cwd_prefix("Bash", r#"cd "F:\p" && cargo test"#, "F:/p"),
            "cargo test"
        );
        assert_eq!(
            command_without_cwd_prefix("PowerShell", r#"Set-Location "F:\p"; cargo test"#, "F:/p"),
            "cargo test"
        );
        // PowerShell also accepts the bash-style wrapper.
        assert_eq!(
            command_without_cwd_prefix("PowerShell", r#"cd "F:\p" && cargo test"#, "F:/p"),
            "cargo test"
        );
        // A `Set-Location` wrapper is not a Bash shape, and an unknown cwd
        // leaves everything intact.
        assert_eq!(
            command_without_cwd_prefix("Bash", r#"Set-Location "F:\p"; cargo test"#, "F:/p"),
            r#"Set-Location "F:\p"; cargo test"#
        );
        assert_eq!(
            command_without_cwd_prefix("Bash", r#"cd "F:\p" && cargo test"#, ""),
            r#"cd "F:\p" && cargo test"#
        );
    }
}

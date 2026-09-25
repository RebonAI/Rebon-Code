//! Turning a PowerShell command into the argv a permission rule matches
//! against.
//!
//! ## Why this is not [`crate::lexer::parse_bash_shape`]
//!
//! It was, and the result was that PowerShell allow rules quietly never
//! matched. The two languages disagree on the character that appears in
//! almost every Windows command:
//!
//! ```text
//! Get-Content C:\temp\notes.txt
//! ```
//!
//! POSIX reads `\` as an escape, so that tokenises to
//! `["Get-Content", "C:tempnotes.txt"]` — a path that does not exist, and one
//! no rule the user could write would ever match. The user sees a permission
//! prompt for a command they explicitly allowed, with nothing on screen
//! explaining why. PowerShell reads `\` as an ordinary character and escapes
//! with a backtick instead.
//!
//! The direction of that bug was safe — a rule that fails to match denies
//! rather than permits — and this module keeps that property while making the
//! common cases actually work.
//!
//! ## The safety rule
//!
//! **Anything not provably a simple command is [`BashShape::UnsafeComplex`],
//! which matches no rule at all.** This tokeniser does not have to understand
//! PowerShell; it has to be unable to mistake something complicated for
//! something simple. Pipelines, sub-expressions, script blocks, redirections,
//! here-strings and `--%` all bail out rather than being approximated.
//!
//! That is also the honest answer to the objection that the real
//! `[System.Management.Automation.Language.Parser]` would be more accurate.
//! Spawning `pwsh` inside a synchronous rule matcher — once per rule, on the
//! permission hot path — is not something this architecture can do, and a
//! hand-written tokeniser cannot match the official parser on exotic input. It
//! does not need to: the exotic input is exactly what bails out.
//!
//! ## Aliases
//!
//! `rm` is `Remove-Item`. Without normalisation a `PowerShell(Remove-Item:*)`
//! deny rule is bypassed by typing the alias, so [`canonical_cmdlet`] maps the
//! built-in aliases onto their cmdlets and the matcher canonicalises **both**
//! sides before comparing — a rule written against either spelling matches a
//! command written in either spelling.
//!
//! The table covers PowerShell's built-in aliases only. A user-defined
//! `Set-Alias` is invisible here, which is a real limit and the reason a deny
//! rule on a shell is advisory rather than a boundary.

use crate::lexer::BashShape;

/// Parse a PowerShell command into its shape.
pub fn parse_powershell_shape(command: &str) -> BashShape {
    match powershell_segments(command, false) {
        Some(mut segments) => match segments.len() {
            0 => BashShape::UnsafeComplex,
            1 => BashShape::Simple(segments.remove(0)),
            _ => BashShape::SafeAndChain(segments),
        },
        None => BashShape::UnsafeComplex,
    }
}

/// The argv of every link, or `None` for anything that is not provably a
/// list of simple commands. `lists_separate` makes `|`, `||` and `;` links
/// too, as `&&` always is; see
/// [`crate::lexer::Grammar::list_operators_separate`] for who may ask.
pub(crate) fn powershell_segments(command: &str, lists_separate: bool) -> Option<Vec<Vec<String>>> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }

    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut token_started = false;
    let characters: Vec<char> = command.chars().collect();
    let mut index = 0usize;

    while index < characters.len() {
        let character = characters[index];
        match character {
            // A backtick escapes the next character, whatever it is. It is
            // also how a newline is continued, so a trailing one with nothing
            // after it is a truncated command rather than a token.
            '`' => {
                let Some(escaped) = characters.get(index + 1) else {
                    return None;
                };
                token.push(*escaped);
                token_started = true;
                index += 2;
            }

            // Single quotes are literal. `''` inside them is one quote —
            // PowerShell's only escape in this mode, and the place a POSIX
            // tokeniser silently splits one token into two.
            '\'' => {
                index += 1;
                loop {
                    match characters.get(index) {
                        None => return None,
                        Some('\'') if characters.get(index + 1) == Some(&'\'') => {
                            token.push('\'');
                            index += 2;
                        }
                        Some('\'') => {
                            index += 1;
                            break;
                        }
                        Some(other) => {
                            token.push(*other);
                            index += 1;
                        }
                    }
                }
                token_started = true;
            }

            // Double quotes interpolate, so anything that could expand is a
            // bail-out rather than a literal.
            '"' => {
                index += 1;
                loop {
                    match characters.get(index) {
                        None => return None,
                        Some('`') => match characters.get(index + 1) {
                            None => return None,
                            Some(escaped) => {
                                token.push(*escaped);
                                index += 2;
                            }
                        },
                        Some('"') if characters.get(index + 1) == Some(&'"') => {
                            token.push('"');
                            index += 2;
                        }
                        Some('"') => {
                            index += 1;
                            break;
                        }
                        // `$(...)` and `@(...)` run arbitrary code inside a
                        // string; there is no argv that describes that.
                        Some('$') | Some('@') if characters.get(index + 1) == Some(&'(') => {
                            return None
                        }
                        Some(other) => {
                            token.push(*other);
                            index += 1;
                        }
                    }
                }
                token_started = true;
            }

            ';' | '|' if lists_separate => {
                if !push_segment(&mut segments, &mut current, &mut token, &mut token_started) {
                    return None;
                }
                let doubled = character == '|' && characters.get(index + 1) == Some(&'|');
                index += if doubled { 2 } else { 1 };
            }

            // Everything below ends the simple-command shape.
            '\n' | '\r' | ';' | '|' | '{' | '}' | '(' | ')' | '<' | '>' => return None,

            // Sub-expressions, array sub-expressions, and here-strings.
            '$' | '@' if characters.get(index + 1) == Some(&'(') => return None,
            '@' if matches!(characters.get(index + 1), Some('\'') | Some('"')) => return None,

            '&' => {
                if characters.get(index + 1) == Some(&'&') {
                    if !push_segment(&mut segments, &mut current, &mut token, &mut token_started) {
                        return None;
                    }
                    index += 2;
                } else {
                    // A bare `&` is the call operator or a background job.
                    return None;
                }
            }

            character if character.is_whitespace() => {
                push_token(&mut current, &mut token, &mut token_started);
                index += 1;
            }

            _ => {
                // `--%` stops PowerShell parsing entirely: everything after
                // it goes to a native program verbatim. Refusing is cheaper
                // than modelling it, and it is rare.
                //
                // Only when it stands alone as a token — `--%H` is an
                // ordinary parameter, and treating its prefix as the
                // stop-parsing token would refuse a command that has none.
                if character == '-'
                    && !token_started
                    && characters.get(index + 1) == Some(&'-')
                    && characters.get(index + 2) == Some(&'%')
                    && !matches!(characters.get(index + 3), Some(next) if !next.is_whitespace())
                {
                    return None;
                }
                token.push(character);
                token_started = true;
                index += 1;
            }
        }
    }

    if !push_segment(&mut segments, &mut current, &mut token, &mut token_started) {
        return None;
    }

    Some(segments)
}

fn push_token(current: &mut Vec<String>, token: &mut String, token_started: &mut bool) {
    // Keyed on `token_started` rather than emptiness, so an explicitly empty
    // argument (`''`) keeps its position instead of vanishing and shifting
    // every argument after it one place left.
    if *token_started {
        current.push(std::mem::take(token));
        *token_started = false;
    }
}

fn push_segment(
    segments: &mut Vec<Vec<String>>,
    current: &mut Vec<String>,
    token: &mut String,
    token_started: &mut bool,
) -> bool {
    push_token(current, token, token_started);
    if current.is_empty() {
        return false;
    }
    segments.push(std::mem::take(current));
    true
}

/// PowerShell's built-in aliases, alias → cmdlet.
///
/// Only the ones that carry authority. An alias for `Get-Location` costs
/// nothing to miss; an alias for `Remove-Item` is a deny rule that does not
/// hold.
const BUILT_IN_ALIASES: &[(&str, &str)] = &[
    ("cat", "Get-Content"),
    ("cd", "Set-Location"),
    ("chdir", "Set-Location"),
    ("copy", "Copy-Item"),
    ("cp", "Copy-Item"),
    ("cpi", "Copy-Item"),
    ("del", "Remove-Item"),
    ("dir", "Get-ChildItem"),
    ("erase", "Remove-Item"),
    ("gc", "Get-Content"),
    ("gci", "Get-ChildItem"),
    ("gcm", "Get-Command"),
    ("gi", "Get-Item"),
    ("gm", "Get-Member"),
    ("gp", "Get-ItemProperty"),
    ("gps", "Get-Process"),
    ("group", "Group-Object"),
    ("iex", "Invoke-Expression"),
    ("ii", "Invoke-Item"),
    ("iwr", "Invoke-WebRequest"),
    ("kill", "Stop-Process"),
    ("ls", "Get-ChildItem"),
    ("mi", "Move-Item"),
    ("mount", "New-PSDrive"),
    ("move", "Move-Item"),
    ("mv", "Move-Item"),
    ("ni", "New-Item"),
    ("ps", "Get-Process"),
    ("pwd", "Get-Location"),
    ("rd", "Remove-Item"),
    ("rdr", "Remove-PSDrive"),
    ("ren", "Rename-Item"),
    ("ri", "Remove-Item"),
    ("rm", "Remove-Item"),
    ("rmdir", "Remove-Item"),
    ("rni", "Rename-Item"),
    ("rp", "Remove-ItemProperty"),
    ("sal", "Set-Alias"),
    ("saps", "Start-Process"),
    ("sc", "Set-Content"),
    ("select", "Select-Object"),
    ("si", "Set-Item"),
    ("sl", "Set-Location"),
    ("sls", "Select-String"),
    ("sp", "Set-ItemProperty"),
    ("spps", "Stop-Process"),
    ("spsv", "Stop-Service"),
    ("start", "Start-Process"),
    ("sv", "Set-Variable"),
    ("where", "Where-Object"),
    ("wget", "Invoke-WebRequest"),
    ("write", "Write-Output"),
];

/// The cmdlet a name refers to, following built-in aliases.
///
/// Returns the input unchanged when it is not an alias — including when it is
/// already a cmdlet, and when it is a native executable such as `git`.
pub fn canonical_cmdlet(name: &str) -> &str {
    // Aliases never carry an extension or a path; skipping those early keeps
    // `C:\tools\rm.exe` from being rewritten into a cmdlet.
    if name.contains(['\\', '/', '.']) {
        return name;
    }
    BUILT_IN_ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map(|(_, cmdlet)| *cmdlet)
        .unwrap_or(name)
}

/// Whether two command names refer to the same cmdlet.
///
/// Case-insensitive, and alias-aware in both directions so a rule written
/// either way matches a command written either way.
pub fn cmdlet_names_match(rule_token: &str, command_token: &str) -> bool {
    rule_token.eq_ignore_ascii_case(command_token)
        || canonical_cmdlet(rule_token).eq_ignore_ascii_case(canonical_cmdlet(command_token))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple(command: &str) -> Vec<String> {
        match parse_powershell_shape(command) {
            BashShape::Simple(argv) => argv,
            other => panic!("expected a simple command for {command:?}, got {other:?}"),
        }
    }

    fn is_complex(command: &str) -> bool {
        matches!(parse_powershell_shape(command), BashShape::UnsafeComplex)
    }

    #[test]
    fn a_windows_path_keeps_its_backslashes() {
        // The bug this module exists for. Under the POSIX tokeniser this
        // produced `C:tempnotes.txt`, so no rule the user could write would
        // ever match and the prompt came back every single time.
        assert_eq!(
            simple(r"Get-Content C:\temp\notes.txt"),
            vec!["Get-Content", r"C:\temp\notes.txt"]
        );
    }

    #[test]
    fn a_unc_path_survives_too() {
        assert_eq!(
            simple(r"Get-Item \\server\share\file"),
            vec!["Get-Item", r"\\server\share\file"]
        );
    }

    #[test]
    fn a_plain_command_splits_on_whitespace() {
        assert_eq!(
            simple("Get-Process -Name pwsh"),
            vec!["Get-Process", "-Name", "pwsh"]
        );
    }

    #[test]
    fn a_backtick_escapes_the_next_character() {
        // PowerShell's escape, where POSIX would read a command substitution
        // and bail.
        assert_eq!(simple("Write-Output a`$b"), vec!["Write-Output", "a$b"]);
        assert_eq!(simple("Write-Output a`;b"), vec!["Write-Output", "a;b"]);
        assert_eq!(simple("Write-Output a` b"), vec!["Write-Output", "a b"]);
    }

    #[test]
    fn a_trailing_backtick_is_a_truncated_command() {
        assert!(is_complex("Get-Process `"));
    }

    #[test]
    fn single_quotes_are_literal_and_double_them_to_escape() {
        assert_eq!(simple("Write-Output 'a b'"), vec!["Write-Output", "a b"]);
        assert_eq!(simple("Write-Output 'it''s'"), vec!["Write-Output", "it's"]);
        assert_eq!(
            simple(r"Write-Output 'C:\a\b'"),
            vec!["Write-Output", r"C:\a\b"]
        );
    }

    #[test]
    fn double_quotes_take_backtick_escapes_and_doubling() {
        assert_eq!(simple(r#"Write-Output "a b""#), vec!["Write-Output", "a b"]);
        assert_eq!(
            simple(r#"Write-Output "say ""hi""""#),
            vec!["Write-Output", r#"say "hi""#]
        );
        assert_eq!(
            simple(r#"Write-Output "a`"b""#),
            vec!["Write-Output", r#"a"b"#]
        );
    }

    #[test]
    fn an_unterminated_quote_is_complex_rather_than_a_token() {
        assert!(is_complex("Write-Output 'unterminated"));
        assert!(is_complex(r#"Write-Output "unterminated"#));
    }

    #[test]
    fn an_environment_variable_is_an_ordinary_token() {
        // `$env:PATH` has no POSIX meaning and must not bail; a command that
        // reads one is still a simple command.
        assert_eq!(
            simple("Write-Output $env:PATH"),
            vec!["Write-Output", "$env:PATH"]
        );
    }

    #[test]
    fn everything_that_can_run_more_than_one_thing_is_complex() {
        // The safety property. Each of these could hide a second command
        // behind a rule written for the first.
        for command in [
            "Get-Process; Remove-Item x",
            "Get-Process | Remove-Item",
            "Get-Process || Remove-Item x",
            "Remove-Item (Get-Content list.txt)",
            "Remove-Item $(Get-Content list.txt)",
            "Remove-Item @(1,2)",
            "Get-ChildItem | ForEach-Object { Remove-Item $_ }",
            "Get-Process > out.txt",
            "Get-Process 2> err.txt",
            "Get-Content < in.txt",
            "Start-Job &",
            "Get-Process\nRemove-Item x",
            "& 'C:\\tools\\thing.exe'",
        ] {
            assert!(is_complex(command), "{command:?} was treated as simple");
        }
    }

    #[test]
    fn a_sub_expression_inside_a_string_is_complex() {
        assert!(is_complex(r#"Write-Output "value is $(Get-Date)""#));
        assert!(is_complex(r#"Write-Output "list is @(1,2)""#));
    }

    #[test]
    fn a_here_string_is_complex() {
        assert!(is_complex("Write-Output @'\nhello\n'@"));
        assert!(is_complex("Write-Output @\"\nhello\n\"@"));
    }

    #[test]
    fn the_stop_parsing_token_is_complex() {
        // After `--%` PowerShell hands the rest to a native program
        // verbatim, so the argv the rule would match is not the argv that
        // runs.
        assert!(is_complex("git --% log --format=%H"));
        assert!(is_complex("Get-Process --%"));
    }

    #[test]
    fn a_parameter_that_merely_starts_with_two_dashes_is_fine() {
        assert_eq!(
            simple("npm install --save-dev"),
            vec!["npm", "install", "--save-dev"]
        );
        assert_eq!(simple("git log --%H"), vec!["git", "log", "--%H"]);
    }

    #[test]
    fn an_and_chain_becomes_segments() {
        match parse_powershell_shape("Get-Process && Get-Service") {
            BashShape::SafeAndChain(segments) => {
                assert_eq!(segments, vec![vec!["Get-Process"], vec!["Get-Service"]]);
            }
            other => panic!("expected a chain, got {other:?}"),
        }
    }

    #[test]
    fn a_chain_with_an_empty_half_is_complex() {
        assert!(is_complex("&& Get-Process"));
        assert!(is_complex("Get-Process &&"));
    }

    #[test]
    fn an_empty_command_is_complex() {
        assert!(is_complex(""));
        assert!(is_complex("   "));
    }

    #[test]
    fn an_explicitly_empty_argument_keeps_its_position() {
        // Dropping it would shift every later argument one place left, so a
        // rule matching on the third argument would be compared against the
        // fourth.
        assert_eq!(
            simple("Write-Output '' second"),
            vec!["Write-Output", "", "second"]
        );
    }

    #[test]
    fn aliases_resolve_to_their_cmdlet() {
        assert_eq!(canonical_cmdlet("rm"), "Remove-Item");
        assert_eq!(canonical_cmdlet("RM"), "Remove-Item");
        assert_eq!(canonical_cmdlet("gci"), "Get-ChildItem");
        assert_eq!(canonical_cmdlet("Remove-Item"), "Remove-Item");
    }

    #[test]
    fn a_non_alias_is_returned_unchanged() {
        assert_eq!(canonical_cmdlet("git"), "git");
        assert_eq!(canonical_cmdlet("Get-Process"), "Get-Process");
        assert_eq!(canonical_cmdlet(""), "");
    }

    #[test]
    fn a_path_or_executable_is_never_rewritten_as_a_cmdlet() {
        // `rm.exe` and `C:\tools\rm` are programs, not the alias.
        assert_eq!(canonical_cmdlet("rm.exe"), "rm.exe");
        assert_eq!(canonical_cmdlet(r"C:\tools\rm"), r"C:\tools\rm");
        assert_eq!(canonical_cmdlet("./rm"), "./rm");
    }

    #[test]
    fn an_alias_cannot_walk_past_a_deny_rule_written_for_the_cmdlet() {
        // The reason the table exists at all: without it,
        // `PowerShell(Remove-Item:*)` as a deny rule is bypassed by typing
        // three characters.
        assert!(cmdlet_names_match("Remove-Item", "rm"));
        assert!(cmdlet_names_match("rm", "Remove-Item"));
        assert!(cmdlet_names_match("del", "ri"));
    }

    #[test]
    fn unrelated_commands_do_not_match() {
        assert!(!cmdlet_names_match("Remove-Item", "Get-Content"));
        assert!(!cmdlet_names_match("rm", "ls"));
        assert!(!cmdlet_names_match("git", "Remove-Item"));
    }

    #[test]
    fn matching_is_case_insensitive_like_powershell_itself() {
        assert!(cmdlet_names_match("get-process", "Get-Process"));
        assert!(cmdlet_names_match("GET-PROCESS", "get-process"));
    }

    #[test]
    fn the_alias_table_has_no_duplicate_or_self_referential_entries() {
        let mut aliases: Vec<&str> = BUILT_IN_ALIASES.iter().map(|(alias, _)| *alias).collect();
        let before = aliases.len();
        aliases.sort_unstable();
        aliases.dedup();
        assert_eq!(aliases.len(), before, "a duplicate alias shadows another");

        for (alias, cmdlet) in BUILT_IN_ALIASES {
            assert!(
                !alias.eq_ignore_ascii_case(cmdlet),
                "{alias} maps to itself"
            );
            // A cmdlet that is itself an alias would make resolution depend
            // on table order.
            assert_eq!(
                canonical_cmdlet(cmdlet),
                *cmdlet,
                "{cmdlet} is itself an alias"
            );
        }
    }
}

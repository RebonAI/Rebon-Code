//! The frozen `exec` argv.
//!
//! The caller builds this shape and has tests pinning it. This module is the
//! other end of that contract, in the same order:
//!
//! ```text
//! sandbox-win.exe exec --quiet
//!   [--deny-read   <path>]...
//!   [--mask-file   <realPath> <fakePath>]...
//!   [--deny-write  <path>]...
//!   [--allow-write <path>]...
//!   [--block-network]
//!   [--inherit-env <NAME>]...
//!   [--env <KEY=VALUE>]...
//!   [--unset-env <KEY>]...
//!   [--cwd <path>]
//!   -- <binShell> <shellArg>... <command>
//! ```
//!
//! ## Unknown flags are an error
//!
//! This is the single most important line in the module. A helper that skips a
//! flag it does not recognise keeps running, exits zero, and reports a confined
//! command — while enforcing strictly less than it was told to. That is the
//! "believed it was blocked, nothing was blocked" failure, and it is why
//! [`crate::core::status`] emits a version the caller can compare.
//!
//! ## What is *not* checked here
//!
//! Three constraints are already discharged on the caller's side, and
//! re-checking them would only add a second, differently-worded refusal for
//! something the helper can never see:
//!
//! * the command line is capped at 30000 UTF-16 units before spawn;
//! * per-exec ACL overrides (`allow_read_overrides` / `allow_write_overrides`)
//!   are refused there with `PerExecAclUnsupported`, so every `--allow-write`
//!   that arrives is session-scoped;
//! * glob write patterns are skipped there with a `GLOB_WRITE_PATTERN` warning,
//!   so every path is concrete.

use std::path::PathBuf;

/// A `--mask-file` pair.
///
/// Kept as a pair even though the fake path is unusable on Windows, so the
/// degradation is visible in one place instead of being erased at parse time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskRule {
    pub real: PathBuf,
    pub fake: PathBuf,
}

/// One parsed `sandbox-win exec` invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecRequest {
    pub quiet: bool,
    pub deny_read: Vec<PathBuf>,
    pub masks: Vec<MaskRule>,
    pub deny_write: Vec<PathBuf>,
    pub allow_write: Vec<PathBuf>,
    pub block_network: bool,
    pub inherit_env: Vec<String>,
    pub set_env: Vec<(String, String)>,
    pub unset_env: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Everything after `--`: the confined process's argv, verbatim.
    pub command: Vec<String>,
}

/// Why an `exec` argv was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgvError {
    /// Refusing is the whole point: a silently ignored flag is a rule the caller
    /// believes is in force and is not.
    #[error(
        "unknown option `{0}` — this sandbox-win is older than the Rebon that called it; \
         reinstall the helper (`sandbox-win.exe install`)"
    )]
    UnknownOption(String),

    #[error("`{flag}` needs a value")]
    MissingValue { flag: &'static str },

    #[error("`{flag}` was given an empty value")]
    EmptyValue { flag: &'static str },

    #[error("`{flag}` was given more than once")]
    DuplicateFlag { flag: &'static str },

    #[error("`--env` needs KEY=VALUE, got `{0}`")]
    MalformedEnv(String),

    #[error("no `--` separator — there is nothing to run")]
    MissingSeparator,

    #[error("`--` was not followed by a command")]
    EmptyCommand,
}

/// Parse the arguments that follow the `exec` subcommand.
pub fn parse_exec(arguments: &[String]) -> Result<ExecRequest, ArgvError> {
    let mut request = ExecRequest::default();
    let mut index = 0usize;
    let mut saw_separator = false;

    while index < arguments.len() {
        let argument = arguments[index].as_str();
        index += 1;

        // Everything past `--` belongs to the confined process and is never
        // interpreted — including anything that looks like a flag.
        if argument == "--" {
            saw_separator = true;
            request.command = arguments[index..].to_vec();
            break;
        }

        match argument {
            "--quiet" => request.quiet = true,
            "--block-network" => request.block_network = true,
            "--deny-read" => {
                request
                    .deny_read
                    .push(path_value(arguments, &mut index, "--deny-read")?);
            }
            "--deny-write" => {
                request
                    .deny_write
                    .push(path_value(arguments, &mut index, "--deny-write")?);
            }
            "--allow-write" => {
                request
                    .allow_write
                    .push(path_value(arguments, &mut index, "--allow-write")?);
            }
            "--mask-file" => {
                let real = path_value(arguments, &mut index, "--mask-file")?;
                let fake = path_value(arguments, &mut index, "--mask-file")?;
                request.masks.push(MaskRule { real, fake });
            }
            "--inherit-env" => {
                request
                    .inherit_env
                    .push(text_value(arguments, &mut index, "--inherit-env")?);
            }
            "--unset-env" => {
                request
                    .unset_env
                    .push(text_value(arguments, &mut index, "--unset-env")?);
            }
            "--env" => {
                let pair = text_value(arguments, &mut index, "--env")?;
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| ArgvError::MalformedEnv(pair.clone()))?;
                if key.is_empty() {
                    return Err(ArgvError::MalformedEnv(pair.clone()));
                }
                request.set_env.push((key.to_string(), value.to_string()));
            }
            "--cwd" => {
                if request.cwd.is_some() {
                    // Two working directories is not a preference to resolve; it means the caller
                    // and the helper disagree about where the command runs.
                    return Err(ArgvError::DuplicateFlag { flag: "--cwd" });
                }
                request.cwd = Some(path_value(arguments, &mut index, "--cwd")?);
            }
            other => return Err(ArgvError::UnknownOption(other.to_string())),
        }
    }

    if !saw_separator {
        return Err(ArgvError::MissingSeparator);
    }
    if request.command.is_empty() {
        return Err(ArgvError::EmptyCommand);
    }
    Ok(request)
}

/// Rebuild the argv a request came from, in the frozen order.
///
/// Not used by the helper at runtime — it exists so the contract can be asserted
/// as a round trip rather than by reading two lists side by side.
pub fn render_exec(request: &ExecRequest) -> Vec<String> {
    let mut arguments = Vec::new();
    if request.quiet {
        arguments.push("--quiet".to_string());
    }
    for path in &request.deny_read {
        arguments.push("--deny-read".into());
        arguments.push(display(path));
    }
    for mask in &request.masks {
        arguments.push("--mask-file".into());
        arguments.push(display(&mask.real));
        arguments.push(display(&mask.fake));
    }
    for path in &request.deny_write {
        arguments.push("--deny-write".into());
        arguments.push(display(path));
    }
    for path in &request.allow_write {
        arguments.push("--allow-write".into());
        arguments.push(display(path));
    }
    if request.block_network {
        arguments.push("--block-network".into());
    }
    for name in &request.inherit_env {
        arguments.push("--inherit-env".into());
        arguments.push(name.clone());
    }
    for (key, value) in &request.set_env {
        arguments.push("--env".into());
        arguments.push(format!("{key}={value}"));
    }
    for key in &request.unset_env {
        arguments.push("--unset-env".into());
        arguments.push(key.clone());
    }
    if let Some(cwd) = &request.cwd {
        arguments.push("--cwd".into());
        arguments.push(display(cwd));
    }
    arguments.push("--".into());
    arguments.extend(request.command.iter().cloned());
    arguments
}

fn display(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

fn text_value(
    arguments: &[String],
    index: &mut usize,
    flag: &'static str,
) -> Result<String, ArgvError> {
    let value = arguments
        .get(*index)
        .ok_or(ArgvError::MissingValue { flag })?
        .clone();
    // `--` is the separator and can never be a value. Consuming it would swallow the
    // boundary and turn the confined command's argv into flags — `--mask-file C:\a
    // -- cmd.exe` would report "unknown option cmd.exe", which points the reader at
    // the wrong argument entirely.
    if value == "--" {
        return Err(ArgvError::MissingValue { flag });
    }
    *index += 1;
    if value.is_empty() {
        return Err(ArgvError::EmptyValue { flag });
    }
    Ok(value)
}

/// A path value, refused when empty.
///
/// An empty path is not a harmless no-op: it resolves to the current directory,
/// so an empty `--deny-write` would place a deny ACE on the project the command
/// is supposed to be able to write.
fn path_value(
    arguments: &[String],
    index: &mut usize,
    flag: &'static str,
) -> Result<PathBuf, ArgvError> {
    text_value(arguments, index, flag).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &[&str]) -> Vec<String> {
        line.iter().map(|word| (*word).to_string()).collect()
    }

    /// A fully-populated session's argv, transcribed from the caller's own tests.
    fn caller_argv() -> Vec<String> {
        words(&[
            "--quiet",
            "--deny-read",
            r"C:\Users\u\.ssh",
            "--mask-file",
            r"C:\Users\u\.npmrc",
            r"C:\tmp\fake",
            "--deny-write",
            r"C:\work\vendor",
            "--allow-write",
            r"C:\work",
            "--block-network",
            "--inherit-env",
            "PATH",
            "--inherit-env",
            "PATHEXT",
            "--env",
            "GIT_CONFIG_COUNT=1",
            "--unset-env",
            "no_proxy",
            "--unset-env",
            "NO_PROXY",
            "--cwd",
            r"C:\work",
            "--",
            "pwsh.exe",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            "ZQBjAGgAbwA=",
        ])
    }

    #[test]
    fn the_callers_argv_parses_into_every_field() {
        let request = parse_exec(&caller_argv()).unwrap();

        assert!(request.quiet);
        assert_eq!(request.deny_read, vec![PathBuf::from(r"C:\Users\u\.ssh")]);
        assert_eq!(
            request.masks,
            vec![MaskRule {
                real: PathBuf::from(r"C:\Users\u\.npmrc"),
                fake: PathBuf::from(r"C:\tmp\fake"),
            }]
        );
        assert_eq!(request.deny_write, vec![PathBuf::from(r"C:\work\vendor")]);
        assert_eq!(request.allow_write, vec![PathBuf::from(r"C:\work")]);
        assert!(request.block_network);
        assert_eq!(request.inherit_env, vec!["PATH", "PATHEXT"]);
        assert_eq!(
            request.set_env,
            vec![("GIT_CONFIG_COUNT".to_string(), "1".to_string())]
        );
        assert_eq!(request.unset_env, vec!["no_proxy", "NO_PROXY"]);
        assert_eq!(request.cwd, Some(PathBuf::from(r"C:\work")));
        assert_eq!(
            request.command,
            words(&[
                "pwsh.exe",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                "ZQBjAGgAbwA="
            ])
        );
    }

    #[test]
    fn the_callers_argv_round_trips_byte_for_byte() {
        // The strongest form of "the contract is frozen": rendering what we parsed
        // reproduces the caller's own argument order.
        let argv = caller_argv();
        assert_eq!(render_exec(&parse_exec(&argv).unwrap()), argv);
    }

    #[test]
    fn a_parsed_request_round_trips_through_render() {
        let request = parse_exec(&caller_argv()).unwrap();
        assert_eq!(parse_exec(&render_exec(&request)).unwrap(), request);
    }

    #[test]
    fn the_minimal_argv_is_a_separator_and_a_command() {
        let request = parse_exec(&words(&["--", "cmd.exe"])).unwrap();
        assert_eq!(request.command, vec!["cmd.exe"]);
        assert!(!request.quiet);
        assert!(!request.block_network);
    }

    #[test]
    fn an_unknown_option_is_refused_rather_than_ignored() {
        // The regression this guards: a newer Rebon passes a rule an older helper does
        // not implement. Ignoring it would run the command with one fewer restriction
        // and report success.
        let error = parse_exec(&words(&["--deny-exec", r"C:\x", "--", "cmd.exe"])).unwrap_err();
        assert_eq!(error, ArgvError::UnknownOption("--deny-exec".into()));
        assert!(error.to_string().contains("older than the Rebon"));
    }

    #[test]
    fn an_unknown_option_after_the_separator_is_the_commands_business() {
        let request = parse_exec(&words(&["--", "cmd.exe", "--deny-exec", "--quiet"])).unwrap();
        assert_eq!(
            request.command,
            words(&["cmd.exe", "--deny-exec", "--quiet"])
        );
        assert!(
            !request.quiet,
            "a flag after `--` must not reach the helper"
        );
    }

    #[test]
    fn a_flag_missing_its_value_is_refused() {
        for flag in ["--deny-read", "--deny-write", "--allow-write", "--cwd"] {
            let error = parse_exec(&words(&[flag])).unwrap_err();
            assert_eq!(error, ArgvError::MissingValue { flag: leak(flag) });
        }
    }

    #[test]
    fn mask_file_needs_both_of_its_paths() {
        let error = parse_exec(&words(&["--mask-file", r"C:\a", "--", "cmd.exe"])).unwrap_err();
        assert_eq!(
            error,
            ArgvError::MissingValue {
                flag: "--mask-file"
            },
            "the separator must not be eaten as the fake path"
        );
    }

    #[test]
    fn no_flag_can_consume_the_separator_as_its_value() {
        // Without this the boundary moves and the confined command's own arguments start
        // being parsed as helper flags — reported as an unknown option pointing at
        // entirely the wrong argument.
        for flag in [
            "--deny-read",
            "--deny-write",
            "--allow-write",
            "--cwd",
            "--env",
            "--inherit-env",
            "--unset-env",
            "--mask-file",
        ] {
            let error = parse_exec(&words(&[flag, "--", "cmd.exe"])).unwrap_err();
            assert_eq!(
                error,
                ArgvError::MissingValue { flag: leak(flag) },
                "{flag} ate the separator"
            );
        }
    }

    #[test]
    fn an_empty_path_is_refused_because_it_means_the_current_directory() {
        let error = parse_exec(&words(&["--deny-write", "", "--", "cmd.exe"])).unwrap_err();
        assert_eq!(
            error,
            ArgvError::EmptyValue {
                flag: "--deny-write"
            }
        );
    }

    #[test]
    fn env_pairs_must_have_a_key_and_a_separator() {
        assert_eq!(
            parse_exec(&words(&["--env", "NOEQUALS", "--", "x"])).unwrap_err(),
            ArgvError::MalformedEnv("NOEQUALS".into())
        );
        assert_eq!(
            parse_exec(&words(&["--env", "=orphan", "--", "x"])).unwrap_err(),
            ArgvError::MalformedEnv("=orphan".into())
        );
    }

    #[test]
    fn an_env_value_may_be_empty_and_may_contain_equals_signs() {
        let request =
            parse_exec(&words(&["--env", "EMPTY=", "--env", "URL=a=b", "--", "x"])).unwrap();
        assert_eq!(
            request.set_env,
            vec![
                ("EMPTY".to_string(), String::new()),
                ("URL".to_string(), "a=b".to_string()),
            ]
        );
    }

    #[test]
    fn two_working_directories_are_a_refusal_not_a_last_one_wins() {
        let error =
            parse_exec(&words(&["--cwd", r"C:\a", "--cwd", r"C:\b", "--", "x"])).unwrap_err();
        assert_eq!(error, ArgvError::DuplicateFlag { flag: "--cwd" });
    }

    #[test]
    fn a_missing_separator_is_refused() {
        assert_eq!(
            parse_exec(&words(&["--quiet", "cmd.exe"])).unwrap_err(),
            ArgvError::UnknownOption("cmd.exe".into())
        );
        assert_eq!(
            parse_exec(&words(&["--quiet"])).unwrap_err(),
            ArgvError::MissingSeparator
        );
    }

    #[test]
    fn a_separator_with_nothing_after_it_is_refused() {
        assert_eq!(
            parse_exec(&words(&["--quiet", "--"])).unwrap_err(),
            ArgvError::EmptyCommand
        );
    }

    #[test]
    fn repeated_rules_accumulate_in_order() {
        let request = parse_exec(&words(&[
            "--deny-read",
            r"C:\a",
            "--deny-read",
            r"C:\b",
            "--allow-write",
            r"C:\w1",
            "--allow-write",
            r"C:\w2",
            "--",
            "x",
        ]))
        .unwrap();
        assert_eq!(
            request.deny_read,
            vec![PathBuf::from(r"C:\a"), PathBuf::from(r"C:\b")]
        );
        assert_eq!(
            request.allow_write,
            vec![PathBuf::from(r"C:\w1"), PathBuf::from(r"C:\w2")]
        );
    }

    #[test]
    fn flags_are_accepted_in_any_order_even_though_the_caller_emits_one() {
        // The caller's order is frozen; accepting only that order would make the helper
        // unusable by hand for no gain.
        let request = parse_exec(&words(&[
            "--cwd",
            r"C:\w",
            "--block-network",
            "--quiet",
            "--deny-read",
            r"C:\s",
            "--",
            "x",
        ]))
        .unwrap();
        assert!(request.quiet && request.block_network);
        assert_eq!(request.cwd, Some(PathBuf::from(r"C:\w")));
    }

    #[test]
    fn a_command_containing_a_bare_double_dash_keeps_it() {
        let request = parse_exec(&words(&["--", "git", "log", "--", "path"])).unwrap();
        assert_eq!(request.command, words(&["git", "log", "--", "path"]));
    }

    /// `MissingValue` carries a `&'static str`; tests build flag names at runtime, so
    /// this maps the handful of known names back to statics.
    fn leak(flag: &str) -> &'static str {
        match flag {
            "--deny-read" => "--deny-read",
            "--deny-write" => "--deny-write",
            "--allow-write" => "--allow-write",
            "--cwd" => "--cwd",
            "--mask-file" => "--mask-file",
            "--env" => "--env",
            "--inherit-env" => "--inherit-env",
            "--unset-env" => "--unset-env",
            other => panic!("unhandled flag {other}"),
        }
    }
}

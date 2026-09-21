//! Subcommand dispatch.
//!
//! Hand-rolled rather than delegated to a parser library, for one reason:
//! `exec`'s argv is a frozen contract whose whole point is that everything after
//! `--` reaches the confined process untouched, and that an unrecognised flag
//! before it is a *refusal* rather than something skipped. Both are exactly the
//! sort of thing an argument parser has its own ideas about, and this is forty
//! lines.

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Non-elevated, read-only, exits zero.
    Status,
    /// Everything after `exec` is handed to the argv parser verbatim.
    Exec(Vec<String>),
    /// Undo the ACEs of sessions that are gone.
    Reap,
    /// One UAC prompt; creates the accounts and the filters.
    Install,
    /// Removes every trace.
    Uninstall,
    /// Runs a real command through each mechanism.
    SelfTest,
    Help,
    Version,
}

/// Parse `argv[1..]`.
pub fn parse_command(arguments: &[String]) -> Result<Command, String> {
    let Some(first) = arguments.first() else {
        return Ok(Command::Help);
    };

    match first.as_str() {
        "status" => expect_no_more(Command::Status, &arguments[1..], "status"),
        "reap" => expect_no_more(Command::Reap, &arguments[1..], "reap"),
        "install" => expect_no_more(Command::Install, &arguments[1..], "install"),
        "uninstall" => expect_no_more(Command::Uninstall, &arguments[1..], "uninstall"),
        "selftest" => expect_no_more(Command::SelfTest, &arguments[1..], "selftest"),
        // Not validated here. `exec`'s argv has its own frozen grammar and its own
        // error type, and re-checking a prefix of it in two places is how the two drift
        // apart.
        "exec" => Ok(Command::Exec(arguments[1..].to_vec())),
        "--help" | "-h" | "help" => Ok(Command::Help),
        "--version" | "-V" => Ok(Command::Version),
        other => Err(format!(
            "unknown command `{other}` — expected one of install, uninstall, status, \
             selftest, exec, reap"
        )),
    }
}

fn expect_no_more(
    command: Command,
    rest: &[String],
    name: &'static str,
) -> Result<Command, String> {
    match rest.first() {
        None => Ok(command),
        Some(extra) => Err(format!("`{name}` takes no arguments, got `{extra}`")),
    }
}

pub const USAGE: &str = "\
sandbox-win — the Windows sandbox helper for Rebon

USAGE:
    sandbox-win.exe <COMMAND>

COMMANDS:
    install      Create the sandbox accounts and install the network filters.
                 Asks for administrator rights, once.
    uninstall    Remove everything install created, plus any ACEs still on
                 disk.
    status       Report what is set up. Reads only; never elevates.
    selftest     Not implemented yet. Will run a real command through each
                 mechanism and check that it is actually confined; today it
                 refuses instead of reporting a result it cannot produce.
    exec ...     Run a command inside the sandbox. See the Rebon sandbox
                 documentation; the argument grammar is not stable for
                 hand use.
    reap         Undo ACEs left behind by Rebon sessions that are no longer
                 running.

Running a command does not need administrator rights. Only `install` and
`uninstall` do.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &[&str]) -> Vec<String> {
        line.iter().map(|word| (*word).to_string()).collect()
    }

    #[test]
    fn every_documented_subcommand_parses() {
        assert_eq!(parse_command(&words(&["status"])).unwrap(), Command::Status);
        assert_eq!(parse_command(&words(&["reap"])).unwrap(), Command::Reap);
        assert_eq!(
            parse_command(&words(&["install"])).unwrap(),
            Command::Install
        );
        assert_eq!(
            parse_command(&words(&["uninstall"])).unwrap(),
            Command::Uninstall
        );
        assert_eq!(
            parse_command(&words(&["selftest"])).unwrap(),
            Command::SelfTest
        );
    }

    #[test]
    fn exec_hands_everything_after_it_over_untouched() {
        // Including the flags. `exec` owns its own grammar, and a dispatcher that peeked
        // at the first argument would have to be kept in step with it forever.
        let command =
            parse_command(&words(&["exec", "--quiet", "--", "cmd.exe", "--help"])).unwrap();
        assert_eq!(
            command,
            Command::Exec(words(&["--quiet", "--", "cmd.exe", "--help"]))
        );
    }

    #[test]
    fn exec_with_nothing_after_it_still_reaches_the_argv_parser() {
        // So the refusal comes from the one place that knows what the grammar is, with
        // the message that names the missing separator.
        assert_eq!(
            parse_command(&words(&["exec"])).unwrap(),
            Command::Exec(vec![])
        );
    }

    #[test]
    fn no_arguments_prints_usage() {
        assert_eq!(parse_command(&[]).unwrap(), Command::Help);
    }

    #[test]
    fn an_unknown_command_is_refused_and_lists_the_real_ones() {
        let error = parse_command(&words(&["exce"])).unwrap_err();
        assert!(error.contains("unknown command `exce`"), "{error}");
        assert!(error.contains("exec"), "{error}");
    }

    #[test]
    fn a_read_only_command_with_stray_arguments_is_refused() {
        // `status extra` almost certainly means the caller thinks `status` takes
        // something. Silently ignoring it would answer a question nobody asked.
        let error = parse_command(&words(&["status", "--json"])).unwrap_err();
        assert!(error.contains("takes no arguments"), "{error}");
        assert!(error.contains("--json"), "{error}");
    }

    #[test]
    fn help_and_version_have_the_usual_spellings() {
        for spelling in ["--help", "-h", "help"] {
            assert_eq!(parse_command(&words(&[spelling])).unwrap(), Command::Help);
        }
        for spelling in ["--version", "-V"] {
            assert_eq!(
                parse_command(&words(&[spelling])).unwrap(),
                Command::Version
            );
        }
    }

    #[test]
    fn the_usage_text_says_which_commands_need_elevation() {
        // The single most important fact about this binary for anyone reading its help:
        // running a command does not prompt.
        assert!(USAGE.contains("administrator rights"), "{USAGE}");
        assert!(USAGE.contains("does not need administrator"), "{USAGE}");
    }

    #[test]
    fn the_usage_text_lists_every_command_the_dispatcher_accepts() {
        for name in ["install", "uninstall", "status", "selftest", "exec", "reap"] {
            assert!(USAGE.contains(name), "usage does not mention {name}");
        }
    }
}

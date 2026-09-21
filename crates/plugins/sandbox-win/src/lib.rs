//! The binary half of the `sandbox` plugin: `sandbox-win.exe`, the Windows
//! sandbox helper Rebon shells out to. It defines no `PluginDef` of its own;
//! its whole contribution is a process that ships beside `rebon`.
//!
//! Windows has no single primitive that does what bubblewrap or seatbelt do, so
//! confinement here is four unrelated mechanisms working together: a downgraded
//! account for the security context, WFP filters keyed on that account's SID for
//! the network, deny ACEs for the filesystem, and a Job Object for the process
//! tree. Three of the four need administrator rights to *set up*, and none can
//! be applied from inside the process being confined, which is why this is a
//! separate binary rather than a module inside `rebon.exe`.
//!
//! ## Current state
//!
//! `install`, `uninstall`, `status`, `reap` and `exec` are implemented: `exec`
//! provisions the ACEs, launches the command as the downgraded account, and
//! relays its exit code. `selftest` is the one subcommand still unimplemented,
//! and it refuses.
//!
//! A helper that exited zero without confining anything would make every
//! sandboxed command report success while running with the caller's full rights
//! — precisely the state Rebon's confinement probe exists to catch. So every
//! failure path here refuses (126 plus a `sandbox-win:` line) rather than
//! running a command unconfined.
//!
//! ## Two output rules that are contract
//!
//! * **stdout belongs to the confined process.** Under `exec` this binary
//!   writes nothing to it — `--quiet` is always passed and everything the helper
//!   has to say goes to stderr.
//! * **stderr carries markers.** A failure is `sandbox-win: <reason>`; a denial
//!   is `SANDBOX_WIN_DENIED <op> <detail>`. Those two prefixes are the only
//!   thing that lets Rebon tell the model "the sandbox refused this" rather than
//!   leaving it to read a bare `Access is denied` from deep inside some tool as
//!   a bug in the user's project.
//!
//! ## The three layers
//!
//! One crate, three modules, and the seam between the first two is the
//! load-bearing one:
//!
//! * [`core`] — pure decision logic, no platform gate and no Win32, so the
//!   whole security-critical verdict matrix runs under `cargo test` on Linux and
//!   macOS as well as Windows.
//! * [`sys`] — the Win32 shell. Off Windows every entry point compiles to
//!   `SysError::Unsupported`.
//! * [`helper`] — the subcommands themselves: argument dispatch, `exec`,
//!   `install` / `uninstall`, `reap` and `status`.
//!
//! [`run`] is the whole of `src/bin/sandbox-win.rs`.

pub mod core;
pub mod helper;
pub mod sys;

use crate::core::markers::{failure, EXIT_HELPER_FAILURE};
use crate::helper::cli::Command;
use crate::helper::{cli, exec, provision, reap, status};

/// The helper's entry point: dispatch one already-collected argv and hand back
/// the process exit code.
pub fn run(arguments: &[String]) -> i32 {
    let command = match cli::parse_command(arguments) {
        Ok(command) => command,
        Err(reason) => {
            eprintln!("{}", failure(&reason));
            return EXIT_HELPER_FAILURE;
        }
    };

    match command {
        Command::Help => {
            print!("{}", cli::USAGE);
            0
        }
        Command::Version => {
            println!("sandbox-win {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Command::Status => {
            // Always zero. The caller runs this on every session start and reads stdout;
            // "nothing is installed" is a normal answer, not a spawn failure.
            print!("{}", status::render_report(&status::probe()));
            0
        }
        Command::Reap => reap::run(),
        Command::Exec(arguments) => exec::run(&arguments),
        Command::Install => provision::install(),
        Command::Uninstall => provision::uninstall(),
        Command::SelfTest => not_yet("selftest"),
    }
}

/// `selftest` — the one subcommand not yet implemented.
fn not_yet(command: &str) -> i32 {
    eprintln!(
        "{}",
        failure(&format!(
            "`{command}` is not implemented yet — it would run a real command through \
             each mechanism to prove the sandbox actually bites. The rest of the \
             helper — install, uninstall, status, reap, exec — is implemented."
        ))
    );
    EXIT_HELPER_FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &[&str]) -> Vec<String> {
        line.iter().map(|word| (*word).to_string()).collect()
    }

    #[test]
    fn help_and_version_succeed() {
        assert_eq!(run(&[]), 0);
        assert_eq!(run(&words(&["--help"])), 0);
        assert_eq!(run(&words(&["--version"])), 0);
    }

    #[test]
    fn status_always_exits_zero() {
        // Contract, not politeness: the caller runs this on every session start, and a
        // machine with nothing installed is the overwhelmingly common case.
        assert_eq!(run(&words(&["status"])), 0);
    }

    #[test]
    fn an_unknown_command_fails_with_the_helper_exit_code() {
        assert_eq!(run(&words(&["exce"])), EXIT_HELPER_FAILURE);
    }

    #[test]
    fn selftest_is_the_one_subcommand_still_unimplemented() {
        // It must refuse rather than exit zero: a caller that read success would believe
        // a mechanism was verified when nothing ran. The implemented privileged commands
        // (install / uninstall) have their own elevation-refusal test in `provision`.
        assert_eq!(run(&words(&["selftest"])), EXIT_HELPER_FAILURE);
    }

    #[test]
    fn exec_refuses_when_the_sandbox_is_not_installed() {
        // `exec` is implemented: with the sandbox provisioned it launches the command
        // for real and relays its exit code, so this cannot assert a fixed code for a
        // well-formed argv. It pins the direction that must always hold — with no
        // accounts on the machine, `exec` refuses (126) rather than running the command
        // unconfined. On a machine that *does* have them (or a non-Windows build, where
        // the probe is unsupported), the assertion is skipped.
        let installed = crate::sys::principal::probe_accounts()
            .map(|probe| probe.is_complete())
            .unwrap_or(false);
        if installed {
            return;
        }
        assert_eq!(
            run(&words(&[
                "exec", "--quiet", "--", "cmd.exe", "/c", "echo", "hi"
            ])),
            EXIT_HELPER_FAILURE
        );
    }

    #[test]
    fn a_malformed_exec_argv_refuses() {
        assert_eq!(run(&words(&["exec", "--nonsense"])), EXIT_HELPER_FAILURE);
    }
}

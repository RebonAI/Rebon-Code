//! Marking every failing step of a Bash command in its stderr.
//!
//! A command chained with `;` or newlines runs every step whatever the last
//! one did, and its `exitCode` is the last step's. `cargo build; cargo test`
//! that fails to build still reports the test run's status, and the build
//! error is somewhere in the middle of the output. A one-line marker on
//! stderr for each failing step — `[rebon: exit 101: cargo build]` — names
//! the step that failed, and truncation keeps the lines around it (see
//! [`crate::output_truncation`]).
//!
//! The marker comes from an `ERR` trap the tool prepends to the payload the
//! shell runs; the command the model wrote, which permission rules and the
//! sandbox policy see, is unchanged. Only bash has the trap — `sh` on macOS
//! is bash, on Debian it is dash — so the prologue checks `$BASH_VERSION`
//! and does nothing anywhere else. What the trap does not change:
//!
//! * the exit status, of each step and of the command — `$?` is read first
//!   and bash restores it after a trap;
//! * failures bash itself does not report to `ERR`: an `if` / `while`
//!   condition, a non-final link of `&&` / `||`, a `!`-negated command;
//! * output the command sends elsewhere: the marker is printed only while
//!   stderr is a pipe of its own, so `{ a; b; } 2>&1`, `2>/dev/null` and
//!   `> log 2>&1` never receive one;
//! * a trap the command sets itself, which replaces this one.
//!
//! It is written on the same line as the command so the line numbers in the
//! shell's own error messages stay the command's.

/// What every marker line starts with.
pub(crate) const MARKER_PREFIX: &str = "[rebon: exit ";

/// The trap. `%.200s` caps how much of a long command the marker repeats.
const PROLOGUE: &str = "if [ -n \"${BASH_VERSION-}\" ]; then trap '__rebon_status=$?; \
[ -p /dev/fd/2 ] && ! [ /dev/fd/2 -ef /dev/fd/1 ] && \
printf \"[rebon: exit %d: %.200s]\\n\" \"$__rebon_status\" \"$BASH_COMMAND\" >&2' ERR; fi; ";

/// The payload the shell runs for `command`: the trap, then the command.
pub(crate) fn with_failure_markers(command: &str) -> String {
    format!("{PROLOGUE}{command}")
}

/// Whether a line of stderr is a marker.
pub(crate) fn is_marker_line(line: &str) -> bool {
    line.starts_with(MARKER_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prologue_prints_the_prefix_the_readers_look_for() {
        assert!(PROLOGUE.contains(&format!("printf \"{MARKER_PREFIX}")));
        assert!(is_marker_line("[rebon: exit 1: false]"));
        assert!(!is_marker_line("error: [rebon: exit 1: false]"));
    }

    #[test]
    fn the_command_follows_the_prologue_on_the_same_line() {
        let payload = with_failure_markers("false; echo ok");
        assert!(payload.ends_with("fi; false; echo ok"));
        assert!(!payload.contains('\n'));
    }

    #[cfg(unix)]
    fn run(payload: &str) -> Option<(Option<i32>, String, String)> {
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(payload)
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        Some((
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_step_is_marked_and_nothing_else_changes() {
        let Some((code, stdout, stderr)) = run(&with_failure_markers("false; echo ok")) else {
            return; // no bash on this machine
        };
        assert_eq!(code, Some(0));
        assert_eq!(stdout, "ok\n");
        assert_eq!(stderr, "[rebon: exit 1: false]\n");

        // The last step's status is still the command's.
        let (code, _, stderr) =
            run(&with_failure_markers("echo a; sh -c 'exit 3'")).expect("bash ran once");
        assert_eq!(code, Some(3));
        assert!(
            stderr.contains("[rebon: exit 3: sh -c 'exit 3']"),
            "{stderr}"
        );

        // `$?` after a failing step is that step's.
        let (_, stdout, _) =
            run(&with_failure_markers("sh -c 'exit 4'; echo \"$?\"")).expect("bash ran");
        assert_eq!(stdout, "4\n");
    }

    #[cfg(unix)]
    #[test]
    fn conditions_and_redirected_stderr_get_no_marker() {
        for command in [
            "if false; then :; fi",
            "false || true",
            "false && true; true",
            "! false",
            "{ false; true; } 2>&1",
            "{ false; true; } 2>/dev/null",
        ] {
            let Some((_, stdout, stderr)) = run(&with_failure_markers(command)) else {
                return;
            };
            assert!(!stdout.contains(MARKER_PREFIX), "{command:?}: {stdout}");
            assert!(!stderr.contains(MARKER_PREFIX), "{command:?}: {stderr}");
        }
    }
}

//! Terminal-level tests for `rebon_session_runtime::commands::control`.
//!
//! They live here rather than beside the code because each of them
//! builds an `AppState`, calls `crate::session_shell::session_command_inputs_from_app`, or
//! drives the TUI reducer — all three are the binary's, so a crate
//! that must not know what a terminal is cannot host them.

use crate::session::commands::control::*;
use crate::session::commands::ConfigDirGuard;
use rebon_slash_commands::Surface;

use crate::tui::app::AppState;
use crate::tui::runner::test_support::make_test_tui_session;
use tempfile::TempDir;

#[test]
fn session_control_serves_status_cost_and_mcp_without_arguments() {
    let tempdir = TempDir::new().expect("tempdir");
    let _config_dir = ConfigDirGuard::set(tempdir.path());

    let app = AppState::new();
    let session = make_test_tui_session();
    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let run = |name: &str| execute_session_control_command(&inputs, &session, name, &[]);

    let status = run("status").expect("/status is a session control command");
    assert!(
        status.output.text.contains("Session status"),
        "{}",
        status.output.text
    );
    let cost = run("cost").expect("/cost is a session control command");
    assert!(
        cost.output.text.contains("Cost estimate (local)"),
        "{}",
        cost.output.text
    );
    let mcp = run("mcp").expect("/mcp is a session control command");
    assert!(mcp.output.text.contains("loader:"), "{}", mcp.output.text);

    for result in [&status, &cost, &mcp] {
        assert_eq!(result.output.tone, "info");
        assert!(result.replay_requests.is_empty());
    }

    let args = ["extra".to_string()];
    for name in ["status", "cost"] {
        match execute_session_control_command(&inputs, &session, name, &args) {
            Err(err) => assert_eq!(err, format!("/{name} does not accept arguments")),
            Ok(_) => panic!("/{name} must reject arguments"),
        }
    }

    // `/mcp` does take arguments now. An unknown one is a usage error
    // rather than a flat refusal: the command exists, the word was wrong.
    match execute_session_control_command(&inputs, &session, "mcp", &args) {
        Err(err) => assert!(err.contains("usage: /mcp"), "{err}"),
        Ok(_) => panic!("/mcp must reject an unknown subcommand"),
    }
}

/// The twelve the surface bit carried the day the hand-written list was
/// deleted, so the boot below is checked to have found the whole set and
/// not, say, eleven of them with the `memory` plugin off.
const SESSION_CONTROL_COMMANDS: &[&str] = &[
    "context",
    "memory",
    "doctor",
    "status",
    "cost",
    "mcp",
    "hooks",
    "compact",
    "prune",
    "permissions",
    "kernel",
    "backend",
];

/// Every command carrying the `SESSION_CONTROL` bit, off the booted seat.
///
/// The kernel is booted because `/memory` lives in its plugin and
/// the compiled-in table no longer holds it; reading the table alone
/// would quietly check eleven commands and call it twelve.
fn forwardable_command_names() -> Vec<String> {
    rebon_harness::kernel_bootstrap::process_kernel();
    let mut names: Vec<String> = rebon_slash_commands::all()
        .into_iter()
        .filter(|spec| spec.available_on(Surface::SessionControl))
        .map(|spec| spec.name.to_string())
        .collect();
    names.sort_unstable();
    let mut expected: Vec<String> = SESSION_CONTROL_COMMANDS
        .iter()
        .map(|s| s.to_string())
        .collect();
    expected.sort_unstable();
    assert_eq!(names, expected, "the surface bit lost or gained a command");
    names
}

/// The forwarding gate and the dispatcher must agree on the closed set.
/// A name that carries the bit but is not dispatchable would be forwarded
/// to a worker that refuses it; a name dispatchable without the bit would
/// silently run in the mirroring UI process against an empty shell
/// session.
#[test]
fn every_session_control_command_is_dispatchable() {
    let tempdir = TempDir::new().expect("tempdir");
    let _config_dir = ConfigDirGuard::set(tempdir.path());
    let app = AppState::new();
    let session = make_test_tui_session();
    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);

    for name in forwardable_command_names() {
        let refused = matches!(
            execute_session_control_command(&inputs, &session, &name, &[]),
            Err(ref err) if err.starts_with("unsupported session command")
        );
        assert!(
            !refused,
            "/{name} carries the bit but the dispatcher refuses it"
        );
    }
}

/// The other direction, which is the one that was missing: `/backend` was
/// added to the dispatcher and not to the list, and the list is what the
/// forwarding gate reads — so a terminal attached to a worker could not
/// reach it, and a terminal that owned its session sent it to the model as
/// prompt text.
///
/// Candidates come from the catalog rather than a second hand-written list,
/// so a command wired into the dispatcher later fails here without anyone
/// remembering to add it.
///
/// One command is deliberately answered without being listed. `/rewind`
/// typed in a terminal opens the turn picker; forwarding the word would
/// replace that interaction with a command nobody composed. The dispatcher
/// answers it because an owner receives it as a typed `Rewind` request,
/// with the turn already chosen. Anything else showing up here is the bug
/// this test was written for.
#[test]
fn the_match_answers_nothing_the_list_omits() {
    /// Answered by the dispatcher, deliberately absent from the forwarding
    /// list. See above.
    const ANSWERED_BUT_NOT_FORWARDED: &[&str] = &["rewind"];

    let tempdir = TempDir::new().expect("tempdir");
    let _config_dir = ConfigDirGuard::set(tempdir.path());
    let app = AppState::new();
    let session = make_test_tui_session();
    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);

    let forwardable = forwardable_command_names();
    for spec in rebon_slash_commands::all() {
        let carries_bit = forwardable.contains(&spec.name.to_string());
        if ANSWERED_BUT_NOT_FORWARDED.contains(&spec.name.as_ref()) {
            assert!(
                !carries_bit,
                "/{} is exempted from forwarding but carries the bit",
                spec.name
            );
            continue;
        }
        let answered = !matches!(
            execute_session_control_command(&inputs, &session, spec.name.as_ref(), &[]),
            Err(ref err) if err.starts_with("unsupported session command")
        );
        assert_eq!(
            answered, carries_bit,
            "/{} is answered by the dispatcher but does not carry the \
                 SESSION_CONTROL bit (or the reverse)",
            spec.name
        );
    }
}

#[test]
fn only_session_control_commands_parse_for_forwarding() {
    rebon_harness::kernel_bootstrap::process_kernel();
    assert_eq!(
        parse_session_control_command("/compact keep the api notes"),
        Some((
            "compact".to_string(),
            vec!["keep".into(), "the".into(), "api".into(), "notes".into()]
        ))
    );
    // Case and surrounding whitespace are the user's, not the protocol's.
    assert_eq!(
        parse_session_control_command("  /COST  "),
        Some(("cost".to_string(), Vec::new()))
    );
    // Local UI commands stay local.
    for local in ["/vim", "/help", "/theme", "/new", "/skills"] {
        assert_eq!(parse_session_control_command(local), None, "{local}");
    }
    // Plain prompts are not commands.
    assert_eq!(parse_session_control_command("compact the notes"), None);
    assert_eq!(parse_session_control_command("/"), None);
}

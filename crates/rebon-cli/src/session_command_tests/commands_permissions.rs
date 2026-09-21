//! Terminal-level tests for `rebon_session_runtime::commands::permissions`.
//!
//! They live here rather than beside the code because each of them
//! builds an `AppState`, calls `crate::session_shell::session_command_inputs_from_app`, or
//! drives the TUI reducer — all three are the binary's, so a crate
//! that must not know what a terminal is cannot host them.

use crate::session::commands::permissions::*;
use rebon_permissions::auto_mode_denials::AutoModeDenialInput;
use std::sync::Arc;

use crate::tui::app::AppState;

#[test]
fn parse_bare_permissions() {
    assert_eq!(
        parse_permissions_command("/permissions"),
        Some(PermissionsCommand::List)
    );
}

#[test]
fn parse_approve_retry_and_clear() {
    assert_eq!(
        parse_permissions_command("/permissions approve auto-mode-denial-1"),
        Some(PermissionsCommand::Approve("auto-mode-denial-1".into())),
    );
    assert_eq!(
        parse_permissions_command("/permissions retry auto-mode-denial-1"),
        Some(PermissionsCommand::Retry("auto-mode-denial-1".into())),
    );
    assert_eq!(
        parse_permissions_command("/permissions clear"),
        Some(PermissionsCommand::ClearResolved),
    );
    assert_eq!(
        parse_permissions_command("/permissions clear all"),
        Some(PermissionsCommand::ClearAll),
    );
}

#[test]
fn parse_rejects_unknown_subcommand() {
    assert!(parse_permissions_command("/permissions nope").is_none());
    assert!(parse_permissions_command("/permissions approve").is_none());
}

#[test]
fn list_empty_store_has_friendly_message() {
    let app = AppState::new();
    let out = execute_permissions_command(
        &crate::session_shell::session_command_inputs_from_app(&app, Default::default()),
        PermissionsCommand::List,
    );
    assert!(out.text.contains("No auto-mode denials"));
    assert!(out.replay_requests.is_empty());
}

#[test]
fn list_store_shows_pending_entry() {
    let (app, id) = seed_app_with_one_denial();
    let out = execute_permissions_command(
        &crate::session_shell::session_command_inputs_from_app(&app, Default::default()),
        PermissionsCommand::List,
    );
    assert!(out.text.contains("pending"), "output: {}", out.text);
    assert!(out.text.contains(&id), "output: {}", out.text);
    assert!(out.replay_requests.is_empty());
}

#[test]
fn approve_flips_status_and_retry_requests_replay() {
    let (app, id) = seed_app_with_one_denial();
    let out = execute_permissions_command(
        &crate::session_shell::session_command_inputs_from_app(&app, Default::default()),
        PermissionsCommand::Approve(id.clone()),
    );
    assert!(out.text.contains("approved"));
    assert!(out.replay_requests.is_empty());
    let out = execute_permissions_command(
        &crate::session_shell::session_command_inputs_from_app(&app, Default::default()),
        PermissionsCommand::Retry(id.clone()),
    );
    assert!(out.text.contains("retry"));
    assert!(!out.text.contains("replay wiring pending"));
    assert_eq!(out.replay_requests.len(), 1);
    let request = &out.replay_requests[0];
    assert_eq!(request.denial_id, id);
    assert_eq!(request.tool_use_id, "t-1");
    assert_eq!(request.tool_name, "Bash");
    assert_eq!(request.tool_input, "{\"command\":\"rm\"}");
    // Now the list should show `retried` status for that id.
    let list = execute_permissions_command(
        &crate::session_shell::session_command_inputs_from_app(&app, Default::default()),
        PermissionsCommand::List,
    );
    assert!(list.text.contains("retried"), "list: {}", list.text);
}

#[test]
fn approve_installs_a_one_shot_fingerprint_exemption() {
    let (app, id) = seed_app_with_one_denial();
    let out = execute_permissions_command(
        &crate::session_shell::session_command_inputs_from_app(&app, Default::default()),
        PermissionsCommand::Approve(id),
    );
    assert!(out.text.contains("approved"));
    // The exemption is keyed on the record's exact tool name + input
    // and admits exactly one identical call.
    assert!(app
        .auto_mode_verdicts
        .take_exemption("Bash", "{\"command\":\"rm\"}"));
    assert!(!app
        .auto_mode_verdicts
        .take_exemption("Bash", "{\"command\":\"rm\"}"));
}

#[test]
fn approve_missing_id_installs_no_exemption() {
    let app = AppState::new();
    let _ = execute_permissions_command(
        &crate::session_shell::session_command_inputs_from_app(&app, Default::default()),
        PermissionsCommand::Approve("ghost".into()),
    );
    assert!(!app
        .auto_mode_verdicts
        .take_exemption("Bash", "{\"command\":\"rm\"}"));
}

#[test]
fn clear_all_empties_the_store() {
    let (app, _id) = seed_app_with_one_denial();
    let out = execute_permissions_command(
        &crate::session_shell::session_command_inputs_from_app(&app, Default::default()),
        PermissionsCommand::ClearAll,
    );
    assert!(out.text.contains("1"));
    assert!(out.replay_requests.is_empty());
    assert_eq!(app.auto_mode_denials.lock().unwrap().len(), 0);
}

#[test]
fn approve_missing_id_reports_error() {
    let app = AppState::new();
    let out = execute_permissions_command(
        &crate::session_shell::session_command_inputs_from_app(&app, Default::default()),
        PermissionsCommand::Approve("ghost".into()),
    );
    assert!(out.text.starts_with("!"));
    assert!(out.replay_requests.is_empty());
}

#[test]
fn retry_missing_id_reports_error_without_replay() {
    let app = AppState::new();
    let out = execute_permissions_command(
        &crate::session_shell::session_command_inputs_from_app(&app, Default::default()),
        PermissionsCommand::Retry("ghost".into()),
    );
    assert!(out.text.starts_with("!"));
    assert!(out.replay_requests.is_empty());
}

fn seed_app_with_one_denial() -> (AppState, String) {
    let app = AppState::new();
    let store = Arc::clone(&app.auto_mode_denials);
    let id = store.lock().unwrap().record(AutoModeDenialInput {
        tool_use_id: "t-1".into(),
        tool_name: "Bash".into(),
        tool_input: "{\"command\":\"rm\"}".into(),
        reason: "classifier blocked".into(),
        display: "Bash: rm".into(),
        timestamp_ms: 10,
        task_id: None,
        conversation_id: None,
    });
    (app, id)
}

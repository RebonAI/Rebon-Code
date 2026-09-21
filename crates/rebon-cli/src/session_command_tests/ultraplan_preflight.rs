//! Terminal-level tests for `rebon_session_runtime::ultraplan_preflight`.
//!
//! They live here rather than beside the code because each of them
//! builds an `AppState`, calls `crate::session_shell::session_command_inputs_from_app`, or
//! drives the TUI reducer — all three are the binary's, so a crate
//! that must not know what a terminal is cannot host them.

use crate::session::ultraplan_preflight::*;
use rebon_types::{CapabilityDiagnosticClass, UltraplanRunState};
use std::path::Path;

#[test]
fn run_preflight_captures_all_authorized_roots() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let shared = temp.path().join("shared");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    let mut session = crate::tui::runner::test_support::make_ultraplan_test_tui_session();
    session.cwd = workspace.to_string_lossy().to_string();
    session.startup.add_dirs = vec![shared.to_string_lossy().to_string()];
    let mut state = UltraplanRunState::new(
        "run".into(),
        session.session_id.clone(),
        "task".into(),
        None,
        1,
    );

    let capability = preflight_ultraplan_run(&session, &mut state).unwrap();

    let workspace = std::fs::canonicalize(workspace).unwrap();
    let shared = std::fs::canonicalize(shared).unwrap();
    assert_eq!(capability.allowed_roots.len(), 2);
    assert!(capability
        .allowed_roots
        .iter()
        .any(|root| Path::new(root) == workspace.as_path()));
    assert!(capability
        .allowed_roots
        .iter()
        .any(|root| Path::new(root) == shared.as_path()));
}

#[test]
fn run_preflight_rejects_required_tool_hidden_by_session_filter() {
    let temp = tempfile::tempdir().unwrap();
    let mut session = crate::tui::runner::test_support::make_ultraplan_test_tui_session();
    session.cwd = temp.path().to_string_lossy().to_string();
    session
        .engine_half
        .session_filter_handle
        .set(rebon_tool::ToolFilter::unrestricted().with_deny(["Grep"]));
    let mut state = UltraplanRunState::new(
        "run".into(),
        session.session_id.clone(),
        "task".into(),
        None,
        1,
    );

    let diagnostic = preflight_ultraplan_run(&session, &mut state).unwrap_err();

    assert_eq!(diagnostic.class, CapabilityDiagnosticClass::ToolUnavailable);
    assert_eq!(diagnostic.capability.as_deref(), Some("Grep"));
}

#[test]
fn diagnostic_is_machine_readable_and_requests_parent_fallback() {
    let state = UltraplanRunState::new("run".into(), "session".into(), "task".into(), None, 1);
    let diagnostic = diagnostic(
        &state,
        CapabilityDiagnosticClass::MissingRoot,
        "missing",
        Some(Path::new("missing")),
        Some("allowed_roots"),
        false,
    );

    let value = serde_json::to_value(&diagnostic).unwrap();
    assert_eq!(value["class"], "missing_root");
    assert_eq!(value["fallback_to_parent"], true);
}

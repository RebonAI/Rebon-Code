//! Terminal-level tests for `rebon_session_runtime::commands::doctor`.
//!
//! They live here rather than beside the code because each of them
//! builds an `AppState`, calls `crate::session_shell::session_command_inputs_from_app`, or
//! drives the TUI reducer — all three are the binary's, so a crate
//! that must not know what a terminal is cannot host them.

use crate::session::commands::doctor::*;
use crate::session::commands::EnvRestore;
use rebon_types::wall_clock_ms;
use std::path::Path;

use crate::tui::app::AppState;
use crate::tui::runner::test_support::make_test_tui_session;
use tempfile::TempDir;

fn row_with_label<'a>(rows: &'a [DoctorRow], label: &str) -> &'a DoctorRow {
    rows.iter()
        .find(|row| row.label == label)
        .unwrap_or_else(|| panic!("missing doctor row `{label}` in {rows:#?}"))
}

fn has_row_containing(rows: &[DoctorRow], label: &str, text: &str) -> bool {
    rows.iter()
        .any(|row| row.label == label && row.message.contains(text))
}

fn has_row_with_label(rows: &[DoctorRow], label: &str) -> bool {
    rows.iter().any(|row| row.label == label)
}

#[test]
fn doctor_output_is_sectioned_and_local_only() {
    let _guard = crate::test_env::lock_env();
    let _env = EnvRestore::new(&[
        "REBON_CONFIG_DIR",
        "REBON_MCP_SERVERS_JSON",
        "REBON_MCP_SERVERS",
    ]);
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let cwd = temp.path().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    write_doctor_config(
        &config_dir,
        "test-key",
        "https://example.test/v1",
        "test-model",
    );
    std::env::set_var("REBON_CONFIG_DIR", &config_dir);
    std::env::remove_var("REBON_MCP_SERVERS_JSON");
    std::env::remove_var("REBON_MCP_SERVERS");

    let app = AppState::default();
    let mut session = make_test_tui_session();
    session.cwd = cwd.display().to_string();
    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_doctor_command(&inputs, &session);

    assert!(output.starts_with("Doctor diagnostics (local-only)"));
    assert!(output.contains("Provider / network"));
    assert!(output.contains("Shell commands"));
    assert!(output.contains("Config migration"));
    assert!(output.contains("MCP servers"));
    assert!(output.contains("Sandbox"));
    assert!(output.contains("live network probe — skipped (local-only)"));
    assert!(output.contains("startup/connect probe — skipped (local-only)"));
    assert!(output.contains("pass: live network probe"));
    assert!(output.contains("Fix: No action is required."));
    assert!(!output.contains("Not implemented yet"));
}

#[test]
fn doctor_provider_reports_missing_api_key_and_invalid_base_url() {
    let _guard = crate::test_env::lock_env();
    let _env = EnvRestore::new(&["REBON_CONFIG_DIR"]);
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    write_doctor_config(&config_dir, "", "not a url", "test-model");
    std::env::set_var("REBON_CONFIG_DIR", &config_dir);

    let session = make_test_tui_session();
    let rows = provider_doctor_rows(&config_dir, &session, wall_clock_ms());

    assert_eq!(
        row_with_label(&rows, "credentials").status,
        DoctorRowStatus::Error
    );
    assert!(row_with_label(&rows, "credentials")
        .message
        .contains("apiKey is empty"));
    assert_eq!(
        row_with_label(&rows, "base URL").status,
        DoctorRowStatus::Error
    );
}

#[test]
fn doctor_provider_reports_expired_oauth_without_refresh_token() {
    let _guard = crate::test_env::lock_env();
    let _env = EnvRestore::new(&["REBON_CONFIG_DIR"]);
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    write_doctor_config(
        &config_dir,
        crate::rebon_config::OPENAI_OAUTH_TOKEN_SENTINEL,
        "https://chatgpt.com/backend-api/codex/responses",
        "gpt-5.4",
    );
    write_file(
        &config_dir.join(".credentials.json"),
        r#"{"openaiOAuth":{"accessToken":"access","expiresAt":1}}"#,
    );
    std::env::set_var("REBON_CONFIG_DIR", &config_dir);

    let session = make_test_tui_session();
    let rows = provider_doctor_rows(&config_dir, &session, 10_000_000);

    assert_eq!(
        row_with_label(&rows, "OAuth token").status,
        DoctorRowStatus::Error
    );
    assert!(row_with_label(&rows, "OAuth token")
        .message
        .contains("no refresh token"));
}

#[test]
fn config_doctor_reports_the_default_config_home_and_round_trip_schema() {
    let _guard = crate::test_env::lock_env();
    let _env = EnvRestore::new(&[
        "USERPROFILE",
        "HOME",
        "REBON_CONFIG_DIR",
        "REBON_CONFIG_HOME",
    ]);
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let config_dir = home.join(crate::rebon_config::DEFAULT_CONFIG_DIR_NAME);
    write_doctor_config(
        &config_dir,
        "test-key",
        "https://example.test/v1",
        "test-model",
    );
    std::env::remove_var("REBON_CONFIG_DIR");
    std::env::remove_var("REBON_CONFIG_HOME");
    if cfg!(windows) {
        std::env::set_var("USERPROFILE", &home);
    } else {
        std::env::set_var("HOME", &home);
    }

    let rows = config_doctor_rows(&config_dir);

    assert_eq!(
        row_with_label(&rows, "round-trip schema").status,
        DoctorRowStatus::Ok
    );
    let source = row_with_label(&rows, "config home source");
    assert_eq!(source.status, DoctorRowStatus::Ok);
    assert!(
        source.message.contains("default"),
        "with no override set the report must say so, got {:?}",
        source.message
    );
}

/// The silent failure this row exists for: a variable set in one shell and
/// not another sends the session to a different data directory.
#[test]
fn config_doctor_names_the_variable_that_moved_the_config_home() {
    let _guard = crate::test_env::lock_env();
    let _env = EnvRestore::new(&[
        "USERPROFILE",
        "HOME",
        "REBON_CONFIG_DIR",
        "REBON_CONFIG_HOME",
    ]);
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("elsewhere");
    write_doctor_config(
        &config_dir,
        "test-key",
        "https://example.test/v1",
        "test-model",
    );
    std::env::remove_var("REBON_CONFIG_HOME");
    std::env::set_var("REBON_CONFIG_DIR", &config_dir);

    let rows = config_doctor_rows(&config_dir);

    let source = row_with_label(&rows, "config home source");
    assert_eq!(source.status, DoctorRowStatus::Ok);
    assert!(
        source.message.contains("REBON_CONFIG_DIR"),
        "the report must name the variable in effect, got {:?}",
        source.message
    );
}

#[test]
fn mcp_doctor_reports_invalid_url_and_empty_command() {
    let bad_url = crate::mcp_config::CollectedMcpServerConfig {
        config: crate::mcp_config::McpServerConfig::Http(rebon_plugin_mcp::HttpServerConfig::new(
            "remote",
            "not a url",
        )),
        source: "test".into(),
    };
    let empty_command = crate::mcp_config::CollectedMcpServerConfig {
        config: crate::mcp_config::McpServerConfig::Stdio(
            rebon_plugin_mcp::StdioServerConfig::new("local", "   ", Vec::new()),
        ),
        source: "test".into(),
    };

    let rows = mcp_doctor_rows_from_configs(Ok(vec![bad_url, empty_command]), None);

    assert_eq!(
        row_with_label(&rows, "MCP `remote` http URL").status,
        DoctorRowStatus::Error
    );
    assert_eq!(
        row_with_label(&rows, "MCP `local` command").status,
        DoctorRowStatus::Error
    );
    assert!(has_row_containing(
        &rows,
        "startup/connect probe",
        "skipped (local-only)"
    ));
}

/// The variable is no longer read, so the only way a user learns their
/// servers stopped starting is this row. A blank or unset value says
/// nothing, because there is nothing to move.
#[test]
fn mcp_doctor_points_a_still_exported_retired_variable_at_its_replacement() {
    let quiet = mcp_doctor_rows_from_configs(Ok(Vec::new()), None);
    assert!(!has_row_with_label(
        &quiet,
        crate::mcp_config::RETIRED_SERVERS_ENV
    ));

    let blank = mcp_doctor_rows_from_configs(Ok(Vec::new()), Some("   "));
    assert!(!has_row_with_label(
        &blank,
        crate::mcp_config::RETIRED_SERVERS_ENV
    ));

    let set = mcp_doctor_rows_from_configs(Ok(Vec::new()), Some("fs:node server.js"));
    let row = row_with_label(&set, crate::mcp_config::RETIRED_SERVERS_ENV);
    assert_eq!(row.status, DoctorRowStatus::Warning);
    assert!(row.message.contains("no longer read"));
    assert!(row.message.contains("mcpServers"));
    assert!(row.message.contains(".mcp.json"));
}

#[test]
fn shell_and_sandbox_doctor_report_missing_commands() {
    let commands = DoctorCommandAvailability::default();

    let shell_rows = shell_command_doctor_rows(true, true, &commands);
    assert_eq!(
        row_with_label(&shell_rows, "rg").status,
        DoctorRowStatus::Error
    );
    assert_eq!(
        row_with_label(&shell_rows, "bwrap").status,
        DoctorRowStatus::Error
    );
    assert_eq!(
        row_with_label(&shell_rows, "socat").status,
        DoctorRowStatus::Error
    );
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn write_doctor_config(config_dir: &Path, api_key: &str, base_url: &str, model: &str) {
    write_file(
        &config_dir.join("config.json"),
        &format!(
            r#"{{
                    "activeCustomProvider": "local",
                    "customProviders": [{{
                        "name": "local",
                        "format": "openai",
                        "baseUrl": "{base_url}",
                        "apiKey": "{api_key}",
                        "model": "{model}"
                    }}],
                    "unknownTopLevel": {{"preserve": true}}
                }}"#
        ),
    );
}

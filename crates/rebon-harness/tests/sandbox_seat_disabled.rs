//! `plugins.sandbox.enabled = false` with `sandbox.enabled = true`, end to end.
//!
//! Its own test binary on purpose. The process kernel boots once per process
//! and reads the plugin switches while it does, so a test that wants a kernel
//! *without* the sandbox plugin cannot share one with tests that want the
//! ordinary kernel — whichever ran first would decide what the other saw.
//!
//! What this proves that the unit tests cannot: that the switch really takes
//! the seat out of a booted registry, and that `resolve_command_sandbox` then
//! reaches its fail-closed branch rather than quietly returning `None` and
//! leaving every shell command unconfined.

use rebon_tools_core::{ToolError, ToolId};

#[test]
fn a_disabled_sandbox_plugin_leaves_the_seat_empty_and_the_session_refuses() {
    let home = tempfile::tempdir().expect("temp config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"sandbox":{"enabled":true},"plugins":{"sandbox":{"enabled":false}}}"#,
    )
    .expect("write settings");
    // Before the kernel boots: `desired_from_settings` is read during boot,
    // and this process boots exactly one kernel.
    std::env::set_var("REBON_CONFIG_DIR", home.path());

    let kernel = rebon_harness::kernel_bootstrap::process_kernel();
    assert!(
        kernel
            .context()
            .get::<rebon_tool::SessionSandboxService>()
            .is_none(),
        "the switch did not take the `session-sandbox` seat out of the registry"
    );

    // And the panel goes with the seat. `/sandbox` describes a sandbox this
    // kernel is not going to build, so offering the command would put a menu
    // entry in the `/` picker whose panel is not registered to open.
    assert!(
        rebon_kernel_seats::kernel_core_commands::command_seat()
            .expect("core-commands always loads")
            .find("sandbox")
            .is_none(),
        "the switch left `/sandbox` on the command seat"
    );
    assert!(
        !kernel
            .context()
            .get::<rebon_kernel_seats::kernel_core_ui::UiSeatService>()
            .expect("core-ui provides the `ui-registry` seat")
            .has(rebon_plugin_sandbox::panel::DIALOG_ID),
        "the switch left the panel on the ui seat"
    );

    let sandbox = rebon_harness::resolve_command_sandbox(home.path())
        .expect("settings ask for a sandbox, so the session must not run unconfined");

    let error = sandbox
        .check(&ToolId::new("Bash"), "echo hi", false)
        .expect_err("every command is refused while the two switches disagree");
    match error {
        ToolError::PermissionDenied { reason, .. } => {
            assert_eq!(reason, rebon_harness::SANDBOX_PLUGIN_DISABLED);
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }

    // And `/doctor` says which of the two switches to change, rather than
    // reporting on a sandbox nothing is going to build.
    let doctor = rebon_harness::sandbox_doctor(home.path(), &[]);
    assert!(doctor.enabled_in_settings);
    assert_eq!(doctor.lines.len(), 1);
    assert_eq!(doctor.lines[0].level, rebon_tool::DoctorLevel::Warning);
    assert!(
        doctor.lines[0]
            .message
            .contains("plugins.sandbox.enabled = false"),
        "{:?}",
        doctor.lines[0]
    );
}

//! The hooks a person configured, on the session `rebon exec` and the
//! desktop app run turns through.
//!
//! Its own test binary for the reason `kernel_unregistered_provider` records:
//! `REBON_CONFIG_DIR` has to be set before the process kernel and the plane
//! singleton first initialize, and both are process-wide. Multi-threaded
//! because the session build boots the plugin plane, which cannot make
//! progress on the single-threaded runtime a bare `#[tokio::test]` gives.
//!
//! What it pins that no unit test can: that the handle
//! `build_headless_session` hands its executor is the one carrying the
//! subscribers, rather than the empty default this surface ran on from the
//! day it was written. A `PreToolUse` hook that refuses `Bash` has to refuse
//! it here exactly as it does in the TUI.

use rebon_core::hooks::{run_pre_tool_use_hooks, PreToolUseDecision};

/// A hook that refuses, written for whichever shell the test host has.
///
/// Exit code 2 is the blocking one, and its stderr is the reason the person
/// reads back. `powershell` on Windows and `bash` elsewhere, because those
/// are the two `ShellKind` interpreters guaranteed to be on the box the
/// tests run on.
fn refusing_hook() -> serde_json::Value {
    if cfg!(windows) {
        serde_json::json!({
            "type": "command",
            "shell": "powershell",
            "command": "[Console]::Error.WriteLine('refused by the configured hook'); exit 2"
        })
    } else {
        serde_json::json!({
            "type": "command",
            "shell": "bash",
            "command": "echo 'refused by the configured hook' >&2; exit 2"
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_headless_session_runs_the_pre_tool_use_hook_from_settings() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let cwd = tempfile::tempdir().expect("cwd");

    // The whole setup a person performs: a provider to talk to, and a
    // `PreToolUse` hook that refuses `Bash`. No turn is run, so the provider
    // is never actually reached — it only has to resolve.
    std::fs::write(
        config_dir.path().join("config.json"),
        serde_json::json!({
            "activeCustomProvider": "offline-provider",
            "customProviders": [{
                "name": "offline-provider",
                "format": "openai",
                "baseUrl": "http://127.0.0.1:1/v1",
                "apiKey": "not-a-real-key",
                "model": "offline-model"
            }]
        })
        .to_string(),
    )
    .expect("config written");
    std::fs::write(
        config_dir.path().join("settings.json"),
        serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [refusing_hook()]
                }]
            }
        })
        .to_string(),
    )
    .expect("settings written");
    std::env::set_var("REBON_CONFIG_DIR", config_dir.path());

    let session = rebon_harness::build_headless_session(rebon_harness::HarnessOverrides {
        cwd: Some(cwd.path().to_string_lossy().into_owned()),
        ..rebon_harness::HarnessOverrides::default()
    })
    .await
    .expect("the headless session builds against the configured provider");

    // The handle carries this session, not somebody else's: a subscriber
    // reads the cwd off the request, and the settings that answer are the
    // ones under this cwd.
    assert_eq!(session.policy.context().cwd, session.cwd);
    assert_eq!(session.policy.context().session_id, session.session_id);
    assert!(
        session
            .policy
            .subscriber_ids()
            .iter()
            .any(|id| id == rebon_core::policy_seat::SETTINGS_HOOKS_SUBSCRIBER_ID),
        "the headless session's policy handle has no settings-hooks subscriber: {:?}",
        session.policy.subscriber_ids()
    );

    let decision = run_pre_tool_use_hooks(
        &session.policy,
        "Bash",
        serde_json::json!({ "command": "rm -rf /" }),
        "tool-use-1",
    )
    .await;
    match decision {
        PreToolUseDecision::Blocked { reason } => assert!(
            reason.contains("refused by the configured hook"),
            "the refusal lost the hook's own reason: {reason}"
        ),
        other => panic!("the configured PreToolUse hook did not refuse Bash: {other:?}"),
    }

    // A tool the matcher does not name is left alone, so the wire is a hook
    // runtime and not a blanket refusal.
    assert!(
        matches!(
            run_pre_tool_use_hooks(
                &session.policy,
                "Read",
                serde_json::json!({ "file_path": "/tmp/x" }),
                "tool-use-2",
            )
            .await,
            PreToolUseDecision::Continue { .. }
        ),
        "a hook matching only `Bash` refused `Read`"
    );
}

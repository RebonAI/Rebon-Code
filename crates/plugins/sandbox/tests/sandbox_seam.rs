//! End-to-end: a sandbox policy actually reaches the spawned process.
//!
//! The unit tests in `exec` and `runtime` check
//! the decision table and the argv construction. What they cannot show
//! is that the two are wired together — that `BashTool::call` and
//! `PowerShellTool::call` consult the policy on the path that really
//! spawns something, rather than on a path that was left behind.
//!
//! These tests therefore assert on observable *behaviour of a real
//! call*: a refusal that stops the process from starting, and a
//! passthrough that still produces output. They deliberately do not
//! require a working `bwrap` / `sandbox-exec` / `sandbox-win.exe`, so they
//! run on any machine — the cases that need a real backend are the
//! backends' own unit tests.

use rebon_plugin_sandbox::runtime::{
    current_platform, has_backend, ConfinedProbe, ConfinedVerdict, SandboxMode, SandboxRuntime,
    SandboxRuntimeInit, SessionSandboxConfig, SupportReport,
};
use rebon_plugin_sandbox::view::{OverrideMode, SandboxPlatform};
use rebon_plugin_sandbox::SandboxPolicy;
use rebon_tool::{BashTool, CommandSandbox, Tool, ToolContext};
use rebon_tools_core::ToolError;
use serde_json::json;
use std::sync::Arc;

struct AlwaysConfined;
impl ConfinedProbe for AlwaysConfined {
    fn probe(&self) -> ConfinedVerdict {
        ConfinedVerdict::confined("integration fixture")
    }
}

fn runtime(session: SessionSandboxConfig) -> Arc<SandboxRuntime> {
    let platform = current_platform();
    Arc::new(SandboxRuntime::new(SandboxRuntimeInit {
        platform,
        session,
        mode: SandboxMode::Strict,
        log_tag: "seam-test".into(),
        debug_session: false,
        support: SupportReport {
            platform,
            ..Default::default()
        },
        probe: Arc::new(AlwaysConfined),
        session_resources: None,
    }))
}

fn policy(mode: OverrideMode, excluded: &[&str]) -> SandboxPolicy {
    SandboxPolicy {
        enabled: true,
        platform: current_platform(),
        override_mode: mode,
        excluded_commands: excluded.iter().map(|name| (*name).to_string()).collect(),
        runtime: Some(runtime(SessionSandboxConfig::default())),
    }
}

/// One place that turns a policy into the session's `CommandSandbox`, so
/// every test below exercises the same seam a real session goes through.
fn context_with(policy: SandboxPolicy) -> ToolContext {
    ToolContext::new().with_command_sandbox(Arc::new(policy) as Arc<dyn CommandSandbox>)
}

/// A command that prints one line on whichever shell `Bash` resolves
/// to. On a Windows box without Git Bash the tool falls back to
/// PowerShell, and `printf` is not a cmdlet.
fn echo_command() -> &'static str {
    if cfg!(windows) && std::env::var_os("REBON_GIT_BASH_PATH").is_none() {
        // Valid in both: `echo` is a shell builtin and a PowerShell
        // alias for Write-Output.
        "echo sandbox-seam-ok"
    } else {
        "printf 'sandbox-seam-ok\\n'"
    }
}

#[tokio::test]
async fn a_closed_policy_refuses_dangerously_disable_sandbox_before_spawning() {
    if !has_backend(current_platform()) {
        return;
    }
    let context = context_with(policy(OverrideMode::Closed, &[]));

    let error = BashTool::new()
        .call(
            json!({ "command": echo_command(), "dangerouslyDisableSandbox": true }),
            &context,
        )
        .await
        .expect_err("strict mode must refuse the flag");

    match error {
        ToolError::PermissionDenied { reason, .. } => {
            assert!(reason.contains("dangerouslyDisableSandbox"), "{reason}");
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
}

#[tokio::test]
async fn an_open_policy_runs_the_command_when_the_flag_is_set() {
    let context = context_with(policy(OverrideMode::Open, &[]));

    let result = BashTool::new()
        .call(
            json!({ "command": echo_command(), "dangerouslyDisableSandbox": true }),
            &context,
        )
        .await
        .expect("open mode accepts the flag");

    assert!(
        result["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("sandbox-seam-ok"),
        "{result}"
    );
}

#[tokio::test]
async fn an_unrestricted_command_still_runs_under_an_active_policy() {
    // The fast path: a session with no filesystem or network rules
    // spawns exactly what it would have spawned with the sandbox off.
    // If this regressed to "wrap everything", it would fail on any
    // machine without a backend installed — which is the point.
    let context = context_with(policy(OverrideMode::Closed, &[]));

    let result = BashTool::new()
        .call(json!({ "command": echo_command() }), &context)
        .await
        .expect("an unrestricted command needs no sandbox");

    assert!(
        result["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("sandbox-seam-ok"),
        "{result}"
    );
}

#[tokio::test]
async fn an_excluded_command_runs_even_under_a_closed_policy() {
    let executable = echo_command()
        .split_whitespace()
        .next()
        .expect("a command word");
    let context = context_with(policy(OverrideMode::Closed, &[executable]));

    let result = BashTool::new()
        .call(json!({ "command": echo_command() }), &context)
        .await
        .expect("an excluded command bypasses the sandbox");

    assert!(result["stdout"]
        .as_str()
        .unwrap_or_default()
        .contains("sandbox-seam-ok"));
}

#[tokio::test]
async fn an_active_policy_with_rules_but_no_runtime_refuses_to_run() {
    if !has_backend(current_platform()) {
        return;
    }
    // The failure this guards against is the quiet one: a policy that
    // says "confine everything" reaching a tool with nothing wired to
    // do it. Running the command anyway would succeed, look normal,
    // and be entirely unconfined.
    let mut policy = policy(OverrideMode::Closed, &[]);
    policy.runtime = None;
    let context = context_with(policy);

    let error = BashTool::new()
        .call(json!({ "command": echo_command() }), &context)
        .await
        .expect_err("a policy with no runtime must refuse");

    assert!(error.to_string().contains("never initialised"), "{error}");
}

#[tokio::test]
async fn a_filesystem_rule_makes_the_wrap_reach_the_platform_backend() {
    if !has_backend(current_platform()) {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let mut session = SessionSandboxConfig::default();
    session.filesystem.allow_write = vec![temp.path().to_path_buf()];

    let policy = SandboxPolicy {
        enabled: true,
        platform: current_platform(),
        override_mode: OverrideMode::Closed,
        excluded_commands: Vec::new(),
        runtime: Some(runtime(session)),
    };
    let context = context_with(policy);

    let outcome = BashTool::new()
        .call(json!({ "command": echo_command() }), &context)
        .await;

    // The assertion is about *which* failure, not about success: on a
    // machine with the backend installed the command runs, and on one
    // without it the error names the missing dependency. Either proves
    // the wrap was attempted; a silent success with no backend would
    // mean the rule was dropped on the floor.
    match outcome {
        Ok(result) => assert!(result["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("sandbox-seam-ok")),
        Err(error) => {
            let text = error.to_string();
            let names_backend = ["bwrap", "sandbox-win", "sandbox-exec", "bubblewrap"]
                .iter()
                .any(|needle| text.contains(needle));
            assert!(names_backend, "unexpected failure: {text}");
        }
    }
}

#[tokio::test]
async fn a_disabled_policy_leaves_the_command_untouched() {
    let context = context_with(SandboxPolicy {
        enabled: false,
        platform: SandboxPlatform::Unknown,
        override_mode: OverrideMode::Closed,
        excluded_commands: Vec::new(),
        runtime: None,
    });

    let result = BashTool::new()
        .call(
            json!({ "command": echo_command(), "dangerouslyDisableSandbox": true }),
            &context,
        )
        .await
        .expect("a disabled sandbox enforces nothing");

    assert!(result["stdout"]
        .as_str()
        .unwrap_or_default()
        .contains("sandbox-seam-ok"));
}

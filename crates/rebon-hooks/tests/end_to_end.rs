//! End-to-end wiring test for the hooks runtime.
//!
//! Exercises the whole pipeline through the public API:
//!
//! 1. A real `settings.json` written into a temp dir
//! 2. `load_all_editable` → `LoadedHooks`
//! 3. `SettingsHookProvider` wrapping those hooks
//! 4. `HookRuntime` fires a PreToolUse event for `tool_name=Bash`
//! 5. The selected hook runs through the executor
//! 6. The aggregate is projected into `HookEffect::BlockToolCall`
//!
//! This is the guard rail that settings parse through effect projection
//! stay wired up consistently. It reaches the crate only through its
//! public surface, so it also fails if an export goes missing.

use std::fs;
use std::sync::Arc;

use async_trait::async_trait;
use rebon_hooks::{
    build_hook_event_metadata, load_all_editable, CommandExecutor, DispatchExecutor,
    ExecutedHookResult, HookEffect, HookEventPayload, HookExecutionError, HookExecutor,
    HookInvocationContext, HookInvocationInput, HookRuntime, HookRuntimeContext, HookSource,
    IndividualHookConfig, MetadataInputs, SettingsHookProvider, SettingsPaths, SettingsSnapshot,
};
use serde_json::json;

fn unique_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("{prefix}-"))
        .tempdir()
        .unwrap()
}

/// Synthetic executor that returns a pre-scripted JSON blob. Used so
/// the E2E test does not depend on a `bash` or `node` binary being
/// present on CI.
struct ScriptedExecutor {
    /// JSON string that every call returns as stdout.
    json: String,
}

#[async_trait]
impl HookExecutor for ScriptedExecutor {
    async fn execute(
        &self,
        hook: &IndividualHookConfig,
        _input: &HookInvocationInput,
        _ctx: &HookRuntimeContext,
    ) -> Result<ExecutedHookResult, HookExecutionError> {
        let parsed = rebon_hooks::output_protocol::parse_hook_output(&self.json);
        Ok(ExecutedHookResult {
            json: parsed.json,
            plain_text: parsed.plain_text,
            validation_error: parsed.validation_error,
            exit_code: 0,
            stderr: String::new(),
            command_label: hook_config_label(hook),
        })
    }
}

fn hook_config_label(hook: &IndividualHookConfig) -> String {
    rebon_hooks::display_text(&hook.config).to_string()
}

#[tokio::test]
async fn settings_file_to_block_tool_call_effect() {
    let dir = unique_dir("rebon-hooks-e2e-block");
    let home = dir.path().join("home");
    let proj = dir.path().join("proj");
    fs::create_dir_all(home.join(".rebon")).unwrap();
    fs::create_dir_all(proj.join(".rebon")).unwrap();

    // Settings file with a PreToolUse hook that matches Bash only.
    fs::write(
        home.join(".rebon").join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        { "type": "command", "command": "lint.sh" }
                    ]
                }]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    // Load → build provider
    let paths = SettingsPaths::resolve(&home, &proj);
    let loaded = load_all_editable(&paths).unwrap();
    assert_eq!(loaded.hooks.len(), 1);
    assert_eq!(loaded.hooks[0].source, HookSource::UserSettings);
    assert_eq!(loaded.hooks[0].matcher.as_deref(), Some("Bash"));
    assert!(loaded.warnings.is_empty());

    let provider = Arc::new(SettingsHookProvider::new(SettingsSnapshot {
        settings_hooks: loaded.hooks,
        ..SettingsSnapshot::default()
    }));
    let metadata = Arc::new(build_hook_event_metadata(&MetadataInputs {
        tool_names: vec!["Bash".into(), "Read".into()],
        ..MetadataInputs::default()
    }));

    // Dispatch executor with a synthetic command backend that returns
    // a `decision: block` JSON blob — exercises the JSON path through
    // the runtime without needing bash on PATH.
    let executor = Arc::new(
        DispatchExecutor::new().with_command(Box::new(ScriptedExecutor {
            json: r#"{"decision":"block","reason":"rebon-hooks denies this call"}"#.into(),
        })),
    );

    let runtime = HookRuntime::new(provider, metadata, executor);

    let input = HookInvocationInput::new(
        HookInvocationContext {
            cwd: proj.to_string_lossy().to_string(),
            transcript_path: String::new(),
            session_id: "e2e-session".into(),
            ..Default::default()
        },
        HookEventPayload::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "rm -rf /"}),
            tool_use_id: "e2e-tool".into(),
        },
    );

    let output = runtime.run_event(&input).await;

    assert_eq!(output.selected, 1, "expected exactly one matching hook");
    assert!(
        output.execution_errors.is_empty(),
        "{:?}",
        output.execution_errors
    );
    assert!(
        output.validation_errors.is_empty(),
        "{:?}",
        output.validation_errors
    );
    assert_eq!(output.per_hook.len(), 1);

    let has_block = output.effects.iter().any(|effect| {
        matches!(
            effect,
            HookEffect::BlockToolCall { reason, .. } if reason.contains("denies")
        )
    });
    assert!(
        has_block,
        "expected BlockToolCall effect, got {:?}",
        output.effects
    );
}

#[tokio::test]
async fn matcher_filters_out_non_matching_tool() {
    let dir = unique_dir("rebon-hooks-e2e-nomatch");
    let home = dir.path().join("home");
    let proj = dir.path().join("proj");
    fs::create_dir_all(home.join(".rebon")).unwrap();
    fs::create_dir_all(proj.join(".rebon")).unwrap();

    fs::write(
        home.join(".rebon").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"x"}]}]}}"#,
    )
    .unwrap();

    let paths = SettingsPaths::resolve(&home, &proj);
    let loaded = load_all_editable(&paths).unwrap();
    let provider = Arc::new(SettingsHookProvider::new(SettingsSnapshot {
        settings_hooks: loaded.hooks,
        ..SettingsSnapshot::default()
    }));
    let metadata = Arc::new(build_hook_event_metadata(&MetadataInputs {
        tool_names: vec!["Bash".into(), "Read".into()],
        ..MetadataInputs::default()
    }));
    let executor = Arc::new(
        DispatchExecutor::new().with_command(Box::new(ScriptedExecutor { json: "".into() })),
    );

    let runtime = HookRuntime::new(provider, metadata, executor);

    let input = HookInvocationInput::new(
        HookInvocationContext {
            cwd: proj.to_string_lossy().to_string(),
            transcript_path: String::new(),
            session_id: "e2e".into(),
            ..Default::default()
        },
        HookEventPayload::PreToolUse {
            tool_name: "Read".into(), // hook matches Bash, not Read
            tool_input: json!({"file_path": "/etc/hosts"}),
            tool_use_id: "x".into(),
        },
    );

    let output = runtime.run_event(&input).await;
    assert_eq!(output.selected, 0);
    assert!(output.per_hook.is_empty());
    assert!(output.effects.is_empty());
}

#[tokio::test]
async fn user_project_local_layering_loads_all_sources() {
    let dir = unique_dir("rebon-hooks-e2e-layering");
    let home = dir.path().join("home");
    let proj = dir.path().join("proj");
    fs::create_dir_all(home.join(".rebon")).unwrap();
    fs::create_dir_all(proj.join(".rebon")).unwrap();

    // Each layer contributes one distinct hook.
    fs::write(
        home.join(".rebon").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"u"}]}]}}"#,
    )
    .unwrap();
    fs::write(
        proj.join(".rebon").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"p"}]}]}}"#,
    )
    .unwrap();
    fs::write(
        proj.join(".rebon").join("settings.local.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"l"}]}]}}"#,
    )
    .unwrap();

    let paths = SettingsPaths::resolve(&home, &proj);
    let loaded = load_all_editable(&paths).unwrap();
    assert_eq!(loaded.hooks.len(), 3);

    // Exactly one hook from each source.
    let sources: std::collections::HashSet<HookSource> =
        loaded.hooks.iter().map(|h| h.source).collect();
    assert!(sources.contains(&HookSource::UserSettings));
    assert!(sources.contains(&HookSource::ProjectSettings));
    assert!(sources.contains(&HookSource::LocalSettings));
}

/// Optional system-integration test: spawn a real `bash` subprocess
/// via `CommandExecutor`. Skipped on hosts without bash so CI stays
/// green on Windows runners without WSL.
#[tokio::test]
async fn command_executor_runs_real_bash_hook_against_settings_file() {
    if !std::process::Command::new("bash")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("bash not present; skipping real-subprocess E2E");
        return;
    }

    let dir = unique_dir("rebon-hooks-e2e-bash");
    let home = dir.path().join("home");
    let proj = dir.path().join("proj");
    fs::create_dir_all(home.join(".rebon")).unwrap();
    fs::create_dir_all(proj.join(".rebon")).unwrap();

    fs::write(
        home.join(".rebon").join("settings.json"),
        serde_json::to_string(&json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": r#"printf '{"decision":"block","reason":"policy: no Bash"}'"#,
                        "shell": "bash",
                        "timeout": 10
                    }]
                }]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let paths = SettingsPaths::resolve(&home, &proj);
    let loaded = load_all_editable(&paths).unwrap();
    let provider = Arc::new(SettingsHookProvider::new(SettingsSnapshot {
        settings_hooks: loaded.hooks,
        ..SettingsSnapshot::default()
    }));
    let metadata = Arc::new(build_hook_event_metadata(&MetadataInputs {
        tool_names: vec!["Bash".into()],
        ..MetadataInputs::default()
    }));
    let executor = Arc::new(DispatchExecutor::new().with_command(Box::new(CommandExecutor::new())));
    let runtime = HookRuntime::new(provider, metadata, executor);

    let input = HookInvocationInput::new(
        HookInvocationContext {
            cwd: proj.to_string_lossy().to_string(),
            transcript_path: String::new(),
            session_id: "bash-e2e".into(),
            ..Default::default()
        },
        HookEventPayload::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "ls"}),
            tool_use_id: "tid".into(),
        },
    );

    let output = runtime.run_event(&input).await;
    assert!(
        output.execution_errors.is_empty(),
        "{:?}",
        output.execution_errors
    );
    let has_block = output
        .effects
        .iter()
        .any(|e| matches!(e, HookEffect::BlockToolCall { .. }));
    assert!(has_block, "expected block effect, got {:?}", output.effects);
}

/// Optional: run the exact same flow through a Node hook. This pins
/// the Node-execution path that was the explicit user goal.
#[tokio::test]
async fn command_executor_runs_real_node_javascript_hook() {
    if !std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("node not present; skipping real-node E2E");
        return;
    }

    let dir = unique_dir("rebon-hooks-e2e-node");
    let home = dir.path().join("home");
    let proj = dir.path().join("proj");
    fs::create_dir_all(home.join(".rebon")).unwrap();
    fs::create_dir_all(proj.join(".rebon")).unwrap();

    // Inline script that writes a block decision to stdout. Demonstrates
    // the full shell=node pipeline: settings parse → runtime selection
    // → node subprocess → JSON parse → effect projection.
    let js = r#"process.stdout.write(JSON.stringify({decision:'block',reason:'JS hook says no'}))"#;
    let settings = json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": js,
                    "shell": "node",
                    "timeout": 15
                }]
            }]
        }
    });
    fs::write(
        home.join(".rebon").join("settings.json"),
        serde_json::to_string(&settings).unwrap(),
    )
    .unwrap();

    let paths = SettingsPaths::resolve(&home, &proj);
    let loaded = load_all_editable(&paths).unwrap();
    let provider = Arc::new(SettingsHookProvider::new(SettingsSnapshot {
        settings_hooks: loaded.hooks,
        ..SettingsSnapshot::default()
    }));
    let metadata = Arc::new(build_hook_event_metadata(&MetadataInputs {
        tool_names: vec!["Bash".into()],
        ..MetadataInputs::default()
    }));
    let executor = Arc::new(DispatchExecutor::new().with_command(Box::new(CommandExecutor::new())));
    let runtime = HookRuntime::new(provider, metadata, executor);

    let input = HookInvocationInput::new(
        HookInvocationContext {
            cwd: proj.to_string_lossy().to_string(),
            transcript_path: String::new(),
            session_id: "node-e2e".into(),
            ..Default::default()
        },
        HookEventPayload::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "ls"}),
            tool_use_id: "tid".into(),
        },
    );

    let output = runtime.run_event(&input).await;
    assert!(
        output.execution_errors.is_empty(),
        "{:?}",
        output.execution_errors
    );
    let has_block = output.effects.iter().any(|e| match e {
        HookEffect::BlockToolCall { reason, .. } => reason.contains("JS hook"),
        _ => false,
    });
    assert!(
        has_block,
        "expected JS-generated block effect, got {:?}",
        output.effects
    );
}

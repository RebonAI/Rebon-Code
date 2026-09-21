//! Subprocess-backed executor for `HookCommand::Command`.
//!
//! ## Shells
//!
//! * [`ShellKind::Bash`] → `bash -c <command>`
//! * [`ShellKind::Powershell`] → `powershell -NoProfile -Command <command>`
//! * [`ShellKind::Node`] → `node -e <command>`
//!
//! Every shell gets the serialized [`HookInvocationInput`] written to
//! stdin so the hook can parse it with `JSON.parse(readFileSync(0))`
//! (Node) / `cat` + `jq` (bash) / `$input | ConvertFrom-Json`
//! (PowerShell).
//!
//! ## Timeout
//!
//! `timeout_override` wins over the hook's own `timeout` field. When
//! both are unset we apply [`DEFAULT_HOOK_TIMEOUT`]. When the
//! deadline fires, the child process is killed and the executor
//! returns [`HookExecutionError::Timeout`].
//!
//! ## Why this crate owns it
//!
//! This is the only executor that spawns a subprocess. It lives here so
//! that every caller shares one implementation of the parse/timeout
//! semantics instead of re-deriving them.

use std::io;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::executor::{ExecutedHookResult, HookExecutionError, HookExecutor, HookRuntimeContext};
use crate::hook_command::{display_text, HookCommand, ShellKind, DEFAULT_HOOK_SHELL};
use crate::individual_hook::IndividualHookConfig;
use crate::invocation::HookInvocationInput;
use crate::output_protocol::parse_hook_output;

/// Default timeout applied when neither `hook.timeout` nor
/// `ctx.timeout_override` is set. The fallback is 60 seconds.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(60);

/// Subprocess-backed executor. Holds no mutable state; cheap to
/// clone (actually it's zero-sized).
#[derive(Debug, Clone, Default)]
pub struct CommandExecutor;

impl CommandExecutor {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl HookExecutor for CommandExecutor {
    async fn execute(
        &self,
        hook: &IndividualHookConfig,
        input: &HookInvocationInput,
        ctx: &HookRuntimeContext,
    ) -> Result<ExecutedHookResult, HookExecutionError> {
        let cmd = match &hook.config {
            HookCommand::Command(c) => c,
            other => {
                return Err(HookExecutionError::UnsupportedType(
                    other.type_str().to_string(),
                ))
            }
        };

        let shell = cmd.shell.unwrap_or(DEFAULT_HOOK_SHELL);
        let mut child = build_command(shell, &cmd.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(&input.cwd)
            .spawn()
            .map_err(|e| map_spawn_error(e, shell))?;

        // Pipe invocation JSON into stdin.
        if let Some(stdin) = child.stdin.as_mut() {
            let body = serde_json::to_vec(input)
                .map_err(|e| HookExecutionError::Transport(format!("serialize input: {e}")))?;
            if let Err(e) = stdin.write_all(&body).await {
                return Err(HookExecutionError::Transport(format!("stdin write: {e}")));
            }
        }
        drop(child.stdin.take());

        let deadline = resolve_timeout(ctx, cmd.timeout);
        let output = match timeout(deadline, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(HookExecutionError::Transport(format!("wait: {e}"))),
            Err(_) => return Err(HookExecutionError::Timeout(deadline)),
        };

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let parsed = parse_hook_output(&stdout);
        let exit_code = output.status.code().unwrap_or(-1);

        Ok(ExecutedHookResult {
            json: parsed.json,
            plain_text: parsed.plain_text,
            validation_error: parsed.validation_error,
            exit_code,
            stderr,
            command_label: display_text(&hook.config).to_string(),
        })
    }
}

fn build_command(shell: ShellKind, command: &str) -> Command {
    let mut cmd = Command::new(shell.interpreter());
    match shell {
        ShellKind::Bash => {
            cmd.arg("-c").arg(command);
        }
        ShellKind::Powershell => {
            cmd.arg("-NoProfile").arg("-Command").arg(command);
        }
        ShellKind::Node => {
            cmd.arg("-e").arg(command);
        }
    }
    // `CREATE_NO_WINDOW` — prevents a visible console on Windows when the
    // host is a GUI process.
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    cmd
}

fn map_spawn_error(err: io::Error, shell: ShellKind) -> HookExecutionError {
    HookExecutionError::Transport(format!("spawn `{}`: {}", shell.interpreter(), err))
}

fn resolve_timeout(ctx: &HookRuntimeContext, hook_timeout: Option<u64>) -> Duration {
    ctx.timeout_override
        .or_else(|| hook_timeout.map(Duration::from_secs))
        .unwrap_or(DEFAULT_HOOK_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookEvent;
    use crate::hook_command::{BashCommandHook, HookCommand};
    use crate::hook_source::HookSource;
    use crate::invocation::{HookEventPayload, HookInvocationContext};
    use serde_json::json;

    fn hook(
        command: &str,
        shell: Option<ShellKind>,
        timeout_s: Option<u64>,
    ) -> IndividualHookConfig {
        IndividualHookConfig {
            event: HookEvent::PreToolUse,
            config: HookCommand::Command(BashCommandHook {
                command: command.into(),
                r#if: None,
                shell,
                timeout: timeout_s,
                status_message: None,
                once: None,
                r#async: None,
                async_rewake: None,
            }),
            matcher: None,
            source: HookSource::UserSettings,
            plugin_name: None,
        }
    }

    fn invocation() -> HookInvocationInput {
        HookInvocationInput::new(
            HookInvocationContext {
                cwd: std::env::temp_dir().to_string_lossy().to_string(),
                transcript_path: String::new(),
                session_id: "test".into(),
                ..Default::default()
            },
            HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: json!({"command": "ls"}),
                tool_use_id: "x".into(),
            },
        )
    }

    fn which(bin: &str) -> bool {
        std::process::Command::new(bin)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn interpreter_maps_to_os_binary() {
        assert_eq!(ShellKind::Bash.interpreter(), "bash");
        assert_eq!(ShellKind::Powershell.interpreter(), "powershell");
        assert_eq!(ShellKind::Node.interpreter(), "node");
    }

    #[test]
    fn resolve_timeout_override_wins() {
        let ctx = HookRuntimeContext {
            matcher: None,
            timeout_override: Some(Duration::from_secs(5)),
        };
        assert_eq!(resolve_timeout(&ctx, Some(30)), Duration::from_secs(5));
    }

    #[test]
    fn resolve_timeout_falls_back_to_default() {
        let ctx = HookRuntimeContext::default();
        assert_eq!(resolve_timeout(&ctx, None), DEFAULT_HOOK_TIMEOUT);
    }

    #[tokio::test]
    async fn bash_hook_echoes_plain_text() {
        if !which("bash") {
            eprintln!("bash not present; skipping");
            return;
        }
        let exec = CommandExecutor::new();
        let h = hook("printf 'hello'", Some(ShellKind::Bash), Some(10));
        let result = exec
            .execute(&h, &invocation(), &HookRuntimeContext::default())
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.plain_text.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn bash_hook_emitting_json_decision_parses() {
        if !which("bash") {
            return;
        }
        let exec = CommandExecutor::new();
        let h = hook(
            r#"printf '{"decision":"block","reason":"no"}'"#,
            Some(ShellKind::Bash),
            Some(10),
        );
        let result = exec
            .execute(&h, &invocation(), &HookRuntimeContext::default())
            .await
            .unwrap();
        assert!(result.json.is_some());
    }

    #[tokio::test]
    async fn node_hook_runs_inline_javascript() {
        if !which("node") {
            eprintln!("node not present; skipping");
            return;
        }
        let exec = CommandExecutor::new();
        let h = hook(
            r#"process.stdout.write(JSON.stringify({continue:false,stopReason:"js"}))"#,
            Some(ShellKind::Node),
            Some(15),
        );
        let result = exec
            .execute(&h, &invocation(), &HookRuntimeContext::default())
            .await
            .unwrap();
        assert!(result.json.is_some());
    }

    #[tokio::test]
    async fn timeout_triggers_when_child_hangs() {
        if !which("bash") {
            return;
        }
        let exec = CommandExecutor::new();
        let h = hook("sleep 5", Some(ShellKind::Bash), None);
        let ctx = HookRuntimeContext {
            matcher: None,
            timeout_override: Some(Duration::from_millis(200)),
        };
        let err = exec.execute(&h, &invocation(), &ctx).await.unwrap_err();
        assert!(matches!(err, HookExecutionError::Timeout(_)));
    }

    #[tokio::test]
    async fn unsupported_variant_returns_error() {
        use crate::hook_command::{HttpHook, PromptHook};
        let exec = CommandExecutor::new();
        let h = IndividualHookConfig {
            event: HookEvent::PreToolUse,
            config: HookCommand::Prompt(PromptHook {
                prompt: "hi".into(),
                r#if: None,
                timeout: None,
                model: None,
                status_message: None,
                once: None,
            }),
            matcher: None,
            source: HookSource::UserSettings,
            plugin_name: None,
        };
        let err = exec
            .execute(&h, &invocation(), &HookRuntimeContext::default())
            .await
            .unwrap_err();
        assert!(matches!(err, HookExecutionError::UnsupportedType(ref t) if t == "prompt"));
        // silence unused import warning
        let _ = HttpHook {
            url: String::new(),
            r#if: None,
            timeout: None,
            headers: None,
            allowed_env_vars: None,
            status_message: None,
            once: None,
        };
    }
}

//! The `PowerShell` tool — a peer of `Bash`, not a variant of it.
//!
//! Its own runtime detection ([`detect`]), its own edition-aware model
//! description ([`prompt`]), its own command construction and spawn arguments
//! ([`command`]), and its own pre-execution guards ([`guard`]). Execution,
//! streaming, backgrounding, and the sandbox policy are shared with `Bash` —
//! those are properties of running a process, not of the language it speaks.
//!
//! Why the split exists at all: PowerShell's syntax shares nothing with POSIX
//! shell, so a single tool would have to give the model one description that
//! is wrong half the time and match permission rules against a grammar the
//! command was not written in.

pub mod command;
pub mod detect;
pub mod guard;
pub mod prompt;

use crate::command_sandbox::{self, BinShell, CommandSandbox, PreparedCommand};
use crate::edit::optional_bool;
use crate::output_truncation::{finish_shell_streams, tool_output_max_bytes};
use crate::shell_process::{ShellLineReader, ShellOutputEncoding};
use crate::{Tool, ToolContext};
use async_trait::async_trait;
use rebon_tools_core::{
    require_valid_input, validation_outcome_from, PermissionDecision, ToolError, ToolId,
    ToolInputSchema, ToolProgressUpdate, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};
use std::sync::OnceLock;
use tokio::process::Command;
use tokio::time::{sleep, Duration};

pub use detect::{is_available, PowerShellEdition, PowerShellRuntime};

pub const POWERSHELL_TOOL_NAME: &str = "PowerShell";
const INVALID_INPUT_CODE: i64 = 400;
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

/// The PowerShell tool.
///
/// Zero-sized: the description depends only on the detected edition, which is
/// itself a process-lifetime constant, so it is built once in a `OnceLock`
/// rather than per instance. That keeps `PowerShellTool` cheap to construct
/// in the dozens of places that register builtin tools.
#[derive(Debug, Clone, Default)]
pub struct PowerShellTool;

/// The edition the descriptions are written for.
///
/// Falls back to the platform's likely edition when no runtime is installed,
/// so `description()` — which the tool-search index calls whether or not the
/// tool is enabled — never has to return an empty string.
fn described_edition() -> PowerShellEdition {
    detect::runtime().map(|runtime| runtime.edition).unwrap_or({
        if cfg!(windows) {
            PowerShellEdition::Desktop
        } else {
            PowerShellEdition::Core
        }
    })
}

fn tool_description() -> &'static str {
    static DESCRIPTION: OnceLock<String> = OnceLock::new();
    DESCRIPTION.get_or_init(|| prompt::build_tool_prompt(described_edition()))
}

fn tool_model_description() -> &'static str {
    static MODEL_DESCRIPTION: OnceLock<String> = OnceLock::new();
    MODEL_DESCRIPTION.get_or_init(|| prompt::build_model_description(described_edition()))
}

#[derive(Debug, Clone)]
struct PowerShellInput {
    command: String,
    timeout_ms: Option<u64>,
    run_in_background: bool,
    dangerously_disable_sandbox: bool,
    description: Option<String>,
}

#[async_trait]
impl Tool for PowerShellTool {
    fn id(&self) -> ToolId {
        ToolId::new(POWERSHELL_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["PowerShellTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::Shell
    }

    fn description(&self) -> &str {
        tool_description()
    }

    fn model_description(&self) -> &str {
        tool_model_description()
    }

    /// The tool is advertised only when the user asked for it (or the
    /// platform resolves to it) *and* a runtime actually exists.
    fn is_enabled(&self) -> bool {
        crate::shell_preference::powershell_tool_enabled()
    }

    fn search_hint(&self) -> Option<&str> {
        Some("execute Windows PowerShell pwsh cmdlet shell commands")
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The PowerShell command to execute."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TIMEOUT_MS,
                    "description": "Foreground defaults to 60000ms; background has no deadline when omitted."
                },
                "description": {
                    "type": "string",
                    "description": "Clear, concise description of what this command does in active voice, 5-10 words."
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Return a shellId immediately and manage it with ShellOutput/ShellStop."
                },
                "dangerouslyDisableSandbox": { "type": "boolean" }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_destructive(&self, _input: &Value) -> bool {
        true
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        validation_outcome_from(prepare_input(self.id(), input))
    }

    async fn check_permissions(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        let command = input
            .get("command")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim();
        let disables_sandbox = input
            .get("dangerouslyDisableSandbox")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(crate::bash::shell_permission_decision(
            input,
            command,
            context,
            disables_sandbox,
            POWERSHELL_TOOL_NAME,
        ))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = prepare_input(self.id(), &input)?;

        // --- Sandbox ---
        // The policy question is answered inside `prepare`, together
        // with building the argv, so the two cannot disagree.
        // `CommandSandbox::check` stays for `check_permissions`, which
        // has to answer before there is a command to build.
        let sandbox = context.command_sandbox();

        // Startup check: an install-me message, not "not available".
        let runtime = detect::runtime().ok_or_else(|| ToolError::Execution {
            tool: self.id(),
            source: anyhow::anyhow!(detect::unavailable_message()),
        })?;

        let output_encoding = ShellOutputEncoding::powershell();
        let prepared = prepare_powershell_command(
            sandbox.map(|sandbox| sandbox.as_ref()),
            &self.id(),
            runtime,
            &parsed.command,
            context.cwd(),
            parsed.dangerously_disable_sandbox,
        )?;
        let mut cmd = configured_powershell_command(&prepared);

        if parsed.run_in_background {
            let registry =
                context
                    .shell_process_registry()
                    .ok_or_else(|| ToolError::Execution {
                        tool: self.id(),
                        source: anyhow::anyhow!("background shell registry is unavailable"),
                    })?;
            let mut result = registry
                .spawn_with_output_encoding(
                    context,
                    cmd,
                    POWERSHELL_TOOL_NAME,
                    parsed.command,
                    parsed.timeout_ms,
                    output_encoding,
                )
                .await?;
            // Same reason as the foreground path, more so: nobody is
            // watching a backgrounded command's output live, so the handover
            // result is the only place the note can land.
            command_sandbox::attach_notices(&mut result, &prepared);
            return Ok(result);
        }

        let mut child = cmd.spawn().map_err(|err| ToolError::Execution {
            tool: self.id(),
            source: anyhow::Error::from(err)
                .context(format!("failed to start {}", runtime.path.display())),
        })?;

        let stdout = child.stdout.take().ok_or_else(|| ToolError::Execution {
            tool: self.id(),
            source: anyhow::anyhow!("failed to capture child stdout"),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| ToolError::Execution {
            tool: self.id(),
            source: anyhow::anyhow!("failed to capture child stderr"),
        })?;

        // Cancel-safe readers: the `select!` below drops whichever read loses
        // the race, and PowerShell flushes a line's text and its newline
        // separately — so a line half-read at that moment is the common case,
        // not the rare one, and it has to survive into the next poll.
        let mut stdout_reader = ShellLineReader::new(stdout, output_encoding);
        let mut stderr_reader = ShellLineReader::new(stderr, output_encoding);
        let timeout = sleep(Duration::from_millis(
            parsed.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
        ));
        tokio::pin!(timeout);

        // Mirror the Bash tool: one arrival-ordered log rather than two
        // per-stream buckets, so the interleaving survives into the result
        // for renderers to replay.
        let mut output_lines: Vec<(bool, String)> = Vec::new();
        let mut stdout_open = true;
        let mut stderr_open = true;
        let mut interrupted = false;

        while stdout_open || stderr_open {
            tokio::select! {
                _ = &mut timeout => {
                    interrupted = true;
                    let _ = child.kill().await;
                    break;
                }
                line = stdout_reader.next_line(), if stdout_open => {
                    match line {
                        Ok(Some(line)) => {
                            context.emit_progress(ToolProgressUpdate::new("stdout").with_message(line.clone()));
                            output_lines.push((false, line));
                        }
                        Ok(None) => stdout_open = false,
                        Err(err) => {
                            return Err(ToolError::Execution {
                                tool: self.id(),
                                source: err.into(),
                            });
                        }
                    }
                }
                line = stderr_reader.next_line(), if stderr_open => {
                    match line {
                        Ok(Some(line)) => {
                            context.emit_progress(ToolProgressUpdate::new("stderr").with_message(line.clone()));
                            output_lines.push((true, line));
                        }
                        Ok(None) => stderr_open = false,
                        Err(err) => {
                            return Err(ToolError::Execution {
                                tool: self.id(),
                                source: err.into(),
                            });
                        }
                    }
                }
            }
        }

        let status = child.wait().await.map_err(|err| ToolError::Execution {
            tool: self.id(),
            source: err.into(),
        })?;

        // Mirror the Bash tool: cap each stream so a single large dump
        // doesn't flood the transcript. Live progress already streamed
        // the untruncated output to the user.
        let max_bytes = tool_output_max_bytes();
        let (stdout, stderr, stream_order) = finish_shell_streams(&output_lines, max_bytes);
        let mut result = json!({
            "stdout": stdout,
            "stderr": stderr,
            "interrupted": interrupted,
            "exitCode": status.code(),
            "timedOut": interrupted,
            "command": parsed.command,
            "dangerouslyDisableSandbox": parsed.dangerously_disable_sandbox,
            "shellEdition": runtime.edition.as_str(),
        });
        if let Some(description) = parsed.description {
            result["description"] = Value::String(description);
        }
        if let Some(sketch) = stream_order {
            result[rebon_tools_core::shell_stream_order::STREAM_ORDER_KEY] = Value::String(sketch);
        }
        command_sandbox::attach_notices(&mut result, &prepared);
        Ok(result)
    }
}

/// The console code page the child's output will be decoded with, handed to
/// the prologue so PowerShell writes in that same page. `None` off Windows,
/// where the whole stack is UTF-8 already.
#[cfg(windows)]
fn console_code_page() -> Option<u32> {
    Some(crate::shell_process::windows_console_code_page())
}

#[cfg(not(windows))]
fn console_code_page() -> Option<u32> {
    None
}

/// Decide how this command reaches pwsh, and wrap it.
///
/// Two of the three passing modes are chosen here, and the
/// choice is made *before* the sandbox is asked to wrap anything,
/// because the sandbox needs the finished shell prefix as its argv
/// start:
///
/// * **not sandboxed** → `-Command <script>`. Rebon spawns pwsh
///   directly, the script is one argv element, and nothing between
///   here and PowerShell's parser can touch it.
/// * **sandboxed** → `-EncodedCommand <base64>`. The payload now
///   crosses at least one more argv or command-line layer, and base64
///   of UTF-16LE is the only form that survives all of them
///   unchanged.
///
/// The mode is decided from the policy rather than from the wrapped
/// result, because the result depends on it: asking the runtime to
/// wrap a `-Command` argv and then discovering it needed encoding
/// would mean re-wrapping.
fn prepare_powershell_command(
    sandbox: Option<&dyn CommandSandbox>,
    tool: &rebon_tools_core::ToolId,
    runtime: &PowerShellRuntime,
    command: &str,
    cwd: Option<&str>,
    dangerously_disable_sandbox: bool,
) -> ToolResult<PreparedCommand> {
    let page = console_code_page();
    // An accepted `dangerouslyDisableSandbox` runs unwrapped, so it keeps the
    // cheaper `-Command` form. A *refused* one is an error either way and the
    // mode never matters.
    let will_wrap =
        sandbox.is_some_and(|sandbox| sandbox.will_wrap(command, dangerously_disable_sandbox));

    let (bin_shell, payload) = if will_wrap {
        (
            BinShell::new(
                runtime.path.to_string_lossy().into_owned(),
                command::sandbox_spawn_prefix(),
            ),
            command::build_encoded_exec_command(command, page),
        )
    } else {
        let mut args = command::spawn_args(command, page);
        // `spawn_args` ends with the script; a `BinShell` is the
        // prefix in front of it.
        let script = args.pop().unwrap_or_default();
        (
            BinShell::new(runtime.path.to_string_lossy().into_owned(), args),
            script,
        )
    };

    match sandbox {
        Some(sandbox) => sandbox.prepare(
            tool,
            command,
            &payload,
            bin_shell,
            cwd,
            dangerously_disable_sandbox,
        ),
        None => Ok(PreparedCommand::passthrough(&bin_shell, &payload, cwd)),
    }
}

/// Build the child process.
///
/// The stdio shape and the environment overrides apply to both the
/// wrapped and unwrapped forms — a sandboxed command still has to
/// come back as UTF-8 without ANSI, and still must have no stdin.
fn configured_powershell_command(prepared: &PreparedCommand) -> Command {
    let mut process = command_sandbox::to_process(prepared);
    for (key, value) in command::environment_overrides() {
        // The sandbox's own variables win: a credential mask or a
        // proxy setting is a policy decision, and these two are
        // formatting preferences.
        if prepared.env_set.iter().all(|(existing, _)| existing != key) {
            process.env(key, value);
        }
    }
    process
}

/// Everything that can refuse a syntactically valid input.
fn validate_parsed_input(parsed: &PowerShellInput) -> ValidationOutcome {
    if parsed
        .timeout_ms
        .is_some_and(|timeout| timeout > MAX_TIMEOUT_MS)
    {
        return ValidationOutcome::invalid(
            format!("`timeout` exceeds max supported timeout of {MAX_TIMEOUT_MS}ms"),
            INVALID_INPUT_CODE,
        );
    }
    if let Some(reason) = guard::hidden_character_refusal(&parsed.command) {
        return ValidationOutcome::invalid(reason, INVALID_INPUT_CODE);
    }
    // A background command is allowed to wait — that is what background is
    // for. The refusal targets a foreground call that would spend its whole
    // timeout window asleep.
    if !parsed.run_in_background {
        if let Some(reason) = guard::blocked_sleep_refusal(&parsed.command) {
            return ValidationOutcome::invalid(reason, INVALID_INPUT_CODE);
        }
    }
    ValidationOutcome::valid()
}

/// Parse and vet one `PowerShell` request, once.
///
/// The credential refusal comes **before** the parse, and stays there: a
/// command that names a live session's token is refused for naming it,
/// whether or not the rest of the input is well-formed.
fn prepare_input(tool: ToolId, input: &Value) -> ToolResult<PowerShellInput> {
    crate::path_scope::refuse_session_credential_command(tool.clone(), tool.as_str(), input)?;
    let parsed = parse_input(input)?;
    require_valid_input(
        tool,
        validate_parsed_input(&parsed),
        "PowerShell input is invalid",
    )?;
    Ok(parsed)
}

fn parse_input(input: &Value) -> ToolResult<PowerShellInput> {
    let tool = ToolId::new(POWERSHELL_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "PowerShell input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let command = object
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "PowerShell input requires a non-empty `command` string".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?
        .to_owned();

    let timeout_ms = match object.get("timeout") {
        Some(Value::Number(raw)) => {
            Some(raw.as_u64().filter(|value| *value >= 1).ok_or_else(|| {
                ToolError::InvalidInput {
                    tool: tool.clone(),
                    reason: "`timeout` must be an integer >= 1".into(),
                    error_code: Some(INVALID_INPUT_CODE),
                }
            })?)
        }
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: "`timeout` must be an integer when provided".into(),
                error_code: Some(INVALID_INPUT_CODE),
            })
        }
    };

    let description = match object.get("description") {
        Some(Value::String(raw)) => Some(raw.trim().to_owned()).filter(|raw| !raw.is_empty()),
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: "`description` must be a string when provided".into(),
                error_code: Some(INVALID_INPUT_CODE),
            })
        }
    };

    Ok(PowerShellInput {
        command,
        timeout_ms,
        description,
        run_in_background: optional_bool(
            object.get("run_in_background"),
            "run_in_background",
            &tool,
        )?
        .unwrap_or(false),
        dangerously_disable_sandbox: optional_bool(
            object.get("dangerouslyDisableSandbox"),
            "dangerouslyDisableSandbox",
            &tool,
        )?
        .unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[cfg(windows)]
    use std::sync::Arc;

    fn tool() -> PowerShellTool {
        PowerShellTool
    }

    // ── which commands run without a prompt ───────────────────────

    #[tokio::test]
    async fn check_permissions_allows_a_command_that_only_reads() {
        for command in [
            "Get-ChildItem -Recurse src",
            "Get-Content Cargo.toml | Select-String version",
            "gci | Select-Object -First 5",
            "git status",
        ] {
            let input = json!({ "command": command });
            let decision = tool()
                .check_permissions(&input, &ToolContext::new())
                .await
                .unwrap();
            assert_eq!(decision, PermissionDecision::allow(input), "{command:?}");
        }
    }

    #[tokio::test]
    async fn check_permissions_asks_for_anything_else() {
        for (command, disables_sandbox) in [
            ("Remove-Item x", false),
            ("Get-ChildItem | Remove-Item", false),
            ("Get-Content x > y", false),
            ("Get-ChildItem | ForEach-Object { $_ }", false),
            ("git $args", false),
            ("Get-ChildItem", true),
        ] {
            let input = json!({
                "command": command,
                "dangerouslyDisableSandbox": disables_sandbox,
            });
            let decision = tool()
                .check_permissions(&input, &ToolContext::new())
                .await
                .unwrap();
            assert_eq!(
                decision.behavior,
                rebon_tools_core::PermissionBehavior::Ask,
                "{command:?}"
            );
        }
    }

    // ── which passing mode a command gets ─────────────────────────

    mod passing_mode {
        use super::*;
        use crate::command_sandbox::PASSTHROUGH_BACKEND;
        use std::path::PathBuf;

        /// A sandbox with no operating system behind it.
        ///
        /// What these tests are about is the *PowerShell* half —
        /// which passing mode a command gets, and whether the payload
        /// survives it. Which commands a real sandbox wraps is the sandbox
        /// plugin's own decision table, tested there; standing a seatbelt
        /// profile up here would test that table twice and this one not at
        /// all.
        #[derive(Debug, Default)]
        struct FakeSandbox {
            /// Commands that bypass, matched on the whole string.
            excluded: Vec<String>,
            /// What the flag does: accepted (bypass) or refused.
            disable_is_refused: bool,
        }

        impl FakeSandbox {
            fn wrapping() -> Self {
                Self::default()
            }

            fn excluding(command: &str) -> Self {
                Self {
                    excluded: vec![command.to_string()],
                    ..Self::default()
                }
            }

            fn strict() -> Self {
                Self {
                    disable_is_refused: true,
                    ..Self::default()
                }
            }
        }

        impl CommandSandbox for FakeSandbox {
            fn check(&self, tool: &ToolId, command: &str, disable: bool) -> ToolResult<()> {
                if self.will_wrap(command, disable) || !disable || !self.disable_is_refused {
                    return Ok(());
                }
                Err(ToolError::PermissionDenied {
                    tool: tool.clone(),
                    reason: "fake strict mode".into(),
                })
            }

            fn will_wrap(&self, command: &str, disable: bool) -> bool {
                if self.excluded.iter().any(|excluded| excluded == command) {
                    return false;
                }
                !disable
            }

            fn prepare(
                &self,
                tool: &ToolId,
                policy_command: &str,
                payload: &str,
                shell: BinShell,
                cwd: Option<&str>,
                disable: bool,
            ) -> ToolResult<PreparedCommand> {
                if !self.will_wrap(policy_command, disable) {
                    if disable && self.disable_is_refused {
                        return Err(ToolError::PermissionDenied {
                            tool: tool.clone(),
                            reason: "fake strict mode".into(),
                        });
                    }
                    return Ok(PreparedCommand::passthrough(&shell, payload, cwd));
                }
                // A stand-in for `bwrap … -- <shell argv>`: the shell and its
                // payload become the *tail* of a longer argv, which is the
                // shape that makes the mode question matter at all.
                let mut args = vec!["--".to_string()];
                args.extend(shell.argv(payload));
                Ok(PreparedCommand {
                    program: "fake-wrapper".to_string(),
                    args,
                    env_set: Vec::new(),
                    env_unset: Vec::new(),
                    cwd: cwd.map(PathBuf::from),
                    confined: true,
                    backend: "fake",
                    notices: Vec::new(),
                })
            }
        }

        fn pwsh() -> PowerShellRuntime {
            PowerShellRuntime {
                path: PathBuf::from("/usr/bin/pwsh"),
                edition: PowerShellEdition::Core,
                reason: detect::DetectionReason::Path,
            }
        }

        fn prepare(
            sandbox: Option<&dyn CommandSandbox>,
            command: &str,
            disable: bool,
        ) -> PreparedCommand {
            prepare_powershell_command(
                sandbox,
                &ToolId::new(POWERSHELL_TOOL_NAME),
                &pwsh(),
                command,
                None,
                disable,
            )
            .unwrap()
        }

        /// The flag that decides the mode, wherever it ended up in
        /// the argv — under a sandbox the pwsh argv is a suffix.
        fn mode_flag(prepared: &PreparedCommand) -> &'static str {
            let args = &prepared.args;
            if args.iter().any(|arg| arg == "-EncodedCommand") {
                "-EncodedCommand"
            } else if args.iter().any(|arg| arg == "-Command") {
                "-Command"
            } else {
                panic!("neither passing mode present: {args:?}")
            }
        }

        #[test]
        fn without_a_sandbox_the_command_is_passed_as_plain_text() {
            let prepared = prepare(None, "Get-Process", false);
            assert_eq!(mode_flag(&prepared), "-Command");
            assert_eq!(prepared.program, "/usr/bin/pwsh");
            assert_eq!(prepared.backend, PASSTHROUGH_BACKEND);
            assert!(prepared.args.last().unwrap().contains("Get-Process"));
        }

        #[test]
        fn a_sandboxed_command_is_passed_base64_encoded() {
            let sandbox = FakeSandbox::wrapping();
            let prepared = prepare(Some(&sandbox), "Get-Process", false);

            assert_eq!(mode_flag(&prepared), "-EncodedCommand");
            assert!(prepared.confined);
            // The payload is the last argv element and carries nothing
            // an intermediate layer could interpret.
            let payload = prepared.args.last().unwrap();
            assert!(payload
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
        }

        #[test]
        fn an_excluded_command_stays_in_plain_text_mode() {
            let sandbox = FakeSandbox::excluding("git status");
            let prepared = prepare(Some(&sandbox), "git status", false);

            assert_eq!(mode_flag(&prepared), "-Command");
            assert!(!prepared.confined);
            assert_eq!(prepared.backend, PASSTHROUGH_BACKEND);
        }

        #[test]
        fn an_accepted_disable_flag_keeps_the_cheaper_plain_text_mode() {
            let sandbox = FakeSandbox::wrapping();
            let prepared = prepare(Some(&sandbox), "Get-Process", true);

            assert_eq!(mode_flag(&prepared), "-Command");
            assert!(!prepared.confined);
        }

        #[test]
        fn a_refused_disable_flag_is_an_error_rather_than_either_mode() {
            let sandbox = FakeSandbox::strict();
            let error = prepare_powershell_command(
                Some(&sandbox),
                &ToolId::new(POWERSHELL_TOOL_NAME),
                &pwsh(),
                "Get-Process",
                None,
                true,
            )
            .unwrap_err();

            assert!(matches!(error, ToolError::PermissionDenied { .. }));
        }

        #[test]
        fn the_encoded_payload_decodes_back_to_the_full_script() {
            use base64::Engine as _;
            let sandbox = FakeSandbox::wrapping();
            let prepared = prepare(Some(&sandbox), "Get-Process", false);

            let payload = prepared.args.last().unwrap();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(payload)
                .unwrap();
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            let script = String::from_utf16(&units).unwrap();

            assert!(script.contains("Get-Process"));
            assert!(
                script.contains("$__rebon_exit"),
                "the exit-code epilogue must survive encoding: {script}"
            );
        }

        #[test]
        fn the_pwsh_argv_is_the_tail_of_the_sandbox_argv() {
            let sandbox = FakeSandbox::wrapping();
            let prepared = prepare(Some(&sandbox), "Get-Process", false);
            let args = &prepared.args;

            // A wrapper runs `… -- pwsh …`, so pwsh and its flags are the
            // tail, in order.
            let pwsh_at = args
                .iter()
                .position(|arg| arg == "/usr/bin/pwsh")
                .expect("pwsh in argv");
            assert_eq!(args[pwsh_at + 1], "-NoProfile");
            assert_eq!(args[pwsh_at + 2], "-NonInteractive");
            assert_eq!(args[pwsh_at + 3], "-EncodedCommand");
            assert_eq!(args.len(), pwsh_at + 5);
        }

        /// The environment the tool adds for formatting must not overwrite
        /// one the sandbox set for a reason.
        #[test]
        fn the_sandbox_environment_wins_over_the_formatting_overrides() {
            let (key, _) = command::environment_overrides()[0];
            let prepared = PreparedCommand {
                program: "pwsh".to_string(),
                args: Vec::new(),
                env_set: vec![(key.to_string(), "sandbox-value".to_string())],
                env_unset: Vec::new(),
                cwd: None,
                confined: true,
                backend: "fake",
                notices: Vec::new(),
            };

            let process = configured_powershell_command(&prepared);
            let envs: Vec<_> = process.as_std().get_envs().collect();
            assert!(
                envs.iter()
                    .any(|(name, value)| *name == key && *value == Some("sandbox-value".as_ref())),
                "{envs:?}"
            );
        }
    }

    #[tokio::test]
    async fn validate_input_accepts_background_mode() {
        let result = tool()
            .validate_input(
                &json!({ "command": "Write-Output hi", "run_in_background": true }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(result.is_valid());
    }

    #[tokio::test]
    async fn validate_input_rejects_non_boolean_background_mode() {
        let result = tool()
            .validate_input(
                &json!({ "command": "Write-Output hi", "run_in_background": "yes" }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!result.is_valid());
    }

    #[tokio::test]
    async fn validate_input_refuses_a_command_the_prompt_cannot_show() {
        let result = tool()
            .validate_input(
                &json!({ "command": "Write-Output \u{1b}[2Ksomething-else" }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!result.is_valid());
        assert!(
            result.message.unwrap().contains("approval prompt"),
            "the refusal has to say why"
        );
    }

    /// But only in the foreground; background is where waiting
    /// belongs.
    #[tokio::test]
    async fn validate_input_refuses_a_long_foreground_sleep_only() {
        let foreground = tool()
            .validate_input(
                &json!({ "command": "Start-Sleep -Seconds 120" }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!foreground.is_valid());

        let background = tool()
            .validate_input(
                &json!({ "command": "Start-Sleep -Seconds 120", "run_in_background": true }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(background.is_valid());
    }

    #[tokio::test]
    async fn validate_input_accepts_a_short_sleep() {
        let result = tool()
            .validate_input(
                &json!({ "command": "Start-Sleep -Milliseconds 200" }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(result.is_valid());
    }

    #[tokio::test]
    async fn validate_input_rejects_a_timeout_over_the_cap() {
        let result = tool()
            .validate_input(
                &json!({ "command": "Write-Output hi", "timeout": MAX_TIMEOUT_MS + 1 }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!result.is_valid());
    }

    #[test]
    fn schema_carries_the_rfc_input_fields() {
        let schema = tool().input_schema();
        let properties = schema["properties"].as_object().unwrap();
        for field in [
            "command",
            "timeout",
            "description",
            "run_in_background",
            "dangerouslyDisableSandbox",
        ] {
            assert!(properties.contains_key(field), "missing {field}");
        }
        assert_eq!(schema["required"], json!(["command"]));
    }

    #[test]
    fn description_matches_the_detected_edition() {
        let powershell = tool();
        let description = powershell.description();
        match described_edition() {
            PowerShellEdition::Core => assert!(description.contains("PowerShell 7+")),
            PowerShellEdition::Desktop => assert!(description.contains("Windows PowerShell 5.1")),
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn call_runs_background_powershell_and_decodes_console_output() {
        let registry = Arc::new(crate::ShellProcessRegistry::new());
        let context = ToolContext::new()
            .with_session_id("powershell-background-test")
            .with_shell_process_registry(registry);
        let started = tool()
            .call(
                json!({
                    "command": "Write-Output first; Write-Output ([char]0x6210 + [char]0x529f); Write-Output second",
                    "run_in_background": true
                }),
                &context,
            )
            .await
            .unwrap();
        let shell_id = started["shellId"].as_str().unwrap().to_owned();
        assert_eq!(started["timeoutMs"], Value::Null);

        let mut cursor = 0;
        let mut output = String::new();
        loop {
            let update = crate::ShellOutputTool
                .call(
                    json!({
                        "shellId": shell_id,
                        "cursor": cursor,
                        "wait": true,
                        "timeout": 5_000
                    }),
                    &context,
                )
                .await
                .unwrap();
            output.push_str(update["output"].as_str().unwrap_or(""));
            cursor = update["nextCursor"].as_u64().unwrap();
            if update["completed"] == true {
                break;
            }
        }

        assert!(output.contains("first"));
        assert!(output.contains("成功"), "{output:?}");
        assert!(output.contains("second"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn call_decodes_windows_console_stdout_and_stderr() {
        let out = tool()
            .call(
                json!({
                    "command": "Write-Output ([char]0x6210 + [char]0x529f); [Console]::Error.WriteLine(([char]0x9519).ToString() + [char]0x8bef)"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(out["stdout"], json!("成功"));
        assert_eq!(out["stderr"], json!("错误"));
    }

    /// The reason PowerShell's own output is pinned to the console code page:
    /// a legacy native tool writes that page's bytes straight into our pipe,
    /// and if PowerShell wrote UTF-8 alongside it neither half would decode.
    #[cfg(windows)]
    #[tokio::test]
    async fn call_decodes_native_command_console_output() {
        let out = tool()
            .call(
                json!({
                    "command": "& ([System.IO.Path]::Combine([Environment]::SystemDirectory, 'taskkill.exe')) /?"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        let stdout = out["stdout"].as_str().unwrap();
        assert!(stdout.contains("TASKKILL"), "{stdout:?}");
        assert!(!stdout.contains('\u{FFFD}'), "{stdout:?}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn call_executes_powershell_command() {
        let out = tool()
            .call(
                json!({ "command": "Write-Output hello" }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(out["stdout"], json!("hello"));
        assert_eq!(out["exitCode"], json!(0));
    }

    /// The epilogue's reason: without it a failing cmdlet still
    /// exits the process 0, so the model reads a failure as a success.
    #[cfg(windows)]
    #[tokio::test]
    async fn failing_cmdlet_reports_a_non_zero_exit_code() {
        let out = tool()
            .call(
                json!({ "command": "Get-Item 'Z:\\definitely\\not\\here' -ErrorAction Stop" }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert_ne!(out["exitCode"], json!(0), "{out}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn native_command_exit_code_survives() {
        let out = tool()
            .call(
                json!({ "command": "& cmd.exe /c exit 7" }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(out["exitCode"], json!(7), "{out}");
    }

    /// The prologue must not be prepended to a script-level declaration, and
    /// the proof is that such a command still runs.
    #[cfg(windows)]
    #[tokio::test]
    async fn script_level_declaration_still_parses() {
        let out = tool()
            .call(
                json!({ "command": "using namespace System.IO\nWrite-Output ([Path]::GetFileName('a\\b.txt'))" }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(out["stdout"], json!("b.txt"), "{out}");
    }
}

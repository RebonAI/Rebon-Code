//! Running a shell tool's foreground command — `Bash` and `PowerShell`
//! share it — and what happens when the command outlives its timeout.
//!
//! A command still running at its timeout is not killed. It is handed to
//! the background-shell registry ([`ShellProcessRegistry::adopt_foreground`])
//! and keeps running as a background shell: the result carries the output so
//! far and the new `shellId`, the output file continues the same stream, and
//! the shell's exit is announced once, like any background shell's. Killing
//! threw away a build or a test run that only needed longer; the model then
//! reran it with a bigger timeout and paid for the first run twice.
//!
//! The command is killed as before only when it cannot be handed over (no
//! registry, no owner, the running-shell limit), and the result says why.
//!
//! [`ShellProcessRegistry::adopt_foreground`]: crate::shell_process::ShellProcessRegistry::adopt_foreground

use crate::shell_process::{ShellLineReader, ShellOutputEncoding};
use crate::ToolContext;
use rebon_tools_core::{ToolError, ToolId, ToolProgressUpdate, ToolResult};
use serde_json::Value;
use std::io;
use tokio::process::Command;
use tokio::time::{sleep, Duration};

/// How a foreground run ended.
pub(crate) enum Ending {
    /// The command exited before its timeout.
    Exited,
    /// The command itself had exited when the timeout passed, but a process
    /// it started (`server &`) still held its output open. Output after the
    /// timeout was not collected; the exit status is the command's.
    OutputHeldOpen,
    /// Timed out and killed, for the reason given: it could not be handed to
    /// the background.
    Killed { reason: String },
    /// Timed out and now running as a background shell. The registry's
    /// handoff fields: `shellId`, `outputPath`, `status`, `backgrounded`,
    /// `note`.
    Backgrounded(Value),
}

pub(crate) struct ForegroundRun {
    /// `(is_stderr, line)` in the order the lines were read.
    pub(crate) lines: Vec<(bool, String)>,
    /// `None` while the command still runs in the background, or when the
    /// platform reports no code (a signal).
    pub(crate) exit_code: Option<i32>,
    pub(crate) ending: Ending,
}

impl ForegroundRun {
    /// Add what the ending means to a tool result that already has
    /// `stdout`, `stderr` and `exitCode`: `interrupted` and `timedOut`, and
    /// for an ending other than a plain exit the fields that explain it.
    pub(crate) fn annotate(self, result: &mut Value) {
        let (interrupted, timed_out) = match &self.ending {
            Ending::Exited | Ending::OutputHeldOpen => (false, false),
            Ending::Killed { .. } => (true, true),
            // Still running: the step is not done, which is what a later
            // step of the same response needs to know.
            Ending::Backgrounded(_) => (false, true),
        };
        result["interrupted"] = Value::Bool(interrupted);
        result["timedOut"] = Value::Bool(timed_out);
        match self.ending {
            Ending::Exited => {}
            Ending::OutputHeldOpen => {
                result["outputCutOff"] = Value::Bool(true);
                result["note"] = Value::String(
                    "The command exited, but a process it started kept its output open until \
                     the timeout; output after that was not collected. Redirect a process you \
                     leave running (`cmd >/dev/null 2>&1 &`)."
                        .to_string(),
                );
            }
            Ending::Killed { reason } => {
                result["note"] = Value::String(format!(
                    "Timed out and was killed: it could not continue in the background \
                     because {reason}."
                ));
            }
            Ending::Backgrounded(handoff) => {
                if let (Some(result), Value::Object(handoff)) = (result.as_object_mut(), handoff) {
                    result.extend(handoff);
                }
            }
        }
    }
}

/// Run `command` in the foreground: stream its lines as progress, collect
/// them, and at `timeout_ms` hand it to the background (see the module
/// docs). `process_error` builds the tool's error for a failed spawn, read
/// or wait, from the stage, the OS error and the pid.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_foreground<F>(
    context: &ToolContext,
    mut command: Command,
    tool_name: &'static str,
    command_text: &str,
    timeout_ms: u64,
    encoding: ShellOutputEncoding,
    process_error: F,
) -> ToolResult<ForegroundRun>
where
    F: Fn(&'static str, io::Error, Option<u32>) -> ToolError,
{
    // Its own group, so the whole tree can be taken over at the timeout.
    crate::shell_process::configure_process_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|err| process_error("spawn", err, None))?;
    let child_pid = child.id();
    let missing = |stream: &str| ToolError::Execution {
        tool: ToolId::new(tool_name),
        source: anyhow::anyhow!("failed to capture child {stream}"),
    };
    let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| missing("stderr"))?;

    // Cancel-safe readers: the `select!` below drops whichever read loses
    // the race, and a line half-read at that moment has to survive into the
    // next poll rather than disappear. PowerShell flushes a line's text and
    // its newline separately, so for it that is the common case.
    let mut stdout_reader = ShellLineReader::new(stdout, encoding);
    let mut stderr_reader = ShellLineReader::new(stderr, encoding);
    let timeout = sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timeout);

    // One arrival-ordered log instead of two per-stream buckets. The result
    // still reports `stdout` / `stderr` separately for the model, but keeping
    // the interleaving here is what lets a renderer put cargo's stderr
    // progress lines back ahead of its stdout results.
    let mut lines: Vec<(bool, String)> = Vec::new();
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut timed_out = false;

    while stdout_open || stderr_open {
        tokio::select! {
            _ = &mut timeout => {
                timed_out = true;
                break;
            }
            line = stdout_reader.next_line(), if stdout_open => {
                match line {
                    Ok(Some(line)) => {
                        context.emit_progress(ToolProgressUpdate::new("stdout").with_message(line.clone()));
                        lines.push((false, line));
                    }
                    Ok(None) => stdout_open = false,
                    Err(err) => return Err(process_error("read_stdout", err, child_pid)),
                }
            }
            line = stderr_reader.next_line(), if stderr_open => {
                match line {
                    Ok(Some(line)) => {
                        context.emit_progress(ToolProgressUpdate::new("stderr").with_message(line.clone()));
                        lines.push((true, line));
                    }
                    Ok(None) => stderr_open = false,
                    Err(err) => return Err(process_error("read_stderr", err, child_pid)),
                }
            }
        }
    }

    let ending = if !timed_out {
        Ending::Exited
    } else if let Ok(Some(_)) = child.try_wait() {
        Ending::OutputHeldOpen
    } else {
        let refused = match context.shell_process_registry() {
            None => Err((
                child,
                "there is no background shell registry here".to_string(),
            )),
            Some(registry) => {
                registry
                    .adopt_foreground(
                        context,
                        child,
                        stdout_reader,
                        stderr_reader,
                        tool_name,
                        command_text.to_string(),
                        &lines,
                        timeout_ms,
                    )
                    .await
            }
        };
        match refused {
            Ok(handoff) => {
                return Ok(ForegroundRun {
                    lines,
                    exit_code: None,
                    ending: Ending::Backgrounded(handoff),
                });
            }
            Err((returned, reason)) => {
                child = returned;
                let _ = child.kill().await;
                Ending::Killed { reason }
            }
        }
    };

    let status = child
        .wait()
        .await
        .map_err(|err| process_error("wait", err, child_pid))?;
    Ok(ForegroundRun {
        lines,
        exit_code: status.code(),
        ending,
    })
}

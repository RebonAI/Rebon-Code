//! `settings.json#statusLine` and `appStatusLines`: loading the config,
//! running the command, and parsing what comes back.
//!
//! This is the same thing a command hook is — same settings files, same
//! subprocess, same output to parse — differing only in that the product
//! is one line of text instead of a set of effects. How an ANSI span is
//! painted stays the reader's business; this module only produces spans
//! ([`parse_status_line_ansi`]).
//!
//! ## Load-order invariant
//!
//! User file first, project file second, project winning per slot: a
//! later file overrides an earlier one, and an explicit `null` clears a
//! slot rather than leaving it inherited. Enforced by
//! `parse_app_status_lines_patch` plus `AppStatusLinesPatch::apply`.
//!
//! ## Subprocess invariants
//!
//! The timeout covers the pipe reads, not just process exit: a descendant
//! that inherits stdout keeps the pipe open after the shell is gone, so
//! an uncapped read would hang the caller indefinitely. Both streams are
//! read with a cap for the same reason. Enforced in
//! `run_status_line_command_with_timeout` and `read_capped`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::settings_loader::SettingsPaths;

pub const STATUS_LINE_MAX_LINES: usize = 5;
pub const STATUS_LINE_STDOUT_LIMIT: usize = 8192;
pub const STATUS_LINE_TIMEOUT: Duration = Duration::from_millis(1200);
pub const DEFAULT_STATUS_LINE_REFRESH_INTERVAL_SECS: u64 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusLineCommandConfig {
    /// External command line (`type: "command"`). Empty when `script` is set.
    pub command: String,
    /// Render script module path (`type: "script"`): the module's default
    /// export is `(status) => string`, run on the embedded JS runtime — no
    /// process spawn, no Node. Takes precedence over `command`.
    pub script: Option<std::path::PathBuf>,
    pub refresh_interval: Option<u64>,
    /// Run this command for the newest message only, instead of every message
    /// in the conversation.
    ///
    /// A command that reports conversation-level state — token usage, cache
    /// behaviour, anything the surface knows only as "right now" — has the same
    /// answer for every row, so running it per message spends one process each
    /// on repeating a single line. Off by default: a command that reports
    /// something about the message itself does want every row.
    pub latest_only: bool,
}

impl StatusLineCommandConfig {
    pub fn refresh_interval_secs(&self) -> u64 {
        self.refresh_interval
            .unwrap_or(DEFAULT_STATUS_LINE_REFRESH_INTERVAL_SECS)
            .max(1)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppStatusLinesConfig {
    pub assistant_message: Option<StatusLineCommandConfig>,
    pub user_message: Option<StatusLineCommandConfig>,
    pub header_right: Option<StatusLineCommandConfig>,
}

impl AppStatusLinesConfig {
    pub fn command(&self, placement: AppStatusLinePlacement) -> Option<&StatusLineCommandConfig> {
        match placement {
            AppStatusLinePlacement::AssistantMessage => self.assistant_message.as_ref(),
            AppStatusLinePlacement::UserMessage => self.user_message.as_ref(),
            AppStatusLinePlacement::HeaderRight => self.header_right.as_ref(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppStatusLinePlacement {
    AssistantMessage,
    UserMessage,
    HeaderRight,
}

impl AppStatusLinePlacement {
    pub const fn payload_name(self) -> &'static str {
        match self {
            Self::AssistantMessage => "assistant_message",
            Self::UserMessage => "user_message",
            Self::HeaderRight => "header_right",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AppStatusLinesLoadError {
    #[error("failed to read status-line settings `{path}`: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("status-line settings `{path}` are not valid JSON: {source}")]
    InvalidJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("invalid appStatusLines in `{path}`: {reason}")]
    InvalidConfig { path: PathBuf, reason: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AppStatusLinesPatch {
    assistant_message: Option<Option<StatusLineCommandConfig>>,
    user_message: Option<Option<StatusLineCommandConfig>>,
    header_right: Option<Option<StatusLineCommandConfig>>,
}

impl AppStatusLinesPatch {
    fn apply(self, target: &mut AppStatusLinesConfig) {
        if let Some(value) = self.assistant_message {
            target.assistant_message = value;
        }
        if let Some(value) = self.user_message {
            target.user_message = value;
        }
        if let Some(value) = self.header_right {
            target.header_right = value;
        }
    }
}

pub fn load_app_status_lines(
    paths: &SettingsPaths,
) -> Result<AppStatusLinesConfig, AppStatusLinesLoadError> {
    let mut resolved = AppStatusLinesConfig::default();
    for path in [&paths.user, &paths.project] {
        let Some(value) = read_settings_value(path)? else {
            continue;
        };
        parse_app_status_lines_patch(&value)
            .map_err(|reason| AppStatusLinesLoadError::InvalidConfig {
                path: path.clone(),
                reason,
            })?
            .apply(&mut resolved);
    }
    Ok(resolved)
}

pub fn resolve_app_status_lines_values(
    user: Option<&Value>,
    project: Option<&Value>,
) -> Result<AppStatusLinesConfig, String> {
    let mut resolved = AppStatusLinesConfig::default();
    for value in [user, project].into_iter().flatten() {
        parse_app_status_lines_patch(value)?.apply(&mut resolved);
    }
    Ok(resolved)
}

fn read_settings_value(path: &Path) -> Result<Option<Value>, AppStatusLinesLoadError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(AppStatusLinesLoadError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|source| AppStatusLinesLoadError::InvalidJson {
            path: path.to_path_buf(),
            source,
        })
}

fn parse_app_status_lines_patch(value: &Value) -> Result<AppStatusLinesPatch, String> {
    let Some(raw) = value.get("appStatusLines") else {
        return Ok(AppStatusLinesPatch::default());
    };
    if raw.is_null() {
        return Ok(AppStatusLinesPatch {
            assistant_message: Some(None),
            user_message: Some(None),
            header_right: Some(None),
        });
    }
    let object = raw
        .as_object()
        .ok_or_else(|| "`appStatusLines` must be an object or null".to_string())?;
    Ok(AppStatusLinesPatch {
        assistant_message: parse_slot(object.get("assistantMessage"), "assistantMessage")?,
        user_message: parse_slot(object.get("userMessage"), "userMessage")?,
        header_right: parse_slot(object.get("headerRight"), "headerRight")?,
    })
}

fn parse_slot(
    value: Option<&Value>,
    slot: &str,
) -> Result<Option<Option<StatusLineCommandConfig>>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(Some(None));
    }
    let object = value
        .as_object()
        .ok_or_else(|| format!("`appStatusLines.{slot}` must be an object or null"))?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("`appStatusLines.{slot}.type` must be `command` or `script`"))?;
    let (command, script) = match kind {
        "command" => {
            let command = object
                .get("command")
                .and_then(Value::as_str)
                .filter(|command| !command.trim().is_empty())
                .ok_or_else(|| {
                    format!("`appStatusLines.{slot}.command` must be a non-empty string")
                })?
                .to_string();
            (command, None)
        }
        "script" => {
            let script = object
                .get("script")
                .and_then(Value::as_str)
                .filter(|script| !script.trim().is_empty())
                .ok_or_else(|| {
                    format!("`appStatusLines.{slot}.script` must be a non-empty path")
                })?;
            (String::new(), Some(std::path::PathBuf::from(script)))
        }
        other => {
            return Err(format!(
                "`appStatusLines.{slot}.type` must be `command` or `script`, got `{other}`"
            ));
        }
    };
    let refresh_interval = match object.get("refreshInterval") {
        None | Some(Value::Null) => None,
        Some(Value::Number(number)) => number
            .as_u64()
            .filter(|seconds| *seconds >= 1)
            .map(Some)
            .ok_or_else(|| {
                format!("`appStatusLines.{slot}.refreshInterval` must be an integer >= 1")
            })?,
        Some(_) => {
            return Err(format!(
                "`appStatusLines.{slot}.refreshInterval` must be an integer >= 1"
            ));
        }
    };
    let latest_only = match object.get("latestOnly") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(latest_only)) => *latest_only,
        Some(_) => {
            return Err(format!(
                "`appStatusLines.{slot}.latestOnly` must be a boolean"
            ));
        }
    };
    Ok(Some(Some(StatusLineCommandConfig {
        command,
        script,
        refresh_interval,
        latest_only,
    })))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLineCommandErrorKind {
    Spawn,
    Io,
    Timeout,
    NonZero,
    Empty,
}

impl StatusLineCommandErrorKind {
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Spawn => "command could not be spawned",
            Self::Io => "command I/O failed",
            Self::Timeout => "command timed out",
            Self::NonZero => "command exited with a non-zero status",
            Self::Empty => "command stdout was empty",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLineCommandFailure {
    pub kind: StatusLineCommandErrorKind,
    pub exit_code: Option<i32>,
    pub stderr: String,
}

impl StatusLineCommandFailure {
    fn new(kind: StatusLineCommandErrorKind) -> Self {
        Self {
            kind,
            exit_code: None,
            stderr: String::new(),
        }
    }
}

pub async fn run_status_line_command(
    command: &str,
    payload: Value,
    cwd: &Path,
    columns: u16,
    lines: u16,
) -> Result<Vec<String>, StatusLineCommandFailure> {
    run_status_line_command_with_timeout(command, payload, cwd, columns, lines, STATUS_LINE_TIMEOUT)
        .await
}

/// [`run_status_line_command`] with an explicit budget.
///
/// The production budget is deliberately tight — a status line that takes
/// longer than a frame is not a status line. That makes it the wrong budget
/// for tests that are checking what a command's *output* turns into: on a
/// loaded machine (a full `cargo test` run, say) merely starting a shell can
/// exceed it, and the test then fails for a reason it was never about.
/// Those tests pass a generous budget; the one that actually exercises the
/// timeout keeps the real one.
pub async fn run_status_line_command_with_timeout(
    command: &str,
    payload: Value,
    cwd: &Path,
    columns: u16,
    lines: u16,
    budget: Duration,
) -> Result<Vec<String>, StatusLineCommandFailure> {
    let mut child = spawn_status_line_command(command, cwd, columns, lines)?;
    let input = serde_json::to_vec(&payload)
        .map_err(|_| StatusLineCommandFailure::new(StatusLineCommandErrorKind::Io))?;
    let mut stdin_task = child.stdin.take().map(|mut stdin| {
        tokio::spawn(async move {
            stdin.write_all(&input).await?;
            stdin.shutdown().await
        })
    });
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| StatusLineCommandFailure::new(StatusLineCommandErrorKind::Io))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| StatusLineCommandFailure::new(StatusLineCommandErrorKind::Io))?;
    let mut stdout_task = tokio::spawn(read_capped(stdout, STATUS_LINE_STDOUT_LIMIT));
    let mut stderr_task = tokio::spawn(read_capped(stderr, 4096));

    // The timeout must cover the pipe reads, not just process exit: a
    // descendant process that inherits stdout/stderr keeps the pipes
    // open after the shell exits, and an uncapped read would hang this
    // future (and its message-command slot) indefinitely.
    let drive = async {
        let status = child
            .wait()
            .await
            .map_err(|_| StatusLineCommandFailure::new(StatusLineCommandErrorKind::Io))?;
        if let Some(task) = stdin_task.as_mut() {
            task.await
                .map_err(|_| StatusLineCommandFailure::new(StatusLineCommandErrorKind::Io))?
                .map_err(|_| StatusLineCommandFailure::new(StatusLineCommandErrorKind::Io))?;
        }
        let stdout = (&mut stdout_task)
            .await
            .map_err(|_| StatusLineCommandFailure::new(StatusLineCommandErrorKind::Io))?
            .map_err(|_| StatusLineCommandFailure::new(StatusLineCommandErrorKind::Io))?;
        let stderr = (&mut stderr_task)
            .await
            .map_err(|_| StatusLineCommandFailure::new(StatusLineCommandErrorKind::Io))?
            .map_err(|_| StatusLineCommandFailure::new(StatusLineCommandErrorKind::Io))?;
        Ok((status, stdout, stderr))
    };
    let outcome = tokio::time::timeout(budget, drive).await;
    let (status, stdout, stderr) = match outcome {
        Ok(Ok(output)) => output,
        Ok(Err(failure)) => {
            if let Some(task) = stdin_task {
                task.abort();
            }
            stdout_task.abort();
            stderr_task.abort();
            return Err(failure);
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            if let Some(task) = stdin_task {
                task.abort();
            }
            stdout_task.abort();
            stderr_task.abort();
            return Err(StatusLineCommandFailure::new(
                StatusLineCommandErrorKind::Timeout,
            ));
        }
    };

    if !status.success() {
        return Err(StatusLineCommandFailure {
            kind: StatusLineCommandErrorKind::NonZero,
            exit_code: status.code(),
            stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
        });
    }

    status_line_output_lines(&stdout)
}

fn status_line_output_lines(stdout: &[u8]) -> Result<Vec<String>, StatusLineCommandFailure> {
    let text = std::str::from_utf8(stdout)
        .map_err(|_| StatusLineCommandFailure::new(StatusLineCommandErrorKind::Io))?
        .trim_end_matches(['\r', '\n']);
    if text.trim().is_empty() {
        return Err(StatusLineCommandFailure::new(
            StatusLineCommandErrorKind::Empty,
        ));
    }
    Ok(text
        .lines()
        .take(STATUS_LINE_MAX_LINES)
        .map(str::to_string)
        .collect())
}

async fn read_capped<R>(mut reader: R, limit: usize) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(limit.min(4096));
    let mut chunk = [0_u8; 4096];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        if output.len() < limit {
            let remaining = limit - output.len();
            output.extend_from_slice(&chunk[..read.min(remaining)]);
        }
    }
    while std::str::from_utf8(&output).is_err_and(|error| error.error_len().is_none()) {
        output.pop();
    }
    Ok(output)
}

#[cfg(windows)]
fn native_shell_commands(command: &str) -> Vec<Command> {
    let mut programs = Vec::new();
    if let Some(root) = std::env::var_os("SystemRoot").or_else(|| std::env::var_os("windir")) {
        let root = PathBuf::from(root);
        programs.push(
            root.join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe"),
        );
        programs.push(
            root.join("Sysnative")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe"),
        );
    }
    programs.push(PathBuf::from("powershell.exe"));
    programs.push(PathBuf::from("pwsh.exe"));
    // `CREATE_NO_WINDOW` — prevents a visible console on Windows. Status
    // lines render on every message/header refresh; without this, a GUI
    // host pops one console window per render.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    programs
        .into_iter()
        .map(|program| {
            let mut command_process = Command::new(program);
            command_process
                .arg("-NoProfile")
                .arg("-NonInteractive")
                .arg("-Command")
                .arg(command)
                .creation_flags(CREATE_NO_WINDOW);
            command_process
        })
        .collect()
}

#[cfg(not(windows))]
fn native_shell_commands(command: &str) -> Vec<Command> {
    [PathBuf::from("/bin/sh"), PathBuf::from("sh")]
        .into_iter()
        .map(|program| {
            let mut command_process = Command::new(program);
            command_process.arg("-c").arg(command);
            command_process
        })
        .collect()
}

fn spawn_status_line_command(
    command: &str,
    cwd: &Path,
    columns: u16,
    lines: u16,
) -> Result<tokio::process::Child, StatusLineCommandFailure> {
    for mut command_process in native_shell_commands(command) {
        command_process
            .current_dir(cwd)
            .env("COLUMNS", columns.to_string())
            .env("LINES", lines.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Ok(child) = command_process.spawn() {
            return Ok(child);
        }
    }
    Err(StatusLineCommandFailure::new(
        StatusLineCommandErrorKind::Spawn,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnsiColor {
    Standard(u8),
    Bright(u8),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnsiStyle {
    pub foreground: Option<AnsiColor>,
    pub background: Option<AnsiColor>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsiSpan {
    pub text: String,
    pub style: AnsiStyle,
}

pub fn parse_status_line_ansi(input: &str) -> Vec<AnsiSpan> {
    let mut spans = Vec::new();
    let mut style = AnsiStyle::default();
    let mut text = String::new();
    let mut chars = input.chars().peekable();

    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    let mut parameters = String::new();
                    let mut final_byte = None;
                    for character in chars.by_ref() {
                        if ('@'..='~').contains(&character) {
                            final_byte = Some(character);
                            break;
                        }
                        parameters.push(character);
                    }
                    if final_byte == Some('m') {
                        push_ansi_span(&mut spans, &text, style);
                        text.clear();
                        apply_sgr(&parameters, &mut style);
                    }
                }
                Some(']') => {
                    let mut escaped = false;
                    for character in chars.by_ref() {
                        if character == '\u{7}' || (escaped && character == '\\') {
                            break;
                        }
                        escaped = character == '\u{1b}';
                    }
                }
                Some(_) | None => {}
            }
            continue;
        }
        if character == '\t' {
            text.push_str("    ");
        } else if !character.is_control() {
            text.push(character);
        }
    }
    push_ansi_span(&mut spans, &text, style);
    spans
}

fn push_ansi_span(spans: &mut Vec<AnsiSpan>, text: &str, style: AnsiStyle) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut().filter(|last| last.style == style) {
        last.text.push_str(text);
    } else {
        spans.push(AnsiSpan {
            text: text.to_string(),
            style,
        });
    }
}

fn apply_sgr(params: &str, style: &mut AnsiStyle) {
    let values = if params.is_empty() {
        vec![0]
    } else {
        params
            .split(';')
            .map(|part| part.parse::<u16>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let mut index = 0;
    while index < values.len() {
        match values[index] {
            0 => *style = AnsiStyle::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            30..=37 => style.foreground = Some(AnsiColor::Standard((values[index] - 30) as u8)),
            39 => style.foreground = None,
            40..=47 => style.background = Some(AnsiColor::Standard((values[index] - 40) as u8)),
            49 => style.background = None,
            90..=97 => style.foreground = Some(AnsiColor::Bright((values[index] - 90) as u8)),
            100..=107 => style.background = Some(AnsiColor::Bright((values[index] - 100) as u8)),
            38 | 48 => {
                let foreground = values[index] == 38;
                if values.get(index + 1) == Some(&5) {
                    if let Some(value) = values
                        .get(index + 2)
                        .and_then(|value| u8::try_from(*value).ok())
                    {
                        set_ansi_color(style, foreground, AnsiColor::Indexed(value));
                        index += 2;
                    }
                } else if values.get(index + 1) == Some(&2) {
                    let rgb = values
                        .get(index + 2..index + 5)
                        .and_then(|values| match values {
                            [r, g, b] => Some((
                                u8::try_from(*r).ok()?,
                                u8::try_from(*g).ok()?,
                                u8::try_from(*b).ok()?,
                            )),
                            _ => None,
                        });
                    if let Some((red, green, blue)) = rgb {
                        set_ansi_color(style, foreground, AnsiColor::Rgb(red, green, blue));
                        index += 4;
                    }
                }
            }
            _ => {}
        }
        index += 1;
    }
}

fn set_ansi_color(style: &mut AnsiStyle, foreground: bool, color: AnsiColor) {
    if foreground {
        style.foreground = Some(color);
    } else {
        style.background = Some(color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_and_project_settings_merge_per_slot() {
        let user = json!({
            "appStatusLines": {
                "assistantMessage": { "type": "command", "command": "assistant-user" },
                "userMessage": { "type": "command", "command": "user-user" },
                "headerRight": { "type": "command", "command": "header-user", "refreshInterval": 8 }
            }
        });
        let project = json!({
            "appStatusLines": {
                "assistantMessage": { "type": "command", "command": "assistant-project" },
                "userMessage": null
            }
        });

        let resolved = resolve_app_status_lines_values(Some(&user), Some(&project)).unwrap();
        assert_eq!(
            resolved.assistant_message.as_ref().unwrap().command,
            "assistant-project"
        );
        assert!(resolved.user_message.is_none());
        assert_eq!(
            resolved.header_right.as_ref().unwrap().command,
            "header-user"
        );
        assert_eq!(
            resolved
                .header_right
                .as_ref()
                .unwrap()
                .refresh_interval_secs(),
            8
        );
    }

    #[test]
    fn script_entries_parse_with_path_and_take_no_command() {
        let user = json!({
            "appStatusLines": {
                "assistantMessage": { "type": "script", "script": "C:/scripts/status.mjs", "refreshInterval": 3 },
                "headerRight": { "type": "command", "command": "header-cmd" }
            }
        });
        let resolved = resolve_app_status_lines_values(Some(&user), None).unwrap();
        let assistant = resolved.assistant_message.as_ref().unwrap();
        assert_eq!(
            assistant.script.as_deref(),
            Some(std::path::Path::new("C:/scripts/status.mjs"))
        );
        assert!(assistant.command.is_empty());
        assert_eq!(assistant.refresh_interval_secs(), 3);
        assert!(resolved.header_right.as_ref().unwrap().script.is_none());
    }

    #[test]
    fn script_entries_require_a_non_empty_path() {
        let user = json!({
            "appStatusLines": {
                "assistantMessage": { "type": "script", "script": "  " }
            }
        });
        let err = resolve_app_status_lines_values(Some(&user), None).unwrap_err();
        assert!(err.contains("script"), "{err}");
    }

    #[test]
    fn null_app_status_lines_disables_every_inherited_slot() {
        let user = json!({
            "appStatusLines": {
                "assistantMessage": { "type": "command", "command": "a" },
                "userMessage": { "type": "command", "command": "u" },
                "headerRight": { "type": "command", "command": "h" }
            }
        });
        let project = json!({ "appStatusLines": null });

        assert_eq!(
            resolve_app_status_lines_values(Some(&user), Some(&project)).unwrap(),
            AppStatusLinesConfig::default()
        );
    }

    #[test]
    fn invalid_slot_config_reports_its_full_path() {
        let value = json!({
            "appStatusLines": {
                "headerRight": { "type": "markdown", "command": "x" }
            }
        });

        let error = resolve_app_status_lines_values(Some(&value), None).unwrap_err();
        assert!(error.contains("appStatusLines.headerRight.type"));
    }

    #[test]
    fn refresh_interval_must_be_positive_integer() {
        for invalid in [json!(0), json!(1.5), json!("2")] {
            let value = json!({
                "appStatusLines": {
                    "headerRight": {
                        "type": "command",
                        "command": "x",
                        "refreshInterval": invalid
                    }
                }
            });
            assert!(resolve_app_status_lines_values(Some(&value), None).is_err());
        }
    }

    #[test]
    fn latest_only_defaults_off_and_rejects_non_booleans() {
        let default = json!({
            "appStatusLines": {
                "assistantMessage": { "type": "command", "command": "x" }
            }
        });
        let resolved = resolve_app_status_lines_values(Some(&default), None).unwrap();
        assert!(!resolved.assistant_message.as_ref().unwrap().latest_only);

        let enabled = json!({
            "appStatusLines": {
                "assistantMessage": { "type": "command", "command": "x", "latestOnly": true }
            }
        });
        let resolved = resolve_app_status_lines_values(Some(&enabled), None).unwrap();
        assert!(resolved.assistant_message.as_ref().unwrap().latest_only);

        for invalid in [json!("true"), json!(1)] {
            let value = json!({
                "appStatusLines": {
                    "assistantMessage": {
                        "type": "command",
                        "command": "x",
                        "latestOnly": invalid
                    }
                }
            });
            let error = resolve_app_status_lines_values(Some(&value), None).unwrap_err();
            assert!(error.contains("appStatusLines.assistantMessage.latestOnly"));
        }
    }

    #[test]
    fn command_must_be_non_empty() {
        for command in ["", "   "] {
            let value = json!({
                "appStatusLines": {
                    "assistantMessage": {
                        "type": "command",
                        "command": command
                    }
                }
            });
            assert!(resolve_app_status_lines_values(Some(&value), None).is_err());
        }
    }

    #[test]
    fn ansi_parser_preserves_sgr_styles_and_strips_other_controls() {
        let spans = parse_status_line_ansi(
            "plain \u{1b}[31;1mred\u{1b}[22;39m normal\u{1b}[2K!\u{1b}]0;hidden\u{7}\u{0}",
        );
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "plain ");
        assert_eq!(spans[1].text, "red");
        assert_eq!(spans[1].style.foreground, Some(AnsiColor::Standard(1)));
        assert!(spans[1].style.bold);
        assert_eq!(spans[2].text, " normal!");
        assert_eq!(spans[2].style, AnsiStyle::default());
    }

    #[test]
    fn ansi_parser_supports_indexed_truecolor_and_unicode() {
        let spans = parse_status_line_ansi("\u{1b}[38;5;200m紫\u{1b}[48;2;1;2;3m色\u{1b}[0m ok");
        assert_eq!(spans[0].text, "紫");
        assert_eq!(spans[0].style.foreground, Some(AnsiColor::Indexed(200)));
        assert_eq!(spans[1].text, "色");
        assert_eq!(spans[1].style.background, Some(AnsiColor::Rgb(1, 2, 3)));
        assert_eq!(spans[2].text, " ok");
        assert_eq!(spans[2].style, AnsiStyle::default());
    }

    #[test]
    fn output_parser_rejects_empty_and_invalid_utf8() {
        assert_eq!(
            status_line_output_lines(b" \r\n").unwrap_err().kind,
            StatusLineCommandErrorKind::Empty
        );
        assert_eq!(
            status_line_output_lines(&[0xff]).unwrap_err().kind,
            StatusLineCommandErrorKind::Io
        );
    }

    #[test]
    fn output_parser_limits_lines_and_preserves_unicode() {
        let lines = status_line_output_lines("一\n二\n三\n四\n五\n六\n".as_bytes()).unwrap();
        assert_eq!(lines, ["一", "二", "三", "四", "五"]);
    }

    #[tokio::test]
    async fn capped_reader_truncates_on_a_utf8_boundary() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let write = tokio::spawn(async move {
            writer.write_all("aaaa界tail".as_bytes()).await.unwrap();
        });

        let output = read_capped(reader, 6).await.unwrap();
        write.await.unwrap();

        assert_eq!(output, b"aaaa");
    }

    #[cfg(windows)]
    fn payload_echo_command() -> &'static str {
        "[Console]::Out.Write($env:COLUMNS + ':' + $env:LINES + ':'); [Console]::Out.Write([Console]::In.ReadToEnd())"
    }

    #[cfg(not(windows))]
    fn payload_echo_command() -> &'static str {
        "printf '%s:%s:' \"$COLUMNS\" \"$LINES\"; cat"
    }

    #[cfg(windows)]
    fn failing_command() -> &'static str {
        "[Console]::Error.Write('bad'); exit 7"
    }

    #[cfg(not(windows))]
    fn failing_command() -> &'static str {
        "printf bad >&2; exit 7"
    }

    #[cfg(windows)]
    fn slow_command() -> &'static str {
        "Start-Sleep -Seconds 3"
    }

    #[cfg(not(windows))]
    fn slow_command() -> &'static str {
        "sleep 3"
    }

    /// What the two output tests below give a command to finish in.
    ///
    /// Not the production budget: they are about what a command's output
    /// turns into, and on Windows they have to start PowerShell to produce
    /// any. Cold, under a full test run, that alone outlasts the 1.2s a
    /// status line gets, and the assertion then reads `Timeout` where it
    /// meant to read the command's own answer. Large enough that reaching
    /// it means something is genuinely wedged.
    const TEST_COMMAND_BUDGET: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn command_receives_payload_and_terminal_dimensions() {
        let lines = run_status_line_command_with_timeout(
            payload_echo_command(),
            json!({ "placement": "assistant_message" }),
            &std::env::current_dir().unwrap(),
            42,
            7,
            TEST_COMMAND_BUDGET,
        )
        .await
        .unwrap();

        assert_eq!(lines, ["42:7:{\"placement\":\"assistant_message\"}"]);
    }

    #[tokio::test]
    async fn command_reports_nonzero_exit_and_stderr() {
        let error = run_status_line_command_with_timeout(
            failing_command(),
            json!({}),
            &std::env::current_dir().unwrap(),
            80,
            1,
            TEST_COMMAND_BUDGET,
        )
        .await
        .unwrap_err();

        assert_eq!(error.kind, StatusLineCommandErrorKind::NonZero);
        assert_eq!(error.exit_code, Some(7));
        assert!(error.stderr.contains("bad"));
    }

    #[tokio::test]
    async fn command_is_killed_after_timeout() {
        let error = run_status_line_command(
            slow_command(),
            json!({}),
            &std::env::current_dir().unwrap(),
            80,
            1,
        )
        .await
        .unwrap_err();

        assert_eq!(error.kind, StatusLineCommandErrorKind::Timeout);
    }
}

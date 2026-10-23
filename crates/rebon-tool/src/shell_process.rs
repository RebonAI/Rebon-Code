use crate::{
    BackgroundShellCompletionStatus, BackgroundShellTaskCompletion, BackgroundShellTaskSpec,
    MonitorEventDisposition, MonitorTaskCompletion, MonitorTaskCompletionStatus, MonitorTaskSource,
    MonitorTaskSpec, TaskRuntimeController, ToolContext,
};
use rebon_tools_core::{ProcessTreeGuard, ToolError, ToolId, ToolProgressUpdate, ToolResult};
use rebon_types::PromptCancel;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout_at, Duration, Instant};
#[cfg(windows)]
use windows_sys::Win32::Globalization::{
    GetACP, GetOEMCP, IsDBCSLeadByteEx, MultiByteToWideChar, CP_UTF8,
};

const INVALID_INPUT_CODE: i64 = 400;
const UNKNOWN_SHELL_CODE: i64 = 404;
const READ_CHUNK_BYTES: usize = 8 * 1024;
/// Output kept in memory for the tail a wait or a completion notification
/// shows. The full output is in the shell's output file.
const TAIL_BUFFER_BYTES: usize = 64 * 1024;
/// How much of that tail one response carries.
const TAIL_RESPONSE_BYTES: usize = 16 * 1024;
const TAIL_RESPONSE_LINES: usize = 100;
/// Past this, output is still tailed but no longer written to the file, so
/// a runaway process cannot fill the disk.
const OUTPUT_FILE_MAX_BYTES: u64 = 64 * 1024 * 1024;
const OUTPUT_FILE_LIMIT_NOTE: &str =
    "\n[rebon: the output file reached its 64 MiB limit; later output was not written]\n";
const MONITOR_LINE_BUFFER_BYTES: usize = 1024 * 1024;
const MAX_RUNNING_SHELLS_PER_OWNER: usize = 16;
const MAX_COMPLETED_SHELLS_PER_OWNER: usize = 64;
/// How long the output readers get to reach end of file once the process
/// group is gone. They normally reach it at once; they do not while a
/// process outside the group holds a pipe's write end. On macOS that can be
/// any process another thread spawned while the pipe was being made: without
/// `pipe2`, std marks a new pipe close-on-exec in a second step.
const READER_DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
pub(crate) enum ShellOutputEncoding {
    Utf8,
    #[cfg(windows)]
    WindowsCodePage(u32),
}

impl ShellOutputEncoding {
    /// Encoding a PowerShell child's output arrives in.
    ///
    /// On Windows this is [`windows_console_code_page`], and the PowerShell
    /// tool pins `[Console]::OutputEncoding` to the *same* page in its command
    /// prologue (see [`crate::powershell::command`]). That alignment is what
    /// makes a single decoder correct: a native command that ends a pipeline
    /// writes its bytes straight to our pipe in the console page, while
    /// PowerShell's own output would otherwise use whatever the host defaults
    /// to — UTF-8 under pwsh 7.
    #[cfg(windows)]
    pub(crate) fn powershell() -> Self {
        Self::WindowsCodePage(windows_console_code_page())
    }

    #[cfg(not(windows))]
    pub(crate) fn powershell() -> Self {
        Self::Utf8
    }

    pub(crate) fn decode_complete(self, bytes: &[u8]) -> String {
        match self {
            Self::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
            #[cfg(windows)]
            Self::WindowsCodePage(CP_UTF8) => String::from_utf8_lossy(bytes).into_owned(),
            #[cfg(windows)]
            Self::WindowsCodePage(code_page) => decode_windows_code_page(code_page, bytes),
        }
    }
}

/// One child stream, read a line at a time, safely across cancellation.
///
/// The foreground shell tools read stdout and stderr in a single
/// `tokio::select!`, so every read is a future that gets dropped the moment
/// the *other* stream wins the race. [`AsyncBufReadExt::read_until`] survives
/// that only if the buffer it appends into outlives the future: its contract
/// is that partially read bytes stay in the caller's buffer and the call can
/// be resumed. A buffer allocated inside the read — the shape both tools used
/// before — is dropped with the cancelled future instead, so the bytes already
/// consumed from the pipe are gone and the line comes back short or empty.
///
/// That is not hypothetical on Windows: PowerShell flushes a line's text and
/// its newline as two writes, so a foreground command that writes to both
/// streams has a real window in which stdout is mid-line when stderr arrives.
/// Owning `pending` here is what closes it.
pub(crate) struct ShellLineReader<R> {
    reader: BufReader<R>,
    /// Bytes of the line being assembled, including any read that a
    /// cancellation cut short. Never contains a `\n`: `read_until` returns as
    /// soon as it finds one, so a cancelled read is always mid-line.
    pending: Vec<u8>,
    encoding: ShellOutputEncoding,
}

impl<R: AsyncRead + Unpin> ShellLineReader<R> {
    pub(crate) fn new(stream: R, encoding: ShellOutputEncoding) -> Self {
        Self {
            reader: BufReader::new(stream),
            pending: Vec::new(),
            encoding,
        }
    }

    /// The next line, or `None` at end of stream.
    ///
    /// Cancel-safe: dropping this future keeps whatever it had read in
    /// `pending`, and the next call continues the same line.
    pub(crate) async fn next_line(&mut self) -> io::Result<Option<String>> {
        let read = self.reader.read_until(b'\n', &mut self.pending).await?;
        // End of stream. A last line without a trailing newline is still a
        // line; only an empty `pending` means there is nothing left at all.
        if read == 0 && self.pending.is_empty() {
            return Ok(None);
        }
        let mut line = std::mem::take(&mut self.pending);
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Ok(Some(self.encoding.decode_complete(&line)))
    }
}

/// The console code page a Windows child writes its output in.
///
/// `CREATE_NO_WINDOW` means the child never inherits our console, so its
/// redirected output uses the OEM page; ANSI is the fallback for the rare
/// system that reports no OEM page at all.
#[cfg(windows)]
pub(crate) fn windows_console_code_page() -> u32 {
    let oem = unsafe { GetOEMCP() };
    let ansi = unsafe { GetACP() };
    [oem, ansi]
        .into_iter()
        .find(|code_page| *code_page != 0)
        .unwrap_or(CP_UTF8)
}

#[derive(Clone)]
pub struct ShellProcessRegistry {
    inner: Arc<RegistryInner>,
}

impl Default for ShellProcessRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ShellProcessRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellProcessRegistry")
            .field("entries", &lock(&self.inner.entries).len())
            .finish()
    }
}

impl ShellProcessRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RegistryInner::default()),
        }
    }

    pub(crate) async fn spawn(
        &self,
        context: &ToolContext,
        command: Command,
        tool_name: &'static str,
        command_text: String,
        timeout_ms: Option<u64>,
    ) -> ToolResult<Value> {
        self.spawn_with_output_encoding(
            context,
            command,
            tool_name,
            command_text,
            timeout_ms,
            ShellOutputEncoding::Utf8,
        )
        .await
    }

    pub(crate) async fn spawn_with_output_encoding(
        &self,
        context: &ToolContext,
        command: Command,
        tool_name: &'static str,
        command_text: String,
        timeout_ms: Option<u64>,
        output_encoding: ShellOutputEncoding,
    ) -> ToolResult<Value> {
        self.spawn_with_registration(
            context,
            command,
            tool_name,
            command_text,
            timeout_ms,
            output_encoding,
            ShellTaskRegistration::BackgroundShell,
        )
        .await
    }

    pub async fn spawn_monitor(
        &self,
        context: &ToolContext,
        command: Command,
        command_text: String,
        description: String,
        redacted_target: String,
        timeout_ms: Option<u64>,
    ) -> ToolResult<String> {
        let value = self
            .spawn_with_registration(
                context,
                command,
                "Monitor",
                command_text,
                timeout_ms,
                ShellOutputEncoding::Utf8,
                ShellTaskRegistration::Monitor {
                    description,
                    redacted_target,
                },
            )
            .await?;
        value
            .get("shellId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| execution_error("Monitor", anyhow::anyhow!("monitor task id missing")))
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_with_registration(
        &self,
        context: &ToolContext,
        mut command: Command,
        tool_name: &'static str,
        command_text: String,
        timeout_ms: Option<u64>,
        output_encoding: ShellOutputEncoding,
        registration: ShellTaskRegistration,
    ) -> ToolResult<Value> {
        let owner = owner_from_context(context, tool_name)?;
        self.ensure_capacity(&owner, tool_name)?;
        configure_process_group(&mut command);

        let mut child = command
            .spawn()
            .map_err(|err| execution_error(tool_name, err))?;
        let process_tree = match ProcessTreeControl::for_child(&child) {
            Ok(process_tree) => Arc::new(process_tree),
            Err(err) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(execution_error(tool_name, err));
            }
        };

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = process_tree.terminate();
                let _ = child.wait().await;
                return Err(execution_error(
                    tool_name,
                    anyhow::anyhow!("failed to capture child stdout"),
                ));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                let _ = process_tree.terminate();
                let _ = child.wait().await;
                return Err(execution_error(
                    tool_name,
                    anyhow::anyhow!("failed to capture child stderr"),
                ));
            }
        };

        let shell_id = self.unique_shell_id(tool_name, registration.is_monitor())?;
        // Monitors push their events as notifications; only a background
        // shell has output the model reads back, so only it gets a file.
        let output_file = if registration.is_monitor() {
            None
        } else {
            match create_output_file(context, &shell_id) {
                Ok(output_file) => Some(output_file),
                Err(err) => {
                    let _ = process_tree.terminate();
                    let _ = child.wait().await;
                    return Err(execution_error(tool_name, err));
                }
            }
        };
        let entry = Arc::new(ShellEntry::new(
            shell_id.clone(),
            owner.clone(),
            tool_name,
            command_text,
            timeout_ms,
            process_tree,
            context.task_runtime_controller().cloned(),
            PromptCancel::new(),
            registration,
            output_file,
        ));

        let capacity_exceeded = {
            let mut entries = lock(&self.inner.entries);
            prune_completed(&mut entries, &owner);
            if running_count(&entries, &owner) >= MAX_RUNNING_SHELLS_PER_OWNER {
                true
            } else {
                entries.insert(shell_id, Arc::clone(&entry));
                false
            }
        };
        if capacity_exceeded {
            let _ = entry.process_tree.terminate();
            let _ = child.wait().await;
            return Err(execution_error(
                tool_name,
                anyhow::anyhow!(
                    "background shell limit reached ({MAX_RUNNING_SHELLS_PER_OWNER} running shells)"
                ),
            ));
        }

        let stdout_task = tokio::spawn(read_stream(
            stdout,
            Arc::clone(&entry),
            "stdout",
            output_encoding,
        ));
        let stderr_task = tokio::spawn(read_stream(
            stderr,
            Arc::clone(&entry),
            "stderr",
            output_encoding,
        ));
        entry.register_task();
        tokio::spawn(supervise_process(
            child,
            stdout_task,
            stderr_task,
            Arc::clone(&entry),
            timeout_ms,
        ));

        Ok(entry.launch_value())
    }

    pub(crate) fn list(&self, context: &ToolContext, caller: &str) -> ToolResult<Value> {
        let owner = owner_from_context(context, caller)?;
        let mut shells: Vec<Value> = lock(&self.inner.entries)
            .values()
            // Monitors deliver through task notifications; listing them
            // here invites polling a stream that is already pushed.
            .filter(|entry| entry.owner == owner && !entry.registration.is_monitor())
            .map(|entry| entry.summary_value())
            .collect();
        shells.sort_by(|left, right| {
            right["startedAtMs"]
                .as_u64()
                .cmp(&left["startedAtMs"].as_u64())
        });
        Ok(json!({ "shells": shells }))
    }

    /// Block until the shell exits or `wait_timeout_ms` passes, then report
    /// its status and the tail of its output. With `wait` false it reports
    /// at once.
    ///
    /// A wait does not return on new output, only on exit: returning on the
    /// first chunk turned every wait on a chatty build into a model
    /// round-trip every few seconds. The output itself is in the file; this
    /// is for the moment the model needs the result.
    pub(crate) async fn wait(
        &self,
        context: &ToolContext,
        caller: &str,
        shell_id: &str,
        wait: bool,
        wait_timeout_ms: u64,
    ) -> ToolResult<Value> {
        let entry = self.entry(context, caller, shell_id)?;
        if entry.registration.is_monitor() {
            return Err(monitor_output_error(caller, shell_id));
        }
        // Subscribe before the first check, so an exit between the check
        // and the wait still wakes it.
        let mut changes = entry.changes.subscribe();
        let mut wait_timed_out = false;
        if wait && !entry.is_completed() {
            // Shown on the tool card while the wait runs, so it names the
            // command being waited on instead of an opaque shellId.
            context.emit_progress(
                ToolProgressUpdate::new("shell_wait")
                    .with_payload(entry.waiting_value(wait_timeout_ms)),
            );
            let deadline = Instant::now() + Duration::from_millis(wait_timeout_ms);
            while !entry.is_completed() {
                match timeout_at(deadline, changes.changed()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) | Err(_) => break,
                }
            }
            wait_timed_out = !entry.is_completed();
        }
        let (value, completed) = entry.result_value(wait_timed_out);
        if completed {
            entry.mark_observed();
        }
        Ok(value)
    }

    pub(crate) fn stop(
        &self,
        context: &ToolContext,
        caller: &str,
        shell_id: &str,
    ) -> ToolResult<Value> {
        let entry = self.entry(context, caller, shell_id)?;
        entry.request_stop(caller)
    }

    fn ensure_capacity(&self, owner: &ShellOwner, caller: &str) -> ToolResult<()> {
        let mut entries = lock(&self.inner.entries);
        prune_completed(&mut entries, owner);
        if running_count(&entries, owner) >= MAX_RUNNING_SHELLS_PER_OWNER {
            return Err(execution_error(
                caller,
                anyhow::anyhow!(
                    "background shell limit reached ({MAX_RUNNING_SHELLS_PER_OWNER} running shells)"
                ),
            ));
        }
        Ok(())
    }

    fn unique_shell_id(&self, caller: &str, monitor: bool) -> ToolResult<String> {
        loop {
            let shell_id = if monitor {
                random_monitor_id()
            } else {
                random_shell_id()
            }
            .map_err(|err| execution_error(caller, err))?;
            if !lock(&self.inner.entries).contains_key(&shell_id) {
                return Ok(shell_id);
            }
        }
    }

    fn entry(
        &self,
        context: &ToolContext,
        caller: &str,
        shell_id: &str,
    ) -> ToolResult<Arc<ShellEntry>> {
        let owner = owner_from_context(context, caller)?;
        lock(&self.inner.entries)
            .get(shell_id)
            .filter(|entry| entry.owner == owner)
            .cloned()
            .ok_or_else(|| unknown_shell_error(caller, shell_id))
    }
}

#[derive(Default)]
struct RegistryInner {
    entries: Mutex<HashMap<String, Arc<ShellEntry>>>,
}

impl Drop for RegistryInner {
    fn drop(&mut self) {
        let entries = self
            .entries
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for entry in entries.values() {
            if !entry.is_completed() {
                let _ = entry.process_tree.terminate();
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ShellOwner {
    session_id: Option<String>,
    agent_id: Option<String>,
}

fn owner_from_context(context: &ToolContext, caller: &str) -> ToolResult<ShellOwner> {
    let owner = ShellOwner {
        session_id: context.session_id().map(str::to_owned),
        agent_id: context.agent_id().map(str::to_owned),
    };
    if context.task_runtime_controller().is_some() && owner.session_id.is_none() {
        return Err(ToolError::InvalidInput {
            tool: ToolId::new(caller),
            reason: "background task runtime operations require a session_id".into(),
            error_code: Some(INVALID_INPUT_CODE),
        });
    }
    if owner.session_id.is_none() && owner.agent_id.is_none() {
        return Err(ToolError::InvalidInput {
            tool: ToolId::new(caller),
            reason: "background shell operations require a session_id or agent_id".into(),
            error_code: Some(INVALID_INPUT_CODE),
        });
    }
    Ok(owner)
}

#[derive(Debug, Clone)]
enum ShellTaskRegistration {
    BackgroundShell,
    Monitor {
        description: String,
        redacted_target: String,
    },
}

impl ShellTaskRegistration {
    fn is_monitor(&self) -> bool {
        matches!(self, Self::Monitor { .. })
    }
}

struct ShellEntry {
    shell_id: String,
    owner: ShellOwner,
    tool_name: &'static str,
    command: String,
    started_at_ms: u64,
    timeout_ms: Option<u64>,
    state: Mutex<ShellState>,
    changes: watch::Sender<u64>,
    process_tree: Arc<ProcessTreeControl>,
    task_runtime_controller: Option<Arc<dyn TaskRuntimeController>>,
    task_cancel: PromptCancel,
    registration: ShellTaskRegistration,
    monitor_line_buffer: Mutex<String>,
    observed: AtomicBool,
    /// Where the full output goes. `None` for a Monitor.
    output_path: Option<PathBuf>,
}

impl Drop for ShellEntry {
    /// The file lives as long as the shell is known: once the registry
    /// forgets the shell, nothing can point the model at it again.
    fn drop(&mut self) {
        if let Some(path) = &self.output_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl ShellEntry {
    #[allow(clippy::too_many_arguments)]
    fn new(
        shell_id: String,
        owner: ShellOwner,
        tool_name: &'static str,
        command: String,
        timeout_ms: Option<u64>,
        process_tree: Arc<ProcessTreeControl>,
        task_runtime_controller: Option<Arc<dyn TaskRuntimeController>>,
        task_cancel: PromptCancel,
        registration: ShellTaskRegistration,
        output_file: Option<(PathBuf, File)>,
    ) -> Self {
        let (changes, _) = watch::channel(0);
        let (output_path, file) = match output_file {
            Some((path, file)) => (Some(path), Some(file)),
            None => (None, None),
        };
        Self {
            shell_id,
            owner,
            tool_name,
            command,
            started_at_ms: now_ms(),
            timeout_ms,
            state: Mutex::new(ShellState {
                file,
                ..ShellState::default()
            }),
            changes,
            process_tree,
            task_runtime_controller,
            task_cancel,
            registration,
            monitor_line_buffer: Mutex::new(String::new()),
            observed: AtomicBool::new(false),
            output_path,
        }
    }

    fn output_path_text(&self) -> Option<String> {
        self.output_path
            .as_ref()
            .map(|path| path.display().to_string())
    }

    fn register_task(&self) {
        let Some(controller) = self.task_runtime_controller.as_ref() else {
            return;
        };
        let Some(session_id) = self.owner.session_id.as_deref() else {
            return;
        };
        match &self.registration {
            ShellTaskRegistration::BackgroundShell => controller.background_shell_started(
                session_id,
                BackgroundShellTaskSpec {
                    shell_id: self.shell_id.clone(),
                    tool_name: self.tool_name.to_string(),
                    command: self.command.clone(),
                    session_id: self.owner.session_id.clone(),
                    agent_id: self.owner.agent_id.clone(),
                    started_at_ms: self.started_at_ms,
                },
                self.task_cancel.clone(),
            ),
            ShellTaskRegistration::Monitor {
                description,
                redacted_target,
            } => controller.monitor_started(
                session_id,
                MonitorTaskSpec {
                    task_id: self.shell_id.clone(),
                    description: description.clone(),
                    source: MonitorTaskSource::Command,
                    redacted_target: redacted_target.clone(),
                    session_id: self.owner.session_id.clone(),
                    agent_id: self.owner.agent_id.clone(),
                    started_at_ms: self.started_at_ms,
                },
                self.task_cancel.clone(),
            ),
        }
    }

    fn mark_observed(&self) {
        if !matches!(&self.registration, ShellTaskRegistration::BackgroundShell)
            || self.observed.swap(true, Ordering::AcqRel)
        {
            return;
        }
        if let (Some(controller), Some(session_id)) = (
            self.task_runtime_controller.as_ref(),
            self.owner.session_id.as_deref(),
        ) {
            controller.background_shell_observed(session_id, &self.shell_id);
        }
    }

    fn push_output(&self, stream: &'static str, text: String) {
        if text.is_empty() {
            return;
        }
        let is_monitor = self.registration.is_monitor();
        let monitor_text = (stream == "stdout" && is_monitor).then(|| text.clone());
        // One lock for the file and the tail, so both see the two streams'
        // chunks in the same order.
        let mut state = lock(&self.state);
        state.output_bytes = state.output_bytes.saturating_add(text.len() as u64);
        state.write_to_file(&text);
        if is_monitor && stream == "stderr" {
            push_bounded(&mut state.stderr_tail, &text, TAIL_BUFFER_BYTES);
        }
        if push_bounded(&mut state.tail, &text, TAIL_BUFFER_BYTES) {
            state.tail_dropped = true;
        }
        drop(state);
        if let Some(text) = monitor_text {
            self.push_monitor_stdout(&text);
        }
        self.bump_change();
    }

    fn push_monitor_stdout(&self, text: &str) {
        let events = {
            let mut buffer = lock(&self.monitor_line_buffer);
            buffer.push_str(text);
            if buffer.len() > MONITOR_LINE_BUFFER_BYTES {
                let mut start = buffer.len() - MONITOR_LINE_BUFFER_BYTES;
                while !buffer.is_char_boundary(start) {
                    start += 1;
                }
                buffer.drain(..start);
            }

            let mut events = Vec::new();
            while let Some(newline) = buffer.find('\n') {
                let mut line = buffer.drain(..=newline).collect::<String>();
                line.pop();
                let line = line.trim();
                if !line.is_empty() {
                    events.push(line.to_string());
                }
            }
            events
        };
        for event in events {
            self.emit_monitor_event(event);
        }
    }

    fn flush_monitor_stdout(&self) {
        if !self.registration.is_monitor() {
            return;
        }
        let line = std::mem::take(&mut *lock(&self.monitor_line_buffer));
        let line = line.trim();
        if !line.is_empty() {
            self.emit_monitor_event(line.to_string());
        }
    }

    fn emit_monitor_event(&self, event: String) {
        let Some(controller) = self.task_runtime_controller.as_ref() else {
            return;
        };
        let Some(session_id) = self.owner.session_id.as_deref() else {
            return;
        };
        if controller.monitor_event(session_id, &self.shell_id, event)
            == MonitorEventDisposition::AutoStop
        {
            self.request_auto_stop();
        }
    }

    fn request_auto_stop(&self) {
        let mut state = lock(&self.state);
        if state.status.is_terminal() || state.auto_stopped {
            return;
        }
        state.auto_stopped = true;
        state.status = ShellStatus::Stopping;
        drop(state);
        self.task_cancel.cancel();
        self.bump_change();
    }

    fn record_error(&self, error: impl Into<String>) {
        let mut state = lock(&self.state);
        if state.error.is_none() {
            state.error = Some(error.into());
        }
        drop(state);
        self.bump_change();
    }

    fn mark_timed_out(&self) -> bool {
        let mut state = lock(&self.state);
        if state.status.is_terminal()
            || state.timed_out
            || state.stop_requested
            || state.auto_stopped
        {
            return false;
        }
        state.timed_out = true;
        state.status = ShellStatus::Stopping;
        drop(state);
        self.bump_change();
        true
    }

    fn mark_stop_requested(&self) -> (bool, bool) {
        let mut state = lock(&self.state);
        if state.status.is_terminal() {
            return (state.stop_requested, true);
        }
        let already_requested = state.stop_requested;
        if !already_requested {
            state.stop_requested = true;
            state.status = ShellStatus::Stopping;
        }
        drop(state);
        if !already_requested {
            self.bump_change();
        }
        (already_requested, false)
    }

    fn finish(&self, wait_result: io::Result<ExitStatus>) {
        let completed_at_ms = now_ms();
        let mut state = lock(&self.state);
        match wait_result {
            Ok(status) => state.exit_code = status.code(),
            Err(err) => {
                if state.error.is_none() {
                    state.error = Some(format!("failed to wait for background shell: {err}"));
                }
            }
        }
        if self.task_cancel.is_cancelled() && !state.timed_out && !state.auto_stopped {
            state.stop_requested = true;
        }
        let monitor_exit_failed = self.registration.is_monitor() && state.exit_code != Some(0);
        if monitor_exit_failed && state.error.is_none() {
            state.error = Some(match state.exit_code {
                Some(code) => format!("monitor command exited with code {code}"),
                None => "monitor command exited without an exit code".to_string(),
            });
        }
        let status = if state.auto_stopped {
            ShellStatus::AutoStopped
        } else if state.timed_out {
            ShellStatus::TimedOut
        } else if state.stop_requested {
            ShellStatus::Stopped
        } else if state.error.is_some() {
            ShellStatus::Failed
        } else {
            ShellStatus::Exited
        };
        state.status = status;
        state.completed_at_ms = Some(completed_at_ms);
        self.process_tree.mark_finished();
        drop(state);

        if let Some(controller) = self.task_runtime_controller.as_ref() {
            match &self.registration {
                ShellTaskRegistration::BackgroundShell => {
                    let completion_status = match status {
                        ShellStatus::Exited => BackgroundShellCompletionStatus::Exited,
                        ShellStatus::TimedOut => BackgroundShellCompletionStatus::TimedOut,
                        ShellStatus::Stopped | ShellStatus::AutoStopped => {
                            BackgroundShellCompletionStatus::Stopped
                        }
                        ShellStatus::Failed => BackgroundShellCompletionStatus::Failed,
                        ShellStatus::Running | ShellStatus::Stopping => {
                            unreachable!("finished shell must have a terminal status")
                        }
                    };
                    let completion = {
                        let state = lock(&self.state);
                        let (output_tail, tail_cut) = response_tail(&state.tail);
                        BackgroundShellTaskCompletion {
                            shell_id: self.shell_id.clone(),
                            status: completion_status,
                            completed_at_ms,
                            exit_code: state.exit_code,
                            output_tail,
                            tail_truncated: tail_cut || state.tail_dropped,
                            output_path: self.output_path_text().unwrap_or_default(),
                            output_bytes: state.output_bytes,
                            error: state.error.clone(),
                            observed: self.observed.load(Ordering::Acquire),
                        }
                    };
                    controller.background_shell_finished(
                        self.owner
                            .session_id
                            .as_deref()
                            .expect("runtime-controlled shell has a session id"),
                        completion,
                    );
                }
                ShellTaskRegistration::Monitor { .. } => {
                    let completion_status = match status {
                        ShellStatus::Exited => MonitorTaskCompletionStatus::Exited,
                        ShellStatus::TimedOut => MonitorTaskCompletionStatus::TimedOut,
                        ShellStatus::Stopped => MonitorTaskCompletionStatus::Stopped,
                        ShellStatus::AutoStopped => MonitorTaskCompletionStatus::AutoStopped,
                        ShellStatus::Failed => MonitorTaskCompletionStatus::Failed,
                        ShellStatus::Running | ShellStatus::Stopping => {
                            unreachable!("finished monitor must have a terminal status")
                        }
                    };
                    // Monitor completions report stderr only: stdout already
                    // went out line by line as events.
                    let (stderr, exit_code, error) = {
                        let state = lock(&self.state);
                        (
                            state.stderr_tail.clone(),
                            state.exit_code,
                            state.error.clone(),
                        )
                    };
                    controller.monitor_finished(
                        self.owner
                            .session_id
                            .as_deref()
                            .expect("runtime-controlled monitor has a session id"),
                        MonitorTaskCompletion {
                            task_id: self.shell_id.clone(),
                            status: completion_status,
                            completed_at_ms,
                            exit_code,
                            stderr: (!stderr.is_empty()).then_some(stderr),
                            error,
                        },
                    );
                }
            }
        }
        self.bump_change();
    }

    fn request_stop(&self, caller: &str) -> ToolResult<Value> {
        let (already_requested, already_completed) = self.mark_stop_requested();

        if !already_completed {
            self.process_tree
                .terminate()
                .map_err(|err| execution_error(caller, err))?;
        }

        let state = lock(&self.state);
        Ok(json!({
            "shellId": self.shell_id,
            "command": truncate_command_field(&self.command),
            "status": state.status.as_str(),
            "stopRequested": state.stop_requested,
            "alreadyRequested": already_requested,
            "alreadyCompleted": already_completed,
            "exitCode": state.exit_code,
        }))
    }

    fn is_completed(&self) -> bool {
        lock(&self.state).status.is_terminal()
    }

    fn summary_value(&self) -> Value {
        let state = lock(&self.state);
        let mut value = json!({
            "shellId": self.shell_id,
            "tool": self.tool_name,
            "command": self.command,
            "status": state.status.as_str(),
            "startedAtMs": self.started_at_ms,
            "completedAtMs": state.completed_at_ms,
            "timeoutMs": self.timeout_ms,
            "exitCode": state.exit_code,
            "timedOut": state.timed_out,
            "stopRequested": state.stop_requested,
            "outputBytes": state.output_bytes,
        });
        if let Some(path) = self.output_path_text() {
            value["outputPath"] = Value::String(path);
        }
        value
    }

    /// The launch result: the summary plus where the output goes and how to
    /// get the result, said at the moment the model needs it.
    fn launch_value(&self) -> Value {
        let mut value = self.summary_value();
        if self.output_path.is_some() {
            let notified = self.task_runtime_controller.is_some() && self.owner.session_id.is_some();
            let note = if notified {
                "Running in the background. Its output streams to outputPath: read it with \
                 Read or search it with Grep. A task notification announces its exit, so keep \
                 working. To block until it exits instead, call ShellOutput with this shellId \
                 and a timeout."
            } else {
                "Running in the background. Its output streams to outputPath: read it with \
                 Read or search it with Grep. To block until it exits, call ShellOutput with \
                 this shellId and a timeout."
            };
            value["note"] = Value::String(note.to_string());
        }
        value
    }

    /// Progress payload while a wait is in flight.
    fn waiting_value(&self, wait_timeout_ms: u64) -> Value {
        let mut value = json!({
            "shellId": self.shell_id,
            "command": truncate_command_field(&self.command),
            "status": lock(&self.state).status.as_str(),
            "waiting": true,
            "waitTimeoutMs": wait_timeout_ms,
        });
        if let Some(path) = self.output_path_text() {
            value["outputPath"] = Value::String(path);
        }
        value
    }

    /// The ShellOutput result for one shell, and whether it has finished.
    /// Flags are emitted only when set, so a plain result stays short.
    fn result_value(&self, wait_timed_out: bool) -> (Value, bool) {
        let state = lock(&self.state);
        let completed = state.status.is_terminal();
        let mut value = json!({
            "shellId": self.shell_id,
            "command": truncate_command_field(&self.command),
            "status": state.status.as_str(),
            "completed": completed,
            "outputBytes": state.output_bytes,
        });
        let object = value.as_object_mut().expect("result_value builds an object");
        if let Some(exit_code) = state.exit_code {
            object.insert("exitCode".into(), json!(exit_code));
        }
        if let Some(error) = &state.error {
            object.insert("error".into(), Value::String(error.clone()));
        }
        for (key, flag) in [
            ("timedOut", state.timed_out),
            ("stopRequested", state.stop_requested),
            ("waitTimedOut", wait_timed_out),
        ] {
            if flag {
                object.insert(key.into(), Value::Bool(true));
            }
        }
        if let Some(path) = self.output_path_text() {
            object.insert("outputPath".into(), Value::String(path));
        }
        let (tail, cut) = response_tail(&state.tail);
        if !tail.is_empty() {
            object.insert("outputTail".into(), Value::String(tail));
        }
        if cut || state.tail_dropped {
            object.insert("tailTruncated".into(), Value::Bool(true));
        }
        if state.file_truncated {
            object.insert("outputFileTruncated".into(), Value::Bool(true));
        }
        if let Some(error) = &state.file_error {
            object.insert("outputFileError".into(), Value::String(error.clone()));
        }
        (value, completed)
    }

    fn bump_change(&self) {
        self.changes.send_modify(|version| {
            *version = version.wrapping_add(1);
        });
    }
}

/// Chars of the originating command echoed back in `ShellOutput`/`ShellStop`
/// responses. The command is context for the reader (headers show it instead
/// of the opaque shellId), not a data channel — cap it to what the header
/// actually displays so it does not inflate every poll.
const COMMAND_FIELD_MAX_CHARS: usize = 80;

fn truncate_command_field(command: &str) -> String {
    let mut chars = command.chars();
    let truncated: String = chars.by_ref().take(COMMAND_FIELD_MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{truncated}\u{2026}")
    } else {
        truncated
    }
}

/// Append `text`, then drop whole characters from the front until the
/// buffer fits `max_bytes`. Returns whether anything was dropped.
fn push_bounded(buffer: &mut String, text: &str, max_bytes: usize) -> bool {
    buffer.push_str(text);
    if buffer.len() <= max_bytes {
        return false;
    }
    let mut start = buffer.len() - max_bytes;
    while !buffer.is_char_boundary(start) {
        start += 1;
    }
    buffer.drain(..start);
    true
}

/// The end of `tail` a response carries: at most [`TAIL_RESPONSE_LINES`]
/// lines and [`TAIL_RESPONSE_BYTES`] bytes, starting on a line boundary when
/// it had to cut. Returns the text and whether it cut anything.
fn response_tail(tail: &str) -> (String, bool) {
    let mut start = tail.len().saturating_sub(TAIL_RESPONSE_BYTES);
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    let mut cut = start > 0;
    let mut slice = &tail[start..];
    if cut {
        // Drop the partial first line, unless it is the only line there is.
        if let Some(newline) = slice.find('\n') {
            if newline + 1 < slice.len() {
                slice = &slice[newline + 1..];
            }
        }
    }
    let body = slice.strip_suffix('\n').unwrap_or(slice);
    if let Some((newline, _)) = body.rmatch_indices('\n').nth(TAIL_RESPONSE_LINES - 1) {
        slice = &slice[newline + 1..];
        cut = true;
    }
    (slice.to_string(), cut)
}

/// Where background shells write their output: the session scratchpad when
/// there is one, so Read needs no prompt and sub-agents can read it too.
fn output_dir(context: &ToolContext) -> PathBuf {
    match context.scratchpad_dir() {
        Some(dir) => dir.join("shells"),
        None => std::env::temp_dir().join("rebon").join("shells"),
    }
}

fn create_output_file(context: &ToolContext, shell_id: &str) -> anyhow::Result<(PathBuf, File)> {
    let dir = output_dir(context);
    std::fs::create_dir_all(&dir).map_err(|err| {
        anyhow::anyhow!(
            "could not create the background output directory `{}`: {err}",
            dir.display()
        )
    })?;
    let path = dir.join(format!("{shell_id}.output"));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|err| {
            anyhow::anyhow!(
                "could not create the background output file `{}`: {err}",
                path.display()
            )
        })?;
    Ok((path, file))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellStatus {
    Running,
    Stopping,
    Exited,
    TimedOut,
    Stopped,
    AutoStopped,
    Failed,
}

impl ShellStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Exited => "exited",
            Self::TimedOut => "timed_out",
            Self::Stopped => "stopped",
            Self::AutoStopped => "auto_stopped",
            Self::Failed => "failed",
        }
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Exited | Self::TimedOut | Self::Stopped | Self::AutoStopped | Self::Failed
        )
    }
}

struct ShellState {
    status: ShellStatus,
    /// The newest combined output, bounded by [`TAIL_BUFFER_BYTES`].
    tail: String,
    /// True once `tail` has dropped output from its front.
    tail_dropped: bool,
    /// A Monitor's stderr, which its completion reports.
    stderr_tail: String,
    output_bytes: u64,
    /// The output file, open until a write fails or the size cap is hit.
    file: Option<File>,
    file_bytes: u64,
    file_truncated: bool,
    file_error: Option<String>,
    exit_code: Option<i32>,
    timed_out: bool,
    stop_requested: bool,
    auto_stopped: bool,
    error: Option<String>,
    completed_at_ms: Option<u64>,
}

impl Default for ShellState {
    fn default() -> Self {
        Self {
            status: ShellStatus::Running,
            tail: String::new(),
            tail_dropped: false,
            stderr_tail: String::new(),
            output_bytes: 0,
            file: None,
            file_bytes: 0,
            file_truncated: false,
            file_error: None,
            exit_code: None,
            timed_out: false,
            stop_requested: false,
            auto_stopped: false,
            error: None,
            completed_at_ms: None,
        }
    }
}

impl ShellState {
    /// Append to the output file. A failed write or the size cap closes the
    /// file for good; the shell itself carries on either way.
    fn write_to_file(&mut self, text: &str) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let room = OUTPUT_FILE_MAX_BYTES.saturating_sub(self.file_bytes);
        let (chunk, hit_cap) = if text.len() as u64 > room {
            let mut end = usize::try_from(room).unwrap_or(usize::MAX).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            (&text[..end], true)
        } else {
            (text, false)
        };
        let written = file.write_all(chunk.as_bytes()).and_then(|()| {
            if hit_cap {
                file.write_all(OUTPUT_FILE_LIMIT_NOTE.as_bytes())
            } else {
                Ok(())
            }
        });
        match written {
            Ok(()) => {
                self.file_bytes = self.file_bytes.saturating_add(chunk.len() as u64);
                if hit_cap {
                    self.file_truncated = true;
                    self.file = None;
                }
            }
            Err(err) => {
                self.file_error = Some(format!("could not write the output file: {err}"));
                self.file = None;
            }
        }
    }
}

async fn read_stream<R>(
    mut reader: R,
    entry: Arc<ShellEntry>,
    stream: &'static str,
    output_encoding: ShellOutputEncoding,
) where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0u8; READ_CHUNK_BYTES];
    let mut decoder = ShellStreamDecoder::new(output_encoding);
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                let text = decoder.decode_chunk(&buffer[..read]);
                entry.push_output(stream, text);
            }
            Err(err) => {
                entry.record_error(format!("failed to read background {stream}: {err}"));
                break;
            }
        }
    }
    entry.push_output(stream, decoder.finish());
    if stream == "stdout" {
        entry.flush_monitor_stdout();
    }
}

struct ShellStreamDecoder {
    output_encoding: ShellOutputEncoding,
    carry: Vec<u8>,
}

impl ShellStreamDecoder {
    fn new(output_encoding: ShellOutputEncoding) -> Self {
        Self {
            output_encoding,
            carry: Vec::new(),
        }
    }

    fn decode_chunk(&mut self, chunk: &[u8]) -> String {
        match self.output_encoding {
            ShellOutputEncoding::Utf8 => decode_stream_chunk(&mut self.carry, chunk),
            #[cfg(windows)]
            ShellOutputEncoding::WindowsCodePage(CP_UTF8) => {
                decode_stream_chunk(&mut self.carry, chunk)
            }
            #[cfg(windows)]
            ShellOutputEncoding::WindowsCodePage(code_page) => {
                decode_windows_code_page_chunk(code_page, &mut self.carry, chunk)
            }
        }
    }

    fn finish(&mut self) -> String {
        self.output_encoding
            .decode_complete(&std::mem::take(&mut self.carry))
    }
}

#[cfg(windows)]
fn decode_windows_code_page_chunk(code_page: u32, carry: &mut Vec<u8>, chunk: &[u8]) -> String {
    let mut combined = std::mem::take(carry);
    combined.extend_from_slice(chunk);
    let complete_len = windows_code_page_complete_prefix_len(code_page, &combined);
    carry.extend_from_slice(&combined[complete_len..]);
    decode_windows_code_page(code_page, &combined[..complete_len])
}

#[cfg(windows)]
fn windows_code_page_complete_prefix_len(code_page: u32, bytes: &[u8]) -> usize {
    let mut index = 0;
    while index < bytes.len() {
        if unsafe { IsDBCSLeadByteEx(code_page, bytes[index]) } != 0 {
            if index + 1 == bytes.len() {
                return index;
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    bytes.len()
}

#[cfg(windows)]
fn decode_windows_code_page(code_page: u32, bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let Ok(input_len) = i32::try_from(bytes.len()) else {
        return String::from_utf8_lossy(bytes).into_owned();
    };
    let wide_len = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            input_len,
            std::ptr::null_mut(),
            0,
        )
    };
    if wide_len <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut wide = vec![0u16; wide_len as usize];
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            input_len,
            wide.as_mut_ptr(),
            wide_len,
        )
    };
    if written <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    String::from_utf16_lossy(&wide[..written as usize])
}

/// Decode a raw read chunk to UTF-8 text, carrying an incomplete trailing
/// multi-byte sequence across the read boundary.
///
/// `carry` holds the bytes of a UTF-8 sequence a previous read split
/// mid-character; they are prepended to `chunk`. Any valid prefix is returned
/// decoded, a fresh incomplete tail (if present) is left in `carry` for the
/// next call, and genuinely invalid bytes are replaced with U+FFFD exactly as
/// `String::from_utf8_lossy` would — one replacement per invalid sequence.
fn decode_stream_chunk(carry: &mut Vec<u8>, chunk: &[u8]) -> String {
    let bytes = std::mem::take(carry);
    let combined = if bytes.is_empty() {
        chunk.to_vec()
    } else {
        let mut combined = bytes;
        combined.extend_from_slice(chunk);
        combined
    };

    let mut text = String::new();
    let mut rest: &[u8] = &combined;
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                text.push_str(valid);
                break;
            }
            Err(err) => {
                let valid_up_to = err.valid_up_to();
                // SAFETY: `rest[..valid_up_to]` is a valid UTF-8 prefix by the
                // contract of `Utf8Error::valid_up_to`.
                text.push_str(unsafe { std::str::from_utf8_unchecked(&rest[..valid_up_to]) });
                match err.error_len() {
                    // No error length ⇒ the trailing bytes are an incomplete but
                    // potentially valid sequence: carry them forward.
                    None => {
                        carry.extend_from_slice(&rest[valid_up_to..]);
                        break;
                    }
                    // A concrete error length ⇒ genuinely invalid bytes: emit one
                    // replacement char and continue decoding past them.
                    Some(invalid_len) => {
                        text.push('\u{FFFD}');
                        rest = &rest[valid_up_to + invalid_len..];
                    }
                }
            }
        }
    }
    text
}

async fn supervise_process(
    mut child: Child,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    entry: Arc<ShellEntry>,
    timeout_ms: Option<u64>,
) {
    let wait_result = if let Some(timeout_ms) = timeout_ms {
        tokio::select! {
            result = child.wait() => result,
            _ = sleep(Duration::from_millis(timeout_ms)) => {
                if entry.mark_timed_out() {
                    if let Err(err) = entry.process_tree.terminate() {
                        entry.record_error(format!("failed to terminate timed-out process tree: {err}"));
                        let _ = child.start_kill();
                    }
                }
                child.wait().await
            }
            _ = entry.task_cancel.notified() => {
                let (_, already_completed) = entry.mark_stop_requested();
                if !already_completed {
                    if let Err(err) = entry.process_tree.terminate() {
                        entry.record_error(format!("failed to terminate cancelled process tree: {err}"));
                        let _ = child.start_kill();
                    }
                }
                child.wait().await
            }
        }
    } else {
        tokio::select! {
            result = child.wait() => result,
            _ = entry.task_cancel.notified() => {
                let (_, already_completed) = entry.mark_stop_requested();
                if !already_completed {
                    if let Err(err) = entry.process_tree.terminate() {
                        entry.record_error(format!("failed to terminate cancelled process tree: {err}"));
                        let _ = child.start_kill();
                    }
                }
                child.wait().await
            }
        }
    };

    if let Err(err) = entry.process_tree.terminate() {
        entry.record_error(format!("failed to clean up process tree: {err}"));
    }
    let drain_deadline = Instant::now() + READER_DRAIN_GRACE;
    drain_reader(stdout_task, drain_deadline, &entry, "stdout").await;
    drain_reader(stderr_task, drain_deadline, &entry, "stderr").await;
    entry.finish(wait_result);
}

/// Let a reader finish what the pipe still holds, but no longer than
/// `deadline`: whatever keeps the pipe open past it is not the shell's.
/// Cutting it off is not an error, so a command that exited cleanly still
/// reads as exited.
async fn drain_reader(
    mut reader: JoinHandle<()>,
    deadline: Instant,
    entry: &ShellEntry,
    stream: &str,
) {
    match tokio::time::timeout_at(deadline, &mut reader).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => entry.record_error(format!("background {stream} reader failed: {err}")),
        Err(_) => {
            tracing::debug!(
                shell_id = %entry.shell_id,
                stream,
                "output cut off: pipe held open outside the process group"
            );
            reader.abort();
        }
    }
}

fn running_count(entries: &HashMap<String, Arc<ShellEntry>>, owner: &ShellOwner) -> usize {
    entries
        .values()
        .filter(|entry| entry.owner == *owner && !entry.is_completed())
        .count()
}

fn prune_completed(entries: &mut HashMap<String, Arc<ShellEntry>>, owner: &ShellOwner) {
    let mut completed: Vec<(String, u64)> = entries
        .iter()
        .filter_map(|(id, entry)| {
            if entry.owner != *owner {
                return None;
            }
            let state = lock(&entry.state);
            state
                .completed_at_ms
                .map(|completed_at_ms| (id.clone(), completed_at_ms))
        })
        .collect();
    if completed.len() < MAX_COMPLETED_SHELLS_PER_OWNER {
        return;
    }
    completed.sort_by_key(|(_, completed_at_ms)| *completed_at_ms);
    let remove_count = completed.len() + 1 - MAX_COMPLETED_SHELLS_PER_OWNER;
    for (id, _) in completed.into_iter().take(remove_count) {
        entries.remove(&id);
    }
}

fn random_shell_id() -> io::Result<String> {
    random_runtime_id("sh_")
}

pub(crate) fn random_monitor_id() -> io::Result<String> {
    random_runtime_id("m_")
}

fn random_runtime_id(prefix: &str) -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|err| io::Error::other(err.to_string()))?;
    let mut id = String::with_capacity(prefix.len() + 32);
    id.push_str(prefix);
    for byte in bytes {
        let _ = write!(id, "{byte:02x}");
    }
    Ok(id)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn execution_error(caller: &str, source: impl Into<anyhow::Error>) -> ToolError {
    ToolError::Execution {
        tool: ToolId::new(caller),
        source: source.into(),
    }
}

fn unknown_shell_error(caller: &str, shell_id: &str) -> ToolError {
    ToolError::InvalidInput {
        tool: ToolId::new(caller),
        reason: format!("unknown shellId `{shell_id}`"),
        error_code: Some(UNKNOWN_SHELL_CODE),
    }
}

/// Reading a Monitor through ShellOutput would duplicate its task
/// notifications, and `wait=true` would park the turn on a stream that
/// is already pushed, so the refusal says where the events go instead.
fn monitor_output_error(caller: &str, shell_id: &str) -> ToolError {
    ToolError::InvalidInput {
        tool: ToolId::new(caller),
        reason: format!(
            "`{shell_id}` is a Monitor: each event already arrives as a task notification \
             and its exit is announced the same way, so do not poll it. Keep working and \
             react to the notifications; stop it with TaskStop."
        ),
        error_code: Some(INVALID_INPUT_CODE),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct ProcessTreeControl {
    active: AtomicBool,
    termination_requested: AtomicBool,
    /// The tree this controls. `None` is a control over no tree at all,
    /// which only a test builds, for a shell that has already finished.
    platform: Option<Mutex<ProcessTreeGuard>>,
}

impl ProcessTreeControl {
    fn for_child(child: &Child) -> io::Result<Self> {
        Ok(Self {
            active: AtomicBool::new(true),
            termination_requested: AtomicBool::new(false),
            platform: Some(Mutex::new(guard_process_tree(child)?)),
        })
    }

    #[cfg(test)]
    fn finished_without_a_tree() -> Self {
        Self {
            active: AtomicBool::new(false),
            termination_requested: AtomicBool::new(false),
            platform: None,
        }
    }

    fn terminate(&self) -> io::Result<()> {
        if !self.active.load(Ordering::Acquire) {
            return Ok(());
        }
        if self
            .termination_requested
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        if let Some(platform) = &self.platform {
            if let Err(err) = lock(platform).terminate() {
                self.termination_requested.store(false, Ordering::Release);
                return Err(err);
            }
        }
        Ok(())
    }

    fn mark_finished(&self) {
        self.active.store(false, Ordering::Release);
    }
}

impl Drop for ProcessTreeControl {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
fn configure_process_group(_command: &mut Command) {}

/// Guard a spawned shell's whole process tree.
///
/// The guard itself lives in `rebon-tools-core`; what stays here is how
/// this caller reaches the child's raw identifier, and what it calls a
/// child that has none. No breakaway: nothing a shell command starts is
/// allowed to opt out of being reaped with it.
#[cfg(windows)]
fn guard_process_tree(child: &Child) -> io::Result<ProcessTreeGuard> {
    let handle = child
        .raw_handle()
        .ok_or_else(|| io::Error::other("child process has no process handle"))?;
    ProcessTreeGuard::for_raw_handle(handle, false)
}

#[cfg(unix)]
fn guard_process_tree(child: &Child) -> io::Result<ProcessTreeGuard> {
    let process_id = child
        .id()
        .ok_or_else(|| io::Error::other("child process has no pid"))?;
    ProcessTreeGuard::for_process_group(process_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// [`ShellLineReader`] is only ever read inside a `tokio::select!`, so the
    /// property worth pinning is what a *cancelled* read does. These drive the
    /// cancellation directly — `timeout(ZERO, ..)` polls the read once and then
    /// drops it — rather than racing two streams, which would only reproduce
    /// the loss under load.
    mod line_reader {
        use super::*;
        use std::collections::VecDeque;
        use std::pin::Pin;
        use std::task::{Context as TaskContext, Poll};
        use tokio::io::ReadBuf;

        enum Step {
            Chunk(&'static [u8]),
            /// A poll that finds nothing yet — the moment a `select!` uses to
            /// hand the turn to the other stream.
            WouldBlock,
        }

        struct ScriptedStream {
            steps: VecDeque<Step>,
        }

        impl ScriptedStream {
            fn new(steps: Vec<Step>) -> Self {
                Self {
                    steps: steps.into(),
                }
            }
        }

        impl AsyncRead for ScriptedStream {
            fn poll_read(
                mut self: Pin<&mut Self>,
                cx: &mut TaskContext<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                match self.steps.pop_front() {
                    Some(Step::Chunk(bytes)) => {
                        buf.put_slice(bytes);
                        Poll::Ready(Ok(()))
                    }
                    Some(Step::WouldBlock) => {
                        // Deliberately never wakes: waking here would let the
                        // read finish before the timeout fires and there would
                        // be no cancellation to test. The timeout's own timer
                        // is what drives the task forward, and the next
                        // `next_line` polls the stream again from scratch.
                        let _ = cx;
                        Poll::Pending
                    }
                    // Out of script: end of stream.
                    None => Poll::Ready(Ok(())),
                }
            }
        }

        fn reader(
            steps: Vec<Step>,
            encoding: ShellOutputEncoding,
        ) -> ShellLineReader<ScriptedStream> {
            ShellLineReader::new(ScriptedStream::new(steps), encoding)
        }

        /// Poll one read exactly once and drop it — the tools' `select!` shape
        /// with the other branch rigged to win.
        async fn cancel_one_read<R: AsyncRead + Unpin>(reader: &mut ShellLineReader<R>) {
            tokio::select! {
                biased;
                line = reader.next_line() => {
                    panic!("the scripted stream must leave the read unfinished, got {line:?}")
                }
                () = std::future::ready(()) => {}
            }
        }

        /// The regression: PowerShell flushes a line's text and its newline as
        /// two writes, so a foreground command that also writes to stderr can
        /// have its stdout read cancelled mid-line. Before the reader owned the
        /// buffer those bytes went with the dropped future and the line came
        /// back empty.
        #[tokio::test]
        async fn a_cancelled_read_resumes_the_same_line() {
            let _env = crate::test_env::hold_env();
            let mut reader = reader(
                vec![
                    Step::Chunk("成功".as_bytes()),
                    Step::WouldBlock,
                    Step::Chunk(b"\r\n"),
                ],
                ShellOutputEncoding::Utf8,
            );

            cancel_one_read(&mut reader).await;

            assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("成功"));
        }

        /// Decoding happens on the assembled line, so a cancellation that lands
        /// inside a multi-byte character is not a decode error either.
        #[tokio::test]
        async fn a_cancelled_read_may_split_a_character() {
            let _env = crate::test_env::hold_env();
            let mut reader = reader(
                vec![
                    Step::Chunk(&[0xE6]),
                    Step::WouldBlock,
                    Step::Chunk(&[0x88, 0x90, b'\n']),
                ],
                ShellOutputEncoding::Utf8,
            );

            cancel_one_read(&mut reader).await;

            assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("成"));
        }

        /// A last line without a trailing newline is still a line, and the
        /// stream ends exactly once afterwards.
        #[tokio::test]
        async fn end_of_stream_yields_the_unterminated_line_then_none() {
            let _env = crate::test_env::hold_env();
            let mut reader = reader(
                vec![Step::Chunk(b"first\nsecond")],
                ShellOutputEncoding::Utf8,
            );

            assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("first"));
            assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("second"));
            assert_eq!(reader.next_line().await.unwrap(), None);
        }
    }

    fn context(session_id: &str) -> ToolContext {
        ToolContext::new().with_session_id(session_id)
    }

    #[test]
    fn response_tail_keeps_the_last_lines_on_a_line_boundary() {
        // Short output comes back whole.
        assert_eq!(response_tail("a\nb\n"), ("a\nb\n".to_string(), false));
        assert_eq!(response_tail(""), (String::new(), false));

        // Too many lines: the newest TAIL_RESPONSE_LINES survive.
        let many: String = (0..TAIL_RESPONSE_LINES + 20)
            .map(|i| format!("line {i}\n"))
            .collect();
        let (tail, cut) = response_tail(&many);
        assert!(cut);
        assert_eq!(tail.lines().count(), TAIL_RESPONSE_LINES);
        assert!(tail.starts_with("line 20\n"), "{tail:?}");
        assert!(tail.ends_with(&format!("line {}\n", TAIL_RESPONSE_LINES + 19)));

        // Too many bytes: cut, and start on a whole line.
        let wide: String = (0..40).map(|i| format!("{i:04}{}\n", "x".repeat(1000))).collect();
        let (tail, cut) = response_tail(&wide);
        assert!(cut);
        assert!(tail.len() <= TAIL_RESPONSE_BYTES);
        assert!(tail.ends_with("x\n"));
        assert!(tail.as_bytes()[0].is_ascii_digit(), "starts mid-line: {tail:?}");

        // One enormous line is still shown rather than dropped, cut on a
        // character boundary.
        let huge = "中".repeat(TAIL_RESPONSE_BYTES);
        let (tail, cut) = response_tail(&huge);
        assert!(cut);
        assert!(!tail.is_empty() && tail.len() <= TAIL_RESPONSE_BYTES);
    }

    #[test]
    fn push_bounded_drops_whole_characters_from_the_front() {
        let mut buffer = String::new();
        assert!(!push_bounded(&mut buffer, "abc", 4));
        assert!(push_bounded(&mut buffer, "中", 4));
        assert_eq!(buffer, "c中");
        // Cutting one byte would split "中"; the whole character goes.
        assert!(push_bounded(&mut buffer, "de", 4));
        assert_eq!(buffer, "de");
    }

    #[test]
    fn output_file_stops_at_its_cap_with_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sh_cap.output");
        let mut state = ShellState {
            file: Some(File::create(&path).unwrap()),
            file_bytes: OUTPUT_FILE_MAX_BYTES - 3,
            ..ShellState::default()
        };
        state.write_to_file("abcdef");
        state.write_to_file("never written");
        assert!(state.file_truncated);
        assert!(state.file.is_none());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("abc{OUTPUT_FILE_LIMIT_NOTE}")
        );
    }

    #[derive(Default)]
    struct RecordingTaskController {
        started: Mutex<Vec<BackgroundShellTaskSpec>>,
        finished: Mutex<Vec<BackgroundShellTaskCompletion>>,
        observed: Mutex<Vec<String>>,
        cancels: Mutex<Vec<PromptCancel>>,
        monitor_started: Mutex<Vec<MonitorTaskSpec>>,
        monitor_events: Mutex<Vec<(String, String)>>,
        monitor_finished: Mutex<Vec<MonitorTaskCompletion>>,
        monitor_disposition: Mutex<Option<MonitorEventDisposition>>,
    }

    #[async_trait::async_trait]
    impl TaskRuntimeController for RecordingTaskController {
        async fn stop_task(
            &self,
            _session_id: &str,
            _task_id: &str,
        ) -> Result<crate::StopTaskOutcome, String> {
            Ok(crate::StopTaskOutcome::NotFound)
        }

        async fn send_message_to_task(
            &self,
            _session_id: &str,
            _task_id: &str,
            _message: String,
        ) -> Result<(), rebon_tools_core::ToolErrorPresentation> {
            Ok(())
        }

        fn background_shell_started(
            &self,
            _session_id: &str,
            spec: BackgroundShellTaskSpec,
            cancel: PromptCancel,
        ) {
            lock(&self.started).push(spec);
            lock(&self.cancels).push(cancel);
        }

        fn background_shell_finished(
            &self,
            _session_id: &str,
            completion: BackgroundShellTaskCompletion,
        ) {
            lock(&self.finished).push(completion);
        }

        fn background_shell_observed(&self, _session_id: &str, shell_id: &str) {
            lock(&self.observed).push(shell_id.to_string());
        }

        fn monitor_started(&self, _session_id: &str, spec: MonitorTaskSpec, cancel: PromptCancel) {
            lock(&self.monitor_started).push(spec);
            lock(&self.cancels).push(cancel);
        }

        fn monitor_event(
            &self,
            _session_id: &str,
            task_id: &str,
            event: String,
        ) -> MonitorEventDisposition {
            lock(&self.monitor_events).push((task_id.to_string(), event));
            (*lock(&self.monitor_disposition)).unwrap_or(MonitorEventDisposition::Queued)
        }

        fn monitor_finished(&self, _session_id: &str, completion: MonitorTaskCompletion) {
            lock(&self.monitor_finished).push(completion);
        }
    }

    fn test_command(script: &str) -> Command {
        #[cfg(windows)]
        {
            let mut command = Command::new("powershell.exe");
            command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            command
        }
    }

    fn configured_command(script: &str) -> Command {
        let mut command = test_command(script);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    fn two_lines_script() -> &'static str {
        #[cfg(windows)]
        {
            "Write-Output first; Write-Output second"
        }
        #[cfg(not(windows))]
        {
            "printf 'first\\nsecond\\n'"
        }
    }

    fn one_line_script() -> &'static str {
        #[cfg(windows)]
        {
            "Write-Output hello"
        }
        #[cfg(not(windows))]
        {
            "printf 'hello\\n'"
        }
    }

    fn stderr_script() -> &'static str {
        #[cfg(windows)]
        {
            "[Console]::Error.WriteLine('oops')"
        }
        #[cfg(not(windows))]
        {
            "printf 'oops\\n' 1>&2"
        }
    }

    fn monitor_lines_script() -> &'static str {
        #[cfg(windows)]
        {
            "[Console]::Out.Write(\"first`n`nsecond`npartial\"); [Console]::Error.WriteLine('ignored')"
        }
        #[cfg(not(windows))]
        {
            "printf 'first\\n\\nsecond\\npartial'; printf 'ignored\\n' 1>&2"
        }
    }

    fn long_running_script() -> &'static str {
        #[cfg(windows)]
        {
            "Start-Sleep -Seconds 30"
        }
        #[cfg(not(windows))]
        {
            "sleep 30"
        }
    }

    fn ticking_script() -> &'static str {
        #[cfg(windows)]
        {
            "foreach ($i in 1..100) { Write-Output \"tick $i\"; Start-Sleep -Milliseconds 100 }"
        }
        #[cfg(not(windows))]
        {
            "i=0; while [ $i -lt 100 ]; do i=$((i+1)); printf 'tick %s\\n' \"$i\"; sleep 0.1; done"
        }
    }

    fn long_running_script_with_initial_line() -> &'static str {
        #[cfg(windows)]
        {
            "Write-Output event; Start-Sleep -Seconds 30"
        }
        #[cfg(not(windows))]
        {
            "printf 'event\\n'; sleep 30"
        }
    }

    fn nested_process_script() -> &'static str {
        #[cfg(windows)]
        {
            "$child = Start-Process -FilePath powershell.exe -ArgumentList @('-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 30') -PassThru; Write-Output ('CHILD_PID=' + $child.Id); Wait-Process -Id $child.Id"
        }
        #[cfg(not(windows))]
        {
            "sleep 30 & child=$!; printf 'CHILD_PID=%s\\n' \"$child\"; wait \"$child\""
        }
    }

    #[cfg(windows)]
    fn process_is_alive(process_id: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE,
        };
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                process_id,
            )
        };
        if handle == 0 {
            return false;
        }
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        unsafe {
            CloseHandle(handle);
        }
        wait == WAIT_TIMEOUT
    }

    #[cfg(unix)]
    fn process_is_alive(process_id: u32) -> bool {
        let result = unsafe { libc::kill(process_id as i32, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// Read the nested process id out of the shell's output file, the way
    /// the model reads a background shell now.
    async fn wait_for_nested_process_id(registry: &ShellProcessRegistry, shell_id: &str) -> u32 {
        let started = Instant::now();
        loop {
            let status = registry
                .wait(&context("session-a"), "ShellOutput", shell_id, false, 0)
                .await
                .unwrap();
            let path = status["outputPath"].as_str().expect("outputPath").to_owned();
            let output = std::fs::read_to_string(&path).unwrap_or_default();
            if let Some(rest) = output.split("CHILD_PID=").nth(1) {
                if let Some(raw_pid) = rest
                    .split(|character: char| !character.is_ascii_digit())
                    .next()
                    .filter(|value| !value.is_empty())
                {
                    return raw_pid.parse().unwrap();
                }
            }
            assert_ne!(
                status["completed"], true,
                "nested process id was not emitted"
            );
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "no CHILD_PID line in {path}"
            );
            sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_for_completion(registry: &ShellProcessRegistry, shell_id: &str) -> Value {
        loop {
            let update = registry
                .wait(&context("session-a"), "ShellOutput", shell_id, true, 5_000)
                .await
                .unwrap();
            if update["completed"] == true {
                return update;
            }
        }
    }

    async fn assert_process_exits(process_id: u32) {
        for _ in 0..100 {
            if !process_is_alive(process_id) {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("process {process_id} is still alive");
    }

    async fn wait_for_monitor_completion(
        controller: &RecordingTaskController,
    ) -> MonitorTaskCompletion {
        for _ in 0..250 {
            if let Some(completion) = lock(&controller.monitor_finished).first().cloned() {
                return completion;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("monitor did not finish");
    }

    #[test]
    fn split_utf8_sequence_reassembles_without_replacement() {
        // "中" is 3 bytes (E4 B8 AD); split it after the first byte so the
        // tail arrives in a second read.
        let glyph = "中".as_bytes();
        assert_eq!(glyph.len(), 3);
        let mut carry = Vec::new();

        let first = decode_stream_chunk(&mut carry, &glyph[..1]);
        assert!(first.is_empty(), "no complete char yet: {first:?}");
        assert!(!carry.is_empty(), "incomplete tail must be carried");

        let second = decode_stream_chunk(&mut carry, &glyph[1..]);
        assert_eq!(second, "中");
        assert!(carry.is_empty(), "tail should be flushed");

        let reassembled = format!("{first}{second}");
        assert!(
            !reassembled.contains('\u{FFFD}'),
            "reassembled text must not contain U+FFFD: {reassembled:?}"
        );
    }

    #[test]
    fn decode_stream_chunk_preserves_text_around_split_and_flags_real_garbage() {
        // A split in the middle of a 3-byte glyph, with valid text on both
        // sides, still round-trips.
        let mut carry = Vec::new();
        let mut input = b"ab".to_vec();
        input.extend_from_slice(&"好".as_bytes()[..2]);
        let head = decode_stream_chunk(&mut carry, &input);
        assert_eq!(head, "ab");
        let mut tail = "好".as_bytes()[2..].to_vec();
        tail.extend_from_slice(b"cd");
        let rest = decode_stream_chunk(&mut carry, &tail);
        assert_eq!(rest, "好cd");
        assert!(carry.is_empty());

        // A genuinely invalid byte (0xFF is never valid UTF-8) is replaced,
        // matching from_utf8_lossy, and does not stall the stream.
        let mut carry = Vec::new();
        let decoded = decode_stream_chunk(&mut carry, &[b'x', 0xFF, b'y']);
        assert_eq!(decoded, "x\u{FFFD}y");
        assert!(carry.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_code_page_decoder_reassembles_split_dbcs_sequence() {
        let mut decoder = ShellStreamDecoder::new(ShellOutputEncoding::WindowsCodePage(936));

        assert_eq!(decoder.decode_chunk(&[0xB3]), "");
        assert_eq!(decoder.decode_chunk(&[0xC9, 0xB9, 0xA6]), "成功");
        assert_eq!(decoder.finish(), "");
    }

    #[test]
    fn random_ids_have_expected_shape() {
        let id = random_shell_id().unwrap();
        assert_eq!(id.len(), 35);
        assert!(id.starts_with("sh_"));
        assert!(id[3..].bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn background_shell_reports_task_lifecycle_and_terminal_observation() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let controller = Arc::new(RecordingTaskController::default());
        let context = context("session-a").with_task_runtime_controller(controller.clone());
        let started = registry
            .spawn(
                &context,
                configured_command(one_line_script()),
                "Bash",
                one_line_script().into(),
                None,
            )
            .await
            .unwrap();
        let shell_id = started["shellId"].as_str().unwrap().to_string();
        // With a runtime to deliver it, the launch note promises a
        // notification.
        assert!(started["note"]
            .as_str()
            .unwrap()
            .contains("task notification"));

        let completion = wait_for_completion(&registry, &shell_id).await;
        assert_eq!(completion["status"], "exited");

        let started = lock(&controller.started);
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].shell_id, shell_id);
        assert_eq!(started[0].session_id.as_deref(), Some("session-a"));
        drop(started);

        let finished = lock(&controller.finished);
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].shell_id, shell_id);
        assert_eq!(finished[0].status, BackgroundShellCompletionStatus::Exited);
        assert!(finished[0].output_tail.contains("hello"));
        assert!(!finished[0].tail_truncated);
        assert_eq!(finished[0].output_path, completion["outputPath"]);
        assert!(finished[0].output_bytes > 0);
        assert!(!finished[0].observed);
        drop(finished);

        assert_eq!(lock(&controller.observed).as_slice(), &[shell_id]);
    }

    #[tokio::test]
    async fn task_cancel_stops_background_shell_consistently() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let controller = Arc::new(RecordingTaskController::default());
        let context = context("session-a").with_task_runtime_controller(controller.clone());
        let started = registry
            .spawn(
                &context,
                configured_command(long_running_script()),
                "Bash",
                long_running_script().into(),
                None,
            )
            .await
            .unwrap();
        let shell_id = started["shellId"].as_str().unwrap().to_string();
        lock(&controller.cancels)[0].cancel();

        let completion = wait_for_completion(&registry, &shell_id).await;
        assert_eq!(completion["status"], "stopped");
        assert_eq!(completion["stopRequested"], true);
        let finished = lock(&controller.finished);
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].status, BackgroundShellCompletionStatus::Stopped);
    }

    #[tokio::test]
    async fn spawn_streams_output_to_a_file_and_a_wait_returns_its_tail() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let launched = registry
            .spawn(
                &context("session-a"),
                configured_command(two_lines_script()),
                "PowerShell",
                "test".into(),
                None,
            )
            .await
            .unwrap();
        let shell_id = launched["shellId"].as_str().unwrap();
        let path = PathBuf::from(launched["outputPath"].as_str().expect("launch names the file"));
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("{shell_id}.output")
        );
        let note = launched["note"].as_str().unwrap();
        assert!(note.contains("Read") && note.contains("ShellOutput"), "{note}");
        // The cursor fields are gone from the launch result.
        for absent in ["oldestCursor", "endCursor", "outputTruncated"] {
            assert!(launched.get(absent).is_none(), "{absent} in {launched}");
        }

        let completed = wait_for_completion(&registry, shell_id).await;
        assert_eq!(completed["status"], "exited");
        assert_eq!(completed["exitCode"], 0);
        assert_eq!(completed["outputPath"], launched["outputPath"]);
        let tail = completed["outputTail"].as_str().unwrap();
        assert!(tail.contains("first") && tail.contains("second"), "{tail:?}");

        let file = std::fs::read_to_string(&path).unwrap();
        assert!(file.contains("first") && file.contains("second"), "{file:?}");
        assert_eq!(completed["outputBytes"], file.len() as u64);
    }

    #[tokio::test]
    async fn wait_result_omits_default_fields() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let result = registry
            .spawn(
                &context("session-a"),
                configured_command(two_lines_script()),
                "PowerShell",
                "test".into(),
                None,
            )
            .await
            .unwrap();
        let shell_id = result["shellId"].as_str().unwrap();
        wait_for_completion(&registry, shell_id).await;

        let full = registry
            .wait(&context("session-a"), "ShellOutput", shell_id, false, 0)
            .await
            .unwrap();
        assert_eq!(full["status"], "exited");
        assert_eq!(full["completed"], true);
        assert_eq!(full["command"], "test");
        let object = full.as_object().unwrap();
        for absent in [
            "output",
            "stderr",
            "events",
            "nextCursor",
            "timedOut",
            "stopRequested",
            "error",
            "waitTimedOut",
            "tailTruncated",
            "outputFileTruncated",
            "outputFileError",
        ] {
            assert!(
                !object.contains_key(absent),
                "`{absent}` should be omitted from {object:?}"
            );
        }
    }

    /// stderr goes to the same file and tail, in arrival order, the way a
    /// terminal shows it.
    #[tokio::test]
    async fn stderr_lands_in_the_same_file_and_tail() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let result = registry
            .spawn(
                &context("session-a"),
                configured_command(stderr_script()),
                "PowerShell",
                "test".into(),
                None,
            )
            .await
            .unwrap();
        let shell_id = result["shellId"].as_str().unwrap();
        let completed = wait_for_completion(&registry, shell_id).await;
        assert!(completed["outputTail"].as_str().unwrap().contains("oops"));
        let file = std::fs::read_to_string(completed["outputPath"].as_str().unwrap()).unwrap();
        assert!(file.contains("oops"), "{file:?}");
    }

    #[tokio::test]
    async fn output_files_go_under_the_session_scratchpad() {
        let _env = crate::test_env::hold_env();
        let scratchpad = tempfile::tempdir().unwrap();
        let registry = ShellProcessRegistry::new();
        let launched = registry
            .spawn(
                &context("session-a").with_scratchpad_dir(scratchpad.path()),
                configured_command(one_line_script()),
                "Bash",
                "test".into(),
                None,
            )
            .await
            .unwrap();
        let path = PathBuf::from(launched["outputPath"].as_str().unwrap());
        assert_eq!(path.parent().unwrap(), scratchpad.path().join("shells"));
        wait_for_completion(&registry, launched["shellId"].as_str().unwrap()).await;
    }

    #[tokio::test]
    async fn output_file_is_removed_once_the_registry_forgets_the_shell() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let launched = registry
            .spawn(
                &context("session-a"),
                configured_command(one_line_script()),
                "Bash",
                "test".into(),
                None,
            )
            .await
            .unwrap();
        let path = PathBuf::from(launched["outputPath"].as_str().unwrap());
        wait_for_completion(&registry, launched["shellId"].as_str().unwrap()).await;
        assert!(path.exists());

        drop(registry);
        for _ in 0..100 {
            if !path.exists() {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("{} outlived its shell", path.display());
    }

    #[tokio::test]
    async fn shell_ids_are_isolated_by_owner() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let result = registry
            .spawn(
                &context("session-a"),
                configured_command(one_line_script()),
                "PowerShell",
                "test".into(),
                None,
            )
            .await
            .unwrap();
        let shell_id = result["shellId"].as_str().unwrap();
        let error = registry
            .wait(&context("session-b"), "ShellOutput", shell_id, false, 1)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown shellId"));
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let result = registry
            .spawn(
                &context("session-a"),
                configured_command(long_running_script()),
                "PowerShell",
                "test".into(),
                None,
            )
            .await
            .unwrap();
        let shell_id = result["shellId"].as_str().unwrap();
        let first = registry
            .stop(&context("session-a"), "ShellStop", shell_id)
            .unwrap();
        let second = registry
            .stop(&context("session-a"), "ShellStop", shell_id)
            .unwrap();
        assert_eq!(first["alreadyRequested"], false);
        assert_eq!(second["alreadyRequested"], true);

        let completed = wait_for_completion(&registry, shell_id).await;
        assert_eq!(completed["status"], "stopped");
    }

    #[tokio::test]
    async fn explicit_timeout_marks_shell_timed_out() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let result = registry
            .spawn(
                &context("session-a"),
                configured_command(long_running_script()),
                "PowerShell",
                "test".into(),
                Some(25),
            )
            .await
            .unwrap();
        let shell_id = result["shellId"].as_str().unwrap();
        let completed = wait_for_completion(&registry, shell_id).await;
        assert_eq!(completed["status"], "timed_out");
        assert_eq!(completed["timedOut"], true);
    }

    #[tokio::test]
    async fn background_operations_require_an_owner() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let error = registry
            .spawn(
                &ToolContext::new(),
                configured_command(one_line_script()),
                "PowerShell",
                "test".into(),
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("session_id or agent_id"));
    }

    #[tokio::test]
    async fn list_only_returns_shells_for_the_current_owner() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        registry
            .spawn(
                &context("session-a"),
                configured_command(one_line_script()),
                "PowerShell",
                "test".into(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            registry.list(&context("session-a"), "ShellOutput").unwrap()["shells"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            registry.list(&context("session-b"), "ShellOutput").unwrap()["shells"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn output_wait_timeout_does_not_stop_the_shell() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let result = registry
            .spawn(
                &context("session-a"),
                configured_command(long_running_script()),
                "PowerShell",
                "test".into(),
                None,
            )
            .await
            .unwrap();
        let shell_id = result["shellId"].as_str().unwrap();
        let output = registry
            .wait(&context("session-a"), "ShellOutput", shell_id, true, 25)
            .await
            .unwrap();
        assert_eq!(output["waitTimedOut"], true);
        assert_eq!(output["status"], "running");
        assert_eq!(output["completed"], false);
        registry
            .stop(&context("session-a"), "ShellStop", shell_id)
            .unwrap();
        let completed = wait_for_completion(&registry, shell_id).await;
        assert_eq!(completed["status"], "stopped");
    }

    /// A wait while the process is running tells the tool card which
    /// command it is waiting on, before any result exists.
    #[tokio::test]
    async fn wait_reports_the_command_it_waits_on_as_progress() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let result = registry
            .spawn(
                &context("session-a"),
                configured_command(long_running_script()),
                "PowerShell",
                "cargo build --release".into(),
                None,
            )
            .await
            .unwrap();
        let shell_id = result["shellId"].as_str().unwrap();
        let (progress_context, mut progress) = context("session-a").with_progress("toolu_wait");
        registry
            .wait(&progress_context, "ShellOutput", shell_id, true, 25)
            .await
            .unwrap();
        let update = progress.try_recv().expect("a wait emits progress");
        assert_eq!(update.kind, "shell_wait");
        let payload = update.payload.unwrap();
        assert_eq!(payload["command"], "cargo build --release");
        assert_eq!(payload["waiting"], true);
        assert_eq!(payload["waitTimeoutMs"], 25);

        // A status check does not block, so it has nothing to announce.
        registry
            .wait(&progress_context, "ShellOutput", shell_id, false, 25)
            .await
            .unwrap();
        assert!(progress.try_recv().is_err());

        registry
            .stop(&context("session-a"), "ShellStop", shell_id)
            .unwrap();
        wait_for_completion(&registry, shell_id).await;
    }

    /// A wait on a process that prints steadily must last until its deadline,
    /// not return on new output. Returning on the first chunk made each wait
    /// on a build a model round-trip every few seconds.
    #[tokio::test]
    async fn wait_lasts_until_the_deadline_instead_of_the_first_chunk() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let result = registry
            .spawn(
                &context("session-a"),
                configured_command(ticking_script()),
                "PowerShell",
                "test".into(),
                None,
            )
            .await
            .unwrap();
        let shell_id = result["shellId"].as_str().unwrap();

        // Start the timed wait once the process is demonstrably printing, so
        // a slow shell start-up cannot eat the window under test.
        let started = tokio::time::Instant::now();
        let ticks_before = loop {
            let poll = registry
                .wait(&context("session-a"), "ShellOutput", shell_id, false, 0)
                .await
                .unwrap();
            let ticks = poll["outputTail"]
                .as_str()
                .unwrap_or("")
                .matches("tick")
                .count();
            if ticks > 0 {
                break ticks;
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "no tick: {poll}"
            );
            sleep(Duration::from_millis(20)).await;
        };

        let waited = tokio::time::Instant::now();
        let output = registry
            .wait(&context("session-a"), "ShellOutput", shell_id, true, 1_500)
            .await
            .unwrap();
        let elapsed = waited.elapsed();

        assert_eq!(output["waitTimedOut"], true, "{output}");
        assert!(
            elapsed >= Duration::from_millis(1_400),
            "returned after {elapsed:?}"
        );
        let ticks = output["outputTail"]
            .as_str()
            .unwrap_or("")
            .matches("tick")
            .count();
        assert!(
            ticks >= ticks_before + 3,
            "expected the ticks printed during the wait: {output}"
        );

        registry
            .stop(&context("session-a"), "ShellStop", shell_id)
            .unwrap();
        wait_for_completion(&registry, shell_id).await;
    }

    #[tokio::test]
    async fn stop_terminates_the_entire_process_tree() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let result = registry
            .spawn(
                &context("session-a"),
                configured_command(nested_process_script()),
                "PowerShell",
                "test".into(),
                None,
            )
            .await
            .unwrap();
        let shell_id = result["shellId"].as_str().unwrap();
        let nested_process_id = wait_for_nested_process_id(&registry, shell_id).await;
        assert!(process_is_alive(nested_process_id));

        registry
            .stop(&context("session-a"), "ShellStop", shell_id)
            .unwrap();
        let completed = wait_for_completion(&registry, shell_id).await;
        assert_eq!(completed["status"], "stopped");
        assert_process_exits(nested_process_id).await;
    }

    #[tokio::test]
    async fn dropping_registry_terminates_the_entire_process_tree() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let result = registry
            .spawn(
                &context("session-a"),
                configured_command(nested_process_script()),
                "PowerShell",
                "test".into(),
                None,
            )
            .await
            .unwrap();
        let shell_id = result["shellId"].as_str().unwrap();
        let nested_process_id = wait_for_nested_process_id(&registry, shell_id).await;
        assert!(process_is_alive(nested_process_id));

        drop(registry);
        assert_process_exits(nested_process_id).await;
    }

    #[tokio::test]
    async fn command_monitor_emits_trimmed_stdout_lines_and_ignores_stderr() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let controller = Arc::new(RecordingTaskController::default());
        let context = context("session-a").with_task_runtime_controller(controller.clone());
        let task_id = registry
            .spawn_monitor(
                &context,
                configured_command(monitor_lines_script()),
                monitor_lines_script().into(),
                "application events".into(),
                "shell command (redacted)".into(),
                None,
            )
            .await
            .unwrap();

        let completion = wait_for_monitor_completion(&controller).await;
        assert_eq!(completion.task_id, task_id);
        assert_eq!(completion.status, MonitorTaskCompletionStatus::Exited);
        assert_eq!(completion.exit_code, Some(0));
        assert!(completion
            .stderr
            .as_deref()
            .unwrap_or_default()
            .contains("ignored"));

        let started = lock(&controller.monitor_started);
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].source, MonitorTaskSource::Command);
        assert_eq!(started[0].redacted_target, "shell command (redacted)");
        drop(started);

        let events = lock(&controller.monitor_events)
            .iter()
            .map(|(_, event)| event.clone())
            .collect::<Vec<_>>();
        assert_eq!(events, vec!["first", "second", "partial"]);
        assert!(!events.iter().any(|event| event.contains("ignored")));
        assert!(lock(&controller.started).is_empty());
    }

    #[tokio::test]
    async fn shell_output_neither_lists_nor_reads_monitors() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let controller = Arc::new(RecordingTaskController::default());
        let context = context("session-a").with_task_runtime_controller(controller.clone());
        let task_id = registry
            .spawn_monitor(
                &context,
                configured_command(one_line_script()),
                one_line_script().into(),
                "events".into(),
                "shell command (redacted)".into(),
                None,
            )
            .await
            .unwrap();
        wait_for_monitor_completion(&controller).await;

        let listed = registry.list(&context, "ShellOutput").unwrap();
        assert_eq!(listed["shells"], json!([]));

        for wait in [false, true] {
            match registry
                .wait(&context, "ShellOutput", &task_id, wait, 1_000)
                .await
                .unwrap_err()
            {
                ToolError::InvalidInput {
                    reason, error_code, ..
                } => {
                    assert!(reason.contains("is a Monitor"), "{reason}");
                    assert!(reason.contains("task notification"), "{reason}");
                    assert_eq!(error_code, Some(INVALID_INPUT_CODE));
                }
                other => panic!("expected a Monitor refusal, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn command_monitor_timeout_reports_timed_out() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let controller = Arc::new(RecordingTaskController::default());
        let context = context("session-a").with_task_runtime_controller(controller.clone());
        registry
            .spawn_monitor(
                &context,
                configured_command(long_running_script()),
                long_running_script().into(),
                "slow events".into(),
                "shell command (redacted)".into(),
                Some(25),
            )
            .await
            .unwrap();

        let completion = wait_for_monitor_completion(&controller).await;
        assert_eq!(completion.status, MonitorTaskCompletionStatus::TimedOut);
    }

    #[tokio::test]
    async fn command_monitor_auto_stop_terminates_process_and_reports_reason() {
        let _env = crate::test_env::hold_env();
        let registry = ShellProcessRegistry::new();
        let controller = Arc::new(RecordingTaskController::default());
        *lock(&controller.monitor_disposition) = Some(MonitorEventDisposition::AutoStop);
        let context = context("session-a").with_task_runtime_controller(controller.clone());
        registry
            .spawn_monitor(
                &context,
                configured_command(long_running_script_with_initial_line()),
                long_running_script_with_initial_line().into(),
                "noisy events".into(),
                "shell command (redacted)".into(),
                None,
            )
            .await
            .unwrap();

        let completion = wait_for_monitor_completion(&controller).await;
        assert_eq!(completion.status, MonitorTaskCompletionStatus::AutoStopped);
        assert_eq!(lock(&controller.monitor_events).len(), 1);
    }

    /// A process outside the group can hold stdout open after the group is
    /// gone; on macOS any process spawned while the pipe was made can. Perl
    /// stands in for one here: `setpgrp` leaves the group the stop reaps,
    /// and it keeps the pipe for longer than the test waits.
    #[cfg(unix)]
    #[tokio::test]
    async fn command_monitor_auto_stop_does_not_wait_for_a_pipe_held_outside_the_group() {
        let _env = crate::test_env::hold_env();
        let script = "perl -e 'setpgrp(0, 0); sleep 10' & sleep 0.5; printf 'event\\n'; sleep 30";
        let registry = ShellProcessRegistry::new();
        let controller = Arc::new(RecordingTaskController::default());
        *lock(&controller.monitor_disposition) = Some(MonitorEventDisposition::AutoStop);
        let context = context("session-a").with_task_runtime_controller(controller.clone());
        registry
            .spawn_monitor(
                &context,
                configured_command(script),
                script.into(),
                "noisy events".into(),
                "shell command (redacted)".into(),
                None,
            )
            .await
            .unwrap();

        let completion = wait_for_monitor_completion(&controller).await;
        assert_eq!(completion.status, MonitorTaskCompletionStatus::AutoStopped);
    }

    /// Output past the in-memory tail is still all in the file, and the
    /// result says the tail is only the end of it.
    #[test]
    fn long_output_keeps_everything_in_the_file_and_flags_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sh_test.output");
        let file = File::create(&path).unwrap();
        let process_tree = Arc::new(ProcessTreeControl::finished_without_a_tree());
        let entry = ShellEntry::new(
            "sh_test".into(),
            ShellOwner {
                session_id: Some("session-a".into()),
                agent_id: None,
            },
            "Bash",
            "test".into(),
            None,
            process_tree,
            None,
            PromptCancel::new(),
            ShellTaskRegistration::BackgroundShell,
            Some((path.clone(), file)),
        );
        let line = format!("{}\n", "x".repeat(99));
        let chunks = 140 * READ_CHUNK_BYTES / line.len();
        for _ in 0..chunks {
            entry.push_output("stdout", line.clone());
        }
        let (value, completed) = entry.result_value(false);
        assert!(!completed);
        assert_eq!(value["tailTruncated"], true);
        assert_eq!(value["outputBytes"], (chunks * line.len()) as u64);
        assert!(value["outputTail"].as_str().unwrap().len() <= TAIL_RESPONSE_BYTES);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            (chunks * line.len()) as u64
        );

        drop(entry);
        assert!(!path.exists(), "the file goes with its entry");
    }
}

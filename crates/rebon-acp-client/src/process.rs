//! The agent child process.
//!
//! An ACP agent is somebody else's CLI. Rebon owns three things about
//! it: how it is started, where its stderr goes, and that it does not
//! outlive the session that spawned it.
//!
//! The last one matters most. An agent that survives its parent is an
//! orphan holding a model API key and a working directory, and on a
//! long-lived desktop app a leaked one per session adds up fast. So
//! [`AgentProcess`] owns the entire spawned process tree, kills it on
//! drop, and never assumes a well-behaved agent will exit on its own
//! when stdin closes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;

use rebon_tools_core::ProcessTreeGuard;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// How to start an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCommand {
    /// Executable to run.
    pub command: String,
    pub args: Vec<String>,
    /// Extra environment. Inherited variables stay inherited; these
    /// are layered on top.
    pub env: BTreeMap<String, String>,
    /// Working directory for the child. `None` inherits Rebon's.
    pub cwd: Option<PathBuf>,
    /// Appended to a spawn failure. Declaring surfaces (plugins in
    /// particular) do not install the CLI they point at; this is where
    /// "npm install -g …" reaches the user at the moment it matters.
    pub spawn_hint: Option<String>,
}

impl AgentCommand {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            spawn_hint: None,
        }
    }

    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// See [`AgentCommand::spawn_hint`].
    pub fn with_spawn_hint(mut self, hint: Option<String>) -> Self {
        self.spawn_hint = hint;
        self
    }

    /// How the command reads in a log line or an error message.
    pub fn display(&self) -> String {
        if self.args.is_empty() {
            self.command.clone()
        } else {
            format!("{} {}", self.command, self.args.join(" "))
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("failed to start agent `{command}`: {source}{}", hint.as_deref().map(|hint| format!(" ({hint})")).unwrap_or_default())]
    Spawn {
        command: String,
        source: std::io::Error,
        hint: Option<String>,
    },
    #[error("agent `{0}` started without a usable stdio pipe")]
    MissingPipe(String),
}

/// Guard a spawned agent's whole process tree.
///
/// The guard itself lives in `rebon-tools-core`; what stays here is how
/// this caller reaches the child's raw identifier, and what it calls a
/// child that has none. No breakaway: nothing an agent starts may opt out
/// of being reaped with it.
#[cfg(windows)]
fn guard_process_tree(child: &Child) -> std::io::Result<ProcessTreeGuard> {
    let handle = child
        .raw_handle()
        .ok_or_else(|| std::io::Error::other("child process has no process handle"))?;
    ProcessTreeGuard::for_raw_handle(handle, false)
}

#[cfg(unix)]
fn guard_process_tree(child: &Child) -> std::io::Result<ProcessTreeGuard> {
    let process_id = child
        .id()
        .ok_or_else(|| std::io::Error::other("child process has no pid"))?;
    ProcessTreeGuard::for_process_group(process_id)
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
fn configure_process_group(_command: &mut Command) {}

/// A running agent, and the pipes to talk to it.
pub struct AgentProcess {
    child: Child,
    process_tree: Option<ProcessTreeGuard>,
    label: String,
}

impl AgentProcess {
    /// Start the agent and take its stdio.
    ///
    /// Returns the process alongside the pipes rather than keeping
    /// them, so the connection can own the halves it needs while the
    /// process handle stays responsible for the lifecycle.
    pub fn spawn(spec: &AgentCommand) -> Result<(Self, ChildStdout, ChildStdin), SpawnError> {
        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        // Without this the child keeps running after Rebon exits on
        // Unix; `Child::kill` on drop only helps if we get to run.
        command.kill_on_drop(true);
        configure_process_group(&mut command);
        // A console-subsystem agent must not flash a console window on
        // the desktop (CREATE_NO_WINDOW, like every other spawn point).
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);

        let mut child = command.spawn().map_err(|source| SpawnError::Spawn {
            command: spec.display(),
            source,
            hint: spec.spawn_hint.clone(),
        })?;
        let process_tree = match guard_process_tree(&child) {
            Ok(process_tree) => Some(process_tree),
            Err(err) => {
                tracing::warn!(
                    agent = %spec.display(),
                    error = %err,
                    "acp-agent: failed to guard process tree"
                );
                None
            }
        };

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SpawnError::MissingPipe(spec.display()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SpawnError::MissingPipe(spec.display()))?;

        let label = spec.display();
        if let Some(stderr) = child.stderr.take() {
            // An agent's stderr is where its crashes explain
            // themselves. Dropping it would turn every startup failure
            // into "the connection closed".
            let log_label = label.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!(agent = %log_label, "acp-agent: {line}");
                }
            });
        }

        Ok((
            Self {
                child,
                process_tree,
                label,
            },
            stdout,
            stdin,
        ))
    }

    /// The command line, for logs and error messages.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether the child has exited, without waiting for it.
    pub fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Stop the child and every descendant it spawned.
    pub async fn shutdown(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            if let Err(err) = self.child.kill().await {
                tracing::warn!(agent = %self.label, error = %err, "acp-agent: failed to kill");
            }
        }
        if let Some(mut process_tree) = self.process_tree.take() {
            if let Err(err) = process_tree.terminate() {
                tracing::warn!(agent = %self.label, error = %err, "acp-agent: failed to reap the tree");
            }
        }
    }
}

impl std::fmt::Debug for AgentProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentProcess")
            .field("label", &self.label)
            .field("pid", &self.child.id())
            .finish()
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        // `kill_on_drop` covers the tokio-managed path; this makes the
        // intent explicit and covers the case where the runtime is
        // already shutting down and will not poll the reaper.
        let _ = self.child.start_kill();
        if let Some(mut process_tree) = self.process_tree.take() {
            let _ = process_tree.terminate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_the_arguments() {
        let spec = AgentCommand::new("my-agent").with_args(["--acp", "--quiet"]);
        assert_eq!(spec.display(), "my-agent --acp --quiet");
        assert_eq!(AgentCommand::new("bare").display(), "bare");
    }

    #[test]
    fn builders_layer_env_and_cwd() {
        let spec = AgentCommand::new("agent")
            .with_env("TOKEN", "secret")
            .with_env("MODE", "acp")
            .with_cwd("/tmp/work");
        assert_eq!(spec.env.get("TOKEN").map(String::as_str), Some("secret"));
        assert_eq!(spec.env.get("MODE").map(String::as_str), Some("acp"));
        assert_eq!(spec.cwd, Some(PathBuf::from("/tmp/work")));
    }

    #[cfg(any(unix, windows))]
    fn process_tree_command(pid_file: &std::path::Path) -> AgentCommand {
        #[cfg(windows)]
        let command = AgentCommand::new("powershell.exe")
            .with_args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$p = Start-Process -FilePath ping.exe -ArgumentList '-t','127.0.0.1' -PassThru; Set-Content -LiteralPath $env:REBON_DESCENDANT_PID_FILE -Value $p.Id -NoNewline; Wait-Process -Id $p.Id",
            ])
            .with_env(
                "REBON_DESCENDANT_PID_FILE",
                pid_file.to_string_lossy().into_owned(),
            );
        #[cfg(unix)]
        let command = AgentCommand::new("sh")
            .with_args([
                "-c",
                "sleep 60 & child=$!; printf %s \"$child\" > \"$REBON_DESCENDANT_PID_FILE\"; wait \"$child\"",
            ])
            .with_env(
                "REBON_DESCENDANT_PID_FILE",
                pid_file.to_string_lossy().into_owned(),
            );
        command
    }

    #[cfg(any(unix, windows))]
    fn unique_pid_dir(test: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("rebon-acp-{test}-"))
            .tempdir()
            .expect("pid dir")
    }

    #[cfg(any(unix, windows))]
    async fn read_descendant_pid(path: &std::path::Path) -> u32 {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Ok(raw) = std::fs::read_to_string(path) {
                    if let Ok(pid) = raw.trim().parse() {
                        break pid;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("descendant pid file")
    }

    #[cfg(windows)]
    fn process_is_running(pid: u32) -> bool {
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle == 0 {
            return false;
        }
        let mut exit_code = 0;
        let read = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        read != 0 && exit_code == windows_sys::Win32::Foundation::STILL_ACTIVE as u32
    }

    #[cfg(unix)]
    fn process_is_running(pid: u32) -> bool {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    #[cfg(any(unix, windows))]
    async fn wait_for_process_exit(pid: u32) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while process_is_running(pid) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("descendant process must exit");
    }

    #[tokio::test]
    #[cfg(any(unix, windows))]
    async fn shutdown_reaps_agent_descendants() {
        let dir = unique_pid_dir("shutdown-tree");
        let pid_file = dir.path().join("descendant.pid");
        let (mut process, stdout, stdin) =
            AgentProcess::spawn(&process_tree_command(&pid_file)).expect("spawn process tree");
        let descendant = read_descendant_pid(&pid_file).await;
        assert!(process_is_running(descendant));

        process.shutdown().await;
        drop((stdout, stdin));
        wait_for_process_exit(descendant).await;
    }

    #[tokio::test]
    #[cfg(any(unix, windows))]
    async fn drop_reaps_agent_descendants() {
        let dir = unique_pid_dir("drop-tree");
        let pid_file = dir.path().join("descendant.pid");
        let (process, stdout, stdin) =
            AgentProcess::spawn(&process_tree_command(&pid_file)).expect("spawn process tree");
        let descendant = read_descendant_pid(&pid_file).await;
        assert!(process_is_running(descendant));

        drop(process);
        drop((stdout, stdin));
        wait_for_process_exit(descendant).await;
    }

    #[tokio::test]
    async fn spawning_a_missing_binary_names_the_command() {
        let spec = AgentCommand::new("rebon-agent-that-does-not-exist-xyz");
        let err = AgentProcess::spawn(&spec).expect_err("must not pretend to have started");
        let message = err.to_string();
        assert!(
            message.contains("rebon-agent-that-does-not-exist-xyz"),
            "error should name the command: {message}"
        );
    }
}

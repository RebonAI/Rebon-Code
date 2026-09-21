use std::fmt;
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result};

const TMUX_COMMAND: &str = "tmux";
const SWARM_SESSION_NAME: &str = "rebon-swarm";
const SWARM_VIEW_WINDOW_NAME: &str = "swarm-view";
const HIDDEN_SESSION_NAME: &str = "rebon-hidden";

pub trait TeamPaneBackend: Send + Sync {
    fn view_pane(&self, pane_id: &str, backend_type: Option<&str>) -> Result<bool>;
    fn kill_pane(&self, pane_id: &str, backend_type: Option<&str>) -> Result<bool>;
    fn set_hidden(&self, pane_id: &str, backend_type: Option<&str>, hide: bool) -> Result<bool>;
}

#[derive(Clone)]
pub struct TeamPaneBackendHandle(Arc<dyn TeamPaneBackend>);

impl TeamPaneBackendHandle {
    pub fn system() -> Self {
        Self(Arc::new(SystemTeamPaneBackend))
    }

    #[cfg(test)]
    pub fn from_arc(backend: Arc<dyn TeamPaneBackend>) -> Self {
        Self(backend)
    }

    pub fn view_pane(&self, pane_id: &str, backend_type: Option<&str>) -> Result<bool> {
        self.0.view_pane(pane_id, backend_type)
    }

    pub fn kill_pane(&self, pane_id: &str, backend_type: Option<&str>) -> Result<bool> {
        self.0.kill_pane(pane_id, backend_type)
    }

    pub fn set_hidden(
        &self,
        pane_id: &str,
        backend_type: Option<&str>,
        hide: bool,
    ) -> Result<bool> {
        self.0.set_hidden(pane_id, backend_type, hide)
    }
}

impl fmt::Debug for TeamPaneBackendHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TeamPaneBackendHandle")
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct SystemTeamPaneBackend;

impl TeamPaneBackend for SystemTeamPaneBackend {
    fn view_pane(&self, pane_id: &str, backend_type: Option<&str>) -> Result<bool> {
        if pane_id.is_empty() || backend_type != Some("tmux") {
            return Ok(false);
        }
        let inside_tmux = inside_tmux();
        let mut args = Vec::new();
        if !inside_tmux {
            args.push("-L".to_string());
            args.push(swarm_socket_name());
        }
        args.push("select-pane".to_string());
        args.push("-t".to_string());
        args.push(pane_id.to_string());
        Ok(run_tmux(&args)?.success)
    }

    fn kill_pane(&self, pane_id: &str, backend_type: Option<&str>) -> Result<bool> {
        if pane_id.is_empty() || backend_type != Some("tmux") {
            return Ok(false);
        }
        let output = if inside_tmux() {
            run_tmux(&["kill-pane".into(), "-t".into(), pane_id.to_string()])?
        } else {
            run_tmux(&[
                "-L".into(),
                swarm_socket_name(),
                "kill-pane".into(),
                "-t".into(),
                pane_id.to_string(),
            ])?
        };
        Ok(output.success)
    }

    fn set_hidden(&self, pane_id: &str, backend_type: Option<&str>, hide: bool) -> Result<bool> {
        if pane_id.is_empty() || backend_type != Some("tmux") {
            return Ok(false);
        }
        if hide {
            hide_tmux_pane(pane_id)
        } else {
            show_tmux_pane(pane_id)
        }
    }
}

#[derive(Debug)]
struct TmuxCommandOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

fn hide_tmux_pane(pane_id: &str) -> Result<bool> {
    let use_external_session = !inside_tmux();
    let mut init_args = Vec::new();
    if use_external_session {
        init_args.push("-L".to_string());
        init_args.push(swarm_socket_name());
    }
    init_args.extend([
        "new-session".to_string(),
        "-d".to_string(),
        "-s".to_string(),
        HIDDEN_SESSION_NAME.to_string(),
    ]);
    let _ = run_tmux(&init_args)?;

    let mut args = Vec::new();
    if use_external_session {
        args.push("-L".to_string());
        args.push(swarm_socket_name());
    }
    args.extend([
        "break-pane".to_string(),
        "-d".to_string(),
        "-s".to_string(),
        pane_id.to_string(),
        "-t".to_string(),
        format!("{HIDDEN_SESSION_NAME}:"),
    ]);
    Ok(run_tmux(&args)?.success)
}

fn show_tmux_pane(pane_id: &str) -> Result<bool> {
    let inside_tmux = inside_tmux();
    let target = if inside_tmux {
        current_tmux_window_target()?
    } else {
        format!("{SWARM_SESSION_NAME}:{SWARM_VIEW_WINDOW_NAME}")
    };
    let mut join_args = Vec::new();
    if !inside_tmux {
        join_args.push("-L".to_string());
        join_args.push(swarm_socket_name());
    }
    join_args.extend([
        "join-pane".to_string(),
        "-h".to_string(),
        "-s".to_string(),
        pane_id.to_string(),
        "-t".to_string(),
        target.clone(),
    ]);
    let joined = run_tmux(&join_args)?;
    if !joined.success {
        return Ok(false);
    }

    let mut layout_args = Vec::new();
    if !inside_tmux {
        layout_args.push("-L".to_string());
        layout_args.push(swarm_socket_name());
    }
    layout_args.extend([
        "select-layout".to_string(),
        "-t".to_string(),
        target.clone(),
        "main-vertical".to_string(),
    ]);
    let _ = run_tmux(&layout_args)?;

    let mut list_args = Vec::new();
    if !inside_tmux {
        list_args.push("-L".to_string());
        list_args.push(swarm_socket_name());
    }
    list_args.extend([
        "list-panes".to_string(),
        "-t".to_string(),
        target.clone(),
        "-F".to_string(),
        "#{pane_id}".to_string(),
    ]);
    let panes = run_tmux(&list_args)?;
    let leader_pane = panes.stdout.lines().find(|line| !line.trim().is_empty());
    if let Some(leader_pane) = leader_pane {
        let mut resize_args = Vec::new();
        if !inside_tmux {
            resize_args.push("-L".to_string());
            resize_args.push(swarm_socket_name());
        }
        resize_args.extend([
            "resize-pane".to_string(),
            "-t".to_string(),
            leader_pane.trim().to_string(),
            "-x".to_string(),
            "30%".to_string(),
        ]);
        let _ = run_tmux(&resize_args)?;
    }
    Ok(true)
}

fn current_tmux_window_target() -> Result<String> {
    let output = run_tmux(&[
        "display-message".into(),
        "-p".into(),
        "#{session_name}:#{window_index}".into(),
    ])?;
    if !output.success {
        anyhow::bail!(
            "failed to resolve current tmux window target: {}",
            output.stderr.trim()
        );
    }
    let target = output.stdout.trim().to_string();
    if target.is_empty() {
        anyhow::bail!("tmux did not return a current window target");
    }
    Ok(target)
}

fn inside_tmux() -> bool {
    std::env::var("TMUX")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

fn swarm_socket_name() -> String {
    format!("rebon-swarm-{}", std::process::id())
}

fn run_tmux(args: &[String]) -> Result<TmuxCommandOutput> {
    let mut command = if cfg!(windows) {
        let mut cmd = Command::new("wsl");
        // On Windows, tmux runs through WSL so it can use the Unix command path.
        cmd.arg("-e").arg(TMUX_COMMAND);
        cmd.env("WSL_UTF8", "1");
        cmd
    } else {
        Command::new(TMUX_COMMAND)
    };

    let output = command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to run `{TMUX_COMMAND}`"))?;
    Ok(TmuxCommandOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

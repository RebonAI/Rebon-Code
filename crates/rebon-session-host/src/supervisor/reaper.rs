use std::process::{Child, ExitStatus};
use std::sync::{Mutex, OnceLock};

use crate::*;

pub type BackgroundWorkerReaperHandle = std::thread::JoinHandle<std::io::Result<ExitStatus>>;

#[derive(Debug)]
pub struct BackgroundWorkerReaperStartError {
    pub error: std::io::Error,
    pub exit_verified: bool,
}

impl BackgroundWorkerReaperStartError {
    fn after_cleanup(error: std::io::Error, cleanup: std::io::Result<ExitStatus>) -> Self {
        match cleanup {
            Ok(_) => Self {
                error,
                exit_verified: true,
            },
            Err(cleanup_error) => Self {
                error: std::io::Error::new(
                    cleanup_error.kind(),
                    format!("{error}; worker exit could not be verified: {cleanup_error}"),
                ),
                exit_verified: false,
            },
        }
    }
}

impl std::fmt::Display for BackgroundWorkerReaperStartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for BackgroundWorkerReaperStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

pub static RETAINED_BACKGROUND_WORKER_CHILDREN: OnceLock<Mutex<Vec<Child>>> = OnceLock::new();

pub fn retained_background_worker_children() -> &'static Mutex<Vec<Child>> {
    RETAINED_BACKGROUND_WORKER_CHILDREN.get_or_init(|| Mutex::new(Vec::new()))
}

pub fn retain_unverified_background_worker_child(child: Child) {
    retained_background_worker_children()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(child);
}

pub fn terminate_child_after_reaper_start_failure(mut child: Child) -> std::io::Result<ExitStatus> {
    if let Err(kill_error) = child.kill() {
        return match child.try_wait() {
            Ok(Some(status)) => Ok(status),
            Ok(None) => {
                let error = std::io::Error::new(
                    kill_error.kind(),
                    format!("failed to stop worker after reaper setup failure: {kill_error}"),
                );
                retain_unverified_background_worker_child(child);
                Err(error)
            }
            Err(wait_error) => {
                let error = std::io::Error::new(
                    wait_error.kind(),
                    format!(
                        "failed to stop worker after reaper setup failure: {kill_error}; \
                         failed to verify exit: {wait_error}"
                    ),
                );
                retain_unverified_background_worker_child(child);
                Err(error)
            }
        };
    }
    loop {
        match child.wait() {
            Ok(status) => return Ok(status),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                retain_unverified_background_worker_child(child);
                return Err(error);
            }
        }
    }
}

pub fn poll_retained_background_worker_children_in(children: &mut Vec<Child>) -> bool {
    let observed_children = !children.is_empty();
    let mut index = 0;
    while index < children.len() {
        let reaped = match children[index].try_wait() {
            Ok(Some(_)) => true,
            Ok(None) => {
                let _ = children[index].kill();
                matches!(children[index].try_wait(), Ok(Some(_)))
            }
            Err(_) => false,
        };
        if reaped {
            children.swap_remove(index);
        } else {
            index += 1;
        }
    }
    observed_children
}

pub fn poll_retained_background_worker_children() -> bool {
    let mut children = retained_background_worker_children()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    poll_retained_background_worker_children_in(&mut children)
}

pub fn spawn_background_worker_reaper_with(
    child: Child,
    spawn: impl FnOnce(
        u32,
        std::sync::mpsc::Receiver<Child>,
    ) -> std::io::Result<BackgroundWorkerReaperHandle>,
) -> Result<BackgroundWorkerReaperHandle, BackgroundWorkerReaperStartError> {
    let pid = child.id();
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Child>(0);
    let handle = match spawn(pid, receiver) {
        Ok(handle) => handle,
        Err(error) => {
            let cleanup = terminate_child_after_reaper_start_failure(child);
            return Err(BackgroundWorkerReaperStartError::after_cleanup(
                error, cleanup,
            ));
        }
    };
    if let Err(std::sync::mpsc::SendError(child)) = sender.send(child) {
        let _ = handle.join();
        let cleanup = terminate_child_after_reaper_start_failure(child);
        return Err(BackgroundWorkerReaperStartError::after_cleanup(
            std::io::Error::other("worker reaper exited before receiving its child handle"),
            cleanup,
        ));
    }
    Ok(handle)
}

pub fn spawn_background_worker_reaper(
    child: Child,
) -> Result<BackgroundWorkerReaperHandle, BackgroundWorkerReaperStartError> {
    spawn_background_worker_reaper_with(child, |pid, receiver| {
        std::thread::Builder::new()
            .name(format!("background-worker-reaper-{pid}"))
            .spawn(move || {
                let mut child = receiver
                    .recv()
                    .map_err(|_| std::io::Error::other("worker reaper lost its child handle"))?;
                child.wait()
            })
    })
}

pub fn state_was_claimed_by_spawned_worker(
    current: &BackgroundJobState,
    child_pid: u32,
    child_identity: &Option<String>,
    expected_turn_generation: u64,
) -> bool {
    let owner = current.recorded_owner();
    owner.pid == Some(child_pid)
        && owner.pid_identity == *child_identity
        && !owner.process_owner_fenced
        && owner.ipc_port.is_some()
        && owner.ipc_token.is_some()
        && current.process.spawn_admitted
        && current.process.status == BackgroundJobStatus::Running
        && owner.turn_generation == expected_turn_generation.wrapping_add(1).max(1)
}

pub fn apply_background_worker_reaper_start_failure(
    current: &mut BackgroundJobState,
    expected_owner: &RecordedOwnerSnapshot,
    exit_verified: bool,
    failed_at: u64,
    error: &str,
) -> bool {
    if current.recorded_owner() != *expected_owner {
        return false;
    }
    let observed_owner = current.recorded_owner();
    if current.process.status != BackgroundJobStatus::Stopped {
        current.process.status = BackgroundJobStatus::Failed;
        current.outcome.error = Some(error.to_string());
    }
    if exit_verified {
        current.clear_recorded_owner();
    } else {
        current.set_recorded_owner(observed_owner.fenced(true));
    }
    current.process.spawn_admitted = false;
    current.outcome.pending_permission = None;
    current.process.completed_at_ms = Some(failed_at);
    current.process.updated_at_ms = failed_at;
    true
}

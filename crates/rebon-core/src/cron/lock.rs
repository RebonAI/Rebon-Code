//! Cross-process advisory lock for the cron scheduler.
//!
//! Implements the "only one scheduler may fire per project" invariant from
//! the runtime. We use `fs2::FileExt::try_lock_exclusive`
//! on a sentinel file (`<project>/.rebon/.scheduler.lock`) to elect a
//! single writer across rebon processes rooted at the same cwd.
//!
//! - Acquire is non-blocking: `try_acquire_scheduler_lock` returns `None`
//!   when another process already holds the lock.
//! - Lock is released when the returned [`SchedulerLock`] drops. The kernel
//!   releases the file lock if the process crashes; we still remove the
//!   sentinel file on graceful shutdown so stale lockfiles don't pile up.

use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

pub const SCHEDULER_LOCK_FILE: &str = ".scheduler.lock";

/// An active scheduler lock. Holding this value guarantees no other rebon
/// process rooted at the same project is currently firing crons.
pub struct SchedulerLock {
    file: Option<File>,
    path: PathBuf,
}

impl SchedulerLock {
    /// Explicitly release the lock. `Drop` does the same thing; this is
    /// here so callers can release on shutdown without waiting for the
    /// value to go out of scope.
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = FileExt::unlock(&file);
            drop(file);
            // Best-effort cleanup — a later acquirer creates it again if
            // missing. We intentionally ignore errors: on Windows the file
            // may still be held open briefly, and the next process will
            // just re-create.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Drop for SchedulerLock {
    fn drop(&mut self) {
        self.release_inner();
    }
}

/// Try to acquire the scheduler lock for `<rebon_dir>/.scheduler.lock`.
/// Returns `Some(lock)` when we became the owner, `None` when another
/// process is already the owner, and `Err` only on unexpected IO failures.
pub fn try_acquire_scheduler_lock(rebon_dir: &Path) -> Result<Option<SchedulerLock>> {
    std::fs::create_dir_all(rebon_dir)
        .with_context(|| format!("failed to create {}", rebon_dir.display()))?;
    let path = rebon_dir.join(SCHEDULER_LOCK_FILE);

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
        .with_context(|| format!("failed to open scheduler lock {}", path.display()))?;

    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(SchedulerLock {
            file: Some(file),
            path,
        })),
        Err(err) => {
            // fs2 returns a generic io::Error for contention; any error here
            // means we don't own the lock. Distinguish "contention" (WouldBlock)
            // from real failure for logging, but treat both as "not us".
            let kind = err.kind();
            if matches!(
                kind,
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::PermissionDenied
            ) {
                Ok(None)
            } else {
                // On Windows the contention error can surface as Other; still
                // treat as "owned by someone else" rather than failing hard.
                // Logged at TRACE: the scheduler probes every 5s, so DEBUG
                // would flood the log file. Enable with
                // `RUST_LOG=rebon_core::cron=trace` when investigating
                // lock-acquisition issues.
                tracing::trace!(
                    path = %path.display(),
                    error = %err,
                    "[CronLock] try_lock failed — assuming contention"
                );
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::TempDir;

    static NONCE: AtomicU64 = AtomicU64::new(0);

    fn tmp_project() -> TempDir {
        tempfile::Builder::new()
            .prefix(&format!(
                "rebon-cron-lock-{}-{}",
                std::process::id(),
                NONCE.fetch_add(1, Ordering::Relaxed)
            ))
            .tempdir()
            .unwrap()
    }

    #[test]
    fn first_acquire_succeeds_and_second_fails() {
        let dir = tmp_project();
        let rebon_dir = dir.path().join(".rebon");
        let first = try_acquire_scheduler_lock(&rebon_dir).unwrap();
        assert!(first.is_some(), "first acquire should succeed");
        let second = try_acquire_scheduler_lock(&rebon_dir).unwrap();
        assert!(second.is_none(), "second acquire must see contention");
    }

    #[test]
    fn lock_is_released_on_drop() {
        let dir = tmp_project();
        let rebon_dir = dir.path().join(".rebon");
        {
            let lock = try_acquire_scheduler_lock(&rebon_dir).unwrap();
            assert!(lock.is_some());
            // lock drops here
        }
        let next = try_acquire_scheduler_lock(&rebon_dir).unwrap();
        assert!(
            next.is_some(),
            "new process should be able to re-acquire after drop"
        );
    }

    #[test]
    fn explicit_release_works() {
        let dir = tmp_project();
        let rebon_dir = dir.path().join(".rebon");
        let first = try_acquire_scheduler_lock(&rebon_dir).unwrap().unwrap();
        first.release();
        let next = try_acquire_scheduler_lock(&rebon_dir).unwrap();
        assert!(next.is_some());
    }
}

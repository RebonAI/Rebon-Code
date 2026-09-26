//! The throwaway projects root `rebon exec --ephemeral` runs a session in.
//!
//! A run that leaves nothing in the store cannot be resumed afterwards, which
//! is the trade the flag makes — the same one `claude -p
//! --no-session-persistence` and `codex exec --ephemeral` make. What it buys
//! is a run that never reaches the chat lists: an eval harness driving `exec`
//! a thousand times should not be a thousand sessions somebody has to delete
//! by hand.
//!
//! It lives under the same temp root as the per-session scratchpads
//! ([`crate::scratchpad`]), because "temporary files this process owns" is one
//! convention and not two.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Separates two roots made in one process, which a pid alone cannot.
static NEXT_ROOT_SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// A projects root that goes away with the value that made it.
#[derive(Debug)]
pub struct EphemeralProjectsRoot {
    root: PathBuf,
}

impl EphemeralProjectsRoot {
    /// Make a fresh root under `$REBON_TMPDIR`, or the OS temp directory.
    ///
    /// A name already taken is skipped rather than reused: the only thing it
    /// can hold is an earlier process's leftovers under this pid, and mixing
    /// two runs' files in one directory is worse than a longer name.
    pub fn create() -> std::io::Result<Self> {
        let base = std::env::var_os("REBON_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("rebon");
        std::fs::create_dir_all(&base)?;
        loop {
            let root = base.join(format!(
                "ephemeral-{}-{}",
                std::process::id(),
                NEXT_ROOT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&root) {
                Ok(()) => return Ok(Self { root }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    /// The root session files land under:
    /// `<root>/<sanitised cwd>/<session id>.jsonl`.
    pub fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for EphemeralProjectsRoot {
    /// Best effort, like `remove_scratchpad_for`: a file another handle still
    /// has open on Windows is logged and the rest of the teardown carries on.
    /// What a failure leaves behind is a directory under the temp root —
    /// never a session in the store the flag promised not to touch.
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                path = %self.root.display(),
                %error,
                "could not remove the ephemeral projects root"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_is_made_under_the_temp_root_and_removed_with_the_guard() {
        let root = EphemeralProjectsRoot::create().expect("a root is made");
        let path = root.path().to_path_buf();
        assert!(path.is_dir(), "the root exists once create returns");
        assert!(
            path.to_string_lossy().contains("ephemeral-"),
            "the root is named for what it is: {path:?}"
        );
        // A caller writes into it exactly as it would into the real root, so
        // the store machinery needs nothing special to use it.
        std::fs::write(path.join("marker"), "x").unwrap();

        drop(root);
        assert!(!path.exists(), "dropping the guard removes the tree");
    }

    #[test]
    fn two_roots_in_one_process_are_never_the_same_directory() {
        let first = EphemeralProjectsRoot::create().unwrap();
        let second = EphemeralProjectsRoot::create().unwrap();
        assert_ne!(first.path(), second.path());
        assert!(first.path().is_dir() && second.path().is_dir());
    }

    /// The teardown a process that already lost the directory must survive:
    /// a second removal is not a failure to report.
    #[test]
    fn dropping_a_root_whose_directory_is_already_gone_is_not_an_error() {
        let root = EphemeralProjectsRoot::create().unwrap();
        std::fs::remove_dir_all(root.path()).unwrap();
        drop(root);
    }
}

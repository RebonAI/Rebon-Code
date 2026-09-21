//! Turn-scoped file-history arming for the prompt executor.
//!
//! `/rewind` / `/checkpoint` can only restore what was snapshotted, and a
//! snapshot only happens while the session's [`FileHistoryTracker`] is armed
//! with the turn's transcript user-row uuid. Historically only the TUI armed
//! the tracker, so every other prompt surface (background jobs driving the
//! desktop app, the standalone ACP server, headless runs) silently produced
//! zero snapshots — worst in non-git working directories, where background
//! jobs cannot use a worktree and mutate the real tree directly.
//!
//! The executor now owns arming: it begins the turn with the resolved user
//! uuid and ends it via a drop guard on every exit path (success, error,
//! cancellation). When the caller wired no tracker at all, a
//! [`SessionFileHistoryTracker`] scoped to `(projects_root, cwd, session_id)`
//! is built per turn so those surfaces snapshot into the same store the
//! rewind UIs read.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rebon_agent_core::file_history::FileHistoryTracker;

/// Session-scoped [`FileHistoryTracker`] used when the executor's caller did
/// not inject one. Mirrors the TUI's shared tracker: writes are tracked only
/// while a prompt turn is armed, and the post-turn head is recorded when the
/// turn ends.
pub struct SessionFileHistoryTracker {
    store: rebon_session::FileHistoryStore,
    current_message_id: Mutex<Option<String>>,
}

impl SessionFileHistoryTracker {
    pub fn new(store: rebon_session::FileHistoryStore) -> Self {
        Self {
            store,
            current_message_id: Mutex::new(None),
        }
    }
}

impl FileHistoryTracker for SessionFileHistoryTracker {
    fn track_before_write(&self, file_path: &Path) -> anyhow::Result<()> {
        let guard = self
            .current_message_id
            .lock()
            .map_err(|_| anyhow::anyhow!("file-history tracker lock poisoned"))?;
        let Some(message_id) = guard.as_deref() else {
            return Ok(());
        };
        // Hold the lock through the store transaction like the TUI tracker
        // does; the store also takes an interprocess lock for the manifest.
        self.store.track_before_write(file_path, message_id)
    }

    fn begin_prompt_turn(&self, message_id: &str) -> anyhow::Result<()> {
        let mut guard = self
            .current_message_id
            .lock()
            .map_err(|_| anyhow::anyhow!("file-history tracker lock poisoned"))?;
        if guard.as_deref() == Some(message_id) {
            return Ok(());
        }
        // A still-armed previous turn means it never ended cleanly; seal its
        // head before arming the new turn so its edits stay rewindable.
        if let Some(previous) = guard.take() {
            if let Err(error) = self.store.record_current_head(&previous) {
                tracing::warn!(
                    error = %error,
                    message_id = %previous,
                    "rebon: failed to record file-history head for unsealed turn"
                );
            }
        }
        self.store.make_snapshot(message_id)?;
        *guard = Some(message_id.to_string());
        Ok(())
    }

    fn end_prompt_turn(&self, message_id: &str) -> anyhow::Result<()> {
        let mut guard = self
            .current_message_id
            .lock()
            .map_err(|_| anyhow::anyhow!("file-history tracker lock poisoned"))?;
        if guard.as_deref() != Some(message_id) {
            return Ok(());
        }
        *guard = None;
        self.store.record_current_head(message_id)
    }
}

/// Ends the armed prompt turn on drop so the post-turn head is recorded on
/// every exit path of `execute()` — success, error, and cancellation alike.
pub struct FileHistoryTurnGuard {
    tracker: Arc<dyn FileHistoryTracker>,
    message_id: String,
}

impl FileHistoryTurnGuard {
    /// Arm `tracker` for the turn identified by `message_id` and return the
    /// guard that will seal it. Arming failures are logged, not fatal: a
    /// broken snapshot store must never block the prompt itself.
    pub fn begin(tracker: Arc<dyn FileHistoryTracker>, message_id: &str) -> Self {
        if let Err(error) = tracker.begin_prompt_turn(message_id) {
            tracing::warn!(
                error = %error,
                message_id = %message_id,
                "rebon: failed to create file-history snapshot for prompt turn"
            );
        }
        Self {
            tracker,
            message_id: message_id.to_string(),
        }
    }
}

impl Drop for FileHistoryTurnGuard {
    fn drop(&mut self) {
        if let Err(error) = self.tracker.end_prompt_turn(&self.message_id) {
            tracing::warn!(
                error = %error,
                message_id = %self.message_id,
                "rebon: failed to record file-history head for completed turn"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn temp_dir() -> TempDir {
        tempfile::Builder::new()
            .prefix("rebon-core-file-history-turn-")
            .tempdir()
            .unwrap()
    }

    fn tracker(temp: &TempDir) -> (SessionFileHistoryTracker, PathBuf) {
        let projects = temp.path().join("projects");
        let cwd = temp.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        let store = rebon_session::FileHistoryStore::new(&projects, &cwd, "sess-turn");
        (SessionFileHistoryTracker::new(store), cwd)
    }

    fn manifest(temp: &TempDir, cwd: &Path) -> rebon_session::FileHistoryManifest {
        rebon_session::FileHistoryStore::new(temp.path().join("projects"), cwd, "sess-turn")
            .load_manifest()
            .expect("manifest exists")
    }

    #[test]
    fn turn_lifecycle_snapshots_and_records_head() {
        let temp = temp_dir();
        let (tracker, cwd) = tracker(&temp);
        let file = cwd.join("a.txt");
        fs::write(&file, "one").unwrap();

        tracker.begin_prompt_turn("u-1").unwrap();
        tracker.track_before_write(&file).unwrap();
        fs::write(&file, "two").unwrap();
        tracker.end_prompt_turn("u-1").unwrap();

        let manifest = manifest(&temp, &cwd);
        assert!(manifest
            .snapshots
            .iter()
            .any(|snapshot| snapshot.message_id == "u-1"));
        assert_eq!(
            manifest
                .current_head
                .as_ref()
                .map(|head| head.message_id.as_str()),
            Some("u-1")
        );
    }

    #[test]
    fn begin_is_idempotent_for_same_turn() {
        let temp = temp_dir();
        let (tracker, cwd) = tracker(&temp);

        tracker.begin_prompt_turn("u-1").unwrap();
        tracker.begin_prompt_turn("u-1").unwrap();

        let manifest = manifest(&temp, &cwd);
        assert_eq!(
            manifest
                .snapshots
                .iter()
                .filter(|snapshot| snapshot.message_id == "u-1")
                .count(),
            1
        );
    }

    #[test]
    fn end_for_stale_turn_is_ignored_after_rearm() {
        let temp = temp_dir();
        let (tracker, cwd) = tracker(&temp);

        tracker.begin_prompt_turn("u-1").unwrap();
        // A new turn arms before the old turn's guard unwinds.
        tracker.begin_prompt_turn("u-2").unwrap();
        tracker.end_prompt_turn("u-1").unwrap();

        // The stale end must not have sealed/cleared the live turn.
        let before = manifest(&temp, &cwd);
        assert_eq!(
            before
                .current_head
                .as_ref()
                .map(|head| head.message_id.as_str()),
            Some("u-1"),
            "re-arm seals the unsealed previous turn, not the live one"
        );
        tracker.end_prompt_turn("u-2").unwrap();
        let after = manifest(&temp, &cwd);
        assert_eq!(
            after
                .current_head
                .as_ref()
                .map(|head| head.message_id.as_str()),
            Some("u-2")
        );
    }

    #[test]
    fn tracking_without_armed_turn_is_a_noop() {
        let temp = temp_dir();
        let (tracker, cwd) = tracker(&temp);
        let file = cwd.join("b.txt");
        fs::write(&file, "x").unwrap();

        tracker.track_before_write(&file).unwrap();

        let store =
            rebon_session::FileHistoryStore::new(temp.path().join("projects"), &cwd, "sess-turn");
        assert!(matches!(
            store.load_manifest(),
            Err(rebon_session::FileHistoryLoadError::Missing)
        ));
    }

    #[test]
    fn guard_seals_turn_on_drop() {
        let temp = temp_dir();
        let (tracker, cwd) = tracker(&temp);
        let tracker: Arc<dyn FileHistoryTracker> = Arc::new(tracker);

        {
            let _guard = FileHistoryTurnGuard::begin(tracker.clone(), "u-1");
        }

        let manifest = manifest(&temp, &cwd);
        assert_eq!(
            manifest
                .current_head
                .as_ref()
                .map(|head| head.message_id.as_str()),
            Some("u-1")
        );
    }
}

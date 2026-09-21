//! Shared turn snapshots and the file-history contract used by tools and agents.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub trait FileHistoryTracker: Send + Sync {
    fn track_before_write(&self, file_path: &Path) -> anyhow::Result<()>;

    /// Arm the tracker for the prompt turn whose transcript user row is
    /// `message_id`: capture the pre-turn snapshot so the turn can be
    /// rewound later. Must be idempotent when re-invoked with the id the
    /// tracker is already armed for (the TUI arms before the executor does).
    fn begin_prompt_turn(&self, _message_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Record the authoritative post-turn head for `message_id` and disarm.
    /// A tracker that has already been re-armed for a newer turn must treat
    /// this as a no-op so a late-unwinding turn cannot clobber its successor.
    fn end_prompt_turn(&self, _message_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Whether a turn is currently armed. `track_before_write` outside
    /// a turn quietly succeeds without capturing anything, so a caller
    /// that wants to warn about an unrewindable write needs this probe.
    /// `None` means the tracker cannot tell — treat as "assume armed".
    fn is_armed(&self) -> Option<bool> {
        None
    }
}

#[derive(Clone)]
pub struct SharedFileHistoryTracker {
    inner: Arc<Mutex<SharedFileHistoryTrackerState>>,
}

struct SharedFileHistoryTrackerState {
    store: rebon_session::FileHistoryStore,
    current_message_id: Option<String>,
}

impl SharedFileHistoryTracker {
    pub fn new(store: rebon_session::FileHistoryStore) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedFileHistoryTrackerState {
                store,
                current_message_id: None,
            })),
        }
    }

    pub fn set_current_message_id(&self, message_id: impl Into<String>) {
        let message_id = message_id.into();
        if let Err(err) = self.make_snapshot(&message_id) {
            tracing::warn!(
                error = %err,
                message_id = %message_id,
                "rebon: failed to create file-history snapshot for prompt"
            );
        }
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.current_message_id = Some(message_id);
        }
    }

    pub fn clear_current_message_id(&self) {
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(message_id) = guard.current_message_id.take() {
                if let Err(err) = guard.store.record_current_head(&message_id) {
                    tracing::warn!(
                        error = %err,
                        message_id = %message_id,
                        "rebon: failed to record completed file-history head"
                    );
                }
            }
        }
    }

    pub fn make_snapshot(&self, message_id: &str) -> anyhow::Result<()> {
        let store = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("file-history tracker lock poisoned"))?
            .store
            .clone();
        store.make_snapshot(message_id)
    }

    pub fn store(&self) -> rebon_session::FileHistoryStore {
        self.inner
            .lock()
            .expect("file-history tracker lock poisoned")
            .store
            .clone()
    }
}

impl FileHistoryTracker for SharedFileHistoryTracker {
    fn track_before_write(&self, file_path: &Path) -> anyhow::Result<()> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("file-history tracker lock poisoned"))?;
        let Some(message_id) = guard.current_message_id.as_deref() else {
            return Ok(());
        };
        // Keep the tracker lock through the store transaction. The store also
        // takes an interprocess lock, so cloned trackers and separate processes
        // cannot lose one another's manifest updates.
        guard.store.track_before_write(file_path, message_id)
    }

    fn is_armed(&self) -> Option<bool> {
        self.inner
            .lock()
            .ok()
            .map(|guard| guard.current_message_id.is_some())
    }

    fn begin_prompt_turn(&self, message_id: &str) -> anyhow::Result<()> {
        // The TUI arms this tracker itself before dispatching the turn, in
        // which case the executor's begin call must be a no-op. Prompt
        // surfaces that skip the TUI arming (background worker, headless)
        // arm here instead.
        {
            let guard = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("file-history tracker lock poisoned"))?;
            if guard.current_message_id.as_deref() == Some(message_id) {
                return Ok(());
            }
        }
        // Seals a still-armed previous turn (recording its head) before
        // arming the new one — same sequence the TUI performs explicitly.
        self.clear_current_message_id();
        self.set_current_message_id(message_id);
        Ok(())
    }

    fn end_prompt_turn(&self, message_id: &str) -> anyhow::Result<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("file-history tracker lock poisoned"))?;
        // Only seal the turn this call belongs to. If the TUI already
        // recorded the head (interrupt flow) or re-armed for a newer turn,
        // a late-unwinding executor must not disturb that state.
        if guard.current_message_id.as_deref() != Some(message_id) {
            return Ok(());
        }
        guard.current_message_id = None;
        guard.store.record_current_head(message_id)
    }
}

/// A [`FileHistoryTracker`] whose real
/// target is bound after construction. The sub-agent spawner is built
/// before the session id (and therefore the real tracker) is known, so
/// it takes one of these placeholders; wiring populates it once the
/// session tracker exists. Until then `track_before_write` is a no-op,
/// matching the pre-binding tracker's own no-op-until-armed behaviour.
#[derive(Clone, Default)]
pub struct DeferredFileHistoryTracker {
    inner: Arc<Mutex<Option<SharedFileHistoryTracker>>>,
}

impl DeferredFileHistoryTracker {
    pub fn bind(&self, tracker: SharedFileHistoryTracker) {
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(tracker);
        }
    }

    fn bound(&self) -> anyhow::Result<Option<SharedFileHistoryTracker>> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("deferred file-history tracker lock poisoned"))?
            .clone())
    }
}

impl FileHistoryTracker for DeferredFileHistoryTracker {
    fn track_before_write(&self, file_path: &Path) -> anyhow::Result<()> {
        match self.bound()? {
            Some(tracker) => FileHistoryTracker::track_before_write(&tracker, file_path),
            None => Ok(()),
        }
    }

    fn begin_prompt_turn(&self, message_id: &str) -> anyhow::Result<()> {
        match self.bound()? {
            Some(tracker) => FileHistoryTracker::begin_prompt_turn(&tracker, message_id),
            None => Ok(()),
        }
    }

    fn end_prompt_turn(&self, message_id: &str) -> anyhow::Result<()> {
        match self.bound()? {
            Some(tracker) => FileHistoryTracker::end_prompt_turn(&tracker, message_id),
            None => Ok(()),
        }
    }
}

/// A tracker over the system temp directory, for tests that need one and do
/// not care where it writes.
///
/// Not gated on `cfg(test)`: another crate's test support builds sessions with
/// it, and a `cfg(test)` item is invisible to another crate's tests.
#[doc(hidden)]
pub fn submit_tracker(session_id: &str) -> SharedFileHistoryTracker {
    SharedFileHistoryTracker::new(rebon_session::FileHistoryStore::new(
        std::env::temp_dir(),
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        session_id,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_tracker(
        tag: &str,
    ) -> (
        tempfile::TempDir,
        SharedFileHistoryTracker,
        PathBuf,
        PathBuf,
    ) {
        let root = tempfile::Builder::new()
            .prefix(&format!("rebon-shared-tracker-{tag}-"))
            .tempdir()
            .unwrap();
        let projects = root.path().join("projects");
        let cwd = root.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let tracker = SharedFileHistoryTracker::new(rebon_session::FileHistoryStore::new(
            &projects, &cwd, "sess-1",
        ));
        (root, tracker, projects, cwd)
    }

    fn head_id(projects: &Path, cwd: &Path) -> Option<String> {
        rebon_session::FileHistoryStore::new(projects, cwd, "sess-1")
            .load_manifest()
            .ok()?
            .current_head
            .map(|head| head.message_id)
    }

    /// The executor's begin call must be a no-op when the TUI already
    /// armed the same turn — no duplicate snapshot, no premature head.
    #[test]
    fn executor_begin_after_tui_arm_is_idempotent() {
        let (_root, tracker, projects, cwd) = temp_tracker("idempotent");

        tracker.set_current_message_id("u-1");
        tracker.begin_prompt_turn("u-1").unwrap();

        let manifest = rebon_session::FileHistoryStore::new(&projects, &cwd, "sess-1")
            .load_manifest()
            .unwrap();
        assert_eq!(
            manifest
                .snapshots
                .iter()
                .filter(|snapshot| snapshot.message_id == "u-1")
                .count(),
            1
        );
        assert!(head_id(&projects, &cwd).is_none());
    }

    /// A background-worker style turn (no TUI arming) is begun and sealed
    /// entirely through the trait hooks.
    #[test]
    fn hook_only_turn_snapshots_and_seals_head() {
        let (_root, tracker, projects, cwd) = temp_tracker("hooks");
        let file = cwd.join("demo.txt");
        std::fs::write(&file, "one").unwrap();

        tracker.begin_prompt_turn("u-1").unwrap();
        tracker.track_before_write(&file).unwrap();
        std::fs::write(&file, "two").unwrap();
        tracker.end_prompt_turn("u-1").unwrap();

        assert_eq!(head_id(&projects, &cwd).as_deref(), Some("u-1"));
        let store = rebon_session::FileHistoryStore::new(&projects, &cwd, "sess-1");
        assert!(matches!(
            store.restore_capability("u-1"),
            rebon_session::FileRestoreCapability::Clean(_)
        ));
    }

    /// A late-unwinding executor guard must not seal a newer turn the TUI
    /// (or the next background prompt) already armed.
    #[test]
    fn stale_end_does_not_disturb_newer_turn() {
        let (_root, tracker, projects, cwd) = temp_tracker("stale-end");

        tracker.begin_prompt_turn("u-1").unwrap();
        // Interrupt: the TUI seals u-1 and arms the next turn.
        tracker.clear_current_message_id();
        tracker.set_current_message_id("u-2");
        // The old turn's guard unwinds late.
        tracker.end_prompt_turn("u-1").unwrap();

        assert_eq!(head_id(&projects, &cwd).as_deref(), Some("u-1"));
        tracker.end_prompt_turn("u-2").unwrap();
        assert_eq!(head_id(&projects, &cwd).as_deref(), Some("u-2"));
    }

    #[test]
    fn poisoned_tracker_fails_closed_but_teardown_recovers() {
        let (_root, tracker, _projects, cwd) = temp_tracker("poison");
        tracker.begin_prompt_turn("u-1").unwrap();
        let poisoned = tracker.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.inner.lock().unwrap();
            panic!("simulate a writer unwinding while holding the tracker lock");
        })
        .join();
        assert_eq!(tracker.is_armed(), None);
        for result in [
            tracker.make_snapshot("u-2"),
            tracker.track_before_write(&cwd.join("untouched.txt")),
            tracker.begin_prompt_turn("u-2"),
            tracker.end_prompt_turn("u-1"),
        ] {
            assert!(result.unwrap_err().to_string().contains("lock poisoned"));
        }
        tracker.clear_current_message_id();
        assert!(tracker
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current_message_id
            .is_none());
    }

    /// An unbound deferred tracker ignores every hook instead of erroring.
    #[test]
    fn unbound_deferred_tracker_is_noop() {
        let deferred = DeferredFileHistoryTracker::default();
        deferred.begin_prompt_turn("u-1").unwrap();
        deferred
            .track_before_write(Path::new("does-not-matter.txt"))
            .unwrap();
        deferred.end_prompt_turn("u-1").unwrap();
    }

    /// Once bound, the deferred tracker forwards the turn hooks to the
    /// session tracker so sub-agent writes join the same snapshots.
    #[test]
    fn bound_deferred_tracker_forwards_hooks() {
        let (_root, tracker, projects, cwd) = temp_tracker("deferred");
        let deferred = DeferredFileHistoryTracker::default();
        deferred.bind(tracker);

        deferred.begin_prompt_turn("u-1").unwrap();
        deferred.end_prompt_turn("u-1").unwrap();

        assert_eq!(head_id(&projects, &cwd).as_deref(), Some("u-1"));
    }
}

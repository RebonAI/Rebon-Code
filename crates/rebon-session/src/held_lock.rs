//! The session lock a caller already holds, handed to a session builder
//! instead of being taken again.
//!
//! The lock it wraps ([`crate::SessionActiveLock`]) is this crate's, so the
//! wrapper belongs beside it instead of in a background host that would then
//! have to reach across crates for a session-ownership type.

/// A session lock the caller already holds, handed to the builder instead of
/// being taken again.
///
/// A worker outlives any one of its sessions: it builds one per turn and drops
/// it when the turn ends. If the lock went with the session, ownership would
/// exist only while a turn was running — the session would read as free to
/// every other process for as long as the worker sat idle, which is most of
/// its life. So the worker keeps the lock across turns and lends it back here.
///
/// The session id travels with it because a lock value cannot say which
/// session it belongs to. A lock offered for a different session is dropped
/// and the normal acquisition runs, rather than being trusted.
pub struct HeldSessionLock {
    pub session_id: String,
    pub lock: crate::SessionActiveLock,
}

impl HeldSessionLock {
    /// The lock, if it is the one this session needs.
    pub fn take_for(held: Option<Self>, session_id: &str) -> Option<crate::SessionActiveLock> {
        let held = held?;
        if held.session_id == session_id {
            Some(held.lock)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held_lock(session_id: &str) -> (tempfile::TempDir, HeldSessionLock) {
        let dir = tempfile::tempdir().unwrap();
        let lock = crate::try_acquire_session_active_lock(dir.path(), "/work", session_id)
            .unwrap()
            .expect("a fresh session is free");
        (
            dir,
            HeldSessionLock {
                session_id: session_id.to_string(),
                lock,
            },
        )
    }

    /// A worker resuming its own session between turns is the caller this
    /// exists for: the in-process registry answers "held" to the holder itself,
    /// so re-acquiring would fail against a lock this process already owns.
    #[test]
    fn a_held_lock_is_reused_for_the_same_session() {
        let (dir, held) = held_lock("sess-1");
        assert!(
            crate::try_acquire_session_active_lock(dir.path(), "/work", "sess-1")
                .unwrap()
                .is_none(),
            "the same process cannot take a lock it already holds — hence the hand-back"
        );

        let reused = HeldSessionLock::take_for(Some(held), "sess-1");

        assert!(reused.is_some(), "the session gets its own lock back");
    }

    /// A lock cannot say which session it belongs to, so one offered for a
    /// different session is dropped rather than trusted — which frees it, and
    /// lets the acquisition for the session actually being built proceed.
    #[test]
    fn a_held_lock_for_another_session_is_dropped_rather_than_reused() {
        let (dir, held) = held_lock("sess-1");

        let reused = HeldSessionLock::take_for(Some(held), "sess-2");

        assert!(reused.is_none(), "a lock for another session is not lent");
        assert!(
            crate::try_acquire_session_active_lock(dir.path(), "/work", "sess-1")
                .unwrap()
                .is_some(),
            "dropping it released it, so sess-1 is free again"
        );
    }

    #[test]
    fn no_held_lock_means_the_builder_acquires_one() {
        assert!(HeldSessionLock::take_for(None, "sess-1").is_none());
    }
}

//! The seat a front end reads the startup update check off.
//!
//! Who starts the check is the point of this module. It used to be the
//! terminal: `run_blocking` spawned a task before the event loop and drained a
//! `oneshot` every frame, so "does this build check for updates" was a
//! property of having a terminal rather than a property of the feature being
//! on, and the receiver had to be threaded through eleven signatures to reach
//! the drain. The plugin owns the check now, and **reading the seat is what
//! starts it**: [`UpdateCheckSeat::poll`] starts one on its first call, once
//! per process, and answers every call after that.
//!
//! Lazily, because the only caller that will ever look at the answer is a
//! front end that can draw a notice. Starting on the session-open path instead
//! would have put an npm request in front of every `rebon exec` in an eval
//! batch, every ACP session and every background worker — none of which
//! display anything — for a result all of them drop.
//!
//! That leaves the switch meaning what it says. With
//! `plugins.updater.enabled = false` the plugin never applies, this service is
//! never provided, so there is no seat to poll, nothing calls `start_once`, no
//! request goes to npm, no notice appears, and `/update` is not on the command
//! seat either.
//!
//! The receiver rather than a plain slot is deliberate. A spawned task that
//! ends without answering — a panic, a runtime shutting down mid-flight — is a
//! state the front end has to be able to tell apart from "still waiting", or a
//! user-requested check would hang on a spinner forever.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;

use crate::check::{check_for_update, UpdateCheckResult};

/// Stable typed service name for the process's update check.
pub const UPDATE_CHECK_SERVICE: &str = "update-check";

/// The typed service definition front ends resolve from the kernel root.
pub struct UpdateCheckService;

impl rebon_kernel::Service for UpdateCheckService {
    type Interface = UpdateCheckSeat;
    const NAME: &'static str = UPDATE_CHECK_SERVICE;
}

/// What a poll of the startup check found.
#[derive(Debug)]
pub enum UpdateCheckPoll {
    /// Nothing outstanding: no check was started, or its answer was already
    /// taken. The steady state for all but a few frames of a session.
    Idle,
    /// A check is in flight; ask again later.
    Pending,
    /// The check answered — with a result, or with the error it failed on.
    Ready(anyhow::Result<UpdateCheckResult>),
    /// The task ended without answering.
    Cancelled,
}

/// The process's one startup update check.
pub struct UpdateCheckSeat {
    /// Set the first time [`UpdateCheckSeat::poll`] decides whether to run a
    /// check, and never cleared — including when the decision is "no", so a
    /// terminal drawing sixty frames a second does not re-decide, and
    /// re-log, on every one of them.
    decided: AtomicBool,
    /// The in-flight check's answer, until a front end takes it.
    pending: Mutex<Option<oneshot::Receiver<anyhow::Result<UpdateCheckResult>>>>,
}

impl Default for UpdateCheckSeat {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdateCheckSeat {
    pub fn new() -> Self {
        Self {
            decided: AtomicBool::new(false),
            pending: Mutex::new(None),
        }
    }

    /// Start the process's update check, unless that decision has been made
    /// already.
    ///
    /// Returns whether this call is the one that put a request in flight —
    /// what the tests assert on. A caller has nothing to do with the answer;
    /// it arrives through [`UpdateCheckSeat::poll`].
    fn start_once(&self) -> bool {
        if self.decided.swap(true, Ordering::AcqRel) {
            return false;
        }
        if env!("CARGO_PKG_VERSION") == "0.0.1" {
            tracing::debug!("updater: skipping update check for dev build");
            return false;
        }
        // The check is a network request, so it needs a runtime to run on. The
        // terminal polls from its blocking runner, which has one; a test that
        // polls without a runtime gets a debug line rather than a panic, and
        // the seat stays in its "nothing outstanding" state.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::debug!("updater: no tokio runtime to run the update check on");
            return false;
        };
        let (tx, rx) = oneshot::channel();
        handle.spawn(async move {
            let _ = tx.send(check_for_update().await);
        });
        *self.pending.lock().expect("update-check slot poisoned") = Some(rx);
        true
    }

    /// Whether this seat has already decided whether to run a check.
    pub fn decided(&self) -> bool {
        self.decided.load(Ordering::Acquire)
    }

    /// Ask whether the startup check has answered, starting one on the first
    /// call. Draining is the caller's: a [`UpdateCheckPoll::Ready`] or
    /// [`UpdateCheckPoll::Cancelled`] is handed over once and the seat goes
    /// back to [`UpdateCheckPoll::Idle`].
    pub fn poll(&self) -> UpdateCheckPoll {
        // Before the lock, not inside it: `start_once` takes the same one.
        self.start_once();
        let mut slot = self.pending.lock().expect("update-check slot poisoned");
        let Some(rx) = slot.as_mut() else {
            return UpdateCheckPoll::Idle;
        };
        match rx.try_recv() {
            Ok(result) => {
                *slot = None;
                UpdateCheckPoll::Ready(result)
            }
            Err(TryRecvError::Empty) => UpdateCheckPoll::Pending,
            Err(TryRecvError::Closed) => {
                *slot = None;
                UpdateCheckPoll::Cancelled
            }
        }
    }

    /// Run one check now, for a user who asked for it rather than for startup.
    ///
    /// Separate from the startup slot on purpose: `/update check` reports its
    /// own outcome inline, and must not consume — or be consumed by — an
    /// answer the startup check is still waiting on.
    pub async fn check_now(&self) -> anyhow::Result<UpdateCheckResult> {
        check_for_update().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The terminal's first drain is what asks npm, and every drain after it
    /// only reads: one request per process, however many frames follow.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_first_drain_starts_the_check_and_later_drains_do_not() {
        let seat = UpdateCheckSeat::new();
        assert!(!seat.decided(), "an unread seat has asked nobody anything");

        assert!(
            matches!(seat.poll(), UpdateCheckPoll::Pending),
            "the first drain puts a request in flight"
        );
        assert!(seat.decided());

        for _ in 0..3 {
            assert!(
                matches!(seat.poll(), UpdateCheckPoll::Pending),
                "later drains read the same request"
            );
        }
        assert!(
            !seat.start_once(),
            "no drain after the first sends a second request"
        );
    }

    /// Nothing polls a seat nobody resolved, so nothing decides anything: a
    /// process that never draws a notice — `rebon exec`, an ACP session, a
    /// background worker — never reaches the registry.
    ///
    /// Written without a runtime on purpose. That is also the branch where
    /// there is nowhere to run the request, and the seat answers by staying
    /// idle rather than panicking.
    #[test]
    fn a_seat_nobody_polls_asks_nothing_and_polling_without_a_runtime_stays_idle() {
        let seat = UpdateCheckSeat::new();
        assert!(!seat.decided());

        assert!(matches!(seat.poll(), UpdateCheckPoll::Idle));
        assert!(
            seat.decided(),
            "the decision is made once, even when it is no"
        );
        assert!(
            !seat.start_once(),
            "and it is not re-made on the next frame"
        );
    }

    /// A task that ends without answering is `Cancelled`, not `Pending`
    /// forever — the distinction a waiting front end needs.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_sender_polls_as_cancelled_then_idle() {
        let seat = UpdateCheckSeat::new();
        // Stand in for the request the first poll would have sent, so this
        // test drives the receiver it was handed rather than a real one.
        seat.decided.store(true, Ordering::Release);
        let (tx, rx) = oneshot::channel();
        *seat.pending.lock().expect("fresh seat") = Some(rx);
        assert!(matches!(seat.poll(), UpdateCheckPoll::Pending));
        drop(tx);
        assert!(matches!(seat.poll(), UpdateCheckPoll::Cancelled));
        assert!(matches!(seat.poll(), UpdateCheckPoll::Idle));
    }
}

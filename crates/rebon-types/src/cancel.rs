//! Cancellation primitive shared between the ACP server and the prompt executor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

/// Cancellation primitive shared between the ACP server and the
/// prompt executor.
///
/// One handle observes cancellation; every clone points at the same
/// underlying flag, and `cancel()` wakes every task that's currently
/// awaiting [`Self::notified`].
#[derive(Debug, Clone, Default)]
pub struct PromptCancel {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl PromptCancel {
    /// Create a fresh, un-cancelled handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Trigger cancellation. Idempotent — subsequent calls are a
    /// no-op. Wakes every task awaiting [`Self::notified`].
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Await the cancel signal. Resolves immediately if the token is
    /// already cancelled; otherwise waits for the next [`Self::cancel`]
    /// call.
    pub async fn notified(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);

        // Register before checking the flag so notify_waiters cannot land
        // between the check and waiter registration.
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn notified_resolves_when_cancelled_before_first_poll() {
        let cancel = PromptCancel::new();
        let notified = cancel.notified();

        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(1), notified)
            .await
            .expect("pre-cancelled waiter did not resolve");
    }

    #[tokio::test]
    async fn notified_resolves_when_cancelled_after_waiter_registration() {
        let cancel = PromptCancel::new();
        let waiter_cancel = cancel.clone();
        let waiter = tokio::spawn(async move {
            waiter_cancel.notified().await;
        });

        tokio::task::yield_now().await;
        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("registered waiter did not resolve")
            .expect("waiter task panicked");
    }

    #[tokio::test]
    async fn cancel_wakes_every_registered_waiter() {
        let cancel = PromptCancel::new();
        let mut waiters = Vec::new();
        for _ in 0..8 {
            let waiter_cancel = cancel.clone();
            waiters.push(tokio::spawn(async move {
                waiter_cancel.notified().await;
            }));
        }

        tokio::task::yield_now().await;
        cancel.cancel();

        for waiter in waiters {
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("registered waiter did not resolve")
                .expect("waiter task panicked");
        }
    }
}

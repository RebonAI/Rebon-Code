//! Work leases and the long poll.
//!
//! A leased item belongs to exactly one worker for `lease_ttl`. The
//! worker extends it with heartbeats; if it stops heartbeating — the
//! machine slept, the process died, the network went — the 1 Hz sweeper
//! in [`crate::RcState::cleanup`] returns the item to `ready` and wakes
//! whoever is polling that environment.
//!
//! The wait itself is a `Notify` per environment rather than a poll
//! interval, so enqueued work reaches a waiting bridge immediately.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::response::Response;

use crate::{auth, db, ids, store::ClaimedWork, RcState};

/// Lease lifetime in whole seconds, as the store stores it.
pub fn lease_seconds(state: &RcState) -> i64 {
    state.config().lease_ttl.as_secs().max(1) as i64
}

/// Wait up to `wait` for a work item on `environment_id`.
///
/// Returns the claimed item together with the plaintext session token
/// minted for it — the only moment that token exists outside its
/// digest. `Ok(None)` means the poll timed out with an empty queue,
/// which the route turns into a 204.
pub async fn await_work(
    state: &Arc<RcState>,
    environment_id: &str,
    reclaim_older_than_ms: Option<u64>,
    wait: Duration,
) -> Result<Option<(ClaimedWork, String)>, Response> {
    let waiter = state.waiter(environment_id);
    let deadline = Instant::now() + wait;
    loop {
        // Arm the wakeup *before* checking the queue. `notify_waiters`
        // stores no permit, so a producer that fires between the check
        // and the wait would otherwise be lost and the poller would
        // sleep on work that is already queued.
        let notified = waiter.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        if let Some(claim) = claim_once(state, environment_id, reclaim_older_than_ms).await? {
            return Ok(Some(claim));
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        tokio::select! {
            _ = &mut notified => {}
            _ = tokio::time::sleep(remaining) => return Ok(None),
        }
        // A wakeup is only a hint: several pollers wake together and the
        // claim transaction picks one winner, so the losers loop round
        // and wait out the rest of their budget.
    }
}

/// One claim attempt. Mints a candidate session token up front and
/// discards it when the queue turns out to be empty; generating 32
/// random bytes is cheaper than a second database round trip.
async fn claim_once(
    state: &Arc<RcState>,
    environment_id: &str,
    reclaim_older_than_ms: Option<u64>,
) -> Result<Option<(ClaimedWork, String)>, Response> {
    let session_token = ids::generate_token();
    let digest = state.digest(auth::DOMAIN_SESSION, &session_token);
    let environment_id = environment_id.to_string();
    let lease = lease_seconds(state);
    let now_unix = ids::now_unix();
    let claimed = db(state, move |state| {
        state.store().claim_work(
            &environment_id,
            &digest,
            reclaim_older_than_ms,
            lease,
            now_unix,
        )
    })
    .await?;
    Ok(claimed.map(|claim| (claim, session_token)))
}

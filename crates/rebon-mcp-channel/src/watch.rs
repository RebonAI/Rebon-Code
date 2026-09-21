//! The watcher: notice when a tracked job needs telling about, and push it.
//!
//! Bounded polling of `state.json`, which is the authority (RFC-0007 §6.4,
//! option 1). The wait is real — a job finishing is an event in another
//! process with no channel back to this one — and the file is what every
//! other endpoint reads too. Only jobs this server tracks are read, at a
//! short cadence while any is active and backing off to a slow one while
//! none is, and waking early when a job is added. A supervisor subscription
//! could replace the loop later without changing what is pushed.
//!
//! Nothing is pushed from memory of an earlier tick: each tick derives what
//! is worth telling from the job's current record and the ledger's list of
//! what was already told, so a restart, a missed intermediate state or a
//! second server all come out the same.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::jobs::Desk;
use crate::push::{self, Throttle, Update};
use crate::server::Outbox;

/// How often the watcher looks.
#[derive(Debug, Clone, Copy)]
pub struct Cadence {
    /// Before the first look: the client has only just registered for pushes.
    pub first: Duration,
    /// While any tracked job is queued, running or parked.
    pub active: Duration,
    /// The ceiling the interval doubles towards while nothing is active.
    pub idle_max: Duration,
}

impl Default for Cadence {
    fn default() -> Self {
        Self {
            first: Duration::from_secs(1),
            active: Duration::from_secs(2),
            idle_max: Duration::from_secs(30),
        }
    }
}

/// What one tick found across every tracked job.
#[derive(Debug, Default)]
pub(crate) struct Tick {
    pub updates: Vec<Update>,
    pub any_active: bool,
}

impl Desk {
    /// Look at every tracked job once. Blocking.
    pub(crate) fn observe_tracked(&self) -> Tick {
        let mut tick = Tick::default();
        for job_id in self.tracked() {
            let observation = self.observe(&job_id);
            if observation.gone {
                self.untrack(&job_id);
                continue;
            }
            tick.any_active |= observation.active;
            tick.updates.extend(observation.update);
        }
        tick
    }
}

/// Run until the connection closes. Pushes go to `outbox`; a closed outbox
/// is the connection gone, and ends the loop.
pub(crate) async fn run(desk: Arc<Desk>, outbox: Outbox, cadence: Cadence) {
    let mut throttle = Throttle::new(push::PUSHES_PER_WINDOW, push::PUSH_WINDOW);
    let mut interval = cadence.first;
    loop {
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = desk.woken.notified() => {}
        }
        let observer = Arc::clone(&desk);
        let tick = match tokio::task::spawn_blocking(move || observer.observe_tracked()).await {
            Ok(tick) => tick,
            Err(error) => {
                tracing::warn!(%error, "rebon mcp: a watcher tick did not finish");
                Tick::default()
            }
        };
        for update in tick.updates {
            throttle.offer(update);
        }
        if let Some(batch) = throttle.drain(Instant::now()) {
            let claimer = Arc::clone(&desk);
            let now_ms = rebon_types::wall_clock_ms();
            let survivors =
                match tokio::task::spawn_blocking(move || claimer.claim(batch, now_ms)).await {
                    Ok(survivors) => survivors,
                    Err(error) => {
                        tracing::warn!(%error, "rebon mcp: recording a push did not finish");
                        Vec::new()
                    }
                };
            if let Some(message) = push::render(&survivors) {
                if outbox.send(message.to_notification()).is_err() {
                    return;
                }
            }
        }
        interval = if tick.any_active || throttle.is_waiting() {
            cadence.active
        } else {
            (interval * 2).min(cadence.idle_max).max(cadence.active)
        };
    }
}

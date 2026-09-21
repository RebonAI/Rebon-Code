//! The live topology of a session stream: one worker, many controllers.
//!
//! Everything durable about a session is in SQLite. What lives here is
//! only who is *attached right now* and where to put a frame — the same
//! division `relay-server` makes between its persisted pairings and its
//! in-memory rooms.
//!
//! ## One worker, many controllers
//!
//! A session is run by exactly one worker, so a second worker connection
//! supersedes the first rather than being refused: the worker that just
//! polled the queue and holds the live lease is the one that should own
//! the socket, and refusing it would strand the session behind a
//! connection whose process may already be gone. The displaced socket is
//! closed with [`close_code::SUPERSEDED`] so its runner can tell that
//! apart from a network drop. Controllers have no such limit.
//!
//! ## Arrival order
//!
//! Controllers write concurrently, each from its own socket task, and a
//! frame is stored before it is routed. Two tasks interleaving those two
//! steps would hand the worker prompts in a different order than the
//! history records them. [`SessionHub::in_arrival_order`] is the per
//! session turnstile a controller frame holds from the first check to the
//! last queue push, so frames are stored, judged (first answer wins) and
//! routed one at a time, in the order they reached it. Tokio's mutex is
//! fair, so that order is the order the tasks asked.
//!
//! ## Backpressure
//!
//! Each attached socket owns a bounded queue. A peer that stops reading
//! is closed with [`close_code::RESOURCE_LIMIT`] rather than buffered
//! forever — an unbounded queue would turn one stalled phone into the
//! server's memory ceiling. The byte budget is checked before the frame
//! is handed to the channel, so a single huge frame cannot slip past a
//! frame-count limit.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU16, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use rebon_bridge::session_stream::close_code;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Frames a single socket may have queued but not yet written.
pub const MAX_QUEUE_FRAMES: usize = 256;
/// Bytes a single socket may have queued but not yet written. Whichever
/// of the two limits is reached first closes the slow consumer.
pub const MAX_QUEUE_BYTES: usize = 1_048_576;

/// One frame on its way to a socket.
///
/// `event_id` is the `session_events` row the frame was persisted under.
/// A controller that is still replaying history drops anything at or
/// below the id it replayed through, which is what makes "replay, then
/// go live" lossless in both directions: the slot is registered before
/// the backlog is read, so no frame can fall between them, and the id
/// removes the overlap that guarantee creates.
///
/// `0` marks a frame that was never persisted — a `stream_error`
/// addressed at one peer, which is about that peer's request rather than
/// about the session.
#[derive(Debug, Clone)]
pub struct Outbound {
    /// Persisted event id, or `0` for an unpersisted frame.
    pub event_id: i64,
    /// The frame's JSON text, shared across every recipient of a fan-out.
    pub text: Arc<str>,
}

impl Outbound {
    /// A persisted frame.
    pub fn persisted(event_id: i64, text: Arc<str>) -> Self {
        Self { event_id, text }
    }

    /// A frame that exists only for this one socket.
    pub fn transient(text: String) -> Self {
        Self {
            event_id: 0,
            text: text.into(),
        }
    }

    /// Bytes this frame costs against a socket's queue budget.
    pub fn byte_len(&self) -> usize {
        self.text.len()
    }
}

/// One attached socket, as the fan-out sees it.
#[derive(Debug)]
pub struct Slot {
    id: u64,
    sender: mpsc::Sender<Outbound>,
    queued_bytes: Arc<AtomicUsize>,
    cancel: CancellationToken,
    close_code: Arc<AtomicU16>,
    /// The work item a worker's session token belongs to. The sweeper
    /// uses it to hang up on a worker whose lease went away.
    work_id: Option<String>,
}

impl Slot {
    /// Build a slot and the receiving half its socket task drains.
    ///
    /// The handles are returned together because they are one object
    /// split in two: the socket task cannot be handed a slot whose
    /// cancellation token or byte counter is a different one from the
    /// fan-out's.
    pub fn new(id: u64, work_id: Option<String>) -> (Self, SlotTask) {
        let (sender, receiver) = mpsc::channel(MAX_QUEUE_FRAMES);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        let close_code = Arc::new(AtomicU16::new(1000));
        let slot = Self {
            id,
            sender,
            queued_bytes: queued_bytes.clone(),
            cancel: cancel.clone(),
            close_code: close_code.clone(),
            work_id,
        };
        let task = SlotTask {
            receiver,
            queued_bytes,
            cancel,
            close_code,
        };
        (slot, task)
    }

    /// This slot's id, which is what detaching it later is keyed on.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Ask this socket to close with `code`.
    fn close(&self, code: u16) {
        self.close_code.store(code, Ordering::Relaxed);
        self.cancel.cancel();
    }

    /// Queue one frame, or fail and close the socket.
    ///
    /// Returns `false` when the slot is finished with — either the peer
    /// is too slow, or its socket task is already gone — and the caller
    /// then drops it from the hub.
    fn enqueue(&self, outbound: Outbound) -> bool {
        let len = outbound.byte_len();
        let mut current = self.queued_bytes.load(Ordering::Acquire);
        loop {
            if current.saturating_add(len) > MAX_QUEUE_BYTES {
                self.close(close_code::RESOURCE_LIMIT);
                return false;
            }
            match self.queued_bytes.compare_exchange_weak(
                current,
                current + len,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
        match self.sender.try_send(outbound) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.queued_bytes.fetch_sub(len, Ordering::AcqRel);
                self.close(close_code::RESOURCE_LIMIT);
                false
            }
            // The socket task is gone (or an upgrade that never
            // completed dropped the receiver). Nothing to close; the
            // caller drops the slot.
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.queued_bytes.fetch_sub(len, Ordering::AcqRel);
                false
            }
        }
    }
}

/// The half of a [`Slot`] the socket task owns.
#[derive(Debug)]
pub struct SlotTask {
    /// Frames queued for this socket.
    pub receiver: mpsc::Receiver<Outbound>,
    /// Bytes currently queued; the socket task subtracts as it writes.
    pub queued_bytes: Arc<AtomicUsize>,
    /// Fires when someone else decided this socket should close.
    pub cancel: CancellationToken,
    /// The code to close with once cancelled.
    pub close_code: Arc<AtomicU16>,
}

#[derive(Debug, Default)]
struct HubInner {
    worker: Option<Slot>,
    controllers: Vec<Slot>,
}

/// Everyone attached to one session right now.
#[derive(Debug, Default)]
pub struct SessionHub {
    inner: Mutex<HubInner>,
    /// Held by a controller frame from admission to routing; see the
    /// module docs.
    arrival: tokio::sync::Mutex<()>,
}

impl SessionHub {
    fn lock(&self) -> std::sync::MutexGuard<'_, HubInner> {
        self.inner.lock().expect("session hub lock")
    }

    /// Install `slot` as the session's worker, displacing whoever held
    /// the role. The displaced socket is closed with
    /// [`close_code::SUPERSEDED`] once the lock is released.
    pub fn attach_worker(&self, slot: Slot) {
        let displaced = {
            let mut inner = self.lock();
            inner.worker.replace(slot)
        };
        if let Some(displaced) = displaced {
            displaced.close(close_code::SUPERSEDED);
        }
    }

    /// Attach one more controller.
    pub fn attach_controller(&self, slot: Slot) {
        self.lock().controllers.push(slot);
    }

    /// Remove the worker, but only if it is still the one with `slot_id`
    /// — a supersession may already have replaced it, and that newer
    /// registration must survive the old socket task's cleanup.
    pub fn detach_worker(&self, slot_id: u64) {
        let mut inner = self.lock();
        if inner.worker.as_ref().is_some_and(|slot| slot.id == slot_id) {
            inner.worker = None;
        }
    }

    /// Remove one controller.
    pub fn detach_controller(&self, slot_id: u64) {
        self.lock().controllers.retain(|slot| slot.id != slot_id);
    }

    /// Wait for this session's turn to take a controller frame, and hold
    /// it until the guard drops. See the module docs.
    pub async fn in_arrival_order(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.arrival.lock().await
    }

    /// The attached worker's slot id, or `None` when no worker is
    /// attached. Checked before a controller's frame is persisted, so a
    /// prompt nobody can run is never written to the session's history;
    /// an answer's claim records it as the socket the answer went to.
    pub fn worker_slot_id(&self) -> Option<u64> {
        self.lock().worker.as_ref().map(Slot::id)
    }

    /// The work item the attached worker authenticated with.
    pub fn worker_work_id(&self) -> Option<String> {
        self.lock()
            .worker
            .as_ref()
            .and_then(|slot| slot.work_id.clone())
    }

    /// Nobody is attached, so the hub can be dropped.
    pub fn is_idle(&self) -> bool {
        let inner = self.lock();
        inner.worker.is_none() && inner.controllers.is_empty()
    }

    /// Close the attached worker, if any, with `code`.
    pub fn close_worker(&self, code: u16) {
        if let Some(worker) = self.lock().worker.as_ref() {
            worker.close(code);
        }
    }

    /// Close everyone attached with `code`. Used when the session itself
    /// ends rather than one connection.
    pub fn close_all(&self, code: u16) {
        let inner = self.lock();
        if let Some(worker) = inner.worker.as_ref() {
            worker.close(code);
        }
        for controller in &inner.controllers {
            controller.close(code);
        }
    }

    /// Hand one frame to the worker. `false` means no worker is attached.
    pub fn send_to_worker(&self, outbound: Outbound) -> bool {
        let mut inner = self.lock();
        let Some(worker) = inner.worker.as_ref() else {
            return false;
        };
        if worker.enqueue(outbound) {
            true
        } else {
            // A worker that cannot keep up is being closed; dropping the
            // slot now stops later frames from queueing behind it.
            inner.worker = None;
            false
        }
    }

    /// Hand one frame to exactly one attached socket, whichever role it
    /// holds. Used to answer the sender of a frame that went nowhere.
    ///
    /// `false` means that socket is no longer attached, in which case
    /// there is nobody left to tell.
    pub fn send_to_slot(&self, slot_id: u64, outbound: Outbound) -> bool {
        let mut inner = self.lock();
        if inner.worker.as_ref().is_some_and(|slot| slot.id == slot_id) {
            let delivered = inner
                .worker
                .as_ref()
                .is_some_and(|slot| slot.enqueue(outbound));
            if !delivered {
                inner.worker = None;
            }
            return delivered;
        }
        let mut delivered = false;
        inner.controllers.retain(|slot| {
            if slot.id != slot_id {
                return true;
            }
            delivered = slot.enqueue(outbound.clone());
            delivered
        });
        delivered
    }

    /// Hand one frame to every controller except `except`.
    ///
    /// Returns how many took it. Controllers that could not are closed
    /// and dropped.
    pub fn fan_out_to_controllers(&self, outbound: Outbound, except: Option<u64>) -> usize {
        let mut inner = self.lock();
        let mut delivered = 0usize;
        inner.controllers.retain(|slot| {
            if except == Some(slot.id) {
                return true;
            }
            if slot.enqueue(outbound.clone()) {
                delivered += 1;
                true
            } else {
                false
            }
        });
        delivered
    }
}

/// Every session with someone attached.
#[derive(Debug, Default)]
pub struct SessionHubs {
    hubs: Mutex<HashMap<String, Arc<SessionHub>>>,
}

impl SessionHubs {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<SessionHub>>> {
        self.hubs.lock().expect("session hubs lock")
    }

    /// The hub for `session_id`, created if this is the first attach.
    pub fn get_or_create(&self, session_id: &str) -> Arc<SessionHub> {
        self.lock()
            .entry(session_id.to_string())
            .or_default()
            .clone()
    }

    /// The hub for `session_id`, or `None` when nobody is attached.
    /// Used by the HTTP routes, which must not create a hub just to
    /// discover there is no one to tell.
    pub fn find(&self, session_id: &str) -> Option<Arc<SessionHub>> {
        self.lock().get(session_id).cloned()
    }

    /// Every session that currently has a worker attached, with the work
    /// item that worker authenticated with.
    pub fn attached_workers(&self) -> Vec<(String, String)> {
        self.lock()
            .iter()
            .filter_map(|(session_id, hub)| {
                hub.worker_work_id()
                    .map(|work_id| (session_id.clone(), work_id))
            })
            .collect()
    }

    /// Drop hubs nobody is attached to. Called from the 1 Hz sweeper.
    pub fn prune(&self) {
        // `Arc::strong_count == 1` means the map holds the only
        // reference, so no connect in flight can have taken this hub and
        // be about to install a slot in it.
        self.lock()
            .retain(|_, hub| Arc::strong_count(hub) > 1 || !hub.is_idle());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(text: &str) -> Outbound {
        Outbound::persisted(1, text.into())
    }

    #[test]
    fn a_second_worker_supersedes_the_first() {
        let hub = SessionHub::default();
        let (first, first_task) = Slot::new(1, Some("wrk_1".into()));
        hub.attach_worker(first);
        let (second, _second_task) = Slot::new(2, Some("wrk_2".into()));
        hub.attach_worker(second);

        assert!(first_task.cancel.is_cancelled());
        assert_eq!(
            first_task.close_code.load(Ordering::Relaxed),
            close_code::SUPERSEDED
        );
        assert_eq!(hub.worker_work_id().as_deref(), Some("wrk_2"));

        // The displaced socket's own cleanup must not evict the worker
        // that replaced it.
        hub.detach_worker(1);
        assert_eq!(hub.worker_work_id().as_deref(), Some("wrk_2"));
        hub.detach_worker(2);
        assert_eq!(hub.worker_slot_id(), None);
    }

    #[test]
    fn the_worker_slot_id_follows_supersession_and_detach() {
        let hub = SessionHub::default();
        assert_eq!(hub.worker_slot_id(), None);
        let (first, _first_task) = Slot::new(7, Some("wrk_1".into()));
        hub.attach_worker(first);
        assert_eq!(hub.worker_slot_id(), Some(7));
        let (second, _second_task) = Slot::new(8, Some("wrk_1".into()));
        hub.attach_worker(second);
        assert_eq!(hub.worker_slot_id(), Some(8));
        hub.detach_worker(8);
        assert_eq!(hub.worker_slot_id(), None);
    }

    #[tokio::test]
    async fn frames_take_their_turn_in_the_order_they_asked() {
        let hub = Arc::new(SessionHub::default());
        let order = Arc::new(Mutex::new(Vec::new()));
        let first = hub.in_arrival_order().await;
        let mut waiting = Vec::new();
        for turn in 0..8 {
            let hub = Arc::clone(&hub);
            let order = Arc::clone(&order);
            waiting.push(tokio::spawn(async move {
                let _turn = hub.in_arrival_order().await;
                order.lock().expect("order lock").push(turn);
            }));
            // On the test's single thread, yielding runs the new task up
            // to the lock, so it queues before the next one is spawned.
            tokio::task::yield_now().await;
        }
        assert!(order.lock().expect("order lock").is_empty());
        drop(first);
        for task in waiting {
            task.await.expect("turn");
        }
        assert_eq!(
            *order.lock().expect("order lock"),
            (0..8).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_frame_reaches_every_controller_but_the_sender() {
        let hub = SessionHub::default();
        let (a, mut a_task) = Slot::new(1, None);
        let (b, mut b_task) = Slot::new(2, None);
        hub.attach_controller(a);
        hub.attach_controller(b);

        assert_eq!(hub.fan_out_to_controllers(frame("hello"), None), 2);
        assert_eq!(
            a_task.receiver.try_recv().expect("a").text.as_ref(),
            "hello"
        );
        assert_eq!(
            b_task.receiver.try_recv().expect("b").text.as_ref(),
            "hello"
        );

        assert_eq!(hub.fan_out_to_controllers(frame("mine"), Some(1)), 1);
        assert!(a_task.receiver.try_recv().is_err());
        assert_eq!(b_task.receiver.try_recv().expect("b").text.as_ref(), "mine");
    }

    #[test]
    fn a_controller_frame_with_no_worker_is_refused() {
        let hub = SessionHub::default();
        assert_eq!(hub.worker_slot_id(), None);
        assert!(!hub.send_to_worker(frame("prompt")));
    }

    #[test]
    fn a_slow_consumer_is_closed_rather_than_buffered() {
        let hub = SessionHub::default();
        let (slot, task) = Slot::new(1, None);
        hub.attach_controller(slot);

        // Fill the frame budget without ever draining.
        for _ in 0..MAX_QUEUE_FRAMES {
            assert_eq!(hub.fan_out_to_controllers(frame("x"), None), 1);
        }
        assert_eq!(hub.fan_out_to_controllers(frame("x"), None), 0);
        assert!(task.cancel.is_cancelled());
        assert_eq!(
            task.close_code.load(Ordering::Relaxed),
            close_code::RESOURCE_LIMIT
        );
        // The overflowing slot is dropped, so it is not retried forever.
        assert_eq!(hub.fan_out_to_controllers(frame("x"), None), 0);
        assert!(hub.is_idle());
    }

    #[test]
    fn the_byte_budget_closes_a_consumer_the_frame_budget_would_not() {
        let hub = SessionHub::default();
        let (slot, task) = Slot::new(1, None);
        hub.attach_controller(slot);
        // Four of these exactly fill the byte budget without overflowing
        // it, and four frames are nowhere near the frame budget — so it
        // is the fifth, and the byte budget alone, that closes the socket.
        let big = Outbound::persisted(1, "x".repeat(MAX_QUEUE_BYTES / 4).into());
        for _ in 0..4 {
            assert_eq!(hub.fan_out_to_controllers(big.clone(), None), 1);
        }
        assert!(!task.cancel.is_cancelled());
        assert_eq!(hub.fan_out_to_controllers(big, None), 0);
        assert!(task.cancel.is_cancelled());
        assert_eq!(
            task.close_code.load(Ordering::Relaxed),
            close_code::RESOURCE_LIMIT
        );
    }

    #[test]
    fn a_slot_whose_task_is_gone_is_dropped_without_a_close_code() {
        let hub = SessionHub::default();
        let (slot, task) = Slot::new(1, None);
        hub.attach_controller(slot);
        // An upgrade that never completed drops the receiving half.
        let close_code_handle = task.close_code.clone();
        drop(task);
        assert_eq!(hub.fan_out_to_controllers(frame("x"), None), 0);
        assert!(hub.is_idle());
        assert_eq!(close_code_handle.load(Ordering::Relaxed), 1000);
    }

    #[test]
    fn the_registry_creates_on_demand_and_prunes_idle_hubs() {
        let hubs = SessionHubs::default();
        assert!(hubs.find("sess_1").is_none());
        let hub = hubs.get_or_create("sess_1");
        let (slot, _task) = Slot::new(1, Some("wrk_1".into()));
        hub.attach_worker(slot);
        assert!(hubs.find("sess_1").is_some());
        assert_eq!(
            hubs.attached_workers(),
            vec![("sess_1".to_string(), "wrk_1".to_string())]
        );

        // Held by this test, so not prunable even once idle.
        hub.detach_worker(1);
        hubs.prune();
        assert!(hubs.find("sess_1").is_some());

        drop(hub);
        hubs.prune();
        assert!(hubs.find("sess_1").is_none());
        assert!(hubs.attached_workers().is_empty());
    }

    #[test]
    fn closing_the_hub_closes_both_roles() {
        let hub = SessionHub::default();
        let (worker, worker_task) = Slot::new(1, Some("wrk_1".into()));
        let (controller, controller_task) = Slot::new(2, None);
        hub.attach_worker(worker);
        hub.attach_controller(controller);
        hub.close_all(close_code::TIMEOUT);
        for task in [worker_task, controller_task] {
            assert!(task.cancel.is_cancelled());
            assert_eq!(task.close_code.load(Ordering::Relaxed), close_code::TIMEOUT);
        }

        let lease = SessionHub::default();
        let (worker, worker_task) = Slot::new(3, Some("wrk_2".into()));
        let (controller, controller_task) = Slot::new(4, None);
        lease.attach_worker(worker);
        lease.attach_controller(controller);
        lease.close_worker(close_code::LEASE_GONE);
        assert!(worker_task.cancel.is_cancelled());
        assert_eq!(
            worker_task.close_code.load(Ordering::Relaxed),
            close_code::LEASE_GONE
        );
        assert!(
            !controller_task.cancel.is_cancelled(),
            "a lost lease is the worker's problem, not the controllers'"
        );
    }
}

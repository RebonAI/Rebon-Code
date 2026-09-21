//! One client for a session host, for every endpoint that talks to one.
//!
//! Before this, three endpoints each grew their own half of the same client.
//! `serve` resolved an owner, held a lease, opened a subscription, tracked a
//! cursor, allocated command ids and implemented the prompt fallback ladder
//! (the `serve` subcommand's hosted path); the terminal's remote attachment
//! did the same again; the desktop app did a third version over files. Three
//! copies of one state machine is how `serve` ended up able to reach an owner
//! the terminal thought was gone.
//!
//! So: [`SessionHostClient`] answers "who owns this session, and can I command
//! them" and hands back a [`SessionHostConnection`], which owns everything that
//! belongs to *one* connection to *one* endpoint generation — the lease, the
//! subscription and its closer, the cursor, the call-id allocator and the
//! in-flight calls. An endpoint keeps what is genuinely its own: rendering,
//! external protocol, and its local projection.
//!
//! # What this deliberately does not do
//!
//! It does not take a session over, write the transcript, or write owner and
//! lease fields. The one write it is allowed is a pending prompt appended for a
//! worker that has not published its endpoint yet — invariant I8 — and the
//! reason [`SessionHostClient::send_prompt`] exists at all.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::owner::{
    resolve_owner, LeaseGuard, OwnerCache, OwnerHandle, OwnerState, SessionEventStream,
    SessionStreamCloser,
};
use crate::{
    BackgroundImageAttachment, BackgroundIpcRequest, BackgroundIpcResponse,
    BackgroundPermissionOptionSnapshot, BackgroundPermissionQuerySnapshot, BackgroundStore,
    ClientLeaseKind, SessionStatusSnapshot,
};

/// How long a `CancelCall` is given to reach the owner.
///
/// Short and fixed: the waiter it releases has already been given up on, so the
/// only thing left to buy is the owner's chance to stop work that is still
/// cancellable. A client that waited here would have replaced one stall with
/// another.
const CANCEL_CALL_BUDGET: Duration = Duration::from_secs(2);

/// Everything a call to a session host can fail as.
///
/// Typed rather than a string, because control flow used to be decided by
/// `err.to_string() == "background turn is no longer cancellable"` — a sentence
/// in a message a translator could have reworded. The legacy wire still carries
/// `ok` plus an `error: String`; [`HostCallError::from_wire`] is the one place
/// that turns it back into a value, and nothing above it may look at the text
/// again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCallError {
    /// The token did not match. The endpoint has been replaced, or this is not
    /// its client.
    Unauthenticated,
    /// The owner is there but this request named a job, session or endpoint
    /// generation it is not.
    ///
    /// Carries which of those it was. The variant is what a caller branches
    /// on; the sentence is what a person reads, and "you named the wrong
    /// session" and "you named the wrong job" are different things to be told
    /// even though the caller does the same thing about both.
    OwnerFence(String),
    /// The owner is shutting down and is no longer taking work.
    OwnerClosing,
    /// This owner does not know this request. An older worker, and never a
    /// reason to keep waiting.
    Unsupported,
    /// The turn, query or generation this was fenced against has moved on.
    StaleGeneration,
    /// The owner refused on policy grounds.
    PermissionRejected,
    /// The call was cancelled — by `CancelCall`, by a cancelled turn, or by the
    /// host shutting down.
    Cancelled,
    /// The deadline passed with no answer. The waiter has been released and a
    /// `CancelCall` sent; a late answer will be dropped.
    HostUnanswered,
    /// The request could not be encoded, or the owner could not decode it.
    InvalidRequest,
    /// The owner answered, but the session behind it had failed.
    SessionFailure,
    /// The connection could not be made or was lost.
    Transport(String),
    /// The owner refused and said why, in words meant for the user.
    ///
    /// The catch-all the legacy wire needs: an owner from an older build has
    /// one error channel and puts everything in it. Show it; never branch on
    /// it.
    Refused(String),
}

impl HostCallError {
    /// Read the legacy `ok`/`error` pair as a value.
    ///
    /// The matching is on the owner's own sentences, and it is deliberately
    /// narrow: a sentence this does not recognise becomes [`Self::Refused`]
    /// rather than being forced into a category it may not belong to. Once the
    /// server answers with a typed `Result`, the mapping moves to the encoder and
    /// this stays only for owners built before it.
    pub fn from_wire(error: Option<String>) -> Self {
        let Some(error) = error else {
            return Self::Refused("the session owner refused the request".to_string());
        };
        let lowered = error.to_ascii_lowercase();
        if lowered.contains("unknown request") || lowered.contains("unsupported") {
            Self::Unsupported
        } else if lowered.contains("token") {
            Self::Unauthenticated
        } else if lowered.contains("no longer cancellable")
            || lowered.contains("different turn generation")
            || lowered.contains("endpoint changed")
            || lowered.contains("different ipc endpoint generation")
        {
            Self::StaleGeneration
        } else if lowered.contains("shutting down") || lowered.contains("closing") {
            Self::OwnerClosing
        } else {
            Self::Refused(error)
        }
    }

    /// Whether retrying with the same `command_id` could still succeed.
    ///
    /// A timeout can: the owner may have the call and simply be slow, and the
    /// idempotency history answers a repeat from memory. A fence failure cannot
    /// — the thing it was aimed at is gone.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::HostUnanswered | Self::Transport(_))
    }
}

impl std::fmt::Display for HostCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthenticated => f.write_str("this client is not the session owner's client"),
            Self::OwnerFence(reason) => f.write_str(reason),
            Self::OwnerClosing => f.write_str("the session owner is shutting down"),
            Self::Unsupported => f.write_str("the session owner does not know this request"),
            Self::StaleGeneration => f.write_str("that turn or prompt has moved on"),
            Self::PermissionRejected => f.write_str("the session owner refused on policy grounds"),
            Self::Cancelled => f.write_str("the call was cancelled"),
            Self::HostUnanswered => f.write_str("the session owner did not answer in time"),
            Self::InvalidRequest => f.write_str("the session owner could not read the request"),
            Self::SessionFailure => f.write_str("the session behind this owner has failed"),
            Self::Transport(err) => write!(f, "could not reach the session owner: {err}"),
            Self::Refused(err) => f.write_str(err),
        }
    }
}

impl std::error::Error for HostCallError {}

/// What an owner answered with.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HostReply {
    /// The payload, for the requests that have one (`Status`, `Rewind`,
    /// `ReconcilePlugins`, `SetSessionOption`).
    pub data: Option<serde_json::Value>,
}

impl HostReply {
    /// Read the payload as `T`, or say the owner did not send one.
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T, HostCallError> {
        let data = self.data.clone().ok_or(HostCallError::InvalidRequest)?;
        serde_json::from_value(data).map_err(|_| HostCallError::InvalidRequest)
    }
}

/// Strictly monotonic call ids, and the calls still waiting on one.
///
/// Monotonic on purpose: a late answer is matched by id,
/// so an id that could repeat would let a discarded answer be taken for the
/// current one. The random salt is what keeps that true *between* processes —
/// two clients starting in the same millisecond would otherwise both allocate
/// `1` and collide in the owner's idempotency history, and the owner would
/// answer one of them from the other's memory.
#[derive(Debug)]
pub struct CallIds {
    salt: String,
    next: AtomicU64,
    in_flight: Mutex<BTreeSet<u64>>,
}

impl Default for CallIds {
    fn default() -> Self {
        Self::new()
    }
}

impl CallIds {
    pub fn new() -> Self {
        let mut buf = [0u8; 4];
        let _ = getrandom::getrandom(&mut buf);
        Self {
            salt: format!("{:08x}", u32::from_le_bytes(buf)),
            next: AtomicU64::new(1),
            in_flight: Mutex::new(BTreeSet::new()),
        }
    }

    /// Take the next id and record it as in flight.
    fn begin(&self) -> InFlight<'_> {
        let sequence = self.next.fetch_add(1, Ordering::Relaxed);
        self.in_flight.lock().expect("poisoned").insert(sequence);
        InFlight {
            ids: self,
            sequence,
            wire: format!("cmd-{}-{sequence:016x}", self.salt),
        }
    }

    /// How many calls are still waiting for an answer.
    ///
    /// Zero after every terminal outcome, timeout included: a client that left
    /// an entry here on the timeout path would be leaking one per stalled call.
    pub fn in_flight(&self) -> usize {
        self.in_flight.lock().expect("poisoned").len()
    }
}

/// One call's id, released whichever way the call ends.
struct InFlight<'a> {
    ids: &'a CallIds,
    sequence: u64,
    wire: String,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.ids
            .in_flight
            .lock()
            .expect("poisoned")
            .remove(&self.sequence);
    }
}

/// A cheap, cloneable resolver and launcher.
///
/// Holds no connection and no lease: it answers "who owns this session" and
/// hands out [`SessionHostConnection`]s. Cloning it shares the owner cache,
/// which is the point — two surfaces in one process asking about the same
/// session a dozen times a second should make one ping, not twenty.
#[derive(Clone)]
pub struct SessionHostClient {
    store: BackgroundStore,
    projects_root: PathBuf,
    /// The `rebon` executable a queued prompt's worker is launched from.
    ///
    /// Carried rather than resolved on demand for the reason the job record
    /// carries `process_path`: the process appending the prompt may be a
    /// desktop app whose `PATH` a worker does not share, so "whichever rebon
    /// the OS finds" is not the same binary the user is running.
    worker_exe: PathBuf,
    owners: Arc<OwnerCache>,
}

/// How long a resolved owner is remembered.
///
/// The same split `OwnerCache` was built with: a host that answered is cheap to
/// re-check, and one that did not made the caller wait out a whole ping
/// timeout, so it is remembered for longer.
const OWNER_REACHABLE_TTL: Duration = Duration::from_millis(300);
const OWNER_UNREACHABLE_TTL: Duration = Duration::from_secs(2);

impl SessionHostClient {
    pub fn new(
        store: BackgroundStore,
        projects_root: impl Into<PathBuf>,
        worker_exe: impl Into<PathBuf>,
    ) -> Self {
        Self {
            store,
            projects_root: projects_root.into(),
            worker_exe: worker_exe.into(),
            owners: Arc::new(OwnerCache::new(OWNER_REACHABLE_TTL, OWNER_UNREACHABLE_TTL)),
        }
    }

    pub fn store(&self) -> &BackgroundStore {
        &self.store
    }

    pub fn projects_root(&self) -> &Path {
        &self.projects_root
    }

    pub fn worker_exe(&self) -> &Path {
        &self.worker_exe
    }

    /// Who owns `session_id`, from the cache when the answer is still fresh.
    pub fn resolve(&self, cwd: &str, session_id: &str) -> OwnerState {
        self.owners.resolve(&self.projects_root, cwd, session_id)
    }

    /// Who owns `session_id`, asking the lock, descriptor and endpoint again.
    ///
    /// The paired uncached form: a caller that has just been refused by an
    /// endpoint needs the answer the cache is holding to be thrown away, not
    /// returned to it.
    pub fn resolve_uncached(&self, cwd: &str, session_id: &str) -> OwnerState {
        self.owners.invalidate(session_id);
        resolve_owner(&self.projects_root, cwd, session_id)
    }

    /// Forget what is known about `session_id`.
    pub fn invalidate(&self, session_id: &str) {
        self.owners.invalidate(session_id);
    }

    /// Open a connection to a reachable owner.
    ///
    /// `None` for every other owner state: an opaque or unreachable owner is
    /// read-only and never taken over, and a free session has nobody to connect
    /// to. The caller decides what to do with that — the terminal offers to
    /// launch, `serve` refuses the tab — because the answer is an endpoint
    /// policy, not a transport one.
    pub fn connect(&self, cwd: &str, session_id: &str) -> Option<SessionHostConnection> {
        match self.resolve(cwd, session_id) {
            OwnerState::OwnedReachable { owner } => Some(SessionHostConnection::new(owner)),
            _ => None,
        }
    }

    /// Deliver a prompt to whoever will run it, by the one prompt ladder.
    ///
    /// Every endpoint used to carry its own version of this and they had
    /// drifted: `serve` fell back to the durable queue and the terminal did
    /// not, so the same prompt typed a second too early was accepted in a
    /// browser tab and refused in a terminal.
    ///
    /// 1. a reachable owner takes it over IPC;
    /// 2. a worker that is starting, a free session with a job record, or an
    ///    owner holding the lock without an endpoint, gets it appended to its
    ///    durable pending queue — the one write a client is allowed
    ///    (invariant I8);
    /// 3. an unreachable owner is refused, and nothing is written. It is alive
    ///    — the lock says so — and appending for a worker that is already
    ///    running would queue a prompt nobody will claim.
    pub fn send_prompt(
        &self,
        cwd: &str,
        session_id: &str,
        job_id: Option<&str>,
        message: String,
        images: Vec<BackgroundImageAttachment>,
    ) -> Result<PromptDelivery, HostCallError> {
        match self.resolve(cwd, session_id) {
            OwnerState::OwnedReachable { owner } => {
                let connection = SessionHostConnection::new(owner);
                connection.call(BackgroundIpcRequest::Reply { message, images })?;
                Ok(PromptDelivery::Delivered)
            }
            OwnerState::OwnedUnreachable { .. } => Err(HostCallError::HostUnanswered),
            OwnerState::Free | OwnerState::OwnedOpaque { .. } => {
                let Some(job_id) = job_id else {
                    return Err(HostCallError::OwnerFence(
                        "the session owner has been replaced".to_string(),
                    ));
                };
                crate::reply_to_background_job_in_store_with_images(
                    &self.store,
                    job_id,
                    message,
                    images,
                    // Wake or launch the worker: a prompt appended for a host
                    // that nobody starts is a prompt that waits forever.
                    true,
                    // The IPC rung of this ladder was taken above, by the
                    // reachable-owner arm; retrying it here would be the second
                    // implementation of the same choice.
                    false,
                    &self.worker_exe,
                )
                .map_err(|err| HostCallError::Refused(err.to_string()))?;
                Ok(PromptDelivery::Queued)
            }
        }
    }
}

/// Where a prompt went.
///
/// Named rather than a bool because the two have different consequences for
/// whoever is waiting: a delivered prompt starts a turn now, a queued one
/// starts when a worker claims it, and a caller that showed the same spinner
/// for both would be lying about one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDelivery {
    /// The owner took it.
    Delivered,
    /// Appended to the job's durable pending queue for a worker to claim.
    Queued,
}

/// A live connection to one endpoint generation of one session.
///
/// Everything here is scoped to that generation. When the endpoint is replaced
/// the whole value is dropped and a new one opened, which is what makes "clear
/// every old waiter, lease and permission route" a single `drop` rather than a
/// checklist each endpoint had to remember.
pub struct SessionHostConnection {
    owner: OwnerHandle,
    lease: Mutex<Option<LeaseGuard>>,
    closer: Mutex<Option<SessionStreamCloser>>,
    calls: CallIds,
    /// The last cursor seen on the subscription, for a reconnect to resume
    /// from.
    last_cursor: AtomicU64,
    /// The last turn generation the owner reported, for fencing answers.
    last_turn_generation: AtomicU64,
    /// Set once the caller is done with this generation. A reconnect loop reads
    /// it instead of being killed from outside.
    closed: Arc<AtomicBool>,
}

impl SessionHostConnection {
    pub fn new(owner: OwnerHandle) -> Self {
        Self {
            owner,
            lease: Mutex::new(None),
            closer: Mutex::new(None),
            calls: CallIds::new(),
            last_cursor: AtomicU64::new(0),
            last_turn_generation: AtomicU64::new(0),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn owner(&self) -> &OwnerHandle {
        &self.owner
    }

    /// The endpoint this connection is fenced to.
    pub fn endpoint(&self) -> crate::BackgroundIpcEndpoint {
        self.owner.endpoint()
    }

    pub fn calls(&self) -> &CallIds {
        &self.calls
    }

    /// The cursor a reconnect should resume from, if anything has been seen.
    pub fn last_cursor(&self) -> Option<u64> {
        match self.last_cursor.load(Ordering::Relaxed) {
            0 => None,
            cursor => Some(cursor),
        }
    }

    pub fn last_turn_generation(&self) -> u64 {
        self.last_turn_generation.load(Ordering::Relaxed)
    }

    /// Whether the caller has finished with this endpoint generation.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// A flag a reconnect loop can watch without holding the connection.
    pub fn cancellation(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.closed)
    }

    /// Send one request, giving it the budget that request kind is documented
    /// to need.
    ///
    /// Paired with [`Self::call_with_timeout`]. This is not a hidden fixed
    /// timeout: a slash command runs on the owner's turn loop and may wait for
    /// a turn to reach a point where it can be taken, which is why
    /// [`crate::command_response_timeout`] gives it minutes and a `Ping`
    /// seconds. A caller with its own deadline — a frame loop, a request with a
    /// user waiting — states it instead.
    /// Slash commands are the one request that does not answer in this shape:
    /// the owner replies with a [`crate::BackgroundCommandResponse`], which has
    /// no `ok` field at all. Sending one through here would fail to decode and
    /// look like a broken owner, so it is refused by name and sent through
    /// [`Self::run_command`] instead.
    pub fn call(&self, request: BackgroundIpcRequest) -> Result<HostReply, HostCallError> {
        if matches!(request, BackgroundIpcRequest::RunCommand { .. }) {
            return Err(HostCallError::InvalidRequest);
        }
        let timeout = default_budget(&request);
        self.call_with_timeout(request, timeout)
    }

    /// Run a slash command on the owner's turn loop and return what it printed.
    ///
    /// Its own entry point because it has its own response shape and its own
    /// budget: a command runs where the live session is, and may wait for the
    /// turn loop to reach a point where it can be taken. The call is registered
    /// under a monotonic id like any other, so giving up on it sends a
    /// `CancelCall` that the owner can actually act on.
    pub fn run_command(
        &self,
        name: &str,
        args: Vec<String>,
    ) -> Result<crate::CommandOutput, HostCallError> {
        let call = self.calls.begin();
        self.owner
            .run_command_with_id(name, args, call.wire.clone())
            .map_err(|err| HostCallError::Refused(err.to_string()))
    }

    /// Send one request and give up after `timeout`.
    ///
    /// Giving up is an action, not an absence of one: the waiter is released,
    /// a [`BackgroundIpcRequest::CancelCall`] is sent so the owner can drop
    /// work it has not committed, and the result is
    /// [`HostCallError::HostUnanswered`]. An owner too old to know `CancelCall`
    /// refuses it, and that refusal is ignored — the waiter is already gone,
    /// and blocking for compatibility would be the exact failure this replaces.
    pub fn call_with_timeout(
        &self,
        request: BackgroundIpcRequest,
        timeout: Duration,
    ) -> Result<HostReply, HostCallError> {
        let call = self.calls.begin();
        let outcome = self
            .owner
            .send_fallibly(request, Some(call.wire.clone()), timeout);
        match outcome {
            Ok(response) => {
                if let Some(status) = response
                    .data
                    .as_ref()
                    .and_then(|data| data.get("turnGeneration"))
                    .and_then(serde_json::Value::as_u64)
                {
                    self.observe_turn_generation(status);
                }
                Ok(HostReply {
                    data: response.data,
                })
            }
            Err(HostCallError::HostUnanswered) => {
                // The id is released by `call`'s drop either way; the cancel is
                // what lets the *owner* release its half.
                self.cancel_call(&call.wire);
                Err(HostCallError::HostUnanswered)
            }
            Err(other) => Err(other),
        }
    }

    /// Tell the owner to stop waiting on `command_id`.
    ///
    /// Best effort by construction: every way this can fail — an owner that
    /// does not know the request, one that has already answered, one that has
    /// gone — leaves the client in the state it wanted to be in anyway.
    fn cancel_call(&self, command_id: &str) {
        let request = BackgroundIpcRequest::CancelCall {
            command_id: command_id.to_string(),
        };
        match self.owner.send_fallibly(request, None, CANCEL_CALL_BUDGET) {
            Ok(_) | Err(HostCallError::Unsupported) => {}
            Err(err) => {
                tracing::debug!(
                    command_id,
                    error = %err,
                    "rebon: could not cancel a call the client gave up on"
                );
            }
        }
    }

    /// Everything this client must agree with the owner about (invariant I4).
    pub fn status(&self) -> Result<SessionStatusSnapshot, HostCallError> {
        let reply = self.call(BackgroundIpcRequest::Status)?;
        let status: SessionStatusSnapshot = reply.parse()?;
        self.observe_turn_generation(status.turn_generation);
        Ok(status)
    }

    fn observe_turn_generation(&self, turn_generation: u64) {
        self.last_turn_generation
            .fetch_max(turn_generation, Ordering::Relaxed);
    }

    /// Hold a lease for as long as this connection does.
    ///
    /// At most one: a second call replaces the first, and dropping the returned
    /// guard is not how a lease is released here — the connection owns it, so
    /// that "this endpoint generation is finished" and "its lease is gone" are
    /// the same event rather than two an endpoint had to sequence.
    pub fn hold_lease(&self, client_id: &str, kind: ClientLeaseKind) {
        let guard = self.owner.hold_lease(client_id, kind);
        *self.lease.lock().expect("poisoned") = Some(guard);
    }

    /// Whether this connection is holding a lease.
    pub fn holds_lease(&self) -> bool {
        self.lease.lock().expect("poisoned").is_some()
    }

    /// Say the lease is being given up because the user meant to be done, as
    /// opposed to handing the session on.
    pub fn mark_lease_deliberate(&self) {
        if let Some(guard) = self.lease.lock().expect("poisoned").as_ref() {
            guard.mark_deliberate();
        }
    }

    /// Open the session's live event stream, resuming from the last cursor
    /// seen.
    ///
    /// The closer is kept here rather than handed out, so that closing the
    /// connection ends the subscription. A reader parked on a quiet session
    /// cannot notice a flag; it has to be woken on the socket.
    pub fn subscribe(&self) -> Result<SessionEventStream, HostCallError> {
        let stream = self
            .owner
            .subscribe(self.last_cursor())
            .map_err(|err| HostCallError::Transport(err.to_string()))?;
        *self.closer.lock().expect("poisoned") = Some(stream.closer());
        Ok(stream)
    }

    /// Follow the event stream on a thread, delivering into a channel.
    ///
    /// The form a frame loop needs: a terminal redrawing and a window painting
    /// both want "whatever arrived since the last frame", and neither can block
    /// on an iterator. Paired with [`Self::subscribe`], which is the blocking
    /// form a pump of its own uses.
    ///
    /// A closed channel is the single signal for every way this ends — the
    /// owner hung up, it never spoke this protocol, the connection broke —
    /// because the caller's answer to all three is the same: carry on polling.
    pub fn subscribe_in_background(
        &self,
    ) -> Option<std::sync::mpsc::Receiver<crate::SessionEvent>> {
        // `since: 0` asks for everything the owner still holds, so a client
        // attaching mid-turn gets the state to render against rather than only
        // what happens next; one reopening a lost stream passes the cursor it
        // reached and gets the difference.
        self.owner
            .subscribe_in_background(Some(self.last_cursor.load(Ordering::Relaxed)))
    }

    /// Record where the stream has got to, so a reconnect resumes there.
    pub fn observe_cursor(&self, cursor: u64) {
        self.last_cursor.fetch_max(cursor, Ordering::Relaxed);
    }

    /// The answer to send for `option_id`, fail-closed.
    ///
    /// An option the owner did not offer is not a reason to guess.
    /// The reply is the owner's own `RejectOnce` when it offered one, and a
    /// cancellation when it did not — never an allow, never the last choice
    /// this client made, never a panic.
    pub fn permission_answer_for(
        query: &BackgroundPermissionQuerySnapshot,
        option_id: Option<&str>,
    ) -> PermissionAnswer {
        let Some(option_id) = option_id else {
            return PermissionAnswer::Cancelled;
        };
        if query
            .options
            .iter()
            .any(|option| option.option_id == option_id)
        {
            return PermissionAnswer::Option(option_id.to_string());
        }
        match reject_once_option(&query.options) {
            Some(reject) => PermissionAnswer::Option(reject.option_id.clone()),
            None => PermissionAnswer::Cancelled,
        }
    }

    /// Answer the prompt the owner is parked on, fenced to it.
    ///
    /// The generation comes from the owner's own snapshot rather than from the
    /// caller: what has to match is the turn the owner is holding now, and a
    /// client fencing with a value it remembered from the frame it rendered
    /// would be guarding against its own staleness instead of against a
    /// replaced prompt.
    pub fn answer_permission(
        &self,
        query_id: u64,
        option_id: Option<&str>,
        extra_text: Option<String>,
    ) -> Result<PermissionOutcome, HostCallError> {
        let status = self.status()?;
        let Some(pending) = status
            .pending_permission
            .as_ref()
            .filter(|pending| pending.query_id == query_id)
        else {
            return Ok(PermissionOutcome::AlreadyResolved);
        };
        let answer = Self::permission_answer_for(pending, option_id);
        self.call(BackgroundIpcRequest::PermissionAnswer {
            query_id,
            turn_generation: pending.turn_generation,
            option_id: answer.option_id(),
            extra_text,
            updated_input: None,
        })?;
        Ok(PermissionOutcome::Applied(answer))
    }

    /// End the subscription and release the lease.
    ///
    /// Idempotent, and the same path a drop takes: an endpoint that closed
    /// explicitly and one that was torn down must leave the owner in the same
    /// state, or "did anyone remember to close it" becomes a question about the
    /// owner's lifetime.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        if let Some(closer) = self.closer.lock().expect("poisoned").take() {
            closer.close();
        }
        // Dropping the guard stops the renewal thread and releases the lease,
        // joined, so the owner learns its client left before this returns.
        self.lease.lock().expect("poisoned").take();
    }
}

impl Drop for SessionHostConnection {
    fn drop(&mut self) {
        self.close();
    }
}

/// The owner's own `RejectOnce`, if it offered one.
///
/// Matched on the option's `kind` rather than its id: ids are generated per
/// query, and the kind is the part the tool contract fixes.
fn reject_once_option(
    options: &[BackgroundPermissionOptionSnapshot],
) -> Option<&BackgroundPermissionOptionSnapshot> {
    options
        .iter()
        .find(|option| option.kind == "reject_once" || option.kind == "rejectOnce")
}

/// What a client actually sends when the user picks an option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionAnswer {
    /// This option id, which the owner offered.
    Option(String),
    /// No answer can be sent: the option was not one of the owner's and it
    /// offered no `RejectOnce` to fall back to.
    Cancelled,
}

impl PermissionAnswer {
    fn option_id(&self) -> Option<String> {
        match self {
            Self::Option(id) => Some(id.clone()),
            Self::Cancelled => None,
        }
    }
}

/// What became of an answer aimed at a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    /// The owner took it, as this answer.
    Applied(PermissionAnswer),
    /// The owner is no longer parked on that prompt. Somebody else answered, or
    /// the turn moved on — a race, not a failure to retry.
    AlreadyResolved,
}

/// The budget a request kind is documented to need.
///
/// Named here rather than inlined at each call so that "how long may this
/// wait" is one table. The turn-loop commands get the store's own per-command
/// budget; everything else is a round trip to a loopback peer.
pub(crate) fn default_budget(request: &BackgroundIpcRequest) -> Duration {
    match request {
        BackgroundIpcRequest::RunCommand { name, .. } => crate::command_response_timeout(name),
        BackgroundIpcRequest::Compact { .. } => crate::command_response_timeout("compact"),
        BackgroundIpcRequest::Rewind { .. } => crate::command_response_timeout("rewind"),
        BackgroundIpcRequest::SetSessionOption { .. } => Duration::from_secs(30),
        _ => Duration::from_secs(2),
    }
}

/// The legacy response, as a typed result.
pub(crate) fn reply_from_wire(
    response: BackgroundIpcResponse,
) -> Result<BackgroundIpcResponse, HostCallError> {
    if response.ok {
        Ok(response)
    } else {
        Err(HostCallError::from_wire(response.error))
    }
}

#[cfg(test)]
mod tests;

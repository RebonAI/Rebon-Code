use std::sync::atomic::{AtomicBool, Ordering};

use super::super::*;
use rebon_plugin_tasks::TaskRegistryResolver;
use rebon_session_host::session_ext::{session_option_key, SessionOptionKey};
use rebon_session_host::{HostCallError, HostReply};

/// What a client Cancel has to get past before the turn is cancelled.
///
/// The turn's Stop hook runs where the turn runs: the
/// worker installs one of these per turn, and an `Err` is the hook's reason for
/// keeping the turn - it goes back to the client as the request's failure
/// instead of a cancellation.
pub(crate) type StopGate = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

#[derive(Clone)]
pub struct BackgroundIpcOwner {
    pub endpoint: BackgroundIpcEndpoint,
    pub pid_identity: Option<String>,
}

impl BackgroundIpcOwner {
    pub(crate) fn matches(&self, state: &BackgroundJobState) -> bool {
        state.process.pid == Some(self.endpoint.pid)
            && state.process.pid_identity == self.pid_identity
            && state.process.ipc_port == Some(self.endpoint.port)
            && state.process.ipc_token.as_deref() == Some(self.endpoint.token.as_str())
    }

    pub(crate) fn ensure_matches(&self, state: &BackgroundJobState) -> anyhow::Result<()> {
        if !self.matches(state) {
            anyhow::bail!(HostCallError::OwnerFence(
                "background IPC endpoint no longer owns this job".to_string()
            ));
        }
        Ok(())
    }
}

pub(crate) struct BackgroundCommandRequest {
    pub(crate) name: String,
    pub(crate) args: Vec<String>,
    pub(crate) response_tx:
        std::sync::mpsc::Sender<RequestResult<rebon_session_host::CommandOutput>>,
}

#[derive(Default)]
pub(crate) struct LivePermissionModeState {
    cell: Option<Arc<Mutex<rebon_permissions::PermissionMode>>>,
    desired: Option<rebon_permissions::PermissionMode>,
}

pub(crate) type SharedLivePermissionModeState = Arc<Mutex<LivePermissionModeState>>;

/// The owner's view of its MCP servers, read into `Status` and `hello`.
///
/// Lives on the server rather than in the job record because it is a fact
/// about this process — which servers it brought up — not about the job.
pub(crate) type SharedLiveMcpStatus = Arc<Mutex<Option<rebon_session_host::McpStatusSnapshot>>>;
/// The agent id the owner's session is running under.
///
/// The router that holds the choice lives in the worker's own process and
/// nothing published it, so `SessionStatusSnapshot.agent` came back `None`
/// from every worker and the fourth of I4's shared values never reached a
/// client. `serve` worked around it by remembering what it had asked for; a
/// client that never asked had no way to know at all.
pub(crate) type SharedLiveAgent = Arc<Mutex<Option<String>>>;

/// How many answered `command_id`s an owner remembers.
///
/// A client retries after losing a connection, not after losing interest, so
/// the window only has to outlive a reconnect. Small enough that the memory is
/// irrelevant, large enough that several clients retrying at once all find
/// their own answer.
const RECENT_COMMAND_MEMORY: usize = 64;

/// Answers already given, keyed by the `command_id` that asked.
///
/// Stored before either wire encodes it: replay must preserve the business
/// category even when the first caller used the legacy string-only protocol.
#[derive(Default)]
pub(crate) struct RecentCommandResults {
    pub(crate) prompts: super::acp_prompt::PromptReplies,
    entries: std::collections::VecDeque<(String, WireResponse)>,
    last: Option<LastCommand>,
    /// The calls still running, keyed by the same `command_id`, each with the
    /// flag its handler watches (design §5.3).
    ///
    /// Filed here rather than beside it because the two halves have to agree:
    /// a cancelled call whose work had already committed still has to leave its
    /// result in `entries`, so that the retry carrying that id gets that one
    /// answer instead of running the command a second time.
    ///
    /// Only calls that carry a `command_id` are registered — that id is the
    /// only name a cancel can use, and a call without one could not be retried
    /// idempotently either. They are the same property.
    in_flight: std::collections::HashMap<String, Arc<AtomicBool>>,
}

/// The most recent command this owner actually ran, published in its status.
/// A client that lost its connection mid-command reads the
/// outcome here instead of guessing whether to retry — `command_id` is the
/// idempotency key it sent, so it can tell its own command from anyone else's.
#[derive(Clone)]
pub(crate) struct LastCommand {
    pub(crate) id: String,
    pub(crate) at_ms: u64,
    pub(crate) error: Option<(HostCallError, String)>,
}

pub(crate) type SharedRecentCommandResults = Arc<Mutex<RecentCommandResults>>;

/// One registered call, deregistered however its handler leaves.
///
/// A guard rather than a pair of calls: the handler has several early returns,
/// and an entry left behind would make the next retry of that id look like it
/// was already running.
struct InFlightGuard {
    registry: SharedRecentCommandResults,
    command_id: String,
    cancelled: Arc<AtomicBool>,
}

impl InFlightGuard {
    fn register(registry: &SharedRecentCommandResults, command_id: &str) -> Self {
        let cancelled = registry.lock().expect("poisoned").begin_call(command_id);
        Self {
            registry: Arc::clone(registry),
            command_id: command_id.to_string(),
            cancelled,
        }
    }

    /// Whether the client gave up while this was running.
    ///
    /// Checked where a handler can still stop without leaving a half-finished
    /// change. Past that point the work finishes and only the *answer* is
    /// dropped — a cancel is not a rollback (design §5.3 item 4).
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.registry
            .lock()
            .expect("poisoned")
            .finish_call(&self.command_id);
    }
}

impl RecentCommandResults {
    pub(crate) fn get(&self, command_id: &str) -> Option<WireResponse> {
        self.prompts.acknowledgement(command_id).or_else(|| {
            self.entries
                .iter()
                .find(|(id, _)| id == command_id)
                .map(|(_, body)| body.clone())
        })
    }

    fn remember(&mut self, command_id: String, body: WireResponse) {
        if self.entries.iter().any(|(id, _)| *id == command_id) {
            return;
        }
        if self.entries.len() >= RECENT_COMMAND_MEMORY {
            self.entries.pop_front();
        }
        self.entries.push_back((command_id, body));
    }

    /// Record a command that ran. Only commands that carried a `command_id`
    /// reach here — a client that did not name its command cannot be looking
    /// for the answer to it.
    fn note_last(
        &mut self,
        command_id: String,
        at_ms: u64,
        error: Option<(HostCallError, String)>,
    ) {
        self.last = Some(LastCommand {
            id: command_id,
            at_ms,
            error,
        });
    }

    fn last(&self) -> Option<LastCommand> {
        self.last.clone()
    }

    /// Register `command_id` and hand back the flag its handler watches.
    ///
    /// A repeat of an id that is still in flight gets the *same* flag rather
    /// than a second entry: two connections carrying one id are one call being
    /// retried, and a cancel has to reach both.
    fn begin_call(&mut self, command_id: &str) -> Arc<AtomicBool> {
        Arc::clone(
            self.in_flight
                .entry(command_id.to_string())
                .or_insert_with(|| Arc::new(AtomicBool::new(false))),
        )
    }

    fn finish_call(&mut self, command_id: &str) {
        self.in_flight.remove(command_id);
    }

    /// Mark `command_id` cancelled. Says whether anything was waiting.
    ///
    /// The entry is removed as well as flagged, so a handler already past its
    /// last cancellation point finds nothing to answer to, and a later retry
    /// starts from a clean slot rather than a pre-cancelled one.
    fn cancel_call(&mut self, command_id: &str) -> bool {
        match self.in_flight.remove(command_id) {
            Some(flag) => {
                flag.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Cancel every call in flight, and say how many there were.
    ///
    /// What shutdown runs (design §5.3 item 6): the same path a single cancel
    /// takes, rather than a shutdown-only mechanism that would have to be kept
    /// in step with this one.
    pub(crate) fn cancel_all_calls(&mut self) -> usize {
        let cancelled = self.in_flight.len();
        for flag in self.in_flight.values() {
            flag.store(true, Ordering::Relaxed);
        }
        self.in_flight.clear();
        cancelled
    }

    /// How many calls are still waiting for an answer. Tests only; see the
    /// wrapper on [`BackgroundIpcServer`].
    #[cfg(test)]
    pub(crate) fn calls_in_flight(&self) -> usize {
        self.in_flight.len()
    }
}

impl LivePermissionModeState {
    /// The mode the turn loop is actually running under.
    ///
    /// The live cell once the session exists; before that, the mode a
    /// `SetPermissionMode` asked for and the session will boot into. `None`
    /// only while nobody has said anything, in which case the caller falls
    /// back to what the job record last published.
    pub(crate) fn in_force(&self) -> Option<rebon_permissions::PermissionMode> {
        self.cell
            .as_ref()
            .map(|cell| *cell.lock().expect("mode cell poisoned"))
            .or(self.desired)
    }
}

#[derive(Clone)]
pub(crate) struct BackgroundTeammateRuntime {
    pub(crate) registry: Arc<TaskRegistry>,
    pub(crate) manager: Arc<dyn TeamManager>,
    pub(crate) handle: tokio::runtime::Handle,
    pub(crate) session_id: String,
    // An empty registry is not quiescent while the detached bridge can still
    // race a final teammate registration from the parent turn.
    pub(crate) task_bridge: BackgroundTaskBridgeState,
}

pub struct BackgroundIpcServer {
    pub port: u16,
    pub token: String,
    pub pid_identity: Option<String>,
    pub cancel: rebon_types::PromptCancel,
    pub(crate) turn_cancel: Arc<Mutex<rebon_types::PromptCancel>>,
    pub(crate) task_registry_resolver: Arc<Mutex<Option<TaskRegistryResolver>>>,
    pub(crate) teammate_runtimes: Arc<Mutex<Vec<BackgroundTeammateRuntime>>>,
    pub(crate) permission_receivers: Arc<Mutex<Vec<BackgroundPermissionReceiver>>>,
    pub(crate) permission_responses:
        Arc<Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<PermissionAnswer>>>>,
    pub(crate) permission_rule_context: Arc<Mutex<Option<BackgroundPermissionRuleContext>>>,
    /// See [`StopGate`]. Attached per turn by the worker.
    pub(crate) stop_gate: Arc<Mutex<Option<StopGate>>>,
    live_permission_mode: SharedLivePermissionModeState,
    live_mcp_status: SharedLiveMcpStatus,
    live_agent: SharedLiveAgent,
    /// Answers already given, and the last command that ran. Shared with every
    /// connection thread: a retry has to reach the memory the first attempt
    /// wrote to, and the status this side publishes has to name the same
    /// command a client is waiting on.
    pub(crate) recent_command_results: SharedRecentCommandResults,
    /// Commands handed over by the IPC reader threads. A tokio channel so the
    /// turn loop can *wait* on one instead of waking on a timer to poll for it.
    pub(crate) command_rx:
        Arc<Mutex<tokio::sync::mpsc::UnboundedReceiver<BackgroundCommandRequest>>>,
    pub(crate) shared_auto_mode_state: crate::build::SharedAutoModeState,
    /// The session's live event stream, shared with every subscriber.
    pub events: SessionEventStream,
    /// Rung whenever a client changed the job record over IPC — a reply
    /// queued, a lease taken. The linger loop waits on it (with a timeout)
    /// instead of only on a timer, so a prompt sent to a parked worker is
    /// picked up when it arrives rather than at the next tick.
    pub(crate) wake: Arc<tokio::sync::Notify>,
    pub(crate) pending_compact: Mutex<Option<Option<String>>>,
    pub(crate) pending_sweep: Mutex<Option<Option<usize>>>,
    pub(crate) persistent_prune_level: Mutex<Option<rebon_api::PruneLevel>>,
    /// Whether the accept thread is still in its loop.
    ///
    /// Observed rather than inferred. "Can something still connect" does not
    /// answer it: a listening socket's backlog completes connections in the
    /// kernel whether or not anyone calls `accept`, so a connect succeeding
    /// says nothing about the thread. This is the thread reporting on itself.
    accept_thread_running: Arc<AtomicBool>,
}

impl BackgroundIpcServer {
    /// Stop serving.
    ///
    /// The binary's own tests drive a real server and have to be able to end
    /// it; the cancel token behind this is how the worker ends it too.
    ///
    /// The flag is set *before* the wake-up connection, never after: the
    /// accept thread checks it the moment accept returns, and a wake that
    /// arrived first would be read as an ordinary client and served.
    pub fn stop(&self) {
        self.close_prompt_calls();
        self.cancel.cancel();
        wake_accept_thread(self.port);
    }

    /// Whether the accept thread is still in its loop.
    ///
    /// Only the tests ask; see the visibility rule in `crates/REBON.md`.
    #[doc(hidden)]
    pub fn accept_thread_running(&self) -> bool {
        self.accept_thread_running.load(Ordering::SeqCst)
    }

    pub fn owner(&self) -> BackgroundIpcOwner {
        BackgroundIpcOwner {
            endpoint: BackgroundIpcEndpoint {
                pid: std::process::id(),
                port: self.port,
                token: self.token.clone(),
            },
            pid_identity: self.pid_identity.clone(),
        }
    }

    /// Release every call still waiting on this host, and say how many.
    ///
    /// What shutdown runs. A client waiting on a command is told the same way a
    /// `CancelCall` would tell it, rather than being left to discover that the
    /// endpoint stopped answering.
    pub(crate) fn cancel_calls_in_flight(&self) -> usize {
        self.recent_command_results
            .lock()
            .expect("poisoned")
            .cancel_all_calls()
    }

    /// How many calls are waiting for an answer right now.
    ///
    /// Only the tests ask: production code either registers a call or cancels
    /// one, and never needs the count. It is here because "the registry is
    /// empty again" is the assertion that a waiter was actually released rather
    /// than merely stopped being waited on.
    #[cfg(test)]
    pub(crate) fn calls_in_flight(&self) -> usize {
        self.recent_command_results
            .lock()
            .expect("poisoned")
            .calls_in_flight()
    }

    pub(crate) fn task_runtime_controller(
        &self,
        store: BackgroundStore,
        job_id: String,
    ) -> Arc<dyn rebon_tool::TaskRuntimeController> {
        Arc::new(BackgroundTaskRuntimeController::new(
            Arc::clone(&self.task_registry_resolver),
            Arc::clone(&self.teammate_runtimes),
            store,
            job_id,
        ))
    }

    pub(crate) fn start_turn_and<T>(
        &self,
        action: impl FnOnce() -> T,
    ) -> (rebon_types::PromptCancel, T) {
        let mut current = self.turn_cancel.lock().expect("poisoned");
        let cancel = rebon_types::PromptCancel::new();
        *current = cancel.clone();
        let result = action();
        (cancel, result)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn start_turn(&self) -> rebon_types::PromptCancel {
        self.start_turn_and(|| ()).0
    }

    /// Install the resolver for this worker's live session scopes.
    ///
    /// The resolver never owns task state and never searches old registries:
    /// every operation supplies the exact session id and resolves the typed
    /// `task-registry` seat from that session's current kernel scope.
    pub(crate) fn attach_task_registry_resolver(&self, resolver: TaskRegistryResolver) {
        *self
            .task_registry_resolver
            .lock()
            .expect("background task registry resolver poisoned") = Some(resolver);
    }

    pub(crate) fn attach_teammate_runtime(
        &self,
        registry: Arc<TaskRegistry>,
        manager: Arc<dyn TeamManager>,
        session_id: String,
    ) -> Option<BackgroundTaskBridgeState> {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return None;
        };
        let mut runtimes = self.teammate_runtimes.lock().expect("poisoned");
        // Drop only finished runtime bridges left by a previous scope. The
        // registry identity itself is always chosen by the session seat; this
        // cache merely retains the manager/handle needed to finish work that
        // already holds that registry Arc.
        runtimes.retain(|runtime| {
            runtime.task_bridge.is_running()
                || runtime.registry.snapshots().iter().any(|snapshot| {
                    !snapshot.status.is_terminal()
                        || matches!(&snapshot.data, TaskData::InProcessTeammate(_))
                })
        });
        if let Some(runtime) = runtimes
            .iter_mut()
            .find(|runtime| Arc::ptr_eq(&runtime.registry, &registry))
        {
            runtime.manager = manager;
            runtime.handle = handle;
            runtime.session_id = session_id;
            Some(runtime.task_bridge.clone())
        } else {
            let task_bridge = BackgroundTaskBridgeState::new();
            runtimes.push(BackgroundTeammateRuntime {
                registry,
                manager,
                handle,
                session_id,
                task_bridge: task_bridge.clone(),
            });
            Some(task_bridge)
        }
    }

    /// Re-key every attached permission receiver to `turn_generation`.
    ///
    /// A warmed session keeps its receiver across the claim that starts
    /// its first turn, and the forwarder relays only queries whose
    /// generation is the job's current one — so the receiver follows the
    /// claim rather than being attached again.
    pub(crate) fn retarget_permission_receivers(&self, turn_generation: u64) {
        for receiver in self
            .permission_receivers
            .lock()
            .expect("poisoned")
            .iter_mut()
        {
            receiver.turn_generation = turn_generation;
        }
    }

    /// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
    #[doc(hidden)]
    pub fn attach_permission_receiver(
        &self,
        turn_generation: u64,
        receiver: tokio::sync::mpsc::UnboundedReceiver<OutboundPermissionQuery>,
    ) {
        self.permission_receivers
            .lock()
            .expect("poisoned")
            .push(BackgroundPermissionReceiver {
                turn_generation,
                receiver,
            });
    }

    pub(crate) fn attach_permission_rule_context(
        &self,
        cwd: String,
        policy_store: rebon_core::policy::PolicyStore,
    ) {
        *self.permission_rule_context.lock().expect("poisoned") =
            Some(BackgroundPermissionRuleContext { cwd, policy_store });
    }

    /// Install what a Cancel has to get past this turn. See [`StopGate`].
    pub(crate) fn attach_stop_gate(&self, gate: StopGate) {
        *self.stop_gate.lock().expect("poisoned") = Some(gate);
    }

    pub(crate) fn attach_permission_mode_cell(
        &self,
        cell: Arc<Mutex<rebon_permissions::PermissionMode>>,
    ) {
        let mut live = self.live_permission_mode.lock().expect("poisoned");
        if let Some(mode) = live.desired.take() {
            *cell.lock().expect("mode cell poisoned") = mode;
        }
        live.cell = Some(cell);
    }

    /// Publish what this owner's MCP servers look like.
    ///
    /// Stored for `Status` and `hello`, and pushed as a `status` event when it
    /// changed, so a mirror's `/mcp` shows the servers the moment they land
    /// rather than at its next poll. Saying the same thing twice is dropped:
    /// the worker calls this at every point the servers could have changed,
    /// most of which they did not.
    pub(crate) fn publish_mcp_status(
        &self,
        store: &BackgroundStore,
        job_id: &str,
        snapshot: Option<rebon_session_host::McpStatusSnapshot>,
    ) {
        let changed = {
            let mut live = self.live_mcp_status.lock().expect("poisoned");
            if *live == snapshot {
                false
            } else {
                *live = snapshot;
                true
            }
        };
        if !changed {
            return;
        }
        if let Ok(state) = store.read_state(job_id) {
            self.events.publish_status(session_status_snapshot(
                &state,
                &self.live_permission_mode,
                &self.live_mcp_status,
                &self.live_agent,
                &self.recent_command_results,
            ));
        }
    }

    /// Push the session's state as the record has it right now.
    ///
    /// For the moments the record changed in a way every client shows and no
    /// other publisher covers: a turn that just ended wrote its usage and its
    /// terminal status, and a client reacting to the turn event would
    /// otherwise show the previous total until something else — a mode
    /// change, a command — happened to publish a snapshot. Not deduplicated:
    /// the caller names the moment, and the record at that moment is new by
    /// construction.
    pub(crate) fn publish_status_now(&self, store: &BackgroundStore, job_id: &str) {
        if let Ok(state) = store.read_state(job_id) {
            self.events.publish_status(session_status_snapshot(
                &state,
                &self.live_permission_mode,
                &self.live_mcp_status,
                &self.live_agent,
                &self.recent_command_results,
            ));
        }
    }

    /// Say which agent the session is running under, and tell the clients.
    ///
    /// Same shape as [`Self::publish_mcp_status`] and for the same reason: the
    /// worker calls it wherever the choice could have moved, most of which it
    /// did not, so saying the same thing twice is dropped rather than becoming
    /// a status event every client re-renders on.
    pub(crate) fn publish_agent(
        &self,
        store: &BackgroundStore,
        job_id: &str,
        agent: Option<String>,
    ) {
        let changed = {
            let mut live = self.live_agent.lock().expect("poisoned");
            if *live == agent {
                false
            } else {
                *live = agent;
                true
            }
        };
        if !changed {
            return;
        }
        if let Ok(state) = store.read_state(job_id) {
            self.events.publish_status(session_status_snapshot(
                &state,
                &self.live_permission_mode,
                &self.live_mcp_status,
                &self.live_agent,
                &self.recent_command_results,
            ));
        }
    }

    /// Whether anything has been published about the MCP servers yet.
    pub(crate) fn has_mcp_status(&self) -> bool {
        self.live_mcp_status.lock().expect("poisoned").is_some()
    }

    pub(crate) fn cancel_permission_response(&self, query_id: u64) -> bool {
        let sender = self
            .permission_responses
            .lock()
            .expect("poisoned")
            .remove(&query_id);
        sender.is_some_and(|sender| sender.send(PermissionAnswer::Cancelled).is_ok())
    }

    pub(crate) fn try_recv_command(&self) -> Option<BackgroundCommandRequest> {
        self.command_rx.lock().expect("poisoned").try_recv().ok()
    }

    /// Wait for the next command. Held by the turn loop's `select!` so an idle
    /// turn sleeps until a command actually arrives; polling for one on a timer
    /// woke the worker 40 times a second for the length of every turn.
    ///
    /// The guard is taken inside the poll and released before it returns, so
    /// nothing is held across a suspension point. Cancelling the returned future
    /// (the `select!` losing this branch) drops no message: `poll_recv` leaves an
    /// unread command queued.
    pub(crate) async fn recv_command(&self) -> Option<BackgroundCommandRequest> {
        std::future::poll_fn(|cx| self.command_rx.lock().expect("poisoned").poll_recv(cx)).await
    }

    #[cfg(test)]
    pub(crate) fn drain_commands(
        &self,
        mut handler: impl FnMut(&str, &[String]) -> Result<rebon_session_host::CommandOutput, String>,
    ) -> usize {
        let mut count = 0;
        while let Some(request) = self.try_recv_command() {
            let result = handler(&request.name, &request.args).or_else(request_refused);
            let _ = request.response_tx.send(result);
            count += 1;
        }
        count
    }

    pub(crate) fn capture_idle_context_command(
        &self,
        session: &crate::EngineSession,
        name: &str,
        args: &[String],
    ) {
        let name = name.trim().trim_start_matches('/');
        if name.eq_ignore_ascii_case("compact") {
            let instructions = (!args.is_empty()).then(|| args.join(" "));
            *self.pending_compact.lock().expect("poisoned") = Some(instructions);
            return;
        }
        if !name.eq_ignore_ascii_case("prune") {
            return;
        }
        match args {
            [command] if command == "sweep" => {
                *self.pending_sweep.lock().expect("poisoned") = Some(None);
            }
            [command, count] if command == "sweep" => {
                if let Ok(count) = count.parse::<usize>() {
                    *self.pending_sweep.lock().expect("poisoned") = Some(Some(count));
                }
            }
            [command, ..] if command == "manual" => {
                *self.persistent_prune_level.lock().expect("poisoned") =
                    Some(session.model.prune_level.get());
            }
            _ => {}
        }
    }

    pub(crate) fn apply_pending_context_commands(&self, session: &crate::EngineSession) {
        if let Some(level) = *self.persistent_prune_level.lock().expect("poisoned") {
            session.model.prune_level.set(level);
        }
        if let Some(count) = self.pending_sweep.lock().expect("poisoned").take() {
            session.model.prune_level.request_sweep(count);
        }
        if let Some(instructions) = self.pending_compact.lock().expect("poisoned").take() {
            session
                .model
                .prune_level
                .budget
                .force_compact_once_with_instructions(instructions);
        }
    }
}

impl Drop for BackgroundIpcServer {
    fn drop(&mut self) {
        self.close_prompt_calls();
        self.cancel.cancel();
        wake_accept_thread(self.port);
    }
}

/// Return a blocked `accept()` by connecting to it.
///
/// There is no other way. A blocking accept is woken by a connection or by the
/// listener closing, and the listener is owned by the thread that is blocked
/// in it. So shutdown makes one connection to its own port; the thread accepts
/// it, sees the cancel flag, and drops it unread.
///
/// The alternative -- a non-blocking listener polled on a timer -- is what this
/// replaces. At the 50 ms it used to poll, every IPC call paid up to 50 ms
/// before anything it asked for began; at a fast enough poll to hide that, an
/// idle worker wakes a thousand times a second, which the idle-CPU audit
/// specifically ruled out.
///
/// A wake that cannot connect is not worth reporting: the only ways it fails
/// are that the listener is already gone (so the thread is already returning)
/// or the machine is out of sockets (so the thread returning is not the
/// problem). The timeout keeps a shutdown from blocking on either.
fn wake_accept_thread(port: u16) {
    let Ok(address) = BackgroundStore::ipc_addr(port).parse() else {
        return;
    };
    let _ = std::net::TcpStream::connect_timeout(&address, Duration::from_millis(250));
}

pub(crate) fn permission_query_id_seed(ipc_token: &str) -> u64 {
    ipc_token
        .get(..16)
        .and_then(|prefix| u64::from_str_radix(prefix, 16).ok())
        .unwrap_or(1)
        .max(1)
}

/// How many connections may wait to be accepted.
///
/// The platform default is small — commonly 128, sometimes far less — and a
/// full backlog does not refuse a connection, it *drops the SYN*: the client
/// waits out its own connect timeout and reports "timed out" against a
/// listener that is alive and well. That is a confusing failure to debug from
/// the client's side, and it costs nothing to make unlikely.
///
/// The number is a ceiling the kernel may lower, not an allocation.
const LISTEN_BACKLOG: i32 = 1024;

/// Bind the owner's port with a backlog rather than the platform's default.
fn bind_with_backlog() -> anyhow::Result<TcpListener> {
    use socket2::{Domain, Socket, Type};

    let address: std::net::SocketAddr = "127.0.0.1:0".parse()?;
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None)?;
    socket.bind(&address.into())?;
    socket.listen(LISTEN_BACKLOG)?;
    Ok(socket.into())
}

pub fn start_background_ipc_server(
    store: &BackgroundStore,
    job_id: &str,
) -> anyhow::Result<BackgroundIpcServer> {
    validate_job_id(job_id)?;
    // Blocking, so an idle worker's accept thread costs nothing at all. It is
    // woken at shutdown by a connection this process makes to itself; see
    // `BackgroundIpcServer::stop`.
    let listener = bind_with_backlog()?;
    let port = listener.local_addr()?.port();
    let token = generate_ipc_token()?;
    let pid_identity = rebon_session_host::process_identity(std::process::id());
    let owner = BackgroundIpcOwner {
        endpoint: BackgroundIpcEndpoint {
            pid: std::process::id(),
            port,
            token: token.clone(),
        },
        pid_identity: pid_identity.clone(),
    };
    let cancel = rebon_types::PromptCancel::new();
    let turn_cancel = Arc::new(Mutex::new(rebon_types::PromptCancel::new()));
    let task_registry_resolver = Arc::new(Mutex::new(None));
    let teammate_runtimes = Arc::new(Mutex::new(Vec::<BackgroundTeammateRuntime>::new()));
    let permission_receivers = Arc::new(Mutex::new(Vec::<BackgroundPermissionReceiver>::new()));
    let permission_responses = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let permission_rule_context: Arc<Mutex<Option<BackgroundPermissionRuleContext>>> =
        Arc::new(Mutex::new(None));
    let stop_gate: Arc<Mutex<Option<StopGate>>> = Arc::new(Mutex::new(None));
    let live_permission_mode = Arc::new(Mutex::new(LivePermissionModeState::default()));
    let live_mcp_status: SharedLiveMcpStatus = Arc::new(Mutex::new(None));
    let live_agent: SharedLiveAgent = Arc::new(Mutex::new(None));
    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
    let command_rx = Arc::new(Mutex::new(command_rx));
    // Shared by every connection thread so a retry reaches the same memory the
    // first attempt wrote to.
    let recent_command_results: SharedRecentCommandResults =
        Arc::new(Mutex::new(RecentCommandResults::default()));
    let events = SessionEventStream::new();
    let wake = Arc::new(tokio::sync::Notify::new());
    let accept_thread_running = Arc::new(AtomicBool::new(true));

    spawn_ipc_accept_thread(
        &accept_thread_running,
        listener,
        store,
        job_id,
        &owner,
        &cancel,
        &turn_cancel,
        &task_registry_resolver,
        &teammate_runtimes,
        &permission_responses,
        &permission_rule_context,
        &stop_gate,
        &live_permission_mode,
        &live_mcp_status,
        &live_agent,
        command_tx.clone(),
        &recent_command_results,
        &events,
        &wake,
    );

    spawn_permission_forward_thread(
        store,
        job_id,
        &owner,
        &token,
        &cancel,
        &permission_receivers,
        &permission_responses,
        &permission_rule_context,
        &events,
    );

    store.append_event(job_id, "ipc_started", serde_json::json!({ "port": port }))?;
    Ok(BackgroundIpcServer {
        port,
        token,
        accept_thread_running,
        live_agent,
        pid_identity,
        cancel,
        turn_cancel,
        task_registry_resolver,
        teammate_runtimes,
        permission_receivers,
        permission_responses,
        permission_rule_context,
        stop_gate,
        live_permission_mode,
        live_mcp_status,
        recent_command_results: Arc::clone(&recent_command_results),
        command_rx,
        shared_auto_mode_state: crate::build::SharedAutoModeState::default(),
        events,
        wake,
        pending_compact: Mutex::new(None),
        pending_sweep: Mutex::new(None),
        persistent_prune_level: Mutex::new(None),
    })
}

/// The typed outcome and its human diagnostic, as already used by the request
/// fence. Keeping both does not parse a category back out of a sentence: only
/// the legacy encoder discards the kind. `HostReply` is the shared payload.
pub(crate) type RequestResult<T = HostReply> = Result<T, (HostCallError, String)>;

pub(crate) fn request_refused<T>(message: impl Into<String>) -> RequestResult<T> {
    let message = message.into();
    Err((HostCallError::Refused(message.clone()), message))
}

/// Store transactions return anyhow errors. Preserve a typed business cause
/// through its diagnostic context; unclassified failures are refusals, never
/// inferred from words such as "token" or "unsupported" in the message.
pub(crate) fn request_error(error: anyhow::Error) -> (HostCallError, String) {
    let message = error.to_string();
    let kind = error
        .downcast_ref::<HostCallError>()
        .cloned()
        .unwrap_or_else(|| HostCallError::Refused(message.clone()));
    (kind, message)
}

fn reply_with_data(value: &impl serde::Serialize) -> RequestResult {
    match serde_json::to_value(value) {
        Ok(data) => Ok(HostReply { data: Some(data) }),
        Err(error) => request_refused(format!("could not serialize the response: {error}")),
    }
}

/// What one request produced, before it is written to whichever wire asked.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WireResponse {
    Standard(RequestResult),
    Command(RequestResult<rebon_session_host::CommandOutput>),
}

impl WireResponse {
    fn error(&self) -> Option<&(HostCallError, String)> {
        match self {
            Self::Standard(result) => result.as_ref().err(),
            Self::Command(result) => result.as_ref().err(),
        }
    }

    /// The only string-only boundary. Keep both historical response shapes
    /// unchanged; ACP and the replay cache never serialize through this wire.
    fn legacy_body(&self) -> serde_json::Result<String> {
        match self {
            Self::Standard(result) => serde_json::to_string(&match result {
                Ok(reply) => BackgroundIpcResponse {
                    ok: true,
                    error: None,
                    data: reply.data.clone(),
                },
                Err((_, message)) => BackgroundIpcResponse::failed(message.clone()),
            }),
            Self::Command(result) => {
                serde_json::to_string(&rebon_session_host::BackgroundCommandResponse {
                    output: result.as_ref().ok().cloned(),
                    error: result.as_ref().err().map(|(_, message)| message.clone()),
                })
            }
        }
    }
}

/// Everything running one request needs from the worker around it.
///
/// A struct rather than sixteen parameters. Two callers pass this now -- the
/// legacy envelope path and the ACP dispatcher -- and sixteen positional
/// arguments, eleven of them `Arc<Mutex<_>>`, is a call nobody can read and
/// whose order the compiler cannot check.
pub(crate) struct RequestContext<'a> {
    pub(crate) store: &'a BackgroundStore,
    pub(crate) job_id: &'a str,
    pub(crate) owner: &'a BackgroundIpcOwner,
    pub(crate) turn_cancel: &'a Arc<Mutex<rebon_types::PromptCancel>>,
    pub(crate) task_registry_resolver: &'a Arc<Mutex<Option<TaskRegistryResolver>>>,
    pub(crate) teammate_runtimes: &'a Arc<Mutex<Vec<BackgroundTeammateRuntime>>>,
    pub(crate) permission_responses: &'a Arc<
        Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<PermissionAnswer>>>,
    >,
    pub(crate) permission_rule_context: &'a Arc<Mutex<Option<BackgroundPermissionRuleContext>>>,
    pub(crate) stop_gate: &'a Arc<Mutex<Option<StopGate>>>,
    pub(crate) live_permission_mode: &'a SharedLivePermissionModeState,
    pub(crate) live_mcp_status: &'a SharedLiveMcpStatus,
    pub(crate) live_agent: &'a SharedLiveAgent,
    pub(crate) command_tx: &'a tokio::sync::mpsc::UnboundedSender<BackgroundCommandRequest>,
    pub(crate) recent_command_results: &'a SharedRecentCommandResults,
    pub(crate) events: &'a SessionEventStream,
    pub(crate) wake: &'a Arc<tokio::sync::Notify>,
}

impl RequestContext<'_> {
    /// Remember the typed answer shared by both protocols. A legacy first
    /// caller must not erase the kind before an ACP retry asks for it.
    pub(crate) fn record_answer(&self, command_id: Option<&str>, response: &WireResponse) {
        if let Some(command_id) = command_id {
            let mut results = self.recent_command_results.lock().expect("poisoned");
            results.remember(command_id.to_string(), response.clone());
            // RFC-0004 §11.2: a client whose connection died between the
            // command and its answer finds the outcome in the owner's status.
            results.note_last(
                command_id.to_string(),
                rebon_session_host::now_ms(),
                response.error().cloned(),
            );
        }
    }

    /// An answer already given for this id, if there is one.
    ///
    /// Read by both protocols for the same reason they write to it: a retry
    /// after a lost connection must be answered, not run again.
    pub(crate) fn remembered_answer(&self, command_id: Option<&str>) -> Option<WireResponse> {
        command_id.and_then(|id| {
            self.recent_command_results
                .lock()
                .expect("poisoned")
                .get(id)
        })
    }

    /// Why this request must not be run here, if it must not.
    ///
    /// Two different questions live in one order-sensitive ladder, and the
    /// order is the contract: a request that names the wrong job is answered
    /// "wrong job" whether or not its token was also wrong. Telling a caller
    /// which of its two mistakes came first is not a leak -- it named a job it
    /// could only have learned from the store -- and reordering the ladder
    /// would change what every existing client is told.
    ///
    /// `authenticated` is a closure so it is evaluated exactly where the token
    /// check has always sat. The legacy envelope carries a token per request;
    /// an ACP connection was authenticated once at `initialize` and passes a
    /// closure that is already true. Both then share the fence below it, which
    /// asks the other question: not "who are you" but "did you mean *this*
    /// worker", which any client can get wrong on any single message after a
    /// worker was replaced.
    ///
    /// The answer carries both the typed kind and the exact sentence. The
    /// legacy wire has one error channel and needs the sentence byte for byte;
    /// the ACP wire needs the kind, and reading it back out of the sentence
    /// would be the string-matching this protocol change exists to remove.
    pub(crate) fn refusal_for(
        &self,
        addressed_job: Option<&str>,
        addressed_session: Option<&str>,
        request: &BackgroundIpcRequest,
        must_name_a_target: bool,
        authenticated: impl FnOnce() -> bool,
    ) -> Option<(rebon_session_host::HostCallError, String)> {
        // An envelope must name something this worker is: the job (the fence
        // that stops a command reaching the process that replaced its intended
        // target) or the session (the routing key a client that found us
        // through `<sid>.owner.json` has). Naming neither is a client bug, not
        // a wildcard.
        let addressed_by_job = addressed_job == Some(self.job_id);
        let addresses_a_job = addressed_job.is_some();
        if addresses_a_job && !addressed_by_job {
            return Some((
                rebon_session_host::HostCallError::OwnerFence(
                    "background IPC job id mismatch".to_string(),
                ),
                "background IPC job id mismatch".to_string(),
            ));
        }
        // Naming neither is a client bug on a wire where every message is its
        // own unauthenticated connection: there, saying nothing would be a
        // wildcard. On a connection that was authenticated once against this
        // worker's token it is not -- the connection already said which worker
        // was meant, and saying it again per message would be ceremony.
        //
        // Everything below still applies either way. This is the *only* thing
        // the connection stands in for.
        if must_name_a_target && !addresses_a_job && addressed_session.is_none() {
            return Some((
                rebon_session_host::HostCallError::OwnerFence(
                    "background IPC envelope addresses neither a job nor a session".to_string(),
                ),
                "background IPC envelope addresses neither a job nor a session".to_string(),
            ));
        }
        if !authenticated() {
            return Some((
                rebon_session_host::HostCallError::Unauthenticated,
                "background IPC authentication failed".to_string(),
            ));
        }
        let state = match self.store.read_state(self.job_id) {
            Ok(state) => state,
            Err(error) => {
                // The job record could not be read. Not the caller's mistake, so
                // not a fence: a client is told the owner could not be reached
                // and may try again.
                return Some((
                    rebon_session_host::HostCallError::Transport(error.to_string()),
                    error.to_string(),
                ));
            }
        };
        if !self.owner.matches(&state) && !matches!(request, BackgroundIpcRequest::Cancel { .. }) {
            return Some((
                rebon_session_host::HostCallError::OwnerFence(
                    "background IPC endpoint no longer owns this job".to_string(),
                ),
                "background IPC endpoint no longer owns this job".to_string(),
            ));
        }
        if addressed_session.is_some()
            && state.identity.session_id.is_some()
            && addressed_session != state.identity.session_id.as_deref()
        {
            return Some((
                rebon_session_host::HostCallError::OwnerFence(
                    "background IPC session id mismatch".to_string(),
                ),
                "background IPC session id mismatch".to_string(),
            ));
        }
        // Addressed by session alone, and this worker has no session bound yet
        // (or a different one): it cannot be the owner the client resolved.
        //
        // The `is_some` used to be implied: on the legacy wire, naming neither
        // was already refused above, so "named no job" meant "named a session".
        // A connection that met that requirement once, at `initialize`, can name
        // neither -- and then this check has nothing to compare and must not
        // fire. Spelled out rather than implied, because the thing implying it
        // is now conditional.
        if !addresses_a_job
            && addressed_session.is_some()
            && state.identity.session_id.as_deref() != addressed_session
        {
            return Some((
                rebon_session_host::HostCallError::OwnerFence(
                    "background IPC session is not open on this worker".to_string(),
                ),
                "background IPC session is not open on this worker".to_string(),
            ));
        }
        None
    }
}

pub(crate) fn handle_background_ipc_stream(
    mut stream: TcpStream,
    store: BackgroundStore,
    job_id: String,
    owner: BackgroundIpcOwner,
    turn_cancel: Arc<Mutex<rebon_types::PromptCancel>>,
    task_registry_resolver: Arc<Mutex<Option<TaskRegistryResolver>>>,
    teammate_runtimes: Arc<Mutex<Vec<BackgroundTeammateRuntime>>>,
    permission_responses: Arc<
        Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<PermissionAnswer>>>,
    >,
    permission_rule_context: Arc<Mutex<Option<BackgroundPermissionRuleContext>>>,
    stop_gate: Arc<Mutex<Option<StopGate>>>,
    live_permission_mode: SharedLivePermissionModeState,
    live_mcp_status: SharedLiveMcpStatus,
    live_agent: SharedLiveAgent,
    command_tx: tokio::sync::mpsc::UnboundedSender<BackgroundCommandRequest>,
    recent_command_results: SharedRecentCommandResults,
    events: SessionEventStream,
    wake: Arc<tokio::sync::Notify>,
) {
    // A client that lost its connection mid-command retries with the same
    // `command_id`. Answering from memory is the difference between a retried
    // `/compact` and two compactions.
    let mut answered_command_id: Option<String> = None;
    // Built once and shared by both protocols, so neither can be handed a
    // different worker than the other.
    let context = RequestContext {
        store: &store,
        job_id: &job_id,
        owner: &owner,
        turn_cancel: &turn_cancel,
        task_registry_resolver: &task_registry_resolver,
        teammate_runtimes: &teammate_runtimes,
        permission_responses: &permission_responses,
        permission_rule_context: &permission_rule_context,
        stop_gate: &stop_gate,
        live_permission_mode: &live_permission_mode,
        live_mcp_status: &live_mcp_status,
        live_agent: &live_agent,
        command_tx: &command_tx,
        recent_command_results: &recent_command_results,
        events: &events,
        wake: &wake,
    };
    // One port, two protocols. Which one this connection is, is decided
    // once, on its first frame, because the two have different connection
    // lifetimes -- a legacy client sends one line and waits, an ACP client
    // keeps the socket. The read half is a second handle on the same socket so
    // the probe's buffer can be handed to the ACP loop without borrowing the
    // handle the answers are written through.
    let read_half = match stream.try_clone() {
        Ok(half) => half,
        Err(error) => {
            // Every `return` from here to the answer leaves the caller with a
            // closed connection and no reply, which reads to it as a reset
            // rather than as a refusal. Each one says why, so the next time
            // a client reports "connection reset" the owner's log can be
            // asked whether it was the one that closed.
            tracing::debug!(
                job_id,
                %error,
                "rebon: dropping an IPC connection unanswered; the socket could not be cloned"
            );
            return;
        }
    };
    let mut reader = std::io::BufReader::new(read_half);
    let legacy_line = match super::wire_probe::classify_first_frame(&mut reader) {
        Ok(super::wire_probe::FirstFrame::Legacy { line }) => {
            // Nothing more will arrive on this connection, so the second
            // handle has no further use.
            drop(reader);
            line
        }
        Ok(super::wire_probe::FirstFrame::Acp { framing, body }) => {
            super::acp_connection::serve_acp_connection(
                reader,
                stream,
                framing,
                body,
                &owner.endpoint.token,
                &context,
            );
            return;
        }
        // A peer that opened a socket and said nothing. Ordinary: a health
        // check that asks whether anything is listening does exactly this,
        // and so does the connection shutdown makes to wake the accept
        // thread. Nothing to answer, and nothing to say about it.
        Ok(super::wire_probe::FirstFrame::Closed) => return,
        // A peer whose socket broke before it finished its first frame. Not
        // the same thing, and it used to share the arm above -- so a
        // connection that died mid-request was indistinguishable from a
        // health check, in a file whose whole job is to answer requests.
        Err(error) => {
            tracing::debug!(
                job_id,
                %error,
                "rebon: dropping an IPC connection unanswered; its first frame did not arrive"
            );
            return;
        }
    };
    let response = match serde_json::from_str::<BackgroundIpcEnvelope>(&legacy_line) {
        Ok(envelope) => {
            // An envelope must name something this worker is: the job (the
            // fence that stops a command reaching the process that replaced
            // its intended target) or the session (the routing key a client
            // that found us through `<sid>.owner.json` has). Naming neither is
            // a client bug, not a wildcard.
            // Retain the category until the legacy encoder below discards it.
            let rejection = context.refusal_for(
                envelope.job_id.as_deref(),
                envelope.session_id.as_deref(),
                &envelope.request,
                // Every envelope is its own unauthenticated connection, so
                // it has to say which worker it meant.
                true,
                || constant_time_eq(&envelope.token, &owner.endpoint.token),
            );
            let remembered = context.remembered_answer(envelope.command_id.as_deref());
            if let Some(error) = rejection {
                WireResponse::Standard(Err(error))
            } else if let Some(answer) = remembered {
                answer
            } else if let BackgroundIpcRequest::Subscribe { since } = envelope.request {
                // The one request that does not answer and hang up. Hand the
                // connection to the streaming loop, which owns it until one
                // side goes away.
                stream_session_events(
                    stream,
                    &store,
                    &job_id,
                    &live_permission_mode,
                    &live_mcp_status,
                    &live_agent,
                    &events,
                    &recent_command_results,
                    since,
                );
                return;
            } else {
                // Remember only what actually ran: a rejected envelope must
                // stay rejectable, and a retry of it must not be answered from
                // a cache of the refusal.
                answered_command_id = envelope.command_id.clone();
                execute_background_request(
                    envelope.request,
                    envelope.command_id.as_deref(),
                    &context,
                )
            }
        }
        Err(error) => {
            WireResponse::Standard(Err((HostCallError::InvalidRequest, error.to_string())))
        }
    };
    // The answer, and the three ways it can fail to arrive. They were three
    // `let _ =`: an owner that could not deliver what it had already done left
    // no trace of it anywhere, and the client saw a connection that closed
    // without answering.
    context.record_answer(answered_command_id.as_deref(), &response);
    if let Ok(body) = response.legacy_body() {
        if let Err(error) = stream.write_all(body.as_bytes()) {
            tracing::debug!(
                job_id,
                %error,
                "rebon: an IPC answer could not be written; the request ran and the caller will not hear it"
            );
            return;
        }
    }
    if let Err(error) = stream.write_all(b"\n").and_then(|()| stream.flush()) {
        tracing::debug!(
            job_id,
            %error,
            "rebon: an IPC answer could not be finished; the request ran and the caller may not read it"
        );
    }
}

/// Run one request and produce the answer, whichever protocol asked for it.
///
/// The legacy envelope path and the ACP dispatcher both come through here, so
/// the two cannot drift: a compatibility period where each protocol had its
/// own copy of what `_session/rewind` means would be a period where the answer
/// depended on which client you happened to use.
pub(crate) fn execute_background_request(
    request: BackgroundIpcRequest,
    command_id: Option<&str>,
    context: &RequestContext<'_>,
) -> WireResponse {
    // Register the call so `CancelCall` has something to find. Held for the
    // whole request and dropped however it leaves, so the map is empty again
    // whichever way this ends.
    let in_flight =
        command_id.map(|id| InFlightGuard::register(context.recent_command_results, id));
    match request {
        BackgroundIpcRequest::RunCommand { name, args } => WireResponse::Command(
            run_command_on_turn_loop(context.command_tx, name, args, in_flight.as_ref()),
        ),
        // Some typed requests are the same work a slash command does, and that
        // work only means anything on the turn loop, where the live session
        // is. Composing the command here rather than making every client spell
        // it keeps one implementation of what each one means; the rest are job
        // record changes and answer without waiting on a turn.
        request => match session_command_for_request(&request) {
            Some((name, args)) => WireResponse::Standard(
                match run_command_on_turn_loop(context.command_tx, name, args, in_flight.as_ref()) {
                    Ok(output) => reply_with_data(&output),
                    Err(error) => Err(error),
                },
            ),
            None => {
                let changes_shared_state = changes_shared_session_state(&request);
                let answer = handle_background_ipc_request(
                    request,
                    context.store,
                    context.job_id,
                    context.owner,
                    context.events,
                    context.turn_cancel,
                    context.task_registry_resolver,
                    context.teammate_runtimes,
                    context.permission_responses,
                    context.permission_rule_context,
                    context.stop_gate,
                    context.live_permission_mode,
                    context.live_mcp_status,
                    context.live_agent,
                    context.recent_command_results,
                );
                // Whatever it changed, the turn loop may be waiting for it.
                if answer.is_ok() {
                    context.wake.notify_one();
                }
                // Invariant I4: a change one client made has to reach the
                // others, or the terminal shows `plan` while the owner
                // enforces `default`. Pushed only when it worked -- a refusal
                // changed nothing.
                if answer.is_ok() && changes_shared_state {
                    if let Ok(state) = context.store.read_state(context.job_id) {
                        context.events.publish_status(session_status_snapshot(
                            &state,
                            context.live_permission_mode,
                            context.live_mcp_status,
                            context.live_agent,
                            context.recent_command_results,
                        ));
                    }
                }
                WireResponse::Standard(answer)
            }
        },
    }
}

/// The pending permission this subscriber has not been told about, as a line.
///
/// `None` when there is no permission waiting, or when the replay already
/// carried this one. The duplicate matters: a client that hears the same
/// question twice raises it twice, and not every consumer of the stream is
/// idempotent about it -- `rebon serve` keys by query id and ignores the
/// repeat, but the terminal mirror pushes one word per event.
///
/// The cursor is the one this subscriber has already reached, because this is
/// a restatement of current state rather than a new event: nothing else is
/// numbered here, and a client's watermark passes permissions through without
/// comparing cursors (`stream_watermark::drain_updates_with`).
pub(crate) fn restated_pending_permission(
    state: &BackgroundJobState,
    replay: &[String],
    cursor: u64,
) -> Option<rebon_session_host::SessionEvent> {
    let query = state.outcome.pending_permission.clone()?;
    let already_sent = replay.iter().any(|line| {
        matches!(
            serde_json::from_str::<rebon_session_host::SessionEvent>(line),
            Ok(rebon_session_host::SessionEvent::Permission { query: replayed, .. })
                if replayed.query_id == query.query_id
        )
    });
    if already_sent {
        return None;
    }
    Some(rebon_session_host::SessionEvent::Permission {
        cursor,
        query: Box::new(query),
    })
}

/// Whether a request changes state every attached client has to agree with.
///
/// Answering a permission and cancelling a turn count: both move the session
/// out of a state the other clients are still rendering.
fn changes_shared_session_state(request: &BackgroundIpcRequest) -> bool {
    matches!(
        request,
        BackgroundIpcRequest::SetPermissionMode { .. }
            | BackgroundIpcRequest::SetSessionOption { .. }
            | BackgroundIpcRequest::PermissionAnswer { .. }
            | BackgroundIpcRequest::AnswerQuestions { .. }
            | BackgroundIpcRequest::Cancel { .. }
            | BackgroundIpcRequest::Lease { .. }
            | BackgroundIpcRequest::ReleaseLease { .. }
    )
}

/// Serve one `Subscribe` connection until the client hangs up or falls behind.
///
/// Order matters: `hello` first, carrying the snapshot the client must agree
/// with before it interprets a single delta, then whatever the ring still held
/// from `since`, then the live stream. A client that reads them in that order
/// can never apply an update against a state it has not seen.
fn stream_session_events(
    mut stream: TcpStream,
    store: &BackgroundStore,
    job_id: &str,
    live_permission_mode: &SharedLivePermissionModeState,
    live_mcp_status: &SharedLiveMcpStatus,
    live_agent: &SharedLiveAgent,
    events: &SessionEventStream,
    recent_command_results: &SharedRecentCommandResults,
    since: Option<u64>,
) {
    // Attach before reading the state, not after. An event published in
    // between then arrives on the stream — a status the client already has in
    // `hello` is harmless, a session update it never sees is a hole in its
    // transcript.
    let subscription = events.subscribe(since);
    let Ok(state) = store.read_state(job_id) else {
        let _ = serde_json::to_writer(
            &mut stream,
            &BackgroundIpcResponse::failed("the job record could not be read"),
        );
        let _ = stream.write_all(b"\n");
        return;
    };
    let snapshot = session_status_snapshot(
        &state,
        live_permission_mode,
        live_mcp_status,
        live_agent,
        recent_command_results,
    );
    let turn_generation = snapshot.turn_generation;
    tracing::debug!(
        job_id,
        since,
        cursor = subscription.cursor,
        replayed = subscription.replay.len(),
        subscribers = events.subscriber_count(),
        "rebon: session event subscriber attached"
    );
    let hello = rebon_session_host::SessionEvent::Hello {
        cursor: subscription.cursor,
        turn_generation,
        status: Box::new(snapshot),
        epoch: events.epoch(),
    };
    // A subscriber writes for as long as the session runs, so the socket must
    // not carry the short timeout a request/response exchange uses.
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    if write_event_line(
        &mut stream,
        &serde_json::to_string(&hello).unwrap_or_default(),
    )
    .is_err()
    {
        return;
    }
    // A permission raised before this client attached reaches it here or not
    // at all. A fresh subscriber (`since: None`) is given no replay on
    // purpose -- it reads history from the transcript -- and a pending
    // permission is not history: it is a question still waiting, and a client
    // that never hears it leaves the tool blocked until the turn is
    // cancelled. So it is restated to this subscriber alone, and only when the
    // replay did not already carry it.
    let restated = restated_pending_permission(&state, &subscription.replay, subscription.cursor)
        .and_then(|event| serde_json::to_string(&event).ok());
    for line in subscription.replay {
        if write_event_line(&mut stream, &line).is_err() {
            return;
        }
    }
    if let Some(line) = restated {
        if write_event_line(&mut stream, &line).is_err() {
            return;
        }
    }
    while let Ok(line) = subscription.events.recv() {
        if write_event_line(&mut stream, &line).is_err() {
            return;
        }
    }
    // The channel closed: either this client fell a whole backlog behind and
    // the owner let it go, or the owner is shutting down. Say which as far as
    // we can — a client told it has a gap re-reads the transcript, a client
    // left guessing renders a hole.
    let gap = rebon_session_host::SessionEvent::Gap {
        from: subscription.cursor,
        to: events.cursor(),
    };
    let _ = write_event_line(
        &mut stream,
        &serde_json::to_string(&gap).unwrap_or_default(),
    );
}

fn write_event_line(stream: &mut TcpStream, line: &str) -> std::io::Result<()> {
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

/// Hand one slash command to the turn loop and wait for what it printed.
///
/// The wait is bounded by the command's own ceiling rather than a flat one:
/// `/compact` runs a summariser over the whole conversation, and a 30s ceiling
/// would time the caller out on essentially every successful compaction.
fn run_command_on_turn_loop(
    command_tx: &tokio::sync::mpsc::UnboundedSender<BackgroundCommandRequest>,
    name: String,
    args: Vec<String>,
    in_flight: Option<&InFlightGuard>,
) -> RequestResult<rebon_session_host::CommandOutput> {
    let deadline = std::time::Instant::now() + rebon_session_host::command_response_timeout(&name);
    let (response_tx, response_rx) = std::sync::mpsc::channel();
    command_tx
        .send(BackgroundCommandRequest {
            name,
            args,
            response_tx,
        })
        .map_err(|_| {
            let message = "background command channel is closed".to_string();
            (HostCallError::Transport(message.clone()), message)
        })?;
    // Waited in slices rather than in one `recv_timeout`, so a `CancelCall`
    // arriving on another connection is noticed while this one is still
    // waiting. Before that the only way out of here was the ceiling, and a
    // client that had already given up left the owner working for minutes on
    // an answer nobody would read.
    //
    // The command itself is not rolled back: it is on the turn loop and may
    // already have run. What a cancel buys is the waiter, and the discarding of
    // an answer that is no longer wanted (design §5.3 item 4).
    loop {
        if in_flight.is_some_and(InFlightGuard::is_cancelled) {
            return Err((
                HostCallError::Cancelled,
                "background command was cancelled by the client".to_string(),
            ));
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err((
                HostCallError::HostUnanswered,
                "background command timed out".to_string(),
            ));
        }
        match response_rx.recv_timeout(remaining.min(CANCEL_POLL_INTERVAL)) {
            Ok(result) => return result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let message = "background command channel is closed".to_string();
                return Err((HostCallError::Transport(message.clone()), message));
            }
        }
    }
}

/// How often a waiting command checks whether its client gave up.
///
/// Short enough that a cancel is felt as promptly as the client's own timeout,
/// long enough that a `/compact` running for minutes does not spin.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The slash command a typed session request is spelled as, when it is one.
///
/// `None` means the request changes the job record rather than the live
/// session, and is answered without waiting for the turn loop to be free.
pub(crate) fn session_command_for_request(
    request: &BackgroundIpcRequest,
) -> Option<(String, Vec<String>)> {
    match request {
        // Which agent (or kernel) runs the session is a live switch the turn
        // loop performs; `/backend` rather than `/agent` on purpose, because in
        // a terminal `/agent` also means "spawn a sub-agent with this prompt"
        // and answering to that name here would let a client send a prompt down
        // this channel.
        BackgroundIpcRequest::SetSessionOption { key, value }
            if matches!(session_option_key(key), SessionOptionKey::Agent) =>
        {
            Some(("backend".to_string(), vec![value.clone()]))
        }
        BackgroundIpcRequest::SetSessionOption { key, value }
            if matches!(session_option_key(key), SessionOptionKey::Kernel) =>
        {
            Some(("kernel".to_string(), vec![value.clone()]))
        }
        BackgroundIpcRequest::Compact { instructions } => Some((
            "compact".to_string(),
            instructions
                .as_ref()
                .filter(|text| !text.trim().is_empty())
                .map(|text| vec![text.clone()])
                .unwrap_or_default(),
        )),
        // The transcript rewrite is a compare-and-swap against the chain the
        // owner holds, so only the owner can do it soundly — and only from
        // the turn loop, where that chain lives.
        BackgroundIpcRequest::Rewind {
            user_message_uuid,
            scope,
        } => Some((
            "rewind".to_string(),
            vec![
                user_message_uuid.clone(),
                scope_argument(*scope).to_string(),
            ],
        )),
        _ => None,
    }
}

fn scope_argument(scope: rebon_session_host::RewindScopeWire) -> &'static str {
    match scope {
        rebon_session_host::RewindScopeWire::Conversation => "conversation",
        rebon_session_host::RewindScopeWire::Code => "code",
        rebon_session_host::RewindScopeWire::Both => "both",
    }
}

pub(crate) fn with_current_turn_locked<T>(
    turn_cancel: &Arc<Mutex<rebon_types::PromptCancel>>,
    action: impl FnOnce(&rebon_types::PromptCancel) -> T,
) -> T {
    let current = turn_cancel.lock().expect("poisoned");
    action(&current)
}

pub(crate) fn handle_background_ipc_request(
    request: BackgroundIpcRequest,
    store: &BackgroundStore,
    job_id: &str,
    owner: &BackgroundIpcOwner,
    events: &SessionEventStream,
    turn_cancel: &Arc<Mutex<rebon_types::PromptCancel>>,
    task_registry_resolver: &Arc<Mutex<Option<TaskRegistryResolver>>>,
    teammate_runtimes: &Arc<Mutex<Vec<BackgroundTeammateRuntime>>>,
    permission_responses: &Arc<
        Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<PermissionAnswer>>>,
    >,
    permission_rule_context: &Arc<Mutex<Option<BackgroundPermissionRuleContext>>>,
    stop_gate: &Arc<Mutex<Option<StopGate>>>,
    live_permission_mode: &SharedLivePermissionModeState,
    live_mcp_status: &SharedLiveMcpStatus,
    live_agent: &SharedLiveAgent,
    recent_command_results: &SharedRecentCommandResults,
) -> RequestResult {
    match request {
        BackgroundIpcRequest::RunCommand { .. } => {
            request_refused("RunCommand must use the command response path".to_string())
        }
        BackgroundIpcRequest::Ping => Ok(HostReply::default()),
        BackgroundIpcRequest::CancelCall { command_id } => {
            // The client gave up on `command_id` and has already released its
            // own waiter; this releases the owner's matching half.
            //
            // The idempotency history is left alone on purpose (design §5.3
            // item 4): an operation past its commit point still finishes, and a
            // retry carrying the same id must replay that one result rather
            // than running the command again.
            let released = recent_command_results
                .lock()
                .expect("poisoned")
                .cancel_call(&command_id);
            tracing::debug!(
                command_id = %command_id,
                released,
                "rebon: a client gave up on a call"
            );
            // `ok` either way, and deliberately: "there was nothing to cancel"
            // is the same outcome the caller wanted — the call is not running
            // here any more. A refusal would read to the client as an owner too
            // old to know the request, which would be a lie.
            reply_with_data(&serde_json::json!({ "released": released }))
        }
        BackgroundIpcRequest::ReconcilePlugins => {
            // Another process wrote a plugin switch; this owner's registry is
            // the one the session's tools come from, so reconcile it now.
            let registry = rebon_harness::kernel_bootstrap::process_plugin_registry();
            let report =
                registry.reconcile(&rebon_harness::kernel_bootstrap::desired_from_settings());
            reply_with_data(&serde_json::json!({
                "generation": report.generation,
                "loaded": report.loaded,
                "unloaded": report.unloaded,
                "failed": report.failed,
                "cascaded": report.cascaded,
            }))
        }
        BackgroundIpcRequest::SetPermissionMode { mode } => {
            let mode = match crate::host::runtime_permissions::permission_mode_from_wire_opt(Some(
                &mode,
            )) {
                Ok(Some(mode)) => mode,
                Ok(None) => unreachable!("a permission mode was provided"),
                Err(error) => {
                    return Err(request_error(error));
                }
            };
            let cell = {
                let mut live = live_permission_mode.lock().expect("poisoned");
                live.desired = Some(mode);
                live.cell.clone()
            };
            if let Some(cell) = cell {
                *cell.lock().expect("mode cell poisoned") = mode;
            }
            // Publish the mode that is now in force. Without this it lives
            // only in this worker's memory, and every other client attached
            // to this job keeps displaying whatever it last knew — a UI
            // claiming `default` while the worker enforces `plan`, or worse
            // the other way round. It also means a respawned worker comes
            // back under the mode the user chose, not the one it booted with.
            if let Err(err) = store.update_state(job_id, |state| {
                owner.ensure_matches(state)?;
                state.identity.runtime.permission_mode = Some(mode.as_wire().to_string());
                state.process.updated_at_ms = rebon_session_host::now_ms();
                Ok(())
            }) {
                tracing::warn!(%err, "could not publish the permission mode to the job state");
            }
            Ok(HostReply::default())
        }
        BackgroundIpcRequest::Reply { message, images } => {
            let message = message.trim().to_string();
            if message.is_empty() {
                return request_refused("background reply is empty".to_string());
            }
            match queue_live_background_reply(store, job_id, owner, message.clone(), images) {
                Ok(()) => {
                    let _ = store.append_event(
                        job_id,
                        "reply_received_ipc",
                        serde_json::json!({ "message": message, "nonInterrupting": true }),
                    );
                    Ok(HostReply::default())
                }
                Err(err) => Err(request_error(err)),
            }
        }
        BackgroundIpcRequest::ReplyTask { task_id, message } => {
            let message = message.trim().to_string();
            if message.is_empty() {
                return request_refused("background task reply is empty".to_string());
            }
            let state = match store.read_state(job_id) {
                Ok(state) => state,
                Err(err) => return Err(request_error(err)),
            };
            if let Err(err) = owner.ensure_matches(&state) {
                return Err(request_error(err));
            }
            let registry = match resolve_background_task_registry(task_registry_resolver, &state) {
                Ok(registry) => registry,
                Err(error) => return request_refused(error),
            };
            match reply_to_registered_background_task(
                &registry,
                teammate_runtimes,
                store,
                job_id,
                &task_id,
                message.clone(),
            ) {
                Ok(()) => {
                    let _ = store.append_event(
                        job_id,
                        "task_reply_received_ipc",
                        serde_json::json!({ "taskId": task_id, "message": message }),
                    );
                    Ok(HostReply::default())
                }
                Err(error) => request_refused(error),
            }
        }
        BackgroundIpcRequest::PermissionAnswer {
            query_id,
            turn_generation,
            option_id,
            extra_text,
            updated_input,
        } => send_background_permission_answer(
            store,
            job_id,
            owner,
            permission_responses,
            permission_rule_context,
            query_id,
            turn_generation,
            option_id,
            extra_text,
            updated_input,
        ),
        BackgroundIpcRequest::AnswerQuestions {
            query_id,
            turn_generation,
            answers,
        } => send_background_question_answer(
            store,
            job_id,
            owner,
            permission_responses,
            query_id,
            turn_generation,
            answers,
        ),
        BackgroundIpcRequest::CancelTasks { task_ids } => handle_cancel_tasks_request(
            store,
            job_id,
            owner,
            task_registry_resolver,
            teammate_runtimes,
            task_ids,
        ),
        BackgroundIpcRequest::Cancel { fence } => handle_cancel_request(
            store,
            job_id,
            owner,
            events,
            turn_cancel,
            permission_responses,
            stop_gate,
            recent_command_results,
            fence,
        ),
        BackgroundIpcRequest::Status => match store.read_state(job_id) {
            Ok(state) => reply_with_data(&session_status_snapshot(
                &state,
                live_permission_mode,
                live_mcp_status,
                live_agent,
                recent_command_results,
            )),
            Err(err) => Err(request_error(err)),
        },
        BackgroundIpcRequest::Steer { message, images } => {
            let message = message.trim().to_string();
            if message.is_empty() {
                return request_refused("background steer is empty".to_string());
            }
            // A steer and a reply reach the model the same way — the pending
            // prompt the attachment poller injects between tool rounds — so
            // the difference the caller cares about is not *how* it is
            // delivered but *what it lands in*: an already-running turn, or a
            // fresh one. Report which, so an ACP client can answer
            // `_session/steering` with the outcome its adapter contract
            // promises instead of guessing.
            let was_running = matches!(
                store.read_state(job_id).map(|state| state.process.status),
                Ok(BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput)
            );
            match queue_live_background_reply(store, job_id, owner, message.clone(), images) {
                Ok(()) => {
                    let _ = store.append_event(
                        job_id,
                        "steer_received_ipc",
                        serde_json::json!({ "message": message, "injected": was_running }),
                    );
                    reply_with_data(&serde_json::json!({
                        "outcome": if was_running { "injected" } else { "startedNewTurn" },
                    }))
                }
                Err(err) => Err(request_error(err)),
            }
        }
        BackgroundIpcRequest::Lease { client_id, kind } => {
            let client_id = client_id.trim().to_string();
            if client_id.is_empty() {
                return request_refused("a lease needs a client id".to_string());
            }
            let now = now_ms();
            let lease = rebon_session_host::ClientLease {
                client_id,
                kind,
                pid: None,
                updated_at_ms: now,
            };
            match store.update_state(job_id, |state| {
                owner.ensure_matches(state)?;
                state.touch_client_lease(lease.clone(), now);
                // Somebody wants this session again, so the shutdown a clean
                // `/exit` scheduled is off. Cleared on arrival rather than on
                // departure, so the signal cannot outlive the exit that meant
                // it and kill the next client's session the moment they look
                // away. `linger_ms` is not touched: that one is the job's own
                // configuration, not ours to rewrite.
                state.lease.exit_when_idle = false;
                state.process.updated_at_ms = now;
                Ok(state.lease.client_leases.len())
            }) {
                Ok(count) => reply_with_data(&serde_json::json!({
                    "leases": count,
                    "ttlMs": rebon_session_host::CLIENT_LEASE_TTL_MS,
                    "renewAfterMs": rebon_session_host::CLIENT_LEASE_RENEW_INTERVAL_MS,
                })),
                Err(err) => Err(request_error(err)),
            }
        }
        BackgroundIpcRequest::ReleaseLease {
            client_id,
            deliberate,
        } => handle_release_lease_request(store, job_id, owner, client_id, deliberate),
        BackgroundIpcRequest::SetSessionOption { key, value } => {
            set_session_option(store, job_id, owner, &key, &value)
        }
        // The live-switch options and `/compact` are answered on the turn loop
        // and never reach here.
        BackgroundIpcRequest::Compact { .. } => {
            request_refused("this request must use the command response path".to_string())
        }
        // Answered on the turn loop and never reaching here.
        BackgroundIpcRequest::Rewind { .. } => {
            request_refused("this request must use the command response path".to_string())
        }
        // Handled by the streaming path, which keeps the connection instead of
        // answering on it.
        BackgroundIpcRequest::Subscribe { .. } => {
            request_refused("Subscribe must use the streaming path".to_string())
        }
    }
}

/// Change a session option the owner holds.
///
/// `effort` and `model` live in the job record because that is where the
/// worker reads them: the effort level when it builds each turn, the model
/// when it builds the session. So they are answered here rather than on the
/// turn loop, and the reply says when the change lands rather than implying it
/// is already in force.
fn set_session_option(
    store: &BackgroundStore,
    job_id: &str,
    owner: &BackgroundIpcOwner,
    key: &str,
    value: &str,
) -> RequestResult {
    let value = value.trim().to_string();
    if value.is_empty() {
        return request_refused(format!("a value is required for `{key}`"));
    }
    let (field, applies_from) = match session_option_key(key) {
        SessionOptionKey::Effort => {
            // Reject an unknown level here rather than letting the worker fail
            // to start its next turn on it.
            if let Err(err) = crate::host::effort_level_from_wire(&value) {
                return Err(request_error(err));
            }
            (SessionOptionKey::Effort, "nextTurn")
        }
        // `nextTurn`, like effort, because the worker now re-reads the model at
        // the top of every turn rather than only when it builds the session.
        // It used to be `nextSession`, which was true and useless: the change
        // landed in the record, the answer said so, and the running session
        // went on using the old model for as long as it lived.
        SessionOptionKey::Model => (SessionOptionKey::Model, "nextTurn"),
        SessionOptionKey::Agent | SessionOptionKey::Kernel => {
            return request_refused(format!(
                "`{key}` is switched on the live session and must use the command path"
            ));
        }
        SessionOptionKey::Unknown => {
            return request_refused(format!("unknown session option `{key}`"));
        }
    };
    let now = now_ms();
    match store.update_state(job_id, |state| {
        owner.ensure_matches(state)?;
        // IPC 配置选项也属于手动覆盖，必须写目标 session，不能让首次选型在下一回合覆盖它。
        if let Some(session_id) = state.identity.session_id.as_deref() {
            let projects = store.root().join("projects");
            match field {
                SessionOptionKey::Effort => rebon_session::model_selection::save_manual_effort(
                    &projects,
                    &state.identity.cwd,
                    session_id,
                    Some(crate::host::effort_level_from_wire(&value)?),
                )?,
                SessionOptionKey::Model => {
                    if let Some(choice) = rebon_session::model_selection::load(
                        &projects,
                        &state.identity.cwd,
                        session_id,
                    )? {
                        if let Some(provider) = choice
                            .provider
                            .or_else(|| state.identity.runtime.provider.clone())
                        {
                            rebon_session::model_selection::save_manual_model(
                                &projects,
                                &state.identity.cwd,
                                session_id,
                                &provider,
                                &value,
                            )?;
                        } else {
                            rebon_session::model_selection::supersede_pending_route(
                                &projects,
                                &state.identity.cwd,
                                session_id,
                            )?;
                        }
                    }
                }
                _ => {}
            }
        }
        match field {
            SessionOptionKey::Effort => state.identity.runtime.effort_level = Some(value.clone()),
            SessionOptionKey::Model => state.identity.runtime.model = Some(value.clone()),
            _ => unreachable!("only the record-backed options reach the write"),
        }
        state.process.updated_at_ms = now;
        Ok(())
    }) {
        Ok(()) => {
            let _ = store.append_event(
                job_id,
                "session_option_set_ipc",
                serde_json::json!({ "key": key, "value": value }),
            );
            reply_with_data(&serde_json::json!({
                "key": key,
                "value": value,
                "appliesFrom": applies_from,
            }))
        }
        Err(err) => Err(request_error(err)),
    }
}

/// Everything a client must agree with the owner about (invariant I4).
pub(crate) fn session_status_snapshot(
    state: &BackgroundJobState,
    live_permission_mode: &SharedLivePermissionModeState,
    live_mcp_status: &SharedLiveMcpStatus,
    live_agent: &SharedLiveAgent,
    recent_command_results: &SharedRecentCommandResults,
) -> rebon_session_host::SessionStatusSnapshot {
    let last_command = recent_command_results.lock().expect("poisoned").last();
    // The mode in force is the one the turn loop is running under, which is
    // the live cell when there is one. `state.runtime.permission_mode` is the
    // published copy and can lag a `SetPermissionMode` by one write.
    let permission_mode = live_permission_mode
        .lock()
        .expect("poisoned")
        .in_force()
        .map(|mode| mode.as_wire().to_string())
        .or_else(|| state.identity.runtime.permission_mode.clone());
    let ask_user_questions = state
        .outcome
        .pending_permission
        .as_ref()
        .and_then(rebon_session_host::ask_user_questions_from_permission);
    let mut leases = state.lease.client_leases.clone();
    let now = now_ms();
    leases.retain(|lease| {
        now.saturating_sub(lease.updated_at_ms) < rebon_session_host::CLIENT_LEASE_TTL_MS
    });
    rebon_session_host::SessionStatusSnapshot {
        job_id: state.identity.job_id.clone(),
        session_id: state.identity.session_id.clone(),
        cwd: state.identity.cwd.clone(),
        status: state.process.status,
        busy: matches!(
            state.process.status,
            BackgroundJobStatus::Running | BackgroundJobStatus::Queued
        ),
        turn_generation: state.process.turn_generation,
        permission_mode,
        plan_mode: state
            .identity
            .runtime
            .permission_mode
            .as_deref()
            .is_some_and(|mode| mode == "plan"),
        model: state.identity.runtime.model.clone(),
        effort: state.identity.runtime.effort_level.clone(),
        // The session agent lives on the router in the worker's own process,
        // not in the job record: the record only ever holds the launch
        // default, so the live value is the one worth publishing.
        agent: live_agent.lock().expect("poisoned").clone(),
        pending_permission: state.outcome.pending_permission.clone(),
        ask_user_questions,
        usage: state.outcome.usage.clone(),
        mcp: live_mcp_status.lock().expect("poisoned").clone(),
        client_leases: leases,
        last_command_id: last_command.as_ref().map(|last| last.id.clone()),
        last_command_at_ms: last_command.as_ref().map_or(0, |last| last.at_ms),
        last_command_error: last_command
            .and_then(|last| last.error)
            .map(|(_, message)| message),
        updated_at_ms: state.process.updated_at_ms,
    }
}

/// Answer a `ReleaseLease` request.
fn handle_release_lease_request(
    store: &BackgroundStore,
    job_id: &str,
    owner: &BackgroundIpcOwner,
    client_id: String,
    deliberate: bool,
) -> RequestResult {
    let now = now_ms();
    match store.update_state(job_id, |state| {
        owner.ensure_matches(state)?;
        let released = state.release_client_lease(&client_id, now);
        if released {
            state.process.updated_at_ms = now;
        }
        // The user said they were done, nobody else is holding the
        // session, and it was never meant to outlive its terminal:
        // stop lingering. The linger exists for a client that might
        // come back, and a `/exit` is the user saying they will not —
        // waiting ten more minutes for them holds a whole plugin and
        // MCP stack open for a window that was closed on purpose.
        //
        // All three conditions carry weight. Without `deliberate` a
        // dropped ssh connection would kill the session it should have
        // waited for. Without the lease count, closing one of two
        // terminals would end the other's. And `Foreground` is the
        // record's own word for "somebody is watching this" — `/bg`
        // writes `Background` precisely when the terminal moves on and
        // the work is supposed to continue without it, so a job that
        // was handed off keeps its hour no matter how its watcher left.
        //
        // Written to the record rather than kept in this process
        // because the record is what the idle loop re-reads each tick;
        // taking a lease clears it again, so a session someone opens
        // later is not born already scheduled to die.
        if deliberate
            && released
            && state.lease.client_leases.is_empty()
            && state.lease.placement == rebon_session_host::JobPlacement::Foreground
        {
            state.lease.exit_when_idle = true;
            state.process.updated_at_ms = now;
        }
        Ok((released, state.lease.client_leases.len()))
    }) {
        Ok((released, count)) => reply_with_data(&serde_json::json!({
            "released": released,
            "leases": count,
        })),
        Err(err) => Err(request_error(err)),
    }
}

/// Answer a `Cancel` request.
///
/// The turn's Stop hook runs here, where the turn runs, before anything is
/// cancelled. A hook that says "keep going" makes this a
/// refusal the client reads as the request's failure -- what a local terminal
/// shows when its own Stop hook answers so.
fn handle_cancel_request(
    store: &BackgroundStore,
    job_id: &str,
    owner: &BackgroundIpcOwner,
    events: &SessionEventStream,
    turn_cancel: &Arc<Mutex<rebon_types::PromptCancel>>,
    permission_responses: &Arc<
        Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<PermissionAnswer>>>,
    >,
    stop_gate: &Arc<Mutex<Option<StopGate>>>,
    recent_command_results: &SharedRecentCommandResults,
    fence: rebon_session_host::BackgroundIpcCancelFence,
) -> RequestResult {
    // The turn's Stop hook runs here, where the turn runs, before
    // anything is cancelled. A hook that says "keep
    // going" makes this a refusal the client reads as the request's
    // failure — what a local terminal shows when its own Stop hook
    // answers so.
    let gate = stop_gate.lock().expect("poisoned").clone();
    if let Some(gate) = gate {
        let turn_running = store.read_state(job_id).is_ok_and(|state| {
            matches!(
                state.process.status,
                BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
            )
        });
        if turn_running {
            if let Err(reason) = gate("cancel") {
                let _ = store.append_event(
                    job_id,
                    "cancel_refused_by_stop_hook",
                    serde_json::json!({ "reason": reason }),
                );
                // Everybody watching this session is looking at a turn that
                // did not stop, not only whoever pressed stop. The answer
                // below still goes to the asker -- a client on the legacy wire
                // reads it there and has for as long as it has existed -- but
                // the reason belongs on the stream too, because that is the
                // line every client shares.
                //
                // It is also the only way an ACP client hears it at all:
                // `session/cancel` is a standard notification and has no
                // answer to carry one.
                events.publish_stop_refused(format!("Stop hook kept the turn running: {reason}"));
                return request_refused(format!("Stop hook kept the turn running: {reason}"));
            }
        }
    }
    let cancelled_at = now_ms();
    // Serialize queue removal with deferred completion. Claimed entries must
    // keep their generation until their executor returns; queued entries never
    // started and can be answered cancelled immediately.
    let mut recent = recent_command_results.lock().expect("poisoned");
    let mut cancelled_prompts = Vec::new();
    let mut running_generation = None;
    let (updated, cancelled_responses) = {
        let mut responses = permission_responses.lock().expect("poisoned");
        with_current_turn_locked(turn_cancel, |current_turn| {
            let updated = store.update_state(job_id, |state| {
                owner.ensure_matches(state)?;
                // The fence identifies the turn the caller observed:
                // generation, status, and the permission it was parked
                // on. `updated_at_ms` is deliberately NOT compared —
                // every appended event bumps it (10+/s while a model
                // streams), so requiring it unchanged between the
                // caller's read and this handler made stop lose the
                // race against its own turn essentially every time.
                if state.process.status != fence.status
                    || state.process.turn_generation != fence.turn_generation
                    || state
                        .outcome
                        .pending_permission
                        .as_ref()
                        .map(|permission| permission.query_id)
                        != fence.pending_permission_query_id
                {
                    return Err(anyhow::Error::new(HostCallError::StaleGeneration)
                        .context("background turn changed before cancellation"));
                }
                let cancelled_turn = matches!(
                    state.process.status,
                    BackgroundJobStatus::Queued
                        | BackgroundJobStatus::Running
                        | BackgroundJobStatus::NeedsInput
                );
                let had_pending_permission = state.outcome.pending_permission.take().is_some();
                if cancelled_turn {
                    cancelled_prompts = state.identity.pending_prompts.clone();
                    running_generation = matches!(
                        state.process.status,
                        BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
                    )
                    .then_some(state.process.turn_generation);
                    state.clear_pending_prompts();
                    state.process.status = BackgroundJobStatus::Idle;
                    state.outcome.summary = Some("turn cancelled".to_string());
                    state.outcome.summary_updated_at_ms = None;
                    state.process.completed_at_ms = None;
                    state.outcome.exit_code = None;
                    state.outcome.error = None;
                    state.process.updated_at_ms = cancelled_at;
                } else if had_pending_permission {
                    state.process.updated_at_ms = cancelled_at;
                }
                Ok((cancelled_turn, had_pending_permission))
            });
            let stale_owner = updated.as_ref().err().is_some_and(|error| {
                matches!(
                    error.downcast_ref::<HostCallError>(),
                    Some(HostCallError::OwnerFence(_))
                )
            });
            let cancelled_responses = if matches!(&updated, Ok((_, true))) || stale_owner {
                std::mem::take(&mut *responses)
            } else {
                std::collections::HashMap::new()
            };
            if matches!(updated.as_ref(), Ok((true, _))) || stale_owner {
                current_turn.cancel();
            }
            (updated, cancelled_responses)
        })
    };
    let deliveries = if updated.is_ok() {
        recent
            .prompts
            .cancelled(cancelled_prompts, running_generation)
    } else {
        Vec::new()
    };
    drop(recent);
    super::acp_prompt::deliver(deliveries);
    let cancelled_permission_count = cancelled_responses.len();
    for (_, sender) in cancelled_responses {
        let _ = sender.send(PermissionAnswer::Cancelled);
    }
    if cancelled_permission_count > 0 && updated.is_ok() {
        let _ = store.append_event(
            job_id,
            "permissions_cancelled_ipc",
            serde_json::json!({ "count": cancelled_permission_count }),
        );
    }
    match updated {
        Ok((cancelled_turn, _)) => {
            if !cancelled_turn {
                return Err((
                    HostCallError::StaleGeneration,
                    "background turn is no longer cancellable".to_string(),
                ));
            }
            Ok(HostReply::default())
        }
        Err(err) => Err(request_error(err)),
    }
}

/// Resolve only the registry belonging to the job's exact live session.
fn resolve_background_task_registry(
    resolver: &Arc<Mutex<Option<TaskRegistryResolver>>>,
    state: &BackgroundJobState,
) -> Result<Arc<TaskRegistry>, String> {
    let session_id = state
        .identity
        .session_id
        .as_deref()
        .ok_or_else(|| "background job has no bound session".to_string())?;
    let resolver = resolver
        .lock()
        .expect("background task registry resolver poisoned")
        .clone()
        .ok_or_else(|| "task-registry resolver is not attached".to_string())?;
    resolver.resolve(session_id)
}

/// Answer a `CancelTasks` request.
fn handle_cancel_tasks_request(
    store: &BackgroundStore,
    job_id: &str,
    owner: &BackgroundIpcOwner,
    task_registry_resolver: &Arc<Mutex<Option<TaskRegistryResolver>>>,
    teammate_runtimes: &Arc<Mutex<Vec<BackgroundTeammateRuntime>>>,
    task_ids: Vec<String>,
) -> RequestResult {
    let resolver = task_registry_resolver
        .lock()
        .expect("background task registry resolver poisoned")
        .clone();
    let cancellation = match store.update_state(job_id, |state| {
        owner.ensure_matches(state)?;
        let resolver = resolver
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("task-registry resolver is not attached"))?;
        let session_id = state
            .identity
            .session_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("background job has no bound session"))?;
        let registry = resolver.resolve(session_id).map_err(anyhow::Error::msg)?;
        Ok(cancel_registered_background_tasks(
            Some(&registry),
            &task_ids,
        ))
    }) {
        Ok(cancellation) => cancellation,
        Err(err) => {
            return Err(request_error(err));
        }
    };
    // Outside the job-state critical section: a registry whose tasks
    // had all settled has no bridge left, so without this the kill
    // never reaches the store and the client keeps rendering the row
    // in whatever state it was parked in.
    for registry in &cancellation.stopped_registries {
        ensure_bridge_for_registry(teammate_runtimes, registry, store, job_id);
    }
    // A row whose worker is gone is exactly the one a user reaches for
    // the stop button over, and the one nothing in this process can
    // act on. Refusing it left the row unstoppable for good; settle it
    // in the store, which every client projects from.
    let mut settled_orphans = Vec::new();
    let mut errors = cancellation.errors;
    for task_id in &cancellation.unknown_task_ids {
        match settle_unknown_task_in_store(store, job_id, task_id) {
            Ok(()) => settled_orphans.push(task_id.clone()),
            Err(error) => errors.push(format!(
                "no live worker holds task {task_id}, and closing the row out failed: {error}"
            )),
        }
    }
    // Bulk cancellation predates targeted remote control and stays
    // best-effort. A one-id request is the strict cancel_task path and
    // must report stale ids or terminal tasks to its caller.
    let single_task_error = (task_ids.len() == 1)
        .then(|| errors.first().cloned())
        .flatten();
    let _ = store.append_event(
        job_id,
        "tasks_cancelled_ipc",
        serde_json::json!({
            "requestedTaskIds": task_ids,
            "stoppedTaskIds": cancellation.stopped_task_ids,
            "settledOrphanTaskIds": settled_orphans,
            "errors": errors,
        }),
    );
    match single_task_error {
        Some(error) => request_refused(error),
        None => Ok(HostReply::default()),
    }
}

/// Forward engine permission queries out to whichever client is watching.
///
/// One thread for the server's whole life. It renumbers each query with an
/// id clients can answer against, expands the engine's single `allow_always`
/// into the same scoped candidates the TUI shows, and only publishes a query
/// once the record has accepted it: a query the state refused is one no client
/// may answer, and pushing it would put a prompt on three screens that nothing
/// can resolve.
#[allow(clippy::too_many_arguments)]
fn spawn_permission_forward_thread(
    store: &BackgroundStore,
    job_id: &str,
    owner: &BackgroundIpcOwner,
    token: &str,
    cancel: &rebon_types::PromptCancel,
    permission_receivers: &Arc<Mutex<Vec<BackgroundPermissionReceiver>>>,
    permission_responses: &Arc<
        Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<PermissionAnswer>>>,
    >,
    permission_rule_context: &Arc<Mutex<Option<BackgroundPermissionRuleContext>>>,
    events: &SessionEventStream,
) {
    let forward_store = store.clone();
    let forward_job_id = job_id.to_string();
    let forward_cancel = cancel.clone();
    let forward_receivers = Arc::clone(permission_receivers);
    let forward_responses = Arc::clone(permission_responses);
    let forward_rule_context = Arc::clone(permission_rule_context);
    let first_query_id = permission_query_id_seed(token);
    let forward_owner = owner.clone();
    let forward_events = events.clone();
    std::thread::spawn(move || {
        let mut next_query_id = first_query_id;
        loop {
            if forward_cancel.is_cancelled() {
                return;
            }
            let query = {
                let mut receivers = forward_receivers.lock().expect("poisoned");
                let mut query = None;
                let mut index = 0;
                while index < receivers.len() {
                    match receivers[index].receiver.try_recv() {
                        Ok(value) => {
                            query = Some((receivers[index].turn_generation, value));
                            break;
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                            index += 1;
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                            receivers.swap_remove(index);
                        }
                    }
                }
                query
            };
            let Some((turn_generation, mut query)) = query else {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };

            let query_id = next_query_id;
            next_query_id = next_query_id.wrapping_add(1).max(1);
            query.id = query_id;
            // Expand the engine's single `allow_always` into the same
            // scoped candidates (exact + generalized, host-labelled)
            // the TUI shows, so remote clients render and answer the
            // full option set.
            let rule_context = forward_rule_context.lock().expect("poisoned").clone();
            if let Some(context) = rule_context.as_ref() {
                crate::permission_policy::add_allow_always_candidates(&mut query, &context.cwd);
            }
            let tool_name = query.tool_name.clone();
            let tool_call_id = query.tool_call_id.clone();
            let session_id = query.session_id.clone();
            let options_json = query
                .options
                .iter()
                .map(|option| {
                    serde_json::json!({
                        "optionId": option.option_id,
                        "label": option.label,
                        "kind": format!("{:?}", option.kind),
                    })
                })
                .collect::<Vec<_>>();
            let mut snapshot = background_permission_snapshot(&query);
            snapshot.turn_generation = turn_generation;
            snapshot.endpoint = Some(forward_owner.endpoint.clone());
            let streamed_snapshot = snapshot.clone();
            let rejected_sender = {
                let mut responses = forward_responses.lock().expect("poisoned");
                responses.insert(query_id, query.response_tx);
                let accepted = forward_store.update_state(&forward_job_id, |state| {
                    if !forward_owner.matches(state) {
                        return Ok(false);
                    }
                    let active_current_turn = state.process.turn_generation == turn_generation
                        && matches!(
                            state.process.status,
                            BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
                        );
                    let terminal_parent = matches!(
                        state.process.status,
                        BackgroundJobStatus::Succeeded | BackgroundJobStatus::Failed
                    );
                    if (!active_current_turn && !terminal_parent)
                        || state.outcome.pending_permission.is_some()
                    {
                        return Ok(false);
                    }
                    if active_current_turn && state.process.status == BackgroundJobStatus::Running {
                        state.process.status = BackgroundJobStatus::NeedsInput;
                        state.outcome.summary = Some(format!("needs input: allow {tool_name}?"));
                        state.outcome.summary_updated_at_ms = None;
                    }
                    state.outcome.pending_permission = Some(snapshot);
                    state.process.updated_at_ms = now_ms();
                    Ok(true)
                });
                match accepted {
                    Ok(true) => None,
                    Ok(false) | Err(_) => responses.remove(&query_id),
                }
            };
            if let Some(sender) = rejected_sender {
                let _ = sender.send(PermissionAnswer::Cancelled);
                continue;
            }
            // Only once the record accepted it: a query the state refused is a
            // query no client may answer, and pushing it would put a prompt on
            // three screens that nothing can resolve.
            forward_events.publish_permission(streamed_snapshot);
            let _ = forward_store.append_event(
                &forward_job_id,
                "permission_requested",
                serde_json::json!({
                    "queryId": query_id,
                    "tool": tool_name,
                    "toolCallId": tool_call_id,
                    "sessionId": session_id,
                    "toolInput": query.tool_input,
                    "options": options_json,
                }),
            );
            loop {
                if forward_cancel.is_cancelled() {
                    return;
                }
                let response_pending = {
                    let mut responses = forward_responses.lock().expect("poisoned");
                    if responses
                        .get(&query_id)
                        .is_some_and(tokio::sync::oneshot::Sender::is_closed)
                    {
                        responses.remove(&query_id);
                    }
                    responses.contains_key(&query_id)
                };
                if !response_pending {
                    let _ = forward_store.update_state(&forward_job_id, |state| {
                        if !forward_owner.matches(state) {
                            return Ok(());
                        }
                        if state
                            .outcome
                            .pending_permission
                            .as_ref()
                            .is_some_and(|pending| {
                                pending.query_id == query_id
                                    && pending.endpoint.as_ref() == Some(&forward_owner.endpoint)
                            })
                        {
                            let resumes_current_turn = state
                                .outcome
                                .pending_permission
                                .as_ref()
                                .is_some_and(|pending| {
                                    pending.turn_generation == state.process.turn_generation
                                });
                            state.outcome.pending_permission = None;
                            if resumes_current_turn
                                && state.process.status == BackgroundJobStatus::NeedsInput
                            {
                                state.process.status = BackgroundJobStatus::Running;
                                state.process.updated_at_ms = now_ms();
                            }
                        }
                        Ok(())
                    });
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    });
}

/// Accept client connections for this job's IPC endpoint.
///
/// One thread for the server's whole life. Every connection thread it starts
/// shares the same state handles, so a retried command reaches the same memory
/// the first attempt wrote to.
#[allow(clippy::too_many_arguments)]
fn spawn_ipc_accept_thread(
    running: &Arc<AtomicBool>,
    listener: TcpListener,
    store: &BackgroundStore,
    job_id: &str,
    owner: &BackgroundIpcOwner,
    cancel: &rebon_types::PromptCancel,
    turn_cancel: &Arc<Mutex<rebon_types::PromptCancel>>,
    task_registry_resolver: &Arc<Mutex<Option<TaskRegistryResolver>>>,
    teammate_runtimes: &Arc<Mutex<Vec<BackgroundTeammateRuntime>>>,
    permission_responses: &Arc<
        Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<PermissionAnswer>>>,
    >,
    permission_rule_context: &Arc<Mutex<Option<BackgroundPermissionRuleContext>>>,
    stop_gate: &Arc<Mutex<Option<StopGate>>>,
    live_permission_mode: &SharedLivePermissionModeState,
    live_mcp_status: &SharedLiveMcpStatus,
    live_agent: &SharedLiveAgent,
    command_tx: tokio::sync::mpsc::UnboundedSender<BackgroundCommandRequest>,
    recent_command_results: &SharedRecentCommandResults,
    events: &SessionEventStream,
    wake: &Arc<tokio::sync::Notify>,
) {
    let accept_running = Arc::clone(running);
    let accept_store = store.clone();
    let accept_job_id = job_id.to_string();
    let accept_owner = owner.clone();
    let accept_cancel = cancel.clone();
    let accept_turn_cancel = Arc::clone(turn_cancel);
    let accept_task_registry_resolver = Arc::clone(task_registry_resolver);
    let accept_teammate_runtimes = Arc::clone(teammate_runtimes);
    let accept_responses = Arc::clone(permission_responses);
    let accept_rule_context = Arc::clone(permission_rule_context);
    let accept_stop_gate = Arc::clone(stop_gate);
    let accept_live_permission_mode = Arc::clone(live_permission_mode);
    let accept_live_mcp_status = Arc::clone(live_mcp_status);
    let accept_live_agent = Arc::clone(live_agent);
    let accept_command_tx = command_tx.clone();
    let accept_recent_results = Arc::clone(recent_command_results);
    let accept_events = events.clone();
    let accept_wake = Arc::clone(wake);
    std::thread::spawn(move || {
        // Cleared however this returns, so a caller that asks whether the
        // thread is still there gets the answer from the thread itself.
        let _running = AcceptThreadMark(accept_running);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    // The shutdown wake-up, or a client that connected just as
                    // the owner stopped. Served either way, and then this
                    // thread leaves.
                    //
                    // Serving it matters: dropping a connection unread is a
                    // *reset*, and a reset reads to the caller as "the owner
                    // broke" rather than "the owner has stopped". The wake-up
                    // costs one handler that reads end-of-input and returns,
                    // which is what it was always going to do.
                    //
                    // The join below has one narrow, accepted cost: if what
                    // arrived at the moment of shutdown is a *subscription*,
                    // this thread waits for it to finish. The wait is bounded
                    // rather than open-ended -- `close_host_resources` releases
                    // the waiters, and a subscription ends when its event
                    // channel closes -- so what it delays is the moment
                    // `accept_thread_running` goes false, not the shutdown
                    // itself.
                    //
                    // The alternative, if that ever bites: have the handler
                    // signal once it has read the first frame, and wait on
                    // that signal rather than on the thread. It would cut the
                    // wait to the length of one read while keeping the answer
                    // going out. Recorded, not built: the cost above has not
                    // been observed to matter.
                    let stopping = accept_cancel.is_cancelled();
                    let _ = stream.set_nonblocking(false);
                    let store = accept_store.clone();
                    let job_id = accept_job_id.clone();
                    let owner = accept_owner.clone();
                    let turn_cancel = Arc::clone(&accept_turn_cancel);
                    let task_registry_resolver = Arc::clone(&accept_task_registry_resolver);
                    let teammate_runtimes = Arc::clone(&accept_teammate_runtimes);
                    let responses = Arc::clone(&accept_responses);
                    let rule_context = Arc::clone(&accept_rule_context);
                    let stop_gate = Arc::clone(&accept_stop_gate);
                    let live_permission_mode = Arc::clone(&accept_live_permission_mode);
                    let live_mcp_status = Arc::clone(&accept_live_mcp_status);
                    let live_agent = Arc::clone(&accept_live_agent);
                    let command_tx = accept_command_tx.clone();
                    let recent_results = Arc::clone(&accept_recent_results);
                    let events = accept_events.clone();
                    let wake = Arc::clone(&accept_wake);
                    let served = std::thread::spawn(move || {
                        handle_background_ipc_stream(
                            stream,
                            store,
                            job_id,
                            owner,
                            turn_cancel,
                            task_registry_resolver,
                            teammate_runtimes,
                            responses,
                            rule_context,
                            stop_gate,
                            live_permission_mode,
                            live_mcp_status,
                            live_agent,
                            command_tx,
                            recent_results,
                            events,
                            wake,
                        );
                    });
                    if stopping {
                        // Waited for, and the waiting is what makes this work:
                        // without it the same suite fails three times in ten
                        // runs where it fails once in sixteen with it. Both
                        // numbers were measured on this machine, the same way.
                        //
                        // The cost is narrow and real: if the connection that
                        // raced the stop happens to be a subscription, this
                        // thread waits for it to end. It holds nothing anyone
                        // else needs -- the listener is already closed to new
                        // work by the flag above -- but the "is the accept
                        // thread still running" flag stays true meanwhile, so
                        // a caller polling it sees a late exit rather than a
                        // prompt one.
                        let _ = served.join();
                        break;
                    }
                }
                Err(err) => {
                    let _ = accept_store.append_event(
                        &accept_job_id,
                        "ipc_accept_failed",
                        serde_json::json!({ "error": err.to_string() }),
                    );
                    break;
                }
            }
        }
    });
}

/// Clears the accept thread's "still running" flag however the thread leaves,
/// including a panic. A flag cleared only on the tidy path would report a
/// thread that died badly as still serving.
struct AcceptThreadMark(Arc<AtomicBool>);

impl Drop for AcceptThreadMark {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

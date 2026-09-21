//! Runs the Node plugin host and stays responsible for it.
//!
//! The protocol crate says what a frame means; [`rebon_node_runtime`] says which
//! Node may run; this crate is what actually starts a process, speaks NDJSON to
//! it over stdio, and decides what happens when it stops answering.
//!
//! # What supervision means here
//!
//! - **No caller waits forever.** Process exit, a poisoned decoder, or a failed
//!   write all end the same way: every in-flight call gets a synthesised error
//!   terminal naming the cause, and the host is marked unusable. See
//!   [`state`] — that rule is a pure function, so it is tested as one.
//! - **A crash has a diagnosis.** The host's stderr is drained into a bounded
//!   tail and travels in the failure, so "the plugin host died" is never the
//!   whole message.
//! - **Nothing restarts itself.** [`PluginHostSupervisor::restart`] exists and
//!   is explicit. A silent relaunch would hand a caller a host with no plugins
//!   loaded and no scopes open while looking like the one it had, and the
//!   epoch bump that makes late frames safe would be invisible.
//! - **A malformed frame is fatal.** Stdout is the protocol channel; anything
//!   that is not a frame means framing is lost, and continuing risks answering
//!   the wrong call.
//!
//! # Plugins
//!
//! [`PluginHostSupervisor::load_plugin`], [`PluginHostSupervisor::call_service`],
//! and [`PluginHostSupervisor::unload_plugin`] drive the host's loader and keep
//! this side's [`rebon_plugin_protocol::PluginRegistry`] in step with it, so
//! "which plugins are loaded and what may be called" has one answer here even
//! though the code being loaded lives in the other process.
//!
//! # Both directions
//!
//! Events travel down ([`PluginHostSupervisor::deliver_event`]) and requests
//! travel up. **Every request the host issues gets exactly one answer**, even
//! one naming a method this side does not implement: an unanswered request is a
//! plugin waiting forever, which is the same failure the downward direction
//! exists to prevent. The answer is committed through the ledger, so a second
//! one is refused rather than sent.
//!
//! # Tools
//!
//! `tool/invoke` is how a plugin reaches rebon's own capabilities, and it is the
//! only way. Three gates stand in front of it, in this order: the plugin's
//! manifest must have declared the tool, this build's exposed set must contain
//! it, and only then does the embedder's [`ToolInvoker`] run — which is where
//! the user's permission is asked. The first two run before any embedder code,
//! so a mistake there cannot widen the plane.
//!
//! # Events
//!
//! The plane runs both ways here too. `event/deliver` sends a plugin something
//! it subscribed to; `event/emit` takes something it published. Publishing is
//! gated the way providing directions always are — the manifest's
//! `publishedTopics` is the ceiling — and separately from the topics a plugin
//! listens to, because hearing a topic and being able to announce one are
//! different powers. What happens to a published event is the embedder's
//! [`EventPublisher`]; the identity riding it is injected here, so attribution
//! is not something a plugin can misreport.
//!
//! # What it does not do
//!
//! It does not sandbox anything: plugins are trusted local code by platform
//! decision. A method this build does not implement is refused by name, not
//! ignored.

pub mod state;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use rebon_plugin_protocol::{
    CallClosed, CallIdentity, CodecError, CommandInvokeRequest, EventDelivery, EventEmitRequest,
    EventSubscribeRequest, EventUnsubscribeRequest, LlmControlRequest, LlmStreamRequest,
    NdjsonCodec, Payload, PluginCommandDefinition, PluginDrainReport, PluginLoadRequest,
    PluginReadyReport, PluginToolDefinition, PluginUnloadRequest, RegistryError, SeatCallRequest,
    ServiceCallRequest, TerminalStatus, ToolInvokeRequest, WireEnvelope, WireMessage,
    CALL_CANCEL_METHOD, COMMAND_INVOKE_METHOD, EVENT_DELIVER_METHOD, EVENT_EMIT_METHOD,
    EVENT_SUBSCRIBE_METHOD, EVENT_UNSUBSCRIBE_METHOD, LLM_CONTROL_METHOD, LLM_STREAM_METHOD,
    PLATFORM_INITIALIZE_METHOD, PLATFORM_SHUTDOWN_METHOD, PLUGIN_LOAD_METHOD, PLUGIN_UNLOAD_METHOD,
    SCOPE_CLOSE_METHOD, SCOPE_OPEN_METHOD, SEAT_CALL_METHOD, SERVICE_CALL_METHOD, TOOL_CALL_METHOD,
    TOOL_INVOKE_METHOD,
};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot, Mutex},
};

pub use state::{HostFailure, Inbound, StateError, SupervisorState, HOST_FAILED_CODE};

/// Absolute path to the host's entry module, for deployments that place it
/// somewhere other than beside the executable.
pub const HOST_SCRIPT_ENV: &str = "REBON_PLUGIN_HOST_JS";

/// Where the composition loader is, when it is not next to the executable.
pub const COMPOSE_LOADER_ENV: &str = "REBON_COMPOSE_LOADER_JS";

/// How much of the host's stderr is kept for a failure message.
pub const DEFAULT_STDERR_TAIL_BYTES: usize = 8 * 1024;

/// How long a graceful shutdown waits for the child before it is killed.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The reason recorded when a host leaves because it was asked to.
///
/// A shut-down host is not alive — it is gone — but it did not crash, and a
/// caller deciding whether to report an incident needs to tell those apart.
pub const SHUTDOWN_REASON: &str = "plugin host shut down on request";

/// How long `platform/initialize` may take before the host is declared unusable.
///
/// Only the handshake is bounded. A `service/call` may legitimately run long and
/// the protocol's answer to that is `call/cancel`, not a timer the caller cannot
/// see; but a host that never answers initialize has nothing to cancel and would
/// hang whoever started it.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// One plugin's request to run a rebon tool, with who is asking attached.
///
/// The identity is injected by the host's bridge rather than reported by the
/// plugin, so `plugin_id` is an attribution an implementation can trust for
/// permission decisions and audit lines.
#[derive(Clone, Debug)]
pub struct ToolInvocation {
    pub identity: CallIdentity,
    pub tool: String,
    pub input: Payload,
}

/// Why a tool run was refused, in the code/message shape the wire uses.
#[derive(Clone, Debug)]
pub struct ToolRefusal {
    pub code: String,
    pub message: String,
}

impl ToolRefusal {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Runs one of rebon's own tools on a plugin's behalf.
///
/// This is where the plugin plane's `tool/invoke` lands, and it is deliberately
/// the *only* way a plugin reaches rebon's capabilities.
///
/// # The permission rule an implementation must keep
///
/// **Route the invocation through rebon's permission broker.** The protocol has
/// no `permission/ask` on purpose: a plugin able to raise a prompt of its own
/// wording could raise a misleading one, and a user approving text the plugin
/// wrote has not really approved anything. Asking on the plugin's behalf, in
/// rebon's words, is what keeps the prompt worth trusting.
///
/// Two gates have already run by the time this is called — the plugin's
/// manifest declared the tool, and the platform's exposed set contains it.
/// Neither of those says the *user* agreed to this particular use of it, which
/// is the question left for the implementation.
pub trait ToolInvoker: Send + Sync {
    fn invoke(
        &self,
        invocation: ToolInvocation,
    ) -> Pin<Box<dyn Future<Output = Result<Payload, ToolRefusal>> + Send + '_>>;
}

/// One plugin's request to call a kernel seat, with who is asking attached.
#[derive(Clone, Debug)]
pub struct SeatInvocation {
    pub identity: CallIdentity,
    pub seat: String,
    pub method: String,
    pub params: Payload,
}

/// Answers a plugin's `seat/call`.
///
/// Separate from [`ToolInvoker`] because the two are different questions. A
/// tool is a model-facing capability whose every use is the user's to permit; a
/// seat is a kernel service the plugin was installed to use. Collapsing them
/// would either put a permission prompt in front of a logger call or drop it
/// from in front of a file read.
///
/// Note what a seat call cannot serve: a cordis disposer is synchronous and
/// cannot await, so anything a teardown path depends on has to be a declaration
/// the protocol already unwinds, not a call made on the way out.
pub trait SeatDispatcher: Send + Sync {
    fn call(
        &self,
        invocation: SeatInvocation,
    ) -> Pin<Box<dyn Future<Output = Result<Payload, ToolRefusal>> + Send + '_>>;
}

/// One event a plugin published, with who published it attached.
#[derive(Clone, Debug)]
pub struct PublishedEvent {
    pub identity: CallIdentity,
    pub topic: String,
    pub event: Payload,
}

/// Receives a plugin's `event/emit`.
///
/// The other half of the event plane from delivery: rebon decides who hears a
/// published event, exactly as it decides what a subscribed plugin is sent.
///
/// Attribution is not optional here. The identity on the invocation is injected
/// by the host rather than reported by the plugin, so an implementation always
/// knows which plugin and which scope incarnation an event came from — which is
/// what makes an event plane shared by mutually-trusting plugins auditable
/// rather than anonymous.
pub trait EventPublisher: Send + Sync {
    fn publish(
        &self,
        event: PublishedEvent,
    ) -> Pin<Box<dyn Future<Output = Result<Payload, ToolRefusal>> + Send + '_>>;
}

#[derive(Clone)]
pub struct HostConfig {
    /// Absolute Node executable, normally from
    /// `rebon_node_runtime::NodeRuntimeResolver`.
    pub node: PathBuf,
    /// Absolute path to the host's `cli.mjs`.
    pub host_script: PathBuf,
    /// Extra arguments handed to the host script.
    ///
    /// The one that matters today is `--loader <module>`, which tells a generic
    /// host what a plugin package may be — a Cordis composition entry, say. The
    /// host refuses to start rather than falling back if it cannot build the
    /// loader it was pointed at, so this is a demand rather than a hint.
    pub host_args: Vec<String>,
    pub working_directory: PathBuf,
    pub host_epoch: u64,
    pub stderr_tail_bytes: usize,
    pub shutdown_timeout: Duration,
    pub startup_timeout: Duration,
    /// Where `tool/invoke` is served, if anywhere.
    pub tool_invoker: Option<Arc<dyn ToolInvoker>>,
    /// The closed set of tools this plane exposes at all.
    pub exposed_tools: BTreeSet<String>,
    /// Where `seat/call` is served, if anywhere.
    pub seat_dispatcher: Option<Arc<dyn SeatDispatcher>>,
    /// The closed set of seats this plane exposes at all.
    pub exposed_seats: BTreeSet<String>,
    /// Where `event/emit` is served, if anywhere.
    ///
    /// No exposed set beside it: a topic is a name rebon routes, not a
    /// capability it hands out, so what bounds publishing is the manifest on one
    /// side and this seam existing at all on the other.
    pub event_publisher: Option<Arc<dyn EventPublisher>>,
}

impl std::fmt::Debug for HostConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostConfig")
            .field("node", &self.node)
            .field("host_script", &self.host_script)
            .field("host_args", &self.host_args)
            .field("working_directory", &self.working_directory)
            .field("host_epoch", &self.host_epoch)
            .field("stderr_tail_bytes", &self.stderr_tail_bytes)
            .field("shutdown_timeout", &self.shutdown_timeout)
            .field("startup_timeout", &self.startup_timeout)
            .field("tool_invoker", &self.tool_invoker.is_some())
            .field("exposed_tools", &self.exposed_tools)
            .field("seat_dispatcher", &self.seat_dispatcher.is_some())
            .field("exposed_seats", &self.exposed_seats)
            .field("event_publisher", &self.event_publisher.is_some())
            .finish()
    }
}

impl HostConfig {
    pub fn new(node: impl Into<PathBuf>, host_script: impl Into<PathBuf>) -> Self {
        Self {
            node: node.into(),
            host_script: host_script.into(),
            host_args: Vec::new(),
            // A constructor default the caller overrides per host; a vanished
            // cwd is not this crate's error to raise.
            working_directory: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            host_epoch: 1,
            stderr_tail_bytes: DEFAULT_STDERR_TAIL_BYTES,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            tool_invoker: None,
            exposed_tools: BTreeSet::new(),
            seat_dispatcher: None,
            exposed_seats: BTreeSet::new(),
            event_publisher: None,
        }
    }

    /// Installs the seam a plugin's published events land on.
    ///
    /// Unpaired, unlike the tool and seat seams: those need a closed set beside
    /// them because they hand out capabilities, and a topic is not one. Absent
    /// by default, so a plane that never wired it refuses publishing rather
    /// than silently dropping events.
    pub fn with_event_publisher(mut self, publisher: Arc<dyn EventPublisher>) -> Self {
        self.event_publisher = Some(publisher);
        self
    }

    /// Installs the seat seam together with the seats it may serve.
    ///
    /// Paired for the same reason as the tool seam: neither half means anything
    /// alone, and the default of neither fails closed.
    pub fn with_seat_dispatcher(
        mut self,
        dispatcher: Arc<dyn SeatDispatcher>,
        exposed: impl IntoIterator<Item = String>,
    ) -> Self {
        self.seat_dispatcher = Some(dispatcher);
        self.exposed_seats = exposed.into_iter().collect();
        self
    }

    /// Installs the tool seam together with the set of tools it may serve.
    ///
    /// The two arrive together because neither means anything alone: an invoker
    /// with an empty set can serve nothing, and a set with no invoker names
    /// tools that cannot run. The default is neither — a plugin plane exposes
    /// no tools until something says which ones, so forgetting this call fails
    /// closed rather than open.
    pub fn with_tool_invoker(
        mut self,
        invoker: Arc<dyn ToolInvoker>,
        exposed: impl IntoIterator<Item = String>,
    ) -> Self {
        self.tool_invoker = Some(invoker);
        self.exposed_tools = exposed.into_iter().collect();
        self
    }

    /// Points the host at the module that decides what a plugin package is.
    pub fn with_loader(mut self, loader: impl AsRef<Path>) -> Self {
        self.host_args.push("--loader".to_owned());
        self.host_args
            .push(loader.as_ref().to_string_lossy().into_owned());
        self
    }

    pub fn with_startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    pub fn with_working_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.working_directory = directory.into();
        self
    }
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("plugin host script is unavailable: {0}")]
    HostScript(String),
    #[error("cannot start the plugin host: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("{0}")]
    State(#[from] StateError),
    #[error("{0}")]
    Call(#[from] HostCallError),
}

#[derive(Debug, Error)]
pub enum HostCallError {
    #[error("plugin host is not usable: {0}")]
    HostFailed(HostFailure),
    #[error("plugin host answered {status:?}: {payload}")]
    Rejected {
        status: TerminalStatus,
        payload: Payload,
    },
    #[error("{0}")]
    State(#[from] StateError),
    /// The code travels in the message, because a caller that logs this string
    /// should not have to reach for the variant to learn which refusal it was.
    #[error("{} {source}", source.code())]
    Registry {
        #[from]
        source: RegistryError,
    },
    /// The host answered, but not with the shape the method promises. Distinct
    /// from a rejection: the host thinks it succeeded and it is wrong.
    #[error("plugin host sent a malformed answer: {0}")]
    Malformed(String),
    /// A manifest asks for a tool this build does not expose. Caught at load,
    /// because a declaration that can never be satisfied is better reported
    /// when the plugin is installed than when it first tries to use it.
    #[error(
        "{UNAVAILABLE_TOOL_CODE} plugin {plugin_id:?} declares tool {tool:?}, which this plugin plane does not expose"
    )]
    UnavailableTool { plugin_id: String, tool: String },
    #[error(
        "{UNAVAILABLE_SEAT_CODE} plugin {plugin_id:?} declares seat {seat:?}, which this plugin plane does not expose"
    )]
    UnavailableSeat { plugin_id: String, seat: String },
    #[error(
        "{UNAVAILABLE_EVENTS_CODE} plugin {plugin_id:?} declares published topic {topic:?}, but this plugin plane has no event publisher"
    )]
    UnavailableEvents { plugin_id: String, topic: String },
    /// A plugin loaded, and activating it registered nothing for a provider
    /// its manifest declares. Caught at load rather than at the first turn:
    /// the load is what a session waits on, so the person who selected that
    /// provider learns it is not there before the turn starts, and learns it
    /// with the package name in hand.
    #[error(
        "{UNREGISTERED_PROVIDER_CODE} plugin {plugin_id:?} declares model provider {provider:?}, but activating it registered no adapter for that name"
    )]
    UnregisteredProvider { plugin_id: String, provider: String },
    /// The host was asked to load a plugin and never answered. Distinct from
    /// every other failure here in that nothing went wrong that anyone could
    /// report: the request left, and the reply did not come back. Without a
    /// bound this is what a caller waits on forever.
    #[error("{HOST_UNANSWERED_CODE} plugin {plugin_id:?} did not answer within {seconds} seconds")]
    HostUnanswered { plugin_id: String, seconds: u64 },
}

/// Finds the host's entry module.
///
/// The environment first, then beside the executable, then a macOS bundle's
/// `Contents/Resources` — see [`shipped_script_candidates`]. There is
/// deliberately no build-time path fallback: one baked into a shipped binary
/// points at a directory that exists only on the machine that compiled it,
/// which is a bug that only ever appears after packaging.
pub fn locate_host_script() -> Result<PathBuf, SupervisorError> {
    if let Some(path) = std::env::var_os(HOST_SCRIPT_ENV) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(SupervisorError::HostScript(format!(
                "{HOST_SCRIPT_ENV} must be an absolute path, got {}",
                path.display()
            )));
        }
        if !path.is_file() {
            return Err(SupervisorError::HostScript(format!(
                "{HOST_SCRIPT_ENV} points at {}, which does not exist",
                path.display()
            )));
        }
        return Ok(path);
    }
    locate_shipped_script("plugin-host/src/cli.mjs").ok_or_else(|| {
        SupervisorError::HostScript(format!(
            "no plugin host script found; set {HOST_SCRIPT_ENV} to an absolute cli.mjs"
        ))
    })
}

/// Deterministic product locations for a script tree shipped with the binary.
///
/// Native and npm installs put the tree beside the executable; a macOS app
/// bundle keeps auxiliary files under `Contents/Resources` while its binaries
/// live in `Contents/MacOS`. PATH and checkout locations are deliberately not
/// searched here: a checkout names its scripts explicitly (the caller's
/// repo-relative fallback), and a shipped binary must not depend on one.
fn shipped_script_candidates(exe_dir: &Path, relative: &str) -> Vec<PathBuf> {
    let mut candidates = vec![exe_dir.join(relative)];
    if let Some(contents) = exe_dir.parent() {
        let resources = contents.join("Resources").join(relative);
        if !candidates.contains(&resources) {
            candidates.push(resources);
        }
    }
    candidates
}

fn locate_shipped_script(relative: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    shipped_script_candidates(dir, relative)
        .into_iter()
        .find(|path| path.is_file())
}

/// Where the composition loader lives.
///
/// The same shape as [`locate_host_script`], and for the same reason: a
/// deployed rebon carries these next to its executable, and a checkout points
/// at them explicitly.
pub fn locate_compose_loader() -> Result<PathBuf, SupervisorError> {
    if let Some(path) = std::env::var_os(COMPOSE_LOADER_ENV) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(SupervisorError::HostScript(format!(
                "{COMPOSE_LOADER_ENV} must be an absolute path, got {}",
                path.display()
            )));
        }
        if !path.is_file() {
            return Err(SupervisorError::HostScript(format!(
                "{COMPOSE_LOADER_ENV} points at {}, which does not exist",
                path.display()
            )));
        }
        return Ok(path);
    }
    locate_shipped_script("compose-runtime/src/index.mjs").ok_or_else(|| {
        SupervisorError::HostScript(format!(
            "no composition loader found; set {COMPOSE_LOADER_ENV} to an absolute index.mjs"
        ))
    })
}

/// A bounded view of the host's most recent stderr.
#[derive(Debug, Default)]
struct StderrTail {
    limit: usize,
    bytes: Vec<u8>,
}

impl StderrTail {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > self.limit {
            let excess = self.bytes.len() - self.limit;
            self.bytes.drain(..excess);
        }
    }

    fn snapshot(&self) -> String {
        String::from_utf8_lossy(&self.bytes).trim().to_owned()
    }
}

/// What a call's answer is delivered into.
///
/// Which shape a call has is chosen by the caller, not by the host: a caller
/// that asked for one answer gets one, and a caller that asked for a stream
/// gets every chunk in order followed by exactly one end.
enum Waiter {
    Once(oneshot::Sender<Result<Payload, HostCallError>>),
    Streaming(mpsc::UnboundedSender<StreamEvent>),
}

impl Waiter {
    /// Delivers the end of a call, whichever shape it has.
    fn finish(self, result: Result<Payload, HostCallError>) {
        match self {
            Self::Once(sender) => {
                let _ = sender.send(result);
            }
            Self::Streaming(sender) => {
                let _ = sender.send(StreamEvent::End(result));
            }
        }
    }
}

/// One item of a streamed answer.
///
/// `End` is always last and always arrives: a stream that stopped producing is
/// indistinguishable from one still working, so the end is a value rather than
/// the channel simply closing.
#[derive(Debug)]
pub enum StreamEvent {
    Chunk(Payload),
    End(Result<Payload, HostCallError>),
}

/// A streamed service answer, and the accounting that has to close with it.
pub struct ServiceStream {
    inner: Arc<Inner>,
    plugin_id: String,
    call_id: String,
    events: mpsc::UnboundedReceiver<StreamEvent>,
    settled: bool,
}

impl std::fmt::Debug for ServiceStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceStream")
            .field("plugin_id", &self.plugin_id)
            .field("call_id", &self.call_id)
            .field("settled", &self.settled)
            .finish()
    }
}

impl ServiceStream {
    /// The call this stream belongs to, for cancelling it.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// The next chunk, or the end. `None` once the end has been taken.
    pub async fn recv(&mut self) -> Option<StreamEvent> {
        let event = self.events.recv().await?;
        if matches!(event, StreamEvent::End(_)) {
            self.settled = true;
            self.inner.close_call(&self.plugin_id, &self.call_id).await;
        }
        Some(event)
    }

    /// Collects the whole stream: every chunk, then whatever it ended as.
    pub async fn collect(mut self) -> (Vec<Payload>, Result<Payload, HostCallError>) {
        let mut chunks = Vec::new();
        while let Some(event) = self.recv().await {
            match event {
                StreamEvent::Chunk(payload) => chunks.push(payload),
                StreamEvent::End(result) => return (chunks, result),
            }
        }
        (
            chunks,
            Err(HostCallError::Malformed(
                "the stream closed without ending".to_owned(),
            )),
        )
    }
}

impl Drop for ServiceStream {
    /// A caller that walks away still closes the call.
    ///
    /// Spawned rather than done inline because the accounting lives behind an
    /// async lock and a `Drop` cannot wait on one — and leaving the call
    /// counted would block the plugin's unload with no way to notice.
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let plugin_id = std::mem::take(&mut self.plugin_id);
        let call_id = std::mem::take(&mut self.call_id);
        tokio::spawn(async move {
            inner.close_call(&plugin_id, &call_id).await;
        });
    }
}

/// Closes one call's accounting however its caller leaves.
///
/// A call that answers once closes itself on the line after it returns, and that
/// line is not reached when the future is dropped mid-await — which is exactly
/// what `tokio::time::timeout` does to a slow plugin, and what a cancelled turn
/// does to every call it was waiting on. The call then stayed counted for the
/// life of the process: the plugin could never empty a drain, so it could never
/// finish unloading, so its id could never be loaded again. [`ServiceStream`]
/// has always made this promise for the calls that stream; this makes it for the
/// calls that answer once.
struct CallGuard {
    inner: Arc<Inner>,
    plugin_id: String,
    call_id: String,
    closed: bool,
}

impl CallGuard {
    fn new(inner: Arc<Inner>, plugin_id: &str, call_id: String) -> Self {
        Self {
            inner,
            plugin_id: plugin_id.to_owned(),
            call_id,
            closed: false,
        }
    }

    /// The ordinary ending: closed here, on the caller's own task, so a call
    /// that has returned has already been counted out.
    ///
    /// The flag is set after the await and not before it, so a caller dropped
    /// inside this close still leaves the fallback below armed.
    async fn close(mut self) {
        self.inner.close_call(&self.plugin_id, &self.call_id).await;
        self.closed = true;
    }

    /// Hands the accounting to a [`ServiceStream`], which closes it on its own
    /// terms — when the stream ends, or when its reader walks away.
    fn hand_to_stream(mut self) -> (Arc<Inner>, String, String) {
        self.closed = true;
        (
            Arc::clone(&self.inner),
            std::mem::take(&mut self.plugin_id),
            std::mem::take(&mut self.call_id),
        )
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        // Spawned for the reason `ServiceStream::drop` spawns: the accounting
        // lives behind an async lock and a `Drop` cannot wait on one.
        let inner = Arc::clone(&self.inner);
        let plugin_id = std::mem::take(&mut self.plugin_id);
        let call_id = std::mem::take(&mut self.call_id);
        tokio::spawn(async move {
            inner.close_call(&plugin_id, &call_id).await;
        });
    }
}

type Waiters = HashMap<String, Waiter>;

/// One upstream `event/emit`, waiting its turn.
struct QueuedEvent {
    identity: CallIdentity,
    payload: Payload,
}

struct Inner {
    /// Upstream events, in the order their frames arrived.
    ///
    /// Every other upstream request is answered on a task of its own, which is
    /// correct because answers are matched by call id. An event is not only an
    /// answer: publishing it is a side effect, and for a stream like a loop's
    /// `turn/end` the order of those effects is the meaning. Spawning per
    /// request let a turn's end overtake the messages that came before it.
    ///
    /// So events get one lane and one drain task. The reader still never
    /// blocks — the send is to an unbounded channel — and the reply is still
    /// written off the reader, by the drain rather than by a fresh task.
    events: mpsc::UnboundedSender<QueuedEvent>,
    state: Mutex<SupervisorState>,
    waiters: StdMutex<Waiters>,
    stdin: Mutex<Option<ChildStdin>>,
    stderr: Arc<StdMutex<StderrTail>>,
    tool_invoker: Option<Arc<dyn ToolInvoker>>,
    exposed_tools: BTreeSet<String>,
    seat_dispatcher: Option<Arc<dyn SeatDispatcher>>,
    exposed_seats: BTreeSet<String>,
    event_publisher: Option<Arc<dyn EventPublisher>>,
}

impl Inner {
    /// Closes one call's accounting, and says so when that finished a drain.
    ///
    /// The single place this side stops counting a call, because the drain that
    /// may be waiting on it ends here: whoever closes the last call of a drain
    /// completes it, exactly as the host retires a plugin in the `finally` of
    /// its last handler. A call already closed is not an error to report — a
    /// host failure answers and closes everything that was open, and a stream
    /// read to its end then dropped arrives here twice by design.
    async fn close_call(&self, plugin_id: &str, call_id: &str) {
        let mut state = self.state.lock().await;
        if let Ok(CallClosed::DrainFinished) =
            state.registry_mut().complete_call(plugin_id, call_id)
        {
            tracing::debug!(
                plugin_id,
                call_id,
                "the last call of a drain closed; the plugin is unloaded"
            );
        }
    }

    /// Marks the host unusable and answers everything waiting on it.
    async fn fail(&self, failure: HostFailure) {
        let owed = {
            let mut state = self.state.lock().await;
            state.fail_all(failure)
        };
        self.settle_failed(owed).await;
    }

    /// The same, but only while `epoch` is still the host that is running.
    ///
    /// A reader outlives the process it reads. It sits parked in `read`, and
    /// the EOF that finally wakes it can be delivered after `restart` has
    /// already put a new child, a new stdin and a new reader on this same
    /// `Inner`. Failing then closes the *new* host's stdin and answers its
    /// handshake with the previous host's death, so a restart came back as
    /// `HostFailed("plugin host closed its output")` whenever the outgoing
    /// reader lost that race — which it does on a loaded machine.
    ///
    /// The epoch check shares the state lock with `fail_all`, because
    /// `restart` advances the epoch under that same lock: checking outside it
    /// would just make the window smaller.
    async fn fail_at_epoch(&self, epoch: u64, failure: HostFailure) {
        let owed = {
            let mut state = self.state.lock().await;
            if state.host_epoch() != epoch {
                return;
            }
            state.fail_all(failure)
        };
        self.settle_failed(owed).await;
    }

    /// Closes stdin and hands every owed call the failure now on record.
    async fn settle_failed(&self, owed: Vec<WireEnvelope>) {
        // Closing stdin first lets a host that is merely wedged notice EOF and
        // exit on its own rather than needing to be killed.
        *self.stdin.lock().await = None;
        let failure = self
            .state
            .lock()
            .await
            .failure()
            .cloned()
            .unwrap_or_else(|| HostFailure::new("plugin host is not usable"));
        let mut waiters = self.waiters.lock().expect("waiters mutex");
        for envelope in owed {
            if let Some(waiter) = waiters.remove(&envelope.identity.call_id) {
                // `Rejected` means the host answered. It did not: this terminal
                // was synthesised because it never will.
                waiter.finish(Err(HostCallError::HostFailed(failure.clone())));
            }
        }
    }

    fn failure_snapshot(&self, reason: impl Into<String>) -> HostFailure {
        let diagnostics = self.stderr.lock().expect("stderr mutex").snapshot();
        HostFailure::new(reason).with_diagnostics(diagnostics)
    }

    async fn write(&self, envelope: &WireEnvelope) -> Result<(), String> {
        let codec = NdjsonCodec::default();
        let bytes = codec
            .encode(envelope)
            .map_err(|error| format!("cannot encode a frame for the plugin host: {error}"))?;
        let mut guard = self.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| "plugin host stdin is closed".to_owned())?;
        stdin
            .write_all(&bytes)
            .await
            .map_err(|error| format!("writing to the plugin host failed: {error}"))?;
        stdin
            .flush()
            .await
            .map_err(|error| format!("flushing to the plugin host failed: {error}"))
    }
}

/// A running plugin host and the bookkeeping that outlives it.
pub struct PluginHostSupervisor {
    inner: Arc<Inner>,
    config: HostConfig,
    child: Mutex<Option<Child>>,
}

impl std::fmt::Debug for PluginHostSupervisor {
    /// Deliberately shallow: the interesting state is behind async locks, and a
    /// `Debug` that took them could deadlock the very failure path it is being
    /// used to diagnose.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHostSupervisor")
            .field("node", &self.config.node)
            .field("host_script", &self.config.host_script)
            .field("host_epoch", &self.config.host_epoch)
            .finish_non_exhaustive()
    }
}

impl PluginHostSupervisor {
    /// Starts the host and completes `platform/initialize`.
    ///
    /// Initialize is part of starting rather than a separate step a caller can
    /// forget: a host that has not been initialised rejects everything else, so
    /// returning one would only produce a confusing first failure.
    pub async fn start(config: HostConfig) -> Result<Self, SupervisorError> {
        let supervisor = Self::spawn(config).await?;
        supervisor.initialize().await?;
        Ok(supervisor)
    }

    async fn spawn(config: HostConfig) -> Result<Self, SupervisorError> {
        let mut command = Command::new(&config.node);
        command
            .arg(&config.host_script)
            .args(&config.host_args)
            .current_dir(&config.working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A host outliving the supervisor would hold plugin state nobody
            // can reach and keep whatever the plugins opened.
            .kill_on_drop(true);
        // Never flash a console window: this runs under a desktop app too.
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        let mut child = command.spawn().map_err(SupervisorError::Spawn)?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            events: events_tx,
            state: Mutex::new(SupervisorState::new(config.host_epoch)?),
            waiters: StdMutex::new(HashMap::new()),
            stdin: Mutex::new(Some(stdin)),
            stderr: Arc::new(StdMutex::new(StderrTail {
                limit: config.stderr_tail_bytes,
                bytes: Vec::new(),
            })),
            tool_invoker: config.tool_invoker.clone(),
            exposed_tools: config.exposed_tools.clone(),
            seat_dispatcher: config.seat_dispatcher.clone(),
            exposed_seats: config.exposed_seats.clone(),
            event_publisher: config.event_publisher.clone(),
        });

        tokio::spawn(drain_stderr(stderr, Arc::clone(&inner.stderr)));
        tokio::spawn(read_frames(stdout, Arc::clone(&inner), config.host_epoch));
        tokio::spawn(drain_events(events_rx, Arc::clone(&inner)));

        Ok(Self {
            inner,
            config,
            child: Mutex::new(Some(child)),
        })
    }

    pub fn host_script(&self) -> &Path {
        &self.config.host_script
    }

    pub async fn host_epoch(&self) -> u64 {
        self.inner.state.lock().await.host_epoch()
    }

    pub async fn is_alive(&self) -> bool {
        self.inner.state.lock().await.is_alive()
    }

    pub async fn failure(&self) -> Option<HostFailure> {
        self.inner.state.lock().await.failure().cloned()
    }

    async fn initialize(&self) -> Result<(), SupervisorError> {
        let identity = {
            let mut state = self.inner.state.lock().await;
            let call_id = state.next_call_id();
            CallIdentity::platform_control(state.host_epoch(), call_id)
                .map_err(|error| StateError::Lifecycle(error.into()))?
        };
        let handshake = self.request(identity, PLATFORM_INITIALIZE_METHOD, Payload::null());
        match tokio::time::timeout(self.config.startup_timeout, handshake).await {
            Ok(result) => {
                result?;
                Ok(())
            }
            Err(_) => {
                let failure = self.inner.failure_snapshot(format!(
                    "plugin host did not answer {PLATFORM_INITIALIZE_METHOD} within {:?}",
                    self.config.startup_timeout
                ));
                self.inner.fail(failure.clone()).await;
                Err(SupervisorError::Call(HostCallError::HostFailed(failure)))
            }
        }
    }

    /// Opens a scope, which is where a workspace root belongs — an ACP or server
    /// process can hold several, so it cannot be a property of the host.
    pub async fn open_scope(
        &self,
        plugin_id: &str,
        scope_id: &str,
        workspace_root: &str,
    ) -> Result<u64, HostCallError> {
        let identity = {
            let mut state = self.inner.state.lock().await;
            state.open_scope(plugin_id, scope_id)?
        };
        let generation = identity.scope_generation;
        let payload = Payload::from(serde_json::json!({ "workspace_root": workspace_root }));
        self.request(identity, SCOPE_OPEN_METHOD, payload).await?;
        Ok(generation)
    }

    /// Closes a scope on an advanced generation, revoking everything bound to
    /// the old one. Returns what was revoked, so the loss is reported rather
    /// than inferred later from delivery failures.
    pub async fn close_scope(
        &self,
        plugin_id: &str,
        scope_id: &str,
    ) -> Result<Vec<(String, String)>, HostCallError> {
        let (identity, revoked) = {
            let mut state = self.inner.state.lock().await;
            state.close_scope(plugin_id, scope_id)?
        };
        self.request(identity, SCOPE_CLOSE_METHOD, Payload::null())
            .await?;
        Ok(revoked)
    }

    /// Loads one plugin, recording what it registered.
    ///
    /// The registry is consulted before the host is asked and updated from what
    /// the host answers, so "which plugins are loaded" has one answer on this
    /// side even though the code being loaded lives on the other.
    pub async fn load_plugin(
        &self,
        request: &PluginLoadRequest,
    ) -> Result<PluginReadyReport, HostCallError> {
        // A manifest asking for a tool this build never exposes describes a
        // plugin that cannot work. Saying so now beats letting it load and fail
        // at whatever moment it first reaches for that tool.
        if let Some(tool) = request
            .invokable_tools
            .iter()
            .find(|tool| !self.inner.exposed_tools.contains(*tool))
        {
            return Err(HostCallError::UnavailableTool {
                plugin_id: request.plugin_id.clone(),
                tool: tool.clone(),
            });
        }
        if let Some(seat) = request
            .seats
            .iter()
            .find(|seat| !self.inner.exposed_seats.contains(*seat))
        {
            return Err(HostCallError::UnavailableSeat {
                plugin_id: request.plugin_id.clone(),
                seat: seat.clone(),
            });
        }
        // Same rule as the two above, one step further: a plugin declaring
        // topics it will publish into a plane with nowhere to publish them is
        // describing work that cannot happen.
        if self.inner.event_publisher.is_none() {
            if let Some(topic) = request.published_topics.first() {
                return Err(HostCallError::UnavailableEvents {
                    plugin_id: request.plugin_id.clone(),
                    topic: topic.clone(),
                });
            }
        }
        {
            let mut state = self.inner.state.lock().await;
            state.registry_mut().admit_load(request)?;
        }
        let identity = self.control_identity().await?;
        let payload =
            Payload::from(serde_json::to_value(request).expect("a load request is plain data"));
        let outcome = self.request(identity, PLUGIN_LOAD_METHOD, payload).await;
        let report = match outcome {
            Ok(payload) => serde_json::from_value::<PluginReadyReport>(
                payload.to_value().map_err(|error| {
                    HostCallError::Malformed(format!("ready report is not JSON: {error}"))
                })?,
            )
            .map_err(|error| HostCallError::Malformed(format!("ready report is invalid: {error}"))),
            Err(error) => Err(error),
        };
        let mut state = self.inner.state.lock().await;
        match report {
            Ok(report) => {
                // A report that claims more than the manifest declared is
                // refused here, and the plugin stays unloaded on this side.
                state.registry_mut().accept_ready(&report)?;
                Ok(report)
            }
            Err(error) => {
                // A load that did not finish leaves nothing behind, so the next
                // attempt starts clean rather than inheriting a half-built entry.
                let _ = state.registry_mut().reject_load(&request.plugin_id);
                Err(error)
            }
        }
    }

    /// Begins draining one plugin and returns the host's ledger of what was
    /// still running.
    pub async fn unload_plugin(&self, plugin_id: &str) -> Result<PluginDrainReport, HostCallError> {
        let request = PluginUnloadRequest {
            plugin_id: plugin_id.to_owned(),
        };
        let local = {
            let mut state = self.inner.state.lock().await;
            state.registry_mut().begin_unload(&request)?
        };
        let identity = self.control_identity().await?;
        let payload =
            Payload::from(serde_json::to_value(&request).expect("an unload request is plain data"));
        let answered = self
            .request(identity, PLUGIN_UNLOAD_METHOD, payload)
            .await?;
        let remote: PluginDrainReport =
            serde_json::from_value(answered.to_value().map_err(|error| {
                HostCallError::Malformed(format!("drain report is not JSON: {error}"))
            })?)
            .map_err(|error| {
                HostCallError::Malformed(format!("drain report is invalid: {error}"))
            })?;

        {
            let mut state = self.inner.state.lock().await;
            // Each side only knows its own ledger. The host's report says what
            // is running inside it; `finish_unload` checks what is running
            // here, which is the count that decides whether this side is done.
            // A refusal is not a failure — it means work is still counted here,
            // and closing the last of it is what finishes the drain instead.
            let _ = state.registry_mut().finish_unload(plugin_id);
        }
        // The host's ledger is the authority on what is still running inside it;
        // this side's is reported alongside so a disagreement is visible.
        Ok(PluginDrainReport {
            revoked_subscriptions: local.revoked_subscriptions,
            ..remote
        })
    }

    /// Calls a service on a loaded plugin, inside one of its open scopes.
    /// One request, optionally with a deadline the caller set.
    ///
    /// `None` leaves the call unbounded, which is still the right answer for a
    /// call whose length is the plugin's business -- a tool the model invoked
    /// may run for as long as the tool takes. A bound belongs to calls a person
    /// is waiting behind, where "it will come eventually" and "it is never
    /// coming" look the same from the other side of the screen.
    ///
    /// On expiry the call is cancelled rather than merely abandoned: dropping
    /// the future stops this side waiting but leaves the plugin working on an
    /// answer nobody will read. The cancel is best effort -- the call may have
    /// just finished, in which case there is nothing in flight to cancel and
    /// saying so is not interesting.
    async fn request_bounded(
        &self,
        identity: CallIdentity,
        method: &str,
        payload: Payload,
        bound: Option<Duration>,
        plugin_id: &str,
    ) -> Result<Payload, HostCallError> {
        let Some(bound) = bound else {
            return self.request(identity, method, payload).await;
        };
        let call_id = identity.call_id.clone();
        match tokio::time::timeout(bound, self.request(identity, method, payload)).await {
            Ok(outcome) => outcome,
            Err(_) => {
                let _ = self.cancel(&call_id).await;
                Err(HostCallError::HostUnanswered {
                    plugin_id: plugin_id.to_owned(),
                    seconds: bound.as_secs(),
                })
            }
        }
    }

    /// One service call, unbounded: how long a service takes is the plugin's
    /// business unless a caller says otherwise with [`Self::call_service_bounded`].
    pub async fn call_service(
        &self,
        plugin_id: &str,
        scope_id: &str,
        service: &str,
        request: Payload,
    ) -> Result<Payload, HostCallError> {
        self.call_service_bounded(plugin_id, scope_id, service, request, None)
            .await
    }

    /// One service call a caller is waiting behind, with the deadline it chose.
    pub async fn call_service_bounded(
        &self,
        plugin_id: &str,
        scope_id: &str,
        service: &str,
        request: Payload,
        bound: Option<Duration>,
    ) -> Result<Payload, HostCallError> {
        let call = ServiceCallRequest {
            service: service.to_owned(),
            request,
        };
        let identity = {
            let mut state = self.inner.state.lock().await;
            let identity = state.call_identity(plugin_id, scope_id)?;
            state.registry_mut().admit_service_call(&identity, &call)?;
            identity
        };
        let guard = CallGuard::new(Arc::clone(&self.inner), plugin_id, identity.call_id.clone());
        let payload =
            Payload::from(serde_json::to_value(&call).expect("a service call is plain data"));
        let outcome = self
            .request_bounded(identity, SERVICE_CALL_METHOD, payload, bound, plugin_id)
            .await;
        // Accounting closes however the call ended: a drain waiting on it must
        // not be held open by a failure — nor by a caller that stopped waiting
        // for the answer, which is what the guard covers.
        guard.close().await;
        outcome
    }

    /// Delivers one event on a live subscription.
    ///
    /// The subscription is checked against the scope incarnation that created
    /// it before anything is sent. A delivery carrying a generation the scope
    /// has moved past would backfill a scope that no longer exists, which is
    /// worse than a lost event: the plugin would act on a session it already
    /// finished.
    pub async fn deliver_event(
        &self,
        plugin_id: &str,
        scope_id: &str,
        subscription: &str,
        topic: &str,
        event: Payload,
    ) -> Result<(), HostCallError> {
        let delivery = EventDelivery {
            subscription: subscription.to_owned(),
            topic: topic.to_owned(),
            event,
        };
        let identity = {
            let mut state = self.inner.state.lock().await;
            let identity = state.call_identity(plugin_id, scope_id)?;
            state
                .registry()
                .admit_event_delivery(&identity, &delivery)?;
            identity
        };
        let payload =
            Payload::from(serde_json::to_value(&delivery).expect("a delivery is plain data"));
        self.request(identity, EVENT_DELIVER_METHOD, payload)
            .await?;
        Ok(())
    }

    /// Calls this side still counts as running inside one plugin.
    pub async fn in_flight(&self, plugin_id: &str) -> Vec<String> {
        let state = self.inner.state.lock().await;
        state.registry().in_flight(plugin_id)
    }

    /// Every subscription one plugin holds, as this side records them.
    pub async fn subscriptions(&self, plugin_id: &str) -> Vec<(String, String)> {
        let state = self.inner.state.lock().await;
        state.registry().subscriptions(plugin_id)
    }

    /// Calls a service whose answer arrives as a stream.
    ///
    /// The returned [`ServiceStream`] is what closes the call out on this side.
    /// Draining it to its end does that; so does dropping it early, because a
    /// call left counted as in flight would block that plugin's unload forever
    /// and the caller walking away is not a reason to hold the plugin hostage.
    pub async fn call_service_streaming(
        &self,
        plugin_id: &str,
        scope_id: &str,
        service: &str,
        request: Payload,
    ) -> Result<ServiceStream, HostCallError> {
        let call = ServiceCallRequest {
            service: service.to_owned(),
            request,
        };
        let identity = {
            let mut state = self.inner.state.lock().await;
            let identity = state.call_identity(plugin_id, scope_id)?;
            state.registry_mut().admit_service_call(&identity, &call)?;
            identity
        };
        let guard = CallGuard::new(Arc::clone(&self.inner), plugin_id, identity.call_id.clone());
        let payload =
            Payload::from(serde_json::to_value(&call).expect("a service call is plain data"));
        self.stream_call(guard, identity, SERVICE_CALL_METHOD, payload)
            .await
    }

    /// Calls a tool a plugin registered, inside one of its open scopes.
    ///
    /// The mirror of [`Self::call_service`] and the opposite of `tool/invoke`:
    /// here rebon is the caller and the plugin owns the tool. Whether the
    /// *user* permitted this run was decided before the call reached here — a
    /// plugin tool goes through rebon's permission pipeline like any other,
    /// which is the embedder's job and not this crate's.
    pub async fn call_tool(
        &self,
        plugin_id: &str,
        scope_id: &str,
        tool: &str,
        input: Payload,
    ) -> Result<Payload, HostCallError> {
        let call = ToolInvokeRequest {
            tool: tool.to_owned(),
            input,
        };
        let identity = {
            let mut state = self.inner.state.lock().await;
            let identity = state.call_identity(plugin_id, scope_id)?;
            state.registry_mut().admit_tool_call(&identity, &call)?;
            identity
        };
        let guard = CallGuard::new(Arc::clone(&self.inner), plugin_id, identity.call_id.clone());
        let payload =
            Payload::from(serde_json::to_value(&call).expect("a tool call is plain data"));
        let outcome = self.request(identity, TOOL_CALL_METHOD, payload).await;
        // Accounting closes however the call ended: a drain waiting on it must
        // not be held open by a failure — nor by a caller that stopped waiting
        // for the answer, which is what the guard covers.
        guard.close().await;
        outcome
    }

    /// The tools one plugin registered, as they describe themselves.
    ///
    /// This is what an embedder offers to a model, which is why it is the
    /// definitions rather than the names.
    pub async fn tools(&self, plugin_id: &str) -> Vec<PluginToolDefinition> {
        let state = self.inner.state.lock().await;
        state.registry().tools(plugin_id)
    }

    /// Streams one model turn through an adapter a plugin registered.
    ///
    /// The transport is the same as a streamed service call; what differs is
    /// the routing key. The turn's contents stay opaque here — translating a
    /// provider's chunks into rebon events belongs to the model layer, not to
    /// the process that carries them.
    pub async fn stream_llm(
        &self,
        plugin_id: &str,
        scope_id: &str,
        provider: &str,
        request: Payload,
    ) -> Result<ServiceStream, HostCallError> {
        let call = LlmStreamRequest {
            provider: provider.to_owned(),
            request,
        };
        let identity = {
            let mut state = self.inner.state.lock().await;
            let identity = state.call_identity(plugin_id, scope_id)?;
            state.registry_mut().admit_llm_stream(&identity, &call)?;
            identity
        };
        let guard = CallGuard::new(Arc::clone(&self.inner), plugin_id, identity.call_id.clone());
        let payload =
            Payload::from(serde_json::to_value(&call).expect("an llm request is plain data"));
        self.stream_call(guard, identity, LLM_STREAM_METHOD, payload)
            .await
    }

    /// Runs one slash command a plugin registered.
    ///
    /// The command half of [`Self::call_tool`]: rebon is the caller, the
    /// plugin owns the implementation, and the answer is the text a person
    /// asked for rather than a tool result a model reads.
    /// One command invocation, unbounded. See [`Self::invoke_command_bounded`].
    pub async fn invoke_command(
        &self,
        plugin_id: &str,
        scope_id: &str,
        request: CommandInvokeRequest,
    ) -> Result<Payload, HostCallError> {
        self.invoke_command_bounded(plugin_id, scope_id, request, None)
            .await
    }

    /// One command invocation a person is waiting behind, with the deadline
    /// the caller chose.
    pub async fn invoke_command_bounded(
        &self,
        plugin_id: &str,
        scope_id: &str,
        request: CommandInvokeRequest,
        bound: Option<Duration>,
    ) -> Result<Payload, HostCallError> {
        let identity = {
            let mut state = self.inner.state.lock().await;
            let identity = state.call_identity(plugin_id, scope_id)?;
            state
                .registry_mut()
                .admit_command_invoke(&identity, &request)?;
            identity
        };
        let guard = CallGuard::new(Arc::clone(&self.inner), plugin_id, identity.call_id.clone());
        let payload =
            Payload::from(serde_json::to_value(&request).expect("a command call is plain data"));
        let outcome = self
            .request_bounded(identity, COMMAND_INVOKE_METHOD, payload, bound, plugin_id)
            .await;
        guard.close().await;
        outcome
    }

    /// The slash commands one plugin registered, as they describe themselves.
    ///
    /// The counterpart of [`Self::tools`]: a command has to appear in a menu,
    /// so what an embedder needs is the definitions rather than the names.
    pub async fn commands(&self, plugin_id: &str) -> Vec<PluginCommandDefinition> {
        let state = self.inner.state.lock().await;
        state.registry().commands(plugin_id)
    }

    /// Tells one adapter something about the conversation around its turns.
    ///
    /// The three signals a stateful provider needs and a stateless one
    /// ignores: the session was reset, a turn ended, the response id it was
    /// carrying is stale. Ordinary request accounting, so a plugin cannot be
    /// retired out from under one — and so a signal sent to a draining plugin
    /// is refused rather than lost.
    pub async fn control_llm(
        &self,
        plugin_id: &str,
        scope_id: &str,
        provider: &str,
        signal: &str,
    ) -> Result<Payload, HostCallError> {
        let call = LlmControlRequest {
            provider: provider.to_owned(),
            signal: signal.to_owned(),
        };
        let identity = {
            let mut state = self.inner.state.lock().await;
            let identity = state.call_identity(plugin_id, scope_id)?;
            state.registry_mut().admit_llm_control(&identity, &call)?;
            identity
        };
        let guard = CallGuard::new(Arc::clone(&self.inner), plugin_id, identity.call_id.clone());
        let payload =
            Payload::from(serde_json::to_value(&call).expect("an llm signal is plain data"));
        let outcome = self.request(identity, LLM_CONTROL_METHOD, payload).await;
        guard.close().await;
        outcome
    }

    /// What each adapter one plugin registered said about itself.
    ///
    /// The counterpart of [`Self::tools`]: an adapter, like a tool, has to be
    /// described before anything routes to it, and the description is reported
    /// once at load rather than asked for per turn.
    pub async fn llm_adapters(&self, plugin_id: &str) -> BTreeMap<String, Payload> {
        let state = self.inner.state.lock().await;
        state.registry().llm_adapters(plugin_id)
    }

    /// Shared tail of the streaming calls: hand back a stream, or close the
    /// accounting if the call never got out.
    async fn stream_call(
        &self,
        guard: CallGuard,
        identity: CallIdentity,
        method: &str,
        payload: Payload,
    ) -> Result<ServiceStream, HostCallError> {
        match self.request_streaming(identity, method, payload).await {
            Ok(events) => {
                let (inner, plugin_id, call_id) = guard.hand_to_stream();
                Ok(ServiceStream {
                    inner,
                    plugin_id,
                    call_id,
                    events,
                    settled: false,
                })
            }
            Err(error) => {
                guard.close().await;
                Err(error)
            }
        }
    }

    /// Asks the host to stop one call it is still working on.
    ///
    /// Cancel is a notification, not a request: it never owns the terminal slot,
    /// and the call still ends the way the host decides to end it — cancelled,
    /// or finished anyway if it was already past the point of stopping. A caller
    /// that treated cancel as the end would be reading an answer that has not
    /// arrived.
    pub async fn cancel(&self, call_id: &str) -> Result<(), HostCallError> {
        let envelope = {
            let state = self.inner.state.lock().await;
            let identity = state
                .pending_identity(call_id)
                .ok_or_else(|| {
                    HostCallError::Malformed(format!("no call {call_id} is in flight to cancel"))
                })?
                .clone();
            let envelope = WireEnvelope::new(
                identity,
                WireMessage::Notification {
                    method: CALL_CANCEL_METHOD.to_owned(),
                    payload: Payload::null(),
                },
            );
            if !state.admit_cancel(&envelope)? {
                // The call belongs to an incarnation that has already been
                // invalidated; there is nothing left to stop.
                return Ok(());
            }
            envelope
        };
        if let Err(reason) = self.inner.write(&envelope).await {
            let failure = self.inner.failure_snapshot(reason);
            self.inner.fail(failure.clone()).await;
            return Err(HostCallError::HostFailed(failure));
        }
        Ok(())
    }

    async fn control_identity(&self) -> Result<CallIdentity, HostCallError> {
        let mut state = self.inner.state.lock().await;
        let call_id = state.next_call_id();
        CallIdentity::platform_control(state.host_epoch(), call_id)
            .map_err(|error| HostCallError::State(StateError::Lifecycle(error.into())))
    }

    /// Sends one request and waits for its terminal.
    pub async fn request(
        &self,
        identity: CallIdentity,
        method: &str,
        payload: Payload,
    ) -> Result<Payload, HostCallError> {
        let (sender, receiver) = oneshot::channel();
        self.dispatch(identity, method, payload, Waiter::Once(sender))
            .await?;

        match receiver.await {
            Ok(result) => result,
            // The only way the sender is dropped without answering is a
            // supervisor bug; treating it as a host failure keeps the promise
            // that no caller waits forever.
            Err(_) => Err(HostCallError::HostFailed(
                self.failure()
                    .await
                    .unwrap_or_else(|| HostFailure::new("plugin host answer was lost")),
            )),
        }
    }

    /// Sends one request whose answer arrives as a stream.
    ///
    /// The channel yields every chunk in order and then exactly one
    /// [`StreamEvent::End`]. Whether a call streams is the *caller's* choice,
    /// not the host's: a caller that used [`Self::request`] on a method that
    /// streams fails that call rather than losing its chunks quietly.
    pub async fn request_streaming(
        &self,
        identity: CallIdentity,
        method: &str,
        payload: Payload,
    ) -> Result<mpsc::UnboundedReceiver<StreamEvent>, HostCallError> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.dispatch(identity, method, payload, Waiter::Streaming(sender))
            .await?;
        Ok(receiver)
    }

    /// Registers a call, records where its answer goes, and writes it.
    ///
    /// Registration happens before the write so that a write that fails still
    /// leaves a call the fail-all can answer.
    async fn dispatch(
        &self,
        identity: CallIdentity,
        method: &str,
        payload: Payload,
        waiter: Waiter,
    ) -> Result<(), HostCallError> {
        let call_id = identity.call_id.clone();
        {
            let mut state = self.inner.state.lock().await;
            state.begin_call(identity.clone())?;
            self.inner
                .waiters
                .lock()
                .expect("waiters mutex")
                .insert(call_id, waiter);
        }

        let envelope = WireEnvelope::new(
            identity,
            WireMessage::Request {
                method: method.to_owned(),
                payload,
            },
        );
        if let Err(reason) = self.inner.write(&envelope).await {
            let failure = self.inner.failure_snapshot(reason);
            self.inner.fail(failure.clone()).await;
            return Err(HostCallError::HostFailed(failure));
        }
        Ok(())
    }

    /// Asks the host to shut down, then waits for the process to leave.
    ///
    /// A host that does not exit within the configured window is killed: an
    /// orderly shutdown is preferred, but not at the price of hanging the
    /// process that asked for it.
    pub async fn shutdown(&self) -> Result<(), HostCallError> {
        if self.is_alive().await {
            let identity = {
                let mut state = self.inner.state.lock().await;
                let call_id = state.next_call_id();
                CallIdentity::platform_control(state.host_epoch(), call_id)
                    .map_err(|error| HostCallError::State(StateError::Lifecycle(error.into())))?
            };
            self.request(identity, PLATFORM_SHUTDOWN_METHOD, Payload::null())
                .await?;
            // Recorded before stdin closes, so the reader's EOF finds a reason
            // already set and "first failure wins" reports the deliberate one
            // rather than "the host closed its output".
            self.inner.fail(HostFailure::new(SHUTDOWN_REASON)).await;
        }
        *self.inner.stdin.lock().await = None;
        self.reap().await;
        Ok(())
    }

    /// Whether this host is gone because it was asked to leave.
    pub async fn was_shut_down(&self) -> bool {
        self.failure()
            .await
            .is_some_and(|failure| failure.reason == SHUTDOWN_REASON)
    }

    async fn reap(&self) {
        let mut guard = self.child.lock().await;
        let Some(child) = guard.as_mut() else { return };
        match tokio::time::timeout(self.config.shutdown_timeout, child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
        *guard = None;
    }

    /// Starts a fresh host on a new epoch, answering whatever the old one owed.
    ///
    /// Nothing calls this automatically. A restart is a decision, because the
    /// new host has no plugins loaded and no scopes open, and pretending
    /// otherwise would be a lie told to whoever holds this handle.
    pub async fn restart(&self) -> Result<Vec<WireEnvelope>, SupervisorError> {
        let _ = self.shutdown().await;
        self.reap().await;
        let next_epoch = self.host_epoch().await.saturating_add(1);
        let owed = {
            let mut state = self.inner.state.lock().await;
            state.restart(next_epoch)?
        };
        self.inner.waiters.lock().expect("waiters mutex").clear();
        self.inner
            .stderr
            .lock()
            .expect("stderr mutex")
            .bytes
            .clear();

        let mut command = Command::new(&self.config.node);
        command
            .arg(&self.config.host_script)
            .args(&self.config.host_args)
            .current_dir(&self.config.working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Never flash a console window: this runs under a desktop app too.
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        let mut child = command.spawn().map_err(SupervisorError::Spawn)?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        *self.inner.stdin.lock().await = Some(stdin);
        *self.child.lock().await = Some(child);
        tokio::spawn(drain_stderr(stderr, Arc::clone(&self.inner.stderr)));
        tokio::spawn(read_frames(stdout, Arc::clone(&self.inner), next_epoch));

        self.initialize().await?;
        Ok(owed)
    }
}

async fn drain_stderr(mut stderr: tokio::process::ChildStderr, tail: Arc<StdMutex<StderrTail>>) {
    let mut buffer = [0u8; 4096];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => tail.lock().expect("stderr mutex").push(&buffer[..read]),
        }
    }
}

/// Decodes the host's stdout and settles the calls it answers.
///
/// Every exit from this loop is a host failure. Clean EOF included: the host
/// closing stdout means it is gone, and any call still waiting is waiting on a
/// process that will never answer.
///
/// `epoch` is the host generation this reader belongs to. It is carried all
/// the way to the failure because that failure may be recorded long after the
/// process died; see `Inner::fail_at_epoch`.
async fn read_frames(mut stdout: tokio::process::ChildStdout, inner: Arc<Inner>, epoch: u64) {
    let mut codec = NdjsonCodec::default();
    let mut buffer = [0u8; 8192];
    let reason = loop {
        let read = match stdout.read(&mut buffer).await {
            Ok(0) => break "plugin host closed its output".to_owned(),
            Ok(read) => read,
            Err(error) => break format!("reading from the plugin host failed: {error}"),
        };
        let frames = match codec.push(&buffer[..read]) {
            Ok(frames) => frames,
            // Stdout is the protocol channel. Anything else on it means framing
            // is lost, and a decoder that has lost framing can associate a
            // reply with the wrong call.
            Err(error) => break framing_reason(&error),
        };
        for frame in frames {
            settle(&inner, frame).await;
        }
    };
    let failure = inner.failure_snapshot(reason);
    inner.fail_at_epoch(epoch, failure).await;
}

fn framing_reason(error: &CodecError) -> String {
    format!("plugin host wrote something that is not a protocol frame: {error}")
}

async fn settle(inner: &Arc<Inner>, frame: WireEnvelope) {
    let accepted = {
        let mut state = inner.state.lock().await;
        state.accept_inbound(&frame)
    };
    match accepted {
        Ok(Inbound::Terminal {
            call_id,
            status,
            payload,
        }) => {
            let waiter = inner
                .waiters
                .lock()
                .expect("waiters mutex")
                .remove(&call_id);
            if let Some(waiter) = waiter {
                let result = match status {
                    TerminalStatus::Success => Ok(payload),
                    status => Err(HostCallError::Rejected { status, payload }),
                };
                waiter.finish(result);
            }
        }
        Ok(Inbound::Chunk { call_id, payload }) => {
            let mut waiters = inner.waiters.lock().expect("waiters mutex");
            match waiters.get(&call_id) {
                Some(Waiter::Streaming(sender)) => {
                    let _ = sender.send(StreamEvent::Chunk(payload));
                }
                // A chunk arrived for a call whose caller asked for a single
                // answer. Dropping it would lose content silently, so the call
                // fails at the mistake instead: the caller chose the wrong
                // shape for a method that streams.
                Some(Waiter::Once(_)) => {
                    if let Some(waiter) = waiters.remove(&call_id) {
                        waiter.finish(Err(HostCallError::Malformed(format!(
                            "call {call_id} streamed a chunk but was not requested as a stream"
                        ))));
                    }
                }
                None => tracing::debug!(call_id, "chunk for a call nobody is waiting on"),
            }
        }
        Ok(Inbound::Stale { call_id }) => {
            tracing::debug!(call_id, "discarded a stale plugin host frame");
        }
        Ok(Inbound::Request {
            identity,
            method,
            payload,
        }) => {
            // Answered off the reader, not on it. Writing the reply here would
            // mean the one task draining the host's stdout is blocked on a
            // write into the host's stdin — and a host whose stdout is full
            // because nobody is reading it cannot drain its stdin either. The
            // debt is already recorded above, so the answer is owed no matter
            // which task produces it, and answers are matched by call id rather
            // than by arrival order.
            if method == EVENT_EMIT_METHOD {
                // The one method whose effect order is its meaning. Queued
                // rather than spawned; see `Inner::events`.
                if inner
                    .events
                    .send(QueuedEvent { identity, payload })
                    .is_err()
                {
                    tracing::warn!("event lane is closed; dropping an upstream event");
                }
                return;
            }
            let inner = Arc::clone(inner);
            tokio::spawn(async move { answer(&inner, identity, &method, payload).await });
        }
        Ok(Inbound::Notification { method, .. }) => {
            // A notification owes nothing back. `call/cancel` is the only one
            // the protocol defines, and the answers below are computed without
            // awaiting anything, so there is never a window to cancel.
            tracing::debug!(%method, "plugin host sent a notification");
        }
        Err(error) => {
            let failure = inner.failure_snapshot(format!(
                "plugin host sent a frame that breaks the call ledger: {error}"
            ));
            inner.fail(failure).await;
        }
    }
}

/// Publishes upstream events one at a time, in the order their frames arrived,
/// and answers each before taking the next.
///
/// The serialisation is the point. Anything else the host asks for is answered
/// on a task of its own, because answers are matched by call id and their order
/// carries no meaning. An event carries meaning in its order — a loop's
/// `turn/end` after the messages of that turn, not before them — and publishing
/// is a side effect that happens whether or not anyone reads the reply.
async fn drain_events(mut events: mpsc::UnboundedReceiver<QueuedEvent>, inner: Arc<Inner>) {
    while let Some(QueuedEvent { identity, payload }) = events.recv().await {
        answer(&inner, identity, EVENT_EMIT_METHOD, payload).await;
    }
}

/// Answers one request from the host.
///
/// The refusal branch matters as much as the successful one: a plugin that
/// called a method this side has not implemented is waiting on a promise, and
/// dropping the request would hang it with no diagnosis. Naming the method in
/// the refusal is what turns "the plugin hangs" into "this build does not have
/// that method yet".
async fn answer(inner: &Arc<Inner>, identity: CallIdentity, method: &str, payload: Payload) {
    let outcome = match method {
        EVENT_SUBSCRIBE_METHOD => match decode::<EventSubscribeRequest>(&payload) {
            Ok(request) => {
                let mut state = inner.state.lock().await;
                state
                    .registry_mut()
                    .subscribe(&identity, &request)
                    .map(|()| {
                        Payload::from(serde_json::json!({"subscribed": request.subscription}))
                    })
                    .map_err(|error| refusal(error.code(), error.to_string()))
            }
            Err(error) => Err(error),
        },
        EVENT_UNSUBSCRIBE_METHOD => match decode::<EventUnsubscribeRequest>(&payload) {
            Ok(request) => {
                let mut state = inner.state.lock().await;
                state
                    .registry_mut()
                    .unsubscribe(&identity.plugin_id, &request)
                    .map(|()| {
                        Payload::from(serde_json::json!({"unsubscribed": request.subscription}))
                    })
                    .map_err(|error| refusal(error.code(), error.to_string()))
            }
            Err(error) => Err(error),
        },
        TOOL_INVOKE_METHOD => match decode::<ToolInvokeRequest>(&payload) {
            Ok(request) => invoke_tool(inner, &identity, request).await,
            Err(error) => Err(error),
        },
        SEAT_CALL_METHOD => match decode::<SeatCallRequest>(&payload) {
            Ok(request) => call_seat(inner, &identity, request).await,
            Err(error) => Err(error),
        },
        EVENT_EMIT_METHOD => match decode::<EventEmitRequest>(&payload) {
            Ok(request) => publish_event(inner, &identity, request).await,
            Err(error) => Err(error),
        },
        other => Err(refusal(
            UNSUPPORTED_METHOD_CODE,
            format!("this supervisor does not implement {other}"),
        )),
    };
    let (status, payload) = match outcome {
        Ok(payload) => (TerminalStatus::Success, payload),
        Err(payload) => (TerminalStatus::Error, payload),
    };
    let reply = WireEnvelope::new(identity, WireMessage::Terminal { status, payload });

    let writable = {
        let mut state = inner.state.lock().await;
        state.commit_inbound_terminal(&reply)
    };
    match writable {
        Ok(true) => {
            if let Err(reason) = inner.write(&reply).await {
                let failure = inner.failure_snapshot(reason);
                inner.fail(failure).await;
            }
        }
        Ok(false) => tracing::debug!(
            call_id = reply.identity.call_id,
            "dropped an answer whose scope incarnation ended while it was computed"
        ),
        Err(error) => {
            let failure = inner.failure_snapshot(format!(
                "answering the plugin host is not possible: {error}"
            ));
            inner.fail(failure).await;
        }
    }
}

/// The refusal a request gets when this build has no handler for its method.
pub const UNSUPPORTED_METHOD_CODE: &str = "[UNSUPPORTED_METHOD]";

/// The refusal for a seat this plugin plane does not expose at all.
pub const UNAVAILABLE_SEAT_CODE: &str = "[UNAVAILABLE_SEAT]";

/// The refusal for publishing on a plane with no event publisher wired.
pub const UNAVAILABLE_EVENTS_CODE: &str = "[UNAVAILABLE_EVENTS]";

/// The refusal for a host that took a request and never answered it.
pub const HOST_UNANSWERED_CODE: &str = "[HOST_UNANSWERED]";

/// The refusal for a provider a plugin declares but never registers.
///
/// The mirror image of `[UNDECLARED_ADAPTER]`, which is a plugin registering
/// something its manifest never asked for. Both are the same question read in
/// opposite directions, and both have to be refused: a manifest that promises
/// a provider its `activate` never registers leaves a caller holding an
/// adapter that answers nothing.
pub const UNREGISTERED_PROVIDER_CODE: &str = "[UNREGISTERED_PROVIDER]";

/// Publishes one event for one plugin.
///
/// Two gates, the same shape the other two directions use: the manifest
/// declared the topic, and this build has somewhere to publish it. What the
/// event *means* is the publisher's business — this side only decides whether
/// the plugin was allowed to say it at all.
async fn publish_event(
    inner: &Arc<Inner>,
    identity: &CallIdentity,
    request: EventEmitRequest,
) -> Result<Payload, Payload> {
    {
        let state = inner.state.lock().await;
        state
            .registry()
            .admit_event_emit(identity, &request)
            .map_err(|error| refusal(error.code(), error.to_string()))?;
    }
    let Some(publisher) = inner.event_publisher.as_ref() else {
        return Err(refusal(
            UNAVAILABLE_EVENTS_CODE,
            format!(
                "topic {:?} cannot be published: this build has no event publisher",
                request.topic
            ),
        ));
    };
    publisher
        .publish(PublishedEvent {
            identity: identity.clone(),
            topic: request.topic,
            event: request.event,
        })
        .await
        .map_err(|refused| refusal(&refused.code, refused.message))
}

/// Calls one seat for one plugin, through the same three gates tools use.
async fn call_seat(
    inner: &Arc<Inner>,
    identity: &CallIdentity,
    request: SeatCallRequest,
) -> Result<Payload, Payload> {
    {
        let state = inner.state.lock().await;
        state
            .registry()
            .admit_seat_call(identity, &request)
            .map_err(|error| refusal(error.code(), error.to_string()))?;
    }
    if !inner.exposed_seats.contains(&request.seat) {
        return Err(refusal(
            UNAVAILABLE_SEAT_CODE,
            format!(
                "seat {:?} is not exposed to plugins by this build",
                request.seat
            ),
        ));
    }
    let Some(dispatcher) = inner.seat_dispatcher.as_ref() else {
        return Err(refusal(
            UNAVAILABLE_SEAT_CODE,
            "this plugin plane has no seat dispatcher installed".to_owned(),
        ));
    };
    dispatcher
        .call(SeatInvocation {
            identity: identity.clone(),
            seat: request.seat,
            method: request.method,
            params: request.params,
        })
        .await
        .map_err(|denial| refusal(&denial.code, denial.message))
}

/// The refusal for a tool this plugin plane does not expose at all.
///
/// Distinct from `[UNAUTHORIZED_TOOL]`, which means the plugin's own manifest
/// never asked for it. One is a packaging mistake the plugin author can fix;
/// the other is a tool this build simply does not offer. Collapsing them would
/// send an author to the wrong place.
pub const UNAVAILABLE_TOOL_CODE: &str = "[UNAVAILABLE_TOOL]";

/// Runs one tool for one plugin, through the three gates in order.
///
/// The manifest and the exposed set are checked here, before the embedder's
/// invoker is reached, so neither a buggy nor a permissive implementation can
/// widen what the plane offers. What the invoker decides — including asking the
/// user — is the third gate and belongs to it.
async fn invoke_tool(
    inner: &Arc<Inner>,
    identity: &CallIdentity,
    request: ToolInvokeRequest,
) -> Result<Payload, Payload> {
    {
        let state = inner.state.lock().await;
        state
            .registry()
            .admit_tool_invoke(identity, &request)
            .map_err(|error| refusal(error.code(), error.to_string()))?;
    }
    if !inner.exposed_tools.contains(&request.tool) {
        return Err(refusal(
            UNAVAILABLE_TOOL_CODE,
            format!(
                "tool {:?} is not exposed to plugins by this build",
                request.tool
            ),
        ));
    }
    let Some(invoker) = inner.tool_invoker.as_ref() else {
        return Err(refusal(
            UNAVAILABLE_TOOL_CODE,
            "this plugin plane has no tool invoker installed".to_owned(),
        ));
    };
    invoker
        .invoke(ToolInvocation {
            identity: identity.clone(),
            tool: request.tool,
            input: request.input,
        })
        .await
        .map_err(|denial| refusal(&denial.code, denial.message))
}

/// The error payload shape both sides use: a bracketed code and a message.
fn refusal(code: &str, message: String) -> Payload {
    Payload::from(serde_json::json!({"code": code, "message": message}))
}

fn decode<T: serde::de::DeserializeOwned>(payload: &Payload) -> Result<T, Payload> {
    let value = payload.to_value().map_err(|error| {
        refusal(
            "[MALFORMED_PAYLOAD]",
            format!("payload is not JSON: {error}"),
        )
    })?;
    serde_json::from_value(value).map_err(|error| {
        refusal(
            "[MALFORMED_PAYLOAD]",
            format!("payload does not match the method's schema: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `Inner` with no process behind it, sitting on `epoch`.
    ///
    /// Enough to exercise the failure path, which is the part of the
    /// supervisor a real child process is worst at reproducing: it needs two
    /// host generations to exist at a chosen instant.
    fn inner_at_epoch(epoch: u64) -> Inner {
        let (events, _events_rx) = mpsc::unbounded_channel();
        Inner {
            events,
            state: Mutex::new(SupervisorState::new(epoch).expect("state")),
            waiters: StdMutex::new(Waiters::new()),
            stdin: Mutex::new(None),
            stderr: Arc::new(StdMutex::new(StderrTail {
                limit: 64,
                bytes: Vec::new(),
            })),
            tool_invoker: None,
            exposed_tools: BTreeSet::new(),
            seat_dispatcher: None,
            exposed_seats: BTreeSet::new(),
            event_publisher: None,
        }
    }

    /// The bug this guards: a reader outlives the process it reads, so the EOF
    /// that wakes it can land after `restart` has installed a new host on the
    /// same `Inner`. Failing on it killed the *replacement*, and the restart
    /// came back as `HostFailed("plugin host closed its output")` — a message
    /// only the dead host's reader could produce.
    #[tokio::test]
    async fn a_previous_epochs_reader_cannot_fail_the_host_that_replaced_it() {
        let inner = inner_at_epoch(2);

        inner
            .fail_at_epoch(1, HostFailure::new("plugin host closed its output"))
            .await;

        let state = inner.state.lock().await;
        assert!(state.is_alive(), "the epoch-2 host is still running");
        assert!(state.failure().is_none(), "nothing was recorded against it");
    }

    /// The other half: the check must not make the reader toothless.
    #[tokio::test]
    async fn the_running_epochs_reader_still_fails_the_host() {
        let inner = inner_at_epoch(2);

        inner
            .fail_at_epoch(2, HostFailure::new("plugin host closed its output"))
            .await;

        let state = inner.state.lock().await;
        assert!(!state.is_alive());
        assert_eq!(
            state.failure().map(|failure| failure.reason.as_str()),
            Some("plugin host closed its output")
        );
    }

    #[test]
    fn the_stderr_tail_keeps_the_end_not_the_beginning() {
        let mut tail = StderrTail {
            limit: 8,
            bytes: Vec::new(),
        };
        tail.push(b"0123456789abc");
        assert_eq!(tail.snapshot(), "56789abc");
        assert_eq!(tail.bytes.len(), 8);
    }

    #[test]
    fn the_stderr_tail_survives_a_split_multibyte_character() {
        let mut tail = StderrTail {
            limit: 4,
            bytes: Vec::new(),
        };
        tail.push("一二".as_bytes());
        // The window can cut a character in half; the snapshot must still be a
        // string a failure message can carry.
        assert!(!tail.snapshot().is_empty());
    }

    #[test]
    fn shipped_scripts_are_sought_beside_the_binary_then_in_bundle_resources() {
        let exe_dir = PathBuf::from("/Applications/Rebon.app/Contents/MacOS");
        let candidates = shipped_script_candidates(&exe_dir, "plugin-host/src/cli.mjs");
        assert_eq!(
            candidates,
            vec![
                exe_dir.join("plugin-host/src/cli.mjs"),
                PathBuf::from("/Applications/Rebon.app/Contents/Resources")
                    .join("plugin-host/src/cli.mjs"),
            ]
        );
    }

    #[test]
    fn a_rootless_exe_dir_yields_only_the_beside_candidate() {
        // A parentless directory has no Resources sibling to offer.
        let candidates = shipped_script_candidates(Path::new("/"), "compose-runtime/src/index.mjs");
        assert_eq!(
            candidates,
            vec![Path::new("/compose-runtime/src/index.mjs")]
        );
    }

    #[test]
    fn a_shipped_script_in_bundle_resources_is_found() {
        let bundle = tempfile::tempdir().unwrap();
        let exe_dir = bundle.path().join("Contents/MacOS");
        let script = bundle
            .path()
            .join("Contents/Resources/plugin-host/src/cli.mjs");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(&script, "export {}\n").unwrap();
        let found = shipped_script_candidates(&exe_dir, "plugin-host/src/cli.mjs")
            .into_iter()
            .find(|path| path.is_file());
        assert_eq!(found, Some(script));
    }

    #[test]
    fn a_relative_host_script_override_is_refused() {
        let error = match locate_host_script_from(Some(PathBuf::from("cli.mjs"))) {
            Err(SupervisorError::HostScript(message)) => message,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert!(error.contains("absolute"), "{error}");
    }

    #[test]
    fn a_missing_host_script_override_names_the_path() {
        let missing = std::env::temp_dir().join("rebon-plugin-host-absent/cli.mjs");
        let error = match locate_host_script_from(Some(missing.clone())) {
            Err(SupervisorError::HostScript(message)) => message,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert!(error.contains("does not exist"), "{error}");
    }

    #[test]
    fn an_existing_host_script_override_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.mjs");
        std::fs::write(&script, "export {}\n").unwrap();
        assert_eq!(
            locate_host_script_from(Some(script.clone())).unwrap(),
            script
        );
    }

    /// The env-var half of [`locate_host_script`], without touching the
    /// process environment — which other tests share.
    fn locate_host_script_from(override_path: Option<PathBuf>) -> Result<PathBuf, SupervisorError> {
        match override_path {
            Some(path) if !path.is_absolute() => Err(SupervisorError::HostScript(format!(
                "{HOST_SCRIPT_ENV} must be an absolute path, got {}",
                path.display()
            ))),
            Some(path) if !path.is_file() => Err(SupervisorError::HostScript(format!(
                "{HOST_SCRIPT_ENV} points at {}, which does not exist",
                path.display()
            ))),
            Some(path) => Ok(path),
            None => locate_host_script(),
        }
    }
}

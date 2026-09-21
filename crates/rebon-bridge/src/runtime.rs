//! Bridge runtime — `ReplBridgeHandle` + `start_bridge_runtime`.
//!
//! This is the **usable** entry point of the crate. It glues the pieces
//! here together into a handle a harness can actually run against a live
//! bridge:
//!
//! 1. A caller constructs a [`BridgeConfig`] and picks a
//!    [`BridgeApiClient`] implementation — the test double
//!    [`InMemoryBridgeApiClient`] in this crate, or the real HTTP client
//!    behind the `http` feature.
//! 2. They call [`start_bridge_runtime`], which:
//!    - registers the environment,
//!    - mints a [`ReplBridgeHandle`] holding the environment id +
//!      secret + session id (`bridge_session_id`, satisfying the
//!      [`crate::active_handle::BridgeHandle`] contract),
//!    - spawns a background poll task that drives
//!      [`BridgeApiClient::poll_for_work`] on a loop until the handle
//!      is shut down,
//!    - returns an `Arc<ReplBridgeHandle>` the caller holds and later
//!      shuts down.
//! 3. On teardown the caller calls
//!    [`ReplBridgeHandle::shutdown`], which signals the poll task to
//!    stop, awaits its completion, and calls
//!    [`BridgeApiClient::deregister_environment`] best-effort.
//!
//! The poll loop is deliberately minimal: it counts polls, stops on
//! shutdown, and records any permanent errors. Real session spawning,
//! work acknowledgement, and child-process lifecycle are out of scope
//! for this crate — they belong in a session-runner layer above it.
//! The runtime is already enough to exercise the full attach-detach path
//! end to end.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;
// `tokio`'s clock rather than `std`'s: the poll loop already sleeps on it,
// and a test that pauses time can then move the session deadline too.
use tokio::time::Instant;

use crate::active_handle::BridgeHandle;
use crate::api_client::{BridgeApiClient, BridgeApiError, PollOptions};
use crate::config::{BridgeConfig, RegisteredEnvironment};
use crate::constants::DEFAULT_SESSION_TIMEOUT_MS;

/// High-level status of the runtime's poll loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStatus {
    /// The poll task has been spawned but has not yet reached its
    /// first loop iteration.
    Starting,
    /// The poll task is running and has completed at least one
    /// iteration (or is actively blocked on a poll call).
    Running,
    /// A [`ReplBridgeHandle::shutdown`] request is in flight — the
    /// poll task has observed the shutdown signal and is winding down
    /// (including the best-effort `deregister_environment`).
    ShuttingDown,
    /// The runtime has been torn down cleanly.
    Stopped,
    /// The runtime observed a non-transient error from
    /// `poll_for_work` and stopped.
    Failed,
    /// The bridge session outlived
    /// [`BridgeConfig::session_timeout_ms`] and was stopped.
    ///
    /// Distinct from [`Self::Failed`]: nothing went wrong, the session
    /// simply reached the bound the service registered it with. It is also
    /// distinct from [`Self::Stopped`], which is a caller asking.
    TimedOut,
}

impl RuntimeStatus {
    /// True when the runtime is in a terminal state: stopped, failed, or
    /// timed out.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped | Self::Failed | Self::TimedOut)
    }
}

/// Tunables for the runtime's poll loop.
///
/// `idle_interval` is the base delay between polls when no work is
/// returned, `error_backoff` is the delay after a transient error,
/// `max_consecutive_errors` is the cutoff after which a transient
/// error is promoted to a hard failure.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeOptions {
    /// Delay between polls when the previous poll returned `Ok(None)`.
    pub idle_interval: Duration,
    /// Delay between polls when the previous poll returned an error.
    pub error_backoff: Duration,
    /// After this many consecutive transient errors the runtime
    /// transitions to [`RuntimeStatus::Failed`] and stops polling.
    pub max_consecutive_errors: u32,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            idle_interval: Duration::from_secs(5),
            error_backoff: Duration::from_secs(1),
            max_consecutive_errors: 5,
        }
    }
}

impl RuntimeOptions {
    /// Fast options useful for tests: single-millisecond delays, same
    /// error cutoff. Keeps the tests snappy without shortening the
    /// production defaults.
    pub fn for_tests() -> Self {
        Self {
            idle_interval: Duration::from_millis(1),
            error_backoff: Duration::from_millis(1),
            max_consecutive_errors: 3,
        }
    }
}

/// Inner state shared between [`ReplBridgeHandle`] and its background
/// poll task.
#[derive(Debug)]
struct HandleShared {
    registered: RegisteredEnvironment,
    config: BridgeConfig,
    poll_count: AtomicU64,
    error_count: AtomicU64,
    shutdown: AtomicBool,
    notify_shutdown: Notify,
    status_tx: watch::Sender<RuntimeStatus>,
    last_error: Mutex<Option<String>>,
    /// When this bridge session started, for the session timeout below.
    /// Taken at registration rather than at the first poll so the bound
    /// covers the whole session, not just its polling.
    started_at: Instant,
}

impl HandleShared {
    fn set_status(&self, status: RuntimeStatus) {
        // `send` only fails if there are no receivers, which is fine —
        // the status is observable via the `status_rx` clone held on
        // the handle regardless.
        let _ = self.status_tx.send(status);
    }

    fn record_error(&self, message: impl Into<String>) {
        let mut guard = self.last_error.lock().expect("last_error poisoned");
        *guard = Some(message.into());
    }
}

/// Concrete bridge handle returned by [`start_bridge_runtime`].
///
/// Implements [`BridgeHandle`] so it can be handed to
/// `Engine::attach_bridge` directly. Cloneable via `Arc` so multiple
/// call sites can observe status without touching the poll task.
pub struct ReplBridgeHandle {
    shared: Arc<HandleShared>,
    api_client: Arc<dyn BridgeApiClient>,
    status_rx: watch::Receiver<RuntimeStatus>,
    // `Mutex<Option<JoinHandle>>` so `shutdown` can take ownership of
    // the handle (`JoinHandle::await` requires owned access) while the
    // containing struct stays shared behind `Arc`.
    poll_task: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for ReplBridgeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplBridgeHandle")
            .field("environment_id", &self.shared.registered.environment_id)
            .field("status", &self.status())
            .field("poll_count", &self.poll_count())
            .field("error_count", &self.error_count())
            .finish()
    }
}

impl ReplBridgeHandle {
    /// Current runtime status.
    pub fn status(&self) -> RuntimeStatus {
        *self.status_rx.borrow()
    }

    /// Number of poll iterations executed so far.
    pub fn poll_count(&self) -> u64 {
        self.shared.poll_count.load(Ordering::Relaxed)
    }

    /// Number of consecutive errors observed in the poll loop since
    /// the last successful poll.
    pub fn error_count(&self) -> u64 {
        self.shared.error_count.load(Ordering::Relaxed)
    }

    /// Clone of the last error message recorded by the poll loop.
    pub fn last_error(&self) -> Option<String> {
        self.shared
            .last_error
            .lock()
            .expect("last_error poisoned")
            .clone()
    }

    /// Borrow the configuration used to bring up the bridge.
    pub fn config(&self) -> &BridgeConfig {
        &self.shared.config
    }

    /// Backend-issued environment id.
    pub fn environment_id(&self) -> &str {
        &self.shared.registered.environment_id
    }

    /// Clone the underlying API client so callers can issue ad-hoc
    /// requests (e.g. `archive_session`) without going through the
    /// poll loop.
    pub fn api_client(&self) -> Arc<dyn BridgeApiClient> {
        self.api_client.clone()
    }

    /// Wait until the runtime reports a [`RuntimeStatus::Running`]
    /// status. Useful for tests and callers that want to delay work
    /// submission until the first poll iteration has landed.
    pub async fn wait_until_running(&self) -> RuntimeStatus {
        let mut rx = self.status_rx.clone();
        loop {
            let current = *rx.borrow();
            if current != RuntimeStatus::Starting {
                return current;
            }
            if rx.changed().await.is_err() {
                return *rx.borrow();
            }
        }
    }

    /// Wait until the runtime reports a terminal status (`Stopped`
    /// or `Failed`).
    pub async fn wait_until_terminal(&self) -> RuntimeStatus {
        let mut rx = self.status_rx.clone();
        loop {
            let current = *rx.borrow();
            if current.is_terminal() {
                return current;
            }
            if rx.changed().await.is_err() {
                return *rx.borrow();
            }
        }
    }

    /// Trigger a graceful shutdown.
    ///
    /// Sets the shutdown flag, notifies the poll task, awaits its
    /// join, then calls `deregister_environment` on a best-effort
    /// basis. The returned `Result` surfaces the deregister outcome
    /// only — the poll task is considered to have succeeded in all
    /// cases where it exits cleanly.
    pub async fn shutdown(&self) -> Result<(), BridgeApiError> {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.notify_shutdown.notify_waiters();

        // Move `ShuttingDown` before awaiting the task so observers
        // can distinguish "shutdown requested" from "shutdown completed".
        if !self.status().is_terminal() {
            self.shared.set_status(RuntimeStatus::ShuttingDown);
        }

        let task = {
            let mut guard = self.poll_task.lock().expect("poll_task slot poisoned");
            guard.take()
        };
        if let Some(task) = task {
            let _ = task.await;
        }

        // Only flip to Stopped if the loop wasn't already in Failed state.
        if !matches!(self.status(), RuntimeStatus::Failed) {
            self.shared.set_status(RuntimeStatus::Stopped);
        }

        // Best-effort deregister — the caller sees the error but the
        // handle is still considered torn down.
        self.api_client
            .deregister_environment(&self.shared.registered.environment_id)
            .await
    }
}

impl BridgeHandle for ReplBridgeHandle {
    fn bridge_session_id(&self) -> &str {
        &self.shared.registered.environment_id
    }

    fn runtime_status(&self) -> Option<RuntimeStatus> {
        Some(self.status())
    }

    fn last_error(&self) -> Option<String> {
        ReplBridgeHandle::last_error(self)
    }

    fn shutdown_blocking(&self, runtime: &tokio::runtime::Handle) -> Result<(), String> {
        runtime
            .block_on(self.shutdown())
            .map_err(|err| err.to_string())
    }
}

/// Bring up a bridge runtime.
///
/// Registers the environment, spawns the poll task, and returns an
/// `Arc<ReplBridgeHandle>` the caller can hand to
/// `Engine::attach_bridge`. The poll task is owned by the handle and
/// torn down via [`ReplBridgeHandle::shutdown`].
///
/// Errors at this layer are always registration failures — once the
/// environment is registered, the poll loop handles its own transient
/// failures until `max_consecutive_errors` is hit.
pub async fn start_bridge_runtime(
    config: BridgeConfig,
    api_client: Arc<dyn BridgeApiClient>,
    options: RuntimeOptions,
) -> Result<Arc<ReplBridgeHandle>, BridgeApiError> {
    let registered = api_client.register_bridge_environment(&config).await?;
    let (status_tx, status_rx) = watch::channel(RuntimeStatus::Starting);

    let shared = Arc::new(HandleShared {
        registered,
        config,
        poll_count: AtomicU64::new(0),
        error_count: AtomicU64::new(0),
        shutdown: AtomicBool::new(false),
        notify_shutdown: Notify::new(),
        status_tx,
        last_error: Mutex::new(None),
        started_at: Instant::now(),
    });

    let task_shared = Arc::clone(&shared);
    let task_client = Arc::clone(&api_client);
    let task_options = options;

    let poll_task = tokio::spawn(async move {
        run_poll_loop(task_shared, task_client, task_options).await;
    });

    Ok(Arc::new(ReplBridgeHandle {
        shared,
        api_client,
        status_rx,
        poll_task: Mutex::new(Some(poll_task)),
    }))
}

/// How long this bridge session may run before the loop stops it.
///
/// `None` on the config means the service did not say, so the product
/// default applies. An explicit `0` disables the bound instead of expiring
/// the session immediately: a zero-length session is never what a caller
/// means, and treating it as "no limit" is the reading that cannot
/// surprise one.
fn session_deadline(config: &BridgeConfig, started_at: Instant) -> Option<Instant> {
    let ms = config
        .session_timeout_ms
        .unwrap_or(DEFAULT_SESSION_TIMEOUT_MS);
    if ms == 0 {
        return None;
    }
    started_at.checked_add(Duration::from_millis(ms))
}

async fn run_poll_loop(
    shared: Arc<HandleShared>,
    api_client: Arc<dyn BridgeApiClient>,
    options: RuntimeOptions,
) {
    shared.set_status(RuntimeStatus::Running);
    let deadline = session_deadline(&shared.config, shared.started_at);

    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            return;
        }

        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                shared.record_error(format!(
                    "bridge session exceeded its {} ms timeout",
                    shared
                        .config
                        .session_timeout_ms
                        .unwrap_or(DEFAULT_SESSION_TIMEOUT_MS)
                ));
                shared.set_status(RuntimeStatus::TimedOut);
                return;
            }
        }

        let poll_options = PollOptions::default();
        let outcome = api_client
            .poll_for_work(
                &shared.registered.environment_id,
                &shared.registered.environment_secret,
                poll_options,
            )
            .await;

        shared.poll_count.fetch_add(1, Ordering::Relaxed);

        let delay = match outcome {
            Ok(_) => {
                shared.error_count.store(0, Ordering::Relaxed);
                options.idle_interval
            }
            Err(err) if err.is_transient() => {
                let now = shared.error_count.fetch_add(1, Ordering::Relaxed) + 1;
                shared.record_error(format!("transient poll error: {err}"));
                tracing::debug!(
                    error = %err,
                    consecutive_errors = now,
                    "rebon-bridge poll loop transient error"
                );
                if now >= options.max_consecutive_errors as u64 {
                    shared.set_status(RuntimeStatus::Failed);
                    return;
                }
                options.error_backoff
            }
            Err(err) => {
                shared.record_error(format!("permanent poll error: {err}"));
                tracing::warn!(error = %err, "rebon-bridge poll loop permanent error");
                shared.set_status(RuntimeStatus::Failed);
                return;
            }
        };

        if shared.shutdown.load(Ordering::SeqCst) {
            return;
        }

        // Sleep, but wake early on shutdown — and never sleep past the
        // session deadline, or a long idle interval would let the session
        // outlive its bound by most of one poll cycle.
        let delay = match deadline {
            Some(deadline) => delay.min(deadline.saturating_duration_since(Instant::now())),
            None => delay,
        };
        tokio::select! {
            biased;
            _ = shared.notify_shutdown.notified() => {
                return;
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_client::{BridgeApiError, InMemoryBridgeApiClient, RecordedMethod};
    use crate::config::{BridgeConfig, WorkData, WorkDataType, WorkResponse};

    fn sample_config() -> BridgeConfig {
        BridgeConfig::minimal("bridge-1", "env-1", "https://api", "wss://sess")
    }

    #[test]
    fn a_zero_session_timeout_disables_the_bound_and_none_takes_the_default() {
        let started = Instant::now();

        let mut off = sample_config();
        off.session_timeout_ms = Some(0);
        assert!(
            session_deadline(&off, started).is_none(),
            "0 means no limit, not a session that expires on arrival"
        );

        let unset = sample_config();
        assert_eq!(unset.session_timeout_ms, None);
        assert_eq!(
            session_deadline(&unset, started),
            started.checked_add(Duration::from_millis(DEFAULT_SESSION_TIMEOUT_MS)),
        );

        let mut explicit = sample_config();
        explicit.session_timeout_ms = Some(1_500);
        assert_eq!(
            session_deadline(&explicit, started),
            started.checked_add(Duration::from_millis(1_500)),
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_session_that_outlives_its_timeout_stops_as_timed_out() {
        // The field was registered, round-tripped and documented as
        // "sessions exceeding this are killed" while nothing read it. This
        // is the clock that makes that sentence true.
        let client = Arc::new(InMemoryBridgeApiClient::new());
        let mut config = sample_config();
        config.session_timeout_ms = Some(30);

        let handle = start_bridge_runtime(
            config,
            client as Arc<dyn BridgeApiClient>,
            RuntimeOptions::for_tests(),
        )
        .await
        .unwrap();
        assert_eq!(handle.wait_until_running().await, RuntimeStatus::Running);

        let status = handle.wait_until_terminal().await;
        assert_eq!(status, RuntimeStatus::TimedOut);
        assert!(status.is_terminal());
        assert!(
            handle
                .last_error()
                .is_some_and(|message| message.contains("30 ms timeout")),
            "{:?}",
            handle.last_error()
        );
    }

    fn sample_work(id: &str) -> WorkResponse {
        WorkResponse {
            id: id.into(),
            response_type: "work".into(),
            environment_id: "env-1".into(),
            state: "ready".into(),
            data: WorkData {
                data_type: WorkDataType::Session,
                id: format!("sess-{id}"),
            },
            secret: "opaque".into(),
            created_at: "2026-04-09T00:00:00.000Z".into(),
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn start_bridge_runtime_registers_and_spawns_poll_loop() {
        let client = Arc::new(InMemoryBridgeApiClient::new());
        let handle = start_bridge_runtime(
            sample_config(),
            client.clone() as Arc<dyn BridgeApiClient>,
            RuntimeOptions::for_tests(),
        )
        .await
        .unwrap();

        // Wait for the poll loop to flip from Starting to Running.
        let state = handle.wait_until_running().await;
        assert_eq!(state, RuntimeStatus::Running);

        // Let a few poll iterations run, then shut down. Under
        // `start_paused = true` we have to advance the virtual clock
        // manually to unblock the sleeps between polls.
        for _ in 0..3 {
            tokio::time::advance(Duration::from_millis(5)).await;
        }
        tokio::task::yield_now().await;

        assert!(handle.poll_count() >= 1);
        handle.shutdown().await.unwrap();
        assert!(matches!(
            handle.status(),
            RuntimeStatus::Stopped | RuntimeStatus::Failed
        ));

        // Environment should have been deregistered on shutdown.
        let methods: Vec<_> = client.calls().into_iter().map(|c| c.method).collect();
        assert!(methods.contains(&RecordedMethod::Register));
        assert!(methods.contains(&RecordedMethod::Poll));
        assert!(methods.contains(&RecordedMethod::Deregister));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn poll_loop_consumes_scripted_work_responses() {
        let client = Arc::new(InMemoryBridgeApiClient::new());
        client.push_poll(Ok(Some(sample_work("1"))));
        client.push_poll(Ok(Some(sample_work("2"))));

        let handle = start_bridge_runtime(
            sample_config(),
            client.clone() as Arc<dyn BridgeApiClient>,
            RuntimeOptions::for_tests(),
        )
        .await
        .unwrap();
        handle.wait_until_running().await;

        for _ in 0..5 {
            tokio::time::advance(Duration::from_millis(5)).await;
            tokio::task::yield_now().await;
        }
        handle.shutdown().await.unwrap();

        assert!(handle.poll_count() >= 2);
        assert_eq!(handle.error_count(), 0);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn poll_loop_stops_after_permanent_error() {
        let client = Arc::new(InMemoryBridgeApiClient::new());
        client.inject_error(
            RecordedMethod::Poll,
            BridgeApiError::Permanent("gone".into()),
        );

        let handle = start_bridge_runtime(
            sample_config(),
            client.clone() as Arc<dyn BridgeApiClient>,
            RuntimeOptions::for_tests(),
        )
        .await
        .unwrap();

        // Permanent error → runtime should move to Failed quickly.
        let terminal = handle.wait_until_terminal().await;
        assert_eq!(terminal, RuntimeStatus::Failed);
        assert!(handle.last_error().is_some());
        assert!(handle.last_error().unwrap().contains("permanent"));

        // Shutdown on a failed runtime still deregisters the environment.
        handle.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn poll_loop_survives_bounded_transient_errors_then_fails() {
        let client = Arc::new(InMemoryBridgeApiClient::new());
        // Inject 3 transient errors back-to-back. `for_tests` sets
        // `max_consecutive_errors = 3`, so the third one should push
        // the runtime into Failed.
        for _ in 0..3 {
            client.inject_error(
                RecordedMethod::Poll,
                BridgeApiError::Transient("hiccup".into()),
            );
        }

        let handle = start_bridge_runtime(
            sample_config(),
            client.clone() as Arc<dyn BridgeApiClient>,
            RuntimeOptions::for_tests(),
        )
        .await
        .unwrap();

        // Drive the clock forward enough to consume all three errors.
        for _ in 0..10 {
            tokio::time::advance(Duration::from_millis(5)).await;
            tokio::task::yield_now().await;
        }
        let terminal = handle.wait_until_terminal().await;
        assert_eq!(terminal, RuntimeStatus::Failed);
        assert_eq!(handle.error_count(), 3);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn transient_error_followed_by_success_resets_error_counter() {
        let client = Arc::new(InMemoryBridgeApiClient::new());
        client.inject_error(
            RecordedMethod::Poll,
            BridgeApiError::Transient("blip".into()),
        );
        client.push_poll(Ok(Some(sample_work("ok"))));

        let handle = start_bridge_runtime(
            sample_config(),
            client.clone() as Arc<dyn BridgeApiClient>,
            RuntimeOptions::for_tests(),
        )
        .await
        .unwrap();
        handle.wait_until_running().await;

        for _ in 0..5 {
            tokio::time::advance(Duration::from_millis(5)).await;
            tokio::task::yield_now().await;
        }
        // Counter should have been reset by the successful poll.
        assert_eq!(handle.error_count(), 0);
        assert!(handle.poll_count() >= 2);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn bridge_handle_trait_exposes_environment_id() {
        let client = Arc::new(InMemoryBridgeApiClient::new());
        let handle = start_bridge_runtime(
            sample_config(),
            client as Arc<dyn BridgeApiClient>,
            RuntimeOptions::for_tests(),
        )
        .await
        .unwrap();
        // `BridgeHandle::bridge_session_id` returns the environment
        // id — that is the ReplBridgeHandle mapping.
        assert_eq!(handle.bridge_session_id(), "env-inmemory-bridge-1");
        assert_eq!(handle.environment_id(), "env-inmemory-bridge-1");
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn register_failure_surfaces_from_start_bridge_runtime() {
        let client = Arc::new(InMemoryBridgeApiClient::new());
        client.inject_error(
            RecordedMethod::Register,
            BridgeApiError::Unauthorized("login first".into()),
        );
        let err = start_bridge_runtime(
            sample_config(),
            client as Arc<dyn BridgeApiClient>,
            RuntimeOptions::for_tests(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BridgeApiError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn shutdown_is_idempotent_when_called_twice() {
        let client = Arc::new(InMemoryBridgeApiClient::new());
        let handle = start_bridge_runtime(
            sample_config(),
            client as Arc<dyn BridgeApiClient>,
            RuntimeOptions::for_tests(),
        )
        .await
        .unwrap();
        handle.shutdown().await.unwrap();
        // Second shutdown: poll task is already gone, deregister goes
        // through again. This is fine — the in-memory client just
        // records another call.
        handle.shutdown().await.unwrap();
        assert!(matches!(handle.status(), RuntimeStatus::Stopped));
    }
}

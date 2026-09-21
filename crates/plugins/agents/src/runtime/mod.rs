//! What actually runs a sub-agent: the spawner, the worker loop behind it,
//! and the team manager that keeps named teammates alive between turns.
//!
//! `rebon_core::run_query` **already does what a worker needs** — spawn a
//! tokio task, drive a fresh agentic loop, emit events, respect a cancel
//! handle. A worker is that call with a curated parameter set plus a facade
//! that presents the result as "here is what the sub-agent said".
//!
//! - [`worker::WorkerSpec`] — the input bundle every caller constructs:
//!   prompt, tool subset, system prompt, max iterations, model.
//! - [`worker::WorkerHandle`] — what [`worker::spawn_worker`] returns: the
//!   event stream, a cancel hook, a completion future, and incremental
//!   progress accessors.
//! - [`spawner::EngineSubAgentSpawner`] — the [`rebon_tool::SubAgentSpawner`]
//!   the `Agent` tool calls, wiring a spec to a real engine and model client.
//! - [`team_manager::InProcessTeamManager`] and its multi-session adapter —
//!   the [`rebon_tool::TeamManager`] behind `TeamCreate` / `SendMessage`.
//!
//! The runtime lives in this plugin rather than beside it because a sub-agent is
//! this plugin's whole subject: turning `plugins.agents.enabled` off used to
//! take the `Agent` tool off the seat while the front end went on building a
//! spawner nobody could reach. The task execution runtime is still owned by
//! [`rebon_plugin_tasks::runtime`]; this module only bridges worker handles
//! into it.

pub mod spawner;
pub mod team_manager;
pub mod worker;

#[cfg(test)]
thread_local! {
    static TEST_ENV_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The ONE process-wide lock serialising env-var access across tests.
///
/// Every test guard that touches a process-global (`REBON_CONFIG_DIR`,
/// `REBON_COORDINATOR_MODE`, …) takes this and only this. A second lock
/// would let two tests acquire them in opposite orders and deadlock —
/// which is exactly what happened when a `TestTaskHome` was placed above
/// a `CoordModeGuard` in two tests while four others had them the other
/// way round: 32 tests hung and the suite never finished.
///
/// The mutex itself is [`rebon_tool::env_test_lock`], the one the rest of
/// this crate's tests already take: these tests now share a binary with
/// them, and two locks over the same environment would be the same fault
/// one crate boundary further out.
///
/// Re-entrant per thread, because a single test legitimately holds
/// several of these guards at once and `std::sync::Mutex` is not
/// re-entrant. `#[tokio::test]` is current-thread, so a test body never
/// migrates threads between acquisitions; a guard taken on a *different*
/// thread still blocks, which is the intended serialisation.
#[cfg(test)]
pub(crate) fn test_config_env_lock() -> TestEnvLock {
    let depth = TEST_ENV_DEPTH.with(|d| d.get());
    // Only the outermost guard on this thread takes the mutex; inner ones
    // ride along, and nothing is released until the outer one drops.
    let guard = (depth == 0).then(|| {
        rebon_tool::env_test_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    });
    TEST_ENV_DEPTH.with(|d| d.set(depth + 1));
    TestEnvLock { _guard: guard }
}

/// Guard returned by [`test_config_env_lock`]. `!Send` by construction
/// (it holds a `MutexGuard`), which is what keeps the thread-local
/// re-entrancy count balanced.
#[cfg(test)]
pub(crate) struct TestEnvLock {
    _guard: Option<std::sync::MutexGuard<'static, ()>>,
}

#[cfg(test)]
impl Drop for TestEnvLock {
    fn drop(&mut self) {
        TEST_ENV_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

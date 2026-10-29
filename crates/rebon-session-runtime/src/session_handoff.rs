//! What a worker keeps across the sessions it builds.
//!
//! A worker outlives any one of its sessions. The ownership lock, MCP stack,
//! kernel session-scope table and cron scheduler therefore return to the
//! worker between turns.

use std::sync::Arc;

use rebon_kernel_seats::kernel_services::SessionKernelScopes;

/// Handles a caller lends to the session it is about to build.
#[derive(Default)]
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub struct HandedBack {
    pub session_lock: Option<rebon_session::HeldSessionLock>,
    pub mcp: Option<crate::mcp::SessionMcp>,
    pub kernel_scopes: Option<HeldKernelScopes>,
    pub cron: Option<HeldCron>,
    pub session_loop: Option<HeldSessionLoop>,
}

/// The session loop a worker's turns run through, kept across the sessions it
/// builds for one conversation so the loop, and what waits in its inbox,
/// outlive any single turn's session.
pub struct HeldSessionLoop {
    pub session_id: String,
    pub session_loop: crate::session_loop::SessionLoop,
}

impl HeldSessionLoop {
    /// Lend the loop only to the session it belongs to.
    pub(crate) fn take_for(
        held: Option<Self>,
        session_id: &str,
    ) -> Option<crate::session_loop::SessionLoop> {
        held.filter(|held| held.session_id == session_id)
            .map(|held| held.session_loop)
    }
}

/// A session's cron scheduler and the queue it fires into, kept between the
/// sessions a worker builds for one conversation.
///
/// A scheduler built with each turn's session died with it: nothing fired
/// between turns, and a prompt still queued when the turn ended was dropped.
/// Kept here, it runs for as long as the worker does, holding the cwd's
/// owner lock, and the worker delivers what it fires while idle.
pub struct HeldCron {
    pub session_id: String,
    pub cwd: String,
    pub poller: Arc<rebon_core::cron::CronPoller>,
    pub session_store: Arc<rebon_tool::SessionCronStore>,
    pub scheduler: rebon_core::cron::SchedulerHandle,
}

impl HeldCron {
    /// Lend the scheduler only to the same session in the same directory; a
    /// mismatch drops it, which stops it and releases the lock.
    pub(crate) fn take_for(
        held: Option<Self>,
        session_id: Option<&str>,
        cwd: &str,
    ) -> Option<Self> {
        let held = held?;
        (session_id == Some(held.session_id.as_str()) && held.cwd == cwd).then_some(held)
    }
}

/// A bounded kernel scope table retained for one exact session.
///
/// The task-registry service owns task identity inside this table. Retaining
/// the table, rather than injecting a registry directly, preserves task state
/// across worker turns without introducing a second source of truth.
pub struct HeldKernelScopes {
    pub session_id: String,
    pub scopes: Arc<SessionKernelScopes>,
}

impl HeldKernelScopes {
    /// Return the table only to the session it belongs to.
    pub(crate) fn take_for(
        held: Option<Self>,
        session_id: Option<&str>,
    ) -> Option<Arc<SessionKernelScopes>> {
        let held = held?;
        match session_id {
            Some(wanted) if held.session_id == wanted => Some(held.scopes),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_core::Engine;

    fn held(session_id: &str) -> HeldKernelScopes {
        HeldKernelScopes {
            session_id: session_id.to_string(),
            scopes: SessionKernelScopes::new(
                rebon_harness::kernel_bootstrap::process_kernel(),
                Arc::new(Engine::with_builtin_tools()),
                rebon_harness::projects_root(),
            ),
        }
    }

    #[tokio::test]
    async fn held_cron_is_lent_only_to_its_own_session_and_directory() {
        let held = |session_id: &str| HeldCron {
            session_id: session_id.to_string(),
            cwd: "/work".to_string(),
            poller: rebon_core::cron::CronPoller::new(),
            session_store: rebon_tool::SessionCronStore::new(),
            scheduler: rebon_core::cron::start_scheduler(
                rebon_core::cron::SchedulerConfig::new(std::env::temp_dir()),
                rebon_core::cron::CronPoller::new(),
            ),
        };
        assert!(HeldCron::take_for(Some(held("s1")), Some("s1"), "/work").is_some());
        assert!(HeldCron::take_for(Some(held("s1")), Some("s2"), "/work").is_none());
        assert!(HeldCron::take_for(Some(held("s1")), Some("s1"), "/elsewhere").is_none());
        assert!(HeldCron::take_for(Some(held("s1")), None, "/work").is_none());
        assert!(HeldCron::take_for(None, Some("s1"), "/work").is_none());
    }

    #[test]
    fn direct_task_registry_injection_paths_stay_removed() {
        let sources = [
            include_str!("build.rs"),
            include_str!("runtime.rs"),
            include_str!("task_notification_poller.rs"),
            // Lives in rebon-cli, not here; still checked because it must not
            // grow a direct registry path of its own.
            include_str!("../../rebon-cli/src/tui/runner/prompt_lifecycle.rs"),
            include_str!("host/ipc/server.rs"),
            include_str!("../../plugins/agents/src/runtime/spawner/mod.rs"),
            include_str!("../../plugins/agents/src/runtime/spawner/builder.rs"),
            include_str!("../../plugins/agents/src/runtime/spawner/spawn_paths.rs"),
            include_str!("../../plugins/agents/src/runtime/spawner/sub_agent_spawner.rs"),
            include_str!("../../plugins/agents/src/runtime/spawner/worker_run.rs"),
            include_str!("../../plugins/agents/src/runtime/spawner/worker_setup.rs"),
            include_str!("../../plugins/workflow/src/runtime.rs"),
        ]
        .join("\n");
        for removed in [
            "with_task_registry(",
            "sync_session_task_registry",
            "HeldTaskRegistry",
            "attach_task_registry(",
        ] {
            assert!(
                !sources.contains(removed),
                "removed direct registry path reappeared: {removed}"
            );
        }
    }

    #[test]
    fn same_session_gets_its_scope_table_back() {
        let held = held("sess-1");
        let original = Arc::clone(&held.scopes);
        let reused = HeldKernelScopes::take_for(Some(held), Some("sess-1"))
            .expect("the session gets its own scope table back");
        assert!(Arc::ptr_eq(&reused, &original));
    }

    #[test]
    fn another_or_missing_session_cannot_reuse_scope_table() {
        assert!(HeldKernelScopes::take_for(Some(held("sess-1")), Some("sess-2")).is_none());
        assert!(HeldKernelScopes::take_for(Some(held("sess-1")), None).is_none());
        assert!(HeldKernelScopes::take_for(None, Some("sess-1")).is_none());
    }
}

//! What a worker keeps across the sessions it builds.
//!
//! A worker outlives any one of its sessions. The ownership lock, MCP stack,
//! and kernel session-scope table therefore return to the worker between turns.

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

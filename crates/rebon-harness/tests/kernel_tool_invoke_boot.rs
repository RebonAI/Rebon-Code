//! Seat membership, on the process kernel's real tool seat.
//!
//! `CORE_TOOL_SEAT` names eleven tools and three of them — `NotebookEdit`,
//! `WebSearch`, `WebFetch` — are registered by plugins on the kernel's
//! `tool-registry` seat, not by the engine's builtin catalog. So the only
//! kernel that can answer "does every seat name resolve" is the one the whole
//! plugin table booted, and that table lives here. The unit tests beside the
//! host keep a kernel of their own, where the engine's catalog is what answers.
//!
//! Moved here with the host itself: `EngineToolInvokeHost` used to look
//! the process kernel up for its upstream context, so this test came for free.
//! It now goes in through `ToolInvoker::invoke`, which is the door the plane
//! itself uses.

use rebon_kernel_seats::kernel_tool_invoke::{EngineToolInvokeHost, CORE_TOOL_SEAT};
use rebon_plugin_protocol::{CallIdentity, Payload};
use rebon_plugin_supervisor::{ToolInvocation, ToolInvoker};

/// One call as a plugin would make it.
fn invocation(tool: &str) -> ToolInvocation {
    ToolInvocation {
        identity: CallIdentity {
            host_epoch: 1,
            plugin_id: "seat-membership-test".to_string(),
            scope_id: "seat-membership-test#1".to_string(),
            scope_generation: 1,
            call_id: format!("call-{tool}"),
        },
        tool: tool.to_string(),
        input: Payload::from(serde_json::json!({})),
    }
}

/// Every seat name must resolve where a plugin reaches it: an empty-input
/// invoke may fail validation, but never UNKNOWN_TOOL (a name that drifted out
/// of the catalog) and never NOT_IN_SEAT.
#[tokio::test]
async fn core_seat_names_all_resolve() {
    let kernel = rebon_harness::kernel_bootstrap::process_kernel();
    let dir = tempfile::tempdir().unwrap();
    let host = EngineToolInvokeHost::new(
        kernel.context(),
        dir.path().to_path_buf(),
        &dir.path().join(".rebon-config"),
        Vec::new(),
    );
    for name in CORE_TOOL_SEAT {
        // The PowerShell tool is registered everywhere but disabled where
        // no pwsh exists, and a disabled tool does not resolve on the seat.
        if *name == "PowerShell" && !rebon_tool::powershell::is_available() {
            continue;
        }
        if let Err(refusal) = host.invoke(invocation(name)).await {
            assert!(
                refusal.code != "[UNKNOWN_TOOL]" && refusal.code != "[NOT_IN_SEAT]",
                "seat name `{name}` failed to resolve: {}",
                refusal.message
            );
        }
    }
}

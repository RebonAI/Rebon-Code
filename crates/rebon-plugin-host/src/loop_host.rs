//! The loop-host seam: what a kernel loop session needs from whoever runs it,
//! without saying where that is.
//!
//! [`KernelLoopBackend`](crate::kernel_loop_backend) drives an agent loop
//! through this pair of traits. One host implements them:
//! [`PlaneLoopHost`](crate::kernel_loop_plane), a realm of plugins on the Node
//! plugin host — one loop, one host process.
//!
//! There used to be two more, and they were the same thing twice: an isolate on
//! a thread here, and that isolate in a spawned `rebon-kernel-host` so a V8-free
//! build could still run loops. The plane serves a V8-free build directly, which
//! left the sidecar as a second way to do what the first way already did.
//!
//! The vendor registry also lives here: `/kernel` needs the full roster,
//! including vendors whose assemblies are not bundled yet.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a per-loop agent needs from its owning session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopAgentSpec {
    /// Owning rebon session id: becomes the event tag and the configured
    /// dsh agent's config id.
    pub session_id: String,
    /// Which loop assembly to compose (an id from [`KNOWN_LOOP_VENDORS`]).
    pub vendor: String,
    /// Model route the loop declares (`AgentOptions.provider/model`); an
    /// adapter for it must be among the user's `kernelPlugins` entries or
    /// every turn ends with the loop's contained NO_ADAPTER error.
    pub provider: String,
    pub model: String,
    /// Workspace root for the loop's tool seat.
    pub workspace_root: PathBuf,
    /// Config dir carrying `kernelPlugins` (adapters, grants).
    pub config_dir: PathBuf,
    /// Optional prompt section for the loop realm's dsh systemPrompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_section: Option<Value>,
}

/// One live loop host, wherever its isolate runs. Method contracts: control
/// calls are queue writes (not model turns), `take_events` yields the stamped
/// `loop:event` / `loop:agent-error` stream once, and `shutdown` is the
/// graceful path while drop is the kill path.
#[async_trait]
pub trait LoopHost: Send + Sync {
    /// The dsh agent/session identity this host drives.
    fn agent_id(&self) -> &str;
    /// Take the session-event stream (once): every dsh session event and
    /// agent error, tag-filtered, `channel: "event" | "error"`.
    fn take_events(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Value>>;
    /// Queue one user message for the next turn and wake the driver.
    async fn followup(&self, text: &str) -> Result<(), String>;
    /// Queue one user message for the next step (mid-turn steering).
    async fn steer(&self, text: &str) -> Result<(), String>;
    /// Cancel the current activity (user-cause).
    async fn cancel(&self) -> Result<(), String>;
    /// The loop agent's own status word (`idle` / `running`).
    async fn status(&self) -> Result<String, String>;
    /// Graceful teardown; the host is unusable afterwards.
    fn shutdown(&self);
    fn has_exited(&self) -> bool;
}

/// Boots loop hosts for a backend. One spawner per backend registration;
/// the choice of implementation is the choice of where the loop runs.
#[async_trait]
pub trait LoopHostSpawner: Send + Sync {
    async fn spawn(&self, spec: LoopAgentSpec) -> Result<Arc<dyn LoopHost>, String>;
}

/// One line for status surfaces.
///
/// There used to be a choice here — an in-process isolate, or the same isolate
/// in a spawned `rebon-kernel-host`. Both hosted the same composition on V8, and
/// both are gone: a loop is a set of plugins on the plane, which is the only
/// runtime left. What the surfaces print is now a fact rather than a selection.
pub fn describe_loop_host() -> &'static str {
    "on the plugin plane (kernel loops run as plugins on Verve, in rebon)"
}

/// One entry in the loop vendor roster.
#[derive(Debug, Clone, Copy)]
pub struct LoopVendor {
    /// Vendor id: the `/kernel <id>` argument, the `kernelPlugins.loops`
    /// key, and the `kernel:<id>` backend name.
    pub id: &'static str,
    /// Human label for lists and switch receipts.
    pub label: &'static str,
    /// Whether this build carries the vendor's JS assembly. A known but
    /// unbundled vendor keeps its registry seat so `/kernel` can say so
    /// loudly instead of "no such vendor".
    pub bundled: bool,
}

/// Every loop vendor rebon knows about. `dsh` is the one whose assembly
/// ships today; `pi` and `opencode` hold their seats until their vendor
/// bundles land (each is its own assembly round: real sources esbuilt into
/// `js/vendor` plus a loop-group composition).
pub const KNOWN_LOOP_VENDORS: &[LoopVendor] = &[
    LoopVendor {
        id: "dsh",
        label: "dsh loop (deepseek-harness)",
        bundled: true,
    },
    LoopVendor {
        id: "pi",
        label: "pi loop",
        bundled: false,
    },
    LoopVendor {
        id: "opencode",
        label: "opencode loop",
        bundled: false,
    },
];

/// Look up a vendor by id (case-insensitive, to match `/agent`'s relaxed
/// switch semantics).
pub fn loop_vendor(id: &str) -> Option<&'static LoopVendor> {
    KNOWN_LOOP_VENDORS
        .iter()
        .find(|vendor| vendor.id.eq_ignore_ascii_case(id.trim()))
}

/// The backend name a vendor registers under (`kernel:<id>`).
pub fn loop_backend_id(vendor: &str) -> String {
    format!("kernel:{vendor}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_has_dsh_bundled_and_seats_for_future_vendors() {
        let dsh = loop_vendor("dsh").expect("dsh seat");
        assert!(dsh.bundled);
        for id in ["pi", "opencode"] {
            let vendor = loop_vendor(id).expect("seat");
            assert!(!vendor.bundled, "{id} must hold an unbundled seat");
        }
        assert!(loop_vendor("nope").is_none());
    }

    #[test]
    fn vendor_lookup_is_case_insensitive_and_trimmed() {
        assert_eq!(loop_vendor(" DSH ").map(|v| v.id), Some("dsh"));
    }
}

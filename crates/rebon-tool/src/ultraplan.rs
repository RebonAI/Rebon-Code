//! The `/ultraplan` run slice of [`ToolContext`](crate::ToolContext).
//!
//! `PlanLedger` lives in the plan-mode plugin; this half exists because
//! `ToolContext` carries the run handle and hands it out through
//! `load_ultraplan_run_state()` and friends, and `ExitPlanMode`,
//! `AskUserQuestion` and `Agent` all read it. Storage only.

use std::sync::{Arc, Mutex};

/// Ultraplan run state that [`ToolContext`](crate::ToolContext) carries in
/// its extension bag.
///
/// `repository` is the file-backed authority; `handle` plus `syncer` /
/// `persister` are the compatibility path for hosts that have not migrated
/// to the repository boundary. Storage only — read and written through the
/// unchanged `load_ultraplan_run_state()` / `capability_context()` and
/// friends.
#[derive(Clone, Default)]
pub struct UltraplanRunContext {
    pub repository: Option<Arc<dyn crate::UltraplanRunRepository>>,
    pub handle: Option<Arc<Mutex<rebon_types::UltraplanRunState>>>,
    pub syncer: Option<crate::UltraplanRunSyncer>,
    pub persister: Option<crate::UltraplanRunPersister>,
    pub capability: Option<rebon_types::CapabilityContext>,
}

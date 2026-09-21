//! The plan-mode contract that outlives the `plan-mode` plugin.
//!
//! `EnterPlanMode`, `ExitPlanMode` and `PlanLedger` themselves live in the
//! plugin, which can be switched off. What stays here is what the rest of
//! the tree names them by and talks to them through: the three canonical
//! tool names, and the approval flag a permission broker stamps on a
//! [`ToolContext`](crate::ToolContext) before `ExitPlanMode` runs.
//!
//! The flag cannot live with the tool: the run loop's permission broker and
//! this crate's own `AutoApprovePermissionBroker` wrapper both write it, and
//! neither may depend on a plugin.

/// Canonical tool name for the plan-mode entry tool.
pub const ENTER_PLAN_MODE_TOOL_NAME: &str = "EnterPlanMode";

/// Canonical tool name for the plan-submission tool.
pub const EXIT_PLAN_MODE_TOOL_NAME: &str = "ExitPlanMode";

/// Canonical tool name for the `/ultraplan` requirement ledger tool.
pub const PLAN_LEDGER_TOOL_NAME: &str = "PlanLedger";

/// Plan-mode state that [`ToolContext`](crate::ToolContext) carries in its
/// extension bag.
///
/// Set only on the context clone handed to `ExitPlanMode` after an
/// interactive permission broker records an explicit approval option.
/// Storage only — read through the unchanged `exit_plan_mode_approved()`
/// accessor.
#[derive(Clone, Copy, Default)]
pub struct PlanModeContext {
    pub exit_approved: bool,
}

//! `plan-mode`: the feature plugin that puts the plan-mode tool pair and
//! the `/ultraplan` requirement ledger on the process tool seat.
//!
//! Three tools around one idea — the session pauses, designs, and only then
//! implements:
//!
//! - `EnterPlanMode` asks the user to switch the session into plan mode;
//! - `ExitPlanMode` submits the finished plan for approval and names the
//!   mode the session comes out in;
//! - `PlanLedger` maintains the versioned requirement ledger a `/ultraplan`
//!   run plans against.
//!
//! **What moved and what did not.** The tools came here whole; everything
//! the rest of the tree reads without them stayed behind:
//!
//! | stayed | where | why |
//! |---|---|---|
//! | the three tool names | [`rebon_tool::plan_mode`] | the TUI, the engine's exposure table and the renderers all name these tools; none of them may depend on a plugin |
//! | `PlanModeContext` (`exit_plan_mode_approved`) | [`rebon_tool::plan_mode`] | `ToolContext` carries the approval flag, and both permission brokers — `rebon-core`'s and `rebon-tool`'s own auto-approve wrapper — stamp it |
//! | `UltraplanRunContext` (the run handle) | [`rebon_tool::ultraplan`] | `ToolContext` hands the run state to `AskUserQuestion` and `Agent` as well, and the next cut moves the `/ultraplan` workflow itself |
//! | the plan-mode session state | [`rebon_session_state`] | the permission mode and its transition flags live on the session record, which the TUI's mode cycle writes without going through a tool; see the note on [`PLUGIN`] |
//!
//! The three reminder attachments came here with the tools — see
//! [`attachments`] — and reach a turn through the kernel's
//! `attachment-producers` seat rather than through the engine's own poller.
//!
//! Turning the plugin off takes all three off the seat: the model can no
//! longer ask to enter plan mode, submit a plan, or touch the ledger, and the
//! reminders stop with them. A session already in plan mode stays in it — the
//! mode is a property of the session record, and the TUI's mode cycle still
//! reaches it — but nothing re-tells the model it is planning.
//!
//! **Session scopes.** All three tools read what they need off
//! `ToolContext` at call time, so they are process-wide registrations and
//! go on the process seat. The plan-mode session state, when it moves,
//! lands on the host's session scope
//! ([`rebon_core::session_scope`](rebon_core::session_scope)) — this
//! plugin used to fork under each session to provide a marker of its own,
//! which is now one seat the host provides for every session.

use std::sync::Arc;

use rebon_core::attachment_seat::{AttachmentSeatService, Order, ATTACHMENT_SEAT_SERVICE};
use rebon_core::permission_seat::{PermissionRuleSeatService, PERMISSION_RULE_SEAT_SERVICE};
use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};

pub mod attachments;
pub mod enter_plan_mode;
pub mod exit_plan_mode;
pub mod permission_rule;
pub mod plan_ledger;

pub use attachments::{PlanModeAttachmentPoller, PlanModeAttachmentProducer};
pub use enter_plan_mode::EnterPlanModeTool;
pub use exit_plan_mode::ExitPlanModeTool;
pub use permission_rule::{
    exit_plan_mode_selection, ExitPlanModeSelection, PlanModePermissionRule,
};
pub use plan_ledger::PlanLedgerTool;

pub use rebon_tool::plan_mode::{
    ENTER_PLAN_MODE_TOOL_NAME, EXIT_PLAN_MODE_TOOL_NAME, PLAN_LEDGER_TOOL_NAME,
};

/// Stable id: the config key `plugins.plan-mode.enabled`.
pub const PLUGIN_ID: &str = "plan-mode";

const PROVIDER_ID: &str = "plan-mode";

/// The three tools, in registration order.
///
/// Public so a test that needs the whole builtin catalogue on a bare
/// [`rebon_core::Engine`] can register them without standing up a kernel.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    vec![
        Arc::new(EnterPlanModeTool) as Arc<dyn rebon_tool::Tool>,
        Arc::new(ExitPlanModeTool),
        Arc::new(PlanLedgerTool),
    ]
}

/// Every tool name this plugin puts on the seat, canonical spelling.
pub const TOOL_NAMES: &[&str] = &[
    ENTER_PLAN_MODE_TOOL_NAME,
    EXIT_PLAN_MODE_TOOL_NAME,
    PLAN_LEDGER_TOOL_NAME,
];

#[derive(Default)]
pub struct PlanModePlugin;

impl Plugin for PlanModePlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).inject(&[
            TOOL_SEAT_SERVICE,
            ATTACHMENT_SEAT_SERVICE,
            PERMISSION_RULE_SEAT_SERVICE,
        ])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        seat.register_tools(ctx, PROVIDER_ID, Priority::Feature, tools())?;

        // The three reminders. [`Order::Transition`] is the rung they held
        // inside the engine's fixed producer order: first, ahead of everything
        // that describes the session's current state.
        let attachments = ctx.require::<AttachmentSeatService>()?;
        attachments.register(
            ctx,
            PROVIDER_ID,
            Order::Transition,
            Arc::new(PlanModeAttachmentProducer),
        )?;

        // What the permission layer must not decide without the user. The
        // engine used to know these two tools by name; it asks the seat now.
        let rules = ctx.require::<PermissionRuleSeatService>()?;
        rules.register(ctx, PROVIDER_ID, Arc::new(PlanModePermissionRule))?;

        Ok(())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(PlanModePlugin::default()))
}

/// This crate's one export to the binary's plugin table.
///
/// Default-enabled, and the switch only reaches the three tools. Plan mode
/// as a *session state* is not this plugin's to turn off: the permission
/// mode lives on the ACP session record, the TUI's Shift+Tab cycle writes
/// it directly, and `rebon-core`'s attachment poller reads it to emit the
/// plan-mode reminders. Moving that state here needs a seam the engine does
/// not have yet.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Plan mode and the /ultraplan ledger (EnterPlanMode, ExitPlanMode, PlanLedger)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_core::attachment_seat::AttachmentSeat;
    use rebon_core::permission_seat::PermissionRuleSeat;
    use rebon_core::tool_seat::ToolSeat;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_tool::{Tool, ToolResolver};

    /// Stands in for the real `core-tools` seat plugin, which this crate does
    /// not depend on. All this plugin needs is the two root seats.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[
                TOOL_SEAT_SERVICE,
                ATTACHMENT_SEAT_SERVICE,
                PERMISSION_RULE_SEAT_SERVICE,
            ])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<AttachmentSeatService>(AttachmentSeat::new())?;
            ctx.provide::<PermissionRuleSeatService>(PermissionRuleSeat::new())?;
            ctx.provide::<ToolSeatService>(ToolSeat::new())
        }
    }

    fn make_seat(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        Ok(Box::new(SeatPlugin))
    }

    static DEFS: &[PluginDef] = &[
        PluginDef {
            id: "test-seat",
            title: "Test seat",
            kind: PluginKind::Core,
            default_enabled: true,
            factory: make_seat,
        },
        PLUGIN,
    ];

    fn boot() -> (Arc<Kernel>, Arc<PluginRegistry>) {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        (kernel, registry)
    }

    fn seat(kernel: &Kernel) -> Arc<ToolSeat> {
        kernel
            .context()
            .get::<ToolSeatService>()
            .expect("the seat is on the root")
    }

    #[test]
    fn the_switch_takes_the_tools_off_the_seat_and_puts_them_back() {
        let (kernel, registry) = boot();
        let seat = seat(&kernel);

        assert_eq!(TOOL_NAMES.len(), 3);
        for name in TOOL_NAMES {
            assert!(
                seat.resolve(name, None).unwrap().is_some(),
                "{name} resolves while plan-mode is loaded"
            );
        }

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("plan-mode is a feature plugin");
        for name in TOOL_NAMES {
            assert!(
                seat.resolve(name, None).unwrap().is_none(),
                "disabling the plugin takes {name} off the seat"
            );
        }

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        for name in TOOL_NAMES {
            assert!(
                seat.resolve(name, None).unwrap().is_some(),
                "{name} is back on the seat"
            );
        }
    }

    /// The reminders go off and on with the tools. A session left in plan
    /// mode keeps its mode — that is the record's — but nothing re-tells the
    /// model it is planning while the plugin is off.
    #[test]
    fn the_switch_takes_the_reminders_off_the_attachment_seat_and_puts_them_back() {
        let (kernel, registry) = boot();
        let attachments = kernel
            .context()
            .get::<AttachmentSeatService>()
            .expect("the attachment seat is on the root");

        assert_eq!(attachments.provider_ids(), vec![PROVIDER_ID.to_string()]);

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("plan-mode is a feature plugin");
        assert!(attachments.provider_ids().is_empty());

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert_eq!(attachments.provider_ids(), vec![PROVIDER_ID.to_string()]);
    }

    /// The names the rest of the tree spells these tools by stayed in
    /// `rebon-tool`; the tools themselves must keep answering to them.
    #[test]
    fn the_tools_answer_to_the_shared_names() {
        assert_eq!(EnterPlanModeTool.id().as_str(), ENTER_PLAN_MODE_TOOL_NAME);
        assert_eq!(ExitPlanModeTool.id().as_str(), EXIT_PLAN_MODE_TOOL_NAME);
        assert_eq!(PlanLedgerTool.id().as_str(), PLAN_LEDGER_TOOL_NAME);
        assert_eq!(
            TOOL_NAMES,
            &["EnterPlanMode", "ExitPlanMode", "PlanLedger"],
            "the wire names are a compatibility surface"
        );
    }
}

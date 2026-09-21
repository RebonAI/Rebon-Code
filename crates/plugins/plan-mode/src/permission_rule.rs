//! What plan mode asks of the permission layer.
//!
//! Three claims, all about the same thing: submitting a plan is a tool call
//! whose **entire output is a user decision**, so a mode that resolves the
//! prompt on the user's behalf does not save them a click — it deletes the
//! decision.
//!
//! | claim | why |
//! |---|---|
//! | `ExitPlanMode` must reach a human in every prompting mode | a classifier looking at it sees a harmless call — it only submits text — and allows it, which skips the approval that is the one gate on the plan |
//! | an `/ultraplan` `ExitPlanMode` must reach a human even under `bypassPermissions` | approving the submitted plan is that workflow's single hard gate. The model plans freely up to it, so a mode that ran it unprompted would delete the checkpoint rather than skip a confirmation |
//! | the four `yes_*` options mean a mode and a context reset | they are plan mode's own options, offered by [`ExitPlanModeTool`](crate::ExitPlanModeTool). What "yes, run with auto mode" *does* is plan mode's to say |
//!
//! **`EnterPlanMode` is deliberately not claimed.** Starting to plan is not a
//! decision that needs signing off: the tool takes no input, only reads, and
//! the mode it asks for is the *more* restrictive one. A mode the user chose
//! so that routine calls stop interrupting them should let it through, and
//! `auto` does. It was claimed here for a while, after a session was seen
//! planning in the mode it started in — but the cause of that was the
//! permission gate reading a cell no tool-driven mode change ever wrote, and
//! the fix for it is `ServerState::attach_permission_mode_cell`, which makes
//! the session record the one writer. The mode change lands whether or not a
//! prompt was shown, so forcing the prompt only put back a confirmation
//! `auto` had promised to handle.
//!
//! **Not covered, deliberately.** `bypassPermissions` opted out of gates by
//! name, and the `ExitPlanMode` claim does not chase it there — only the
//! ultraplan one does, and only because a workflow checkpoint is a different
//! thing from a confirmation. `dontAsk` refuses everything that would prompt,
//! `ExitPlanMode` included: an ultraplan plan cannot be approved under
//! `dontAsk` at all, which fails closed rather than quietly prompting a
//! surface that promised never to.
//!
//! These lived in `rebon_core::permission` as `matches!(tool_name, ...)`
//! arms and a `match option_id` truth table. The engine now asks the
//! `permission-rules` seat instead, so nothing there knows these two tools by
//! name.

use rebon_core::permission_seat::{DecisionScope, PermissionRule, RejectionNote};
use rebon_tool::ToolContext;
use serde_json::Value;

use crate::EXIT_PLAN_MODE_TOOL_NAME;

/// What one approved `ExitPlanMode` option means: the mode the session leaves
/// plan mode into, and whether the context is cleared on the way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitPlanModeSelection {
    pub permission_mode: &'static str,
    pub clear_context: bool,
}

/// The option ids `ExitPlanMode` offers, and what each one does.
///
/// `None` for anything else — including `reject_once`, which is a rejection
/// and never reaches here.
pub fn exit_plan_mode_selection(option_id: &str) -> Option<ExitPlanModeSelection> {
    match option_id {
        "yes_clear_context_auto" => Some(ExitPlanModeSelection {
            permission_mode: "auto",
            clear_context: true,
        }),
        "yes_auto" => Some(ExitPlanModeSelection {
            permission_mode: "auto",
            clear_context: false,
        }),
        "yes_accept_edits" => Some(ExitPlanModeSelection {
            permission_mode: "acceptEdits",
            clear_context: false,
        }),
        "yes_default" => Some(ExitPlanModeSelection {
            permission_mode: "default",
            clear_context: false,
        }),
        _ => None,
    }
}

/// The label each of plan mode's own options carries in the dialog.
fn exit_plan_mode_option_label(option_id: &str) -> Option<&'static str> {
    match option_id {
        "yes_clear_context_auto" => Some("Yes, clear context and run with auto mode"),
        "yes_auto" => Some("Yes, run with auto mode"),
        "yes_accept_edits" => Some("Yes, auto-accept edits"),
        "yes_default" => Some("Yes, manually approve edits"),
        _ => None,
    }
}

/// Plan mode's entry on the `permission-rules` seat.
pub struct PlanModePermissionRule;

impl PermissionRule for PlanModePermissionRule {
    fn requires_user_decision(
        &self,
        tool_name: &str,
        context: &ToolContext,
    ) -> Option<DecisionScope> {
        // `/ultraplan` submits its finished plan through `ExitPlanMode`, and
        // the user approving that plan is the workflow's single hard gate.
        // That one outranks even "no gates".
        if tool_name == EXIT_PLAN_MODE_TOOL_NAME && context.ultraplan_context().is_some() {
            return Some(DecisionScope::EvenUnderBypass);
        }
        // Submitting a plan is claimed; *starting* to plan is not. Entering
        // plan mode reads nothing, writes nothing and moves the session into
        // a stricter mode, and that move lands whether or not a prompt was
        // shown — so a mode that resolves prompts on the user's behalf is
        // free to resolve this one. See the module docs.
        (tool_name == EXIT_PLAN_MODE_TOOL_NAME).then_some(DecisionScope::WhenPrompting)
    }

    fn option_label(&self, option_id: &str) -> Option<String> {
        exit_plan_mode_option_label(option_id).map(str::to_string)
    }

    fn on_approved(
        &self,
        tool_name: &str,
        option_id: &str,
        input: &mut Value,
        context: &ToolContext,
    ) -> Option<ToolContext> {
        if tool_name != EXIT_PLAN_MODE_TOOL_NAME {
            return None;
        }
        let selection = exit_plan_mode_selection(option_id)?;
        if let Some(object) = input.as_object_mut() {
            object.insert(
                "permissionMode".into(),
                Value::String(selection.permission_mode.into()),
            );
            object.insert("clearContext".into(), Value::Bool(selection.clear_context));
        }
        Some(context.with_exit_plan_mode_approval())
    }

    fn rejection_note(&self, tool_name: &str) -> Option<RejectionNote> {
        (tool_name == EXIT_PLAN_MODE_TOOL_NAME).then(|| RejectionNote {
            guidance: "Stay in plan mode and continue discussing or revising the current plan instead of implementing it.".into(),
            feedback_label: "User feedback".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ENTER_PLAN_MODE_TOOL_NAME;

    fn context() -> ToolContext {
        ToolContext::default()
    }

    /// Submitting a plan reaches a human in every prompting mode, and stops
    /// there: `bypassPermissions` opted out of gates by name and this claim
    /// does not chase it.
    #[test]
    fn submitting_a_plan_must_reach_a_human_while_prompting() {
        let rule = PlanModePermissionRule;
        assert_eq!(
            rule.requires_user_decision(EXIT_PLAN_MODE_TOOL_NAME, &context()),
            Some(DecisionScope::WhenPrompting)
        );
    }

    /// Entering plan mode is not a claim, so no mode is forced to prompt for
    /// it — `auto` resolves it like any other read-only call. It was claimed
    /// for a while as a fix for a session that kept planning in the mode it
    /// started in; the cause of that was a permission-mode cell no
    /// tool-driven move wrote, and it is fixed where the mode is stored.
    #[test]
    fn starting_to_plan_is_not_a_claim() {
        let rule = PlanModePermissionRule;
        assert_eq!(
            rule.requires_user_decision(ENTER_PLAN_MODE_TOOL_NAME, &context()),
            None,
            "auto mode must let EnterPlanMode through instead of prompting"
        );
    }

    /// The one gate that outranks `bypassPermissions`. Untouched by the
    /// `EnterPlanMode` carve-out: it is keyed on the tool *and* an active
    /// `/ultraplan` run, and `EnterPlanMode` never reaches it.
    #[test]
    fn an_ultraplan_submission_outranks_bypass() {
        use rebon_types::{ExecutionPolicy, PolicyMode, UltraplanContext};

        let rule = PlanModePermissionRule;
        let context = ToolContext::default().with_execution_policy(ExecutionPolicy::ultraplan(
            UltraplanContext::planning_turn("run", "plan", PolicyMode::Enforce),
        ));
        assert_eq!(
            rule.requires_user_decision(EXIT_PLAN_MODE_TOOL_NAME, &context),
            Some(DecisionScope::EvenUnderBypass)
        );
        assert_eq!(
            rule.requires_user_decision(ENTER_PLAN_MODE_TOOL_NAME, &context),
            None,
            "the ultraplan gate is on the submission, not on starting to plan"
        );
    }

    /// Nothing else is plan mode's business — including tools whose names
    /// merely look like the pair's.
    #[test]
    fn no_other_tool_is_claimed() {
        let rule = PlanModePermissionRule;
        for tool in [
            "Read",
            "Write",
            "Bash",
            "Agent",
            "PlanLedger",
            "EnterPlanModeX",
            "MyExitPlanMode",
        ] {
            assert_eq!(
                rule.requires_user_decision(tool, &context()),
                None,
                "{tool}"
            );
        }
    }

    #[test]
    fn each_yes_option_names_a_mode_and_a_context_reset() {
        for (option_id, permission_mode, clear_context) in [
            ("yes_clear_context_auto", "auto", true),
            ("yes_auto", "auto", false),
            ("yes_accept_edits", "acceptEdits", false),
            ("yes_default", "default", false),
        ] {
            let selection = exit_plan_mode_selection(option_id).expect(option_id);
            assert_eq!(selection.permission_mode, permission_mode, "{option_id}");
            assert_eq!(selection.clear_context, clear_context, "{option_id}");
        }
        assert!(exit_plan_mode_selection("reject_once").is_none());
        assert!(exit_plan_mode_selection("allow_once").is_none());
    }

    #[test]
    fn an_approved_option_writes_the_mode_and_marks_the_context() {
        let rule = PlanModePermissionRule;
        let mut input = serde_json::json!({"plan": "the plan"});
        let approved = rule
            .on_approved(
                EXIT_PLAN_MODE_TOOL_NAME,
                "yes_clear_context_auto",
                &mut input,
                &context(),
            )
            .expect("the option is plan mode's");

        assert_eq!(input["permissionMode"], "auto");
        assert_eq!(input["clearContext"], Value::Bool(true));
        assert!(approved.exit_plan_mode_approved());
    }

    /// A rejection, or another tool's option, leaves the call alone.
    #[test]
    fn an_unclaimed_option_changes_nothing() {
        let rule = PlanModePermissionRule;
        for (tool, option) in [
            (EXIT_PLAN_MODE_TOOL_NAME, "reject_once"),
            (EXIT_PLAN_MODE_TOOL_NAME, "allow_once"),
            (ENTER_PLAN_MODE_TOOL_NAME, "yes_auto"),
            ("Bash", "yes_auto"),
        ] {
            let mut input = serde_json::json!({"plan": "the plan"});
            assert!(
                rule.on_approved(tool, option, &mut input, &context())
                    .is_none(),
                "{tool}/{option}"
            );
            assert_eq!(input, serde_json::json!({"plan": "the plan"}));
        }
    }

    #[test]
    fn the_labels_are_plan_modes_and_only_plan_modes() {
        let rule = PlanModePermissionRule;
        assert_eq!(
            rule.option_label("yes_clear_context_auto").as_deref(),
            Some("Yes, clear context and run with auto mode")
        );
        assert_eq!(
            rule.option_label("yes_auto").as_deref(),
            Some("Yes, run with auto mode")
        );
        assert_eq!(
            rule.option_label("yes_accept_edits").as_deref(),
            Some("Yes, auto-accept edits")
        );
        assert_eq!(
            rule.option_label("yes_default").as_deref(),
            Some("Yes, manually approve edits")
        );
        assert_eq!(rule.option_label("allow_once"), None);
        assert_eq!(rule.option_label("reject_once"), None);
    }

    /// A turned-down plan sends the model back to discussing it, not to
    /// retrying the tool.
    #[test]
    fn rejecting_the_plan_says_to_stay_in_plan_mode() {
        let rule = PlanModePermissionRule;
        let note = rule
            .rejection_note(EXIT_PLAN_MODE_TOOL_NAME)
            .expect("ExitPlanMode has a note");
        assert_eq!(
            note.guidance,
            "Stay in plan mode and continue discussing or revising the current plan instead of implementing it."
        );
        assert_eq!(note.feedback_label, "User feedback");

        assert_eq!(rule.rejection_note(ENTER_PLAN_MODE_TOOL_NAME), None);
        assert_eq!(rule.rejection_note("Bash"), None);
    }
}

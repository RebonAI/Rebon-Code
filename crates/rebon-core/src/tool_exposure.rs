use rebon_tool::{ExecutionSurface, Tool, INVOKE_DEFERRED_TOOL_NAME};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolExposure {
    Eager,
    Deferred,
    Hidden,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BuiltinToolExposurePolicy {
    /// Which room the agent is in. Carried on the policy rather than read
    /// from the global at each decision so the table below is a pure
    /// function of (tool, surface) — testable, and ready for the day a
    /// process hosts one of each.
    surface: ExecutionSurface,
}

impl BuiltinToolExposurePolicy {
    pub(crate) fn default_coding_agent() -> Self {
        Self::for_surface(rebon_tool::execution_surface())
    }

    pub(crate) fn for_surface(surface: ExecutionSurface) -> Self {
        Self { surface }
    }

    pub(crate) fn exposure_for(&self, tool: &dyn Tool) -> ToolExposure {
        if !tool.is_enabled() {
            return ToolExposure::Hidden;
        }

        let id = tool.id();
        let name = id.as_str();
        if self.surface.is_unattended() && unattended_dead_end(name) {
            return ToolExposure::Hidden;
        }

        match name {
            name if name == INVOKE_DEFERRED_TOOL_NAME => ToolExposure::Eager,
            "ToolSearch" | "Read" | "Glob" | "Grep" | "Edit" | "Write" | "MultiEdit" | "Bash"
            | "ShellOutput" | "ShellStop" | "Skill" | "Agent" | "TaskCreate" | "TaskGet"
            | "TaskList" | "TaskUpdate" | "AskUserQuestion" | "EscalateQuestion"
            | "ResolveEscalation" | "EnterPlanMode" | "ExitPlanMode" => ToolExposure::Eager,
            // PlanLedger is the ultraplan ledger and answers
            // "only available while an /ultraplan execution policy is
            // active" to everyone else. It used to reach Eager through
            // the `_` arm below by accident — nothing declared it, and
            // the trait default says do not defer. Named here so that
            // accident is a decision: eager where a plan run can start,
            // hidden where one cannot (see `unattended_dead_end`).
            "PlanLedger" => ToolExposure::Eager,

            // NotebookEdit only applies to .ipynb files, which most
            // sessions never touch — it stays discoverable via ToolSearch
            // rather than costing every session its schema.
            // Profiles are a low-frequency decision about the user's own
            // setup. The model reaches them through ToolSearch rather than
            // carrying two schemas in every session's prompt.
            name if name == rebon_tool::NOTEBOOK_EDIT_TOOL_NAME => ToolExposure::Deferred,
            "Workflow" | "StructuredOutput" | "ProfileSwitch" | "ProfileSave" => {
                ToolExposure::Deferred
            }

            // PowerShell is eager wherever it is a shell the model is expected
            // to reach for: whenever the user selected it (or selected both),
            // and on Windows, where it is the native shell. Elsewhere under
            // `auto` it stays registered but discoverable through ToolSearch,
            // so a Linux session does not carry its schema for nothing.
            "PowerShell" => {
                let selected = matches!(
                    rebon_tool::shell_tool_preference(),
                    rebon_tool::ShellToolPreference::PowerShell
                        | rebon_tool::ShellToolPreference::Both
                );
                if selected || cfg!(windows) {
                    ToolExposure::Eager
                } else {
                    ToolExposure::Deferred
                }
            }

            "Mcp" | "Monitor" | "SaveMemory" | "SendMessage" | "CronCreate" | "CronList"
            | "CronDelete" | "Sleep" | "TeamCreate" | "TeamDelete" | "WebSearch" | "WebFetch" => {
                ToolExposure::Deferred
            }

            name if name.starts_with("mcp__") => {
                if tool.should_defer() {
                    ToolExposure::Deferred
                } else {
                    ToolExposure::Eager
                }
            }

            // Preserve TodoWrite's legacy feature flag and defer switch exactly.
            "TodoWrite" => {
                if tool.should_defer() {
                    ToolExposure::Deferred
                } else {
                    ToolExposure::Eager
                }
            }

            // Compatibility fallback for non-builtin/unknown tools.
            _ => {
                if tool.should_defer() {
                    ToolExposure::Deferred
                } else {
                    ToolExposure::Eager
                }
            }
        }
    }

    pub(crate) fn is_eager(&self, tool: &dyn Tool) -> bool {
        self.exposure_for(tool) == ToolExposure::Eager
    }

    pub(crate) fn is_deferred(&self, tool: &dyn Tool) -> bool {
        self.exposure_for(tool) == ToolExposure::Deferred
    }
}

/// Tools that cannot succeed when nobody is watching, and are therefore
/// not shown at all in [`rebon_tool::ExecutionSurface::Unattended`].
///
/// Every name here was measured failing on Terminal-Bench 4.0 r1,
/// and each fails for its own reason:
///
/// - `EnterPlanMode` / `ExitPlanMode`: nobody can approve a plan, so
///   `ExitPlanMode` falls to the unattended approver, which prefers the
///   `yes_auto` option — and that choice lands in the session record and
///   really does drive permission resolution. 27 of 27 trials escalated
///   themselves into auto mode this way, then lost 22 calls to the
///   classifier's fail-closed rule. Not exposing plan mode severs the
///   chain at step 3, before the escalation exists to be undone.
/// - `AskUserQuestion`: `requires_permission_broker_response` forces an
///   Ask in *every* mode, `bypassPermissions` included, so there is no
///   configuration in which this returns an answer here. 6 calls, 0
///   answers — and the model reached for it precisely *after* being
///   refused, which is the right instinct into a dead end.
/// - `PlanLedger`: refuses unless an ultraplan execution policy is
///   running, and unattended runs do not start one. 4 calls, 0 successes.
/// - `Workflow`: spawns a fleet of sub-agents, and unattended sessions
///   are built with `sub_agents_enabled: false`. 1 call, 0 successes.
///
/// `run_code` belongs to the same list by symptom, but not by mechanism:
/// it fails wherever no Node runtime is reachable, attended or not, so it
/// is gated in `RunCodeTool::is_enabled` instead — the check that already
/// runs first here.
fn unattended_dead_end(name: &str) -> bool {
    matches!(
        name,
        "EnterPlanMode" | "ExitPlanMode" | "AskUserQuestion" | "PlanLedger" | "Workflow"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rebon_tool::ToolContext;
    use rebon_tools_core::{ToolId, ToolInputSchema, ToolResult};
    use serde_json::{json, Value};

    /// A tool that is nothing but its name — which is all the exposure
    /// table ever looks at.
    struct NamedTool(&'static str);

    #[async_trait]
    impl Tool for NamedTool {
        fn id(&self) -> ToolId {
            ToolId::new(self.0)
        }

        fn description(&self) -> &str {
            "stub"
        }

        fn input_schema(&self) -> ToolInputSchema {
            json!({ "type": "object" })
        }

        async fn call(&self, _input: Value, _context: &ToolContext) -> ToolResult<Value> {
            Ok(Value::Null)
        }
    }

    fn exposure(surface: ExecutionSurface, name: &'static str) -> ToolExposure {
        BuiltinToolExposurePolicy::for_surface(surface).exposure_for(&NamedTool(name))
    }

    /// The 13 calls that could not have succeeded, plus
    /// the plan-mode pair whose approval nobody was there to give.
    #[test]
    fn unattended_hides_the_tools_that_have_no_one_to_answer_them() {
        for name in [
            "EnterPlanMode",
            "ExitPlanMode",
            "AskUserQuestion",
            "PlanLedger",
            "Workflow",
        ] {
            assert_eq!(
                exposure(ExecutionSurface::Unattended, name),
                ToolExposure::Hidden,
                "{name} is a dead end when nobody is watching"
            );
        }
    }

    /// The same names are how an attended session works, so hiding them
    /// there would be the worse mistake.
    #[test]
    fn interactive_keeps_every_one_of_them() {
        for name in [
            "EnterPlanMode",
            "ExitPlanMode",
            "AskUserQuestion",
            "PlanLedger",
        ] {
            assert_eq!(
                exposure(ExecutionSurface::Interactive, name),
                ToolExposure::Eager,
                "{name} is how an attended session asks"
            );
        }
        assert_eq!(
            exposure(ExecutionSurface::Interactive, "Workflow"),
            ToolExposure::Deferred
        );
    }

    /// Hiding is narrow: the surface changes nothing for the tools that
    /// work the same in an empty room.
    #[test]
    fn the_working_tools_are_untouched_by_the_surface() {
        for name in ["Bash", "Read", "Edit", "ToolSearch", "Skill"] {
            assert_eq!(
                exposure(ExecutionSurface::Unattended, name),
                exposure(ExecutionSurface::Interactive, name),
                "{name} does not depend on anyone being there"
            );
        }
        assert_eq!(
            exposure(ExecutionSurface::Unattended, "Bash"),
            ToolExposure::Eager
        );
        assert_eq!(
            exposure(ExecutionSurface::Unattended, "WebSearch"),
            ToolExposure::Deferred
        );
    }

    /// `PlanLedger` used to land on Eager through the unknown-tool arm,
    /// which meant nothing had decided it. Now it is named — and the
    /// unknown-tool arm still answers for names nobody has heard of.
    #[test]
    fn an_unknown_tool_still_falls_through_to_eager() {
        assert_eq!(
            exposure(ExecutionSurface::Interactive, "SomeFuturePluginTool"),
            ToolExposure::Eager
        );
        assert_eq!(
            exposure(ExecutionSurface::Unattended, "SomeFuturePluginTool"),
            ToolExposure::Eager
        );
    }
}

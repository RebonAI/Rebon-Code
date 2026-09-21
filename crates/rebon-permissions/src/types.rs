//! Core owned data contracts shared by the `rebon-permissions` modules.
//!
//! Implements:
//! * permission modes, rule/update shapes, decision reasons, and
//!   permission-result unions.
//! * the `ToolUseConfirm`/prompt-facing shape, reduced here to the pure
//!   data the permission dialog needs.

use core::fmt;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PermissionMode {
    AcceptEdits,
    BypassPermissions,
    Default,
    DontAsk,
    Plan,
    Auto,
    Bubble,
}

impl PermissionMode {
    pub fn as_wire(self) -> &'static str {
        match self {
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::BypassPermissions => "bypassPermissions",
            PermissionMode::Default => "default",
            PermissionMode::DontAsk => "dontAsk",
            PermissionMode::Plan => "plan",
            PermissionMode::Auto => "auto",
            PermissionMode::Bubble => "bubble",
        }
    }

    /// Parse a wire value, falling back to `Default` for anything unknown.
    ///
    /// This is the only parser: it lives next to [`Self::as_wire`] so the two cannot drift.
    /// A caller that needs a typo rejected rather than lowered to `Default`
    /// matches the input against its known spellings first; this parser is
    /// the lenient fallback, not the validation gate.
    ///
    /// `bubble` deliberately parses to `Default`: it is a type-level variant
    /// that the runtime set never contains, and callers present it externally
    /// as `default`.
    pub fn from_wire(value: &str) -> Self {
        match value {
            "acceptEdits" => PermissionMode::AcceptEdits,
            "bypassPermissions" => PermissionMode::BypassPermissions,
            "default" => PermissionMode::Default,
            "dontAsk" => PermissionMode::DontAsk,
            "plan" => PermissionMode::Plan,
            "auto" => PermissionMode::Auto,
            _ => PermissionMode::Default,
        }
    }

    /// Modes that may apply to one live session but must never become a shared default.
    pub fn is_session_scoped(self) -> bool {
        matches!(
            self,
            PermissionMode::Plan | PermissionMode::BypassPermissions
        )
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// Allow / deny / ask, defined once for the whole tree.
///
/// The rule engine that produces a behavior and the tool pipeline that
/// consumes it used to hold separate copies of this enum with the same three
/// variants and the same three wire strings.
pub use rebon_tools_core::PermissionBehavior;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PermissionRuleSource {
    UserSettings,
    ProjectSettings,
    LocalSettings,
    FlagSettings,
    PolicySettings,
    CliArg,
    Command,
    Session,
}

impl PermissionRuleSource {
    pub fn as_wire(self) -> &'static str {
        match self {
            PermissionRuleSource::UserSettings => "userSettings",
            PermissionRuleSource::ProjectSettings => "projectSettings",
            PermissionRuleSource::LocalSettings => "localSettings",
            PermissionRuleSource::FlagSettings => "flagSettings",
            PermissionRuleSource::PolicySettings => "policySettings",
            PermissionRuleSource::CliArg => "cliArg",
            PermissionRuleSource::Command => "command",
            PermissionRuleSource::Session => "session",
        }
    }
}

impl fmt::Display for PermissionRuleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PermissionRuleValue {
    pub tool_name: String,
    pub rule_content: Option<String>,
}

impl PermissionRuleValue {
    pub fn new(tool_name: impl Into<String>, rule_content: Option<impl Into<String>>) -> Self {
        Self {
            tool_name: tool_name.into(),
            rule_content: rule_content.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRule {
    pub source: PermissionRuleSource,
    pub rule_behavior: PermissionBehavior,
    pub rule_value: PermissionRuleValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PermissionUpdateDestination {
    UserSettings,
    ProjectSettings,
    LocalSettings,
    Session,
    CliArg,
}

impl PermissionUpdateDestination {
    pub fn as_wire(self) -> &'static str {
        match self {
            PermissionUpdateDestination::UserSettings => "userSettings",
            PermissionUpdateDestination::ProjectSettings => "projectSettings",
            PermissionUpdateDestination::LocalSettings => "localSettings",
            PermissionUpdateDestination::Session => "session",
            PermissionUpdateDestination::CliArg => "cliArg",
        }
    }
}

impl fmt::Display for PermissionUpdateDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionUpdate {
    AddRules {
        destination: PermissionUpdateDestination,
        rules: Vec<PermissionRuleValue>,
        behavior: PermissionBehavior,
    },
    ReplaceRules {
        destination: PermissionUpdateDestination,
        rules: Vec<PermissionRuleValue>,
        behavior: PermissionBehavior,
    },
    RemoveRules {
        destination: PermissionUpdateDestination,
        rules: Vec<PermissionRuleValue>,
        behavior: PermissionBehavior,
    },
    SetMode {
        destination: PermissionUpdateDestination,
        mode: PermissionMode,
    },
    AddDirectories {
        destination: PermissionUpdateDestination,
        directories: Vec<String>,
    },
    RemoveDirectories {
        destination: PermissionUpdateDestination,
        directories: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileOperationType {
    Read,
    Write,
    Create,
}

impl FileOperationType {
    pub fn as_wire(self) -> &'static str {
        match self {
            FileOperationType::Read => "read",
            FileOperationType::Write => "write",
            FileOperationType::Create => "create",
        }
    }
}

impl fmt::Display for FileOperationType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPermissionContext {
    pub mode: PermissionMode,
    pub additional_working_directories: BTreeSet<String>,
}

impl ToolPermissionContext {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            additional_working_directories: BTreeSet::new(),
        }
    }
}

impl Default for ToolPermissionContext {
    fn default() -> Self {
        Self::new(PermissionMode::Default)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingClassifierCheck {
    pub command: String,
    pub cwd: String,
    pub descriptions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ThemeColor {
    Permission,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionRender {
    Choice,
    Input {
        placeholder: String,
        initial_value: Option<String>,
        allow_empty_submit_to_cancel: bool,
        show_label_with_value: bool,
        label_value_separator: Option<String>,
        reset_cursor_on_update: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption<T> {
    pub label: String,
    pub value: T,
    pub render: OptionRender,
}

impl<T> SelectOption<T> {
    pub fn choice(label: impl Into<String>, value: T) -> Self {
        Self {
            label: label.into(),
            value,
            render: OptionRender::Choice,
        }
    }

    pub fn input(
        label: impl Into<String>,
        value: T,
        placeholder: impl Into<String>,
        initial_value: Option<String>,
        show_label_with_value: bool,
        label_value_separator: Option<String>,
        reset_cursor_on_update: bool,
    ) -> Self {
        Self {
            label: label.into(),
            value,
            render: OptionRender::Input {
                placeholder: placeholder.into(),
                initial_value,
                allow_empty_submit_to_cancel: true,
                show_label_with_value,
                label_value_separator,
                reset_cursor_on_update,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxOverrideReason {
    ExcludedCommand,
    DangerouslyDisableSandbox,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecisionReason {
    Rule {
        rule: PermissionRule,
    },
    Mode {
        mode: PermissionMode,
    },
    SubcommandResults {
        reasons: Vec<(String, PermissionResult)>,
    },
    PermissionPromptTool {
        permission_prompt_tool_name: String,
        tool_result: String,
    },
    Hook {
        hook_name: String,
        hook_source: Option<String>,
        reason: Option<String>,
    },
    AsyncAgent {
        reason: String,
    },
    SandboxOverride {
        reason: SandboxOverrideReason,
    },
    Classifier {
        classifier: String,
        reason: String,
    },
    WorkingDir {
        reason: String,
    },
    SafetyCheck {
        reason: String,
        classifier_approvable: bool,
    },
    Other {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionResult {
    Allow {
        decision_reason: Option<PermissionDecisionReason>,
    },
    Ask {
        message: String,
        decision_reason: Option<PermissionDecisionReason>,
        suggestions: Vec<PermissionUpdate>,
    },
    Deny {
        message: String,
        decision_reason: PermissionDecisionReason,
    },
    Passthrough {
        message: String,
        decision_reason: Option<PermissionDecisionReason>,
        suggestions: Vec<PermissionUpdate>,
    },
}

impl PermissionResult {
    pub fn decision_reason(&self) -> Option<&PermissionDecisionReason> {
        match self {
            PermissionResult::Allow { decision_reason }
            | PermissionResult::Ask {
                decision_reason, ..
            }
            | PermissionResult::Passthrough {
                decision_reason, ..
            } => decision_reason.as_ref(),
            PermissionResult::Deny {
                decision_reason, ..
            } => Some(decision_reason),
        }
    }

    pub fn suggestions(&self) -> &[PermissionUpdate] {
        match self {
            PermissionResult::Ask { suggestions, .. }
            | PermissionResult::Passthrough { suggestions, .. } => suggestions.as_slice(),
            PermissionResult::Allow { .. } | PermissionResult::Deny { .. } => &[],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PermissionSubmitKind {
    Accept,
    Reject,
}

impl PermissionSubmitKind {
    pub fn as_wire(self) -> &'static str {
        match self {
            PermissionSubmitKind::Accept => "accept",
            PermissionSubmitKind::Reject => "reject",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ToolType {
    Tool,
    Command,
    Edit,
    Read,
}

impl ToolType {
    pub fn as_wire(self) -> &'static str {
        match self {
            ToolType::Tool => "tool",
            ToolType::Command => "command",
            ToolType::Edit => "edit",
            ToolType::Read => "read",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PermissionMode;

    #[test]
    fn only_plan_and_bypass_are_session_scoped() {
        for mode in [PermissionMode::Plan, PermissionMode::BypassPermissions] {
            assert!(mode.is_session_scoped(), "mode={mode:?}");
        }
        for mode in [
            PermissionMode::AcceptEdits,
            PermissionMode::Default,
            PermissionMode::DontAsk,
            PermissionMode::Auto,
            PermissionMode::Bubble,
        ] {
            assert!(!mode.is_session_scoped(), "mode={mode:?}");
        }
    }
}

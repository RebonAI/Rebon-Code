//! `WizardState` state machine + per-step validators.
//!
//! Covers the wizard pieces:
//!
//! * Top-level step ordering.
//! * Location picker.
//! * Method picker
//!   (`generate` vs `manual`). Picking `manual` jumps to step 3.
//! * LLM generation step.
//! * agent_type entry +
//!   validation via [`crate::surface::validate::validate_agent_type`].
//! * system_prompt entry.
//! * when_to_use entry.
//! * Tools selection step.
//! * Model picker step.
//! * Color picker step.
//! * Memory scope picker
//!   (only when `is_auto_memory_enabled()`).
//! * Confirmation
//!   wrapper.
//! * Final confirmation +
//!   save (validation re-runs).
//!
//! The Rust implementation models the wizard as a pure reducer:
//!
//! 1. [`AgentWizardData`] is the accumulating draft.
//! 2. [`WizardStep`] is the discriminated union of "which step are
//!    we on", with the index baked in for direct step navigation.
//! 3. [`WizardState`] holds the draft + the current step + the
//!    "auto memory enabled" gate (so the step list shape is stable
//!    for tests).
//! 4. [`WizardEvent`] drives transitions.

use crate::surface::types::{AgentMemoryScope, EffortValue, SettingSource};
use crate::surface::validate::validate_agent_type;

/// The accumulating draft as the user moves through the wizard.
///
/// Holds data collected by wizard updates. Every field is optional because
/// the user fills them in over multiple steps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentWizardData {
    /// Where the agent file will be saved.
    pub location: Option<SettingSource>,
    /// `generate` (LLM) or `manual` creation.
    pub method: Option<CreationMethod>,
    /// Set to `true` when the user picked the `generate` method.
    pub was_generated: bool,
    /// Agent type (kebab-case identifier).
    pub agent_type: Option<String>,
    /// Description shown in pickers.
    pub when_to_use: Option<String>,
    /// System prompt body.
    pub system_prompt: Option<String>,
    /// Allowed tool names. `None` = wizard hasn't visited the tools
    /// step or the user picked "all". `Some(vec![])` = explicitly
    /// none.
    pub tools: Option<Vec<String>>,
    /// Optional model identifier.
    pub model: Option<String>,
    /// Optional color label.
    pub color: Option<String>,
    /// Optional memory scope (or `None` for "no memory").
    pub selected_memory: Option<AgentMemoryScope>,
    /// Optional reasoning-effort hint.
    pub effort: Option<EffortValue>,
}

/// Creation method picked on [`WizardStep::Method`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreationMethod {
    /// Use the LLM to generate the agent body.
    Generate,
    /// Fill the wizard manually.
    Manual,
}

impl CreationMethod {
    /// The string value for this method.
    pub fn as_str(self) -> &'static str {
        match self {
            CreationMethod::Generate => "generate",
            CreationMethod::Manual => "manual",
        }
    }
}

/// Discriminated union of the wizard steps. The index pins the
/// zero-based position in the step list for direct step navigation.
///
/// Step order: `Location, Method, Generate, Type, Prompt, Description,
/// Tools, Model, Color, (Memory if auto-memory), Confirm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    /// Step 0 — location picker.
    Location,
    /// Step 1 — method picker.
    Method,
    /// Step 2 — generate step (only reached when `CreationMethod::Generate`).
    Generate,
    /// Step 3 — agent_type entry.
    Type,
    /// Step 4 — system prompt entry.
    Prompt,
    /// Step 5 — description entry.
    Description,
    /// Step 6 — tool selection.
    Tools,
    /// Step 7 — model picker.
    Model,
    /// Step 8 — color picker.
    Color,
    /// Step 9 — memory scope (only present when auto-memory is on).
    Memory,
    /// Final step — confirm + save.
    Confirm,
}

/// The full step list (with or without `Memory`).
pub fn step_list(auto_memory_enabled: bool) -> Vec<WizardStep> {
    let mut out = vec![
        WizardStep::Location,
        WizardStep::Method,
        WizardStep::Generate,
        WizardStep::Type,
        WizardStep::Prompt,
        WizardStep::Description,
        WizardStep::Tools,
        WizardStep::Model,
        WizardStep::Color,
    ];
    if auto_memory_enabled {
        out.push(WizardStep::Memory);
    }
    out.push(WizardStep::Confirm);
    out
}

/// Wizard reducer state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WizardState {
    /// The accumulating draft.
    pub data: AgentWizardData,
    /// Currently-displayed step.
    pub current: WizardStep,
    /// Whether the auto-memory step is in the list. Captured at
    /// construction so the step list is stable.
    pub auto_memory_enabled: bool,
    /// The most recent error to show inline (per-step).
    pub error: Option<String>,
}

impl WizardState {
    /// Build a fresh wizard state.
    pub fn new(auto_memory_enabled: bool) -> Self {
        WizardState {
            data: AgentWizardData::default(),
            current: WizardStep::Location,
            auto_memory_enabled,
            error: None,
        }
    }

    /// Apply an event. Pure reducer.
    pub fn handle_event(mut self, event: WizardEvent) -> WizardOutcome {
        match event {
            WizardEvent::PickLocation(loc) => {
                self.data.location = Some(loc);
                self.current = self.next_step();
                self.error = None;
                WizardOutcome::Continue(self)
            }
            WizardEvent::PickMethod(method) => {
                self.data.method = Some(method);
                self.data.was_generated = matches!(method, CreationMethod::Generate);
                self.current = match method {
                    CreationMethod::Generate => WizardStep::Generate,
                    CreationMethod::Manual => WizardStep::Type,
                };
                self.error = None;
                WizardOutcome::Continue(self)
            }
            WizardEvent::SubmitGenerated {
                agent_type,
                when_to_use,
                system_prompt,
            } => {
                self.data.agent_type = Some(agent_type);
                self.data.when_to_use = Some(when_to_use);
                self.data.system_prompt = Some(system_prompt);
                self.data.was_generated = true;
                // After generation we jump straight to the Tools step
                // (skipping Type/Prompt/Description).
                self.current = WizardStep::Tools;
                self.error = None;
                WizardOutcome::Continue(self)
            }
            WizardEvent::SubmitType(value) => {
                let trimmed = value.trim();
                if let Some(err) = validate_agent_type(trimmed) {
                    self.error = Some(err.to_string());
                    return WizardOutcome::Continue(self);
                }
                self.data.agent_type = Some(trimmed.to_string());
                self.error = None;
                self.current = WizardStep::Prompt;
                WizardOutcome::Continue(self)
            }
            WizardEvent::SubmitPrompt(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    self.error = Some("System prompt is required".into());
                    return WizardOutcome::Continue(self);
                }
                self.data.system_prompt = Some(trimmed.to_string());
                self.error = None;
                self.current = WizardStep::Description;
                WizardOutcome::Continue(self)
            }
            WizardEvent::SubmitDescription(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    self.error = Some("Description is required".into());
                    return WizardOutcome::Continue(self);
                }
                self.data.when_to_use = Some(trimmed.to_string());
                self.error = None;
                self.current = WizardStep::Tools;
                WizardOutcome::Continue(self)
            }
            WizardEvent::SubmitTools(tools) => {
                self.data.tools = Some(tools);
                self.error = None;
                self.current = WizardStep::Model;
                WizardOutcome::Continue(self)
            }
            WizardEvent::SubmitModel(model) => {
                self.data.model = model;
                self.error = None;
                self.current = WizardStep::Color;
                WizardOutcome::Continue(self)
            }
            WizardEvent::SubmitColor(color) => {
                self.data.color = color;
                self.error = None;
                self.current = if self.auto_memory_enabled {
                    WizardStep::Memory
                } else {
                    WizardStep::Confirm
                };
                WizardOutcome::Continue(self)
            }
            WizardEvent::SubmitMemory(scope) => {
                self.data.selected_memory = scope;
                self.error = None;
                self.current = WizardStep::Confirm;
                WizardOutcome::Continue(self)
            }
            WizardEvent::Confirm => WizardOutcome::Saved(self.data),
            WizardEvent::Cancel => WizardOutcome::Cancelled,
            WizardEvent::Back => {
                self.current = self.prev_step();
                self.error = None;
                WizardOutcome::Continue(self)
            }
            WizardEvent::GoToStep(step) => {
                if self.is_step_visible(step) {
                    self.current = step;
                    self.error = None;
                }
                WizardOutcome::Continue(self)
            }
        }
    }

    fn is_step_visible(&self, step: WizardStep) -> bool {
        if step == WizardStep::Memory && !self.auto_memory_enabled {
            return false;
        }
        true
    }

    fn next_step(&self) -> WizardStep {
        let list = step_list(self.auto_memory_enabled);
        if let Some(idx) = list.iter().position(|s| *s == self.current) {
            if idx + 1 < list.len() {
                return list[idx + 1];
            }
        }
        self.current
    }

    fn prev_step(&self) -> WizardStep {
        let list = step_list(self.auto_memory_enabled);
        if let Some(idx) = list.iter().position(|s| *s == self.current) {
            if idx > 0 {
                return list[idx - 1];
            }
        }
        self.current
    }

    /// Memory step option list.
    /// Returns labels in order — when `location` is
    /// [`SettingSource::UserSettings`], the recommended option is "User scope"; otherwise
    /// "Project scope".
    pub fn memory_options(&self) -> Vec<MemoryOption> {
        let is_user_scope = matches!(self.data.location, Some(SettingSource::UserSettings));
        if is_user_scope {
            vec![
                MemoryOption {
                    label: "User scope (~/.rebon/agent-memory/) (Recommended)",
                    value: Some(AgentMemoryScope::User),
                },
                MemoryOption {
                    label: "None (no persistent memory)",
                    value: None,
                },
                MemoryOption {
                    label: "Project scope (.rebon/agent-memory/)",
                    value: Some(AgentMemoryScope::Project),
                },
            ]
        } else {
            vec![
                MemoryOption {
                    label: "Project scope (.rebon/agent-memory/) (Recommended)",
                    value: Some(AgentMemoryScope::Project),
                },
                MemoryOption {
                    label: "None (no persistent memory)",
                    value: None,
                },
                MemoryOption {
                    label: "User scope (~/.rebon/agent-memory/)",
                    value: Some(AgentMemoryScope::User),
                },
            ]
        }
    }

    /// Location step option list.
    pub fn location_options() -> Vec<LocationOption> {
        vec![
            LocationOption {
                label: "Project (.rebon/agents/)",
                value: SettingSource::ProjectSettings,
            },
            LocationOption {
                label: "Personal (~/.rebon/agents/)",
                value: SettingSource::UserSettings,
            },
        ]
    }

    /// Method step option list.
    pub fn method_options() -> Vec<MethodOption> {
        vec![
            MethodOption {
                label: "Generate with Rebon (recommended)",
                value: CreationMethod::Generate,
            },
            MethodOption {
                label: "Manual configuration",
                value: CreationMethod::Manual,
            },
        ]
    }
}

/// Returned from [`WizardState::handle_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WizardOutcome {
    /// The wizard advanced (or stayed on the current step with an
    /// error). Holds the new state.
    Continue(WizardState),
    /// The user finished the wizard — the consumer should now save
    /// the agent.
    Saved(AgentWizardData),
    /// The user cancelled.
    Cancelled,
}

/// Events the wizard reducer accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WizardEvent {
    /// Pick a location and advance.
    PickLocation(SettingSource),
    /// Pick a method (Generate jumps to the Generate step, Manual
    /// jumps to the Type step).
    PickMethod(CreationMethod),
    /// LLM finished generating; advance to Tools.
    SubmitGenerated {
        /// Generated agent_type.
        agent_type: String,
        /// Generated description.
        when_to_use: String,
        /// Generated system prompt.
        system_prompt: String,
    },
    /// Submit the agent_type field (validated via
    /// [`validate_agent_type`]).
    SubmitType(String),
    /// Submit the system_prompt field (non-empty after trim).
    SubmitPrompt(String),
    /// Submit the description field (non-empty after trim).
    SubmitDescription(String),
    /// Submit the tool selection.
    SubmitTools(Vec<String>),
    /// Submit the model selection (`None` for default).
    SubmitModel(Option<String>),
    /// Submit the color selection (`None` for automatic).
    SubmitColor(Option<String>),
    /// Submit the memory scope (`None` for "no memory").
    SubmitMemory(Option<AgentMemoryScope>),
    /// Final confirm — return [`WizardOutcome::Saved`].
    Confirm,
    /// Cancel — return [`WizardOutcome::Cancelled`].
    Cancel,
    /// Back to the previous step.
    Back,
    /// Jump to a specific step. Ignored if the
    /// step isn't visible (e.g., `Memory` when auto-memory is off).
    GoToStep(WizardStep),
}

/// One row in the location picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocationOption {
    /// Display label.
    pub label: &'static str,
    /// Underlying value.
    pub value: SettingSource,
}

/// One row in the method picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodOption {
    /// Display label.
    pub label: &'static str,
    /// Underlying value.
    pub value: CreationMethod,
}

/// One row in the memory picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryOption {
    /// Display label.
    pub label: &'static str,
    /// Underlying value (`None` for "no memory").
    pub value: Option<AgentMemoryScope>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(state: WizardState, event: WizardEvent) -> WizardState {
        match state.handle_event(event) {
            WizardOutcome::Continue(s) => s,
            other => panic!("expected Continue, got {other:?}"),
        }
    }

    // ---- step list ----

    #[test]
    fn step_list_without_memory() {
        let l = step_list(false);
        assert_eq!(l.len(), 10);
        assert_eq!(l[0], WizardStep::Location);
        assert_eq!(l[9], WizardStep::Confirm);
        assert!(!l.contains(&WizardStep::Memory));
    }

    #[test]
    fn step_list_with_memory() {
        let l = step_list(true);
        assert_eq!(l.len(), 11);
        assert!(l.contains(&WizardStep::Memory));
        assert_eq!(l[10], WizardStep::Confirm);
    }

    // ---- pick location ----

    #[test]
    fn pick_location_advances_to_method() {
        let state = WizardState::new(false);
        let state = run(
            state,
            WizardEvent::PickLocation(SettingSource::UserSettings),
        );
        assert_eq!(state.current, WizardStep::Method);
        assert_eq!(state.data.location, Some(SettingSource::UserSettings));
    }

    // ---- pick method ----

    #[test]
    fn pick_method_generate_goes_to_generate_step() {
        let state = WizardState::new(false);
        let state = run(
            state,
            WizardEvent::PickLocation(SettingSource::UserSettings),
        );
        let state = run(state, WizardEvent::PickMethod(CreationMethod::Generate));
        assert_eq!(state.current, WizardStep::Generate);
        assert!(state.data.was_generated);
    }

    #[test]
    fn pick_method_manual_jumps_to_type_step() {
        let state = WizardState::new(false);
        let state = run(
            state,
            WizardEvent::PickLocation(SettingSource::UserSettings),
        );
        let state = run(state, WizardEvent::PickMethod(CreationMethod::Manual));
        assert_eq!(state.current, WizardStep::Type);
        assert!(!state.data.was_generated);
    }

    // ---- type step validation ----

    #[test]
    fn type_step_rejects_invalid() {
        let mut state = WizardState::new(false);
        state.current = WizardStep::Type;
        let state = run(state, WizardEvent::SubmitType("ab".into()));
        assert_eq!(state.current, WizardStep::Type);
        assert!(state.error.unwrap().contains("at least 3 characters"));
    }

    #[test]
    fn type_step_trims_whitespace() {
        let mut state = WizardState::new(false);
        state.current = WizardStep::Type;
        let state = run(state, WizardEvent::SubmitType("  abc  ".into()));
        assert_eq!(state.data.agent_type, Some("abc".into()));
        assert_eq!(state.current, WizardStep::Prompt);
    }

    #[test]
    fn type_step_advances_on_valid() {
        let mut state = WizardState::new(false);
        state.current = WizardStep::Type;
        let state = run(state, WizardEvent::SubmitType("code-reviewer".into()));
        assert_eq!(state.data.agent_type, Some("code-reviewer".into()));
        assert_eq!(state.current, WizardStep::Prompt);
        assert!(state.error.is_none());
    }

    // ---- prompt step validation ----

    #[test]
    fn prompt_step_rejects_empty() {
        let mut state = WizardState::new(false);
        state.current = WizardStep::Prompt;
        let state = run(state, WizardEvent::SubmitPrompt("    ".into()));
        assert_eq!(state.current, WizardStep::Prompt);
        assert_eq!(state.error.as_deref(), Some("System prompt is required"));
    }

    #[test]
    fn prompt_step_trims_and_advances() {
        let mut state = WizardState::new(false);
        state.current = WizardStep::Prompt;
        let state = run(
            state,
            WizardEvent::SubmitPrompt("  You are a code reviewer  ".into()),
        );
        assert_eq!(
            state.data.system_prompt.as_deref(),
            Some("You are a code reviewer")
        );
        assert_eq!(state.current, WizardStep::Description);
    }

    // ---- description step ----

    #[test]
    fn description_step_rejects_empty() {
        let mut state = WizardState::new(false);
        state.current = WizardStep::Description;
        let state = run(state, WizardEvent::SubmitDescription("".into()));
        assert_eq!(state.error.as_deref(), Some("Description is required"));
    }

    #[test]
    fn description_step_advances() {
        let mut state = WizardState::new(false);
        state.current = WizardStep::Description;
        let state = run(
            state,
            WizardEvent::SubmitDescription("Use this when reviewing".into()),
        );
        assert_eq!(state.current, WizardStep::Tools);
    }

    // ---- tools / model / color flow ----

    #[test]
    fn tools_step_advances_to_model() {
        let mut state = WizardState::new(false);
        state.current = WizardStep::Tools;
        let state = run(state, WizardEvent::SubmitTools(vec!["Read".into()]));
        assert_eq!(state.current, WizardStep::Model);
        assert_eq!(state.data.tools, Some(vec!["Read".into()]));
    }

    #[test]
    fn model_step_advances_to_color() {
        let mut state = WizardState::new(false);
        state.current = WizardStep::Model;
        let state = run(state, WizardEvent::SubmitModel(Some("opus".into())));
        assert_eq!(state.current, WizardStep::Color);
        assert_eq!(state.data.model, Some("opus".into()));
    }

    #[test]
    fn color_step_advances_to_confirm_when_auto_memory_off() {
        let mut state = WizardState::new(false);
        state.current = WizardStep::Color;
        let state = run(state, WizardEvent::SubmitColor(Some("blue".into())));
        assert_eq!(state.current, WizardStep::Confirm);
    }

    #[test]
    fn color_step_advances_to_memory_when_auto_memory_on() {
        let mut state = WizardState::new(true);
        state.current = WizardStep::Color;
        let state = run(state, WizardEvent::SubmitColor(None));
        assert_eq!(state.current, WizardStep::Memory);
    }

    // ---- memory step ----

    #[test]
    fn memory_step_advances_to_confirm() {
        let mut state = WizardState::new(true);
        state.current = WizardStep::Memory;
        let state = run(
            state,
            WizardEvent::SubmitMemory(Some(AgentMemoryScope::User)),
        );
        assert_eq!(state.current, WizardStep::Confirm);
        assert_eq!(state.data.selected_memory, Some(AgentMemoryScope::User));
    }

    #[test]
    fn memory_options_user_scope_recommended_first() {
        let mut state = WizardState::new(true);
        state.data.location = Some(SettingSource::UserSettings);
        let opts = state.memory_options();
        assert!(opts[0].label.contains("User scope"));
        assert!(opts[0].label.contains("Recommended"));
    }

    #[test]
    fn memory_options_project_scope_recommended_first() {
        let mut state = WizardState::new(true);
        state.data.location = Some(SettingSource::ProjectSettings);
        let opts = state.memory_options();
        assert!(opts[0].label.contains("Project scope"));
        assert!(opts[0].label.contains("Recommended"));
    }

    // ---- generate step ----

    #[test]
    fn submit_generated_jumps_to_tools() {
        let mut state = WizardState::new(false);
        state.current = WizardStep::Generate;
        let state = run(
            state,
            WizardEvent::SubmitGenerated {
                agent_type: "test-runner".into(),
                when_to_use: "Use this when running tests".into(),
                system_prompt: "You are a test runner.".into(),
            },
        );
        assert_eq!(state.current, WizardStep::Tools);
        assert_eq!(state.data.agent_type, Some("test-runner".into()));
        assert!(state.data.was_generated);
    }

    // ---- confirm + cancel ----

    #[test]
    fn confirm_returns_saved_outcome() {
        let mut state = WizardState::new(false);
        state.current = WizardStep::Confirm;
        state.data.agent_type = Some("test".into());
        let outcome = state.handle_event(WizardEvent::Confirm);
        assert!(matches!(outcome, WizardOutcome::Saved(_)));
    }

    #[test]
    fn cancel_returns_cancelled() {
        let state = WizardState::new(false);
        let outcome = state.handle_event(WizardEvent::Cancel);
        assert_eq!(outcome, WizardOutcome::Cancelled);
    }

    // ---- back navigation ----

    #[test]
    fn back_from_method_returns_to_location() {
        let mut state = WizardState::new(false);
        state.current = WizardStep::Method;
        let state = run(state, WizardEvent::Back);
        assert_eq!(state.current, WizardStep::Location);
    }

    #[test]
    fn back_at_first_step_no_op() {
        let state = WizardState::new(false);
        let state = run(state, WizardEvent::Back);
        assert_eq!(state.current, WizardStep::Location);
    }

    // ---- go to step ----

    #[test]
    fn go_to_step_advances() {
        let state = WizardState::new(false);
        let state = run(state, WizardEvent::GoToStep(WizardStep::Type));
        assert_eq!(state.current, WizardStep::Type);
    }

    #[test]
    fn go_to_memory_when_disabled_is_no_op() {
        let state = WizardState::new(false);
        let state = run(state, WizardEvent::GoToStep(WizardStep::Memory));
        assert_eq!(state.current, WizardStep::Location);
    }

    // ---- option lists ----

    #[test]
    fn location_options_count() {
        assert_eq!(WizardState::location_options().len(), 2);
    }

    #[test]
    fn location_options_first_is_project() {
        let opts = WizardState::location_options();
        assert_eq!(opts[0].value, SettingSource::ProjectSettings);
    }

    #[test]
    fn method_options_count() {
        assert_eq!(WizardState::method_options().len(), 2);
    }

    #[test]
    fn creation_method_strings() {
        assert_eq!(CreationMethod::Generate.as_str(), "generate");
        assert_eq!(CreationMethod::Manual.as_str(), "manual");
    }

    // ---- end-to-end happy path ----

    #[test]
    fn manual_path_end_to_end() {
        let state = WizardState::new(false);
        let state = run(
            state,
            WizardEvent::PickLocation(SettingSource::UserSettings),
        );
        let state = run(state, WizardEvent::PickMethod(CreationMethod::Manual));
        let state = run(state, WizardEvent::SubmitType("code-reviewer".into()));
        let state = run(
            state,
            WizardEvent::SubmitPrompt("You are a careful code reviewer.".into()),
        );
        let state = run(
            state,
            WizardEvent::SubmitDescription("Use this when reviewing".into()),
        );
        let state = run(state, WizardEvent::SubmitTools(vec![]));
        let state = run(state, WizardEvent::SubmitModel(None));
        let state = run(state, WizardEvent::SubmitColor(None));
        assert_eq!(state.current, WizardStep::Confirm);
        let outcome = state.handle_event(WizardEvent::Confirm);
        assert!(matches!(outcome, WizardOutcome::Saved(_)));
    }

    #[test]
    fn generate_path_end_to_end() {
        let state = WizardState::new(true);
        let state = run(
            state,
            WizardEvent::PickLocation(SettingSource::UserSettings),
        );
        let state = run(state, WizardEvent::PickMethod(CreationMethod::Generate));
        // We're on Generate, then submit results.
        let state = run(
            state,
            WizardEvent::SubmitGenerated {
                agent_type: "test-runner".into(),
                when_to_use: "Use when testing".into(),
                system_prompt: "You are a test runner with expertise.".into(),
            },
        );
        let state = run(state, WizardEvent::SubmitTools(vec![]));
        let state = run(state, WizardEvent::SubmitModel(None));
        let state = run(state, WizardEvent::SubmitColor(None));
        // Auto-memory enabled — extra Memory step.
        let state = run(state, WizardEvent::SubmitMemory(None));
        assert_eq!(state.current, WizardStep::Confirm);
    }
}

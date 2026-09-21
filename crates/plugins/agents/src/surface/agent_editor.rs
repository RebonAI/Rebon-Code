//! Agent-editor reducer ([`AgentEditorState`]).
//!
//! A small state machine over four edit modes (`menu` /
//! `edit-tools` / `edit-color` / `edit-model`) plus a dirty-tracking
//! check on save.
//!
//! This module models:
//!
//! 1. The [`EditMode`] enum.
//! 2. The [`AgentEditorState`] reducer with cursor tracking.
//! 3. A pure [`compute_save_changes`] helper that performs the
//!    per-field "has it changed" checks used on save.

use crate::surface::types::AgentSummary;

/// Which sub-mode the editor is currently in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditMode {
    /// Top-level menu — the user picks which field to edit.
    Menu,
    /// Tool selection sub-mode.
    EditTools,
    /// Color selection sub-mode.
    EditColor,
    /// Model selection sub-mode.
    EditModel,
}

/// One row in the editor menu. Pinned in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MenuItem {
    /// Open the agent .md file in the user's editor.
    OpenInEditor,
    /// Edit the tool selection.
    EditTools,
    /// Edit the model.
    EditModel,
    /// Edit the color.
    EditColor,
    /// Cancel and go back.
    Back,
}

impl MenuItem {
    /// Display label.
    pub fn label(self) -> &'static str {
        match self {
            MenuItem::OpenInEditor => "Open in editor",
            MenuItem::EditTools => "Edit tools",
            MenuItem::EditModel => "Edit model",
            MenuItem::EditColor => "Edit color",
            MenuItem::Back => "Back",
        }
    }
}

/// The fixed menu order.
pub const EDITOR_MENU_ITEMS: &[MenuItem] = &[MenuItem::OpenInEditor, MenuItem::Back];

/// Pending changes to apply on save.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SaveChanges {
    /// New tool list (`None` = unchanged).
    pub tools: Option<Vec<String>>,
    /// New color (`None` = unchanged, `Some(None)` = automatic).
    pub color: Option<Option<String>>,
    /// New model (`None` = unchanged).
    pub model: Option<String>,
}

impl SaveChanges {
    /// True iff at least one field is non-None.
    pub fn has_any(&self) -> bool {
        self.tools.is_some() || self.color.is_some() || self.model.is_some()
    }
}

/// Reducer state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEditorState {
    /// The agent being edited.
    pub agent: AgentSummary,
    /// Currently-active sub-mode.
    pub mode: EditMode,
    /// Cursor position in the menu.
    pub menu_index: usize,
    /// The most recent error to display.
    pub error: Option<String>,
    /// Currently-selected color, kept apart from `agent` so the picker
    /// can preview it without committing.
    pub selected_color: Option<String>,
}

impl AgentEditorState {
    /// Build a fresh state focused on `agent`.
    pub fn new(agent: AgentSummary) -> Self {
        let selected_color = agent.color.clone();
        AgentEditorState {
            agent,
            mode: EditMode::Menu,
            menu_index: 0,
            error: None,
            selected_color,
        }
    }

    /// Apply an event. Pure reducer.
    pub fn handle_event(self, event: AgentEditorEvent) -> EditorOutcome {
        match event {
            AgentEditorEvent::Up => {
                if self.mode != EditMode::Menu {
                    return EditorOutcome::Continue(self);
                }
                let mut s = self;
                s.menu_index = s.menu_index.saturating_sub(1);
                EditorOutcome::Continue(s)
            }
            AgentEditorEvent::Down => {
                if self.mode != EditMode::Menu {
                    return EditorOutcome::Continue(self);
                }
                let mut s = self;
                let max = EDITOR_MENU_ITEMS.len() - 1;
                s.menu_index = (s.menu_index + 1).min(max);
                EditorOutcome::Continue(s)
            }
            AgentEditorEvent::Select => {
                if self.mode != EditMode::Menu {
                    return EditorOutcome::Continue(self);
                }
                let item = EDITOR_MENU_ITEMS[self.menu_index];
                match item {
                    MenuItem::OpenInEditor => EditorOutcome::OpenInEditor(self),
                    MenuItem::EditTools => {
                        let mut s = self;
                        s.mode = EditMode::EditTools;
                        EditorOutcome::Continue(s)
                    }
                    MenuItem::EditModel => {
                        let mut s = self;
                        s.mode = EditMode::EditModel;
                        EditorOutcome::Continue(s)
                    }
                    MenuItem::EditColor => {
                        let mut s = self;
                        s.mode = EditMode::EditColor;
                        EditorOutcome::Continue(s)
                    }
                    MenuItem::Back => EditorOutcome::Back,
                }
            }
            AgentEditorEvent::SubmitTools(tools) => {
                let mut s = self;
                s.mode = EditMode::Menu;
                s.error = None;
                let changes = SaveChanges {
                    tools: Some(tools),
                    color: None,
                    model: None,
                };
                EditorOutcome::Save(s, changes)
            }
            AgentEditorEvent::SubmitColor(color) => {
                let mut s = self;
                s.selected_color = color.clone();
                s.mode = EditMode::Menu;
                s.error = None;
                let changes = SaveChanges {
                    tools: None,
                    color: Some(color),
                    model: None,
                };
                EditorOutcome::Save(s, changes)
            }
            AgentEditorEvent::SubmitModel(model) => {
                let mut s = self;
                s.mode = EditMode::Menu;
                s.error = None;
                let changes = SaveChanges {
                    tools: None,
                    color: None,
                    model,
                };
                EditorOutcome::Save(s, changes)
            }
            AgentEditorEvent::CancelSubMode => {
                let mut s = self;
                s.mode = EditMode::Menu;
                s.error = None;
                EditorOutcome::Continue(s)
            }
            AgentEditorEvent::SetError(msg) => {
                let mut s = self;
                s.error = Some(msg);
                EditorOutcome::Continue(s)
            }
        }
    }
}

/// Events the reducer accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEditorEvent {
    /// Up arrow (menu only).
    Up,
    /// Down arrow (menu only).
    Down,
    /// Enter key.
    Select,
    /// Tool selection complete.
    SubmitTools(Vec<String>),
    /// Color selection complete.
    SubmitColor(Option<String>),
    /// Model selection complete.
    SubmitModel(Option<String>),
    /// Cancel a sub-mode and return to the menu.
    CancelSubMode,
    /// Display an error.
    SetError(String),
}

/// Outcome of an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorOutcome {
    /// State updated.
    Continue(AgentEditorState),
    /// User asked to open the file in their editor.
    OpenInEditor(AgentEditorState),
    /// User asked to save changes.
    Save(AgentEditorState, SaveChanges),
    /// User asked to back out of the editor.
    Back,
}

/// Pure helper: should we actually save? Runs the dirty-check over
/// the pending changes.
pub fn compute_save_needed(agent: &AgentSummary, changes: &SaveChanges) -> bool {
    let tools_changed = changes.tools.is_some();
    let model_changed = changes.model.is_some();
    let color_changed = if let Some(new) = &changes.color {
        new != &agent.color
    } else {
        false
    };
    tools_changed || model_changed || color_changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::types::{AgentSource, SettingSource};

    fn sample() -> AgentSummary {
        let mut a = AgentSummary::minimal(
            "code-reviewer",
            "Use this when reviewing code",
            "You are a code reviewer with experience.",
            AgentSource::Settings(SettingSource::UserSettings),
        );
        a.color = Some("blue".to_string());
        a.model = Some("opus".to_string());
        a
    }

    #[test]
    fn editor_starts_in_menu_mode() {
        let s = AgentEditorState::new(sample());
        assert_eq!(s.mode, EditMode::Menu);
        assert_eq!(s.menu_index, 0);
        assert_eq!(s.selected_color, Some("blue".to_string()));
    }

    #[test]
    fn down_advances_in_menu_only() {
        let s = AgentEditorState::new(sample());
        let s = match s.handle_event(AgentEditorEvent::Down) {
            EditorOutcome::Continue(s) => s,
            _ => panic!(),
        };
        assert_eq!(s.menu_index, 1);
    }

    #[test]
    fn down_saturates() {
        let s = AgentEditorState::new(sample());
        let mut s = s;
        for _ in 0..10 {
            s = match s.handle_event(AgentEditorEvent::Down) {
                EditorOutcome::Continue(s) => s,
                _ => panic!(),
            };
        }
        assert_eq!(s.menu_index, EDITOR_MENU_ITEMS.len() - 1);
    }

    #[test]
    fn up_saturates_at_top() {
        let s = AgentEditorState::new(sample());
        let s = match s.handle_event(AgentEditorEvent::Up) {
            EditorOutcome::Continue(s) => s,
            _ => panic!(),
        };
        assert_eq!(s.menu_index, 0);
    }

    #[test]
    fn select_open_in_editor_returns_outcome() {
        let s = AgentEditorState::new(sample());
        match s.handle_event(AgentEditorEvent::Select) {
            EditorOutcome::OpenInEditor(_) => {}
            other => panic!("expected OpenInEditor, got {other:?}"),
        }
    }

    #[test]
    fn select_back_returns_back() {
        let mut s = AgentEditorState::new(sample());
        s.menu_index = 1; // Back
        match s.handle_event(AgentEditorEvent::Select) {
            EditorOutcome::Back => {}
            _ => panic!(),
        }
    }

    #[test]
    fn submit_tools_returns_save_with_tools() {
        let s = AgentEditorState::new(sample());
        match s.handle_event(AgentEditorEvent::SubmitTools(vec!["FileReadTool".into()])) {
            EditorOutcome::Save(state, changes) => {
                assert_eq!(state.mode, EditMode::Menu);
                assert_eq!(changes.tools, Some(vec!["FileReadTool".into()]));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn submit_color_updates_selected_color() {
        let s = AgentEditorState::new(sample());
        match s.handle_event(AgentEditorEvent::SubmitColor(Some("red".into()))) {
            EditorOutcome::Save(state, changes) => {
                assert_eq!(state.selected_color, Some("red".into()));
                assert_eq!(changes.color, Some(Some("red".into())));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn submit_model_returns_save_with_model() {
        let s = AgentEditorState::new(sample());
        match s.handle_event(AgentEditorEvent::SubmitModel(Some("sonnet".into()))) {
            EditorOutcome::Save(_, changes) => {
                assert_eq!(changes.model, Some("sonnet".into()));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn cancel_submode_returns_to_menu() {
        let s = AgentEditorState::new(sample());
        let mut s = s;
        s.mode = EditMode::EditTools;
        let s = match s.handle_event(AgentEditorEvent::CancelSubMode) {
            EditorOutcome::Continue(s) => s,
            _ => panic!(),
        };
        assert_eq!(s.mode, EditMode::Menu);
    }

    #[test]
    fn set_error_attaches_error() {
        let s = AgentEditorState::new(sample());
        let s = match s.handle_event(AgentEditorEvent::SetError("oops".into())) {
            EditorOutcome::Continue(s) => s,
            _ => panic!(),
        };
        assert_eq!(s.error.as_deref(), Some("oops"));
    }

    // ---- compute_save_needed ----

    #[test]
    fn save_needed_when_tools_changed() {
        let agent = sample();
        let changes = SaveChanges {
            tools: Some(vec!["FileReadTool".into()]),
            color: None,
            model: None,
        };
        assert!(compute_save_needed(&agent, &changes));
    }

    #[test]
    fn save_needed_when_model_changed() {
        let agent = sample();
        let changes = SaveChanges {
            tools: None,
            color: None,
            model: Some("opus".into()),
        };
        assert!(compute_save_needed(&agent, &changes));
    }

    #[test]
    fn save_needed_when_color_actually_differs() {
        let agent = sample();
        let changes = SaveChanges {
            tools: None,
            color: Some(Some("red".into())),
            model: None,
        };
        assert!(compute_save_needed(&agent, &changes));
    }

    #[test]
    fn save_not_needed_when_color_same() {
        let agent = sample();
        let changes = SaveChanges {
            tools: None,
            color: Some(Some("blue".into())),
            model: None,
        };
        assert!(!compute_save_needed(&agent, &changes));
    }

    #[test]
    fn save_not_needed_when_no_changes() {
        let agent = sample();
        let changes = SaveChanges::default();
        assert!(!compute_save_needed(&agent, &changes));
    }

    #[test]
    fn menu_items_are_pinned() {
        assert_eq!(EDITOR_MENU_ITEMS[0], MenuItem::OpenInEditor);
        assert_eq!(EDITOR_MENU_ITEMS[1], MenuItem::Back);
    }

    #[test]
    fn menu_labels_pinned() {
        assert_eq!(MenuItem::EditTools.label(), "Edit tools");
        assert_eq!(MenuItem::EditColor.label(), "Edit color");
    }

    #[test]
    fn save_changes_has_any() {
        let mut c = SaveChanges::default();
        assert!(!c.has_any());
        c.tools = Some(vec![]);
        assert!(c.has_any());
    }
}

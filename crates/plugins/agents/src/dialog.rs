//! The `/agents` panel: browse, create, edit and delete agent definitions.
//!
//! It lives with the agents plugin rather than in the terminal because
//! everything it edits is this plugin's — the agent files, the wizard
//! that writes one, the validation that decides a draft is well formed.
//! Registered on the `ui-registry` seat, so switching the plugin off
//! takes the panel away with the `Agent` tool instead of leaving a menu
//! that edits definitions nothing will run.
//!
//! A [`DialogModel`] described declaratively: a wrapped body of
//! individually styled rows over a two-row hint block pinned to the
//! bottom. That is a [`ViewSpec::Panel`] whose second pane always
//! stacks under the first, which is the only shape that keeps both the
//! wrapping body and the fixed hint block — a list or an outline would
//! lose one or the other.
//!
//! What crosses the boundary is [`PanelRow`]: text this module has
//! already formatted and truncated, plus the [`RowTone`] role it wants
//! painted in. Formatting is the panel's, because it is what the panel
//! knows; turning a role into a colour is the surface's, because it is
//! what the surface knows.

use std::path::Path;

use rebon_dialog::model::{
    DialogAction, DialogKey, DialogModel, DialogOutcome, KeyPress, PanelPane, PanelRow, PanelSplit,
    PanelView, RowTone, TextSpan, ViewSpec,
};
use rebon_ui_seat::ids;
use rebon_width::truncate_to_width;

use crate::agent_files::{format_save_error, load_agents, path_context, RealAgentFs};
use crate::surface::agent_detail::{build_agent_detail, source_label, ToolsDisplay};
use crate::surface::agent_editor::{AgentEditorState, MenuItem, EDITOR_MENU_ITEMS};
use crate::surface::agent_file::{
    actual_agent_file_path, actual_relative_agent_file_path, delete_agent_from_file,
    save_agent_to_file, AgentPathContext,
};
use crate::surface::agents_list::{
    list_title, selectable_in_order, sort_agents, AgentsListEvent, AgentsListState, ListOutcome,
};
use crate::surface::agents_menu::{
    agent_menu_items, fresh_agent, AgentMenuItem, AgentsMenuEvent, AgentsMenuState,
};
use crate::surface::color_picker::{AgentColorName, ColorPickerEvent, ColorPickerState};
use crate::surface::mode_state::ModeState;
use crate::surface::model_selector::{ModelOption, ModelSelectorEvent, ModelSelectorState};
use crate::surface::types::{AgentMemoryScope, AgentSource, AgentSummary, SettingSource};
use crate::surface::validate::{validate_agent, AgentDraft, ResolvedTools};
use crate::surface::wizard::{WizardEvent, WizardOutcome, WizardState, WizardStep};
use crate::tool_buckets::{selector_state, ToolEntry, ToolSelectorState};

/// Stable id, used for action routing and for the surface's native
/// painter table.
pub const DIALOG_ID: &str = ids::dialog::AGENTS;

/// Open the highlighted agent's file in an external editor. One value:
/// the absolute path. The panel stays open.
pub const ACTION_OPEN: &str = ids::action::OPEN;

/// Ask the host to generate an agent definition from a description.
/// One value: the description. The panel stays open and waits for
/// [`AgentsDialogState::apply_generation_result`].
pub const ACTION_GENERATE: &str = ids::action::GENERATE;

const PROJECT_CONFIG_DIR_NAME: &str = rebon_session::config_home::DEFAULT_CONFIG_DIR_NAME;

/// Width to build rows for before the surface has said how wide the
/// panel is. Only a test that never paints sees it.
const FALLBACK_WIDTH: usize = 80;

/// Rows the hint block under the body always takes: one for the key
/// hints, one for a notice when there is one.
const FOOTER_ROWS: u16 = 2;

/// One row in a single role.
///
/// Every line this panel builds carries one role for its whole width —
/// a heading, a dim hint, the highlighted entry — so a row is one run.
/// The role is what crosses the boundary; which colour it becomes is
/// the surface's to decide.
fn row(text: impl Into<String>, tone: RowTone) -> PanelRow {
    PanelRow::one(TextSpan::new(text, tone))
}

/// A body-text row.
fn plain_row(text: impl Into<String>) -> PanelRow {
    row(text, RowTone::Normal)
}

/// What opening the `/agents` panel needs.
///
/// Richer than the seat's positional strings — the model options are
/// three-field records — so it travels as the opaque payload the seat
/// carries for exactly this, and the plugin's dialog factory downcasts
/// it.
/// Encoding it into one string instead would put a second copy of the
/// shape in the front end.
#[derive(Debug, Clone)]
pub struct AgentsDialogInput {
    /// The project the panel lists agents for.
    pub cwd: String,
    /// Every tool name the session offers, for the wizard's tool step
    /// and for marking an agent's tool list unrecognised.
    pub tool_names: Vec<String>,
    /// Which agent to select on open, when the command named one.
    pub initial_agent_type: Option<String>,
    /// Extra Model-step options for the ACP agents this session has
    /// declared.
    pub external_model_options: Vec<ModelOption>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Notice {
    Info(String),
    Error(String),
}

#[derive(Debug, Clone)]
struct CreateWizardHost {
    state: WizardState,
    text_input: String,
    option_index: usize,
    tool_selector: ToolSelectorState,
    tool_cursor: usize,
    model_selector: ModelSelectorState,
    /// The Model step's option list: the built-in tiers plus one
    /// `<agentId>:` entry per declared ACP agent this session can run.
    model_options: Vec<ModelOption>,
    color_picker: ColorPickerState,
    /// Text input for the Generate step's description prompt.
    generate_input: String,
    /// Whether an LLM generation call is currently in flight.
    is_generating: bool,
}

/// Cloneable reducer state for the `/agents` dialog.
#[derive(Debug, Clone)]
pub struct AgentsDialogState {
    pub menu: AgentsMenuState,
    pub list: AgentsListState,
    pub editor: Option<AgentEditorState>,
    wizard: Option<CreateWizardHost>,
    pub action_index: usize,
    pub agents: Vec<AgentSummary>,
    pub tool_names: Vec<String>,
    /// Extra Model-step options for declared ACP agents (`<id>:` specs).
    external_model_options: Vec<ModelOption>,
    path_ctx: AgentPathContext,
    initial_agent_type: Option<String>,
    notice: Option<Notice>,
    /// Columns the surface last painted the body at, from
    /// [`DialogModel::note_viewport`]. Rows are truncated and wrapped
    /// as they are built, so the panel has to know the width before it
    /// answers `view`.
    viewport_cols: usize,
}

/// What one key did, before it is turned into a [`DialogOutcome`].
///
/// Kept as the reducers' own vocabulary rather than folded into the
/// host's: every mode answers in these terms, and `Ignored` says
/// something `DialogOutcome` cannot — the panel did not act, but a
/// modal still swallows the key rather than letting it type into the
/// prompt behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DialogKeyOutcome {
    Consumed,
    Dismiss,
    OpenInEditor {
        path: String,
    },
    /// The Generate step wants to kick off an LLM call. The host should
    /// run it and feed the result back via
    /// [`AgentsDialogState::apply_generation_result`].
    RequestGeneration {
        prompt: String,
    },
    Ignored,
}

impl CreateWizardHost {
    fn new(tool_names: &[String], external_model_options: &[ModelOption]) -> Self {
        let mut model_options = default_agent_model_options();
        model_options.extend_from_slice(external_model_options);
        let mut host = Self {
            state: WizardState::new(true),
            text_input: String::new(),
            option_index: 0,
            tool_selector: selector_state(build_tool_entries(tool_names), None),
            tool_cursor: 0,
            model_selector: ModelSelectorState::new(&model_options, None),
            model_options,
            color_picker: ColorPickerState::new(None),
            generate_input: String::new(),
            is_generating: false,
        };
        host.sync_for_current_step(tool_names);
        host
    }

    fn sync_for_current_step(&mut self, tool_names: &[String]) {
        match self.state.current {
            WizardStep::Location => {
                self.option_index = self
                    .state
                    .data
                    .location
                    .and_then(|value| {
                        WizardState::location_options()
                            .iter()
                            .position(|option| option.value == value)
                    })
                    .unwrap_or(0);
            }
            WizardStep::Method => {
                self.option_index = self
                    .state
                    .data
                    .method
                    .and_then(|value| {
                        WizardState::method_options()
                            .iter()
                            .position(|option| option.value == value)
                    })
                    .unwrap_or(1);
            }
            WizardStep::Type => {
                self.text_input = self.state.data.agent_type.clone().unwrap_or_default();
            }
            WizardStep::Prompt => {
                self.text_input = self.state.data.system_prompt.clone().unwrap_or_default();
            }
            WizardStep::Description => {
                self.text_input = self.state.data.when_to_use.clone().unwrap_or_default();
            }
            WizardStep::Tools => {
                self.tool_selector = selector_state(
                    build_tool_entries(tool_names),
                    self.state.data.tools.as_deref(),
                );
                self.tool_cursor = 0;
            }
            WizardStep::Model => {
                let initial_model = self
                    .state
                    .data
                    .model
                    .clone()
                    .unwrap_or_else(|| "inherit".to_string());
                self.model_selector =
                    ModelSelectorState::new(&self.model_options, Some(initial_model.as_str()));
            }
            WizardStep::Color => {
                self.color_picker =
                    ColorPickerState::new(parse_color_name(self.state.data.color.as_deref()));
            }
            WizardStep::Memory => {
                self.option_index = self
                    .state
                    .memory_options()
                    .iter()
                    .position(|option| option.value == self.state.data.selected_memory)
                    .unwrap_or(0);
            }
            WizardStep::Generate => {
                // Keep generate_input as-is (user may go back and
                // forward); reset generating flag.
                self.is_generating = false;
            }
            WizardStep::Confirm => {}
        }
    }
}

impl AgentsDialogState {
    /// Open the agents dialog rooted at `cwd`.
    pub fn open(cwd: &Path, tool_names: Vec<String>, initial_agent_type: Option<&str>) -> Self {
        Self::open_with_external_model_options(cwd, tool_names, initial_agent_type, Vec::new())
    }

    /// [`Self::open`] with extra Model-step options for the ACP agents
    /// this session has declared.
    pub fn open_with_external_model_options(
        cwd: &Path,
        tool_names: Vec<String>,
        initial_agent_type: Option<&str>,
        external_model_options: Vec<ModelOption>,
    ) -> Self {
        let path_ctx = path_context(cwd);
        let agents = load_agents(&path_ctx);
        let initial_agent_type = initial_agent_type.map(ToOwned::to_owned);
        let list = build_list_state(&agents, initial_agent_type.as_deref());
        Self {
            menu: AgentsMenuState::default_initial(),
            list,
            editor: None,
            wizard: None,
            action_index: 0,
            agents,
            tool_names,
            external_model_options,
            path_ctx,
            initial_agent_type,
            notice: None,
            viewport_cols: FALLBACK_WIDTH,
        }
    }

    /// Refresh agent definitions from disk so external edits appear
    /// while the dialog stays open.
    pub fn refresh(&mut self) {
        let selected = self.selection_key();
        self.agents = load_agents(&self.path_ctx);
        self.list = build_list_state(
            &self.agents,
            selected
                .as_ref()
                .map(|(agent_type, _)| agent_type.as_str())
                .or(self.initial_agent_type.as_deref()),
        );
        if let Some((agent_type, source)) = selected {
            if let Some(agent) = self
                .list
                .selectable
                .iter()
                .find(|item| item.agent_type == agent_type && item.source == source)
            {
                self.list.selected = Some(agent.clone());
            }
        }
        self.menu.mode = sync_mode_with_agents(self.menu.mode.clone(), &self.agents);
        self.editor = self.editor.as_ref().and_then(|editor| {
            fresh_agent(&self.agents, &editor.agent).map(|agent| {
                let mut next = editor.clone();
                next.agent = agent.clone();
                next.selected_color = agent.color.clone();
                next
            })
        });
        if let Some(wizard) = self.wizard.as_mut() {
            wizard.sync_for_current_step(&self.tool_names);
        }
        self.clamp_action_index();
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.notice = Some(Notice::Error(message.into()));
    }

    /// Feed a successful LLM generation result back into the wizard.
    /// Called by the host when the async generation task completes.
    pub fn apply_generation_result(&mut self, agent: crate::surface::generate::GeneratedAgent) {
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        if !wizard.is_generating {
            // User cancelled while the task was in flight.
            return;
        }
        wizard.is_generating = false;
        let outcome = wizard
            .state
            .clone()
            .handle_event(WizardEvent::SubmitGenerated {
                agent_type: agent.identifier,
                when_to_use: agent.when_to_use,
                system_prompt: agent.system_prompt,
            });
        wizard_apply_continue(wizard, outcome, &self.tool_names);
    }

    /// Feed a generation failure back into the wizard.
    pub fn apply_generation_error(&mut self, message: &str) {
        if let Some(wizard) = self.wizard.as_mut() {
            wizard.is_generating = false;
        }
        self.set_error(message);
    }

    /// Existing agent identifiers, used to build the avoid list for
    /// the generation prompt.
    pub fn existing_identifiers(&self) -> Vec<String> {
        self.agents.iter().map(|a| a.agent_type.clone()).collect()
    }

    fn handle_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        match self.menu.mode.clone() {
            ModeState::ListAgents { .. } => self.handle_list_key(key),
            ModeState::AgentMenu { .. } => self.handle_agent_menu_key(key),
            ModeState::ViewAgent { .. } => self.handle_back_only_key(key),
            ModeState::DeleteConfirm { .. } => self.handle_delete_key(key),
            ModeState::EditAgent { .. } => self.handle_edit_key(key),
            ModeState::CreateAgent => self.handle_create_key(key),
            ModeState::MainMenu => self.handle_placeholder_key(key),
        }
    }

    fn handle_list_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        self.initial_agent_type = None;
        match key {
            DialogKey::Escape => DialogKeyOutcome::Dismiss,
            DialogKey::Up => {
                if let ListOutcome::Continue(state) =
                    self.list.clone().handle_event(AgentsListEvent::Up)
                {
                    self.list = state;
                }
                DialogKeyOutcome::Consumed
            }
            DialogKey::Down => {
                if let ListOutcome::Continue(state) =
                    self.list.clone().handle_event(AgentsListEvent::Down)
                {
                    self.list = state;
                }
                DialogKeyOutcome::Consumed
            }
            DialogKey::Char {
                value: 'c' | 'C', ..
            } => {
                self.open_create_wizard();
                DialogKeyOutcome::Consumed
            }
            DialogKey::Enter => {
                match self.list.clone().handle_event(AgentsListEvent::Select) {
                    ListOutcome::Selected(agent) => {
                        self.notice = None;
                        self.menu = self
                            .menu
                            .clone()
                            .handle_event(AgentsMenuEvent::OpenAgentMenu(agent));
                        self.action_index = 0;
                    }
                    ListOutcome::CreateNew => self.open_create_wizard(),
                    ListOutcome::Continue(_) => {}
                }
                DialogKeyOutcome::Consumed
            }
            _ => DialogKeyOutcome::Ignored,
        }
    }

    fn handle_agent_menu_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        let Some(agent) = self.current_agent().cloned() else {
            self.menu = AgentsMenuState::default_initial();
            return DialogKeyOutcome::Consumed;
        };
        let items = agent_menu_items(&agent);
        match key {
            DialogKey::Escape => {
                self.menu = self.menu.clone().handle_event(AgentsMenuEvent::Back);
                self.action_index = 0;
                DialogKeyOutcome::Consumed
            }
            DialogKey::Up => {
                if self.action_index == 0 {
                    self.action_index = items.len().saturating_sub(1);
                } else {
                    self.action_index -= 1;
                }
                DialogKeyOutcome::Consumed
            }
            DialogKey::Down => {
                if !items.is_empty() {
                    self.action_index = (self.action_index + 1) % items.len();
                }
                DialogKeyOutcome::Consumed
            }
            DialogKey::Enter => {
                let item = items
                    .get(self.action_index)
                    .copied()
                    .unwrap_or(AgentMenuItem::Back);
                self.notice = None;
                self.menu = self
                    .menu
                    .clone()
                    .handle_event(AgentsMenuEvent::PickAgentMenuItem(item));
                if matches!(self.menu.mode, ModeState::EditAgent { .. }) {
                    self.editor = Some(AgentEditorState::new(agent));
                } else {
                    self.editor = None;
                }
                self.action_index = 0;
                DialogKeyOutcome::Consumed
            }
            _ => DialogKeyOutcome::Ignored,
        }
    }

    fn handle_back_only_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        match key {
            DialogKey::Escape | DialogKey::Enter => {
                self.menu = self.menu.clone().handle_event(AgentsMenuEvent::Back);
                DialogKeyOutcome::Consumed
            }
            _ => DialogKeyOutcome::Ignored,
        }
    }

    fn handle_delete_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        match key {
            DialogKey::Escape => {
                self.menu = self
                    .menu
                    .clone()
                    .handle_event(AgentsMenuEvent::DeleteCancelled);
                DialogKeyOutcome::Consumed
            }
            DialogKey::Enter => {
                let Some(agent) = self.current_agent().cloned() else {
                    self.menu = AgentsMenuState::default_initial();
                    return DialogKeyOutcome::Consumed;
                };
                let mut fs = RealAgentFs;
                match delete_agent_from_file(&mut fs, &self.path_ctx, &agent) {
                    Ok(path) => {
                        self.notice = Some(Notice::Info(format!("Deleted {path}")));
                        self.menu = self
                            .menu
                            .clone()
                            .handle_event(AgentsMenuEvent::DeleteConfirmed);
                        self.refresh();
                    }
                    Err(err) => {
                        self.set_error(format!("Delete failed: {}", format_save_error(&err)))
                    }
                }
                DialogKeyOutcome::Consumed
            }
            _ => DialogKeyOutcome::Ignored,
        }
    }

    fn handle_edit_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        let Some(editor) = self.editor.as_mut() else {
            if let Some(agent) = self.current_agent().cloned() {
                self.editor = Some(AgentEditorState::new(agent));
            } else {
                self.menu = AgentsMenuState::default_initial();
                return DialogKeyOutcome::Consumed;
            }
            return DialogKeyOutcome::Consumed;
        };

        match key {
            DialogKey::Escape => {
                self.editor = None;
                self.menu = self
                    .menu
                    .clone()
                    .handle_event(AgentsMenuEvent::EditCancelled);
                DialogKeyOutcome::Consumed
            }
            DialogKey::Up => {
                editor.menu_index = editor.menu_index.saturating_sub(1);
                DialogKeyOutcome::Consumed
            }
            DialogKey::Down => {
                editor.menu_index = (editor.menu_index + 1).min(EDITOR_MENU_ITEMS.len() - 1);
                DialogKeyOutcome::Consumed
            }
            DialogKey::Enter => {
                let item = EDITOR_MENU_ITEMS[editor.menu_index];
                match item {
                    MenuItem::OpenInEditor => {
                        match actual_agent_file_path(&self.path_ctx, &editor.agent) {
                            Ok(path) => DialogKeyOutcome::OpenInEditor { path },
                            Err(err) => {
                                self.set_error(format!("Could not resolve file path: {err:?}"));
                                DialogKeyOutcome::Consumed
                            }
                        }
                    }
                    MenuItem::Back => {
                        self.editor = None;
                        self.menu = self
                            .menu
                            .clone()
                            .handle_event(AgentsMenuEvent::EditCancelled);
                        DialogKeyOutcome::Consumed
                    }
                    MenuItem::EditTools | MenuItem::EditModel | MenuItem::EditColor => {
                        unreachable!("structured edit items are not exposed by EDITOR_MENU_ITEMS")
                    }
                }
            }
            _ => DialogKeyOutcome::Ignored,
        }
    }

    fn handle_create_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        if self.wizard.is_none() {
            self.wizard = Some(CreateWizardHost::new(
                &self.tool_names,
                &self.external_model_options,
            ));
        }
        let Some(current_step) = self.wizard.as_ref().map(|wizard| wizard.state.current) else {
            return DialogKeyOutcome::Consumed;
        };

        match current_step {
            WizardStep::Location => self.handle_create_location_key(key),
            WizardStep::Method => self.handle_create_method_key(key),
            WizardStep::Type => self.handle_create_text_key(key, WizardTextKind::Type),
            WizardStep::Prompt => self.handle_create_text_key(key, WizardTextKind::Prompt),
            WizardStep::Description => {
                self.handle_create_text_key(key, WizardTextKind::Description)
            }
            WizardStep::Tools => self.handle_create_tools_key(key),
            WizardStep::Model => self.handle_create_model_key(key),
            WizardStep::Color => self.handle_create_color_key(key),
            WizardStep::Memory => self.handle_create_memory_key(key),
            WizardStep::Confirm => self.handle_create_confirm_key(key),
            WizardStep::Generate => self.handle_create_generate_key(key),
        }
    }

    /// The location step: pick where the new agent's definition is written.
    fn handle_create_location_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        let options = WizardState::location_options();
        match key {
            DialogKey::Escape => {
                self.menu = self
                    .menu
                    .clone()
                    .handle_event(AgentsMenuEvent::CreateCancelled);
                self.wizard = None;
            }
            DialogKey::Up => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.option_index = wizard.option_index.saturating_sub(1);
                }
            }
            DialogKey::Down => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.option_index =
                        (wizard.option_index + 1).min(options.len().saturating_sub(1));
                }
            }
            DialogKey::Enter => {
                if let Some(wizard) = self.wizard.as_mut() {
                    let outcome = wizard.state.clone().handle_event(WizardEvent::PickLocation(
                        options[wizard.option_index].value,
                    ));
                    wizard_apply_continue(wizard, outcome, &self.tool_names);
                }
            }
            _ => {}
        }
        DialogKeyOutcome::Consumed
    }

    /// The method step: pick how the definition's body is authored.
    fn handle_create_method_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        let options = WizardState::method_options();
        match key {
            DialogKey::Escape => wizard_back_or_cancel(self),
            DialogKey::Up => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.option_index = wizard.option_index.saturating_sub(1);
                }
            }
            DialogKey::Down => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.option_index =
                        (wizard.option_index + 1).min(options.len().saturating_sub(1));
                }
            }
            DialogKey::Enter => {
                if let Some(wizard) = self.wizard.as_mut() {
                    let method = options[wizard.option_index].value;
                    let outcome = wizard
                        .state
                        .clone()
                        .handle_event(WizardEvent::PickMethod(method));
                    wizard_apply_continue(wizard, outcome, &self.tool_names);
                }
            }
            _ => {}
        }
        DialogKeyOutcome::Consumed
    }

    /// The three free-text steps, which differ only in the event they submit.
    fn handle_create_text_key(&mut self, key: DialogKey, kind: WizardTextKind) -> DialogKeyOutcome {
        if let Some(wizard) = self.wizard.as_mut() {
            let text = wizard.text_input.clone();
            let submit = match kind {
                WizardTextKind::Type => WizardEvent::SubmitType(text),
                WizardTextKind::Prompt => WizardEvent::SubmitPrompt(text),
                WizardTextKind::Description => WizardEvent::SubmitDescription(text),
            };
            handle_wizard_text_input(key, wizard, submit, &self.tool_names, kind);
        }
        DialogKeyOutcome::Consumed
    }

    /// The tools step: a multi-select over the tool names the host offers.
    fn handle_create_tools_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        match key {
            DialogKey::Escape => wizard_back_or_cancel(self),
            DialogKey::Up => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.tool_cursor = wizard.tool_cursor.saturating_sub(1);
                }
            }
            DialogKey::Down => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.tool_cursor =
                        (wizard.tool_cursor + 1).min(wizard.tool_selector.tools.len());
                }
            }
            DialogKey::Char {
                value: 'a' | 'A', ..
            } => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.tool_selector = wizard.tool_selector.clone().toggle_all();
                }
            }
            DialogKey::Char { value: ' ', .. } => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard_toggle_tool_at_cursor(wizard);
                }
            }
            DialogKey::Enter => {
                if let Some(wizard) = self.wizard.as_mut() {
                    let selected = wizard
                        .tool_selector
                        .confirm()
                        .unwrap_or_else(|| vec!["*".to_string()]);
                    let outcome = wizard
                        .state
                        .clone()
                        .handle_event(WizardEvent::SubmitTools(selected));
                    wizard_apply_continue(wizard, outcome, &self.tool_names);
                }
            }
            _ => {}
        }
        DialogKeyOutcome::Consumed
    }

    /// The model step: pick the agent's model, or inherit the caller's.
    fn handle_create_model_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        match key {
            DialogKey::Escape => wizard_back_or_cancel(self),
            DialogKey::Up => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.model_selector = wizard
                        .model_selector
                        .clone()
                        .handle_event(ModelSelectorEvent::Up)
                }
            }
            DialogKey::Down => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.model_selector = wizard
                        .model_selector
                        .clone()
                        .handle_event(ModelSelectorEvent::Down)
                }
            }
            DialogKey::Enter => {
                if let Some(wizard) = self.wizard.as_mut() {
                    let selected = wizard.model_selector.confirm();
                    let value = selected.and_then(|model| {
                        if model == "inherit" {
                            None
                        } else {
                            Some(model)
                        }
                    });
                    let outcome = wizard
                        .state
                        .clone()
                        .handle_event(WizardEvent::SubmitModel(value));
                    wizard_apply_continue(wizard, outcome, &self.tool_names);
                }
            }
            _ => {}
        }
        DialogKeyOutcome::Consumed
    }

    /// The color step: pick the swatch the agent is drawn with.
    fn handle_create_color_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        match key {
            DialogKey::Escape => wizard_back_or_cancel(self),
            DialogKey::Up => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.color_picker = wizard
                        .color_picker
                        .clone()
                        .handle_event(ColorPickerEvent::Up)
                }
            }
            DialogKey::Down => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.color_picker = wizard
                        .color_picker
                        .clone()
                        .handle_event(ColorPickerEvent::Down)
                }
            }
            DialogKey::Enter => {
                if let Some(wizard) = self.wizard.as_mut() {
                    let value = wizard
                        .color_picker
                        .confirm()
                        .map(|color| color.as_str().to_string());
                    let outcome = wizard
                        .state
                        .clone()
                        .handle_event(WizardEvent::SubmitColor(value));
                    wizard_apply_continue(wizard, outcome, &self.tool_names);
                }
            }
            _ => {}
        }
        DialogKeyOutcome::Consumed
    }

    /// The memory step: pick which durable-memory directories the agent gets.
    fn handle_create_memory_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        let options = self
            .wizard
            .as_ref()
            .map(|wizard| wizard.state.memory_options())
            .unwrap_or_default();
        match key {
            DialogKey::Escape => wizard_back_or_cancel(self),
            DialogKey::Up => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.option_index = wizard.option_index.saturating_sub(1);
                }
            }
            DialogKey::Down => {
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.option_index =
                        (wizard.option_index + 1).min(options.len().saturating_sub(1));
                }
            }
            DialogKey::Enter => {
                if let Some(wizard) = self.wizard.as_mut() {
                    let outcome = wizard.state.clone().handle_event(WizardEvent::SubmitMemory(
                        options[wizard.option_index].value,
                    ));
                    wizard_apply_continue(wizard, outcome, &self.tool_names);
                }
            }
            _ => {}
        }
        DialogKeyOutcome::Consumed
    }

    /// The confirm step: Enter writes the definition, Esc walks back.
    fn handle_create_confirm_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        match key {
            DialogKey::Escape => wizard_back_or_cancel(self),
            DialogKey::Enter => {
                self.finish_create_wizard();
            }
            _ => {}
        }
        DialogKeyOutcome::Consumed
    }

    /// The generate step: type a description, Enter asks the host to write it.
    fn handle_create_generate_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        if let Some(wizard) = self.wizard.as_mut() {
            if wizard.is_generating {
                // While generating, only Esc cancels.
                if matches!(key, DialogKey::Escape) {
                    wizard.is_generating = false;
                    self.set_error("Generation cancelled.");
                }
            } else {
                match key {
                    DialogKey::Escape => wizard_back_or_cancel(self),
                    DialogKey::Backspace => {
                        if let Some(w) = self.wizard.as_mut() {
                            w.generate_input.pop();
                        }
                    }
                    DialogKey::Char { value: ch, .. } => {
                        if let Some(w) = self.wizard.as_mut() {
                            w.generate_input.push(ch);
                        }
                    }
                    DialogKey::Enter => {
                        let prompt = wizard.generate_input.trim().to_string();
                        if prompt.is_empty() {
                            self.set_error("Please describe what the agent should do.");
                        } else {
                            wizard.is_generating = true;
                            self.notice = None;
                            return DialogKeyOutcome::RequestGeneration { prompt };
                        }
                    }
                    _ => {}
                }
            }
        }
        DialogKeyOutcome::Consumed
    }

    fn handle_placeholder_key(&mut self, key: DialogKey) -> DialogKeyOutcome {
        match key {
            DialogKey::Escape | DialogKey::Enter => {
                self.menu = AgentsMenuState::default_initial();
                DialogKeyOutcome::Consumed
            }
            _ => DialogKeyOutcome::Ignored,
        }
    }

    fn current_agent(&self) -> Option<&AgentSummary> {
        self.menu.mode.anchored_agent()
    }

    fn selection_key(&self) -> Option<(String, AgentSource)> {
        if let Some(agent) = self.current_agent() {
            return Some((agent.agent_type.clone(), agent.source.clone()));
        }
        self.list
            .selected
            .as_ref()
            .map(|agent| (agent.agent_type.clone(), agent.source.clone()))
    }

    fn clamp_action_index(&mut self) {
        if let Some(agent) = self.current_agent() {
            let items = agent_menu_items(agent);
            if self.action_index >= items.len() {
                self.action_index = items.len().saturating_sub(1);
            }
        } else {
            self.action_index = 0;
        }
    }

    fn open_create_wizard(&mut self) {
        self.notice = None;
        self.menu = self
            .menu
            .clone()
            .handle_event(AgentsMenuEvent::OpenCreateAgent);
        self.wizard = Some(CreateWizardHost::new(
            &self.tool_names,
            &self.external_model_options,
        ));
    }

    fn dialog_title(&self) -> String {
        match &self.menu.mode {
            ModeState::ListAgents { source } => {
                format!("{} / File-backed agents", list_title(source))
            }
            ModeState::AgentMenu { agent, .. } => format!("{} / Menu", agent.agent_type),
            ModeState::ViewAgent { agent, .. } => format!("{} / Detail", agent.agent_type),
            ModeState::EditAgent { agent, .. } => format!("{} / Edit", agent.agent_type),
            ModeState::DeleteConfirm { agent, .. } => format!("{} / Delete", agent.agent_type),
            ModeState::CreateAgent => "Create agent".into(),
            ModeState::MainMenu => "Agents".into(),
        }
    }

    fn body_lines(&self, width: usize) -> Vec<PanelRow> {
        match &self.menu.mode {
            ModeState::ListAgents { .. } => self.list_body_lines(width),
            ModeState::AgentMenu { .. } => self.agent_menu_body_lines(width),
            ModeState::ViewAgent { .. } => self.detail_body_lines(width),
            ModeState::EditAgent { .. } => self.edit_body_lines(width),
            ModeState::DeleteConfirm { .. } => self.delete_body_lines(width),
            ModeState::CreateAgent => self.create_body_lines(width),
            ModeState::MainMenu => vec![plain_row("MainMenu is not used.")],
        }
    }

    fn footer_lines(&self, width: usize) -> Vec<PanelRow> {
        let hint = match self.menu.mode {
            ModeState::ListAgents { .. } => "Up/Down select | Enter open | Esc close",
            ModeState::AgentMenu { .. } => "Up/Down select | Enter confirm | Esc back",
            ModeState::ViewAgent { .. } => "Enter / Esc back",
            ModeState::EditAgent { .. } => "Up/Down select | Enter run | Esc back",
            ModeState::DeleteConfirm { .. } => "Enter delete | Esc cancel",
            ModeState::CreateAgent => match self.wizard.as_ref().map(|wizard| wizard.state.current)
            {
                Some(WizardStep::Type | WizardStep::Prompt | WizardStep::Description) => {
                    "Type text | Enter submit | Esc back"
                }
                Some(WizardStep::Tools) => "Up/Down move | Space toggle | a all | Enter submit",
                Some(WizardStep::Model | WizardStep::Color | WizardStep::Memory) => {
                    "Up/Down select | Enter submit | Esc back"
                }
                Some(WizardStep::Confirm) => "Enter save | Esc back",
                _ => "Up/Down select | Enter submit | Esc back",
            },
            ModeState::MainMenu => "Esc back",
        };

        let mut lines = vec![row(truncate_to_width(hint, width), RowTone::Dim)];

        let notice = match &self.notice {
            Some(Notice::Info(message)) => Some((message, RowTone::Warning)),
            Some(Notice::Error(message)) => Some((message, RowTone::Error)),
            None => None,
        };
        if let Some((message, tone)) = notice {
            lines.push(row(truncate_to_width(message, width), tone));
        }

        lines
    }

    fn list_body_lines(&self, width: usize) -> Vec<PanelRow> {
        let mut lines = vec![
            row(
                "This pass wires Project and User file-backed agents first.",
                RowTone::Dim,
            ),
            row(
                "Built-in/plugin/wizard/generator flows can land on top of this later.",
                RowTone::Dim,
            ),
            PanelRow::blank(),
        ];

        let create_prefix = if self.list.create_new_selected {
            "> "
        } else {
            "  "
        };
        lines.push(row(
            truncate_to_width(&format!("{create_prefix}Create new agent"), width),
            row_tone(self.list.create_new_selected),
        ));
        lines.push(plain_row("Press `c` to jump into the create wizard."));
        lines.push(PanelRow::blank());

        if self.list.selectable.is_empty() {
            lines.push(plain_row("No agent files found."));
            lines.push(plain_row(format!(
                "Project: ./{PROJECT_CONFIG_DIR_NAME}/agents/"
            )));
            lines.push(plain_row("User: ~/.rebon/agents/"));
            return lines;
        }

        for agent in &self.list.selectable {
            let is_selected = self
                .list
                .selected
                .as_ref()
                .map(|selected| {
                    selected.agent_type == agent.agent_type && selected.source == agent.source
                })
                .unwrap_or(false);
            let prefix = if is_selected { "> " } else { "  " };
            let source = source_label(&agent.source);
            // Both legs list together; the tag is how you tell an
            // external agent from one rebon runs itself.
            let runtime = if agent.runtime.is_external() {
                format!("  [{}]", agent.runtime.as_str())
            } else {
                String::new()
            };
            let text = truncate_to_width(
                &format!("{prefix}{}  [{source}]{runtime}", agent.agent_type),
                width,
            );
            lines.push(row(text, row_tone(is_selected)));
            if !agent.when_to_use.is_empty() {
                lines.push(row(
                    truncate_to_width(&format!("   {}", agent.when_to_use), width),
                    RowTone::Dim,
                ));
            }
        }
        lines
    }

    fn agent_menu_body_lines(&self, width: usize) -> Vec<PanelRow> {
        let Some(agent) = self.current_agent() else {
            return vec![plain_row("The selected agent no longer exists.")];
        };
        let items = agent_menu_items(agent);
        let mut lines = vec![
            row(
                truncate_to_width(&format!("agent: {}", agent.agent_type), width),
                RowTone::Strong,
            ),
            row(
                truncate_to_width(&format!("source: {}", source_label(&agent.source)), width),
                RowTone::Dim,
            ),
            PanelRow::blank(),
        ];
        for (idx, item) in items.iter().enumerate() {
            let selected = idx == self.action_index;
            let prefix = if selected { "> " } else { "  " };
            lines.push(row(
                truncate_to_width(&format!("{prefix}{}", item.label()), width),
                row_tone(selected),
            ));
        }
        lines
    }

    fn detail_body_lines(&self, width: usize) -> Vec<PanelRow> {
        let Some(agent) = self.current_agent() else {
            return vec![plain_row("The selected agent no longer exists.")];
        };
        let file_path = actual_relative_agent_file_path(&self.path_ctx, agent)
            .unwrap_or_else(|_| "unknown path".to_string());
        let detail = build_agent_detail(agent, file_path, &resolve_tools(agent, &self.tool_names));
        let mut lines = vec![
            row(truncate_to_width(&detail.file_path, width), RowTone::Dim),
            PanelRow::blank(),
            row("Description", RowTone::Strong),
            plain_row(truncate_to_width(&detail.description, width)),
            PanelRow::blank(),
            row("Tools", RowTone::Strong),
        ];
        match detail.tools {
            ToolsDisplay::AllTools => lines.push(plain_row("All tools")),
            ToolsDisplay::None => lines.push(plain_row("None")),
            ToolsDisplay::Explicit { valid, invalid } => {
                if !valid.is_empty() {
                    lines.push(plain_row(truncate_to_width(&valid.join(", "), width)));
                }
                if !invalid.is_empty() {
                    lines.push(row(
                        truncate_to_width(&format!("Unrecognized: {}", invalid.join(", ")), width),
                        RowTone::Warning,
                    ));
                }
            }
        }

        lines.push(PanelRow::blank());
        lines.push(plain_row(format!(
            "Model: {}",
            detail.model.unwrap_or_else(|| "inherit".into())
        )));
        lines.push(plain_row(format!(
            "Color: {}",
            detail.color.unwrap_or_else(|| "auto".into())
        )));
        if let Some(memory) = detail.memory {
            lines.push(plain_row(format!("Memory: {memory}")));
        }
        if let Some(effort) = agent.effort.as_ref() {
            lines.push(plain_row(format!("Effort: {}", effort.as_str())));
        }
        lines.push(plain_row(format!("Runtime: {}", detail.runtime)));
        if let Some(caveat) = detail.runtime_caveat.as_deref() {
            // The agent list is where a user decides which agent to
            // trust with a task. What it cannot do belongs here, not
            // in a surprise later.
            for line in wrap_to_width(caveat, width) {
                lines.push(row(line, RowTone::Dim));
            }
        }
        lines.push(PanelRow::blank());
        lines.push(row("System prompt", RowTone::Strong));
        for line in detail.system_prompt.lines() {
            lines.push(plain_row(truncate_to_width(line, width)));
        }
        lines
    }

    fn edit_body_lines(&self, width: usize) -> Vec<PanelRow> {
        let Some(editor) = self.editor.as_ref() else {
            return vec![plain_row("Editor state is not initialised.")];
        };
        let file_path = actual_relative_agent_file_path(&self.path_ctx, &editor.agent)
            .unwrap_or_else(|_| "unknown path".to_string());
        let mut lines = vec![
            row(truncate_to_width(&file_path, width), RowTone::Dim),
            PanelRow::blank(),
            row(
                "Only the external editor entrypoint is wired in this pass.",
                RowTone::Dim,
            ),
            PanelRow::blank(),
        ];
        for (idx, item) in EDITOR_MENU_ITEMS.iter().enumerate() {
            let selected = idx == editor.menu_index;
            let prefix = if selected { "> " } else { "  " };
            lines.push(row(
                truncate_to_width(&format!("{prefix}{}", item.label()), width),
                row_tone(selected),
            ));
        }
        lines
    }

    fn delete_body_lines(&self, width: usize) -> Vec<PanelRow> {
        let Some(agent) = self.current_agent() else {
            return vec![plain_row("The selected agent no longer exists.")];
        };
        let file_path = actual_relative_agent_file_path(&self.path_ctx, agent)
            .unwrap_or_else(|_| "unknown path".to_string());
        vec![
            row("Delete this agent file?", RowTone::Error),
            PanelRow::blank(),
            plain_row(format!("Agent: {}", agent.agent_type)),
            plain_row(format!("Source: {}", source_label(&agent.source))),
            plain_row(truncate_to_width(&format!("File: {file_path}"), width)),
        ]
    }

    fn create_body_lines(&self, width: usize) -> Vec<PanelRow> {
        let Some(wizard) = self.wizard.as_ref() else {
            return vec![plain_row("Wizard state is not initialised.")];
        };
        let mut lines = vec![
            row(
                format!("Step: {}", wizard_step_label(wizard.state.current)),
                RowTone::Strong,
            ),
            PanelRow::blank(),
        ];

        if let Some(error) = wizard.state.error.as_deref() {
            lines.push(row(truncate_to_width(error, width), RowTone::Error));
            lines.push(PanelRow::blank());
        }

        match wizard.state.current {
            WizardStep::Location => {
                lines.push(plain_row("Choose where the agent file should be stored."));
                lines.push(PanelRow::blank());
                for (idx, option) in WizardState::location_options().iter().enumerate() {
                    lines.push(render_option_line(
                        idx == wizard.option_index,
                        option.label,
                        width,
                    ));
                }
            }
            WizardStep::Method => {
                lines.push(plain_row("Choose how to create the agent."));
                lines.push(PanelRow::blank());
                for (idx, option) in WizardState::method_options().iter().enumerate() {
                    lines.push(render_option_line(
                        idx == wizard.option_index,
                        option.label,
                        width,
                    ));
                }
            }
            WizardStep::Type => {
                lines.push(plain_row("Agent type (kebab-case)."));
                lines.push(PanelRow::blank());
                lines.push(plain_row(render_input_line(&wizard.text_input, width)));
            }
            WizardStep::Prompt => {
                lines.push(plain_row("System prompt."));
                lines.push(PanelRow::blank());
                lines.push(plain_row(render_input_line(&wizard.text_input, width)));
            }
            WizardStep::Description => {
                lines.push(plain_row("Description / when to use."));
                lines.push(PanelRow::blank());
                lines.push(plain_row(render_input_line(&wizard.text_input, width)));
            }
            WizardStep::Tools => {
                lines.push(plain_row("Select tools. Space toggles, `a` toggles all."));
                lines.push(PanelRow::blank());
                lines.push(render_option_line(
                    wizard.tool_cursor == 0,
                    &format!(
                        "All tools [{}]",
                        if wizard.tool_selector.is_all_selected() {
                            "selected"
                        } else {
                            "not selected"
                        }
                    ),
                    width,
                ));
                for (idx, tool) in wizard.tool_selector.tools.iter().enumerate() {
                    let checked = wizard.tool_selector.selected.contains(&tool.name);
                    lines.push(render_option_line(
                        wizard.tool_cursor == idx + 1,
                        &format!("[{}] {}", if checked { "x" } else { " " }, tool.name),
                        width,
                    ));
                }
            }
            WizardStep::Model => {
                lines.push(plain_row("Choose a model override."));
                lines.push(PanelRow::blank());
                for (idx, option) in wizard.model_selector.options.iter().enumerate() {
                    lines.push(render_option_line(
                        idx == wizard.model_selector.selected_index,
                        &format!("{} - {}", option.label, option.description),
                        width,
                    ));
                }
            }
            WizardStep::Color => {
                lines.push(plain_row("Choose an agent color."));
                lines.push(PanelRow::blank());
                for (idx, option) in crate::surface::color_picker::color_options()
                    .iter()
                    .enumerate()
                {
                    lines.push(render_option_line(
                        idx == wizard.color_picker.selected_index,
                        option.label(),
                        width,
                    ));
                }
            }
            WizardStep::Memory => {
                lines.push(plain_row("Choose memory scope."));
                lines.push(PanelRow::blank());
                for (idx, option) in wizard.state.memory_options().iter().enumerate() {
                    lines.push(render_option_line(
                        idx == wizard.option_index,
                        option.label,
                        width,
                    ));
                }
            }
            WizardStep::Confirm => {
                lines.push(plain_row("Review the new agent before saving."));
                lines.push(PanelRow::blank());
                lines.extend(render_confirm_summary(&wizard.state, width));
            }
            WizardStep::Generate => {
                if wizard.is_generating {
                    lines.push(plain_row("Generating agent from description..."));
                    lines.push(PanelRow::blank());
                    lines.push(row("Press Esc to cancel", RowTone::Dim));
                } else {
                    lines.push(plain_row(
                        "Describe what this agent should do (be comprehensive for best results).",
                    ));
                    lines.push(PanelRow::blank());
                    lines.push(plain_row(render_input_line(&wizard.generate_input, width)));
                }
            }
        }

        lines
    }

    fn finish_create_wizard(&mut self) {
        let Some(wizard) = self.wizard.as_ref() else {
            return;
        };
        let data = wizard.state.data.clone();
        let Some(location) = data.location else {
            self.set_error("Wizard location is missing.");
            return;
        };
        let Some(agent_type) = data.agent_type.clone() else {
            self.set_error("Wizard agent type is missing.");
            return;
        };
        let Some(when_to_use) = data.when_to_use.clone() else {
            self.set_error("Wizard description is missing.");
            return;
        };
        let Some(system_prompt) = data.system_prompt.clone() else {
            self.set_error("Wizard system prompt is missing.");
            return;
        };

        let draft_tools = normalize_wizard_tools(data.tools.clone());
        let validation = validate_agent(
            &AgentDraft {
                agent_type: agent_type.clone(),
                when_to_use: when_to_use.clone(),
                tools: draft_tools.clone(),
                system_prompt: system_prompt.clone(),
                source: AgentSource::Settings(location),
            },
            &ResolvedTools {
                invalid_tools: draft_tools
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|tool| !self.tool_names.iter().any(|name| name == tool))
                    .collect(),
            },
            &self.agents,
        );
        if !validation.is_valid {
            self.set_error(
                validation
                    .errors
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "Validation failed.".to_string()),
            );
            return;
        }

        let mut fs = RealAgentFs;
        match save_agent_to_file(
            &mut fs,
            &self.path_ctx,
            AgentSource::Settings(location),
            &agent_type,
            &when_to_use,
            draft_tools.as_deref(),
            &system_prompt,
            true,
            data.color.as_deref(),
            data.model.as_deref(),
            data.selected_memory,
            data.effort.as_ref(),
        ) {
            Ok(path) => {
                self.notice = Some(Notice::Info(format!("Saved {path}")));
                self.menu = self.menu.clone().handle_event(AgentsMenuEvent::CreateSaved);
                self.wizard = None;
                self.initial_agent_type = Some(agent_type);
                self.refresh();
            }
            Err(err) => self.set_error(format!("Save failed: {}", format_save_error(&err))),
        }
    }
}

impl DialogModel for AgentsDialogState {
    rebon_dialog::dialog_plumbing!();

    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        match self.handle_key(press.key) {
            DialogKeyOutcome::Dismiss => DialogOutcome::Close,
            // Both of these leave the panel open: the editor opens
            // beside it, and the generation result comes back into the
            // wizard step that asked for it.
            DialogKeyOutcome::OpenInEditor { path } => {
                DialogOutcome::Action(DialogAction::staying(DIALOG_ID, ACTION_OPEN, path))
            }
            DialogKeyOutcome::RequestGeneration { prompt } => {
                DialogOutcome::Action(DialogAction::staying(DIALOG_ID, ACTION_GENERATE, prompt))
            }
            // A key no mode acts on is still swallowed: this is a modal,
            // and letting it through would type into the prompt behind.
            DialogKeyOutcome::Consumed | DialogKeyOutcome::Ignored => DialogOutcome::None,
        }
    }

    fn note_viewport(&mut self, _rows: u16, cols: u16) {
        // Rows are truncated and wrapped as they are built, so the
        // width has to arrive before `view` is asked, not after.
        self.viewport_cols = usize::from(cols).max(1);
    }

    fn view(&self) -> ViewSpec {
        let width = self.viewport_cols;
        ViewSpec::Panel(PanelView {
            title: format!(" {} ", self.dialog_title()),
            body: PanelPane {
                rows: self.body_lines(width),
                wrap: true,
                ..PanelPane::default()
            },
            // The hint block is a pane rather than the panel's own
            // footer because it is two rows, not one: the key hints,
            // and a notice under them when the panel has something to
            // say. `stack_below: u16::MAX` is how a panel says "always
            // below", which is what a hint block wants at any width.
            side: Some(PanelPane {
                rows: self.footer_lines(width),
                wrap: true,
                ..PanelPane::default()
            }),
            split: PanelSplit {
                stack_below: u16::MAX,
                stacked_second_rows: FOOTER_ROWS,
                ..PanelSplit::default()
            },
            ..PanelView::default()
        })
    }
}

fn build_list_state(agents: &[AgentSummary], initial_agent_type: Option<&str>) -> AgentsListState {
    let sorted = sort_agents(agents.to_vec());
    let selectable = selectable_in_order(&sorted, &crate::surface::utils::AgentSourceFilter::All);
    let mut list = AgentsListState::new(selectable, true);
    if let Some(agent_type) = initial_agent_type {
        if let Some(selected) = list
            .selectable
            .iter()
            .find(|agent| agent.agent_type == agent_type)
            .cloned()
        {
            list.create_new_selected = false;
            list.selected = Some(selected);
        }
    } else if let Some(first) = list.selectable.first().cloned() {
        list.create_new_selected = false;
        list.selected = Some(first);
    }
    list
}

fn sync_mode_with_agents(mode: ModeState, agents: &[AgentSummary]) -> ModeState {
    match mode {
        ModeState::ListAgents { source } => ModeState::ListAgents { source },
        ModeState::MainMenu => ModeState::ListAgents {
            source: crate::surface::utils::AgentSourceFilter::All,
        },
        ModeState::CreateAgent => ModeState::CreateAgent,
        ModeState::AgentMenu {
            agent,
            previous_mode,
        } => fresh_agent(agents, &agent)
            .map(|fresh| ModeState::AgentMenu {
                agent: Box::new(fresh.clone()),
                previous_mode,
            })
            .unwrap_or_else(|| AgentsMenuState::default_initial().mode),
        ModeState::ViewAgent {
            agent,
            previous_mode,
        } => fresh_agent(agents, &agent)
            .map(|fresh| ModeState::ViewAgent {
                agent: Box::new(fresh.clone()),
                previous_mode,
            })
            .unwrap_or_else(|| AgentsMenuState::default_initial().mode),
        ModeState::EditAgent {
            agent,
            previous_mode,
        } => fresh_agent(agents, &agent)
            .map(|fresh| ModeState::EditAgent {
                agent: Box::new(fresh.clone()),
                previous_mode,
            })
            .unwrap_or_else(|| AgentsMenuState::default_initial().mode),
        ModeState::DeleteConfirm {
            agent,
            previous_mode,
        } => fresh_agent(agents, &agent)
            .map(|fresh| ModeState::DeleteConfirm {
                agent: Box::new(fresh.clone()),
                previous_mode,
            })
            .unwrap_or_else(|| AgentsMenuState::default_initial().mode),
    }
}

fn resolve_tools(agent: &AgentSummary, tool_names: &[String]) -> ResolvedTools {
    let invalid_tools = agent
        .tools
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|tool| tool != "*" && !tool_names.iter().any(|name| name == tool))
        .collect();
    ResolvedTools { invalid_tools }
}

/// Break `text` into lines that fit `width`, on word boundaries.
///
/// The detail view truncates most rows because they are short labels.
/// A caveat is a sentence, and a truncated caveat is worse than none —
/// it stops mid-explanation and reads like a rendering bug.
fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    let width = width.max(16);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.len()
        } else {
            current.chars().count() + 1 + word.chars().count()
        };
        if candidate > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[derive(Debug, Clone, Copy)]
enum WizardTextKind {
    Type,
    Prompt,
    Description,
}

fn wizard_apply_continue(
    wizard: &mut CreateWizardHost,
    outcome: WizardOutcome,
    tool_names: &[String],
) {
    if let WizardOutcome::Continue(state) = outcome {
        wizard.state = state;
        wizard.sync_for_current_step(tool_names);
    }
}

fn wizard_back_or_cancel(dialog: &mut AgentsDialogState) {
    let Some(wizard) = dialog.wizard.as_mut() else {
        return;
    };
    if wizard.state.current == WizardStep::Location {
        dialog.menu = dialog
            .menu
            .clone()
            .handle_event(AgentsMenuEvent::CreateCancelled);
        dialog.wizard = None;
        return;
    }
    let outcome = wizard.state.clone().handle_event(WizardEvent::Back);
    wizard_apply_continue(wizard, outcome, &dialog.tool_names);
}

fn handle_wizard_text_input(
    key: DialogKey,
    wizard: &mut CreateWizardHost,
    submit_event: WizardEvent,
    tool_names: &[String],
    kind: WizardTextKind,
) {
    match key {
        DialogKey::Escape => {}
        DialogKey::Backspace => {
            wizard.text_input.pop();
        }
        DialogKey::Char { value: ch, .. } => wizard.text_input.push(ch),
        DialogKey::Enter => {
            let outcome = wizard.state.clone().handle_event(submit_event);
            wizard_apply_continue(wizard, outcome, tool_names);
        }
        _ => {}
    }

    if matches!(key, DialogKey::Escape) {
        let outcome = wizard.state.clone().handle_event(WizardEvent::Back);
        wizard_apply_continue(wizard, outcome, tool_names);
    } else {
        match kind {
            WizardTextKind::Type | WizardTextKind::Prompt | WizardTextKind::Description => {}
        }
    }
}

fn wizard_toggle_tool_at_cursor(wizard: &mut CreateWizardHost) {
    if wizard.tool_cursor == 0 {
        wizard.tool_selector = wizard.tool_selector.clone().toggle_all();
        return;
    }
    if let Some(tool) = wizard.tool_selector.tools.get(wizard.tool_cursor - 1) {
        wizard.tool_selector = wizard.tool_selector.clone().toggle_tool(&tool.name);
    }
}

fn default_agent_model_options() -> Vec<ModelOption> {
    vec![
        ModelOption {
            value: "sonnet".into(),
            label: "Sonnet".into(),
            description: "Balanced performance - best for most agents".into(),
        },
        ModelOption {
            value: "opus".into(),
            label: "Opus".into(),
            description: "Most capable for complex reasoning tasks".into(),
        },
        ModelOption {
            value: "haiku".into(),
            label: "Haiku".into(),
            description: "Fast and efficient for simple tasks".into(),
        },
        ModelOption {
            value: "inherit".into(),
            label: "Inherit from parent".into(),
            description: "Use the same model as the main conversation".into(),
        },
    ]
}

fn build_tool_entries(tool_names: &[String]) -> Vec<ToolEntry> {
    tool_names
        .iter()
        .filter(|name| name.as_str() != "Agent")
        .map(|name| ToolEntry::new(name.clone(), name.starts_with("mcp__")))
        .collect()
}

fn parse_color_name(color: Option<&str>) -> Option<AgentColorName> {
    match color {
        Some("red") => Some(AgentColorName::Red),
        Some("blue") => Some(AgentColorName::Blue),
        Some("green") => Some(AgentColorName::Green),
        Some("yellow") => Some(AgentColorName::Yellow),
        Some("purple") => Some(AgentColorName::Purple),
        Some("orange") => Some(AgentColorName::Orange),
        Some("pink") => Some(AgentColorName::Pink),
        Some("cyan") => Some(AgentColorName::Cyan),
        _ => None,
    }
}

fn normalize_wizard_tools(tools: Option<Vec<String>>) -> Option<Vec<String>> {
    match tools {
        Some(values) if values.len() == 1 && values[0] == "*" => None,
        other => other,
    }
}

fn wizard_step_label(step: WizardStep) -> &'static str {
    match step {
        WizardStep::Location => "Location",
        WizardStep::Method => "Method",
        WizardStep::Generate => "Generate",
        WizardStep::Type => "Type",
        WizardStep::Prompt => "Prompt",
        WizardStep::Description => "Description",
        WizardStep::Tools => "Tools",
        WizardStep::Model => "Model",
        WizardStep::Color => "Color",
        WizardStep::Memory => "Memory",
        WizardStep::Confirm => "Confirm",
    }
}

/// The role a row takes when it is or is not the selection.
///
/// One rule for every list in the panel: the agent list, the action
/// menu, the editor menu and every wizard option list all mark the
/// highlighted row the same way.
fn row_tone(selected: bool) -> RowTone {
    if selected {
        RowTone::Focus
    } else {
        RowTone::Normal
    }
}

fn render_option_line(selected: bool, label: &str, width: usize) -> PanelRow {
    let prefix = if selected { "> " } else { "  " };
    row(
        truncate_to_width(&format!("{prefix}{label}"), width),
        row_tone(selected),
    )
}

fn render_input_line(value: &str, width: usize) -> String {
    let display = if value.is_empty() { "_" } else { value };
    truncate_to_width(&format!("> {display}"), width)
}

fn render_confirm_summary(wizard: &WizardState, width: usize) -> Vec<PanelRow> {
    let data = &wizard.data;
    let mut rows = Vec::new();
    rows.push(plain_row(truncate_to_width(
        &format!(
            "Location: {}",
            match data.location {
                Some(SettingSource::ProjectSettings) => "Project",
                Some(SettingSource::UserSettings) => "Personal",
                Some(SettingSource::LocalSettings) => "Local",
                Some(SettingSource::FlagSettings) => "CLI flag",
                Some(SettingSource::PolicySettings) => "Managed",
                None => "unset",
            }
        ),
        width,
    )));
    rows.push(plain_row(truncate_to_width(
        &format!(
            "Type: {}",
            data.agent_type.as_deref().unwrap_or("(missing)")
        ),
        width,
    )));
    rows.push(plain_row(truncate_to_width(
        &format!(
            "Description: {}",
            data.when_to_use.as_deref().unwrap_or("(missing)")
        ),
        width,
    )));
    rows.push(plain_row(truncate_to_width(
        &format!("Model: {}", data.model.as_deref().unwrap_or("inherit")),
        width,
    )));
    rows.push(plain_row(truncate_to_width(
        &format!("Color: {}", data.color.as_deref().unwrap_or("automatic")),
        width,
    )));
    rows.push(plain_row(truncate_to_width(
        &format!(
            "Memory: {}",
            match data.selected_memory {
                Some(AgentMemoryScope::User) => "user",
                Some(AgentMemoryScope::Project) => "project",
                None => "none",
            }
        ),
        width,
    )));
    rows.push(plain_row(truncate_to_width(
        &format!(
            "Tools: {}",
            match normalize_wizard_tools(data.tools.clone()) {
                None => "all".into(),
                Some(values) if values.is_empty() => "none".into(),
                Some(values) => values.join(", "),
            }
        ),
        width,
    )));
    rows.push(PanelRow::blank());
    rows.push(row("Press Enter to save the agent file.", RowTone::Dim));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::wizard::CreationMethod;
    use tempfile::TempDir;

    /// Point the config home at `config_dir` for the duration of `run`.
    ///
    /// The lock is `rebon-tool`'s process-wide env lock, the same one
    /// `TestConfigHome` takes: these tests read the config home through
    /// the agent registry, so they must not run beside anything else
    /// that moves it.
    fn with_config_dir<T>(config_dir: &Path, run: impl FnOnce() -> T) -> T {
        let _guard = rebon_tool::env_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::env::var("REBON_CONFIG_DIR").ok();
        std::env::set_var("REBON_CONFIG_DIR", config_dir);
        let result = run();
        match old {
            Some(value) => std::env::set_var("REBON_CONFIG_DIR", value),
            None => std::env::remove_var("REBON_CONFIG_DIR"),
        }
        result
    }

    fn write_agent_file(
        dir: &Path,
        file_name: &str,
        agent_type: &str,
        description: &str,
        prompt: &str,
    ) {
        std::fs::create_dir_all(dir).expect("create agents dir");
        std::fs::write(
            dir.join(file_name),
            format!("---\nname: {agent_type}\ndescription: {description}\n---\n{prompt}\n"),
        )
        .expect("write agent file");
    }

    #[test]
    fn registry_agents_include_builtins_in_dialog_listing() {
        let project = TempDir::new().expect("temp project");
        let config = TempDir::new().expect("temp config");
        with_config_dir(config.path(), || {
            let dialog = AgentsDialogState::open(project.path(), vec!["Read".into()], None);
            let builtin = dialog
                .agents
                .iter()
                .find(|agent| agent.agent_type == "rebon-code-guide")
                .expect("builtin rebon-code-guide should be visible");
            assert_eq!(builtin.source, AgentSource::BuiltIn);
            assert!(!builtin.is_editable());
            assert!(dialog
                .agents
                .iter()
                .any(|agent| agent.agent_type == "Explore"));
        });
    }

    #[test]
    fn registry_agents_hide_disabled_file_backed_override() {
        let project = TempDir::new().expect("temp project");
        let config = TempDir::new().expect("temp config");
        let agents_dir = config.path().join("agents");
        write_agent_file(
            &agents_dir,
            "rebon-code-guide.md",
            "rebon-code-guide",
            "DISABLED. Do not invoke.",
            "This agent is intentionally disabled.",
        );

        with_config_dir(config.path(), || {
            let dialog = AgentsDialogState::open(project.path(), vec!["Read".into()], None);
            assert!(!dialog
                .agents
                .iter()
                .any(|agent| agent.agent_type == "rebon-code-guide"));
            assert!(dialog
                .agents
                .iter()
                .any(|agent| agent.agent_type == "Explore"));
        });
    }

    #[test]
    fn registry_agents_mark_file_backed_override_editable() {
        let project = TempDir::new().expect("temp project");
        let config = TempDir::new().expect("temp config");
        let agents_dir = project.path().join(PROJECT_CONFIG_DIR_NAME).join("agents");
        write_agent_file(
            &agents_dir,
            "rebon-code-guide.md",
            "rebon-code-guide",
            "Custom guide override",
            "Project override prompt.",
        );

        with_config_dir(config.path(), || {
            let dialog = AgentsDialogState::open(project.path(), vec!["Read".into()], None);
            let overridden = dialog
                .agents
                .iter()
                .find(|agent| agent.agent_type == "rebon-code-guide")
                .expect("overridden builtin should be visible");
            assert_eq!(
                overridden.source,
                AgentSource::Settings(SettingSource::ProjectSettings)
            );
            assert!(overridden.is_editable());
            assert_eq!(overridden.filename, None);
            assert_eq!(overridden.system_prompt, "Project override prompt.");
        });
    }

    #[test]
    fn registry_agents_preserve_non_matching_file_stem_for_edit_path() {
        let project = TempDir::new().expect("temp project");
        let config = TempDir::new().expect("temp config");
        let agents_dir = config.path().join("agents");
        write_agent_file(
            &agents_dir,
            "friendly-name.md",
            "custom-worker",
            "Custom worker",
            "User prompt.",
        );

        with_config_dir(config.path(), || {
            let dialog = AgentsDialogState::open(project.path(), vec!["Read".into()], None);
            let custom = dialog
                .agents
                .iter()
                .find(|agent| agent.agent_type == "custom-worker")
                .expect("custom user agent should be visible");
            assert_eq!(
                custom.source,
                AgentSource::Settings(SettingSource::UserSettings)
            );
            assert!(custom.is_editable());
            assert_eq!(custom.filename.as_deref(), Some("friendly-name"));
        });
    }

    #[test]
    fn build_list_state_keeps_create_option_and_selects_first_agent() {
        let agents = vec![
            AgentSummary::minimal(
                "alpha",
                "a useful agent",
                "prompt body long enough",
                AgentSource::Settings(SettingSource::UserSettings),
            ),
            AgentSummary::minimal(
                "beta",
                "another useful agent",
                "prompt body long enough",
                AgentSource::Settings(SettingSource::ProjectSettings),
            ),
        ];
        let state = build_list_state(&agents, None);
        assert!(state.has_create_option);
        assert!(!state.create_new_selected);
        assert_eq!(
            state
                .selected
                .as_ref()
                .map(|agent| agent.agent_type.as_str()),
            Some("beta")
        );
    }

    #[test]
    fn build_list_state_prefers_requested_agent_type() {
        let agents = vec![
            AgentSummary::minimal(
                "alpha",
                "a useful agent",
                "prompt body long enough",
                AgentSource::Settings(SettingSource::UserSettings),
            ),
            AgentSummary::minimal(
                "beta",
                "another useful agent",
                "prompt body long enough",
                AgentSource::Settings(SettingSource::ProjectSettings),
            ),
        ];
        let state = build_list_state(&agents, Some("beta"));
        assert_eq!(
            state
                .selected
                .as_ref()
                .map(|agent| agent.agent_type.as_str()),
            Some("beta")
        );
    }

    #[test]
    fn handle_list_key_c_opens_create_wizard() {
        let project = TempDir::new().expect("temp project");
        let config = TempDir::new().expect("temp config");
        with_config_dir(config.path(), || {
            let mut dialog = AgentsDialogState::open(project.path(), vec!["Read".into()], None);
            assert!(matches!(dialog.menu.mode, ModeState::ListAgents { .. }));
            assert_eq!(
                dialog.on_key(DialogKey::plain('c').into()),
                DialogOutcome::None
            );
            assert!(matches!(dialog.menu.mode, ModeState::CreateAgent));
            assert!(dialog.wizard.is_some());
        });
    }

    #[test]
    fn finish_create_wizard_saves_agent_file_and_returns_to_list() {
        let project = TempDir::new().expect("temp project");
        let config = TempDir::new().expect("temp config");
        with_config_dir(config.path(), || {
            let mut dialog = AgentsDialogState::open(project.path(), vec!["Read".into()], None);
            dialog.open_create_wizard();

            let wizard = dialog.wizard.as_mut().expect("wizard state");
            wizard.state.data.location = Some(SettingSource::ProjectSettings);
            wizard.state.data.method = Some(CreationMethod::Manual);
            wizard.state.data.agent_type = Some("code-reviewer".into());
            wizard.state.data.when_to_use =
                Some("Use this agent when reviewing code for regressions".into());
            wizard.state.data.system_prompt =
                Some("You are a code reviewer focused on regressions and tests.".into());
            wizard.state.data.tools = Some(vec!["Read".into()]);
            wizard.state.current = WizardStep::Confirm;

            dialog.finish_create_wizard();

            let saved_path = project
                .path()
                .join(PROJECT_CONFIG_DIR_NAME)
                .join("agents")
                .join("code-reviewer.md");
            assert!(saved_path.exists());
            let saved = std::fs::read_to_string(saved_path).expect("saved file");
            assert!(saved.contains("name: code-reviewer"));
            assert!(saved.contains("tools: Read"));
            assert!(matches!(dialog.menu.mode, ModeState::ListAgents { .. }));
            assert_eq!(
                dialog
                    .list
                    .selected
                    .as_ref()
                    .map(|agent| agent.agent_type.as_str()),
                Some("code-reviewer")
            );
        });
    }

    fn panel_view(dialog: &AgentsDialogState) -> PanelView {
        match dialog.view() {
            ViewSpec::Panel(view) => view,
            other => panic!("the agents panel describes a panel, not {other:?}"),
        }
    }

    fn row_texts(rows: &[PanelRow]) -> Vec<String> {
        rows.iter().map(PanelRow::text).collect()
    }

    /// The panel opened over an empty project and an empty config home,
    /// so nothing the machine running the test happens to have installed
    /// changes which row is selected.
    fn open_isolated<T>(read: impl FnOnce(&mut AgentsDialogState) -> T) -> T {
        let project = TempDir::new().expect("temp project");
        let config = TempDir::new().expect("temp config home");
        with_config_dir(config.path(), || {
            let mut dialog = AgentsDialogState::open(project.path(), vec!["Read".into()], None);
            read(&mut dialog)
        })
    }

    /// What the deleted terminal-side painter used to assert on a
    /// painted screen: the title, the first rows, and the key hints.
    #[test]
    fn the_view_is_a_panel_of_rows_over_a_hint_block_that_always_stacks_below() {
        let view = open_isolated(|dialog| panel_view(dialog));

        assert!(
            view.title.contains("File-backed agents"),
            "{:?}",
            view.title
        );
        let body = row_texts(&view.body.rows);
        assert!(
            body.iter().any(|row| row.contains("Create new agent")),
            "{body:?}"
        );
        assert!(view.body.wrap, "a long row wraps rather than being clipped");

        // The hints are a pane of their own rather than the panel's
        // footer row: they are two rows tall, and they must keep both
        // whether or not a notice is showing, so the body above them
        // does not shift as one appears and goes.
        let hints = view.side.expect("the hint block is a pane of its own");
        assert_eq!(
            row_texts(&hints.rows),
            vec!["Up/Down select | Enter open | Esc close".to_string()]
        );
        assert_eq!(hints.rows[0].spans[0].tone, RowTone::Dim);
        assert_eq!(view.split.stacked_second_rows, FOOTER_ROWS);
        assert_eq!(
            view.split.stack_below,
            u16::MAX,
            "the hint block sits below the body at every width"
        );
        assert!(
            view.footer.is_empty(),
            "the panel's own one-row footer is unused"
        );
        assert!(
            view.desired_height.is_none(),
            "the panel fills whatever frame the host offers it"
        );
    }

    /// The selection is a role on the row, not a reversed line: the
    /// panel already draws its own `> ` caret, and reversing on top of
    /// that would read as two selections.
    #[test]
    fn the_selected_row_carries_the_focus_role_and_is_not_reversed() {
        let view = open_isolated(|dialog| panel_view(dialog));
        let selected = view
            .body
            .rows
            .iter()
            .find(|row| row.text().starts_with("> "))
            .expect("one row of the list carries the selection caret");
        assert_eq!(selected.spans[0].tone, RowTone::Focus);
        assert!(!selected.highlighted);
    }

    #[test]
    fn a_notice_takes_the_second_row_of_the_hint_block() {
        let hints = open_isolated(|dialog| {
            dialog.set_error("Save failed: no such directory");
            panel_view(dialog).side.expect("the hint block")
        });
        let rows = row_texts(&hints.rows);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[1].contains("Save failed"), "{rows:?}");
        assert_eq!(hints.rows[1].spans[0].tone, RowTone::Error);
    }

    /// Rows are truncated as they are built, so the width has to reach
    /// the panel before `view` is asked, not after it has answered.
    #[test]
    fn the_surfaces_width_reaches_the_rows_before_the_view_is_asked_for() {
        let create_row = |dialog: &AgentsDialogState| {
            panel_view(dialog)
                .body
                .rows
                .iter()
                .map(PanelRow::text)
                .find(|row| row.contains("Create"))
                .expect("the create entry")
        };
        let (wide, narrow) = open_isolated(|dialog| {
            let wide = create_row(dialog);
            dialog.note_viewport(10, 12);
            (wide, create_row(dialog))
        });
        assert!(narrow.chars().count() <= 12, "{narrow:?}");
        assert!(narrow.len() < wide.len(), "{narrow:?} vs {wide:?}");
    }
}

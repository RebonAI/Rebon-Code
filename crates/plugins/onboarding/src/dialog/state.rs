//! The wizard's state and every step it can take.
//!
//! Nothing here draws or reads a keystroke; the terminal half calls in
//! with a decision already made and reads the result back out of these
//! fields. They are `pub` for exactly that reason.

use crate::migrate::{DiscoverySnapshot, ImportCategory, ImportSummary};
use crate::onboarding::{
    actionable_login_options, grouped_login_pane_rows, LoginMethodOption, ProviderField,
    ProviderFormMode, SetupPane, Step, TextField,
};
use crate::onboarding::{normalize_single_line_paste, PRESET_ROWS_VISIBLE};
use rebon_design_system::theme::ThemeName;
use rebon_picker::theme_picker::ThemeSetting;

use super::outcome::{OnboardingDialogOutcome, OnboardingStepTransition};
use rebon_config::UiMode;

/// Supported provider formats, cycled through on Tab when `Format`
/// is focused.
pub const PROVIDER_FORMATS: [&str; 3] = ["openai", "openai-responses", "anthropic"];

impl PanelStatus {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingSetupChoice {
    Preserve,
    Reconfigure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderPresetSelection {
    Preset(&'static str),
    Custom,
}

impl ProviderPresetSelection {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Preset(id) => rebon_config::provider_preset_by_id(id)
                .map(|preset| preset.display_name)
                .unwrap_or(id),
            Self::Custom => "Custom",
        }
    }
}

/// Sub-view within the OAuth flow overlay that the Provider step
/// enters after the user picks "OpenAI account" from LoginMethods.
///
/// The dialog never performs IO itself — the runner drives the flow
/// via [`OnboardingDialogOutcome::StartOpenAIOAuth`] and the
/// `report_oauth_*` methods, which just nudge this enum between
/// states. Esc from any of these variants cancels back to
/// `LoginMethods` and emits
/// [`OnboardingDialogOutcome::CancelOpenAIOAuth`] so the runner can
/// tear down the listener / in-flight request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthView {
    /// PKCE prepared, browser-launch attempted. Runner transitions
    /// to `WaitingCallback` once the listener is armed, or
    /// `AwaitingPaste` if port 1455 was busy.
    StartingBrowser { authorize_url: String },
    /// Local listener on port 1455 is holding for the browser
    /// callback. The URL is shown so the user can copy-paste it if
    /// the browser launch failed silently.
    WaitingCallback { authorize_url: String },
    /// Paste fallback — the port was busy or the listener errored
    /// out, so the user needs to copy the full callback URL back
    /// from the browser address bar. `input` tracks the paste buffer
    /// and `error` surfaces the last parse/state-mismatch message.
    AwaitingPaste {
        authorize_url: String,
        input: TextField,
        error: Option<String>,
    },
    /// Token-exchange POST in flight.
    Exchanging,
    /// Something failed. Enter retries from the top, Esc backs out
    /// to LoginMethods. `can_retry` is `false` when retrying would
    /// hit the same error (e.g. persisted-credentials write failed).
    Error { message: String, can_retry: bool },
}

/// Per-pane banner text shown after a side-effect completes. Cleared
/// when the user edits any field so stale status doesn't stick around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelStatus {
    None,
    Ok(String),
    Err(String),
}

/// Everything the wizard would otherwise have read off disk.
///
/// Built by whoever opens the dialog, so the machine below can be driven
/// from a test without a config directory, a home directory, or a theme
/// on disk. [`crate::store::onboarding_open_inputs`] builds the real one.
#[derive(Debug, Clone, Default)]
pub struct OnboardingOpenInputs {
    /// Which providers are configured and how, as `/onboarding` sees it.
    pub setup_status: rebon_config::ProviderSetupStatus,
    /// The custom providers, already read out of the store.
    pub providers: Vec<ProviderSnapshot>,
    /// Label/value pairs for the theme picker, already built.
    pub theme_options: Vec<(String, ThemeSetting)>,
    /// The theme id saved on disk, so reopening highlights it.
    pub saved_theme: Option<String>,
    /// What an import would find in the home directory.
    pub migration: DiscoverySnapshot,
}

/// Cloneable render + reducer state for the onboarding dialog.
#[derive(Debug, Clone)]
pub struct OnboardingDialogState {
    pub steps: Vec<Step>,
    pub current_step_index: usize,
    /// True when all steps are completed.
    pub done: bool,
    pub dialog_title: &'static str,
    pub show_welcome: bool,
    /// When `true`, the Provider step's Login pane advances to the
    /// next step on Enter (onboarding wizard flow). When `false`, Enter
    /// on a login method stays on the dedicated dialog unless that method
    /// starts its own flow.
    pub allow_advance_from_login: bool,
    /// When `true`, this is a dedicated `/theme` picker (a single
    /// `Step::Theme`). The runner uses this to avoid marking onboarding
    /// as completed when the theme-only dialog advances/closes.
    pub is_theme_only: bool,
    /// First-run onboarding runs before the final TUI surface is chosen,
    /// so its UI mode selection can apply without a restart.
    pub ui_mode_applies_immediately: bool,

    // Theme picker state
    pub theme_focus: usize,
    pub theme_options: Vec<(String, ThemeSetting)>,

    // UI mode picker state
    pub ui_mode: UiMode,

    // Setup step state
    pub setup_pane: SetupPane,
    pub provider_setup_status: rebon_config::ProviderSetupStatus,
    pub existing_setup_choice: ExistingSetupChoice,
    pub login_from_existing_setup: bool,
    /// Login methods that are executable in this build. Unsupported
    /// decorative options are omitted rather than left focusable.
    pub login_options: Vec<LoginMethodOption>,
    /// Index in `LoginMethods`. Values 0..login_options.len() select an
    /// option; login_options.len() selects the "Add custom provider"
    /// CTA.
    pub login_focus: usize,

    // Provider panel form fields.
    pub provider_form: ProviderFormState,
    pub provider_form_mode: ProviderFormMode,
    /// Focused field in the provider panel.
    pub provider_field_focus: ProviderField,
    /// Snapshot of custom providers (name + models) used for the
    /// Models section. Refreshed when `AddProvider` / `AddProviderModel`
    /// succeeds (via `report_*_result`).
    pub provider_snapshot: Vec<ProviderSnapshot>,
    /// Name of the provider whose models are displayed in the Models
    /// section. Defaults to the most recently added provider.
    pub selected_provider: Option<String>,
    /// Whether the current setup has usable model credentials from a
    /// configured provider or supported environment variable.
    pub provider_ready: bool,
    /// Banner for the Add Provider form.
    pub add_provider_status: PanelStatus,
    /// Banner for the Add Model form.
    pub add_model_status: PanelStatus,
    /// When `Some`, the Provider step is showing the OpenAI OAuth
    /// sub-view instead of LoginMethods/ProviderPanel. Cleared by
    /// Esc, by `report_oauth_success`, or by the runner calling
    /// [`OnboardingDialogState::clear_oauth_view`].
    pub oauth_view: Option<OAuthView>,

    // Migration step state.
    /// Counts + absolute source paths per category. Computed once
    /// when the dialog opens so counts don't jitter while the user
    /// walks through categories.
    pub migration_snapshot: DiscoverySnapshot,
    /// Parallel to [`ImportCategory::ALL`]. `true` means the user
    /// wants to import that category. Defaults to `true` for any
    /// non-empty category so the sensible path is one Enter press.
    pub migration_selected: [bool; 5],
    /// Index into [`ImportCategory::ALL`] of the currently-focused
    /// checklist row.
    pub migration_focus: usize,
    /// Banner shown after the migration runs. `None` before the
    /// user confirms; `Ok(summary)` after a successful run;
    /// `Err(message)` if the runner reported a failure.
    pub migration_status: Option<Result<ImportSummary, String>>,
}

/// Snapshot of a provider for rendering the Models section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSnapshot {
    pub name: String,
    pub format: String,
    pub base_url: String,
    pub api_key: String,
    pub active_model: String,
    pub models: Vec<String>,
}

/// Text input state for the provider form + new-model entry.
#[derive(Debug, Clone)]
pub struct ProviderFormState {
    pub preset_options: Vec<ProviderPresetSelection>,
    pub preset_idx: usize,
    pub name: TextField,
    pub api_key: TextField,
    pub base_url: TextField,
    pub format_idx: usize,
    pub model: TextField,
    pub new_model: TextField,
}

impl Default for ProviderFormState {
    fn default() -> Self {
        let mut state = Self {
            preset_options: provider_preset_options(),
            preset_idx: 0,
            name: TextField::default(),
            api_key: TextField::default(),
            base_url: TextField::default(),
            format_idx: 0,
            model: TextField::default(),
            new_model: TextField::default(),
        };
        state.apply_selected_preset();
        state
    }
}

impl ProviderFormState {
    pub fn format(&self) -> &'static str {
        PROVIDER_FORMATS[self.format_idx.min(PROVIDER_FORMATS.len() - 1)]
    }

    pub fn selected_preset(&self) -> ProviderPresetSelection {
        self.preset_options
            .get(self.preset_idx)
            .copied()
            .unwrap_or(ProviderPresetSelection::Custom)
    }

    pub fn selected_preset_id(&self) -> Option<&'static str> {
        let preset = self.selected_provider_preset()?;
        if self.name.value.trim() != preset.id
            || self.base_url.value.trim() != preset.base_url
            || self.format() != preset.format
            || self.model.value.trim() != preset.default_model
        {
            return None;
        }
        match self.selected_preset() {
            ProviderPresetSelection::Preset(id) => Some(id),
            ProviderPresetSelection::Custom => None,
        }
    }

    pub fn selected_provider_preset(&self) -> Option<&'static rebon_config::ProviderPreset> {
        match self.selected_preset() {
            ProviderPresetSelection::Preset(id) => rebon_config::provider_preset_by_id(id),
            ProviderPresetSelection::Custom => None,
        }
    }

    pub fn cycle_preset(&mut self, forward: bool) {
        if self.preset_options.is_empty() {
            return;
        }
        let n = self.preset_options.len();
        self.preset_idx = if forward {
            (self.preset_idx + 1) % n
        } else {
            (self.preset_idx + n - 1) % n
        };
        self.apply_selected_preset();
    }

    pub fn apply_selected_preset(&mut self) {
        let Some(preset) = self.selected_provider_preset() else {
            self.clear_preset_fields();
            return;
        };
        self.name.set(preset.id);
        self.base_url.set(preset.base_url);
        self.format_idx = PROVIDER_FORMATS
            .iter()
            .position(|format| *format == preset.format)
            .unwrap_or(0);
        self.model.set(preset.default_model);
    }

    pub fn clear_preset_fields(&mut self) {
        self.name.clear();
        self.base_url.clear();
        self.format_idx = 0;
        self.model.clear();
    }

    pub fn clear_preset_coupling(&mut self) {
        if let Some(idx) = self
            .preset_options
            .iter()
            .position(|selection| matches!(selection, ProviderPresetSelection::Custom))
        {
            self.preset_idx = idx;
        }
    }

    pub fn populate_from_snapshot(&mut self, snapshot: &ProviderSnapshot) {
        self.clear_preset_coupling();
        self.name.set(&snapshot.name);
        self.api_key.set(&snapshot.api_key);
        self.base_url.set(&snapshot.base_url);
        self.format_idx = PROVIDER_FORMATS
            .iter()
            .position(|format| *format == snapshot.format)
            .unwrap_or(0);
        self.model.set(&snapshot.active_model);
        self.new_model.clear();
    }

    pub fn clear_after_add(&mut self) {
        self.clear_preset_coupling();
        self.clear_preset_fields();
        self.api_key.clear();
    }

    pub fn success_subject(&self, submitted_name: &str) -> String {
        self.selected_provider_preset()
            .map(|preset| format!("{} provider", preset.display_name))
            .unwrap_or_else(|| format!("Provider \"{submitted_name}\""))
    }

    pub fn cycle_format(&mut self, forward: bool) {
        let n = PROVIDER_FORMATS.len();
        self.format_idx = if forward {
            (self.format_idx + 1) % n
        } else {
            (self.format_idx + n - 1) % n
        };
        self.clear_preset_coupling();
    }
}

fn provider_preset_options() -> Vec<ProviderPresetSelection> {
    let mut options: Vec<ProviderPresetSelection> = rebon_config::provider_presets()
        .iter()
        .map(|preset| ProviderPresetSelection::Preset(preset.id))
        .collect();
    options.push(ProviderPresetSelection::Custom);
    options
}

/// Content rows `render_oauth_view` emits for `view`, used to size the
/// inline host. Mirrors that method line for line — the OAuth sub-views
/// are the tallest thing the `/login` dialog shows, so an inline host
/// sized from the login pane alone would clip them.
fn oauth_view_content_lines(view: &OAuthView) -> usize {
    // "OpenAI account login" + blank, then the per-view body.
    2 + match view {
        OAuthView::StartingBrowser { .. } => 6,
        OAuthView::WaitingCallback { .. } => 7,
        OAuthView::AwaitingPaste { error, .. } => 10 + usize::from(error.is_some()),
        OAuthView::Exchanging => 5,
        OAuthView::Error { .. } => 4,
    }
}

impl OnboardingDialogState {
    pub fn open_with_ui_mode_from(inputs: OnboardingOpenInputs, ui_mode: UiMode) -> Self {
        let has_provider = inputs.setup_status.has_provider_configuration();
        Self::open_with_setup(!has_provider, inputs, false, ui_mode, true)
    }

    /// Open the onboarding dialog from the `/onboarding` slash command.
    /// Unlike first-run onboarding, this always includes the setup step
    /// so users can revisit login/provider guidance later.
    pub fn open_for_command_with_ui_mode_from(
        inputs: OnboardingOpenInputs,
        ui_mode: UiMode,
    ) -> Self {
        Self::open_for_command_with_status_and_ui_mode(inputs, ui_mode)
    }

    pub fn open_for_command_with_status_and_ui_mode(
        inputs: OnboardingOpenInputs,
        ui_mode: UiMode,
    ) -> Self {
        Self::open_with_setup(true, inputs, true, ui_mode, false)
    }

    /// Open the dedicated login pane: the one an authentication failure
    /// reopens so the user can sign in again without the full wizard.
    pub fn open_for_login_pane_from(inputs: OnboardingOpenInputs) -> Self {
        let theme_options = inputs.theme_options;
        let theme_focus = Self::initial_theme_focus(&theme_options, inputs.saved_theme.as_deref());
        let provider_setup_status = inputs.setup_status;
        let snapshot = inputs.providers;
        let selected_provider = snapshot.first().map(|p| p.name.clone());
        Self {
            steps: vec![Step::Provider],
            current_step_index: 0,
            done: false,
            dialog_title: "Login",
            show_welcome: false,
            allow_advance_from_login: false,
            is_theme_only: false,
            ui_mode_applies_immediately: false,
            theme_focus,
            theme_options,
            ui_mode: UiMode::default(),
            setup_pane: SetupPane::LoginMethods,
            provider_setup_status: provider_setup_status.clone(),
            existing_setup_choice: ExistingSetupChoice::Preserve,
            login_from_existing_setup: false,
            login_options: actionable_login_options(),
            login_focus: 0,
            provider_form: ProviderFormState::default(),
            provider_form_mode: ProviderFormMode::Add,
            provider_field_focus: ProviderField::Preset,
            provider_snapshot: snapshot,
            selected_provider,
            provider_ready: provider_setup_status.has_provider_configuration(),
            add_provider_status: PanelStatus::None,
            add_model_status: PanelStatus::None,
            oauth_view: None,
            migration_snapshot: DiscoverySnapshot::default(),
            migration_selected: [false; 5],
            migration_focus: 0,
            migration_status: None,
        }
    }

    /// Open the dedicated `/provider` dialog directly on the custom provider panel.
    pub fn open_for_provider_command_from(inputs: OnboardingOpenInputs) -> Self {
        // The login constructor consumes the inputs, and the panel wants the
        // same provider list one step later.
        let providers = inputs.providers.clone();
        let mut state = Self::open_for_login_pane_from(inputs);
        state.dialog_title = "Provider";
        state.setup_pane = SetupPane::ProviderPanel;
        state.provider_form = ProviderFormState::default();
        state.provider_form_mode = ProviderFormMode::Add;
        state.add_provider_status = PanelStatus::None;
        state.add_model_status = PanelStatus::None;
        state.refresh_provider_snapshot(providers);
        state.provider_field_focus = if state.provider_snapshot.is_empty() {
            state.selected_provider = None;
            ProviderField::Preset
        } else {
            if let Some(snapshot) = state.selected_provider_snapshot().cloned() {
                state.load_provider_form_from_snapshot(&snapshot);
            }
            ProviderField::ProviderList
        };
        state
    }

    /// Open the dedicated `/migrate` importer. Contains only the
    /// Migration step, so a user who installs Claude Code / Codex
    /// content after setup can import it without re-running the whole
    /// wizard. Like the other dedicated commands it leaves the
    /// onboarding-completed flag alone.
    ///
    /// Unlike the wizard, the step is kept even when nothing is
    /// discovered: the user asked for it explicitly and deserves the
    /// "nothing found, here's where I looked" panel instead of a
    /// dialog that opens and immediately closes.
    pub fn open_for_migrate_command_from(inputs: OnboardingOpenInputs) -> Self {
        let migration_snapshot = inputs.migration;
        let mut migration_selected = [false; 5];
        for (idx, category) in ImportCategory::ALL.iter().enumerate() {
            migration_selected[idx] = migration_snapshot.count(*category) > 0;
        }
        let provider_setup_status = inputs.setup_status;
        let snapshot = inputs.providers;
        let selected_provider = snapshot.first().map(|p| p.name.clone());
        let theme_options = inputs.theme_options;
        let theme_focus = Self::initial_theme_focus(&theme_options, inputs.saved_theme.as_deref());
        Self {
            steps: vec![Step::Migration],
            current_step_index: 0,
            done: false,
            dialog_title: "Import",
            show_welcome: false,
            allow_advance_from_login: false,
            is_theme_only: false,
            ui_mode_applies_immediately: false,
            theme_focus,
            theme_options,
            ui_mode: UiMode::default(),
            setup_pane: SetupPane::LoginMethods,
            provider_setup_status: provider_setup_status.clone(),
            existing_setup_choice: ExistingSetupChoice::Preserve,
            login_from_existing_setup: false,
            login_options: actionable_login_options(),
            login_focus: 0,
            provider_form: ProviderFormState::default(),
            provider_form_mode: ProviderFormMode::Add,
            provider_field_focus: ProviderField::Preset,
            provider_snapshot: snapshot,
            selected_provider,
            provider_ready: provider_setup_status.has_provider_configuration(),
            add_provider_status: PanelStatus::None,
            add_model_status: PanelStatus::None,
            oauth_view: None,
            migration_snapshot,
            migration_selected,
            migration_focus: 0,
            migration_status: None,
        }
    }

    /// Open the dedicated `/theme` picker. Contains only the Theme
    /// step; on Enter, the runner persists the selection and closes
    /// the dialog without touching the onboarding-completed flag.
    pub fn open_for_theme_command_from(inputs: OnboardingOpenInputs) -> Self {
        let theme_options = inputs.theme_options;
        let theme_focus = Self::initial_theme_focus(&theme_options, inputs.saved_theme.as_deref());
        let provider_setup_status = inputs.setup_status;
        let snapshot = inputs.providers;
        let selected_provider = snapshot.first().map(|p| p.name.clone());
        Self {
            steps: vec![Step::Theme],
            current_step_index: 0,
            done: false,
            dialog_title: "Theme",
            show_welcome: false,
            allow_advance_from_login: false,
            is_theme_only: true,
            ui_mode_applies_immediately: false,
            theme_focus,
            theme_options,
            ui_mode: UiMode::default(),
            setup_pane: SetupPane::LoginMethods,
            provider_setup_status: provider_setup_status.clone(),
            existing_setup_choice: ExistingSetupChoice::Preserve,
            login_from_existing_setup: false,
            login_options: actionable_login_options(),
            login_focus: 0,
            provider_form: ProviderFormState::default(),
            provider_form_mode: ProviderFormMode::Add,
            provider_field_focus: ProviderField::Preset,
            provider_snapshot: snapshot,
            selected_provider,
            provider_ready: provider_setup_status.has_provider_configuration(),
            add_provider_status: PanelStatus::None,
            add_model_status: PanelStatus::None,
            oauth_view: None,
            migration_snapshot: DiscoverySnapshot::default(),
            migration_selected: [false; 5],
            migration_focus: 0,
            migration_status: None,
        }
    }

    pub fn open_with_setup(
        include_setup: bool,
        inputs: OnboardingOpenInputs,
        guard_existing: bool,
        ui_mode: UiMode,
        ui_mode_applies_immediately: bool,
    ) -> Self {
        let migration_snapshot = inputs.migration;
        // Default every non-empty category to selected so the user
        // only has to press Enter when they don't want to cherry-pick.
        let mut migration_selected = [false; 5];
        for (idx, category) in ImportCategory::ALL.iter().enumerate() {
            migration_selected[idx] = migration_snapshot.count(*category) > 0;
        }

        let mut steps = vec![Step::Theme, Step::UiMode];
        if include_setup {
            steps.push(Step::Provider);
        }
        // Skip the Migration step entirely when there's nothing to
        // migrate — avoids a pointless "Nothing found" panel on fresh
        // machines while still showing it for anyone coming from
        // Claude Code / Codex.
        if migration_snapshot.total() > 0 {
            steps.push(Step::Migration);
        }
        steps.push(Step::Security);
        steps.push(Step::Done);

        let theme_options = inputs.theme_options;
        let theme_focus = Self::initial_theme_focus(&theme_options, inputs.saved_theme.as_deref());
        let provider_setup_status = inputs.setup_status;
        let snapshot = inputs.providers;
        let selected_provider = snapshot.first().map(|p| p.name.clone());
        let setup_pane = if guard_existing && provider_setup_status.should_confirm_reconfiguration()
        {
            SetupPane::ExistingSetup
        } else {
            SetupPane::LoginMethods
        };

        Self {
            steps,
            current_step_index: 0,
            done: false,
            dialog_title: "Setup guide",
            show_welcome: true,
            allow_advance_from_login: true,
            is_theme_only: false,
            ui_mode_applies_immediately,
            theme_focus,
            theme_options,
            ui_mode,
            setup_pane,
            provider_setup_status: provider_setup_status.clone(),
            existing_setup_choice: ExistingSetupChoice::Preserve,
            login_from_existing_setup: false,
            login_options: actionable_login_options(),
            login_focus: 0,
            provider_form: ProviderFormState::default(),
            provider_form_mode: ProviderFormMode::Add,
            provider_field_focus: ProviderField::Preset,
            provider_snapshot: snapshot,
            selected_provider,
            provider_ready: provider_setup_status.has_provider_configuration(),
            add_provider_status: PanelStatus::None,
            add_model_status: PanelStatus::None,
            oauth_view: None,
            migration_snapshot,
            migration_selected,
            migration_focus: 0,
            migration_status: None,
        }
    }

    /// Focus the user's previously saved theme (if any) so that
    /// reopening the dialog highlights the current selection instead
    /// of snapping back to the first option.
    pub fn initial_theme_focus(options: &[(String, ThemeSetting)], saved: Option<&str>) -> usize {
        saved
            .and_then(|saved| {
                options
                    .iter()
                    .position(|(_, setting)| setting.id() == saved)
            })
            .unwrap_or(0)
    }

    pub fn current_step(&self) -> Option<Step> {
        self.steps.get(self.current_step_index).copied()
    }

    pub fn current_step_label(&self) -> Option<&'static str> {
        self.current_step().map(Step::label)
    }

    pub fn dialog_title(&self) -> &'static str {
        self.dialog_title
    }

    /// True when this is the focused `/provider` panel (opened via
    /// [`Self::open_for_provider_command`]). Inline mode hosts this
    /// variant inside the prompt input area instead of taking over the
    /// whole terminal, so the runner keeps it out of the
    /// inline-fullscreen surface set. Screen mode still renders it as a
    /// centered fullscreen overlay like the other onboarding variants.
    pub fn is_provider_panel(&self) -> bool {
        self.dialog_title == "Provider"
    }

    /// True when this is the dedicated login pane (opened via
    /// [`Self::open_for_login_pane`]), as opposed to the multi-step
    /// setup wizard that also owns a Provider step.
    pub fn is_login_pane(&self) -> bool {
        self.dialog_title == "Login"
    }

    /// True when inline mode hosts this dialog inside the prompt input
    /// area instead of switching the terminal to the alternate screen.
    /// Both dedicated command dialogs (`/provider`, `/login`) are small
    /// enough for the prompt host, and taking over the whole terminal
    /// for them costs the user their scrollback. The multi-step wizard
    /// still gets the full screen.
    pub fn is_inline_hosted(&self) -> bool {
        self.is_provider_panel() || self.is_login_pane()
    }

    /// Rows this dialog wants when hosted inline in the prompt area, so
    /// the inline layout grows the host downward to fit the visible pane
    /// instead of taking over the whole viewport. Mirrors the line
    /// counts in the matching `render_*` method plus the border + footer
    /// chrome. The inline host clamps this to the available height, so
    /// an oversized pane simply fills the viewport.
    pub fn inline_host_desired_height(&self) -> u16 {
        let content = match (self.oauth_view.as_ref(), self.setup_pane) {
            (Some(view), _) => oauth_view_content_lines(view),
            (None, SetupPane::ProviderPanel) => return self.provider_panel_desired_height(),
            (None, SetupPane::LoginMethods) => self.login_pane_content_lines(),
            (None, SetupPane::ExistingSetup) => self.existing_setup_content_lines(),
        };
        // border(2) + footer(1); the dedicated dialogs have no welcome row.
        (content + 3).min(u16::MAX as usize) as u16
    }

    /// Content rows `render_login_pane` emits.
    pub fn login_pane_content_lines(&self) -> usize {
        // "Authentication" + "Select a login method:" + blank, the
        // grouped method rows, then blank + hint (+ status banner).
        3 + grouped_login_pane_rows(&self.login_options).len()
            + 2
            + usize::from(!self.add_provider_status.is_none())
    }

    /// Content rows `render_existing_setup_pane` emits.
    pub fn existing_setup_content_lines(&self) -> usize {
        let status = &self.provider_setup_status;
        // Title + subtitle + blank.
        let mut lines = 3;
        lines += usize::from(status.onboarding_completed);
        if status.providers.is_empty() {
            lines += usize::from(status.environment_providers.is_empty());
        } else {
            // "Saved providers:" + up to 4 rows + the "… and N more" line.
            lines += 1 + status.providers.len().min(4) + usize::from(status.providers.len() > 4);
        }
        lines += usize::from(!status.environment_providers.is_empty());
        lines += usize::from(status.config_error.is_some());
        // blank + the two choices + blank + hint (+ status banner).
        lines + 5 + usize::from(!self.add_provider_status.is_none())
    }

    /// Rows the `/provider` panel needs when hosted inline in the prompt
    /// area, so the inline layout grows the host downward to fit the form
    /// instead of taking over the whole viewport (which would scroll the
    /// conversation out of view). Mirrors the line counts in
    /// `render_provider_panel` — the taller of the two side-by-side panes
    /// — plus the border + footer chrome. The inline host clamps this to
    /// the available height, so an oversized form simply fills the screen.
    pub fn provider_panel_desired_height(&self) -> u16 {
        let providers = self.provider_snapshot.len();
        // Left list pane: header(3) + 2 rows/provider (or a "none yet"
        // line) + blank + "Add new" + hint(3).
        let list_lines = 3 + if providers == 0 { 1 } else { 2 * providers } + 3;

        // Right detail pane: the edit form (with the models section) is
        // taller than the add form; count whichever is showing.
        let detail_lines = if let Some(snap) = self.selected_provider_snapshot() {
            let models = snap.models.len().max(1);
            let banners = usize::from(!self.add_provider_status.is_none())
                + usize::from(!self.add_model_status.is_none());
            // header(3) + 5 fields + blank + save + blank + "Models" +
            // models + blank + new-model + blank + add-model = 16 fixed.
            16 + models + banners
        } else {
            let presets = self
                .provider_form
                .preset_options
                .len()
                .min(PRESET_ROWS_VISIBLE);
            let banner = usize::from(!self.add_provider_status.is_none());
            // header/hint/blank/"Quick setup:"(4) + visible presets + blank +
            // 5 fields + blank + save(2) = 12 fixed.
            12 + presets + banner
        };

        // border(2) + footer(1); the provider panel has no welcome row.
        let content = list_lines.max(detail_lines) + 3;
        content.min(u16::MAX as usize) as u16
    }

    /// Switch the already-open provider panel into add mode (empty form,
    /// preset focus). Used when the `/provider` switcher escalates to
    /// "add a new provider".
    pub fn enter_provider_add_mode(&mut self) {
        self.start_add_provider_from_list();
    }

    /// Switch the already-open provider panel into edit mode for the named
    /// provider when it exists in the current snapshot. Returns whether it
    /// was found (falls back to add mode when not).
    pub fn enter_provider_edit_mode(&mut self, name: &str) -> bool {
        if let Some(snapshot) = self
            .provider_snapshot
            .iter()
            .find(|provider| provider.name == name)
            .cloned()
        {
            self.start_edit_provider(&snapshot);
            true
        } else {
            self.start_add_provider_from_list();
            false
        }
    }

    pub fn is_theme_only(&self) -> bool {
        self.is_theme_only
    }

    /// The `/migrate` importer — the only dialog whose step list is
    /// exactly the Migration step (the wizard always leads with Theme).
    pub fn is_migrate_only(&self) -> bool {
        self.steps.as_slice() == [Step::Migration]
    }

    /// Whether finishing this dialog must leave the onboarding-completed
    /// flag alone. Dedicated single-step commands (`/theme`, `/migrate`)
    /// are not "setup ran to completion" — marking them as such would
    /// silently suppress first-run onboarding for a user who never saw it.
    pub fn skips_completion_flag(&self) -> bool {
        self.is_theme_only || self.is_migrate_only()
    }

    pub fn ui_mode_change_requires_restart(&self) -> bool {
        !self.ui_mode_applies_immediately
    }

    pub fn onboarding_source(&self) -> &'static str {
        if self.is_theme_only {
            "slash_theme"
        } else if self.is_migrate_only() {
            "slash_migrate"
        } else if self.dialog_title == "Provider" {
            "slash_provider"
        } else if self.dialog_title == "Login" {
            "slash_login"
        } else {
            "slash_onboarding"
        }
    }

    pub fn current_theme_name(&self) -> ThemeName {
        self.theme_options
            .get(self.theme_focus)
            .and_then(|(_, setting)| ThemeName::from_str(setting.id()))
            .unwrap_or(ThemeName::Dark)
    }

    pub fn advance(&mut self) -> OnboardingStepTransition {
        let previous_step = self.current_step().map(Step::label);
        if self.current_step_index + 1 >= self.steps.len() {
            self.done = true;
        } else {
            self.current_step_index += 1;
        }
        OnboardingStepTransition {
            previous_step,
            next_step: self.current_step().map(Step::label),
        }
    }

    pub fn retreat(&mut self) -> OnboardingStepTransition {
        let previous_step = self.current_step().map(Step::label);
        self.done = false;
        if self.current_step_index > 0 {
            self.current_step_index -= 1;
        }
        OnboardingStepTransition {
            previous_step,
            next_step: self.current_step().map(Step::label),
        }
    }

    pub fn advance_step_outcome(&mut self) -> OnboardingDialogOutcome {
        if matches!(self.current_step(), Some(Step::Theme)) {
            if let Some((_, setting)) = self.theme_options.get(self.theme_focus) {
                let setting = *setting;
                let transition = self.advance();
                return OnboardingDialogOutcome::ThemeSelected {
                    setting,
                    transition,
                };
            }
        }

        if matches!(self.current_step(), Some(Step::UiMode)) {
            let mode = self.ui_mode;
            let transition = self.advance();
            return OnboardingDialogOutcome::UiModeSelected { mode, transition };
        }

        if matches!(self.current_step(), Some(Step::Provider)) && !self.provider_ready {
            self.add_provider_status = PanelStatus::Err(
                "Connect OpenAI or save a custom provider before continuing.".into(),
            );
            return OnboardingDialogOutcome::None;
        }

        let transition = self.advance();
        if self.done {
            OnboardingDialogOutcome::Completed {
                transition: Some(transition),
            }
        } else if transition.previous_step != transition.next_step {
            OnboardingDialogOutcome::Advanced(transition)
        } else {
            OnboardingDialogOutcome::None
        }
    }

    /// Leave the OAuth sub-view and go back to whatever step is underneath.
    ///
    /// Both drivers call it when the user gives up mid-login: it is the one
    /// state change that "cancelled" means, and it has to be the same change
    /// on every front end, which is why it is here and not next to the key
    /// that triggers it.
    pub fn clear_oauth_view(&mut self) {
        self.oauth_view = None;
    }

    /// Insert bracketed-paste text into the active onboarding input.
    /// Returns `true` when an onboarding field owned the paste.
    pub fn handle_paste(&mut self, text: &str) -> bool {
        let pasted = normalize_single_line_paste(text);

        if let Some(OAuthView::AwaitingPaste { input, error, .. }) = self.oauth_view.as_mut() {
            if !pasted.is_empty() {
                input.insert_str(&pasted);
                *error = None;
            }
            return true;
        }
        if self.oauth_view.is_some()
            || !matches!(self.current_step(), Some(Step::Provider))
            || self.setup_pane != SetupPane::ProviderPanel
        {
            return false;
        }

        let edited_field = self.provider_field_focus;
        let field = match edited_field {
            ProviderField::Name => Some(&mut self.provider_form.name),
            ProviderField::ApiKey => Some(&mut self.provider_form.api_key),
            ProviderField::BaseUrl => Some(&mut self.provider_form.base_url),
            ProviderField::Model => Some(&mut self.provider_form.model),
            ProviderField::NewModelInput => Some(&mut self.provider_form.new_model),
            _ => None,
        };
        let Some(field) = field else {
            return false;
        };
        if pasted.is_empty() {
            return true;
        }

        field.insert_str(&pasted);
        if edited_field == ProviderField::NewModelInput {
            self.add_model_status = PanelStatus::None;
        } else {
            self.add_provider_status = PanelStatus::None;
            if matches!(
                edited_field,
                ProviderField::Name | ProviderField::BaseUrl | ProviderField::Model
            ) {
                self.provider_form.clear_preset_coupling();
            }
        }
        true
    }

    pub fn cycle_panel_focus(&mut self, forward: bool) {
        let mut order: Vec<ProviderField> = Vec::new();
        order.push(ProviderField::ProviderList);
        if self.selected_provider.is_none() || self.provider_snapshot.is_empty() {
            order.extend(ProviderField::ADD_FORM_ORDER.iter().copied());
        } else {
            order.extend(ProviderField::FORM_ORDER.iter().copied());
            order.extend(ProviderField::MODELS_ORDER.iter().copied());
        }
        let Some(idx) = order.iter().position(|f| *f == self.provider_field_focus) else {
            self.provider_field_focus = order
                .first()
                .copied()
                .unwrap_or(ProviderField::ProviderList);
            return;
        };
        let len = order.len();
        let next = if forward {
            (idx + 1) % len
        } else {
            (idx + len - 1) % len
        };
        self.provider_field_focus = order[next];
    }

    pub fn provider_list_len(&self) -> usize {
        self.provider_snapshot.len() + 1
    }

    pub fn selected_provider_row(&self) -> usize {
        self.selected_provider
            .as_deref()
            .and_then(|name| self.provider_snapshot.iter().position(|p| p.name == name))
            .unwrap_or(self.provider_snapshot.len())
    }

    pub fn cycle_provider_list_selection(&mut self, forward: bool) {
        let len = self.provider_list_len();
        if len == 0 {
            return;
        }
        let current = self.selected_provider_row();
        let next = if forward {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        if next < self.provider_snapshot.len() {
            let snapshot = self.provider_snapshot[next].clone();
            self.load_provider_form_from_snapshot(&snapshot);
            self.add_model_status = PanelStatus::None;
        } else {
            self.selected_provider = None;
            self.provider_form = ProviderFormState::default();
            self.provider_form_mode = ProviderFormMode::Add;
            self.add_provider_status = PanelStatus::None;
        }
    }

    pub fn start_add_provider_from_list(&mut self) {
        self.selected_provider = None;
        self.provider_form = ProviderFormState::default();
        self.provider_form_mode = ProviderFormMode::Add;
        self.provider_field_focus = ProviderField::Preset;
        self.add_provider_status = PanelStatus::None;
        self.add_model_status = PanelStatus::None;
    }

    pub fn start_edit_selected_provider(&mut self) {
        if let Some(snapshot) = self.selected_provider_snapshot().cloned() {
            self.start_edit_provider(&snapshot);
        }
    }

    pub fn start_edit_provider(&mut self, snapshot: &ProviderSnapshot) {
        self.load_provider_form_from_snapshot(snapshot);
        self.provider_field_focus = ProviderField::Name;
    }

    pub fn load_provider_form_from_snapshot(&mut self, snapshot: &ProviderSnapshot) {
        self.selected_provider = Some(snapshot.name.clone());
        self.provider_form.populate_from_snapshot(snapshot);
        self.provider_form_mode = ProviderFormMode::Edit {
            original_name: snapshot.name.clone(),
        };
        self.add_provider_status = PanelStatus::None;
        self.add_model_status = PanelStatus::None;
    }

    pub fn selected_provider_snapshot(&self) -> Option<&ProviderSnapshot> {
        self.selected_provider
            .as_deref()
            .and_then(|name| self.provider_snapshot.iter().find(|p| p.name == name))
    }

    pub fn submit_save_provider(&mut self) -> OnboardingDialogOutcome {
        let name = self.provider_form.name.value.trim().to_string();
        let api_key = self.provider_form.api_key.value.trim().to_string();
        let base_url = self.provider_form.base_url.value.trim().to_string();
        let format = self.provider_form.format().to_string();
        let model = self.provider_form.model.value.trim().to_string();
        if name.is_empty() {
            self.add_provider_status = PanelStatus::Err("Name cannot be empty.".into());
            self.provider_field_focus = ProviderField::Name;
            return OnboardingDialogOutcome::None;
        }
        if base_url.is_empty() {
            self.add_provider_status = PanelStatus::Err("Base URL cannot be empty.".into());
            self.provider_field_focus = ProviderField::BaseUrl;
            return OnboardingDialogOutcome::None;
        }
        // A preset whose endpoint takes no key (Ollama) is complete without
        // one; the stored entry gets a placeholder so the bearer header is
        // well-formed. Its model list comes from the endpoint, so the
        // model may be left blank too and filled by `/provider models`.
        let keyless_preset = self
            .provider_form
            .selected_provider_preset()
            .is_some_and(|preset| !preset.api_key_required);
        if api_key.is_empty() && !keyless_preset {
            self.add_provider_status = PanelStatus::Err("API key cannot be empty.".into());
            self.provider_field_focus = ProviderField::ApiKey;
            return OnboardingDialogOutcome::None;
        }
        if model.is_empty() && !keyless_preset {
            self.add_provider_status = PanelStatus::Err("Initial model cannot be empty.".into());
            self.provider_field_focus = ProviderField::Model;
            return OnboardingDialogOutcome::None;
        }
        if let Some(original_name) = self.provider_form_mode.original_name() {
            return OnboardingDialogOutcome::UpdateProvider {
                original_name: original_name.to_string(),
                name,
                api_key,
                base_url,
                format,
                model,
            };
        }
        let preset_id = self.provider_form.selected_preset_id().map(str::to_string);
        OnboardingDialogOutcome::AddProvider {
            preset_id,
            name,
            api_key,
            base_url,
            format,
            model,
        }
    }

    pub fn submit_add_model(&mut self) -> OnboardingDialogOutcome {
        let Some(provider_name) = self.selected_provider.clone() else {
            self.add_model_status = PanelStatus::Err("Choose an existing provider first.".into());
            self.provider_field_focus = ProviderField::ProviderList;
            return OnboardingDialogOutcome::None;
        };
        let model = self.provider_form.new_model.value.trim().to_string();
        if model.is_empty() {
            self.add_model_status = PanelStatus::Err("Model cannot be empty.".into());
            return OnboardingDialogOutcome::None;
        }
        OnboardingDialogOutcome::AddProviderModel {
            provider_name,
            model,
        }
    }

    /// Runner calls this after completing the `AddProvider` outcome.
    /// On success the new provider becomes selected in the provider list. On error the banner
    /// surfaces the message and the form keeps its values.
    pub fn report_add_provider_result(
        &mut self,
        providers: Vec<ProviderSnapshot>,
        result: Result<String, String>,
    ) -> Option<OnboardingStepTransition> {
        match result {
            Ok(name) => {
                self.add_provider_status = PanelStatus::Ok(format!(
                    "{} saved and activated; connection will be checked on first request.",
                    self.provider_form.success_subject(&name)
                ));
                self.provider_form.clear_after_add();
                self.provider_form_mode = ProviderFormMode::Add;
                self.refresh_provider_snapshot(providers);
                self.selected_provider = Some(name);
                self.provider_ready = true;
                self.provider_field_focus = ProviderField::ProviderList;
                if self.allow_advance_from_login
                    && matches!(self.current_step(), Some(Step::Provider))
                    && self.current_step_index + 1 < self.steps.len()
                {
                    let transition = self.advance();
                    if transition.previous_step != transition.next_step {
                        return Some(transition);
                    }
                }
            }
            Err(msg) => {
                self.add_provider_status = PanelStatus::Err(msg);
                self.provider_field_focus = ProviderField::SaveProviderButton;
            }
        }
        None
    }

    /// Runner calls this after completing the `UpdateProvider` outcome.
    pub fn report_update_provider_result(
        &mut self,
        providers: Vec<ProviderSnapshot>,
        result: Result<String, String>,
    ) -> Option<OnboardingStepTransition> {
        match result {
            Ok(name) => {
                self.add_provider_status = PanelStatus::Ok(format!(
                    "Provider \"{name}\" saved and activated; connection will be checked on first request."
                ));
                self.refresh_provider_snapshot(providers);
                self.selected_provider = Some(name.clone());
                self.provider_ready = true;
                if let Some(snapshot) = self.selected_provider_snapshot().cloned() {
                    self.provider_form.populate_from_snapshot(&snapshot);
                    self.provider_form_mode = ProviderFormMode::Edit {
                        original_name: snapshot.name,
                    };
                } else {
                    self.provider_form.name.set(&name);
                    self.provider_form_mode = ProviderFormMode::Edit {
                        original_name: name,
                    };
                }
                self.provider_field_focus = ProviderField::ProviderList;
                if self.allow_advance_from_login
                    && matches!(self.current_step(), Some(Step::Provider))
                    && self.current_step_index + 1 < self.steps.len()
                {
                    let transition = self.advance();
                    if transition.previous_step != transition.next_step {
                        return Some(transition);
                    }
                }
            }
            Err(msg) => {
                self.add_provider_status = PanelStatus::Err(msg);
                self.provider_field_focus = ProviderField::SaveProviderButton;
            }
        }
        None
    }

    /// Runner calls this after completing the `AddProviderModel`
    /// outcome. See [`Self::report_add_provider_result`].
    pub fn report_add_model_result(
        &mut self,
        providers: Vec<ProviderSnapshot>,
        result: Result<(String, String), String>,
    ) {
        match result {
            Ok((provider, model)) => {
                self.add_model_status =
                    PanelStatus::Ok(format!("Added model \"{model}\" to \"{provider}\"."));
                self.provider_form.new_model.clear();
                self.refresh_provider_snapshot(providers);
                self.selected_provider = Some(provider);
                if let Some(snapshot) = self.selected_provider_snapshot().cloned() {
                    self.provider_form.populate_from_snapshot(&snapshot);
                    self.provider_form_mode = ProviderFormMode::Edit {
                        original_name: snapshot.name,
                    };
                }
            }
            Err(msg) => {
                self.add_model_status = PanelStatus::Err(msg);
            }
        }
    }

    pub fn refresh_provider_snapshot(&mut self, providers: Vec<ProviderSnapshot>) {
        self.provider_snapshot = providers;
        if let Some(name) = self.selected_provider.as_ref() {
            if !self.provider_snapshot.iter().any(|p| p.name == *name) {
                self.selected_provider = if self.provider_snapshot.is_empty() {
                    None
                } else {
                    self.provider_snapshot.first().map(|p| p.name.clone())
                };
            }
        }
        if self.selected_provider.is_none() {
            self.provider_form_mode = ProviderFormMode::Add;
        }
    }

    /// Runner calls this once PKCE is prepared + the browser launch
    /// has been attempted. `port_available` decides whether the
    /// dialog moves through `StartingBrowser` → `WaitingCallback`
    /// (listener path) or `StartingBrowser` → `AwaitingPaste`
    /// (paste-fallback path, when port 1455 was busy).
    pub fn report_oauth_prepared(&mut self, authorize_url: String, port_available: bool) {
        let view = if port_available {
            OAuthView::StartingBrowser { authorize_url }
        } else {
            OAuthView::AwaitingPaste {
                authorize_url,
                input: TextField::default(),
                error: None,
            }
        };
        self.oauth_view = Some(view);
    }

    /// Transition to `WaitingCallback`. Runner calls this once the
    /// local listener is armed on port 1455 and we're holding for
    /// the browser to round-trip.
    pub fn report_oauth_listening(&mut self) {
        let authorize_url = match &self.oauth_view {
            Some(OAuthView::StartingBrowser { authorize_url })
            | Some(OAuthView::WaitingCallback { authorize_url }) => authorize_url.clone(),
            Some(OAuthView::AwaitingPaste { authorize_url, .. }) => authorize_url.clone(),
            _ => return,
        };
        self.oauth_view = Some(OAuthView::WaitingCallback { authorize_url });
    }

    /// Transition to `AwaitingPaste` mid-flight. Used when the
    /// listener errored out (port 1455 grabbed after probe, OS
    /// socket error, etc.) and the runner wants to fall back to the
    /// manual paste path without dumping the whole flow.
    pub fn report_oauth_paste_required(&mut self) {
        let authorize_url = match &self.oauth_view {
            Some(OAuthView::StartingBrowser { authorize_url })
            | Some(OAuthView::WaitingCallback { authorize_url }) => authorize_url.clone(),
            Some(OAuthView::AwaitingPaste { authorize_url, .. }) => authorize_url.clone(),
            _ => return,
        };
        self.oauth_view = Some(OAuthView::AwaitingPaste {
            authorize_url,
            input: TextField::default(),
            error: None,
        });
    }

    /// Transition to `Exchanging`. Runner calls this right before it
    /// POSTs the authorization code to the token endpoint.
    pub fn report_oauth_exchanging(&mut self) {
        self.oauth_view = Some(OAuthView::Exchanging);
    }

    /// Mark the OAuth flow as successful. Clears the sub-view, marks
    /// the provider step as satisfied for setup-step purposes by
    /// setting `add_provider_status` to an OK banner, and advances
    /// the wizard if it's a multi-step run.
    pub fn report_oauth_success(
        &mut self,
        providers: Vec<ProviderSnapshot>,
    ) -> Option<OnboardingStepTransition> {
        self.oauth_view = None;
        self.setup_pane = SetupPane::LoginMethods;
        self.provider_ready = true;
        self.refresh_provider_snapshot(providers);
        self.add_provider_status = PanelStatus::Ok("OpenAI account connected.".into());
        // Mirror the multi-step advance that `AddProvider` does on
        // success — in wizard mode (Theme → Provider → Security)
        // this moves us to Security; in the stand-alone /login
        // dialog it's a no-op.
        if self.allow_advance_from_login {
            let transition = self.advance();
            if transition.previous_step != transition.next_step {
                return Some(transition);
            }
        }
        None
    }

    /// Surface an OAuth flow error. `can_retry = true` means Enter
    /// will restart the flow from the top (prepare + browser);
    /// `false` locks the view until the user hits Esc.
    pub fn report_oauth_error(&mut self, message: String, can_retry: bool) {
        self.oauth_view = Some(OAuthView::Error { message, can_retry });
    }

    /// Update the paste-input error banner without clearing the
    /// typed text — used when
    /// [`OnboardingDialogOutcome::SubmitOpenAIOAuthPaste`] was
    /// accepted by the dialog but the runner rejected it (state
    /// mismatch, parse failure).
    pub fn report_oauth_paste_error(&mut self, message: String) {
        if let Some(OAuthView::AwaitingPaste { error, .. }) = self.oauth_view.as_mut() {
            *error = Some(message);
        }
    }

    pub fn selected_categories(&self) -> Vec<ImportCategory> {
        ImportCategory::ALL
            .iter()
            .enumerate()
            .filter(|(idx, _)| self.migration_selected[*idx])
            .map(|(_, c)| *c)
            .collect()
    }

    /// Runner calls this after executing [`OnboardingDialogOutcome::RunMigration`]
    /// to surface the summary and stop the spinner.
    pub fn report_migration_result(&mut self, result: Result<ImportSummary, String>) {
        self.migration_status = Some(result);
    }

    /// Read-only access to the discovered source files so the runner
    /// can invoke [`crate::migrate::perform_migration`] without
    /// re-scanning the disk.
    pub fn migration_snapshot(&self) -> &DiscoverySnapshot {
        &self.migration_snapshot
    }

    pub fn footer_text(&self) -> String {
        if let Some(view) = &self.oauth_view {
            return match view {
                OAuthView::StartingBrowser { .. } => "Esc cancel".to_string(),
                OAuthView::WaitingCallback { .. } => "Esc cancel".to_string(),
                OAuthView::AwaitingPaste { .. } => "Enter submit · Esc cancel".to_string(),
                OAuthView::Exchanging => "…".to_string(),
                OAuthView::Error { can_retry, .. } => {
                    if *can_retry {
                        "Enter retry · Esc back".to_string()
                    } else {
                        "Esc back".to_string()
                    }
                }
            };
        }
        match self.current_step() {
            Some(Step::Theme) => {
                if self.is_theme_only {
                    "Up/Down preview · Enter save · Esc close".to_string()
                } else {
                    "Up/Down preview · Enter/Right save and continue · Esc close".to_string()
                }
            }
            Some(Step::UiMode) => {
                "Up/Down select · Enter/Right save and continue · Left back · Esc close".to_string()
            }
            Some(Step::Provider) => match self.setup_pane {
                SetupPane::ExistingSetup => {
                    "Up/Down move · Enter/Right confirm · Left back · Esc close".to_string()
                }
                SetupPane::LoginMethods => {
                    "Up/Down move · Enter select · Right continue when ready · Left back · Esc close"
                        .to_string()
                }
                SetupPane::ProviderPanel => {
                    let escape = if self.dialog_title == "Provider" {
                        "Esc close"
                    } else {
                        "Esc back"
                    };
                    format!(
                        "Up/Down select · Enter edit/confirm · Tab/Shift+Tab move · {escape}"
                    )
                }
            },
            Some(Step::Migration) => {
                if self.migration_status.is_some() {
                    "R retry · Enter/Right continue · Left back · Esc close".to_string()
                } else {
                    "Up/Down move · Space toggle · Enter import/skip · Left back · Esc close"
                        .to_string()
                }
            }
            Some(Step::Security) => "Enter/Right continue · Left back · Esc close".to_string(),
            Some(Step::Done) => "Enter finish · Left review · Esc close".to_string(),
            None => "Esc close".to_string(),
        }
    }
}

//! What the wizard hands back to whichever surface is driving it.
//!
//! Data only, and deliberately so: the three drivers (the in-session
//! dialog, the pre-startup loop, and the OAuth driver) each match on
//! these variants and perform the writes themselves, so the on-disk
//! effects of `/onboarding` are identical whichever one ran it.

use crate::migrate::ImportCategory;
use rebon_picker::theme_picker::ThemeSetting;

use rebon_config::UiMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnboardingStepTransition {
    pub previous_step: Option<&'static str>,
    pub next_step: Option<&'static str>,
}

/// What the runner should do after a key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnboardingDialogOutcome {
    /// No side effect.
    None,
    /// Close the dialog (user pressed Esc — onboarding NOT completed).
    Close,
    /// All steps completed naturally. Runner should mark onboarding
    /// done in config. Contains the transition that completed the
    /// dialog when completion was caused by an `advance()` call.
    Completed {
        transition: Option<OnboardingStepTransition>,
    },
    /// User advanced from one onboarding step to another without any
    /// other runner side effect.
    Advanced(OnboardingStepTransition),
    /// User moved focus to a theme. Runner should preview it.
    ThemePreview(ThemeSetting),
    /// User selected a theme. Runner should apply it.
    ThemeSelected {
        setting: ThemeSetting,
        transition: OnboardingStepTransition,
    },
    /// User selected how the TUI should use the terminal. Runner should
    /// persist the setting before continuing.
    UiModeSelected {
        mode: UiMode,
        transition: OnboardingStepTransition,
    },
    /// User submitted the Add Provider form. Runner should persist the
    /// provider via [`rebon_config::add_custom_provider`] and
    /// call [`super::state::OnboardingDialogState::report_add_provider_result`] so
    /// the dialog can show success or the error and stay open.
    AddProvider {
        preset_id: Option<String>,
        name: String,
        api_key: String,
        base_url: String,
        format: String,
        model: String,
    },
    /// User submitted edits for an existing provider. Runner should
    /// persist via [`rebon_config::update_custom_provider`] and
    /// call [`super::state::OnboardingDialogState::report_update_provider_result`].
    UpdateProvider {
        original_name: String,
        name: String,
        api_key: String,
        base_url: String,
        format: String,
        model: String,
    },
    /// User submitted the Add Model form. Runner should persist via
    /// [`rebon_config::add_custom_provider_model`] and call
    /// [`super::state::OnboardingDialogState::report_add_provider_result`].
    AddProviderModel {
        provider_name: String,
        model: String,
    },
    /// User picked "OpenAI account" in LoginMethods. Runner should
    /// spawn the OAuth flow (prepare → launch browser → start
    /// listener) via [`crate::oauth::flow`] and then nudge the
    /// dialog through the sub-views with the `report_oauth_*`
    /// methods.
    StartOpenAIOAuth,
    /// User submitted a pasted callback URL / query string /
    /// `code#state` blob from the `AwaitingPaste` sub-view. Runner
    /// should parse via [`crate::oauth::flow::parse_pasted_callback`]
    /// against the stashed PKCE challenge, then run
    /// [`crate::oauth::flow::exchange_and_persist`] and call
    /// [`super::state::OnboardingDialogState::report_oauth_error`] or
    /// [`super::state::OnboardingDialogState::report_oauth_success`] accordingly.
    SubmitOpenAIOAuthPaste(String),
    /// User hit Esc in any OAuth sub-view. Runner should drop the
    /// listener / in-flight request and discard the PKCE challenge.
    CancelOpenAIOAuth,
    /// User confirmed the Migration step with a set of categories
    /// selected. Runner should run
    /// [`crate::migrate::perform_migration`] against
    /// `config_home_dir()` and call
    /// [`super::state::OnboardingDialogState::report_migration_result`] with the
    /// summary so the dialog can show it and advance.
    RunMigration(Vec<ImportCategory>),
}

//! What the wizard reads off disk before it opens, in one place.
//!
//! The state machine itself takes an [`OnboardingOpenInputs`] and never looks
//! anything up, which is what lets it be exercised without a config directory.
//! Six entry points build that value, and they all build the same one — so it
//! is built here rather than six times, and the six constructors below are the
//! whole of what a caller needs to know.

use crate::migrate::{self as migration_import, DiscoverySnapshot};
use rebon_config::UiMode;
use rebon_picker::theme_picker::{self, ThemeSetting};

use crate::dialog::state::{OnboardingDialogState, OnboardingOpenInputs, ProviderSnapshot};

/// Everything the wizard needs read off disk, gathered in one place.
pub fn onboarding_open_inputs() -> OnboardingOpenInputs {
    let theme_options: Vec<(String, ThemeSetting)> = theme_picker::build_options(false)
        .into_iter()
        .map(|option| (option.label, option.value))
        .collect();
    OnboardingOpenInputs {
        setup_status: rebon_config::provider_setup_status(),
        providers: load_provider_snapshot(),
        theme_options,
        saved_theme: rebon_config::saved_theme(),
        migration: migration_import::home_dir()
            .map(|home| DiscoverySnapshot::discover(&home))
            .unwrap_or_default(),
    }
}

impl OnboardingDialogState {
    /// First-run onboarding.
    pub fn open_with_ui_mode(ui_mode: UiMode) -> Self {
        Self::open_with_ui_mode_from(onboarding_open_inputs(), ui_mode)
    }

    /// `/onboarding`, which always includes the setup step.
    pub fn open_for_command_with_ui_mode(ui_mode: UiMode) -> Self {
        Self::open_for_command_with_ui_mode_from(onboarding_open_inputs(), ui_mode)
    }

    /// `/login`.
    pub fn open_for_login_pane() -> Self {
        Self::open_for_login_pane_from(onboarding_open_inputs())
    }

    /// `/provider`, opening straight on the custom provider panel.
    pub fn open_for_provider_command() -> Self {
        Self::open_for_provider_command_from(onboarding_open_inputs())
    }

    /// `/migrate`.
    pub fn open_for_migrate_command() -> Self {
        Self::open_for_migrate_command_from(onboarding_open_inputs())
    }

    /// `/theme`.
    pub fn open_for_theme_command() -> Self {
        Self::open_for_theme_command_from(onboarding_open_inputs())
    }
}

/// Whether the user has any model provider available (env var or custom
/// provider in config). When false, the setup step is included in onboarding.
pub fn has_any_provider() -> bool {
    rebon_config::provider_setup_status().has_provider_configuration()
}

/// The configured custom providers, in the shape the wizard's provider panel
/// lists them.
///
/// A provider with an empty `models` list but a non-empty `model` shows that
/// one model, because that is the older single-model spelling of the same
/// fact; a provider with neither shows none.
pub fn load_provider_snapshot() -> Vec<ProviderSnapshot> {
    rebon_config::list_custom_providers()
        .into_iter()
        .map(|info| {
            let models = if info.models.is_empty() {
                if info.model.is_empty() {
                    Vec::new()
                } else {
                    vec![info.model.clone()]
                }
            } else {
                info.models
            };
            ProviderSnapshot {
                name: info.name,
                format: info.format,
                base_url: info.base_url,
                api_key: info.api_key,
                active_model: info.model,
                models,
            }
        })
        .collect()
}

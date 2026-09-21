//! Writing the wizard's answers down, and telling the wizard what happened.
//!
//! Three of [`OnboardingDialogOutcome`](crate::dialog::OnboardingDialogOutcome)'s
//! variants are a provider write: add one, edit one, add a model to one. Each
//! is the same three steps — call `rebon-config`, re-read the provider list,
//! hand the result back to the wizard so its panel shows it — and each was
//! written out twice, once in the pre-startup loop and once in the in-session
//! dialog, because those are two event loops rather than two features.
//!
//! Two copies of a write path is one copy too many: the on-disk effect of
//! `/onboarding` has to be the same whichever loop the user reached it
//! through, and the only way to be sure of that is for there to be one path.
//! So the write lives here, and each loop calls it and reacts to what it
//! returns.
//!
//! What is *not* here is the reacting: closing the dialog, firing a hook,
//! repainting a theme. Those need an `AppState`, a session and a frame, and
//! they differ between the two loops on purpose.

use crate::dialog::{OnboardingDialogState, OnboardingStepTransition};
use crate::store::load_provider_snapshot;

/// Add a provider — from a preset when `preset_id` is set, from the typed
/// fields otherwise — and report the result into the wizard.
///
/// Returns whether a provider now exists that did not before (the pre-startup
/// loop uses it to decide whether startup can continue) and the step the
/// wizard moved to, if it moved.
pub fn apply_add_provider(
    state: &mut OnboardingDialogState,
    preset_id: Option<&str>,
    name: &str,
    format: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> (bool, Option<OnboardingStepTransition>) {
    let result = match preset_id {
        Some(preset_id) => rebon_config::add_custom_provider_from_preset(preset_id, api_key),
        None => rebon_config::add_custom_provider(name, format, base_url, api_key, model),
    }
    .map(|info| info.name)
    .map_err(|err| err.to_string());
    let added = result.is_ok();
    let transition = state.report_add_provider_result(load_provider_snapshot(), result);
    (added, transition)
}

/// Edit an existing provider in place and report the result into the wizard.
pub fn apply_update_provider(
    state: &mut OnboardingDialogState,
    original_name: &str,
    name: &str,
    format: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Option<OnboardingStepTransition> {
    let result =
        rebon_config::update_custom_provider(original_name, name, format, base_url, api_key, model)
            .map(|info| info.name)
            .map_err(|err| err.to_string());
    state.report_update_provider_result(load_provider_snapshot(), result)
}

/// Add one model to an existing provider and report the result into the
/// wizard. No transition: adding a model never advances a step.
pub fn apply_add_provider_model(
    state: &mut OnboardingDialogState,
    provider_name: &str,
    model: &str,
) {
    let result = rebon_config::add_custom_provider_model(provider_name, model)
        .map(|_| (provider_name.to_string(), model.to_string()))
        .map_err(|err| err.to_string());
    state.report_add_model_result(load_provider_snapshot(), result);
}

/// Run the import the user ticked boxes for, against the snapshot the wizard
/// discovered when it opened, and report the summary into it.
///
/// The summary is returned as well as reported, because one of the two hosts
/// logs it and the other does not, and which one logs is not a fact about the
/// import.
pub fn apply_run_migration(
    state: &mut OnboardingDialogState,
    categories: &[crate::migrate::ImportCategory],
) -> crate::migrate::ImportSummary {
    let config_home = rebon_config::config_home_dir();
    let snapshot = state.migration_snapshot.clone();
    let summary = crate::migrate::perform_migration(&config_home, &snapshot, categories);
    state.report_migration_result(Ok(summary.clone()));
    summary
}

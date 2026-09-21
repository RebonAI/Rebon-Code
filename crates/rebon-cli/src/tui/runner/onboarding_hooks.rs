use tokio::runtime::Handle;

use crate::tui::wiring::TuiEngineSession;
use rebon_plugin_onboarding::{OnboardingDialogState, OnboardingStepTransition};

pub(super) fn fire_onboarding_opened(
    session: &TuiEngineSession,
    handle: &Handle,
    dialog: &OnboardingDialogState,
    source: &str,
) {
    fire_onboarding_event(
        session,
        handle,
        "opened",
        source,
        dialog.current_step_label().map(str::to_string),
        None,
        None,
        None,
        Some(dialog.dialog_title().to_string()),
        Some(dialog.is_theme_only()),
    );
}

pub(super) fn fire_onboarding_advanced(
    session: &TuiEngineSession,
    handle: &Handle,
    dialog: &OnboardingDialogState,
    transition: OnboardingStepTransition,
) {
    fire_onboarding_event(
        session,
        handle,
        "advanced",
        dialog.onboarding_source(),
        dialog.current_step_label().map(str::to_string),
        transition.previous_step.map(str::to_string),
        transition.next_step.map(str::to_string),
        None,
        Some(dialog.dialog_title().to_string()),
        Some(dialog.is_theme_only()),
    );
}

pub(super) fn fire_onboarding_closed(
    session: &TuiEngineSession,
    handle: &Handle,
    dialog: &OnboardingDialogState,
    outcome: &str,
) {
    fire_onboarding_event(
        session,
        handle,
        "closed",
        dialog.onboarding_source(),
        dialog.current_step_label().map(str::to_string),
        None,
        None,
        Some(outcome.to_string()),
        Some(dialog.dialog_title().to_string()),
        Some(dialog.is_theme_only()),
    );
}

pub(super) fn fire_onboarding_completed(
    session: &TuiEngineSession,
    handle: &Handle,
    dialog: &OnboardingDialogState,
) {
    fire_onboarding_event(
        session,
        handle,
        "completed",
        dialog.onboarding_source(),
        dialog.current_step_label().map(str::to_string),
        None,
        None,
        Some("completed".to_string()),
        Some(dialog.dialog_title().to_string()),
        Some(dialog.is_theme_only()),
    );
}

#[allow(clippy::too_many_arguments)]
fn fire_onboarding_event(
    session: &TuiEngineSession,
    handle: &Handle,
    phase: &str,
    source: &str,
    step: Option<String>,
    previous_step: Option<String>,
    next_step: Option<String>,
    outcome: Option<String>,
    dialog_title: Option<String>,
    theme_only: Option<bool>,
) {
    let verdict = handle.block_on(session.engine_half.runtime.policy.emit(
        rebon_core::policy_seat::HookEventPayload::Onboarding {
            phase: phase.to_string(),
            source: source.to_string(),
            step,
            previous_step,
            next_step,
            outcome,
            dialog_title,
            theme_only,
        },
    ));
    // A notification event: the wizard has no branch an effect could change.
    if !verdict.effects().is_empty() {
        tracing::debug!(
            phase,
            source,
            effect_count = verdict.effects().len(),
            "Onboarding hook effects ignored"
        );
    }
}

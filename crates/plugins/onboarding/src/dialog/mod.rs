//! The onboarding wizard with no terminal attached.
//!
//! `/onboarding`, `/login`, `/provider`, `/theme` and `/migrate` all open
//! the same wizard, and three surfaces drive it: the in-session dialog,
//! the blocking pre-startup loop, and the OAuth driver. What each of them
//! shares is the step machine and the answers it hands back, which is
//! what lives here.
//!
//! Reading configuration is the caller's job. A constructor is handed the
//! provider setup status and the saved theme rather than looking them up,
//! which is what lets the whole machine be exercised without touching a
//! config directory -- the rule `rebon_dialog::provider_dialog` states for
//! its own rows.
//!
//! Drawing the wizard, and turning a keystroke into a step, stay in the
//! terminal half. That half reads
//! and writes these fields directly, which is why they are `pub` rather than
//! private with accessors: the split is drawn between "what the wizard knows"
//! and "what a frame looks like", not between two owners of the same state.
//!
//! What is deliberately *not* here is the key handling. Its guards read
//! modifier bits (`Ctrl+u` clears a field; a bare arrow steps the wizard but
//! a modified one does not) that no frontend-agnostic key vocabulary in this
//! repo carries, and translating through one that drops them would change
//! behaviour silently. It stays with the frame it belongs to.

pub mod outcome;
pub mod state;

pub use outcome::{OnboardingDialogOutcome, OnboardingStepTransition};
pub use state::{
    login_outcome, ExistingSetupChoice, OAuthView, OnboardingDialogState, OnboardingOpenInputs,
    PanelStatus, ProviderFormState, ProviderPresetSelection, ProviderSnapshot, PROVIDER_FORMATS,
};

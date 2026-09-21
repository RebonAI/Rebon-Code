//! Wiring `ProfileSwitch` / `ProfileSave` into the terminal's permission flow.
//!
//! The decision is [`crate::session::profile_proposal`]'s, shared with the
//! background worker. What is left here is the two things that are this front
//! end's alone: which handles the proposal is resolved against
//! ([`TuiProfileSession`]), and where a provider/model re-resolve is parked
//! until the event loop has the `&mut` session and the tokio handle it needs.

use serde_json::Value;

use rebon_core::permission::OutboundPermissionQuery;
use rebon_plugin_profile::ProfileProposal;

use crate::session::profile_proposal::{
    apply_approved_proposal as apply_proposal, resolve_profile_proposal as resolve_proposal,
    ProfileProposalOutcome,
};
use crate::tui::app::AppState;
use crate::tui::wiring::TuiEngineSession;

use super::profile_command::{runtime_update, TuiProfileSession};

pub(super) use crate::session::profile_proposal::{approved_answer, not_applied_answer};

/// Intercept a profile proposal on its way to the modal.
///
/// Returns `None` when the call was refused outright — the model has already
/// been told why and no prompt is raised. Otherwise the query comes back with
/// the resolved proposal attached to its metadata, which is what
/// `build_pending_permission` turns into a
/// [`crate::tui::permission_modal::PermissionKind::Profile`].
pub(super) fn resolve_profile_proposal(
    app: &mut AppState,
    session: &TuiEngineSession,
    outbound: OutboundPermissionQuery,
) -> Option<OutboundPermissionQuery> {
    let config_dir = crate::rebon_config::config_home_dir();
    let outcome = {
        let mut target = TuiProfileSession::new(app, session);
        resolve_proposal(&mut target, &config_dir, outbound)
    };
    match outcome {
        ProfileProposalOutcome::Prompt(outbound) => Some(outbound),
        ProfileProposalOutcome::Refused => {
            app.pending_permission_view = None;
            None
        }
    }
}

/// Carry out an approved proposal, parking the runtime re-resolve the event
/// loop has to perform.
///
/// The returned value is what [`approved_answer`] hands back to the tool, and
/// the only thing that makes its `call()` report success.
pub(super) fn apply_approved_proposal(
    app: &mut AppState,
    session: Option<&TuiEngineSession>,
    proposal: &ProfileProposal,
) -> Result<Value, String> {
    let config_dir = crate::rebon_config::config_home_dir();
    let applied = match session {
        Some(session) => {
            let mut target = TuiProfileSession::new(app, session);
            apply_proposal(Some(&mut target), &config_dir, proposal)
        }
        None => apply_proposal(None, &config_dir, proposal),
    };
    // Parked before the verdict is read: a partial apply may already have
    // moved the provider on disk, and a session left on the old client would
    // fail every request after a failure it was told was only partial.
    //
    // The re-resolve itself needs `&mut` on the session and a tokio handle,
    // and this is a callback that holds neither. The event loop performs it on
    // its next pass.
    if let Some(refresh) = applied.runtime_refresh {
        app.pending_profile_runtime_refresh = Some(runtime_update(refresh));
    }
    applied.result
}

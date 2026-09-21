//! Putting a live session onto a different provider/model.
//!
//! Three surfaces move a session's runtime after it was built — the terminal
//! draining an approved `ProfileSwitch`, a worker picking up the model its job
//! record names, and a background profile approval — and all three mean the
//! same twelve field writes. They lived as three copies; the one that drifted
//! first is the one that matters, because a session left half-moved keeps
//! talking to the old client while every surface says it moved.
//!
//! What is *not* here is what each surface does about a failure. The terminal
//! writes a transcript line, the worker writes a job event; both need the
//! reason, so this hands it back rather than reporting it.

use crate::rebon_config::RuntimeOverride;
use crate::EngineSession;

/// Install an already-resolved runtime on `session`.
///
/// Every field the runtime owns moves together: the client and retry
/// notifier on the owner's half, and the model block through
/// `SessionModel::adopt`, which includes the shared `runtime_model` cell the
/// executor reads — a session that kept the old cell would keep sending the
/// old model's requests through the new client.
pub(crate) fn install_resolved_runtime(
    session: &mut EngineSession,
    runtime: rebon_harness::RuntimeModel,
) {
    runtime.publish_provider_capabilities();
    session.engine_half.client = runtime.client.clone();
    session.engine_half.retry_notifier = runtime.retry_notifier.clone();
    session.model.adopt(&runtime);
}

/// Re-resolve `overrides` and install the result, or report why not.
///
/// A resolve that fails leaves the session exactly as it was — still on a
/// client that works — because a session with no client at all is worse than
/// one on the wrong model.
pub async fn resolve_and_install_runtime(
    session: &mut EngineSession,
    overrides: RuntimeOverride,
) -> Result<(), String> {
    match crate::build::resolve_runtime_model(overrides).await {
        Ok(runtime) => {
            rebon_session::model_selection::save_manual_model(
                &session.engine_half.runtime.projects_root,
                &session.cwd,
                &session.session_id,
                &runtime.provider_name,
                &runtime.model,
            )
            .map_err(|error| format!("cannot persist session model selection: {error}"))?;
            install_resolved_runtime(session, runtime);
            Ok(())
        }
        Err(err) => Err(err.to_string()),
    }
}

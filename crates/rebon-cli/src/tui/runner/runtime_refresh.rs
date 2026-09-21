use tokio::runtime::Handle;

use crate::tui::app::AppState;
use crate::tui::wiring::TuiEngineSession;

use super::inject_system_message;
use crate::session::commands::effort::provider_kind_from_format;
use crate::session::commands::provider::ProviderRuntimeUpdate;

pub(super) fn refresh_runtime_model(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    update: Option<ProviderRuntimeUpdate>,
    handle: &Handle,
) -> bool {
    let Some(update) = update else {
        return true;
    };
    match handle.block_on(
        crate::session::runtime_refresh::resolve_and_install_runtime(
            &mut session.session,
            crate::rebon_config::RuntimeOverride::default(),
        ),
    ) {
        Ok(()) => {
            app.effort_provider_kind = provider_kind_from_format(session.model.provider_format);
            app.refresh_empty_startup_banner();
            true
        }
        Err(err) => {
            inject_system_message(
                app,
                "error",
                &format!("Failed to switch runtime provider/model: {err}"),
            );
            session.model.provider_name = update.provider_name;
            session.model.name = update.model_name;
            app.follow_transcript_tail = true;
            false
        }
    }
}

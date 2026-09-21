//! `/profile` in the terminal — the handles, and nothing else.
//!
//! [`rebon_plugin_profile`] owns the command: parsing it, editing one field, the
//! order the surfaces are applied in, and every sentence the user reads. What
//! is left here is the part that is genuinely this front end's — which handle
//! is which, where the permission mirror the engine's broker reads lives, and
//! re-resolving the runtime after provider/model move, which needs a tokio
//! handle the shared layer does not have.

use tokio::runtime::Handle;

use rebon_permissions::PermissionMode;
use rebon_plugin_profile::{ProfileCommandResult, ProfileSession, RuntimeRefresh};
use rebon_tool::ToolFilter;

use crate::tui::app::AppState;
use crate::tui::wiring::TuiEngineSession;

/// This session, seen through the six methods a profile needs.
pub(super) struct TuiProfileSession<'a> {
    pub app: &'a mut AppState,
    pub session: &'a TuiEngineSession,
}

impl<'a> TuiProfileSession<'a> {
    pub fn new(app: &'a mut AppState, session: &'a TuiEngineSession) -> Self {
        Self { app, session }
    }
}

impl ProfileSession for TuiProfileSession<'_> {
    fn model_name(&self) -> String {
        self.session.model.name.clone()
    }

    fn agent_id(&self) -> String {
        self.session.engine_half.runtime.session_agents.current_id()
    }

    fn known_agent_ids(&self) -> Vec<String> {
        let agents = &self.session.engine_half.runtime.session_agents;
        agents.choices().into_iter().map(|c| c.id).collect()
    }

    fn switch_agent(&self, id: &str) -> Result<String, String> {
        self.session
            .engine_half
            .runtime
            .session_agents
            .switch_to(id)
    }

    fn permission_mode(&self) -> PermissionMode {
        self.app.permission_mode
    }

    /// Written through `AppState`'s setter so the mirror the engine's broker
    /// reads moves with it, and pushed into the session record so the mode
    /// change lands like a real one rather than only in the footer.
    fn set_permission_mode(&mut self, mode: PermissionMode) {
        super::permission_mode::record_background_permission_mode_acceptance(mode);
        self.app.set_permission_mode(mode);
        let _ = self.session.engine_half.handler.apply_config_option_local(
            &self.session.session_id,
            "permissions",
            mode.as_wire(),
        );
    }

    fn tool_filter(&self) -> ToolFilter {
        self.session.engine_half.session_filter_handle.current()
    }

    fn set_tool_filter(&self, filter: ToolFilter) {
        self.session.engine_half.session_filter_handle.set(filter);
    }

    fn default_tool_filter(&self) -> ToolFilter {
        crate::tui::wiring::build_default_tool_filter_for_context(
            self.session.engine_half.coordinator_mode_handle.get(),
            self.session.startup.queue_session,
        )
    }
}

/// The terminal's name for a provider/model move that still has to reach the
/// model client.
pub(super) fn runtime_update(
    refresh: RuntimeRefresh,
) -> crate::session::commands::provider::ProviderRuntimeUpdate {
    crate::session::commands::provider::ProviderRuntimeUpdate {
        provider_name: refresh.provider_name,
        model_name: refresh.model_name,
    }
}

/// Whether running this command would hand the session to another agent.
///
/// Asked before anything is applied: a backend switch cannot happen mid-prompt
/// or while the session is attached elsewhere, and a profile reaching that
/// switch by a different road has to meet the same gates `/backend` does.
pub(super) fn switches_agent(text: &str) -> bool {
    rebon_plugin_profile::switches_agent(&crate::rebon_config::config_home_dir(), text)
}

pub(super) fn handle_profile_command(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    text: &str,
) -> ProfileCommandResult {
    let config_dir = crate::rebon_config::config_home_dir();
    let outcome = {
        let mut target = TuiProfileSession::new(app, session);
        rebon_plugin_profile::handle_profile_command(&mut target, &config_dir, text)
    };
    if outcome.report.is_err {
        return outcome.report;
    }
    if let Some(refresh) = outcome.runtime_refresh {
        if !super::runtime_refresh::refresh_runtime_model(
            app,
            session,
            Some(runtime_update(refresh)),
            handle,
        ) {
            // `refresh_runtime_model` has already put the reason in the
            // transcript; saying it twice adds nothing.
            return ProfileCommandResult::err(
                "Profile applied, but the runtime did not pick up the new provider/model."
                    .to_string(),
            );
        }
    }
    outcome.report
}

#[cfg(test)]
mod tests {
    use rebon_config::profile_store::Profile;
    use tempfile::TempDir;

    use super::*;

    fn vendor_config(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            crate::rebon_config::config_json_path(dir),
            r#"{
                "activeCustomProvider":"vendor",
                "customProviders":[
                    {"name":"vendor","format":"openai","baseUrl":"https://example.com",
                     "apiKey":"sk","model":"vendor-pro",
                     "models":["vendor-pro","vendor-flash"]}
                ]
            }"#,
        )
        .unwrap();
    }

    /// The one thing this file still decides: a mode change that stops at the
    /// footer is one the tools never honour, because the engine's broker
    /// cannot see `AppState` — it reads the cell.
    #[test]
    fn the_adapter_moves_the_mirror_the_broker_reads() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        let mut app = AppState::new();
        let session = crate::tui::runner::test_support::make_test_tui_session();
        app.permission_mode_cell = session.engine_half.permission_mode_cell.clone();
        let profile = Profile {
            permission_mode: Some("acceptEdits".into()),
            ..Profile::new("edits")
        };

        let outcome = {
            let mut target = TuiProfileSession::new(&mut app, &session);
            rebon_plugin_profile::apply_to_session(&mut target, tmp.path(), &profile, false)
        };

        assert!(!outcome.report.is_err, "{}", outcome.report.text);
        assert_eq!(app.permission_mode, PermissionMode::AcceptEdits);
        assert_eq!(
            *session.engine_half.permission_mode_cell.lock().unwrap(),
            PermissionMode::AcceptEdits
        );
    }

    /// `/profile reset` puts back this session's default surface, which only
    /// the terminal knows how to build.
    #[test]
    fn reset_reaches_this_sessions_own_default_surface() {
        let mut app = AppState::new();
        let session = crate::tui::runner::test_support::make_test_tui_session();
        session
            .engine_half
            .session_filter_handle
            .set(ToolFilter::allow_only(["Read"]));

        let result = {
            let target = TuiProfileSession::new(&mut app, &session);
            rebon_plugin_profile::reset_tool_surface(&target)
        };

        assert!(!result.is_err, "{}", result.text);
        assert!(session
            .engine_half
            .session_filter_handle
            .current()
            .allows("Write", &[]));
    }

    /// A profile naming a backend nobody configured is a note, not a refusal —
    /// and the list it is checked against is this session's.
    #[test]
    fn the_agent_list_comes_from_this_session() {
        let mut app = AppState::new();
        let session = crate::tui::runner::test_support::make_test_tui_session();
        let target = TuiProfileSession::new(&mut app, &session);

        let ids = target.known_agent_ids();

        assert!(
            ids.iter().any(|id| id == "local"),
            "the local engine is always a choice: {ids:?}"
        );
        assert_eq!(target.agent_id(), "local");
    }
}

//! `/status`: what this session is, in one screenful.
//!
//! Provider, model, working directory, MCP, agents and tasks all come
//! off the session itself; the handful of terminal facts it also shows
//! (vim mode, the ultraplan phase, an update notice) arrive through
//! [`super::SessionCommandInputs`], so a worker answers `/status` over
//! IPC with no screen attached.

use crate::mcp::TuiMcpLoadStatus;

pub fn execute_status_command(
    inputs: &crate::commands::SessionCommandInputs,
    session: &crate::EngineSession,
) -> String {
    let active_agents = session.engine_half.agent_registry.active_snapshot().len();
    let active_tasks = inputs
        .task_snapshots
        .iter()
        .filter(|task| !task.status.is_terminal())
        .count();
    let vim_mode = inputs.vim_mode.unwrap_or("disabled").to_string();

    rebon_slash_commands::formatters::format_status_command(
        rebon_slash_commands::formatters::StatusCommandDto {
            provider: session.model.provider_name.clone(),
            model: session.model.name.clone(),
            cwd: session.cwd.clone(),
            ui_mode: format!("{:?}", inputs.ui_mode),
            vim_mode,
            mcp_client: mcp_status_label(session).to_string(),
            active_agents,
            active_tasks,
            session_title: inputs.session_title.map(str::to_string),
            ultraplan_phase: inputs.ultraplan_phase.clone(),
            update_status: update_status_section(inputs.update_notice),
        },
    )
}

/// The update section, or nothing at all when the `updater` plugin is off.
///
/// Asking the seat rather than a config key: the plugin owning the section is
/// the same plugin that owns `/update` and the startup check, so a session
/// that turned updates off gets a `/status` with no update heading rather
/// than one reporting on a feature it does not have.
fn update_status_section(
    notice: Option<&rebon_plugin_updater::UpdateNoticeState>,
) -> Option<String> {
    rebon_plugin_updater::update_check_seat(
        rebon_harness::kernel_bootstrap::process_kernel().context(),
    )
    .map(|_| rebon_plugin_updater::format_update_status(notice))
}

fn mcp_status_label(session: &crate::EngineSession) -> &'static str {
    match session.engine_half.mcp.as_ref().map(|mcp| &mcp.load_status) {
        Some(TuiMcpLoadStatus::Ready { .. }) => "present",
        Some(TuiMcpLoadStatus::Loading) => "loading",
        Some(TuiMcpLoadStatus::Failed { .. } | TuiMcpLoadStatus::NotConfigured) => "not connected",
        // The servers live with the session's owner, not in this process.
        None => "held by the session's owner",
    }
}

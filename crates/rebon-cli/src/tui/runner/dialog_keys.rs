//! Modal-dialog key/mouse dispatch and refresh helpers. Owns the
//! "is this key consumed by an open dialog?" gate-keepers used by the
//! main event loop (`maybe_handle_active_dialog_key`, the per-dialog
//! `maybe_handle_*_dialog_key` shims, `maybe_handle_dialog_shortcut`
//! for the global Ctrl+Shift+P / Ctrl+Shift+F / Ctrl+R bindings,
//! plus `maybe_handle_agent_view_key`/`_mouse` and
//! `maybe_handle_resume_dialog_mouse`), and the `refresh_*` rebuilders
//! that re-sync a dialog's layout against the latest task/registry
//! snapshot after the underlying state changes.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::SystemTime;

use ratatui::crossterm::event::{
    self, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use rebon_design_system::theme::ThemeName;
use rebon_plugin_tasks::runtime::TaskRegistry;
use rebon_tui::RenderTheme;
use rebon_types::format_system_time_iso_ms;
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use crate::session::transcript_replay::BackgroundAgentTaskRef;
use crate::tui::agent_view::AgentViewKeyOutcome;
use crate::tui::app::AppState;
use crate::tui::dialog_host::HostKey;
use crate::tui::external_editor::{edit_text_in_external_editor, open_file_in_external_editor};
use crate::tui::global_search_dialog::{GlobalSearchDialogOutcome, GlobalSearchDialogState};
use crate::tui::goal_confirm_dialog::GoalConfirmOutcome;
use crate::tui::mcp_dialog::McpDialogOutcome;
use crate::tui::resume_dialog::ResumeDialogOutcome;
use crate::tui::rewind_dialog::RewindDialogOutcome;
use crate::tui::terminal::TerminalGuard;
use crate::tui::wiring::TuiEngineSession;
use crate::ui_config::{MathRenderingMode, UiMode};
use rebon_dialog::model::DialogAction;
use rebon_plugin_agents::dialog::AgentsDialogState;
use rebon_plugin_onboarding::OnboardingDialogState;
use rebon_types::ReasoningEffort;

use super::onboarding_hooks::{
    fire_onboarding_advanced, fire_onboarding_closed, fire_onboarding_completed,
};
use super::permission_mode::record_background_permission_mode_acceptance;
use super::submit::set_goal_and_start;
use super::{
    apply_global_search_action, apply_new_session, apply_quick_open_action, apply_rewind_outcome,
    handle_agent_view_outcome, pause_preserve_task, spawn_agent_generation, switch_to_live_agent,
    switch_to_main_agent, ActivePrompt,
};
use crate::session::commands::context::{
    execute_compact_command, execute_prune_command, PruneCommand,
};
use crate::session::commands::effort::set_fast_mode;
use crate::session::commands::provider::ProviderRuntimeUpdate;
use crate::session_shell::session_command_inputs_from_app;

/// Refresh the teams dialog layout against the current registry snapshot.
pub(super) fn refresh_teams_dialog(app: &mut AppState) {
    let snapshots = app.task_snapshots();
    let Some(dialog) = app.teams_dialog.as_mut() else {
        return;
    };
    if !dialog.refresh(&snapshots) {
        app.teams_dialog = None;
    }
}

pub(super) fn refresh_background_tasks_dialog(app: &mut AppState) {
    let snapshots = app.task_snapshots();
    if let Some(dialog) = app.background_tasks_dialog.as_mut() {
        dialog.refresh(&snapshots);
    }
}

fn apply_saved_ui_mode(
    session: &mut TuiEngineSession,
    mode: UiMode,
    apply_to_current_session: bool,
) {
    session.configured_ui_mode = mode;
    if apply_to_current_session {
        session.ui_mode = mode;
    }
    session
        .engine_half
        .handler
        .seed_config_option_value("uiMode", &mode.to_string());
}

fn persist_ui_mode(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    mode: UiMode,
    apply_to_current_session: bool,
    show_restart_notice: bool,
) {
    let applied_value = mode.to_string();
    if let Err(err) = crate::rebon_config::save_ui_mode(&applied_value) {
        tracing::warn!(error = %err, "failed to persist UI mode setting");
    } else {
        apply_saved_ui_mode(session, mode, apply_to_current_session);
        if show_restart_notice {
            let uuid = format!(
                "s-ui-mode-restart-required-{}",
                rebon_types::wall_clock_ms_u128()
            );
            let timestamp = format_system_time_iso_ms(SystemTime::now());
            rebon_tui::reducer(
                &mut app.rebon_tui,
                rebon_tui::Action::Commit(rebon_tui::Message::System(rebon_tui::SystemMessage {
                    uuid,
                    timestamp,
                    subtype: "ui_mode_restart_required".into(),
                    content: Some(format!(
                        "UI mode set to {mode}. Restart rebon for this change to take effect."
                    )),
                    level: Some(rebon_tui::SystemLevel::Info),
                    is_meta: None,
                })),
            );
            app.follow_transcript_tail = true;
        }
    }
}

pub(super) fn refresh_live_agent_tool_activity(app: &mut AppState) {
    use rebon_plugin_tasks::runtime::{TaskData, TaskSnapshot, TaskStatus};
    use rebon_tui::{LiveAgentToolActivity, LiveAgentToolStatus};

    fn matches_ref(snapshot: &TaskSnapshot, task_ref: &BackgroundAgentTaskRef) -> bool {
        task_ref.task_id.as_deref() == Some(snapshot.id.as_str())
            || task_ref.agent_id.as_deref() == Some(snapshot.id.as_str())
    }

    fn display_for_snapshot(snapshot: &TaskSnapshot) -> LiveAgentToolActivity {
        let status = match snapshot.status {
            TaskStatus::Pending | TaskStatus::Running => LiveAgentToolStatus::Running,
            TaskStatus::Completed => LiveAgentToolStatus::Completed,
            TaskStatus::Failed => LiveAgentToolStatus::Failed,
            TaskStatus::Killed => LiveAgentToolStatus::Cancelled,
        };
        let text = match snapshot.status {
            TaskStatus::Pending | TaskStatus::Running => {
                rebon_plugin_tasks::ui::task_activity::format_snapshot_activity(snapshot)
            }
            TaskStatus::Failed => snapshot
                .error
                .clone()
                .filter(|error| !error.trim().is_empty())
                .or_else(|| {
                    snapshot
                        .last_progress
                        .clone()
                        .filter(|progress| !progress.trim().is_empty())
                }),
            TaskStatus::Completed | TaskStatus::Killed => None,
        };
        let token_count = match &snapshot.data {
            TaskData::LocalAgent(data) => non_zero(data.token_count),
            TaskData::InProcessTeammate(data) => non_zero(data.token_count),
            _ => None,
        }
        .or_else(|| result_u64(snapshot, &["total_tokens", "totalTokens"]));
        let tool_use_count = match &snapshot.data {
            TaskData::LocalAgent(data) => non_zero(data.tool_use_count),
            TaskData::InProcessTeammate(data) => non_zero(data.tool_use_count),
            _ => None,
        }
        .or_else(|| {
            result_u64(
                snapshot,
                &[
                    "tool_call_count",
                    "tool_use_count",
                    "toolCallCount",
                    "toolUseCount",
                ],
            )
        });
        let terminal_result = if snapshot.status.is_terminal() {
            snapshot.result.as_ref().and_then(json_object_to_hash_map)
        } else {
            None
        };
        LiveAgentToolActivity {
            text,
            status,
            title: Some(snapshot.title.clone()),
            display_name: snapshot
                .metadata_str("display_name")
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string),
            start_time_ms: Some(snapshot.start_time_ms),
            end_time_ms: snapshot.end_time_ms,
            tool_use_count,
            token_count,
            terminal_result,
        }
    }

    fn display_for_background_snapshot(
        snapshot: &rebon_session_host::BackgroundTaskSnapshot,
    ) -> LiveAgentToolActivity {
        let task = &snapshot.task;
        let status = match task.status.as_str() {
            "pending" | "running" => LiveAgentToolStatus::Running,
            "completed" => LiveAgentToolStatus::Completed,
            "failed" => LiveAgentToolStatus::Failed,
            "killed" | "cancelled" | "stopped" => LiveAgentToolStatus::Cancelled,
            _ => LiveAgentToolStatus::Unknown,
        };
        let text = match status {
            LiveAgentToolStatus::Running => task.last_progress.clone(),
            LiveAgentToolStatus::Failed => task
                .error
                .clone()
                .filter(|error| !error.trim().is_empty())
                .or_else(|| task.last_progress.clone()),
            LiveAgentToolStatus::Completed
            | LiveAgentToolStatus::Cancelled
            | LiveAgentToolStatus::Unknown => None,
        };
        LiveAgentToolActivity {
            text,
            status,
            title: Some(task.title.clone()),
            display_name: None,
            start_time_ms: Some(task.start_time_ms),
            end_time_ms: task.end_time_ms,
            tool_use_count: task.tool_use_count.filter(|count| *count > 0),
            token_count: task.token_count.filter(|count| *count > 0),
            terminal_result: task
                .result
                .as_ref()
                .filter(|_| !matches!(status, LiveAgentToolStatus::Running))
                .and_then(json_object_to_hash_map),
        }
    }

    fn json_object_to_hash_map(value: &Value) -> Option<HashMap<String, Value>> {
        value.as_object().map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
    }

    fn non_zero(value: u64) -> Option<u64> {
        (value > 0).then_some(value)
    }

    fn result_u64(snapshot: &TaskSnapshot, keys: &[&str]) -> Option<u64> {
        let result = snapshot.result.as_ref()?;
        keys.iter()
            .find_map(|key| result.get(*key).and_then(|value| value.as_u64()))
            .filter(|value| *value > 0)
    }

    let snapshots = app.task_snapshots();
    super::live_agent_view::prune_stored_agent_views(app, &snapshots);
    let mut next = HashMap::new();
    for (tool_call_id, task_ref) in &app.background_agent_tool_tasks {
        let activity = snapshots
            .iter()
            .find(|snapshot| matches_ref(snapshot, task_ref))
            .map(display_for_snapshot)
            .or_else(|| {
                app.remote_background_tasks
                    .values()
                    .find(|snapshot| {
                        task_ref.task_id.as_deref() == Some(snapshot.task.task_id.as_str())
                            || task_ref.agent_id.as_deref()
                                == snapshot
                                    .task
                                    .agent_id
                                    .as_deref()
                                    .or(Some(snapshot.task.task_id.as_str()))
                    })
                    .map(display_for_background_snapshot)
            })
            .unwrap_or(LiveAgentToolActivity {
                text: None,
                status: LiveAgentToolStatus::Unknown,
                title: None,
                display_name: None,
                start_time_ms: None,
                end_time_ms: None,
                tool_use_count: None,
                token_count: None,
                terminal_result: None,
            });
        next.insert(tool_call_id.clone(), activity);
    }

    if next != app.live_agent_tool_activity {
        app.live_agent_tool_activity = next;
        app.live_agent_tool_activity_revision =
            app.live_agent_tool_activity_revision.wrapping_add(1);
    }
}

pub(super) fn refresh_agents_dialog(app: &mut AppState) {
    if let Some(dialog) = app.dialogs.top_as_mut::<AgentsDialogState>() {
        dialog.refresh();
    }
}

pub(super) fn maybe_handle_resume_dialog_mouse(app: &mut AppState, mouse: MouseEvent) -> bool {
    let Some(dialog) = app.resume_dialog.as_mut() else {
        return false;
    };
    match mouse.kind {
        MouseEventKind::ScrollUp => dialog.scroll_up(),
        MouseEventKind::ScrollDown => dialog.scroll_down(),
        _ => {}
    }
    true
}

pub(super) fn maybe_handle_agent_view_key(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    key: &KeyEvent,
    guard: &mut TerminalGuard,
) -> bool {
    let Some(view) = app.agent_view.as_mut() else {
        return false;
    };
    let outcome = view.handle_key(key);
    match outcome {
        AgentViewKeyOutcome::EditInputExternally { text } => {
            match guard.suspend_for_terminal_child(|| edit_text_in_external_editor(&text)) {
                Ok(edited) => {
                    if let Some(view) = app.agent_view.as_mut() {
                        view.replace_input_from_external_editor(edited);
                    }
                }
                Err(err) => {
                    if let Some(view) = app.agent_view.as_mut() {
                        view.status = Some(format!("external editor failed: {err}"));
                    }
                }
            }
            true
        }
        outcome => handle_agent_view_outcome(app, session, handle, active_prompt, outcome),
    }
}

pub(super) fn maybe_handle_agent_view_mouse(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    mouse: MouseEvent,
) -> bool {
    let Some(view) = app.agent_view.as_mut() else {
        return false;
    };
    let outcome = view.handle_mouse(mouse);
    handle_agent_view_outcome(app, session, handle, active_prompt, outcome)
}

/// Intercept crossterm key events when the background-tasks dialog
/// is open. Returns `true` when the event was consumed and the
/// outer loop should `continue` without dispatching to the prompt
/// surface. mount-point pattern used by
/// [`super::maybe_handle_permission_key`].
pub(super) fn maybe_handle_background_tasks_dialog_key(
    app: &mut AppState,
    tasks: &TaskRegistry,
    key: &KeyEvent,
    active_prompt: &mut Option<ActivePrompt>,
) -> bool {
    if app.background_tasks_dialog.is_none() {
        return false;
    }
    let Some(dialog) = app.background_tasks_dialog.as_mut() else {
        return false;
    };

    // A key the dialog cannot act on — a release, a chord — still keeps
    // the dialog in focus, so the user cannot submit a prompt underneath
    // an open modal by pressing something it ignores.
    let Some(press) = rebon_tui::dialog_view::translate_key(key) else {
        return true;
    };

    use rebon_plugin_tasks::ui::background_tasks_dialog::DialogKeyOutcome;
    match dialog.handle_key(press.key) {
        DialogKeyOutcome::Dismiss => {
            app.background_tasks_dialog = None;
            true
        }
        DialogKeyOutcome::SwitchToLiveAgent { task_id } => {
            switch_to_live_agent(app, active_prompt, &task_id);
            app.background_tasks_dialog = None;
            true
        }
        DialogKeyOutcome::PausePreserve { task_id } => {
            pause_preserve_task(app, active_prompt, tasks, &task_id);
            true
        }
        DialogKeyOutcome::SwitchToMainAgent => {
            switch_to_main_agent(app);
            app.background_tasks_dialog = None;
            true
        }
        DialogKeyOutcome::Consumed => true,
        DialogKeyOutcome::Ignored => {
            // Unknown keys still keep the dialog in focus while
            // the modal is open. This prevents users from
            // accidentally sending a prompt while the dialog is
            // waiting for input.
            true
        }
    }
}

/// Intercept crossterm key events when the teams dialog is open.
pub(super) fn maybe_handle_teams_dialog_key(
    app: &mut AppState,
    tasks: &TaskRegistry,
    key: &KeyEvent,
) -> bool {
    if app.teams_dialog.is_none() {
        return false;
    }

    let Some(dialog) = app.teams_dialog.as_mut() else {
        return false;
    };

    let Some(press) = rebon_tui::dialog_view::translate_key(key) else {
        return true;
    };

    use rebon_plugin_tasks::ui::teams_dialog::DialogKeyOutcome;
    match dialog.handle_key(press.key, tasks) {
        DialogKeyOutcome::Dismiss => {
            app.teams_dialog = None;
            true
        }
        DialogKeyOutcome::OpenTaskDetail { task_id } => {
            let snapshots = app.task_snapshots();
            app.teams_dialog = None;
            app.background_tasks_dialog = crate::tui::ui_registry::open_background_tasks(
                rebon_plugin_tasks::ui::background_tasks_dialog::BackgroundTasksDialogOpen {
                    snapshots,
                    initial_detail_task_id: Some(task_id),
                    foregrounded_task_id: app.foregrounded_task_id.clone(),
                    kind_filter: None,
                },
            );
            true
        }
        DialogKeyOutcome::Consumed | DialogKeyOutcome::Ignored => true,
    }
}

pub(super) fn maybe_handle_dialog_shortcut(app: &mut AppState, cwd: &Path, key: &KeyEvent) -> bool {
    if app.has_fullscreen_dialog()
        || app.side_question_visible
        || !matches!(
            key.kind,
            event::KeyEventKind::Press | event::KeyEventKind::Repeat
        )
    {
        return false;
    }

    let KeyCode::Char(ch) = key.code else {
        return false;
    };
    let lower = ch.to_ascii_lowercase();
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if ctrl && shift && lower == 'p' {
        open_quick_open(app, cwd);
        app.slash_picker = None;
        app.at_mention_picker = None;
        return true;
    }
    if ctrl && shift && lower == 'f' {
        app.global_search_dialog = Some(GlobalSearchDialogState::open());
        app.slash_picker = None;
        app.at_mention_picker = None;
        return true;
    }
    if ctrl && !shift && lower == 'r' {
        open_history_search(app);
        app.slash_picker = None;
        app.at_mention_picker = None;
        return true;
    }

    false
}

pub(in crate::tui::runner) fn handle_goal_confirm_replace(
    app: &mut AppState,
    prompt: String,
    max_sessions: Option<u32>,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
) {
    let command_text = format!("/goal {prompt}");
    if super::submit::prepare_goal_clarification_if_needed(
        app,
        &command_text,
        &prompt,
        max_sessions,
    ) {
        return;
    }
    set_goal_and_start(
        app,
        &command_text,
        prompt,
        max_sessions,
        session,
        handle,
        active_prompt,
    );
}

fn apply_skills_selection(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    disabled: &BTreeSet<String>,
) -> anyhow::Result<(usize, usize)> {
    apply_skills_selection_with_persist(app, session, disabled, |selection| {
        crate::rebon_config::save_disabled_skills(selection)
    })
}

fn apply_skills_selection_with_persist<F>(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    disabled: &BTreeSet<String>,
    persist: F,
) -> anyhow::Result<(usize, usize)>
where
    F: FnOnce(&BTreeSet<String>) -> anyhow::Result<()>,
{
    // This call must stay before every runtime mutation. A failed write leaves
    // both the registry and slash-command snapshot exactly as they were.
    persist(disabled)?;
    session
        .engine_half
        .skill_registry
        .set_disabled_skills(disabled);
    super::commands::refresh_registered_skill_slash_commands(
        &mut app.slash_commands,
        session.engine_half.skill_registry.as_ref(),
    );
    let total = session.engine_half.skill_registry.all_entries().len();
    let enabled = session.engine_half.skill_registry.entries().len();
    Ok((enabled, total.saturating_sub(enabled)))
}

/// Where the blocking OAuth driver paints the login dialog in inline
/// mode: the bottom of the live viewport, sized to the dialog, matching
/// the prompt-host slot `render_inline_active_dialog` gives it between
/// frames. Oversized panes take the whole viewport rather than
/// overflowing it.
fn inline_oauth_dialog_area(area: Rect, desired_height: u16) -> Rect {
    let height = desired_height.clamp(1, area.height.max(1)).min(area.height);
    Rect {
        x: area.x,
        y: area.bottom().saturating_sub(height),
        width: area.width,
        height,
    }
}

/// Persist one settings option and apply what it changes now. Split out
/// of the router because it is the one arm touching nearly everything:
/// UI mode, math renderer, prune level, model, permission mode.
fn apply_settings_config(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    theme: &mut RenderTheme,
    action: &DialogAction,
) {
    let config_id = action.value().to_string();
    let applied_value = action.value_at(1).to_string();
    if config_id == "uiMode" {
        if let Ok(mode) = applied_value.parse::<UiMode>() {
            persist_ui_mode(app, session, mode, false, true);
        }
    }
    if config_id == "mathRendering" {
        if let Ok(mode) = applied_value.parse::<MathRenderingMode>() {
            let settings_path =
                crate::rebon_config::user_settings_file(&crate::rebon_config::config_home_dir());
            if let Err(err) =
                crate::rebon_config::save_math_rendering_mode_in_file(&settings_path, mode)
            {
                tracing::warn!(error = %err, "failed to persist math rendering setting");
            }
            session.math_rendering_mode = mode;
            app.math_rendering_mode = mode;
            theme.math_display =
                super::terminal_capabilities::terminal_math_display_mode(mode, app.ui_mode);
            // Formula presentation can affect wrapping and row height. The
            // event loop always redraws after this consumed key, so dropping
            // measurements here requests a fresh transcript layout without
            // inventing a renderer-specific formula cache.
            app.clear_all_transcript_measure_caches();
        }
    }
    let _updated = session.engine_half.handler.apply_config_option_local(
        &session.session_id,
        &config_id,
        &applied_value,
    );
    let permission_mode_to_apply = if config_id == "permissions" {
        let mode = rebon_permissions::PermissionMode::from_wire(&applied_value);
        Some(mode)
    } else {
        None
    };
    if config_id == "context_prune" {
        session
            .model
            .prune_level
            .set(rebon_api::PruneLevel::from_config_value(&applied_value));
    }
    if config_id == "auto_compact" {
        session
            .model
            .prune_level
            .budget
            .set_auto_compact_enabled(applied_value == "on");
    }
    if config_id == "model" {
        match crate::rebon_config::persist_model_config_choice(&applied_value) {
            Ok(info) => {
                let runtime_update = Some(ProviderRuntimeUpdate {
                    provider_name: info
                        .as_ref()
                        .map(|info| info.name.clone())
                        .unwrap_or_else(|| session.model.provider_name.clone()),
                    model_name: info
                        .as_ref()
                        .map(|info| crate::rebon_config::resolve_env_value(&info.model))
                        .unwrap_or_else(|| applied_value.trim().to_string()),
                });
                super::runtime_refresh::refresh_runtime_model(app, session, runtime_update, handle);
            }
            Err(err) => {
                tracing::warn!(error = %err, model = %applied_value, "failed to persist user model setting");
            }
        }
    }
    if config_id == "fast_mode" {
        match set_fast_mode(session, applied_value == "on") {
            Ok(_) => {
                app.refresh_empty_startup_banner();
            }
            Err(text) => super::inject_system_message(app, "warning", &text),
        }
    }
    if config_id == "update_auto_install" {
        if let Err(err) =
            rebon_plugin_updater::save_update_auto_install_setting(applied_value == "on")
        {
            tracing::warn!(error = %err, "failed to persist update auto-install setting");
        }
    }
    if config_id == "sub_agents" {
        let enabled = applied_value == "on";
        // Flip the process-global atomic so the engine
        // filters AgentTool (and the system-prompt
        // delegation section) from the next turn without
        // needing a restart.
        rebon_tool::set_sub_agents_enabled(enabled);
        // Write to the user config file so the choice
        // survives restarts.
        crate::rebon_config::save_sub_agents_enabled(enabled);
    }
    if let Some(mode) = permission_mode_to_apply {
        app.set_permission_mode(mode);
        record_background_permission_mode_acceptance(mode);
    }
    // The next frame's projection re-syncs the option list.
}

/// Open the file picker over this session's index.
///
/// The index and the file reads stay here; the panel gets a handle to
/// each, so it can ask on every keystroke exactly as it used to.
fn open_quick_open(app: &mut AppState, cwd: &Path) {
    use rebon_dialog::quick_open_dialog::{FileCandidates, PreviewSource, QuickOpenDialogState};

    struct Index(crate::file_scanner::FileIndex);

    impl FileCandidates for Index {
        fn matching(&self, query: &str, limit: usize) -> Vec<String> {
            self.0
                .search(query, limit)
                .into_iter()
                .map(|result| result.path.replace('\\', "/"))
                .collect()
        }
    }

    struct Files(std::path::PathBuf);

    impl PreviewSource for Files {
        fn preview(&self, path: &str, rows: usize) -> Vec<String> {
            crate::tui::dialog_support::read_preview_lines(
                &self.0.join(std::path::PathBuf::from(path)),
                0,
                rows,
            )
        }
    }

    app.dialogs.push(QuickOpenDialogState::open(
        std::sync::Arc::new(Index(app.file_index.clone())),
        std::sync::Arc::new(Files(cwd.to_path_buf())),
    ));
}

/// Open the prompt search over the session's history, projecting the
/// store's entries into the panel's input — the age string is this
/// terminal's formatting, and the panel is portable.
fn open_history_search(app: &mut AppState) {
    use crate::tui::dialog_support::format_relative_age;
    use rebon_dialog::history_search_dialog::{HistoryInput, HistorySearchDialogState};
    let width = rebon_dialog::history_search::AGE_WIDTH;
    let history = app
        .history
        .iter()
        .map(|entry| HistoryInput {
            age: format!("{:<width$}", format_relative_age(entry.timestamp)),
            display: entry.display.clone(),
            timestamp: entry.timestamp,
        })
        .collect();
    app.dialogs
        .push(HistorySearchDialogState::open(history, app.input.clone()));
}

/// Resolve the level id a `/effort` action carries into the enum the
/// rest of rebon persists. The picker's table lives in `rebon-dialog`,
/// which is dependency-free, so the ids meet the enum here.
fn effort_level_from_id(id: &str) -> Option<ReasoningEffort> {
    ReasoningEffort::from_wire_exact(id)
}

/// Apply a [`DialogAction`] a hosted dialog emitted: one `match` over
/// (dialog, action) in place of a branch per dialog in the dispatcher.
/// The host has already popped the dialog if the action asked it to.
/// The in-flight work one key may start or cancel.
///
/// Two slots rather than two more parameters: both live on the event
/// loop's state, both are `Option`s a dialog action fills or clears, and
/// the dispatcher that routes those actions needs whichever slot the
/// action names.
pub(super) struct PendingWork<'a> {
    pub active_prompt: &'a mut Option<ActivePrompt>,
    pub agent_generation: &'a mut Option<
        oneshot::Receiver<Result<rebon_plugin_agents::surface::generate::GeneratedAgent, String>>,
    >,
}

fn route_hosted_dialog_action(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    theme: &mut RenderTheme,
    cwd: &Path,
    agent_generation: &mut Option<
        oneshot::Receiver<Result<rebon_plugin_agents::surface::generate::GeneratedAgent, String>>,
    >,
    action: DialogAction,
) {
    use rebon_dialog::{doctor_dialog, plugins_dialog};
    use rebon_plugin_agents::dialog as agents_dialog;
    use rebon_ui_seat::ids::{action, dialog};

    match (action.dialog, action.action) {
        (dialog::EFFORT, action::SELECT) => {
            let Some(level) = effort_level_from_id(action.value()) else {
                return;
            };
            if super::remote_session_option::route_session_option_to_owner(
                app,
                session,
                "effort",
                level.as_str(),
            ) {
                return;
            }
            let command = format!("/effort {}", level.as_str());
            let output = crate::session::commands::effort::execute_persisted_effort_command(
                &mut app.effort_level,
                app.effort_provider_kind,
                crate::session::commands::effort::EffortCommand::Set(level),
            );
            if let Err(error) = rebon_session::model_selection::save_manual_effort(
                &session.engine_half.runtime.projects_root,
                &session.cwd,
                &session.session_id,
                app.effort_level,
            ) {
                super::transcript_messages::inject_system_message(
                    app,
                    "warning",
                    &format!("Cannot persist session effort: {error}"),
                );
            }
            tracing::debug!("rebon-cli: /effort picker - {output}");
            super::transcript_messages::inject_local_command_feedback_with_command(
                app, "effort", &command, &output,
            );
            app.follow_transcript_tail = true;
        }
        (dialog::MODEL, action::SELECT) => {
            let model = action.value().to_string();
            // Picking from the list means the same thing typing it does,
            // so it goes to the same place: the owner, when there is one.
            if super::remote_session_option::route_session_option_to_owner(
                app, session, "model", &model,
            ) {
                return;
            }
            match crate::rebon_config::set_custom_provider_model(action.value_at(1), &model) {
                Ok(info) => {
                    // The model now lives on the provider entry itself;
                    // clear any legacy global override so it cannot shadow
                    // provider switches later.
                    if let Err(err) = crate::rebon_config::save_user_model(None) {
                        tracing::warn!(error = %err, "failed to clear legacy user model setting");
                    }
                    let update = ProviderRuntimeUpdate {
                        provider_name: info.name.clone(),
                        model_name: crate::rebon_config::resolve_env_value(&info.model),
                    };
                    if super::runtime_refresh::refresh_runtime_model(
                        app,
                        session,
                        Some(update),
                        handle,
                    ) && !app.refresh_empty_startup_banner()
                    {
                        super::inject_system_message(
                            app,
                            "local_command",
                            &format!(
                                "Switched model for \"{}\" to \"{}\".",
                                session.model.provider_name, session.model.name
                            ),
                        );
                    }
                }
                Err(err) => {
                    super::inject_system_message(app, "error", &err.to_string());
                }
            }
            app.follow_transcript_tail = true;
        }
        (agents_dialog::DIALOG_ID, agents_dialog::ACTION_OPEN) => {
            let path = action.value().to_string();
            if !open_file_in_external_editor(Path::new(&path), None) {
                if let Some(dialog) = app.dialogs.top_as_mut::<AgentsDialogState>() {
                    dialog.set_error(format!("Failed to open external editor for {path}"));
                }
            }
        }
        (agents_dialog::DIALOG_ID, agents_dialog::ACTION_GENERATE) => {
            spawn_agent_generation(app, session, handle, agent_generation, action.value());
        }
        (dialog::MEMORY, action::OPEN) => {
            let path = std::path::PathBuf::from(action.value());
            if !open_file_in_external_editor(&path, None) {
                tracing::warn!(
                    path = %path.display(),
                    "failed to open memory file in external editor"
                );
            }
        }
        (dialog::SKILLS, action::APPLY) => {
            let disabled: BTreeSet<String> = action.values.into_iter().collect();
            match apply_skills_selection(app, session, &disabled) {
                Ok((enabled, disabled_count)) => {
                    super::transcript_messages::inject_local_command_feedback(
                        app,
                        "skills",
                        &format!("Skills updated: {enabled} enabled, {disabled_count} disabled."),
                    );
                }
                Err(err) => super::transcript_messages::inject_system_message(
                    app,
                    "error",
                    &format!("Failed to save skill settings: {err}"),
                ),
            }
            app.follow_transcript_tail = true;
        }
        (dialog::PROVIDER, action::ACTIVATE) => {
            // No value at all is the "Default (env)" row.
            route_provider_activation(app, session, handle, action.values.into_iter().next());
        }
        (dialog::PROVIDER, action::ADD) => {
            open_provider_form(app, session, handle, None);
        }
        (dialog::PROVIDER, action::EDIT) => {
            open_provider_form(app, session, handle, Some(action.value()));
        }
        (dialog::PROVIDER, action::REMOVE) => {
            let name = action.value();
            match crate::rebon_config::remove_custom_provider(name) {
                Ok(was_active) => {
                    super::inject_system_message(
                        app,
                        "local_command",
                        &format!("Provider \"{name}\" removed."),
                    );
                    if was_active {
                        let update = ProviderRuntimeUpdate {
                            provider_name: "env".into(),
                            model_name: crate::tui::wiring::default_model(),
                        };
                        super::runtime_refresh::refresh_runtime_model(
                            app,
                            session,
                            Some(update),
                            handle,
                        );
                    }
                    // Rebuild so the switcher stays open with the updated
                    // list, through the seat like every other open.
                    if let Some(rebuilt) = crate::tui::ui_registry::open(
                        dialog::PROVIDER,
                        rebon_ui_seat::DialogArgs::none(),
                    ) {
                        app.dialogs.push_boxed(rebuilt);
                    }
                }
                Err(err) => super::inject_system_message(app, "error", &err.to_string()),
            }
            app.follow_transcript_tail = true;
        }
        (dialog::DOCTOR, action::RERUN) => {
            let inputs = session_command_inputs_from_app(app, session.ui_mode);
            let report = crate::session::commands::doctor::collect_doctor_report(&inputs, session);
            // The panel stayed open, so the fresh report is written back
            // into it rather than handed to a new one.
            if let Some(dialog) = app.dialogs.top_as_mut::<doctor_dialog::DoctorDialogState>() {
                dialog.replace_report(&report);
            }
        }
        (dialog::PLUGINS, action::EXECUTE) => {
            let command = action.value().to_string();
            let result = crate::session::commands::plugin::handle_plugin_command(&command, session);
            if result.is_err {
                super::transcript_messages::inject_system_message(app, "error", &result.text);
            } else {
                super::transcript_messages::inject_local_command_feedback_with_command(
                    app,
                    "plugin",
                    &command,
                    &result.text,
                );
            }
            app.follow_transcript_tail = true;
            let refreshed = (!result.is_err).then(|| {
                crate::session::commands::plugin::handle_plugin_command("/plugin list", session)
            });
            if let Some(dialog) = app
                .dialogs
                .top_as_mut::<plugins_dialog::PluginsDialogState>()
            {
                dialog.set_feedback(result.text, result.is_err);
                if let Some(list) = refreshed {
                    dialog.refresh(&list.text, list.is_err);
                }
            }
        }
        (dialog::SETTINGS, action::APPLY_CONFIG) => {
            apply_settings_config(app, session, handle, theme, &action);
        }
        (dialog::CONTEXT, action::COMPACT) => {
            let output = execute_compact_command(session, None);
            super::transcript_messages::inject_system_message(app, "local_command", &output);
            app.follow_transcript_tail = true;
        }
        (dialog::CONTEXT, action::PRUNE_SWEEP) => {
            let output = execute_prune_command(PruneCommand::Sweep(None), session);
            super::transcript_messages::inject_system_message(app, "prune", &output);
            app.follow_transcript_tail = true;
        }
        (dialog::HISTORY_SEARCH, action::APPLY) => {
            app.input = action.value().to_string();
            app.cursor_offset = app.input.len();
        }
        (dialog::QUICK_OPEN, action::APPLY) => {
            let path = action.value_at(1);
            let quick = match action.value() {
                "mention" => rebon_dialog::quick_open::handle_insert(path, true),
                "insert" => rebon_dialog::quick_open::handle_insert(path, false),
                _ => rebon_dialog::quick_open::handle_select(path),
            };
            apply_quick_open_action(app, cwd, quick);
        }
        (dialog, other) => tracing::warn!(dialog, action = other, "unrouted dialog action"),
    }
}

/// 切换到指定 provider；`None` 表示恢复环境变量配置。
fn route_provider_activation(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    name: Option<String>,
) {
    match name {
        Some(name) => match crate::rebon_config::set_active_custom_provider(&name) {
            Ok(info) => {
                let update = ProviderRuntimeUpdate {
                    provider_name: info.name.clone(),
                    model_name: crate::rebon_config::resolve_env_value(&info.model),
                };
                if super::runtime_refresh::refresh_runtime_model(app, session, Some(update), handle)
                {
                    super::transcript_messages::inject_provider_switch(
                        app,
                        &session.model.provider_name,
                        &session.model.name,
                    );
                }
            }
            Err(err) => super::inject_system_message(app, "error", &err.to_string()),
        },
        None => match crate::rebon_config::clear_active_custom_provider() {
            Ok(()) => {
                let update = ProviderRuntimeUpdate {
                    provider_name: "env".into(),
                    model_name: crate::tui::wiring::default_model(),
                };
                if super::runtime_refresh::refresh_runtime_model(app, session, Some(update), handle)
                {
                    super::transcript_messages::inject_provider_switch(
                        app,
                        &session.model.provider_name,
                        &session.model.name,
                    );
                }
            }
            Err(err) => super::inject_system_message(
                app,
                "error",
                &format!("Failed to deactivate provider: {err}"),
            ),
        },
    }
}

/// Open the onboarding provider form in add or edit mode. Onboarding is
/// not on the dialog host yet, so this still writes its `Option` field.
fn open_provider_form(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    edit: Option<&str>,
) {
    let mut dialog = OnboardingDialogState::open_for_provider_command();
    match edit {
        Some(name) => {
            dialog.enter_provider_edit_mode(name);
        }
        None => dialog.enter_provider_add_mode(),
    }
    app.onboarding_dialog = Some(dialog);
    if let Some(dialog) = app.onboarding_dialog.as_ref() {
        super::onboarding_hooks::fire_onboarding_opened(session, handle, dialog, "slash_provider");
    }
}

pub(super) fn maybe_handle_active_dialog_key(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    pending: PendingWork<'_>,
    cwd: &Path,
    key: &KeyEvent,
    theme: &mut RenderTheme,
    guard: &mut TerminalGuard,
) -> bool {
    let PendingWork {
        active_prompt,
        agent_generation,
    } = pending;
    // Dialogs ported to `DialogModel` live on one stack instead of one
    // `Option` field each, so this is the whole dispatch for all of
    // them: the top one takes the key, and any action it emits is
    // routed below.
    match crate::tui::dialog_host::handle_key(&mut app.dialogs, key) {
        HostKey::NotConsumed => {}
        HostKey::Consumed => {
            // A closing key pops the panel; an in-flight generation for
            // the panel that just closed has nowhere to land.
            if agent_generation.is_some() && app.dialogs.top_as::<AgentsDialogState>().is_none() {
                *agent_generation = None;
            }
            return true;
        }
        HostKey::Action(action) => {
            route_hosted_dialog_action(app, session, handle, theme, cwd, agent_generation, action);
            return true;
        }
    }

    if let Some(dialog) = app.goal_confirm_dialog.as_mut() {
        match dialog.handle_key(key) {
            GoalConfirmOutcome::Replace {
                prompt,
                max_sessions,
            } => {
                app.goal_confirm_dialog = None;
                handle_goal_confirm_replace(
                    app,
                    prompt,
                    max_sessions,
                    session,
                    handle,
                    active_prompt,
                );
            }
            GoalConfirmOutcome::Cancel => {
                app.goal_confirm_dialog = None;
            }
            GoalConfirmOutcome::None => {}
        }
        return true;
    }

    if let Some(dialog) = app.global_search_dialog.as_mut() {
        let outcome = dialog.handle_key(key);
        dialog.sync_preview(cwd);
        match outcome {
            GlobalSearchDialogOutcome::Apply(action) => {
                app.global_search_dialog = None;
                apply_global_search_action(app, cwd, action);
            }
            GlobalSearchDialogOutcome::None => {}
        }
        return true;
    }

    if let Some(dialog) = app.mcp_dialog.as_mut() {
        match dialog.handle_key(key) {
            McpDialogOutcome::Close => app.mcp_dialog = None,
            McpDialogOutcome::OpenConfig(path) => {
                if !open_file_in_external_editor(&path, None) {
                    tracing::warn!(path = %path.display(), "unable to open MCP config in external editor");
                }
            }
            McpDialogOutcome::None => {}
        }
        return true;
    }

    if app.resume_dialog.is_some() {
        let outcome = app
            .resume_dialog
            .as_mut()
            .expect("resume dialog disappeared")
            .handle_key(key);
        match outcome {
            ResumeDialogOutcome::UseFullHistory => {
                app.resume_dialog = None;
            }
            ResumeDialogOutcome::CancelResume => {
                app.resume_dialog = None;
                apply_new_session(app, session);
            }
            ResumeDialogOutcome::CopySessionId(session_id) => {
                rebon_tui::selection::copy_to_clipboard_osc52(&session_id);
            }
            ResumeDialogOutcome::Close => {
                let startup_resume = app
                    .resume_dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.is_startup_resume());
                if startup_resume && app.ui_mode == UiMode::Inline {
                    app.pending_inline_startup_banner = true;
                }
                app.resume_dialog = None;
            }
            ResumeDialogOutcome::None => {}
        }
        return true;
    }

    if let Some(dialog) = app.rewind_dialog.as_mut() {
        match dialog.handle_key(key) {
            RewindDialogOutcome::Close => {
                app.rewind_dialog = None;
            }
            RewindDialogOutcome::Restore {
                selected_index,
                option,
                feedback,
            } => {
                apply_rewind_outcome(app, session, selected_index, option, feedback);
                app.rewind_dialog = None;
            }
            RewindDialogOutcome::None => {}
        }
        return true;
    }

    if app.onboarding_dialog.is_some() {
        handle_onboarding_dialog_key(app, session, handle, key, theme, guard);
        return true;
    }

    false
}

/// The onboarding wizard's keys — and with them `/theme`, `/migrate` and
/// the dedicated login pane, which are the same dialog opened at one
/// step. The caller has already checked that the dialog is open.
fn handle_onboarding_dialog_key(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    key: &KeyEvent,
    theme: &mut RenderTheme,
    guard: &mut TerminalGuard,
) {
    use rebon_plugin_onboarding::{
        apply_add_provider, apply_add_provider_model, apply_run_migration, apply_update_provider,
        OnboardingDialogOutcome,
    };
    let outcome = crate::tui::onboarding_dialog::handle_key(
        app.onboarding_dialog
            .as_mut()
            .expect("the caller checked the onboarding dialog is open"),
        key,
    );
    let is_done = app.onboarding_dialog.as_ref().map_or(false, |d| d.done);
    let is_theme_only = app
        .onboarding_dialog
        .as_ref()
        .map_or(false, |d| d.is_theme_only());
    let ui_mode_requires_restart = app
        .onboarding_dialog
        .as_ref()
        .is_some_and(|dialog| dialog.ui_mode_change_requires_restart());
    match outcome {
        OnboardingDialogOutcome::Close => {
            if let Some(dialog) = app.onboarding_dialog.as_ref() {
                fire_onboarding_closed(session, handle, dialog, "escape");
            }
            app.onboarding_dialog = None;
        }
        OnboardingDialogOutcome::Completed { transition } => {
            // `/theme` and `/migrate` are single-step commands, not a
            // setup run — finishing them must not flip the
            // onboarding-completed flag for a user who never saw setup.
            let skips_completion_flag = app
                .onboarding_dialog
                .as_ref()
                .is_some_and(|dialog| dialog.skips_completion_flag());
            if let Some(dialog) = app.onboarding_dialog.as_ref() {
                if let Some(transition) = transition {
                    if transition.previous_step != transition.next_step {
                        fire_onboarding_advanced(session, handle, dialog, transition);
                    }
                }
                if skips_completion_flag {
                    fire_onboarding_closed(session, handle, dialog, "command_done");
                } else {
                    fire_onboarding_completed(session, handle, dialog);
                }
            }
            app.onboarding_dialog = None;
            if !skips_completion_flag {
                crate::rebon_config::complete_onboarding();
                tracing::info!("onboarding: completed, saved to config");
            }
        }
        OnboardingDialogOutcome::Advanced(transition) => {
            if let Some(dialog) = app.onboarding_dialog.as_ref() {
                fire_onboarding_advanced(session, handle, dialog, transition);
            }
        }
        OnboardingDialogOutcome::ThemePreview(setting) => {
            let theme_id = setting.id();
            if let Some(name) = ThemeName::from_str(theme_id) {
                rebon_design_system::theme::set_active_theme(name);
                *theme = RenderTheme {
                    supports_hyperlinks: theme.supports_hyperlinks,
                    math_display: theme.math_display,
                    ..RenderTheme::from_theme_name(name)
                };
            }
            tracing::info!(theme = %theme_id, "onboarding: theme preview");
        }
        OnboardingDialogOutcome::ThemeSelected {
            setting,
            transition,
        } => {
            let theme_id = setting.id();
            if let Some(name) = ThemeName::from_str(theme_id) {
                rebon_design_system::theme::set_active_theme(name);
                *theme = RenderTheme {
                    supports_hyperlinks: theme.supports_hyperlinks,
                    math_display: theme.math_display,
                    ..RenderTheme::from_theme_name(name)
                };
            }
            crate::rebon_config::save_theme(theme_id);
            tracing::info!(theme = %theme_id, "onboarding: theme selected");
            if let Some(dialog) = app.onboarding_dialog.as_ref() {
                if is_done {
                    if is_theme_only {
                        fire_onboarding_closed(session, handle, dialog, "theme_selected");
                    } else {
                        fire_onboarding_completed(session, handle, dialog);
                    }
                } else if transition.previous_step != transition.next_step {
                    fire_onboarding_advanced(session, handle, dialog, transition);
                }
            }
            if is_done {
                if !is_theme_only {
                    crate::rebon_config::complete_onboarding();
                }
                app.onboarding_dialog = None;
            }
        }
        OnboardingDialogOutcome::UiModeSelected { mode, transition } => {
            let applied_value = mode.to_string();
            persist_ui_mode(app, session, mode, true, ui_mode_requires_restart);
            let _updated = session.engine_half.handler.apply_config_option_local(
                &session.session_id,
                "uiMode",
                &applied_value,
            );
            if let Some(dialog) = app.onboarding_dialog.as_ref() {
                if transition.previous_step != transition.next_step {
                    fire_onboarding_advanced(session, handle, dialog, transition);
                }
            }
        }
        OnboardingDialogOutcome::AddProvider {
            preset_id,
            name,
            api_key,
            base_url,
            format,
            model,
        } => {
            if let Some(dialog) = app.onboarding_dialog.as_mut() {
                let (_, transition) = apply_add_provider(
                    dialog,
                    preset_id.as_deref(),
                    &name,
                    &format,
                    &base_url,
                    &api_key,
                    &model,
                );
                if let Some(transition) = transition {
                    fire_onboarding_advanced(session, handle, dialog, transition);
                }
            }
        }
        OnboardingDialogOutcome::UpdateProvider {
            original_name,
            name,
            api_key,
            base_url,
            format,
            model,
        } => {
            if let Some(dialog) = app.onboarding_dialog.as_mut() {
                let transition = apply_update_provider(
                    dialog,
                    &original_name,
                    &name,
                    &format,
                    &base_url,
                    &api_key,
                    &model,
                );
                if let Some(transition) = transition {
                    fire_onboarding_advanced(session, handle, dialog, transition);
                }
            }
        }
        OnboardingDialogOutcome::AddProviderModel {
            provider_name,
            model,
        } => {
            if let Some(dialog) = app.onboarding_dialog.as_mut() {
                apply_add_provider_model(dialog, &provider_name, &model);
            }
        }
        OnboardingDialogOutcome::StartOpenAIOAuth => {
            // The driver owns the terminal while it blocks, so it has
            // to paint the same slot the frame renderer would: the
            // whole screen in screen mode, the bottom-anchored prompt
            // host in inline mode. Read the surface before borrowing
            // the dialog out of `app`.
            let inline_host = app.ui_mode == UiMode::Inline;
            let mut connected = false;
            let mut close_after_success = false;
            if let Some(dialog) = app.onboarding_dialog.as_mut() {
                let terminal = guard.terminal();
                let redraw = |s: &OnboardingDialogState| -> std::io::Result<()> {
                    terminal.draw(|frame| {
                        let area = frame.area();
                        let target = if inline_host {
                            inline_oauth_dialog_area(area, s.inline_host_desired_height())
                        } else {
                            area
                        };
                        crate::tui::onboarding_dialog::render(s, frame, target);
                    })?;
                    Ok(())
                };
                let drive_result =
                    crate::oauth_drive::drive_oauth_flow_blocking(dialog, handle, redraw);
                match drive_result {
                    Ok(crate::oauth_drive::Outcome::Success { transition }) => {
                        connected = true;
                        if let Some(transition) = transition {
                            fire_onboarding_advanced(session, handle, dialog, transition);
                        }
                        if dialog.done {
                            fire_onboarding_completed(session, handle, dialog);
                        }
                        // The dedicated login pane has nothing
                        // left to ask once the account is connected.
                        // Leaving it open re-arms Enter on the same
                        // "OpenAI account" row, which starts another
                        // browser round-trip — the login loop.
                        if dialog.is_login_pane() {
                            close_after_success = true;
                            fire_onboarding_closed(session, handle, dialog, "oauth_success");
                        }
                    }
                    Ok(crate::oauth_drive::Outcome::Cancelled)
                    | Ok(crate::oauth_drive::Outcome::Failed) => {}
                    Err(err) => {
                        tracing::error!(error = %err, "oauth drive failed");
                        dialog.report_oauth_error(err.to_string(), false);
                    }
                }
            }
            if close_after_success {
                app.onboarding_dialog = None;
            }
            if connected {
                // The session's model client still holds the token
                // (or the missing-provider error) it resolved at
                // startup. Re-resolve from disk so the very next
                // prompt uses the credentials we just persisted.
                let fallback = ProviderRuntimeUpdate {
                    provider_name: session.model.provider_name.clone(),
                    model_name: session.model.name.clone(),
                };
                let refreshed = super::runtime_refresh::refresh_runtime_model(
                    app,
                    session,
                    Some(fallback),
                    handle,
                );
                // Only the login pane closes on success; the wizard keeps
                // its own success banner on screen and walks to the
                // next step, so a transcript line there would be
                // noise the user cannot even see yet.
                if refreshed && close_after_success {
                    super::transcript_messages::inject_local_command_feedback(
                        app,
                        "login",
                        &format!(
                            "OpenAI account connected. Using provider {} · model {}.",
                            session.model.provider_name, session.model.name
                        ),
                    );
                    app.follow_transcript_tail = true;
                }
            }
        }
        OnboardingDialogOutcome::SubmitOpenAIOAuthPaste(_)
        | OnboardingDialogOutcome::CancelOpenAIOAuth => {
            // These bubble out of the dialog's key handler only
            // while the driver owns the event loop above.
        }
        OnboardingDialogOutcome::RunMigration(categories) => {
            if let Some(dialog) = app.onboarding_dialog.as_mut() {
                let summary = apply_run_migration(dialog, &categories);
                tracing::info!(
                    copied = summary.copied,
                    skipped = summary.skipped_existing,
                    failed = summary.failed,
                    "onboarding: migration complete"
                );
            }
        }
        OnboardingDialogOutcome::None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{local_agent_snapshot, make_test_tui_session};
    use super::*;
    use rebon_plugin_tasks::runtime::{TaskId, TaskStatus};
    use rebon_tui::{LiveAgentToolActivity, LiveAgentToolStatus};

    #[test]
    fn saved_settings_ui_mode_does_not_change_current_session_mode() {
        let mut session = make_test_tui_session();

        apply_saved_ui_mode(&mut session, UiMode::Inline, false);

        assert_eq!(session.ui_mode, UiMode::Screen);
        assert_eq!(session.configured_ui_mode, UiMode::Inline);
    }

    #[test]
    fn onboarding_ui_mode_updates_current_and_configured_modes() {
        let mut session = make_test_tui_session();

        apply_saved_ui_mode(&mut session, UiMode::Inline, true);

        assert_eq!(session.ui_mode, UiMode::Inline);
        assert_eq!(session.configured_ui_mode, UiMode::Inline);
    }

    /// The blocking OAuth driver paints the dialog itself. In inline mode
    /// it must stay in the prompt host at the bottom of the live
    /// viewport instead of taking the whole surface the way screen mode
    /// does.
    #[test]
    fn inline_oauth_dialog_area_is_bottom_anchored_and_bounded() {
        let viewport = Rect::new(0, 30, 120, 16);

        let fitted = inline_oauth_dialog_area(viewport, 9);
        assert_eq!(fitted, Rect::new(0, 37, 120, 9));
        assert_eq!(fitted.bottom(), viewport.bottom());

        // A pane taller than the viewport fills it rather than
        // overflowing past the bottom edge.
        let oversized = inline_oauth_dialog_area(viewport, 40);
        assert_eq!(oversized, viewport);

        // Degenerate viewports never produce a zero-height rect that
        // would render nothing at all.
        assert_eq!(
            inline_oauth_dialog_area(Rect::new(0, 0, 80, 1), 9).height,
            1
        );
    }

    #[test]
    fn failed_skills_persistence_does_not_change_runtime_or_slash_commands() {
        let mut session = make_test_tui_session();
        let registry = std::sync::Arc::new(rebon_plugin_skill::SkillRegistry::new());
        for id in ["skill-a", "skill-b"] {
            registry.register(rebon_plugin_skill::Skill {
                id: id.into(),
                title: id.into(),
                description: format!("Description for {id}"),
                prompt_template: String::new(),
                suggested_tools: Vec::new(),
                source: rebon_plugin_skill::SkillSource::Project,
                argument_hint: None,
                argument_names: Vec::new(),
                skill_root: None,
                user_invocable: true,
                disable_model_invocation: false,
                required_tools: Vec::new(),
            });
        }
        registry.set_disabled_skills(["skill-b"]);
        session.engine_half.skill_registry = registry;
        let mut app = AppState::new();
        super::super::commands::register_local_slash_commands(&mut app.slash_commands);
        super::super::commands::refresh_registered_skill_slash_commands(
            &mut app.slash_commands,
            session.engine_half.skill_registry.as_ref(),
        );
        let slash_before = format!("{:?}", app.slash_commands);
        let requested = ["skill-a".to_string()].into_iter().collect();

        let result =
            apply_skills_selection_with_persist(&mut app, &mut session, &requested, |_| {
                Err(anyhow::anyhow!("simulated write failure"))
            });

        assert!(result.is_err());
        assert_eq!(
            session.engine_half.skill_registry.disabled_skills(),
            ["skill-b".to_string()].into_iter().collect()
        );
        assert_eq!(format!("{:?}", app.slash_commands), slash_before);
    }

    #[test]
    fn successful_skills_apply_updates_registry_slashes_and_counts() {
        let mut session = make_test_tui_session();
        let registry = std::sync::Arc::new(rebon_plugin_skill::SkillRegistry::new());
        for id in ["skill-a", "skill-b"] {
            registry.register(rebon_plugin_skill::Skill {
                id: id.into(),
                title: id.into(),
                description: format!("Description for {id}"),
                prompt_template: String::new(),
                suggested_tools: Vec::new(),
                source: rebon_plugin_skill::SkillSource::Project,
                argument_hint: None,
                argument_names: Vec::new(),
                skill_root: None,
                user_invocable: true,
                disable_model_invocation: false,
                required_tools: Vec::new(),
            });
        }
        session.engine_half.skill_registry = registry;
        let mut app = AppState::new();
        super::super::commands::register_local_slash_commands(&mut app.slash_commands);
        super::super::commands::refresh_registered_skill_slash_commands(
            &mut app.slash_commands,
            session.engine_half.skill_registry.as_ref(),
        );
        let requested = ["skill-a".to_string()].into_iter().collect();

        let counts =
            apply_skills_selection_with_persist(&mut app, &mut session, &requested, |_| Ok(()))
                .unwrap();

        assert_eq!(counts, (1, 1));
        assert!(session.engine_half.skill_registry.is_disabled("skill-a"));
        assert!(!app
            .slash_commands
            .iter()
            .any(|command| command.name == "skill-a"));
        assert!(app
            .slash_commands
            .iter()
            .any(|command| command.name == "skill-b"));
    }

    fn refreshed_activity(
        status: TaskStatus,
        error: Option<&str>,
        last_progress: Option<&str>,
    ) -> LiveAgentToolActivity {
        let task_id = "agent-1";
        let mut app = AppState::new();
        let mut snapshot = local_agent_snapshot(task_id, status, true);
        snapshot.error = error.map(str::to_string);
        snapshot.last_progress = last_progress.map(str::to_string);
        snapshot.start_time_ms = 1_234;
        snapshot.end_time_ms = snapshot.status.is_terminal().then_some(5_234);
        snapshot.metadata = serde_json::json!({ "display_name": "explore-engine" });
        app.tasks.insert(
            TaskId::new(task_id),
            snapshot,
            rebon_types::PromptCancel::new(),
        );
        app.background_agent_tool_tasks.insert(
            "toolu_agent".to_string(),
            BackgroundAgentTaskRef {
                task_id: Some(task_id.to_string()),
                agent_id: None,
            },
        );

        refresh_live_agent_tool_activity(&mut app);

        app.live_agent_tool_activity
            .remove("toolu_agent")
            .expect("live agent activity should be refreshed")
    }

    #[test]
    fn local_background_snapshot_projects_display_name_and_start_time() {
        let activity = refreshed_activity(TaskStatus::Running, None, None);

        assert_eq!(activity.display_name.as_deref(), Some("explore-engine"));
        assert_eq!(activity.start_time_ms, Some(1_234));
        assert_eq!(activity.end_time_ms, None);
    }

    #[test]
    fn stopped_local_background_snapshot_projects_end_time_without_a_result() {
        let activity = refreshed_activity(TaskStatus::Killed, None, None);

        assert_eq!(activity.status, LiveAgentToolStatus::Cancelled);
        assert_eq!(activity.start_time_ms, Some(1_234));
        assert_eq!(activity.end_time_ms, Some(5_234));
        assert_eq!(activity.terminal_result, None);
    }

    #[test]
    fn remote_background_snapshot_drives_live_agent_activity() {
        let mut app = AppState::new();
        app.background_agent_tool_tasks.insert(
            "toolu_remote".into(),
            BackgroundAgentTaskRef {
                task_id: Some("agent-remote".into()),
                agent_id: Some("agent-remote".into()),
            },
        );
        app.remote_background_tasks.insert(
            "agent-remote".into(),
            rebon_session_host::BackgroundTaskSnapshot {
                task: rebon_session_host::BackgroundTaskDescriptor {
                    task_id: "agent-remote".into(),
                    title: "Inspect remote session".into(),
                    kind: "local_agent".into(),
                    status: "running".into(),
                    is_backgrounded: true,
                    start_time_ms: 1,
                    end_time_ms: None,
                    last_progress: Some("reading source".into()),
                    error: None,
                    prompt: None,
                    parent_tool_call_id: Some("toolu_remote".into()),
                    agent_id: Some("agent-remote".into()),
                    agent_name: Some("Explore".into()),
                    agent_type: Some("Explore".into()),
                    model: Some("test-model".into()),
                    token_count: Some(42),
                    tool_use_count: Some(3),
                    result: None,
                },
                updated_at_ms: 2,
                log_preview: Vec::new(),
                transcript: Vec::new(),
            },
        );

        refresh_live_agent_tool_activity(&mut app);

        let activity = app
            .live_agent_tool_activity
            .get("toolu_remote")
            .expect("remote activity");
        assert_eq!(activity.status, LiveAgentToolStatus::Running);
        assert_eq!(activity.text.as_deref(), Some("reading source"));
        assert_eq!(activity.title.as_deref(), Some("Inspect remote session"));
        assert_eq!(activity.start_time_ms, Some(1));
        assert_eq!(activity.end_time_ms, None);
        assert_eq!(activity.token_count, Some(42));
        assert_eq!(activity.tool_use_count, Some(3));

        let remote = app
            .remote_background_tasks
            .get_mut("agent-remote")
            .expect("remote snapshot");
        remote.task.status = "stopped".into();
        remote.task.end_time_ms = Some(5);
        remote.task.result = None;
        refresh_live_agent_tool_activity(&mut app);

        let stopped = app
            .live_agent_tool_activity
            .get("toolu_remote")
            .expect("stopped remote activity");
        assert_eq!(stopped.status, LiveAgentToolStatus::Cancelled);
        assert_eq!(stopped.end_time_ms, Some(5));
        assert_eq!(stopped.terminal_result, None);
    }

    #[test]
    fn failed_live_agent_activity_prefers_snapshot_error() {
        let error = "unsupported model `gpt-unsupported` for provider `test`";
        let activity = refreshed_activity(
            TaskStatus::Failed,
            Some(error),
            Some("less specific progress text"),
        );

        assert_eq!(activity.status, LiveAgentToolStatus::Failed);
        assert_eq!(activity.text.as_deref(), Some(error));
    }

    #[test]
    fn failed_live_agent_activity_falls_back_to_last_progress() {
        let activity = refreshed_activity(
            TaskStatus::Failed,
            None,
            Some("agent process exited before returning a result"),
        );

        assert_eq!(
            activity.text.as_deref(),
            Some("agent process exited before returning a result")
        );
    }

    #[test]
    fn non_failed_terminal_live_agent_activity_keeps_text_empty() {
        let completed = refreshed_activity(
            TaskStatus::Completed,
            Some("ignored completed error"),
            Some("ignored completed progress"),
        );
        let cancelled = refreshed_activity(
            TaskStatus::Killed,
            Some("ignored cancelled error"),
            Some("ignored cancelled progress"),
        );

        assert_eq!(completed.status, LiveAgentToolStatus::Completed);
        assert_eq!(completed.text, None);
        assert_eq!(cancelled.status, LiveAgentToolStatus::Cancelled);
        assert_eq!(cancelled.text, None);
    }

    #[test]
    fn background_tasks_dialog_esc_closes_and_allows_input_again() {
        let mut app = AppState::new();
        let snapshots = app.task_snapshots();
        app.background_tasks_dialog = Some(
            rebon_plugin_tasks::ui::background_tasks_dialog::BackgroundTasksDialogState::open(
                &snapshots,
                None,
                app.foregrounded_task_id.clone(),
            ),
        );

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let mut active_prompt = None;
        let tasks = app.tasks.clone();
        assert!(maybe_handle_background_tasks_dialog_key(
            &mut app,
            tasks.as_ref(),
            &esc,
            &mut active_prompt
        ));
        assert!(app.background_tasks_dialog.is_none());

        let typed = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(!maybe_handle_background_tasks_dialog_key(
            &mut app,
            tasks.as_ref(),
            &typed,
            &mut active_prompt
        ));
    }

    #[test]
    fn a_key_release_is_swallowed_without_reaching_the_background_tasks_dialog() {
        // The dialog reducer no longer sees terminal key kinds, so this
        // boundary is where a release stops. Letting one through would both
        // open a detail pane on the release half of an Enter press and, by
        // returning false, let the keystroke fall into the prompt under an
        // open modal.
        let mut app = AppState::new();
        let snapshots = app.task_snapshots();
        app.background_tasks_dialog = Some(
            rebon_plugin_tasks::ui::background_tasks_dialog::BackgroundTasksDialogState::open(
                &snapshots,
                None,
                app.foregrounded_task_id.clone(),
            ),
        );

        let release = KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            ratatui::crossterm::event::KeyEventKind::Release,
        );
        let mut active_prompt = None;
        let tasks = app.tasks.clone();
        assert!(maybe_handle_background_tasks_dialog_key(
            &mut app,
            tasks.as_ref(),
            &release,
            &mut active_prompt
        ));
        assert!(
            app.background_tasks_dialog
                .as_ref()
                .is_some_and(|dialog| dialog.is_list_mode()),
            "a release must not open a detail pane"
        );
    }
}

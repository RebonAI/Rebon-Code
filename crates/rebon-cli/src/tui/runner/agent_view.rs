//! Agent View helpers: open/refresh/reorder the Agent View dialog,
//! load and persist its grouping preference, derive dispatch cwd and
//! detach predicates. Excludes the larger orchestration (dispatch,
//! outcome handling, detach/reattach) which still lives in `mod.rs`
//! because it touches many runner internals.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::tui::app::AppState;
use crate::tui::wiring::TuiEngineSession;

fn agent_view_task_snapshots(app: &AppState) -> Vec<rebon_plugin_tasks::runtime::TaskSnapshot> {
    app.agent_task_snapshots()
}

pub(super) fn refresh_agent_view_if_due(app: &mut AppState) {
    if !app
        .agent_view
        .as_ref()
        .is_some_and(|view| view.refresh_is_due(std::time::Instant::now()))
    {
        return;
    }
    refresh_agent_view(app);
}

pub(super) fn refresh_agent_view(app: &mut AppState) {
    if app.agent_view.is_none() {
        return;
    }
    crate::background::refresh_agent_view_supervisor_heartbeat();
    let store = crate::background::cli_default_store();
    let jobs = store.list_jobs().unwrap_or_default();
    let snapshots = agent_view_task_snapshots(app);
    if let Some(view) = app.agent_view.as_mut() {
        view.refresh(&store, jobs, &snapshots);
    }
}

/// Open the list over the session on screen. `this_terminal_job_id` is
/// the job that session lives in, if it lives in one — that job is not
/// listed, being the very screen the list is on; a
/// caller that has just left its session passes `None`.
pub(super) fn open_agent_view(app: &mut AppState, this_terminal_job_id: Option<&str>) {
    let store = crate::background::cli_default_store();
    open_agent_view_with_store(app, &store, None, this_terminal_job_id);
}

pub(super) fn open_agent_view_with_store(
    app: &mut AppState,
    store: &crate::background::BackgroundStore,
    status: Option<String>,
    this_terminal_job_id: Option<&str>,
) {
    crate::background::keep_supervisor_alive_for_agent_view();
    let jobs = store.list_jobs().unwrap_or_default();
    let snapshots = agent_view_task_snapshots(app);
    let mut view = crate::tui::agent_view::AgentViewState::open_with_grouping(
        store,
        jobs,
        &snapshots,
        None,
        load_agent_view_grouping_preference(),
        this_terminal_job_id.map(str::to_string),
    );
    if status.is_some() {
        view.status = status;
    }
    app.agent_view = Some(view);
}

pub(super) fn move_agent_view_job(app: &mut AppState, job_id: &str, up: bool) {
    let store = crate::background::cli_default_store();
    let rows = app
        .agent_view
        .as_ref()
        .map(|view| view.rows.clone())
        .unwrap_or_default();
    let Some(index) = rows.iter().position(|row| {
        row.source == crate::tui::agent_view::AgentViewRowSource::Job && row.id == job_id
    }) else {
        return;
    };
    let current_group = rows[index].group;
    let neighbor = if up {
        rows[..index].iter().rev().find(|row| {
            row.source == crate::tui::agent_view::AgentViewRowSource::Job
                && row.group == current_group
        })
    } else {
        rows[index.saturating_add(1)..].iter().find(|row| {
            row.source == crate::tui::agent_view::AgentViewRowSource::Job
                && row.group == current_group
        })
    };
    let Some(neighbor) = neighbor else {
        if let Some(view) = app.agent_view.as_mut() {
            view.status = Some("no job row to move past".to_string());
        }
        return;
    };
    let result = if up {
        store.move_job_before(job_id, &neighbor.id)
    } else {
        store.move_job_after(job_id, &neighbor.id)
    };
    match result {
        Ok(()) => {
            if let Some(view) = app.agent_view.as_mut() {
                view.status = Some(format!("moved {job_id} {}", if up { "up" } else { "down" }));
            }
            refresh_agent_view(app);
        }
        Err(err) => {
            if let Some(view) = app.agent_view.as_mut() {
                view.status = Some(format!("failed to move {job_id}: {err}"));
            }
        }
    }
}

pub(super) fn agent_view_dispatch_cwd(app: &AppState, session: &TuiEngineSession) -> PathBuf {
    let Some(view) = app.agent_view.as_ref() else {
        return PathBuf::from(&session.cwd);
    };
    view.selected_dispatch_cwd()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&session.cwd))
}

pub(super) fn background_prompt_for_detach(session_id: &str, final_prompt: Option<&str>) -> String {
    final_prompt
        .map(str::to_string)
        .unwrap_or_else(|| format!("Continue session {session_id}"))
}

pub(super) fn session_already_has_background_job(
    session_id: &str,
    except_job_id: Option<&str>,
) -> bool {
    crate::background::cli_default_store()
        .list_jobs()
        .map(|jobs| {
            jobs.into_iter().any(|job| {
                job.identity.session_id.as_deref() == Some(session_id)
                    && except_job_id != Some(job.identity.job_id.as_str())
                    && job.status() != crate::background::BackgroundJobStatus::Stopped
            })
        })
        .unwrap_or(false)
}

pub(super) fn has_active_detach_blocking_tasks(app: &AppState) -> usize {
    app.task_snapshots()
        .into_iter()
        .filter(|snapshot| {
            matches!(
                snapshot.status,
                rebon_plugin_tasks::runtime::TaskStatus::Pending
                    | rebon_plugin_tasks::runtime::TaskStatus::Running
            ) && !snapshot.is_backgrounded
        })
        .count()
}

/// Unfinished work that lives in **this process** and would die with this
/// terminal.
///
/// Deliberately counts backgrounded tasks too, unlike
/// [`has_active_detach_blocking_tasks`]. `is_backgrounded` routes a task to
/// the background indicator instead of the inline panel — it is a display
/// choice, not a statement that the work moved to another process. `/hosted`
/// promises a session that survives the terminal, so it has to count
/// everything that would not.
pub(super) fn has_terminal_bound_tasks(app: &AppState) -> usize {
    app.task_snapshots()
        .into_iter()
        .filter(|snapshot| {
            matches!(
                snapshot.status,
                rebon_plugin_tasks::runtime::TaskStatus::Pending
                    | rebon_plugin_tasks::runtime::TaskStatus::Running
            )
        })
        .count()
}

pub(super) fn should_detach_attached_session_on_ctrl_z(
    app: &AppState,
    session: &TuiEngineSession,
    key: &KeyEvent,
) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && matches!(key.code, KeyCode::Char('z') | KeyCode::Char('Z'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && attached_with_empty_prompt(app, session)
}

/// Ctrl+Z is the one key that takes an attached session back to the list.
///
/// Ctrl+C is deliberately not: on every attached session — opened from the
/// list, `rebon attach`, or handed over from this terminal — it means what
/// it means on a local one, cancel the running turn and then exit, with the
/// worker carrying on after the terminal is gone.
fn attached_with_empty_prompt(app: &AppState, session: &TuiEngineSession) -> bool {
    session.attached_background_job_id.is_some() && app.input.is_empty()
}

pub(super) fn load_agent_view_grouping_preference(
) -> crate::tui::agent_view::AgentViewGroupingPreference {
    crate::rebon_config::load_agent_view_preferences()
        .map(|prefs| {
            crate::tui::agent_view::AgentViewGroupingPreference::from_config(&prefs.grouping)
        })
        .unwrap_or(crate::tui::agent_view::AgentViewGroupingPreference::State)
}

pub(super) fn persist_agent_view_grouping_preference(
    grouping: crate::tui::agent_view::AgentViewGroupingPreference,
) {
    let prefs = crate::rebon_config::AgentViewPreferences {
        grouping: grouping.as_config().to_string(),
        disabled: false,
    };
    if let Err(err) = crate::rebon_config::save_agent_view_preferences(&prefs) {
        tracing::debug!(error = %err, "failed to persist Agent View grouping preference");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::test_support::make_test_tui_session;

    #[test]
    fn agent_view_snapshots_include_remote_external_acp_tasks() {
        let mut app = AppState::default();
        app.remote_background_tasks.insert(
            "agent-acp".into(),
            rebon_session_host::BackgroundTaskSnapshot {
                task: rebon_session_host::BackgroundTaskDescriptor {
                    task_id: "agent-acp".into(),
                    title: "Inspect ACP flow".into(),
                    kind: "local_agent".into(),
                    status: "running".into(),
                    is_backgrounded: true,
                    start_time_ms: 1,
                    end_time_ms: None,
                    last_progress: Some("streaming answer".into()),
                    error: None,
                    prompt: None,
                    parent_tool_call_id: Some("tool-agent".into()),
                    agent_id: Some("agent-acp".into()),
                    agent_name: Some("claude-advisor".into()),
                    agent_type: Some("claude-advisor".into()),
                    model: Some("acp:claude:agent-default".into()),
                    token_count: None,
                    tool_use_count: Some(0),
                    result: None,
                },
                updated_at_ms: 2,
                log_preview: Vec::new(),
                transcript: vec![
                    rebon_session_host::BackgroundTaskTranscriptEntry::Assistant {
                        text: "Visible from Agent View".into(),
                        timestamp_ms: 0,
                    },
                ],
            },
        );

        let snapshots = agent_view_task_snapshots(&app);

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].id.as_str(), "agent-acp");
        assert!(crate::tui::agent_switcher::is_external_acp_agent_snapshot(
            &snapshots[0]
        ));
    }

    #[test]
    fn ctrl_z_detaches_only_attached_empty_prompt_sessions() {
        let key = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL);
        let mut app = AppState::default();
        let mut session = make_test_tui_session();

        assert!(!should_detach_attached_session_on_ctrl_z(
            &app, &session, &key
        ));

        session.attached_background_job_id = Some("bg-1".into());
        assert!(should_detach_attached_session_on_ctrl_z(
            &app, &session, &key
        ));

        app.input = "keep undo available while editing".into();
        assert!(!should_detach_attached_session_on_ctrl_z(
            &app, &session, &key
        ));
    }

    #[test]
    fn background_prompt_for_detach_uses_visible_session_prompt() {
        assert_eq!(
            background_prompt_for_detach("session-123", None),
            "Continue session session-123"
        );
        assert_eq!(
            background_prompt_for_detach("session-123", Some("final note")),
            "final note"
        );
    }
}

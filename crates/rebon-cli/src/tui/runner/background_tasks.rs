//! Background-task keybinding runtime for Ctrl+B.

use crate::tui::app::AppState;

use super::live_agent_view::switch_to_main_agent;

/// Handle Ctrl+B via the state-machine dispatcher from
/// `rebon_tui::status::session_background_press`. Returns `true` when the
/// keybinding was active and consumed the press, `false` when the
/// caller should fall through to cursor-left.
pub(super) fn apply_background_tasks(
    app: &mut AppState,
    tasks: &rebon_plugin_tasks::runtime::TaskRegistry,
    is_loading: bool,
) -> bool {
    use rebon_plugin_tasks::runtime::TaskStatus;
    use rebon_tui::status::{
        session_background_keybinding_active, session_background_press, BackgroundPressOutcome,
        SessionBackgroundInputs,
    };

    let snapshots = app.task_snapshots();
    let has_foreground = snapshots.iter().any(|s| {
        matches!(s.status, TaskStatus::Running | TaskStatus::Pending)
            && !s.is_backgrounded
            && !rebon_plugin_tasks::runtime::is_agent_snapshot_idle(s)
    });

    // Active gate: consume Ctrl+B only when there is something visible
    // to background. Session background mode is disabled for this path,
    // so idle Ctrl+B can still fall through to cursor-left.
    if !session_background_keybinding_active(has_foreground, false, is_loading) {
        return false;
    }

    let outcome = session_background_press(SessionBackgroundInputs {
        background_tasks_disabled: std::env::var("REBON_DISABLE_BACKGROUND_TASKS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
        has_foreground_tasks: has_foreground,
        has_used_background_task: true,
        session_bg_enabled: false,
        is_loading,
    });

    match outcome {
        BackgroundPressOutcome::BackgroundForeground { .. } => {
            for snap in &snapshots {
                if matches!(snap.status, TaskStatus::Running | TaskStatus::Pending)
                    && !snap.is_backgrounded
                    && !rebon_plugin_tasks::runtime::is_agent_snapshot_idle(snap)
                {
                    tasks.set_backgrounded(&snap.id);
                }
            }
            switch_to_main_agent(app);
            true
        }
        BackgroundPressOutcome::DoublePressSession => true,
        BackgroundPressOutcome::Ignored => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_test_task(
        reg: &rebon_plugin_tasks::runtime::TaskRegistry,
        id: &str,
        status: rebon_plugin_tasks::runtime::TaskStatus,
        backgrounded: bool,
    ) {
        use rebon_plugin_tasks::runtime::{
            BashTaskKind, LocalShellData, TaskData, TaskId, TaskSnapshot,
        };
        let snapshot = TaskSnapshot {
            id: TaskId::new(id),
            kind: rebon_plugin_tasks::runtime::TaskKind::LocalShell,
            status,
            title: "cmd".into(),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: backgrounded,
            notified: false,
            start_time_ms: 0,
            end_time_ms: None,
            metadata: serde_json::json!({}),
            data: TaskData::LocalShell(LocalShellData {
                command: "echo".into(),
                exit_code: None,
                interrupted: false,
                display_kind: BashTaskKind::Bash,
                agent_id: None,
            }),
        };
        reg.insert(TaskId::new(id), snapshot, rebon_types::PromptCancel::new());
    }

    #[test]
    fn background_tasks_returns_false_when_no_tasks() {
        let mut app = AppState::new();
        let tasks = app.tasks.clone();
        assert!(!apply_background_tasks(&mut app, tasks.as_ref(), false));
        assert!(app.background_tasks_dialog.is_none());
    }

    #[test]
    fn background_tasks_returns_false_when_all_already_backgrounded() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_test_task(
            &reg,
            "s1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = std::sync::Arc::new(reg);
        let tasks = app.tasks.clone();
        assert!(!apply_background_tasks(&mut app, tasks.as_ref(), false));
        assert!(app.background_tasks_dialog.is_none());
    }

    #[test]
    fn background_tasks_backgrounds_running_foreground_without_opening_dialog() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_test_task(
            &reg,
            "s1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        insert_test_task(
            &reg,
            "s2",
            rebon_plugin_tasks::runtime::TaskStatus::Completed,
            false,
        );
        let reg = std::sync::Arc::new(reg);
        app.tasks = reg.clone();

        assert!(apply_background_tasks(&mut app, reg.as_ref(), false));
        assert!(
            reg.snapshot(&rebon_plugin_tasks::runtime::TaskId::new("s1"))
                .unwrap()
                .is_backgrounded
        );
        assert!(
            !reg.snapshot(&rebon_plugin_tasks::runtime::TaskId::new("s2"))
                .unwrap()
                .is_backgrounded
        );
        assert!(app.background_tasks_dialog.is_none());
    }

    #[test]
    fn background_tasks_pending_task_also_backgrounded() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_test_task(
            &reg,
            "s1",
            rebon_plugin_tasks::runtime::TaskStatus::Pending,
            false,
        );
        let reg = std::sync::Arc::new(reg);
        app.tasks = reg.clone();

        assert!(apply_background_tasks(&mut app, reg.as_ref(), false));
        assert!(
            reg.snapshot(&rebon_plugin_tasks::runtime::TaskId::new("s1"))
                .unwrap()
                .is_backgrounded
        );
    }
}

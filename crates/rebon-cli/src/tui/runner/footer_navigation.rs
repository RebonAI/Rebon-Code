use rebon_plugin_tasks::runtime::TaskSnapshot;
use rebon_plugin_tasks::ui::background_tasks_dialog::{
    BackgroundTasksDialogOpen, BackgroundTasksDialogState,
};
use rebon_tui::promptinput::footer_navigation::FooterItem;

use crate::tui::app::AppState;

use super::live_agent_view::{switch_to_live_agent, switch_to_main_agent};

/// Open the unfiltered background-task panel through the `ui-registry`
/// seat. `None` when the tasks plugin is off, in which case nothing opens.
fn open_tasks_panel(
    app: &AppState,
    snapshots: &[TaskSnapshot],
) -> Option<BackgroundTasksDialogState> {
    crate::tui::ui_registry::open_background_tasks(BackgroundTasksDialogOpen {
        snapshots: snapshots.to_vec(),
        foregrounded_task_id: app.foregrounded_task_id.clone(),
        ..BackgroundTasksDialogOpen::default()
    })
}

/// Direction of a footer-motion key press.
pub(super) enum FooterMotionDirection {
    Up,
    Down,
    Next,
    Previous,
}

/// Derive the ordered visible footer pill list for the current frame.
///
/// The bridge pill reports the independently running local RC service, not
/// a bridge attached to the foreground session.
pub(super) fn derive_footer_items(app: &AppState) -> Vec<FooterItem> {
    use rebon_tui::promptinput::footer_navigation::{build_footer_items, FooterVisibility};

    let snapshots = app.task_snapshots();
    let has_agent_switcher = !crate::tui::agent_switcher::build_agent_switcher_rows(
        &app.agent_task_snapshots(),
        !app.is_loading,
    )
    .is_empty();
    let team_input = rebon_plugin_tasks::ui::teams_view::build_team_status_input(&snapshots);
    let teams_visible =
        rebon_plugin_tasks::ui::teams::team_status::render_team_status(&team_input, false, false)
            .is_some();
    build_footer_items(FooterVisibility {
        tasks: rebon_plugin_tasks::ui::tasks_view::has_background_tasks(&snapshots)
            || has_agent_switcher,
        workflows: rebon_plugin_tasks::ui::tasks_view::has_session_workflows(&snapshots),
        teams: teams_visible,
        bridge: app.rc_status.is_visible(),
    })
}

/// Resolve a footer-motion key through the state-machine
/// `rebon_tui::promptinput::footer_motion` reducers and apply the resulting
/// mutations to [`AppState`]. Opens the background tasks dialog when
/// the plan asks for it.
pub(super) fn apply_footer_motion(app: &mut AppState, direction: FooterMotionDirection) {
    use rebon_tui::promptinput::footer_navigation::{resolve_visible_footer_selection, FooterItem};

    let snapshots = app.agent_task_snapshots();
    let agent_rows =
        crate::tui::agent_switcher::build_agent_switcher_rows(&snapshots, !app.is_loading);
    let agent_count = agent_rows.len();
    if agent_count == 0 {
        app.teammate_footer_index = 0;
    } else if app.teammate_footer_index >= agent_count {
        app.teammate_footer_index = agent_count - 1;
    }

    let footer_items = derive_footer_items(app);
    let footer_item_selected =
        resolve_visible_footer_selection(app.footer_selection, &footer_items);
    if footer_item_selected == Some(FooterItem::Tasks) {
        match direction {
            FooterMotionDirection::Previous if agent_count > 0 => {
                app.teammate_footer_index =
                    (app.teammate_footer_index + agent_count - 1) % agent_count;
                return;
            }
            FooterMotionDirection::Next if agent_count > 0 => {
                app.teammate_footer_index = (app.teammate_footer_index + 1) % agent_count;
                return;
            }
            FooterMotionDirection::Down if agent_count > 0 => {
                app.teammate_footer_index = (app.teammate_footer_index + 1) % agent_count;
                return;
            }
            FooterMotionDirection::Up if agent_count > 0 => {
                if app.teammate_footer_index == 0 {
                    app.footer_selection = None;
                } else {
                    app.teammate_footer_index -= 1;
                }
                return;
            }
            _ => {}
        }
    }

    match direction {
        FooterMotionDirection::Down if footer_item_selected == Some(FooterItem::Tasks) => {
            app.background_tasks_dialog = open_tasks_panel(app, &snapshots);
        }
        FooterMotionDirection::Up => {
            use rebon_tui::promptinput::footer_motion::{resolve_footer_up, FooterMotionInput};
            let input = FooterMotionInput {
                footer_items,
                footer_item_selected,
                tasks_selected: footer_item_selected == Some(FooterItem::Tasks),
                is_teammate_mode: false,
                in_process_teammate_count: agent_count.saturating_sub(1),
                teammate_footer_index: app.teammate_footer_index,
                internal_build: false,
                coordinator_task_count: 0,
                coordinator_task_index: -1,
                min_coordinator_index: 0,
            };
            if let Some(update) = resolve_footer_up(&input).selection_update {
                app.footer_selection = update.selection;
            }
        }
        FooterMotionDirection::Down => {
            use rebon_tui::promptinput::footer_motion::{resolve_footer_down, FooterMotionInput};
            let input = FooterMotionInput {
                footer_items,
                footer_item_selected,
                tasks_selected: footer_item_selected == Some(FooterItem::Tasks),
                is_teammate_mode: false,
                in_process_teammate_count: agent_count.saturating_sub(1),
                teammate_footer_index: app.teammate_footer_index,
                internal_build: false,
                coordinator_task_count: 0,
                coordinator_task_index: -1,
                min_coordinator_index: 0,
            };
            let plan = resolve_footer_down(&input);
            if let Some(update) = plan.selection_update {
                app.footer_selection = update.selection;
            }
            if plan.show_bashes_dialog {
                app.background_tasks_dialog = open_tasks_panel(app, &snapshots);
            }
        }
        FooterMotionDirection::Next | FooterMotionDirection::Previous => {}
    }
}

/// Resolve the footer's open-selected key through the state-machine
/// [`rebon_tui::promptinput::footer_actions::resolve_footer_open_selected_action`]
/// reducer and apply the resulting side effect.
pub(super) fn apply_footer_open_selected(app: &mut AppState) {
    use rebon_tui::promptinput::footer_actions::{
        resolve_footer_open_selected_action, FooterOpenSelectedAction, FooterOpenSelectedInput,
    };
    use rebon_tui::promptinput::footer_navigation::resolve_visible_footer_selection;

    let snapshots = app.agent_task_snapshots();
    let agent_rows =
        crate::tui::agent_switcher::build_agent_switcher_rows(&snapshots, !app.is_loading);
    if agent_rows.is_empty() {
        app.teammate_footer_index = 0;
    } else if app.teammate_footer_index >= agent_rows.len() {
        app.teammate_footer_index = agent_rows.len() - 1;
    }

    let footer_items = derive_footer_items(app);
    let footer_item_selected =
        resolve_visible_footer_selection(app.footer_selection, &footer_items);
    if footer_item_selected == Some(FooterItem::Tasks) {
        if app.teammate_footer_index == 0 {
            switch_to_main_agent(app);
            app.footer_selection = None;
            return;
        }
        if let Some(task_id) = agent_rows
            .get(app.teammate_footer_index)
            .and_then(|row| row.task_id.as_deref())
            .map(ToString::to_string)
        {
            let mut ignored_active_prompt = None;
            if switch_to_live_agent(app, &mut ignored_active_prompt, &task_id) {
                app.footer_selection = None;
            }
            return;
        }
    }

    if footer_item_selected == Some(FooterItem::Workflows) {
        app.background_tasks_dialog =
            crate::tui::ui_registry::open_background_tasks(BackgroundTasksDialogOpen {
                snapshots: snapshots.clone(),
                kind_filter: Some(rebon_plugin_tasks::runtime::TaskKind::LocalWorkflow),
                ..BackgroundTasksDialogOpen::default()
            });
        app.footer_selection = None;
        return;
    }

    let in_process_teammate_ids = agent_rows
        .iter()
        .skip(1)
        .filter_map(|row| row.task_id.clone())
        .collect::<Vec<_>>();

    let footer_items = derive_footer_items(app);
    let footer_item_selected =
        resolve_visible_footer_selection(app.footer_selection, &footer_items);

    let action = resolve_footer_open_selected_action(&FooterOpenSelectedInput {
        footer_item_selected,
        view_selection_mode: String::from("none"),
        is_teammate_mode: true,
        teammate_footer_index: app.teammate_footer_index,
        in_process_teammate_ids,
        coordinator_task_index: -1,
        visible_agent_tasks: vec![],
    });

    match action {
        FooterOpenSelectedAction::ShowBashesDialog => {
            app.background_tasks_dialog = open_tasks_panel(app, &snapshots);
            app.footer_selection = None;
        }
        FooterOpenSelectedAction::ShowTeamsDialog => {
            app.teams_dialog =
                rebon_plugin_tasks::ui::teams_dialog::TeamsDialogState::open(&snapshots, None);
            app.footer_selection = None;
        }
        FooterOpenSelectedAction::EnterTeammateView { task_id } => {
            let mut ignored_active_prompt = None;
            if switch_to_live_agent(app, &mut ignored_active_prompt, &task_id) {
                app.footer_selection = None;
            }
        }
        FooterOpenSelectedAction::ExitTeammateView => {
            switch_to_main_agent(app);
            app.footer_selection = None;
            app.teammate_footer_index = 0;
        }
        FooterOpenSelectedAction::ShowBridgeDialog => {
            app.dialogs
                .push(crate::tui::bridge_dialog::BridgeDialogState::new(
                    app.rc_status.clone(),
                ));
            app.footer_selection = None;
        }
        FooterOpenSelectedAction::ClearSelection | FooterOpenSelectedAction::None => {
            app.footer_selection = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_status_error_makes_the_footer_accessible() {
        let mut app = AppState::new();
        app.rc_status.error = Some("Cannot read local RC status".into());
        assert!(derive_footer_items(&app).contains(&FooterItem::Bridge));
    }

    #[test]
    fn bridge_footer_opens_the_shared_dialog_and_escape_closes_it() {
        let mut app = AppState::new();
        app.rc_status.error = Some("Cannot read local RC status".into());
        app.footer_selection = Some(FooterItem::Bridge);
        apply_footer_open_selected(&mut app);
        assert_eq!(app.dialogs.top_id(), Some("bridge"));
        assert!(app.has_fullscreen_dialog());
        assert_eq!(app.footer_selection, None);
        crate::tui::dialog_host::handle_key(
            &mut app.dialogs,
            &ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Esc,
                ratatui::crossterm::event::KeyModifiers::NONE,
            ),
        );
        assert!(!app.has_fullscreen_dialog());
    }

    #[test]
    fn bridge_footer_is_hidden_before_rc_is_configured() {
        let app = AppState::new();
        assert!(!derive_footer_items(&app).contains(&FooterItem::Bridge));
    }

    fn local_agent_snapshot(
        id: &str,
        status: rebon_plugin_tasks::runtime::TaskStatus,
        backgrounded: bool,
    ) -> rebon_plugin_tasks::runtime::TaskSnapshot {
        use rebon_plugin_tasks::runtime::{
            LocalAgentData, TaskData, TaskId, TaskKind, TaskSnapshot,
        };

        TaskSnapshot {
            id: TaskId::new(id),
            kind: TaskKind::LocalAgent,
            status,
            title: "worker".into(),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: backgrounded,
            notified: false,
            start_time_ms: 0,
            end_time_ms: None,
            metadata: serde_json::json!({}),
            data: TaskData::LocalAgent(LocalAgentData {
                prompt: "do work".into(),
                agent_type: "general-purpose".into(),
                model: None,
                system: None,
                allowed_tools: None,
                token_count: 0,
                tool_use_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
                pending_messages: Vec::new(),
                retrieved: false,
            }),
        }
    }

    fn insert_local_agent_task(
        reg: &rebon_plugin_tasks::runtime::TaskRegistry,
        id: &str,
        status: rebon_plugin_tasks::runtime::TaskStatus,
        backgrounded: bool,
    ) {
        let cancel = rebon_types::PromptCancel::new();
        let snapshot = local_agent_snapshot(id, status, backgrounded);
        reg.insert(
            rebon_plugin_tasks::runtime::TaskId::new(id),
            snapshot,
            cancel,
        );
    }

    fn set_local_agent_page(
        reg: &rebon_plugin_tasks::runtime::TaskRegistry,
        id: &str,
        title: &str,
        transcript: &[&str],
    ) {
        reg.update(&rebon_plugin_tasks::runtime::TaskId::new(id), |snapshot| {
            snapshot.title = title.to_string();
            let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data else {
                panic!("expected local agent");
            };
            data.transcript = transcript
                .iter()
                .map(
                    |text| rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry::Assistant {
                        text: (*text).to_string(),
                    },
                )
                .collect();
        });
    }

    fn insert_local_workflow_task(reg: &rebon_plugin_tasks::runtime::TaskRegistry, id: &str) {
        use rebon_plugin_tasks::runtime::{
            LocalWorkflowData, TaskData, TaskId, TaskKind, TaskSnapshot,
        };

        let snapshot = TaskSnapshot {
            id: TaskId::new(id),
            kind: TaskKind::LocalWorkflow,
            status: rebon_plugin_tasks::runtime::TaskStatus::Running,
            title: "workflow".into(),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: true,
            notified: false,
            start_time_ms: 0,
            end_time_ms: None,
            metadata: serde_json::json!({}),
            data: TaskData::LocalWorkflow(LocalWorkflowData {
                run_id: "wf_test".into(),
                workflow_name: "workflow".into(),
                summary: None,
                agent_count: 1,
                progress_entries: Vec::new(),
                token_count: 0,
                tool_use_count: 0,
                output_path: None,
                script_path: None,
                args: None,
            }),
        };
        reg.insert(
            rebon_plugin_tasks::runtime::TaskId::new(id),
            snapshot,
            rebon_types::PromptCancel::new(),
        );
    }

    #[test]
    fn footer_tasks_down_cycles_agent_rows_and_enter_opens_view_only() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        insert_local_agent_task(
            &reg,
            "agent-2",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        let reg = std::sync::Arc::new(reg);
        app.tasks = reg.clone();
        app.rebon_tui.flush_counter = 7;
        app.footer_selection = Some(FooterItem::Tasks);
        app.teammate_footer_index = 0;

        apply_footer_motion(&mut app, FooterMotionDirection::Down);
        assert_eq!(app.teammate_footer_index, 1);
        apply_footer_open_selected(&mut app);
        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-1"));
        assert_eq!(
            app.pending_page_hard_refresh.as_deref(),
            Some("Agent: worker")
        );
        assert_eq!(
            app.main_agent_view
                .as_ref()
                .expect("main view preserved")
                .tui
                .flush_counter,
            7
        );
        assert_eq!(app.footer_selection, None);
        assert!(
            reg.snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-1"))
                .unwrap()
                .is_backgrounded
        );
    }

    #[test]
    fn footer_workflows_enter_opens_workflow_dialog() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_workflow_task(&reg, "workflow-1");
        app.tasks = std::sync::Arc::new(reg);
        app.footer_selection = Some(FooterItem::Workflows);

        apply_footer_open_selected(&mut app);

        let dialog = app
            .background_tasks_dialog
            .as_ref()
            .expect("workflows dialog");
        assert_eq!(
            dialog.kind_filter,
            Some(rebon_plugin_tasks::runtime::TaskKind::LocalWorkflow)
        );
        assert_eq!(app.footer_selection, None);
    }

    #[test]
    fn footer_tasks_enter_on_main_restores_main_agent_view_only() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let reg = std::sync::Arc::new(reg);
        app.tasks = reg.clone();
        app.main_agent_view = Some(crate::tui::app::StoredTranscriptView::default());
        app.foregrounded_task_id = Some("agent-1".into());
        app.footer_selection = Some(FooterItem::Tasks);
        app.teammate_footer_index = 0;

        apply_footer_open_selected(&mut app);

        assert_eq!(app.foregrounded_task_id, None);
        assert_eq!(app.pending_page_hard_refresh.as_deref(), Some("Main"));
        assert_eq!(app.footer_selection, None);
        assert!(
            !reg.snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-1"))
                .unwrap()
                .is_backgrounded
        );
    }

    #[test]
    fn footer_repeated_enter_on_same_agent_does_not_request_refresh() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = std::sync::Arc::new(reg);
        app.footer_selection = Some(FooterItem::Tasks);
        app.teammate_footer_index = 1;

        apply_footer_open_selected(&mut app);
        assert!(app.pending_page_hard_refresh.is_some());
        app.pending_page_hard_refresh = None;
        app.footer_selection = Some(FooterItem::Tasks);
        app.teammate_footer_index = 1;

        apply_footer_open_selected(&mut app);

        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-1"));
        assert_eq!(app.pending_page_hard_refresh, None);
    }

    #[test]
    fn footer_agent_switches_keep_each_transcript_and_restore_main() {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-a",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        insert_local_agent_task(
            &reg,
            "agent-b",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        set_local_agent_page(&reg, "agent-a", "Alpha", &["short"]);
        set_local_agent_page(
            &reg,
            "agent-b",
            "Beta",
            &["long-1", "long-2", "long-3", "long-4"],
        );
        app.tasks = std::sync::Arc::new(reg);
        app.rebon_tui.flush_counter = 23;

        let open = |app: &mut AppState, index| {
            app.footer_selection = Some(FooterItem::Tasks);
            app.teammate_footer_index = index;
            apply_footer_open_selected(app);
        };

        open(&mut app, 1);
        assert_eq!(
            app.pending_page_hard_refresh.as_deref(),
            Some("Agent: Alpha")
        );
        assert_eq!(app.rebon_tui.transcript.len(), 2);

        open(&mut app, 2);
        assert_eq!(
            app.pending_page_hard_refresh.as_deref(),
            Some("Agent: Beta")
        );
        assert_eq!(app.rebon_tui.transcript.len(), 5);
        assert_eq!(
            app.local_agent_views
                .get("agent-a")
                .expect("alpha view saved")
                .tui
                .transcript
                .len(),
            2
        );

        open(&mut app, 1);
        assert_eq!(
            app.pending_page_hard_refresh.as_deref(),
            Some("Agent: Alpha")
        );
        assert_eq!(app.rebon_tui.transcript.len(), 2);
        assert_eq!(
            app.local_agent_views
                .get("agent-b")
                .expect("beta view saved")
                .tui
                .transcript
                .len(),
            5
        );

        open(&mut app, 0);
        assert_eq!(app.pending_page_hard_refresh.as_deref(), Some("Main"));
        assert_eq!(app.foregrounded_task_id, None);
        assert_eq!(app.rebon_tui.flush_counter, 23);
    }
}

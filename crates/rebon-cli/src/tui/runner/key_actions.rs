use tokio::runtime::Handle;

use rebon_tui::input::{should_swallow_event, PasteGateEvent};
use rebon_tui::promptinput::paste_flow::normalize_pasted_text;
use rebon_tui::promptinput::prompt_surface::extract_all_ref_positions;
use rebon_tui::promptinput::{plan_input_change, plan_input_event};
use rebon_tui::RenderTheme;

use rebon_slash_commands::help::HELP_TABS;

use crate::tui::app::{AppState, SelectionOwner};
use crate::tui::dispatch::{
    apply_text_edit, flush_queue, has_editable_queued_commands, history_down, history_up,
    pop_queued_command_into_input, pop_undo,
};
use crate::tui::event::{
    cursor_down_line, cursor_down_visual_line, cursor_end, cursor_home, cursor_left, cursor_right,
    cursor_up_line, cursor_up_visual_line, is_cursor_on_first_line, is_cursor_on_last_line,
    KeyAction,
};
use crate::tui::permission_modal::PendingPermission;
use crate::tui::wiring::TuiEngineSession;
use crate::ui_config::UiMode;

use super::background_tasks::apply_background_tasks;
use super::footer_navigation::{
    apply_footer_motion, apply_footer_open_selected, derive_footer_items, FooterMotionDirection,
};
use super::interrupt_flow::{
    apply_cancel_or_exit, apply_cancel_or_exit_without_session, apply_interrupt,
    apply_interrupt_without_session,
};
use super::layout_and_scroll::{apply_prompt_input_repin, snap_cursor_left, snap_cursor_right};
use super::mid_turn_submit_queue::reconcile_mid_turn_consumed_queued_submits;
use super::paste_burst::{apply_paste_from_clipboard, apply_paste_image_from_clipboard};
use super::permission_mode::{apply_cycle_permission_mode, cycle_permission_mode_before_session};
use super::prompt_lifecycle::maybe_spawn_next_queued_prompt;
use super::status_bar::wall_clock_ms;
use super::submit::submit_or_queue;
use super::transcript_messages::inject_local_command_feedback;
use super::ActivePrompt;
use crate::session::commands::effort::set_fast_mode;

enum HelpTabDirection {
    Previous,
    Next,
}

fn move_help_tab(app: &mut AppState, direction: HelpTabDirection) {
    if !app.help_open {
        return;
    }
    let tab_count = HELP_TABS.len();
    app.help_tab_index = match direction {
        HelpTabDirection::Previous => app.help_tab_index.checked_sub(1).unwrap_or(tab_count - 1),
        HelpTabDirection::Next => (app.help_tab_index + 1) % tab_count,
    };
}

pub(super) fn handle_key_action(
    app: &mut AppState,
    action: KeyAction,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    pending_permission: &mut Option<PendingPermission>,
    ui_mode: UiMode,
    _theme: &mut RenderTheme,
) -> bool {
    match action {
        KeyAction::Interrupt => {
            if dismiss_notice_or_selection_on_interrupt(app) {
                return false;
            }
            let had_active_prompt = active_prompt.is_some();
            let interrupted = apply_interrupt(app, session, active_prompt, ui_mode);
            if interrupted
                && had_active_prompt
                && active_prompt.is_none()
                && !app.queued_auto_drain_paused_after_withdrawal
                && !app.suppress_late_visible_updates_after_withdrawal
                && has_editable_queued_commands(app)
            {
                maybe_spawn_next_queued_prompt(app, session, handle, active_prompt);
            }
            false
        }
        KeyAction::CancelOrExit => {
            if app.agent_view.is_some() {
                return apply_cancel_or_exit(app, session, active_prompt, ui_mode);
            }
            // If there's an active selection, Ctrl+C triggers a copy
            // (deferred to next render where the buffer is available)
            // and clears the selection instead of canceling/exiting.
            if app.selection.has_selection() {
                app.pending_copy = true;
                return false;
            }
            // The same Ctrl+C on every session, attached or not: cancel
            // what is running, and a second press within the window exits.
            // An attached session's worker keeps running after the exit;
            // Ctrl+Z is the key that goes back to the list.
            apply_cancel_or_exit(app, session, active_prompt, ui_mode)
        }
        KeyAction::CursorLeft => {
            // On an empty prompt the key has nothing to move through, so the
            // second press in a row is free to mean something — and this is
            // the thing worth having within reach *before* you start typing.
            if super::session_detach_attach::host_session_on_double_left(
                app,
                session,
                active_prompt,
            ) {
                return false;
            }
            let chips = extract_all_ref_positions(&app.input);
            app.cursor_offset =
                snap_cursor_left(&chips, cursor_left(&app.input, app.cursor_offset));
            false
        }
        KeyAction::Submit(text) => {
            if !submit_key_reaches_the_prompt(app) {
                return false;
            }
            submit_or_queue(
                app,
                text,
                session,
                handle,
                active_prompt,
                pending_permission,
                ui_mode,
            )
        }
        KeyAction::CyclePermissionMode => {
            apply_cycle_permission_mode(app, session, active_prompt.is_some());
            false
        }
        KeyAction::ToggleFastMode => {
            let output = set_fast_mode(session, !session.model.service_tier.is_fast());
            super::transcript_messages::inject_fast_command_result(app, output, true);
            false
        }
        KeyAction::BackgroundTasks => {
            if !apply_background_tasks(
                app,
                session.engine_half.tasks.as_ref(),
                active_prompt.is_some(),
            ) {
                let chips = extract_all_ref_positions(&app.input);
                app.cursor_offset =
                    snap_cursor_left(&chips, cursor_left(&app.input, app.cursor_offset));
            }
            false
        }
        other => handle_app_only_key_action(app, other, ui_mode),
    }
}

/// The keys that work before the session exists: everything
/// that edits the composer or the view, plus the few session actions with
/// a session-less meaning — Enter goes to the worker's job or waits for the
/// session, Ctrl+C clears or exits, Esc clears, Shift+Tab cycles the mode
/// the session will inherit. Returns `true` to exit the loop.
pub(super) fn handle_key_action_without_session(
    app: &mut AppState,
    action: KeyAction,
    slot: &mut super::SessionSlot,
    ui_mode: UiMode,
) -> bool {
    match action {
        KeyAction::Interrupt => {
            if dismiss_notice_or_selection_on_interrupt(app) {
                return false;
            }
            apply_interrupt_without_session(app);
            false
        }
        KeyAction::CancelOrExit => apply_cancel_or_exit_without_session(app),
        KeyAction::CursorLeft => {
            // No session to hand over yet, so a double Left is just Left.
            let chips = extract_all_ref_positions(&app.input);
            app.cursor_offset =
                snap_cursor_left(&chips, cursor_left(&app.input, app.cursor_offset));
            false
        }
        KeyAction::Submit(text) => {
            if !submit_key_reaches_the_prompt(app) {
                return false;
            }
            super::startup_submit::submit_before_session(app, text, slot);
            false
        }
        KeyAction::CyclePermissionMode => {
            if cycle_permission_mode_before_session(app) {
                slot.note_permission_mode_cycled();
            }
            false
        }
        KeyAction::ToggleFastMode => {
            inject_local_command_feedback(
                app,
                "fast",
                "Fast mode can be toggled once the session is ready.",
            );
            app.follow_transcript_tail = true;
            false
        }
        other => handle_app_only_key_action(app, other, ui_mode),
    }
}

/// Esc's first duties, which need no session: dismiss an update notice,
/// or drop a selection. `true` when one of them consumed the press.
fn dismiss_notice_or_selection_on_interrupt(app: &mut AppState) -> bool {
    // Esc first dismisses a visible update notice and persists
    // the dismissed version quietly.
    if let Some(notice) = app.clear_update_notice() {
        let latest_version = notice.latest_version;
        let dismissed_at_ms = wall_clock_ms();
        if let Err(err) =
            crate::rebon_config::persist_update_dismissal(&latest_version, dismissed_at_ms)
        {
            tracing::debug!(%err, latest_version = %latest_version, "failed to persist update notice dismissal");
        }
        return true;
    }
    // Esc gives up on a command that is still expanding. The expansion is not
    // cancelled -- whoever registered the command decides when it returns --
    // but its answer is dropped on arrival rather than submitted to a person
    // who has moved on.
    if let Some(pending) = app.expanding_command.take() {
        super::inject_system_message(
            app,
            "command-expanding",
            &format!("Stopped waiting for /{} to expand.", pending.name),
        );
        app.follow_transcript_tail = true;
        return true;
    }
    // Esc clears selection if one exists, before doing anything else.
    if app.selection.has_selection() {
        app.selection.clear();
        app.selection_owner = SelectionOwner::Transcript;
        return true;
    }
    if has_editable_queued_commands(app) {
        flush_queue(app);
        app.last_esc_press_ms = 0;
    }
    false
}

/// Whether Enter is a submit at all right now, as opposed to closing the
/// help overlay or being part of a paste.
fn submit_key_reaches_the_prompt(app: &mut AppState) -> bool {
    if app.help_open {
        app.help_open = false;
        app.last_esc_press_ms = 0;
        return false;
    }
    // Paste gate: swallow Return during a bracketed paste to
    // prevent pasted newlines from triggering submit. With
    // crossterm this is structurally guarded (Event::Paste is
    // atomic), but we wire the check defensively.
    !should_swallow_event(app.is_pasting, &PasteGateEvent { return_key: true })
}

/// The key actions that touch only `AppState`: the composer, the help
/// overlay, history, footer navigation, output verbosity, the clipboard.
/// Shared by the session and the session-less handlers; the actions that
/// need a session are answered by those and never reach here.
fn handle_app_only_key_action(app: &mut AppState, action: KeyAction, ui_mode: UiMode) -> bool {
    match action {
        KeyAction::Ignored => false,
        KeyAction::Undo => {
            pop_undo(app);
            false
        }
        KeyAction::CursorRight => {
            let chips = extract_all_ref_positions(&app.input);
            app.cursor_offset =
                snap_cursor_right(&chips, cursor_right(&app.input, app.cursor_offset));
            false
        }
        KeyAction::CursorHome => {
            let chips = extract_all_ref_positions(&app.input);
            app.cursor_offset =
                snap_cursor_left(&chips, cursor_home(&app.input, app.cursor_offset));
            false
        }
        KeyAction::CursorEnd => {
            let chips = extract_all_ref_positions(&app.input);
            app.cursor_offset =
                snap_cursor_right(&chips, cursor_end(&app.input, app.cursor_offset));
            false
        }
        KeyAction::SetPromptMode(mode) => {
            app.mode = mode;
            app.help_open = false;
            app.last_esc_press_ms = 0;
            false
        }
        KeyAction::ToggleHelp => {
            app.help_open = !app.help_open;
            if app.help_open {
                app.help_tab_index = 0;
            }
            app.last_esc_press_ms = 0;
            false
        }
        KeyAction::HelpPreviousTab => {
            move_help_tab(app, HelpTabDirection::Previous);
            false
        }
        KeyAction::HelpNextTab => {
            move_help_tab(app, HelpTabDirection::Next);
            false
        }
        KeyAction::TextEdit(edit) => {
            apply_prompt_input_repin(app);
            let event_plan = plan_input_event(&edit.event_input);
            let change_plan = plan_input_change(&edit.change_input);
            apply_text_edit(app, &edit, &event_plan, &change_plan);
            false
        }
        KeyAction::ToggleToolOutput if ui_mode == UiMode::Inline => {
            use rebon_tui::ToolOutputVerbosity;
            app.tool_output_verbosity = match app.tool_output_verbosity {
                ToolOutputVerbosity::Compact | ToolOutputVerbosity::Normal => {
                    ToolOutputVerbosity::Verbose
                }
                ToolOutputVerbosity::Verbose => ToolOutputVerbosity::Compact,
            };
            false
        }
        KeyAction::ToggleToolOutput => {
            // If task list is collapsed, Ctrl+O first expands it.
            if app.task_list_collapsed {
                app.task_list_collapsed = false;
            } else {
                // Ctrl+O is the inline "open this collapsed output"
                // affordance, so it should reveal the full per-tool details.
                // Ctrl+E remains as an alias for the same show-all toggle.
                use rebon_tui::ToolOutputVerbosity;
                app.tool_output_verbosity = match app.tool_output_verbosity {
                    ToolOutputVerbosity::Compact | ToolOutputVerbosity::Normal => {
                        ToolOutputVerbosity::Verbose
                    }
                    ToolOutputVerbosity::Verbose => ToolOutputVerbosity::Compact,
                };
            }
            false
        }
        KeyAction::ShowAllOutput => {
            use rebon_tui::ToolOutputVerbosity;
            app.tool_output_verbosity = match app.tool_output_verbosity {
                ToolOutputVerbosity::Verbose => ToolOutputVerbosity::Compact,
                _ => ToolOutputVerbosity::Verbose,
            };
            false
        }
        KeyAction::PromptUp => {
            if prompt_input_width(app)
                .and_then(|width| cursor_up_visual_line(&app.input, app.cursor_offset, width))
                .or_else(|| {
                    (prompt_input_width(app).is_none()
                        && !is_cursor_on_first_line(&app.input, app.cursor_offset))
                    .then(|| cursor_up_line(&app.input, app.cursor_offset))
                    .flatten()
                })
                .is_some_and(|pos| {
                    app.cursor_offset = pos;
                    true
                })
            {
                return false;
            }
            reconcile_mid_turn_consumed_queued_submits(app);
            if has_editable_queued_commands(app) {
                pop_queued_command_into_input(app);
            } else {
                history_up(app);
            }
            false
        }
        KeyAction::PromptDown => {
            if prompt_input_width(app)
                .and_then(|width| cursor_down_visual_line(&app.input, app.cursor_offset, width))
                .or_else(|| {
                    (prompt_input_width(app).is_none()
                        && !is_cursor_on_last_line(&app.input, app.cursor_offset))
                    .then(|| cursor_down_line(&app.input, app.cursor_offset))
                    .flatten()
                })
                .is_some_and(|pos| {
                    app.cursor_offset = pos;
                    true
                })
            {
                return false;
            }
            if app.input.is_empty() {
                let items = derive_footer_items(app);
                if let Some(entered) = rebon_tui::promptinput::enter_footer_from_history(
                    true, &items, /* has_seen_tasks_hint */ true,
                ) {
                    app.footer_selection = entered.selection;
                }
            } else {
                history_down(app);
            }
            false
        }
        KeyAction::FooterUp => {
            apply_footer_motion(app, FooterMotionDirection::Up);
            false
        }
        KeyAction::FooterDown => {
            apply_footer_motion(app, FooterMotionDirection::Down);
            false
        }
        KeyAction::FooterNext => {
            apply_footer_motion(app, FooterMotionDirection::Next);
            false
        }
        KeyAction::FooterPrevious => {
            apply_footer_motion(app, FooterMotionDirection::Previous);
            false
        }
        KeyAction::FooterClear => {
            app.footer_selection = None;
            false
        }
        KeyAction::FooterOpenSelected => {
            apply_footer_open_selected(app);
            false
        }
        KeyAction::BackgroundTasks => {
            // No session means no task registry to open. Preserve the legacy
            // idle Ctrl+B behavior as backward-char until the session arrives.
            let chips = extract_all_ref_positions(&app.input);
            app.cursor_offset =
                snap_cursor_left(&chips, cursor_left(&app.input, app.cursor_offset));
            false
        }
        // Answered by the callers, which hold (or lack) the session.
        KeyAction::Interrupt
        | KeyAction::CancelOrExit
        | KeyAction::CursorLeft
        | KeyAction::Submit(_)
        | KeyAction::CyclePermissionMode
        | KeyAction::ToggleFastMode => false,
        KeyAction::PasteImageFromClipboard => {
            apply_prompt_input_repin(app);
            if app
                .agent_view
                .as_ref()
                .is_some_and(|view| view.is_input_focused())
            {
                if let Some(image) = crate::tui::clipboard_image::read_clipboard_image_or_path() {
                    if let Some(view) = app.agent_view.as_mut() {
                        view.paste_image(image);
                    }
                }
            } else {
                apply_paste_image_from_clipboard(app);
            }
            false
        }
        KeyAction::PasteFromClipboard => {
            apply_prompt_input_repin(app);
            if app
                .agent_view
                .as_ref()
                .is_some_and(|view| view.is_input_focused())
            {
                if let Some(image) = crate::tui::clipboard_image::read_clipboard_image_or_path() {
                    if let Some(view) = app.agent_view.as_mut() {
                        view.paste_image(image);
                    }
                } else if let Some(view) = app.agent_view.as_mut() {
                    if let Ok(mut cb) = arboard::Clipboard::new() {
                        if let Ok(text) = cb.get_text() {
                            if !text.is_empty() {
                                view.paste_text(&text);
                                // Drop a possible terminal echo of the
                                // same gesture (see paste_echo docs).
                                if super::paste_echo::paste_echo_enabled() {
                                    let expected = normalize_pasted_text(&text);
                                    if !expected.is_empty() {
                                        app.paste_echo = Some(
                                            super::paste_echo::PasteEchoSuppressor::for_direct_key_paste(
                                                expected,
                                                std::time::Instant::now(),
                                            ),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                apply_paste_from_clipboard(app);
            }
            false
        }
        // Scroll actions are consumed earlier in the event loop and
        // never reach this function.
        KeyAction::ScrollUp
        | KeyAction::ScrollDown
        | KeyAction::PageUp
        | KeyAction::PageDown
        | KeyAction::ScrollHome
        | KeyAction::ScrollEnd => false,
    }
}

fn prompt_input_width(app: &AppState) -> Option<usize> {
    app.last_prompt_input_area
        .map(|area| area.width as usize)
        .filter(|width| *width > 0)
}

#[cfg(test)]
mod tests {
    use rebon_tui::{RenderTheme, ToolOutputVerbosity};
    use tempfile::TempDir;
    use tokio::runtime::{Builder, Handle, Runtime};
    use tokio::sync::oneshot;

    use crate::session::submit_payload::SubmitPayload;
    use crate::tui::app::AppState;
    use crate::tui::dispatch::{enqueue_submit_payload, queued_text};
    use crate::tui::event::KeyAction;
    use crate::ui_config::UiMode;

    use super::super::prompt_lifecycle::maybe_spawn_next_queued_prompt;
    use super::super::test_support::{insert_local_agent_task, make_test_tui_session};
    use super::super::ActivePrompt;
    use super::handle_key_action;

    fn make_immediate_handle() -> (Runtime, Handle) {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let handle = runtime.handle().clone();
        (runtime, handle)
    }

    #[derive(Default)]
    struct RecordingPromptExecutor {
        requests: std::sync::Mutex<Vec<rebon_agent_core::PromptRequest>>,
    }

    #[async_trait::async_trait]
    impl rebon_agent_core::PromptExecutor for RecordingPromptExecutor {
        async fn execute(
            &self,
            request: rebon_agent_core::PromptRequest,
        ) -> Result<rebon_agent_core::PromptOutcome, rebon_agent_core::PromptExecutorError>
        {
            self.requests.lock().expect("requests lock").push(request);
            Ok(rebon_agent_core::PromptOutcome::end_turn())
        }
    }

    impl RecordingPromptExecutor {
        fn take_requests(&self) -> Vec<rebon_agent_core::PromptRequest> {
            std::mem::take(&mut *self.requests.lock().expect("requests lock"))
        }
    }

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
    fn agent_view_cancel_or_exit_bypasses_transcript_selection() {
        let (runtime, handle) = make_immediate_handle();
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        // Use an in-process agent task (it surfaces as an Agent View row) and
        // select it, so the narrowed first Ctrl+C stops exactly this agent.
        insert_local_agent_task(
            &reg,
            "agent-selected",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = std::sync::Arc::new(reg);
        let mut view =
            crate::tui::agent_view::AgentViewState::open(&store, Vec::new(), &app.task_snapshots());
        assert!(view.select_task_row_for_test("agent-selected"));
        app.agent_view = Some(view);
        app.selection.start(0, 0);
        app.selection.update(1, 0);
        app.selection.finish();
        let mut session = make_test_tui_session();
        session.engine_half.tasks = app.tasks.clone();
        let mut active_prompt = None;

        assert!(!handle_key_action(
            &mut app,
            KeyAction::CancelOrExit,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
            &mut RenderTheme::default(),
        ));

        assert_eq!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-selected"))
                .expect("task snapshot")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Killed
        );
        assert!(!app.pending_copy);
        assert!(app.selection.has_selection());
        drop(runtime);
    }

    #[test]
    fn agent_view_cancel_or_exit_bypasses_attached_session_detach() {
        let (runtime, handle) = make_immediate_handle();
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "keep running".into(),
                std::path::PathBuf::from("."),
                crate::background::BackgroundRuntimeFields {
                    provider: None,
                    model: None,
                    fast_mode: None,
                    channels: Vec::new(),
                    development_channels: Vec::new(),
                    provider_format: None,
                    ui_mode: None,
                    effort_level: None,
                    permission_mode: None,
                    capability_mode: rebon_types::AgentCapabilityMode::Normal,
                    settings: Vec::new(),
                    add_dirs: Vec::new(),
                    plugin_dirs: Vec::new(),
                    mcp_configs: Vec::new(),
                    strict_mcp_config: false,
                },
            )
            .unwrap();
        job.process.status = crate::background::BackgroundJobStatus::Running;
        store.write_state(&job).unwrap();

        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-mounted",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = std::sync::Arc::new(reg);
        let mut view = crate::tui::agent_view::AgentViewState::open(
            &store,
            store.list_jobs().unwrap(),
            &app.task_snapshots(),
        );
        assert!(view.select_task_row_for_test("agent-mounted"));
        app.agent_view = Some(view);
        let mut session = make_test_tui_session();
        session.engine_half.tasks = app.tasks.clone();
        session.attached_background_job_id = Some(job.identity.job_id.clone());
        let mut active_prompt = None;

        assert!(!handle_key_action(
            &mut app,
            KeyAction::CancelOrExit,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
            &mut RenderTheme::default(),
        ));
        assert_eq!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-mounted"))
                .expect("task snapshot")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Killed
        );
        assert_eq!(
            session.attached_background_job_id.as_deref(),
            Some(job.identity.job_id.as_str())
        );
        assert!(app.agent_view.is_some());
        assert_eq!(
            store
                .read_state(&job.identity.job_id)
                .unwrap()
                .process
                .status,
            crate::background::BackgroundJobStatus::Running
        );

        assert!(handle_key_action(
            &mut app,
            KeyAction::CancelOrExit,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
            &mut RenderTheme::default(),
        ));
        assert_eq!(
            store
                .read_state(&job.identity.job_id)
                .unwrap()
                .process
                .status,
            crate::background::BackgroundJobStatus::Running
        );
        drop(runtime);
    }

    #[test]
    fn enter_closes_help_overlay_without_submitting() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        app.help_open = true;
        app.input = String::from("draft");
        app.cursor_offset = app.input.len();
        let mut session = make_test_tui_session();
        let mut active_prompt: Option<ActivePrompt> = None;

        assert!(!handle_key_action(
            &mut app,
            KeyAction::Submit(String::from("draft")),
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
            &mut RenderTheme::default(),
        ));

        assert!(!app.help_open);
        assert_eq!(app.input, "draft");
        assert!(active_prompt.is_none());
        assert!(app.rebon_tui.transcript.is_empty());
        drop(runtime);
    }

    #[test]
    fn esc_cancels_running_work_and_starts_next_queued_message() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let recorder = std::sync::Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        app.mode = "prompt".into();
        app.input = "draft".into();
        app.cursor_offset = 5;
        enqueue_submit_payload(
            &mut app,
            SubmitPayload {
                text: "already queued".into(),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            },
        );
        let (_tx, rx) = oneshot::channel();
        let cancel = rebon_types::PromptCancel::new();
        let mut active = Some(ActivePrompt::new(rx, cancel.clone()));

        assert!(!handle_key_action(
            &mut app,
            KeyAction::Interrupt,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
            &mut RenderTheme::default(),
        ));
        runtime.block_on(tokio::task::yield_now());

        assert!(cancel.is_cancelled());
        assert!(active.is_some());
        assert!(app.is_loading);
        assert!(app.input.is_empty());
        assert_eq!(app.cursor_offset, 0);
        assert_eq!(app.queued_commands.len(), 1);
        assert_eq!(queued_text(&app.queued_commands[0]), Some("draft"));
        assert_eq!(app.queued_submit_payloads.len(), 1);
        assert_eq!(app.queued_submit_payloads[0].text, "draft");
        assert!(!app.suppress_late_visible_updates_after_withdrawal);
        assert!(!app.queued_auto_drain_paused_after_withdrawal);
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        let rows = app.rebon_tui.transcript.rows();
        assert!(
            matches!(&rows[0], rebon_tui::Message::User(user) if user.message.content.iter().any(|block| matches!(block, rebon_tui::UserContentBlock::Text(text) if text.text == "already queued")))
        );
        assert_eq!(app.last_esc_press_ms, 0);

        drop(active.take());
        runtime.shutdown_background();
    }

    #[test]
    fn esc_cancels_responding_queued_prompt_and_starts_next_queued_message() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let recorder = std::sync::Arc::new(RecordingPromptExecutor::default());
        session.set_test_executor(recorder.clone());
        for text in ["queued A", "queued B"] {
            enqueue_submit_payload(
                &mut app,
                SubmitPayload {
                    text: text.into(),
                    model_text: None,
                    user_message_uuid: None,
                    image_pastes: Vec::new(),
                    directory_attachments: Vec::new(),
                    execution_policy: None,
                    skill_invocations: Vec::new(),
                },
            );
        }
        let mut active = None;
        maybe_spawn_next_queued_prompt(&mut app, &mut session, &handle, &mut active);
        runtime.block_on(tokio::task::yield_now());
        let initial_requests = recorder.take_requests();
        assert_eq!(initial_requests.len(), 1);
        assert!(matches!(
            &initial_requests[0].prompt[0],
            rebon_types::ContentBlock::Text(text) if text.text == "queued A"
        ));
        if let Some(active) = active.as_mut() {
            active.reply_started = true;
        }
        app.rebon_tui.overlay.set_streaming_text("partial A");

        assert!(!handle_key_action(
            &mut app,
            KeyAction::Interrupt,
            &mut session,
            &handle,
            &mut active,
            &mut None,
            UiMode::Screen,
            &mut RenderTheme::default(),
        ));
        runtime.block_on(tokio::task::yield_now());

        assert!(active.is_some());
        assert!(app.queued_commands.is_empty());
        assert!(app.queued_submit_payloads.is_empty());
        assert!(!app.queued_auto_drain_paused_after_withdrawal);
        assert!(!app.suppress_late_visible_updates_after_withdrawal);
        let requests = recorder.take_requests();
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            &requests[0].prompt[0],
            rebon_types::ContentBlock::Text(text) if text.text == "queued B"
        ));
        let rows = app.rebon_tui.transcript.rows();
        assert!(rows.iter().any(|row| matches!(
            row,
            rebon_tui::Message::User(user) if user.message.content.iter().any(|block| matches!(block, rebon_tui::UserContentBlock::Text(text) if text.text == "queued B"))
        )));

        drop(active.take());
        runtime.shutdown_background();
    }

    #[test]
    fn toggle_tool_output_uses_verbose_mode_in_inline_live_region() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let mut active_prompt: Option<ActivePrompt> = None;

        app.tool_output_verbosity = ToolOutputVerbosity::Compact;
        assert!(!handle_key_action(
            &mut app,
            KeyAction::ToggleToolOutput,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Inline,
            &mut RenderTheme::default(),
        ));
        assert_eq!(app.tool_output_verbosity, ToolOutputVerbosity::Verbose);

        assert!(!handle_key_action(
            &mut app,
            KeyAction::ToggleToolOutput,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Inline,
            &mut RenderTheme::default(),
        ));
        assert_eq!(app.tool_output_verbosity, ToolOutputVerbosity::Compact);
        drop(runtime);
    }

    #[test]
    fn toggle_tool_output_uses_verbose_mode_in_screen() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let mut active_prompt: Option<ActivePrompt> = None;

        app.tool_output_verbosity = ToolOutputVerbosity::Compact;
        assert!(!handle_key_action(
            &mut app,
            KeyAction::ToggleToolOutput,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
            &mut RenderTheme::default(),
        ));
        assert_eq!(app.tool_output_verbosity, ToolOutputVerbosity::Verbose);

        assert!(!handle_key_action(
            &mut app,
            KeyAction::ToggleToolOutput,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
            &mut RenderTheme::default(),
        ));
        assert_eq!(app.tool_output_verbosity, ToolOutputVerbosity::Compact);
        drop(runtime);
    }

    #[test]
    fn prompt_down_from_empty_prompt_enters_footer_when_items_visible() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_test_task(
            &reg,
            "s1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = std::sync::Arc::new(reg);
        let mut session = make_test_tui_session();
        let mut active_prompt: Option<ActivePrompt> = None;

        assert!(!handle_key_action(
            &mut app,
            KeyAction::PromptDown,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
            &mut RenderTheme::default(),
        ));

        assert_eq!(
            app.footer_selection,
            Some(rebon_tui::promptinput::footer_navigation::FooterItem::Tasks)
        );
        assert!(app.input.is_empty());
        drop(runtime);
    }

    #[test]
    fn prompt_down_from_nonempty_draft_does_not_enter_footer() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_test_task(
            &reg,
            "s1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        app.tasks = std::sync::Arc::new(reg);
        app.input = "draft".into();
        app.cursor_offset = app.input.len();
        let mut session = make_test_tui_session();
        let mut active_prompt: Option<ActivePrompt> = None;

        assert!(!handle_key_action(
            &mut app,
            KeyAction::PromptDown,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
            &mut RenderTheme::default(),
        ));

        assert!(app.footer_selection.is_none());
        assert_eq!(app.input, "draft");
        drop(runtime);
    }

    #[test]
    fn prompt_up_inside_wrapped_line_moves_cursor_instead_of_history() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        app.last_prompt_input_area = Some(ratatui::layout::Rect::new(0, 0, 3, 3));
        app.input = "abcdef".into();
        app.cursor_offset = 4;
        app.history = vec![crate::session::input_history::HistoryEntry {
            display: "history".into(),
            pasted_contents: Vec::new(),
            timestamp: 1,
        }];
        let mut session = make_test_tui_session();
        let mut active_prompt: Option<ActivePrompt> = None;

        assert!(!handle_key_action(
            &mut app,
            KeyAction::PromptUp,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
            &mut RenderTheme::default(),
        ));

        assert_eq!(app.input, "abcdef");
        assert_eq!(app.cursor_offset, 1);
        assert_eq!(app.history_index, 0);
        drop(runtime);
    }

    #[test]
    fn prompt_down_inside_wrapped_line_moves_cursor_instead_of_footer() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_test_task(
            &reg,
            "s1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        app.tasks = std::sync::Arc::new(reg);
        app.last_prompt_input_area = Some(ratatui::layout::Rect::new(0, 0, 3, 3));
        app.input = "abcdef".into();
        app.cursor_offset = 1;
        let mut session = make_test_tui_session();
        let mut active_prompt: Option<ActivePrompt> = None;

        assert!(!handle_key_action(
            &mut app,
            KeyAction::PromptDown,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
            &mut RenderTheme::default(),
        ));

        assert_eq!(app.input, "abcdef");
        assert_eq!(app.cursor_offset, 4);
        assert!(app.footer_selection.is_none());
        drop(runtime);
    }
    #[test]
    fn background_tasks_key_moves_cursor_left_when_idle() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        app.input = "hello".into();
        app.cursor_offset = 3;
        let mut session = make_test_tui_session();
        let mut active_prompt: Option<ActivePrompt> = None;

        handle_key_action(
            &mut app,
            KeyAction::BackgroundTasks,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut None,
            UiMode::Screen,
            &mut RenderTheme::default(),
        );

        assert_eq!(app.cursor_offset, 2);
        assert!(app.background_tasks_dialog.is_none());
        drop(runtime);
    }

    #[test]
    fn toggle_fast_key_action_emits_local_feedback() {
        let _guard = crate::test_env::lock_env();
        let tmp = TempDir::new().unwrap();
        let previous_config_dir = std::env::var_os("REBON_CONFIG_DIR");
        std::env::set_var("REBON_CONFIG_DIR", tmp.path());

        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        session.model.service_tier_available = true;
        let mut active_prompt = None;
        let mut pending_permission = None;
        let mut theme = RenderTheme::default();

        let should_quit = handle_key_action(
            &mut app,
            KeyAction::ToggleFastMode,
            &mut session,
            &handle,
            &mut active_prompt,
            &mut pending_permission,
            UiMode::Screen,
            &mut theme,
        );

        match previous_config_dir {
            Some(value) => std::env::set_var("REBON_CONFIG_DIR", value),
            None => std::env::remove_var("REBON_CONFIG_DIR"),
        }

        assert!(!should_quit);
        assert!(session.model.service_tier.is_fast());
        assert!(app.follow_transcript_tail);
        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(rows.len(), 1);
        let rebon_tui::Message::System(system) = &rows[0] else {
            panic!("expected local feedback system row");
        };
        assert_eq!(system.subtype, "local_command");
        assert!(system.uuid.starts_with("s-fast-"));
        let content = system.content.as_deref().unwrap_or("");
        assert!(content.contains("Fast mode"), "{content}");
        drop(runtime);
    }
}

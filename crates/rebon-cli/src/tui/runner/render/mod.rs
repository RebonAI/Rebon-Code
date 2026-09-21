//! TUI rendering — frame composition, header/footer, transcript, dialogs, overlays, and text utilities.

use std::time::SystemTime;

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui::Frame;
use rebon_width::WidthStr;

use rebon_dialog::settings_usage::format_cost_default;
use rebon_permissions::permission_mode_symbol;
use rebon_shell::shell_time_display::{format_duration, DurationFormatOptions};
use rebon_tui::input::{should_show_argument_hint, ArgumentHintInput};
use rebon_tui::layout::fullscreen_layout::{self, LayoutInput, LayoutZones, StickyPrompt};
use rebon_tui::layout::new_messages_pill;
use rebon_tui::layout::unseen_divider;
use rebon_tui::promptinput::{
    build_queue_display, resolve_task_list_layout, task_icon, CompletionTimestamp, ListTask,
    PromptInputRuntimeState, QueueDisplayInput, QueueDisplayItem, QueueDisplayLayout,
    TaskListLayoutInput, TaskListStatus,
};
use rebon_tui::render::animated_asterisk::animated_asterisk_state;
use rebon_tui::{
    parse_theme_color, render_prompt_input, render_transcript_cached_with_running_hints,
    RenderTheme,
};
use rebon_types::SessionUpdateParams;

use crate::session::settings_rows::SettingsDialogView;
use crate::session::ultraplan_run::UltraplanPhase;
use crate::tui::app::{AppState, SelectionOwner};
use crate::tui::dispatch::visible_queue_len;
use crate::tui::permission_modal::{
    measure_permission_inline, render_permission_inline_with_cursor,
};
use crate::tui::ultraplan_widget;
use crate::tui::update::translate_session_update;
use crate::tui::wiring::TuiEngineSession;

use super::StatusBarInfo;
use crate::session::commands::fmt_tokens;

const RESUME_DIALOG_MIN_HEIGHT_ROWS: u16 = 30;

mod config;
mod custom_status_line;
mod dialogs;
mod footer;
mod frame;
mod header;
mod inline;
pub(crate) use inline::InlineTailMeasureSlot;
mod landing;
mod notices;
mod pickers;
mod prompt;
mod tasks;
mod transcript_area;
mod updates;

use config::*;
use custom_status_line::*;
use dialogs::*;
use footer::*;
use header::*;
use inline::*;
use landing::*;
use notices::*;
use pickers::*;
use prompt::*;
use tasks::*;
use transcript_area::*;

#[allow(unused_imports)]
pub(super) use config::{build_settings_dialog_view, ensure_ui_mode_config_option};
pub(super) use frame::render_frame;
pub(super) use inline::{
    desired_inline_viewport_height, inline_live_content_overflows_viewport, render_inline_frame,
    InlineViewportHeightInput,
};
#[allow(unused_imports)]
pub(super) use rebon_width::truncate_to_ellipsis;
pub(super) use tasks::collect_task_views;
pub(crate) use updates::drain_pending_updates;
pub(super) use updates::{drain_file_list, note_cancel_race, shorten_cwd};

fn tasks_footer_is_selected(app: &AppState) -> bool {
    app.footer_selection == Some(rebon_tui::promptinput::footer_navigation::FooterItem::Tasks)
        && agent_switcher_height(app) == 0
}

/// Rows the progress strip above the prompt wants.
///
/// The compaction bar and the `/ultraplan` widget share one layout slot:
/// they never both apply (an ultraplan run drives turns, and `/compact` only
/// starts an immediate run at an idle prompt), and giving each its own chunk
/// would cost every layout site a constraint that is zero almost always.
/// Compaction wins the tie because it is the shorter-lived of the two and
/// the user is waiting on it right now.
fn progress_widget_height(app: &AppState, max_height: u16) -> u16 {
    let compacting = crate::tui::compact_widget::desired_height(app, max_height);
    if compacting > 0 {
        compacting
    } else {
        ultraplan_widget::desired_height(app, max_height)
    }
}

fn render_progress_widget(frame: &mut Frame, area: Rect, app: &AppState, theme: &RenderTheme) {
    if crate::tui::compact_widget::desired_height(app, area.height) > 0 {
        crate::tui::compact_widget::render(frame, area, app, theme);
    } else {
        ultraplan_widget::render(frame, area, app, theme);
    }
}

fn transcript_render_extras<'a>(
    foregrounded_task_id: Option<&str>,
    live_agent_tool_activity: &'a std::collections::HashMap<
        String,
        rebon_tui::LiveAgentToolActivity,
    >,
    live_activity_revision: u64,
    auto_mode_allowed_tool_ids: &'a std::collections::HashMap<
        String,
        rebon_types::AutoModeAllowSource,
    >,
) -> rebon_tui::TranscriptRenderExtras<'a> {
    // A foregrounded task renders its own transcript, whose tool ids this
    // session's overlays say nothing about.
    if foregrounded_task_id.is_some() {
        rebon_tui::TranscriptRenderExtras::empty()
    } else {
        rebon_tui::TranscriptRenderExtras {
            live_agent_tool_activity,
            live_activity_revision,
            auto_mode_allowed_tool_ids,
            ..rebon_tui::TranscriptRenderExtras::empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::status_bar::GoalActivityInfo;
    use super::*;
    use super::{
        frame::clear_chunk_background,
        updates::{FILE_LIST_DRAIN_PATH_BUDGET, FILE_LIST_DRAIN_UPDATE_BUDGET},
    };
    use crate::file_scanner::{FileListUpdate, FileScanStatus};
    use crate::session::ultraplan_run::UltraplanStatus;
    use crate::tui::permission_modal::{
        AskUserQuestionAnswer, AskUserQuestionEntry, AskUserQuestionOption, PendingPermission,
        PermissionKind, PermissionModalAction, PermissionModalView, PermissionOptionView,
    };
    use crate::tui::runner::test_support::make_test_tui_session;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;
    use rebon_core::permission::{
        OutboundPermissionQuery, PermissionOptionKind, PermissionQueryOption,
    };
    use rebon_plugin_updater::UpdateNoticeState;
    use rebon_tui::promptinput::derive_prompt_input_runtime_state;
    use rebon_tui::{
        AssistantContentBlock, AssistantMessage, AssistantMessageInner, AssistantRole,
        AssistantTextBlock, AssistantToolUseBlock, Message, ToolOutputVerbosity, TranscriptStore,
        UserContentBlock, UserMessage, UserMessageInner, UserRole, UserTextBlock,
    };
    use rebon_types::{
        ContentBlock, ImageContent, SessionUpdate, SessionUpdateParams, TextContent, ToolCallStatus,
    };
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

    fn app_with_ultraplan_phase(phase: UltraplanPhase) -> AppState {
        let mut app = AppState::default();
        app.ultraplan_status = Some(UltraplanStatus {
            run_id: String::from("ultraplan-test"),
            phase,
            task_title: String::from("test task"),
            started_at_ms: None,
            worker_count: None,
            context: None,
            round: 1,
            last_verdict: None,
            last_coverage: None,
            execution_reexploration_count: 0,
        });
        app
    }

    fn update_notice() -> UpdateNoticeState {
        UpdateNoticeState {
            current_version: "1.0.0".into(),
            latest_version: "9.9.9".into(),
            package_name: "rebon".into(),
            command: "npm install -g @rebon/cli".into(),
            checked_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn compacting_app() -> AppState {
        let mut app = AppState::new();
        app.rebon_tui.transcript = TranscriptStore::from_rows(vec![user("u1", "hello")]);
        app.compact_run = Some(crate::tui::app::CompactRunState {
            phase: crate::tui::app::CompactPhase::Summarizing,
            started_at: std::time::Instant::now(),
            messages_before: 142,
            respond_to_command_id: None,
        });
        app
    }

    #[test]
    fn a_running_compaction_paints_its_bar_above_the_prompt() {
        let rows = render_screen_rows(compacting_app(), 120, 20);

        let bar = rows
            .iter()
            .find(|row| row.contains("Compacting"))
            .unwrap_or_else(|| panic!("the compaction bar must be on screen: {rows:?}"));
        assert!(bar.contains('▕') && bar.contains('▏'), "{bar}");
        assert!(bar.contains("Summarizing conversation"), "{bar}");
        assert!(bar.contains("142 messages"), "{bar}");
    }

    /// The bar and the `/ultraplan` widget share one layout slot. A run that
    /// starts while an ultraplan is on screen must not stack a second strip
    /// on top of it — nor lose the ultraplan widget once it finishes.
    #[test]
    fn the_compaction_bar_takes_the_progress_slot_from_the_ultraplan_widget() {
        let mut app = app_with_ultraplan_phase(UltraplanPhase::Executing);
        let ultraplan_only = progress_widget_height(&app, 8);
        assert!(ultraplan_only > 0, "fixture must show the ultraplan widget");

        app.compact_run = compacting_app().compact_run;
        assert_eq!(progress_widget_height(&app, 8), 1);

        app.compact_run = None;
        assert_eq!(progress_widget_height(&app, 8), ultraplan_only);
    }

    #[test]
    fn an_idle_session_reserves_no_progress_rows() {
        assert_eq!(progress_widget_height(&AppState::new(), 8), 0);
    }

    #[test]
    fn foregrounded_agent_transcript_extras_ignore_main_activity() {
        let mut app = AppState::new();
        app.live_agent_tool_activity.insert(
            "toolu_agent".into(),
            rebon_tui::LiveAgentToolActivity {
                text: Some("running".into()),
                status: rebon_tui::LiveAgentToolStatus::Running,
                title: None,
                display_name: None,
                start_time_ms: None,
                end_time_ms: None,
                tool_use_count: None,
                token_count: None,
                terminal_result: None,
            },
        );
        app.live_agent_tool_activity_revision = 7;

        {
            let extras = transcript_render_extras(
                app.foregrounded_task_id.as_deref(),
                &app.live_agent_tool_activity,
                app.live_agent_tool_activity_revision,
                &app.auto_mode_allowed_tool_ids,
            );
            assert_eq!(extras.live_agent_tool_activity.len(), 1);
            assert_eq!(extras.live_activity_revision, 7);
        }

        app.foregrounded_task_id = Some("agent-1".into());
        let extras = transcript_render_extras(
            app.foregrounded_task_id.as_deref(),
            &app.live_agent_tool_activity,
            app.live_agent_tool_activity_revision,
            &app.auto_mode_allowed_tool_ids,
        );
        assert!(extras.live_agent_tool_activity.is_empty());
        assert_eq!(extras.live_activity_revision, 0);
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        let mut s = String::new();
        for x in 0..buf.area().width {
            s.push_str(buf[(x, y)].symbol());
        }
        s.trim_end().to_string()
    }

    fn all_rows(buf: &Buffer) -> Vec<String> {
        (0..buf.area().height).map(|y| row_text(buf, y)).collect()
    }

    fn trim_blank_boundaries(rows: &[String]) -> Vec<String> {
        let start = rows
            .iter()
            .position(|line| !line.trim().is_empty())
            .unwrap_or(rows.len());
        let end = rows
            .iter()
            .rposition(|line| !line.trim().is_empty())
            .map(|idx| idx + 1)
            .unwrap_or(start);
        rows[start..end].to_vec()
    }

    fn row_y_containing(rows: &[String], needle: &str) -> usize {
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not found in rows: {rows:?}"))
    }

    fn cell_at_text<'a>(buf: &'a Buffer, text: &str) -> Option<&'a ratatui::buffer::Cell> {
        for y in 0..buf.area().height {
            let row = row_text(buf, y);
            if let Some(byte_x) = row.find(text) {
                let x = row[..byte_x].width() as u16;
                return Some(&buf[(x, y)]);
            }
        }
        None
    }

    fn send_text_chunk(tx: &UnboundedSender<SessionUpdateParams>, session: &str, text: &str) {
        tx.send(SessionUpdateParams {
            session_id: session.into(),
            update: SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text(TextContent {
                    text: text.into(),
                    annotations: None,
                }),
            },
        })
        .expect("send failed");
    }

    #[test]
    fn drain_appends_every_buffered_text_chunk_to_streaming_overlay() {
        let (tx, mut rx): (_, UnboundedReceiver<SessionUpdateParams>) = unbounded_channel();
        send_text_chunk(&tx, "sess-1", "hello ");
        send_text_chunk(&tx, "sess-1", "world");
        send_text_chunk(&tx, "sess-1", "!");

        let mut app = AppState::new();
        let count = drain_pending_updates(&mut app, &mut rx);
        assert_eq!(count, 3);
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("hello world!")
        );
    }

    #[test]
    fn drain_returns_zero_when_channel_is_empty() {
        let (_tx, mut rx): (
            UnboundedSender<SessionUpdateParams>,
            UnboundedReceiver<SessionUpdateParams>,
        ) = unbounded_channel();
        let mut app = AppState::new();
        let count = drain_pending_updates(&mut app, &mut rx);
        assert_eq!(count, 0);
        assert!(app.rebon_tui.overlay.is_empty());
    }

    #[test]
    fn drain_stops_cleanly_when_publisher_is_dropped() {
        let (tx, mut rx): (_, UnboundedReceiver<SessionUpdateParams>) = unbounded_channel();
        send_text_chunk(&tx, "sess-1", "before drop");
        drop(tx);

        let mut app = AppState::new();
        let count = drain_pending_updates(&mut app, &mut rx);
        assert_eq!(count, 1);
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("before drop")
        );

        let count = drain_pending_updates(&mut app, &mut rx);
        assert_eq!(count, 0);
    }

    #[test]
    fn drain_is_a_noop_on_non_text_agent_message_chunks() {
        let (tx, mut rx): (_, UnboundedReceiver<SessionUpdateParams>) = unbounded_channel();
        tx.send(SessionUpdateParams {
            session_id: "sess-1".into(),
            update: SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Image(ImageContent {
                    mime_type: "image/png".into(),
                    data: String::new(),
                    uri: None,
                    annotations: None,
                }),
            },
        })
        .unwrap();

        let mut app = AppState::new();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::SetStreamingText("keep me".into()),
        );

        let count = drain_pending_updates(&mut app, &mut rx);
        assert_eq!(count, 1);
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("keep me")
        );
    }

    fn user(uuid: &str, text: &str) -> Message {
        Message::User(UserMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: UserMessageInner {
                role: UserRole::User,
                content: vec![UserContentBlock::Text(UserTextBlock { text: text.into() })],
            },
            is_compact_summary: None,
            is_meta: None,
            is_visible_in_transcript_only: None,
            image_paste_ids: None,
            plan_content: None,
        })
    }

    fn assistant_text(uuid: &str, text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::Text(AssistantTextBlock {
                    text: text.into(),
                })],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    fn assistant_tool(uuid: &str, id: &str, name: &str, path: &str) -> Message {
        Message::Assistant(AssistantMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                    id: id.into(),
                    name: name.into(),
                    input: json!({ "path": path }),
                    tool_call_content: None,
                    raw_output: None,
                    title: Some(path.into()),
                    locations: None,
                    status: Some(ToolCallStatus::Completed),
                })],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    fn assistant_edit_tool(uuid: &str, id: &str, path: &str, old: &str, new: &str) -> Message {
        Message::Assistant(AssistantMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                    id: id.into(),
                    name: "Edit".into(),
                    input: json!({ "file_path": path }),
                    tool_call_content: Some(vec![rebon_types::ToolCallContent::Diff(
                        rebon_types::DiffContent {
                            path: path.into(),
                            old_text: Some(old.into()),
                            new_text: new.into(),
                        },
                    )]),
                    raw_output: None,
                    title: Some(path.into()),
                    locations: None,
                    status: Some(ToolCallStatus::Completed),
                })],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    fn render_inline_rows(rows: Vec<Message>, height: u16) -> Vec<String> {
        render_inline_rows_with_verbosity(rows, height, ToolOutputVerbosity::Compact)
    }

    fn render_inline_rows_with_verbosity(
        rows: Vec<Message>,
        height: u16,
        verbosity: ToolOutputVerbosity,
    ) -> Vec<String> {
        let theme = RenderTheme::plain();
        let mut state = rebon_tui::AppState::default();
        state.transcript = TranscriptStore::from_rows(rows);
        let area = Rect::new(0, 0, 80, height);
        let mut buf = Buffer::empty(area);
        let mut cache = rebon_tui::TranscriptMeasureCache::new();
        rebon_tui::render_transcript_cached_with_running_hints(
            &state,
            area,
            &mut buf,
            &theme,
            usize::MAX,
            verbosity,
            0,
            None,
            &mut cache,
            false,
            rebon_tui::TranscriptRenderExtras {
                render_thinking_only_rows: true,
                force_verbose_edit_tool_previews: true,
                ..rebon_tui::TranscriptRenderExtras::empty()
            },
        );
        all_rows(&buf)
    }

    fn render_full_inline_frame_rows(
        app: AppState,
        height: u16,
        committed_rows: usize,
    ) -> Vec<String> {
        render_full_inline_frame_rows_with_loading(app, height, committed_rows, false)
    }

    fn render_full_inline_frame_rows_with_loading_and_terminal_height(
        app: AppState,
        height: u16,
        terminal_height: u16,
        committed_rows: usize,
        is_loading: bool,
    ) -> Vec<String> {
        render_full_inline_frame_rows_with_loading_and_terminal_height_and_cursor(
            app,
            height,
            terminal_height,
            committed_rows,
            is_loading,
        )
        .0
    }

    fn render_full_inline_frame_rows_with_loading_and_terminal_height_and_cursor(
        mut app: AppState,
        height: u16,
        terminal_height: u16,
        committed_rows: usize,
        is_loading: bool,
    ) -> (Vec<String>, Option<(u16, u16)>) {
        app.is_loading = is_loading;
        let theme = RenderTheme::plain();
        let runtime_state =
            derive_prompt_input_runtime_state(&app.build_runtime_input(), |_, _, _| {
                "#ffffff".to_string()
            });
        let status = StatusBarInfo {
            provider: "test",
            model: "model",
            cwd: "cwd",
            elapsed_ms: 0,
            effort_display: String::new(),
            fast_mode_display: String::new(),
            context_left_pct: None,
            agent_activity: None,
            goal_activity: None,
            footer_action_hint: None,
            new_session_hint: None,
        };
        let backend = TestBackend::new(80, height);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let mut cursor_hint = None;
        terminal
            .draw(|frame| {
                render_inline_frame(
                    frame,
                    &mut app,
                    &runtime_state,
                    &theme,
                    is_loading,
                    &status,
                    None,
                    committed_rows,
                    terminal_height,
                    &mut cursor_hint,
                );
            })
            .expect("draw");
        (all_rows(terminal.backend().buffer()), cursor_hint)
    }

    fn render_full_inline_frame_rows_with_loading(
        app: AppState,
        height: u16,
        committed_rows: usize,
        is_loading: bool,
    ) -> Vec<String> {
        render_full_inline_frame_rows_with_loading_and_terminal_height(
            app,
            height,
            height,
            committed_rows,
            is_loading,
        )
    }

    /// Like `render_full_inline_frame_rows_with_loading` but borrows `app` so a
    /// test can render several frames against the same state and inspect the
    /// resulting layout across frames.
    fn render_inline_rows_borrowed(
        app: &mut AppState,
        height: u16,
        committed_rows: usize,
        is_loading: bool,
    ) -> Vec<String> {
        app.is_loading = is_loading;
        let theme = RenderTheme::plain();
        let runtime_state =
            derive_prompt_input_runtime_state(&app.build_runtime_input(), |_, _, _| {
                "#ffffff".to_string()
            });
        let status = StatusBarInfo {
            provider: "test",
            model: "model",
            cwd: "cwd",
            elapsed_ms: 0,
            effort_display: String::new(),
            fast_mode_display: String::new(),
            context_left_pct: None,
            agent_activity: None,
            goal_activity: None,
            footer_action_hint: None,
            new_session_hint: None,
        };
        let backend = TestBackend::new(80, height);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| {
                let mut cursor_hint = None;
                render_inline_frame(
                    frame,
                    app,
                    &runtime_state,
                    &theme,
                    is_loading,
                    &status,
                    None,
                    committed_rows,
                    height,
                    &mut cursor_hint,
                );
            })
            .expect("draw");
        all_rows(terminal.backend().buffer())
    }

    fn render_screen_rows(app: AppState, width: u16, height: u16) -> Vec<String> {
        render_screen_rows_and_cursor(app, width, height).0
    }

    fn render_screen_rows_and_cursor(
        mut app: AppState,
        width: u16,
        height: u16,
    ) -> (Vec<String>, Option<(u16, u16)>) {
        let theme = RenderTheme::plain();
        let runtime_state =
            derive_prompt_input_runtime_state(&app.build_runtime_input(), |_, _, _| {
                "#ffffff".to_string()
            });
        let status = StatusBarInfo {
            provider: "test",
            model: "model",
            cwd: "cwd",
            elapsed_ms: 0,
            effort_display: String::new(),
            fast_mode_display: String::new(),
            context_left_pct: None,
            agent_activity: None,
            goal_activity: None,
            footer_action_hint: None,
            new_session_hint: None,
        };
        let session = make_test_tui_session();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let mut cursor_hint = None;
        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &mut app,
                    &runtime_state,
                    &theme,
                    false,
                    &status,
                    Some(&session),
                    &mut cursor_hint,
                );
            })
            .expect("draw");
        (all_rows(terminal.backend().buffer()), cursor_hint)
    }

    #[test]
    fn screen_ask_user_other_cursor_wins_over_background_prompt() {
        let mut app = AppState::new();
        app.input = "background draft".into();
        app.cursor_offset = app.input.len();
        app.pending_permission_view = Some(ask_user_permission_view(true));

        let (rows, cursor) = render_screen_rows_and_cursor(app, 24, 24);
        let input_row = row_y_containing(&rows, "hello world");

        assert_eq!(cursor, Some((16, input_row as u16)), "rows: {rows:?}");
    }

    /// The first frame goes out before the session exists:
    /// the composer, what the user typed into it, and the status bar with
    /// the hosted dot all render from what the slot knows, with no session.
    #[test]
    fn a_frame_renders_with_no_session_and_shows_the_composer_and_the_hosted_dot() {
        let mut app = AppState::new();
        app.input = "typed before the session".into();
        app.cursor_offset = app.input.len();
        let theme = RenderTheme::plain();
        let runtime_state =
            derive_prompt_input_runtime_state(&app.build_runtime_input(), |_, _, _| {
                "#ffffff".to_string()
            });
        let status = StatusBarInfo {
            provider: "openai",
            model: "gpt-preview",
            cwd: "C:/proj",
            elapsed_ms: 0,
            effort_display: String::new(),
            fast_mode_display: String::new(),
            context_left_pct: None,
            agent_activity: None,
            goal_activity: None,
            footer_action_hint: Some("·"),
            new_session_hint: None,
        };
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let mut cursor_hint = None;
        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &mut app,
                    &runtime_state,
                    &theme,
                    false,
                    &status,
                    None,
                    &mut cursor_hint,
                );
            })
            .expect("draw without a session");
        let rows = all_rows(terminal.backend().buffer());
        assert!(
            rows.iter()
                .any(|row| row.contains("typed before the session")),
            "the composer is drawn: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("gpt-preview")),
            "the status bar shows the preview's model: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains('·')),
            "the hosted dot is the only sign of the wait: {rows:?}"
        );
        assert!(cursor_hint.is_some(), "the caret is placed in the composer");
    }

    #[test]
    fn screen_permission_without_other_focus_hides_background_prompt_cursor() {
        let mut app = AppState::new();
        app.input = "background draft".into();
        app.cursor_offset = app.input.len();
        app.pending_permission_view = Some(ask_user_permission_view(false));

        let (_rows, cursor) = render_screen_rows_and_cursor(app, 24, 24);

        assert_eq!(cursor, None);
    }

    #[test]
    fn screen_scrolled_transcript_renders_sticky_anchor_and_scroll_to_bottom() {
        let mut app = AppState::new();
        let long_answer = (0..24)
            .map(|idx| format!("assistant line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.rebon_tui.transcript = TranscriptStore::from_rows(vec![
            user("u1", "first prompt anchor text"),
            assistant_text("a1", &long_answer),
        ]);
        app.follow_transcript_tail = false;
        app.scroll_offset = 2;
        app.total_content_lines = 80;
        app.prev_frame_area = Some(Rect::new(0, 2, 80, 10));

        let rows = render_screen_rows(app, 80, 18);

        assert!(
            rows.first()
                .is_some_and(|row| row.contains("> first prompt anchor text")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("scroll to bottom")),
            "rows: {rows:?}"
        );
    }

    #[test]
    fn screen_following_tail_keeps_normal_header_and_hides_scroll_to_bottom() {
        let mut app = AppState::new();
        app.rebon_tui.transcript = TranscriptStore::from_rows(vec![user("u1", "hello")]);
        app.follow_transcript_tail = true;
        app.total_content_lines = 10;

        let rows = render_screen_rows(app, 80, 18);

        assert!(
            rows.first().is_some_and(|row| row.contains("Rebon v")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("scroll to bottom")),
            "rows: {rows:?}"
        );
    }

    fn render_inline_buffer_in_area(
        mut app: AppState,
        area: Rect,
        committed_rows: usize,
    ) -> Buffer {
        let theme = RenderTheme::plain();
        let runtime_state =
            derive_prompt_input_runtime_state(&app.build_runtime_input(), |_, _, _| {
                "#ffffff".to_string()
            });
        let status = StatusBarInfo {
            provider: "test",
            model: "model",
            cwd: "cwd",
            elapsed_ms: 0,
            effort_display: String::new(),
            fast_mode_display: String::new(),
            context_left_pct: None,
            agent_activity: None,
            goal_activity: None,
            footer_action_hint: None,
            new_session_hint: None,
        };
        let backend = TestBackend::new(area.right(), area.bottom());
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| {
                let mut cursor_hint = None;
                render_inline_frame_in_area(
                    frame,
                    area,
                    &mut app,
                    &runtime_state,
                    &theme,
                    false,
                    &status,
                    None,
                    committed_rows,
                    area.bottom().max(frame.area().height),
                    &mut cursor_hint,
                );
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    fn render_inline_rows_in_area(app: AppState, area: Rect, committed_rows: usize) -> Vec<String> {
        all_rows(&render_inline_buffer_in_area(app, area, committed_rows))
    }

    struct InlineTaskEnv {
        _guard: std::sync::MutexGuard<'static, ()>,
        _tmp: TempDir,
        task_list_id: String,
        old_config_dir: Option<std::ffi::OsString>,
        old_task_list_id: Option<std::ffi::OsString>,
        old_team_name: Option<std::ffi::OsString>,
        old_session_id: Option<std::ffi::OsString>,
    }

    impl InlineTaskEnv {
        fn new(task_list_id: &str) -> Self {
            let guard = crate::test_env::lock_env();
            let tmp = TempDir::new().unwrap();
            let old_config_dir = std::env::var_os("REBON_CONFIG_DIR");
            let old_task_list_id = std::env::var_os("REBON_TASK_LIST_ID");
            let old_team_name = std::env::var_os("REBON_TEAM_NAME");
            let old_session_id = std::env::var_os("REBON_SESSION_ID");

            std::env::set_var("REBON_CONFIG_DIR", tmp.path());
            std::env::set_var("REBON_TASK_LIST_ID", task_list_id);
            std::env::remove_var("REBON_TEAM_NAME");
            std::env::remove_var("REBON_SESSION_ID");

            Self {
                _guard: guard,
                _tmp: tmp,
                task_list_id: task_list_id.to_string(),
                old_config_dir,
                old_task_list_id,
                old_team_name,
                old_session_id,
            }
        }

        fn create_task(&self, subject: &str) {
            rebon_tool::tasks::create_task(
                &self.task_list_id,
                rebon_tool::tasks::NewTask {
                    subject: subject.into(),
                    description: format!("{subject} description"),
                    active_form: None,
                    owner: None,
                    status: rebon_tool::tasks::TaskListStatus::Pending,
                    blocks: Vec::new(),
                    blocked_by: Vec::new(),
                    metadata: None,
                },
            )
            .unwrap();
        }
    }

    impl Drop for InlineTaskEnv {
        fn drop(&mut self) {
            restore_env("REBON_CONFIG_DIR", self.old_config_dir.as_ref());
            restore_env("REBON_TASK_LIST_ID", self.old_task_list_id.as_ref());
            restore_env("REBON_TEAM_NAME", self.old_team_name.as_ref());
            restore_env("REBON_SESSION_ID", self.old_session_id.as_ref());
        }
    }

    fn restore_env(name: &str, value: Option<&std::ffi::OsString>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    fn render_owned_inline_frame_buffer(
        mut app: AppState,
        height: u16,
        committed_rows: usize,
    ) -> Buffer {
        let theme = RenderTheme::plain();
        let runtime_state =
            derive_prompt_input_runtime_state(&app.build_runtime_input(), |_, _, _| {
                "#ffffff".to_string()
            });
        let status = StatusBarInfo {
            provider: "test",
            model: "model",
            cwd: "cwd",
            elapsed_ms: 0,
            effort_display: String::new(),
            fast_mode_display: String::new(),
            context_left_pct: None,
            agent_activity: None,
            goal_activity: None,
            footer_action_hint: None,
            new_session_hint: None,
        };
        let backend = TestBackend::new(80, height);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| {
                clear_rect(frame, frame.area());
            })
            .expect("seed draw");
        terminal
            .draw(|frame| {
                let mut cursor_hint = None;
                render_inline_frame(
                    frame,
                    &mut app,
                    &runtime_state,
                    &theme,
                    false,
                    &status,
                    None,
                    committed_rows,
                    height,
                    &mut cursor_hint,
                );
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    fn desired_inline_height_for_test(
        app: &AppState,
        base_height: u16,
        terminal_height: u16,
        committed_rows: usize,
    ) -> u16 {
        desired_inline_viewport_height(
            app,
            &RenderTheme::plain(),
            InlineViewportHeightInput {
                width: 80,
                terminal_height,
                base_height,
                committed_rows,
                elapsed_ms: 0,
            },
        )
    }

    fn permission_view() -> crate::tui::permission_modal::PermissionModalView {
        crate::tui::permission_modal::PermissionModalView {
            query_id: 1,
            tool_call_id: "tool-1".into(),
            title: "Allow Bash?".into(),
            summary: "Run tests".into(),
            options: vec![
                PermissionOptionView {
                    option_id: "allow".into(),
                    label: "Allow".into(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                PermissionOptionView {
                    option_id: "reject".into(),
                    label: "Reject".into(),
                    kind: PermissionOptionKind::RejectOnce,
                },
            ],
            selected: 0,
            extra_text: String::new(),
            extra_text_focused: false,
            kind: PermissionKind::Generic,
        }
    }

    fn ask_user_permission_view(other_focused: bool) -> PermissionModalView {
        let mut answer = AskUserQuestionAnswer::new();
        answer.highlighted_row = usize::from(other_focused);
        answer.other_text = "hello world".into();
        answer.other_cursor_offset = answer.other_text.len();
        PermissionModalView {
            query_id: 2,
            tool_call_id: "tool-ask".into(),
            title: "Choice".into(),
            summary: String::new(),
            options: Vec::new(),
            selected: 0,
            extra_text: String::new(),
            extra_text_focused: false,
            kind: PermissionKind::AskUserQuestion {
                questions: vec![AskUserQuestionEntry {
                    question: "How should we proceed?".into(),
                    header: "Choice".into(),
                    options: vec![AskUserQuestionOption {
                        label: "Use default".into(),
                        description: String::new(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
                answers: vec![answer],
                active_question: 0,
                confirmation_active: false,
                confirmation_selected: 0,
                original_input: serde_json::json!({}),
            },
        }
    }

    #[test]
    fn help_overlay_renders_general_tab() {
        let mut app = AppState::new();
        app.help_open = true;
        app.slash_commands = rebon_slash_commands::for_surface(rebon_slash_commands::Surface::Tui);

        let rows = render_screen_rows(app, 100, 40);

        assert!(
            rows.iter().any(|row| row.contains("Rebon v")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("Rebon understands your codebase")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("Shortcuts")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("Ctrl+O")),
            "rows: {rows:?}"
        );
    }

    #[test]
    fn help_overlay_renders_commands_tab() {
        let mut app = AppState::new();
        app.help_open = true;
        app.help_tab_index = 1;
        app.slash_commands = rebon_slash_commands::for_surface(rebon_slash_commands::Surface::Tui);

        let rows = render_screen_rows(app, 100, 40);

        assert!(
            rows.iter()
                .any(|row| row.contains("Browse default commands:")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("/help")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("/compact")),
            "rows: {rows:?}"
        );
    }

    #[test]
    fn help_overlay_renders_custom_skill_tab() {
        let mut app = AppState::new();
        app.help_open = true;
        app.help_tab_index = 2;
        app.slash_commands = vec![
            rebon_types::SlashCommand {
                name: "help".into(),
                description: "Show help".into(),
                input: None,
                category: Some(rebon_types::SlashCommandCategory::Command),
                aliases: Vec::new(),
            },
            rebon_types::SlashCommand {
                name: "simplify".into(),
                description: "Review changed code for reuse, quality, and efficiency".into(),
                input: None,
                category: Some(rebon_types::SlashCommandCategory::Skill),
                aliases: Vec::new(),
            },
        ];

        let rows = render_screen_rows(app, 100, 40);

        assert!(
            rows.iter()
                .any(|row| row.contains("Browse custom commands:")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("/simplify")),
            "rows: {rows:?}"
        );
        assert!(rows
            .iter()
            .all(|row| !row.contains("No custom commands found")));
    }

    fn app_with_agent_switcher_rows() -> AppState {
        let mut app = AppState::default();
        let registry = rebon_plugin_tasks::runtime::TaskRegistry::new();
        let task_id = rebon_plugin_tasks::runtime::TaskId::new("agent-1");
        let mut snapshot = rebon_plugin_tasks::runtime::TaskSnapshot::new_pending(
            task_id.clone(),
            "Bash ok".into(),
            rebon_plugin_tasks::runtime::TaskData::LocalAgent(
                rebon_plugin_tasks::runtime::LocalAgentData {
                    prompt: "verify".into(),
                    agent_type: "verification".into(),
                    model: None,
                    system: None,
                    allowed_tools: None,
                    token_count: 30_600,
                    tool_use_count: 0,
                    transcript: Vec::new(),
                    streaming_text: None,
                    pending_messages: Vec::new(),
                    retrieved: false,
                },
            ),
        );
        snapshot.status = rebon_plugin_tasks::runtime::TaskStatus::Running;
        snapshot.is_backgrounded = true;
        snapshot.metadata = serde_json::json!({
            "display_name": "verify-final-app-stability"
        });
        registry.insert(task_id, snapshot, rebon_types::PromptCancel::new());
        app.tasks = std::sync::Arc::new(registry);
        app
    }

    fn app_with_tall_in_progress_workflow_overlay() -> AppState {
        let mut app = AppState::default();
        let entries: Vec<serde_json::Value> = std::iter::once(json!({
            "sequence": 0,
            "entry": { "type": "phase", "title": "scan", "state": "start" }
        }))
        .chain((1..=8u64).map(|i| {
            json!({
                "sequence": i,
                "entry": {
                    "type": "agent",
                    "index": i,
                    "state": if i <= 6 { "completed" } else { "start" },
                    "phaseTitle": "scan",
                    "label": format!("agent-{i}"),
                    "tokens": 100 * i,
                    "toolCalls": 1
                }
            })
        }))
        .chain(std::iter::once(json!({
            "sequence": 9,
            "entry": { "type": "log", "message": "latest note" }
        })))
        .collect();
        app.rebon_tui
            .overlay
            .upsert_streaming_tool_use(rebon_tui::StreamingToolUse {
                call_id: "workflow-tall".into(),
                tool_name: "Workflow".into(),
                kind: rebon_types::ToolKind::Other,
                status: ToolCallStatus::InProgress,
                title: None,
                content: None,
                locations: None,
                raw_input: Some(std::collections::HashMap::from([(
                    "script".to_string(),
                    json!("workflow()"),
                )])),
                raw_output: Some(std::collections::HashMap::from([
                    ("status".to_string(), json!("running")),
                    (
                        "workflowProgress".to_string(),
                        json!({
                            "runId": "wf_tall",
                            "workflowName": "tall-run",
                            "entries": entries
                        }),
                    ),
                ])),
            });
        app
    }

    /// An in-progress Workflow card cannot drain to scrollback, so in a short
    /// inline viewport the bottom-anchored live region used to clip its header
    /// (and on very short viewports the whole card) into nowhere. The live
    /// card must instead elide its middle so the header, run summary, and the
    /// freshest rows all stay on screen.
    #[test]
    fn inline_short_viewport_keeps_live_workflow_card_header_visible() {
        let app = app_with_tall_in_progress_workflow_overlay();

        let buf = render_owned_inline_frame_buffer(app, 14, 0);
        let rows = all_rows(&buf);

        assert!(
            rows.iter().any(|row| row.contains("Workflow: tall-run")),
            "live card header must stay visible in a short viewport: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("run wf_tall · running")),
            "run summary must stay visible: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("rows hidden")),
            "card middle must elide instead of clipping the header: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("agent-8")),
            "freshest agent row stays visible: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("agent-1 ")),
            "elided middle rows are dropped, not painted: {rows:?}"
        );
    }

    fn inline_transcript_height(rows: Vec<Message>) -> u16 {
        inline_transcript_height_with_verbosity(rows, rebon_tui::ToolOutputVerbosity::Compact)
    }

    fn inline_transcript_height_with_verbosity(
        rows: Vec<Message>,
        verbosity: rebon_tui::ToolOutputVerbosity,
    ) -> u16 {
        let theme = RenderTheme::plain();
        let mut state = rebon_tui::AppState::default();
        state.transcript = TranscriptStore::from_rows(rows);
        let mut cache = rebon_tui::TranscriptMeasureCache::new();
        measure_inline_transcript_height(
            &state,
            80,
            &theme,
            verbosity,
            0,
            &mut cache,
            false,
            rebon_tui::TranscriptRenderExtras {
                render_thinking_only_rows: true,
                force_verbose_edit_tool_previews: true,
                ..rebon_tui::TranscriptRenderExtras::empty()
            },
        )
    }

    #[test]
    fn drain_file_list_keeps_complete_after_late_nonfatal_untracked_terminal_updates() {
        let mut app = AppState::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(FileListUpdate::Complete).unwrap();
        tx.send(FileListUpdate::TimedOut("git ls-files --others".into()))
            .unwrap();
        tx.send(FileListUpdate::Failed(
            "git ls-files --others failed".into(),
        ))
        .unwrap();
        drop(tx);

        drain_file_list(&mut app, &mut rx);

        assert_eq!(app.file_scan_status, FileScanStatus::Complete);
    }

    #[test]
    fn completed_empty_at_picker_stays_no_matches_after_late_nonfatal_untracked_timeout() {
        let mut app = AppState::default();
        app.input = "@missing".into();
        app.cursor_offset = app.input.len();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(FileListUpdate::Complete).unwrap();
        tx.send(FileListUpdate::TimedOut("git ls-files --others".into()))
            .unwrap();
        drop(tx);

        drain_file_list(&mut app, &mut rx);
        crate::tui::at_mention_picker::sync(
            &mut app.at_mention_picker,
            &app.input,
            app.cursor_offset,
            ".",
            &app.file_index,
            &app.file_scan_status,
        );

        let data = crate::tui::at_mention_picker::render_data(&app.at_mention_picker).unwrap();
        assert_eq!(data.rows.len(), 1);
        assert_eq!(data.rows[0].display, "No matching files");
        assert!(!data.rows[0].is_selectable);
        assert_eq!(app.file_scan_status, FileScanStatus::Complete);
    }

    #[test]
    fn drain_file_list_respects_per_frame_update_budget() {
        let mut app = AppState::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for i in 0..(FILE_LIST_DRAIN_UPDATE_BUDGET + 2) {
            tx.send(FileListUpdate::Tracked(vec![format!("file-{i}.rs")]))
                .unwrap();
        }

        drain_file_list(&mut app, &mut rx);

        assert_eq!(app.file_index.len(), FILE_LIST_DRAIN_UPDATE_BUDGET);
        assert!(
            rx.try_recv().is_ok(),
            "updates beyond the frame budget should remain queued"
        );
    }

    #[test]
    fn drain_file_list_respects_per_frame_path_budget_and_leaves_backlog() {
        let mut app = AppState::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(FileListUpdate::Tracked(
            (0..FILE_LIST_DRAIN_PATH_BUDGET)
                .map(|i| format!("file-{i}.rs"))
                .collect(),
        ))
        .unwrap();
        tx.send(FileListUpdate::Tracked(vec!["backlog.rs".into()]))
            .unwrap();

        drain_file_list(&mut app, &mut rx);

        assert_eq!(app.file_index.len(), FILE_LIST_DRAIN_PATH_BUDGET);
        let remaining = rx
            .try_recv()
            .expect("updates beyond the path budget should remain queued");
        match remaining {
            FileListUpdate::Tracked(paths) => assert_eq!(paths, vec!["backlog.rs".to_string()]),
            other => panic!("expected tracked backlog update, got {other:?}"),
        }
    }

    #[test]
    fn inline_picker_area_clamps_to_non_zero_origin_parent() {
        let parent = Rect::new(0, 8, 135, 12);
        let prompt = Rect::new(0, 18, 135, 1);
        let picker = inline_picker_area(prompt, parent, 10).expect("picker area");

        assert!(
            picker.y >= parent.y,
            "picker starts above inline area: {picker:?}"
        );
        assert!(
            picker.bottom() <= parent.bottom(),
            "picker exceeds inline area: {picker:?}"
        );
        assert!(picker.x >= parent.x);
        assert!(picker.right() <= parent.right());
    }

    #[test]
    fn inline_slash_picker_non_zero_origin_does_not_panic_or_write_outside_area() {
        let area = Rect::new(0, 8, 135, 12);
        let mut app = AppState::default();
        app.input = "/".into();
        app.cursor_offset = app.input.len();
        app.slash_commands = rebon_slash_commands::for_surface(rebon_slash_commands::Surface::Tui);
        crate::tui::slash_picker::sync(&mut app.slash_picker, &app.input, &app.slash_commands);

        let rows = render_inline_rows_in_area(app, area, 0);

        assert!(
            rows.iter().any(|row| row.contains("commands")),
            "slash picker did not render: {rows:?}"
        );
    }

    #[test]
    fn inline_at_mention_picker_non_zero_origin_does_not_panic_or_write_outside_area() {
        let area = Rect::new(0, 8, 135, 12);
        let mut app = AppState::default();
        app.input = "@".into();
        app.cursor_offset = app.input.len();
        app.file_index
            .merge(vec!["src/main.rs".into(), "Cargo.toml".into()]);
        crate::tui::at_mention_picker::sync(
            &mut app.at_mention_picker,
            &app.input,
            app.cursor_offset,
            ".",
            &app.file_index,
            &app.file_scan_status,
        );

        let rows = render_inline_rows_in_area(app, area, 0);

        assert!(
            rows.iter().any(|row| row.contains("mentions")),
            "mention picker did not render: {rows:?}"
        );
    }

    /// The login pane renders in the inline prompt host, replacing the input
    /// the way `/provider` does — not by switching the terminal to the
    /// alternate screen and hiding the conversation.
    #[test]
    fn inline_frame_hosts_the_login_dialog_in_the_prompt_area() {
        let mut app = AppState::new();
        app.rebon_tui.transcript = TranscriptStore::from_rows(vec![user("u1", "hello there")]);
        app.onboarding_dialog =
            Some(rebon_plugin_onboarding::OnboardingDialogState::open_for_login_pane());

        let rows = render_full_inline_frame_rows(app, 24, 0);

        assert!(
            rows.iter().any(|row| row.contains("Login (1/1)")),
            "login dialog title should render inline: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("Select a login method:")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("hello there")),
            "the conversation stays visible above the host: {rows:?}"
        );
    }

    #[test]
    fn inline_picker_close_collapses_prompt_host_to_normal_height() {
        let area = Rect::new(0, 8, 135, 12);
        let mut open_app = slash_picker_app();
        open_app.input = "one\ntwo".into();
        open_app.cursor_offset = open_app.input.len();
        crate::tui::slash_picker::sync(
            &mut open_app.slash_picker,
            &open_app.input,
            &open_app.slash_commands,
        );
        // Simulate stale picker state while the prompt text no longer matches
        // the slash-picker trigger. The host height must follow the current
        // prompt/dialog state, not the previous picker-expanded frame.
        open_app.slash_picker = None;
        let normal_height = prompt_height_for_width(&open_app, area.width, area.height);

        let rows = render_inline_rows_in_area(open_app, area, 0);
        let prompt_border_rows = rows
            .iter()
            .filter(|row| row.contains('┌') || row.contains('└'))
            .count() as u16;

        assert_eq!(
            inline_prompt_host_height(&AppState::default(), area.width, area.height),
            3
        );
        assert_eq!(normal_height, 4);
        assert!(
            rows.iter().all(|row| !row.contains("commands")),
            "closed slash picker still rendered: {rows:?}"
        );
        assert_eq!(
            prompt_border_rows, 2,
            "closed picker should render only the prompt surface without a reserved picker host: {rows:?}"
        );
    }

    #[test]
    fn inline_at_mention_picker_close_collapses_prompt_host_to_normal_height() {
        let area = Rect::new(0, 8, 135, 12);
        let mut app = at_mention_picker_app();
        assert!(app.at_mention_picker.is_some());
        app.at_mention_picker = None;

        let rows = render_inline_rows_in_area(app, area, 0);

        assert!(
            rows.iter().all(|row| !row.contains("mentions")),
            "closed @ picker still rendered: {rows:?}"
        );
        assert_eq!(
            inline_prompt_host_height(&AppState::default(), area.width, area.height),
            3
        );
    }

    #[test]
    fn inline_tiny_live_content_keeps_visible_transcript_row_before_large_prompt() {
        let area = Rect::new(0, 0, 80, 3);
        let host_height = compact_inline_host_height(3, 2, area.height, false);
        let layout = inline_layout(
            area,
            host_height,
            compact_inline_transcript_height(2, host_height, area.height, false),
            false,
        );

        assert_eq!(layout.transcript.height, 1);
        assert_eq!(layout.prompt.height, 2);
    }

    #[test]
    fn resume_dialog_uses_minimum_screen_height_when_terminal_allows() {
        let mut app = AppState::default();
        app.resume_dialog = Some(crate::tui::resume_dialog::ResumeDialogState::open());

        let rows = render_screen_rows(app, 100, 40);
        let top = rows
            .iter()
            .position(|row| row.contains("Resume Session"))
            .expect("resume dialog title should render");
        let bottom = rows
            .iter()
            .rposition(|row| row.contains('┘'))
            .expect("resume dialog bottom border should render");

        assert!(bottom + 1 - top >= 30, "rows: {rows:?}");
    }

    #[test]
    fn inline_resume_dialog_uses_prompt_host_space() {
        let area = Rect::new(0, 8, 135, 12);
        let mut app = AppState::default();
        app.resume_dialog = Some(crate::tui::resume_dialog::ResumeDialogState::open());

        let rows = render_inline_rows_in_area(app, area, 0);
        let top = rows
            .iter()
            .position(|row| row.contains("Resume Session"))
            .expect("resume dialog title should render");
        let bottom = rows
            .iter()
            .rposition(|row| row.contains('┘'))
            .expect("resume dialog bottom border should render");

        assert!(
            rows.iter().any(|row| row.contains("Loading sessions")),
            "resume dialog body did not render: {rows:?}"
        );
        assert!(bottom + 1 - top >= 10, "rows: {rows:?}");
    }

    #[test]
    fn resumed_mode_choice_replaces_screen_prompt_input() {
        let entry = crate::session::resume_listing::SessionEntry {
            session_id: "resume-choice".into(),
            transcript_cwd: "/repo".into(),
            title: "Long session".into(),
            created_at_ms: 1,
            jsonl_bytes: Some(1024),
            joinable: false,
        };
        let mut dialog =
            crate::tui::resume_dialog::ResumeDialogState::open_exact(entry.session_id.clone());
        let load = dialog.maybe_take_load_request().unwrap();
        dialog.apply_load_results(load.generation, vec![entry]);
        let prepare = dialog.maybe_take_prepare_request().unwrap();
        dialog.apply_prepare_success(prepare.generation);

        let mut app = AppState::default();
        app.resume_dialog = Some(dialog);
        let rows = render_screen_rows(app, 100, 40);
        let top = rows
            .iter()
            .position(|row| row.contains("Resume Session"))
            .expect("resume mode choice should render");
        let rendered = rows.join("\n");

        assert!(
            top >= 25,
            "choice should replace the bottom prompt: {rows:?}"
        );
        assert!(rendered.contains("Resume with summary"));
        assert!(rendered.contains("Resume with full history"));
        assert!(rendered.contains("Start a new empty session"));
    }

    #[test]
    fn screen_effort_dialog_renders_five_levels_without_ultra() {
        let mut app = AppState::default();
        app.dialogs
            .push(rebon_dialog::effort_dialog::EffortDialogState::open(
                "gpt-5.6-sol",
                Some("max"),
            ));

        let rows = render_screen_rows(app, 100, 20);
        let rendered = rows.join("\n");
        assert!(rendered.contains("Select Reasoning Level for gpt-5.6-sol"));
        assert!(rendered.contains("Low"));
        assert!(rendered.contains("Medium"));
        assert!(rendered.contains("High"));
        assert!(rendered.contains("Extra high"));
        assert!(rendered.contains("Max"));
        assert!(!rendered.contains("Ultra"));
    }

    #[test]
    fn inline_effort_dialog_uses_compact_prompt_host() {
        let area = Rect::new(0, 8, 100, 12);
        let mut app = AppState::default();
        app.dialogs
            .push(rebon_dialog::effort_dialog::EffortDialogState::open(
                "gpt-5.6-sol",
                None,
            ));

        assert_eq!(inline_dialog_host_height(&app, area.height), 9);
        let rows = render_inline_rows_in_area(app, area, 0);
        assert!(
            rows.iter()
                .any(|row| row.contains("Select Reasoning Level for gpt-5.6-sol")),
            "effort dialog title did not render: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("1. Low")),
            "effort dialog options did not render: {rows:?}"
        );
    }

    #[test]
    fn inline_settings_dialog_gets_full_viewport_height() {
        let mut app = AppState::default();
        app.dialogs
            .push(rebon_dialog::settings_dialog::SettingsDialogState::open(
                &rebon_dialog::settings_dialog::SettingsDialogOpen::default(),
            ));

        assert_eq!(inline_dialog_host_height(&app, 12), 12);
    }

    #[test]
    fn inline_background_tasks_dialog_gets_visible_body_height() {
        let area = Rect::new(0, 8, 135, 12);
        let mut app = AppState::default();
        let snapshots = app.task_snapshots();
        app.background_tasks_dialog = Some(
            rebon_plugin_tasks::ui::background_tasks_dialog::BackgroundTasksDialogState::open(
                &snapshots, None, None,
            ),
        );

        let rows = render_inline_rows_in_area(app, area, 0);

        assert!(
            rows.iter().any(|row| row.contains("Background tasks")),
            "background tasks title did not render: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("no running tasks")),
            "background tasks subtitle did not render: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("no background tasks")),
            "background tasks empty state did not render: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("close")),
            "background tasks close action did not render: {rows:?}"
        );
    }

    #[test]
    fn inline_active_dialog_replaces_prompt_and_stays_in_non_zero_origin_area() {
        let area = Rect::new(0, 8, 135, 12);
        let mut app = AppState::default();
        app.dialogs.push(test_quick_open());

        let rows = render_inline_rows_in_area(app, area, 0);

        assert!(
            rows.iter()
                .any(|row| row.contains("Quick Open") || row.contains("Search:")),
            "quick-open dialog did not render: {rows:?}"
        );
    }

    fn render_inline_buffer_prefilled(
        mut app: AppState,
        terminal_area: Rect,
        inline_area: Rect,
        sentinel: &str,
    ) -> Buffer {
        let theme = RenderTheme::plain();
        let runtime_state =
            derive_prompt_input_runtime_state(&app.build_runtime_input(), |_, _, _| {
                "#ffffff".to_string()
            });
        let status = StatusBarInfo {
            provider: "test",
            model: "model",
            cwd: "cwd",
            elapsed_ms: 0,
            effort_display: String::new(),
            fast_mode_display: String::new(),
            context_left_pct: None,
            agent_activity: None,
            goal_activity: None,
            footer_action_hint: None,
            new_session_hint: None,
        };
        let backend = TestBackend::new(terminal_area.width, terminal_area.height);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                for y in terminal_area.y..terminal_area.bottom() {
                    for x in terminal_area.x..terminal_area.right() {
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.set_symbol(sentinel);
                        }
                    }
                }
                let mut cursor_hint = None;
                inline::render_inline_frame_in_area(
                    frame,
                    inline_area,
                    &mut app,
                    &runtime_state,
                    &theme,
                    false,
                    &status,
                    None,
                    0,
                    terminal_area.height,
                    &mut cursor_hint,
                );
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    fn assert_outside_area_unchanged(buf: &Buffer, owned: Rect, sentinel: &str) {
        let area = buf.area();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                if x >= owned.x && x < owned.right() && y >= owned.y && y < owned.bottom() {
                    continue;
                }
                assert_eq!(
                    buf[(x, y)].symbol(),
                    sentinel,
                    "cell ({x},{y}) outside {owned:?} was modified"
                );
            }
        }
    }

    fn buffer_contains(buf: &Buffer, needle: &str) -> bool {
        all_rows(buf).iter().any(|row| row.contains(needle))
    }

    fn slash_picker_app() -> AppState {
        let mut app = AppState::default();
        app.input = "/".into();
        app.cursor_offset = app.input.len();
        app.slash_commands = rebon_slash_commands::for_surface(rebon_slash_commands::Surface::Tui);
        crate::tui::slash_picker::sync(&mut app.slash_picker, &app.input, &app.slash_commands);
        app
    }

    fn at_mention_picker_app() -> AppState {
        let mut app = AppState::default();
        app.input = "@".into();
        app.cursor_offset = app.input.len();
        app.file_index
            .merge(vec!["src/main.rs".into(), "Cargo.toml".into()]);
        crate::tui::at_mention_picker::sync(
            &mut app.at_mention_picker,
            &app.input,
            app.cursor_offset,
            ".",
            &app.file_index,
            &app.file_scan_status,
        );
        app
    }

    fn deterministic_at_mention_picker_app() -> AppState {
        let mut app = AppState::default();
        app.input = "@Cargo".into();
        app.cursor_offset = app.input.len();
        app.file_index
            .merge(vec!["src/main.rs".into(), "Cargo.toml".into()]);
        crate::tui::at_mention_picker::sync(
            &mut app.at_mention_picker,
            &app.input,
            app.cursor_offset,
            ".",
            &app.file_index,
            &app.file_scan_status,
        );
        app
    }

    /// A picker over an index and previewer that answer nothing — the
    /// layout tests only need a panel of the right shape on the stack.
    fn test_quick_open() -> rebon_dialog::quick_open_dialog::QuickOpenDialogState {
        use rebon_dialog::quick_open_dialog::{FileCandidates, PreviewSource};
        struct Empty;
        impl FileCandidates for Empty {
            fn matching(&self, _query: &str, _limit: usize) -> Vec<String> {
                Vec::new()
            }
        }
        impl PreviewSource for Empty {
            fn preview(&self, _path: &str, _rows: usize) -> Vec<String> {
                Vec::new()
            }
        }
        rebon_dialog::quick_open_dialog::QuickOpenDialogState::open(
            std::sync::Arc::new(Empty),
            std::sync::Arc::new(Empty),
        )
    }

    fn quick_open_dialog_app() -> AppState {
        let mut app = AppState::default();
        app.dialogs.push(test_quick_open());
        app
    }

    #[test]
    fn inline_slash_picker_leaves_prefilled_cells_outside_inline_area_unchanged() {
        let inline_area = Rect::new(0, 8, 135, 12);
        let buf = render_inline_buffer_prefilled(
            slash_picker_app(),
            Rect::new(0, 0, 135, 24),
            inline_area,
            "X",
        );

        assert!(
            buffer_contains(&buf, "commands"),
            "slash picker did not render"
        );
        assert_outside_area_unchanged(&buf, inline_area, "X");
    }

    #[test]
    fn inline_at_mention_picker_leaves_prefilled_cells_outside_inline_area_unchanged() {
        let inline_area = Rect::new(0, 8, 135, 12);
        let buf = render_inline_buffer_prefilled(
            at_mention_picker_app(),
            Rect::new(0, 0, 135, 24),
            inline_area,
            "X",
        );

        assert!(
            buffer_contains(&buf, "mentions"),
            "mention picker did not render"
        );
        assert_outside_area_unchanged(&buf, inline_area, "X");
    }

    #[test]
    fn inline_prompt_replacement_dialog_leaves_prefilled_cells_outside_inline_area_unchanged() {
        let inline_area = Rect::new(0, 8, 135, 12);
        let buf = render_inline_buffer_prefilled(
            quick_open_dialog_app(),
            Rect::new(0, 0, 135, 24),
            inline_area,
            "X",
        );

        assert!(
            buffer_contains(&buf, "Quick Open") || buffer_contains(&buf, "Search:"),
            "quick-open dialog did not render"
        );
        assert_outside_area_unchanged(&buf, inline_area, "X");
    }

    #[test]
    fn inline_pickers_and_dialogs_tiny_areas_do_not_panic_or_write_outside_bounds() {
        for app in [
            slash_picker_app(),
            at_mention_picker_app(),
            quick_open_dialog_app(),
        ] {
            let inline_area = Rect::new(2, 2, 2, 1);
            let buf = render_inline_buffer_prefilled(app, Rect::new(0, 0, 8, 5), inline_area, "X");
            assert_outside_area_unchanged(&buf, inline_area, "X");
        }
    }

    fn test_layout_zones() -> LayoutZones {
        LayoutZones {
            passthrough: false,
            show_sticky_header: false,
            sticky_header_text: None,
            pad_collapsed: false,
            scroll_padding_top: 0,
            show_pill: false,
            pill_count: 0,
            show_bottom_float: false,
            show_modal: false,
            modal_rows: 0,
            modal_columns: 0,
            modal_max_height: 0,
            show_bottom_bar: true,
            show_suggestions_overlay: false,
            show_dialog_overlay: false,
        }
    }

    #[test]
    fn footer_renders_action_hint_after_cwd() {
        let app = AppState::default();
        let area = Rect::new(0, 0, 80, 1);
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let status = StatusBarInfo {
            provider: "test",
            model: "model",
            cwd: "cwd",
            elapsed_ms: 0,
            effort_display: String::new(),
            fast_mode_display: String::new(),
            context_left_pct: None,
            agent_activity: None,
            goal_activity: None,
            footer_action_hint: Some("press ← again for agents view"),
            new_session_hint: None,
        };
        let zones = test_layout_zones();

        terminal
            .draw(|frame| render_footer(frame, area, &app, &status, true, &zones))
            .expect("draw");
        let row = row_text(terminal.backend().buffer(), 0);
        assert!(row.contains("cwd  press ← again for agents view"), "{row}");
    }

    #[test]
    fn footer_renders_permission_mode_cycle_hint_in_parentheses() {
        let mut app = AppState::default();
        app.set_permission_mode(rebon_permissions::PermissionMode::Auto);
        let area = Rect::new(0, 0, 80, 1);
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let status = StatusBarInfo {
            provider: "test",
            model: "model",
            cwd: "cwd",
            elapsed_ms: 0,
            effort_display: String::new(),
            fast_mode_display: String::new(),
            context_left_pct: None,
            agent_activity: None,
            goal_activity: None,
            footer_action_hint: None,
            new_session_hint: None,
        };
        let zones = test_layout_zones();

        terminal
            .draw(|frame| render_footer(frame, area, &app, &status, true, &zones))
            .expect("draw");
        let row = row_text(terminal.backend().buffer(), 0);
        assert!(row.contains("Auto (shift+tab to cycle)"), "{row}");
    }

    #[test]
    fn footer_prioritizes_background_tasks_after_permission_mode() {
        let mut app = AppState::default();
        app.set_permission_mode(rebon_permissions::PermissionMode::Auto);
        let registry = rebon_plugin_tasks::runtime::TaskRegistry::new();
        let task_id = rebon_plugin_tasks::runtime::TaskId::new("shell-1");
        let mut snapshot = rebon_plugin_tasks::runtime::TaskSnapshot::new_pending(
            task_id.clone(),
            "cargo test".into(),
            rebon_plugin_tasks::runtime::TaskData::LocalShell(
                rebon_plugin_tasks::runtime::LocalShellData {
                    command: "cargo test".into(),
                    exit_code: None,
                    interrupted: false,
                    display_kind: rebon_plugin_tasks::runtime::BashTaskKind::Bash,
                    agent_id: None,
                },
            ),
        );
        snapshot.status = rebon_plugin_tasks::runtime::TaskStatus::Running;
        snapshot.is_backgrounded = true;
        registry.insert(task_id, snapshot, rebon_types::PromptCancel::new());
        app.tasks = std::sync::Arc::new(registry);
        app.footer_selection = Some(rebon_tui::promptinput::footer_navigation::FooterItem::Tasks);

        let area = Rect::new(0, 0, 70, 1);
        let backend = TestBackend::new(70, 1);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let status = StatusBarInfo {
            provider: "test",
            model: "model",
            cwd: "cwd",
            elapsed_ms: 0,
            effort_display: String::new(),
            fast_mode_display: String::new(),
            context_left_pct: Some(5),
            agent_activity: None,
            goal_activity: None,
            footer_action_hint: None,
            new_session_hint: None,
        };
        let mut zones = test_layout_zones();
        zones.show_pill = true;
        zones.pill_count = 7;

        terminal
            .draw(|frame| render_footer(frame, area, &app, &status, true, &zones))
            .expect("draw");
        let row = row_text(terminal.backend().buffer(), 0);
        let permission = row.find("Auto (shift+tab to cycle)").expect("{row}");
        let tasks = row.find("1 background shell running").expect("{row}");
        assert!(permission < tasks, "{row}");
    }

    #[test]
    fn footer_agent_switcher_does_not_render_background_task_status() {
        let mut app = app_with_agent_switcher_rows();
        app.footer_selection = Some(rebon_tui::promptinput::footer_navigation::FooterItem::Tasks);
        let area = Rect::new(0, 0, 80, 1);
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let status = StatusBarInfo {
            provider: "test",
            model: "model",
            cwd: "cwd",
            elapsed_ms: 0,
            effort_display: String::new(),
            fast_mode_display: String::new(),
            context_left_pct: None,
            agent_activity: None,
            goal_activity: None,
            footer_action_hint: None,
            new_session_hint: None,
        };
        let zones = test_layout_zones();

        terminal
            .draw(|frame| render_footer(frame, area, &app, &status, true, &zones))
            .expect("draw");
        let row = row_text(terminal.backend().buffer(), 0);
        assert!(!row.contains("background task"), "{row}");
        assert!(!row.contains("background shell"), "{row}");

        let rows = render_full_inline_frame_rows(app, 12, 0);
        assert!(
            rows.iter().any(|row| row.contains("> @Main")),
            "agent row should own focus: {rows:?}"
        );
    }

    #[test]
    fn footer_renders_goal_elapsed_on_right() {
        let app = AppState::default();
        let area = Rect::new(0, 0, 80, 1);
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let status = StatusBarInfo {
            provider: "test",
            model: "model",
            cwd: "cwd",
            elapsed_ms: 0,
            effort_display: String::new(),
            fast_mode_display: String::new(),
            context_left_pct: None,
            agent_activity: None,
            goal_activity: Some(GoalActivityInfo {
                status: crate::goal::GoalStatus::Active,
                elapsed_ms: 65_000,
            }),
            footer_action_hint: None,
            new_session_hint: None,
        };
        let zones = test_layout_zones();

        terminal
            .draw(|frame| render_footer(frame, area, &app, &status, true, &zones))
            .expect("draw");
        let row = row_text(terminal.backend().buffer(), 0);
        assert!(row.contains("goal active · 1m 5s"), "{row}");
    }

    #[test]
    fn total_elapsed_timer_label_formats_duration() {
        assert_eq!(total_elapsed_timer_label(1_500), "total elapsed · 1s");
        assert_eq!(total_elapsed_timer_label(65_000), "total elapsed · 1m 5s");
    }

    #[test]
    fn goal_elapsed_timer_label_formats_duration() {
        assert_eq!(
            goal_elapsed_timer_label(crate::goal::GoalStatus::Active, 1_500),
            "goal active · 1s"
        );
        assert_eq!(
            goal_elapsed_timer_label(crate::goal::GoalStatus::Complete, 65_000),
            "goal complete · 1m 5s"
        );
    }

    #[test]
    fn ultraplan_header_label_renders_planning_chip() {
        let app = app_with_ultraplan_phase(UltraplanPhase::PlanModeActive);

        assert_eq!(ultraplan_header_label(&app), Some(" Ultraplan Planning "));
    }

    #[test]
    fn ultraplan_header_label_is_none_during_executing_phase() {
        // During Executing the footer's permission-mode pill already
        // signals the workflow state; the top-right chip stays hidden
        // to avoid a duplicate marker.
        let app = app_with_ultraplan_phase(UltraplanPhase::Executing);

        assert_eq!(ultraplan_header_label(&app), None);
    }

    #[test]
    fn ultraplan_header_label_is_none_without_status() {
        let app = AppState::default();

        assert_eq!(ultraplan_header_label(&app), None);
    }

    #[test]
    fn clear_chunk_background_resets_only_target_area() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        // Repro for the Ctrl+O ghost-residue bug: a chunk below the
        // transcript inherits stale glyphs from a previous frame when
        // its renderer paints sparse content. `clear_chunk_background`
        // must wipe every cell in the chunk's area (and only that area).
        let backend = TestBackend::new(20, 8);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| {
                // Pre-fill the entire buffer with stale content that
                // simulates leftover transcript glyphs.
                let area = frame.area();
                let buf = frame.buffer_mut();
                for y in area.y..area.y + area.height {
                    for x in area.x..area.x + area.width {
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.set_symbol("X");
                        }
                    }
                }

                // Clear only the bottom 3 rows — the chunk we own.
                let chunk = Rect::new(0, 5, 20, 3);
                clear_chunk_background(frame, chunk);

                let buf = frame.buffer_mut();
                // Rows 0-4 should be untouched.
                for y in 0..5 {
                    for x in 0..20 {
                        assert_eq!(
                            buf[(x, y)].symbol(),
                            "X",
                            "row {y} col {x} should retain stale glyph"
                        );
                    }
                }
                // Rows 5-7 should be reset to the empty-cell symbol.
                for y in 5..8 {
                    for x in 0..20 {
                        assert_eq!(
                            buf[(x, y)].symbol(),
                            " ",
                            "row {y} col {x} should be cleared"
                        );
                    }
                }
            })
            .expect("draw");
    }

    #[test]
    fn clear_chunk_background_is_a_noop_for_zero_dimensions() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(10, 4);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| {
                // Pre-fill so we can detect any unexpected mutation.
                let area = frame.area();
                let buf = frame.buffer_mut();
                for y in area.y..area.y + area.height {
                    for x in area.x..area.x + area.width {
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.set_symbol("X");
                        }
                    }
                }

                // Variable-height chunks can collapse to height==0 when
                // their feature is hidden (queue banner not visible,
                // ultraplan absent). The helper must not mutate the
                // buffer in that case — otherwise it would erase content
                // owned by the adjacent chunk.
                clear_chunk_background(frame, Rect::new(0, 2, 10, 0));
                clear_chunk_background(frame, Rect::new(0, 2, 0, 1));

                let buf = frame.buffer_mut();
                for y in 0..4 {
                    for x in 0..10 {
                        assert_eq!(
                            buf[(x, y)].symbol(),
                            "X",
                            "row {y} col {x} must not be touched"
                        );
                    }
                }
            })
            .expect("draw");
    }

    #[test]
    fn inline_commit_adds_separator_before_overlay_only_live_content() {
        let mut app = AppState::default();
        app.rebon_tui.transcript = TranscriptStore::from_rows(vec![user("u1", "committed prompt")]);
        app.rebon_tui
            .overlay
            .append_streaming_thinking("streaming overlay answer");

        let rows = render_full_inline_frame_rows(app, 12, 1);
        let overlay_y = row_y_containing(&rows, "streaming overlay answer");

        assert!(
            overlay_y > 0,
            "overlay should reserve a boundary row: {rows:?}"
        );
        assert!(
            rows[..overlay_y].iter().any(|row| row.trim().is_empty()),
            "expected blank separator before overlay-only live content: {rows:?}"
        );
    }

    #[test]
    fn inline_ask_user_other_reports_absolute_cursor() {
        let mut app = AppState::default();
        app.pending_permission_view = Some(ask_user_permission_view(true));

        let (rows, cursor) =
            render_full_inline_frame_rows_with_loading_and_terminal_height_and_cursor(
                app, 14, 30, 0, false,
            );
        let input_row = row_y_containing(&rows, "hello world");

        assert_eq!(cursor, Some((16, input_row as u16)), "rows: {rows:?}");
    }

    #[test]
    fn inline_commit_adds_separator_before_permission_only_live_content() {
        let mut app = AppState::default();
        app.rebon_tui.transcript = TranscriptStore::from_rows(vec![user("u1", "committed prompt")]);
        app.pending_permission_view = Some(permission_view());

        let rows = render_full_inline_frame_rows(app, 14, 1);
        let permission_y = row_y_containing(&rows, "Allow Bash?");

        assert!(
            permission_y > 0,
            "permission suffix should reserve a boundary row: {rows:?}"
        );
        assert!(
            rows[..permission_y].iter().any(|row| row.trim().is_empty()),
            "expected blank separator before permission-only live content: {rows:?}"
        );
    }

    #[test]
    fn inline_live_content_render_keeps_footer_status_row() {
        let mut app = AppState::default();
        app.rebon_tui.transcript =
            TranscriptStore::from_rows(vec![assistant_text("a1", "Directory listing for crates:")]);
        app.set_permission_mode(rebon_permissions::PermissionMode::AcceptEdits);

        let rows = render_full_inline_frame_rows(app, 6, 0);

        assert!(
            rows.iter()
                .any(|row| row.contains("Directory listing for crates")),
            "expected live transcript content to render: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("cwd")),
            "footer cwd/status row should stay visible while live inline content is visible: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("Accept edits")),
            "footer permission mode should stay visible while live inline content is visible: {rows:?}"
        );
    }

    #[test]
    fn inline_streaming_overlay_render_keeps_footer_status_row() {
        let mut app = AppState::default();
        app.rebon_tui
            .overlay
            .append_streaming_text("streaming assistant output");

        let rows = render_full_inline_frame_rows(app, 6, 0);

        assert!(
            rows.iter()
                .any(|row| row.contains("streaming assistant output")),
            "expected streaming overlay content to render: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("cwd")),
            "footer cwd/status row should stay visible while streaming inline content is visible: {rows:?}"
        );
    }

    #[test]
    fn inline_custom_status_line_keeps_permission_and_background_tasks() {
        let mut app = AppState::default();
        app.set_permission_mode(rebon_permissions::PermissionMode::Auto);
        app.custom_status_line.config = Some(crate::ui_config::StatusLineConfig {
            kind: crate::ui_config::StatusLineKind::Command,
            command: String::from("ignored"),
            script: None,
            padding: Some(1),
            refresh_interval: Some(1),
            hide_vim_mode_indicator: Some(false),
        });
        app.custom_status_line.output = vec![String::from("● GPT 5.6 SOL")];

        let registry = rebon_plugin_tasks::runtime::TaskRegistry::new();
        let task_id = rebon_plugin_tasks::runtime::TaskId::new("shell-1");
        let mut snapshot = rebon_plugin_tasks::runtime::TaskSnapshot::new_pending(
            task_id.clone(),
            "cargo test".into(),
            rebon_plugin_tasks::runtime::TaskData::LocalShell(
                rebon_plugin_tasks::runtime::LocalShellData {
                    command: "cargo test".into(),
                    exit_code: None,
                    interrupted: false,
                    display_kind: rebon_plugin_tasks::runtime::BashTaskKind::Bash,
                    agent_id: None,
                },
            ),
        );
        snapshot.status = rebon_plugin_tasks::runtime::TaskStatus::Running;
        snapshot.is_backgrounded = true;
        registry.insert(task_id, snapshot, rebon_types::PromptCancel::new());
        app.tasks = std::sync::Arc::new(registry);

        let rows = render_full_inline_frame_rows(app, 6, 0);
        let footer = rows
            .iter()
            .find(|row| row.contains("● GPT 5.6 SOL"))
            .expect("configured statusLine should render in inline footer");
        let permission = footer.find("⏵⏵ Auto").expect(footer);
        let tasks = footer.find("1 background shell running").expect(footer);
        let custom = footer.find("● GPT 5.6 SOL").expect(footer);
        assert!(permission < tasks && tasks < custom, "{footer:?}");
    }

    #[test]
    fn inline_custom_status_line_wraps_instead_of_clipping() {
        let mut app = AppState::default();
        app.custom_status_line.config = Some(crate::ui_config::StatusLineConfig {
            kind: crate::ui_config::StatusLineKind::Command,
            command: String::from("ignored"),
            script: None,
            padding: Some(0),
            refresh_interval: Some(1),
            hide_vim_mode_indicator: Some(false),
        });
        let first_row = "A".repeat(80);
        app.custom_status_line.output = vec![format!("{first_row}SECOND")];

        let rows = render_full_inline_frame_rows(app, 7, 0);
        let first = rows
            .iter()
            .position(|row| row == &first_row)
            .expect("first wrapped statusLine row should remain visible");
        let second = rows
            .iter()
            .position(|row| row.trim_end() == "SECOND")
            .expect("statusLine overflow should continue on a second row");

        assert_eq!(second, first + 1, "{rows:?}");
    }

    #[test]
    fn inline_queue_banner_renders_above_prompt() {
        let mut app = AppState::default();
        app.queued_commands
            .push(rebon_tui::promptinput::QueuedCommand {
                mode: "prompt".into(),
                value: rebon_tui::promptinput::QueuedCommandValue::Text("first queued".into()),
            });
        app.queued_commands
            .push(rebon_tui::promptinput::QueuedCommand {
                mode: "prompt".into(),
                value: rebon_tui::promptinput::QueuedCommandValue::Text("second queued".into()),
            });

        let rows = render_full_inline_frame_rows(app, 9, 0);

        assert!(
            rows.iter().any(|row| row.contains("2 queued messages")),
            "queued header should render in inline mode: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("first queued")),
            "first queued message should render in inline mode: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("second queued")),
            "second queued message should render in inline mode: {rows:?}"
        );
    }

    #[test]
    fn inline_permission_temporarily_hides_queue_and_prompt_surface() {
        let mut app = AppState::default();
        app.input = "draft prompt".into();
        app.cursor_offset = app.input.len();
        app.pending_permission_view = Some(permission_view());

        let desired_without_queue = desired_inline_height_for_test(&app, 4, 30, 0);
        app.queued_commands
            .push(rebon_tui::promptinput::QueuedCommand {
                mode: "prompt".into(),
                value: rebon_tui::promptinput::QueuedCommandValue::Text(
                    "queued while asking".into(),
                ),
            });
        let desired_with_queue = desired_inline_height_for_test(&app, 4, 30, 0);
        assert_eq!(
            desired_with_queue, desired_without_queue,
            "hidden queue should not reserve viewport rows while permission is pending"
        );

        let rows = render_full_inline_frame_rows(app, desired_with_queue, 0);

        assert!(
            rows.iter().any(|row| row.contains("Allow Bash?")),
            "permission surface should render: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("1 queued message")),
            "queued header should be hidden while permission is pending: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("queued while asking")),
            "queued message should be hidden while permission is pending: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("draft prompt")),
            "permission surface should replace prompt input content: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains('┌')),
            "permission surface should not leave the prompt top border behind: {rows:?}"
        );
    }

    #[test]
    fn loading_waiting_inline_viewport_preserves_full_prompt_border() {
        let mut app = AppState::default();
        app.input = "帮我用 explorer 看下 help 相关的代码".into();
        app.cursor_offset = app.input.len();

        let desired = desired_inline_height_for_test(&app, 2, 20, 0);
        assert!(
            desired >= 4,
            "loading prompt should reserve prompt + footer rows, got {desired}"
        );

        let rows = render_full_inline_frame_rows_with_loading(app, desired, 0, true);
        assert!(
            rows.iter()
                .any(|row| row.starts_with("│❯ ") && row.contains("explorer")),
            "loading prompt should keep the normal input glyph: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains('┌')),
            "prompt top border should render during waiting/loading: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains('└')),
            "prompt bottom border should render during waiting/loading: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("cwd")),
            "footer should render below waiting/loading prompt: {rows:?}"
        );
    }

    #[test]
    fn inline_render_uses_terminal_height_cap_after_viewport_expands_for_multiline_prompt() {
        let mut app = AppState::default();
        app.input = "one\ntwo\nthree".into();
        app.cursor_offset = app.input.len();

        let desired = desired_inline_height_for_test(&app, 4, 30, 0);
        assert!(
            desired > 4,
            "multiline prompt should expand inline viewport, got {desired}"
        );

        let rows = render_full_inline_frame_rows_with_loading_and_terminal_height(
            app, desired, 30, 0, false,
        );

        assert!(
            rows.iter().any(|row| row.contains("one")),
            "first prompt line should render: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("two")),
            "second prompt line should render: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("three")),
            "third prompt line should render: {rows:?}"
        );
    }

    #[test]
    fn desired_inline_viewport_expands_for_live_preview_and_queue_banner() {
        let mut app = AppState::default();
        app.rebon_tui
            .overlay
            .append_streaming_text("streaming assistant output");
        app.queued_commands
            .push(rebon_tui::promptinput::QueuedCommand {
                mode: "prompt".into(),
                value: rebon_tui::promptinput::QueuedCommandValue::Text(
                    "queued while streaming".into(),
                ),
            });

        let desired = desired_inline_height_for_test(&app, 4, 20, 0);

        assert!(
            desired > 4,
            "live preview plus queued banner should expand inline viewport, got {desired}"
        );
        let rows = render_full_inline_frame_rows(app, desired, 0);
        assert!(
            rows.iter()
                .any(|row| row.contains("streaming assistant output")),
            "live preview should render in expanded viewport: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("1 queued message")),
            "queued banner should render in expanded viewport: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("cwd")),
            "footer should render in expanded viewport: {rows:?}"
        );
    }

    #[test]
    fn desired_inline_viewport_expands_for_at_picker_below_prompt() {
        let app = at_mention_picker_app();

        let desired = desired_inline_height_for_test(&app, 4, 20, 0);

        assert!(
            desired > 4,
            "@ picker should expand inline viewport, got {desired}"
        );
        let rows = render_full_inline_frame_rows(app, desired, 0);
        assert!(
            rows.iter().any(|row| row.contains("@ mentions")),
            "@ picker should render in expanded viewport: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("cwd")),
            "footer should stay visible under expanded picker: {rows:?}"
        );
    }

    #[test]
    fn desired_inline_viewport_does_not_reserve_base_blank_rows_for_picker() {
        let app = deterministic_at_mention_picker_app();

        let desired = desired_inline_height_for_test(&app, 12, 30, 0);

        assert!(
            desired < 12,
            "@ picker should size to content, got {desired}"
        );
        let rows = render_full_inline_frame_rows(app, desired, 0);
        assert!(
            rows.last().is_some_and(|row| row.contains("cwd")),
            "content-sized picker viewport should end at the footer, not blank rows: {rows:?}"
        );
    }

    #[test]
    fn quiet_inline_viewport_drops_base_height_floor() {
        let app = AppState::default();

        // The idle viewport no longer floors to the configured base_height: a
        // small and a large base produce the SAME height (content + a single
        // reservoir row), and neither inflates to the tall base.
        let small_base = desired_inline_height_for_test(&app, 4, 30, 0);
        let large_base = desired_inline_height_for_test(&app, 20, 30, 0);

        assert_eq!(
            small_base, large_base,
            "idle viewport tracks content + reservoir, not the base_height floor"
        );
        assert!(
            large_base < 20,
            "idle viewport must not inflate to a tall base_height: {large_base}"
        );
    }

    /// Natural-flow idle viewport keeps exactly one reservoir row beneath the
    /// footer (`IDLE_BOTTOM_RESERVOIR_ROWS`): adding a content row grows the
    /// viewport by exactly one, so the blank band below the prompt never
    /// inflates past a single breathing row.
    #[test]
    fn inline_natural_flow_viewport_keeps_single_row_reservoir() {
        let _env = InlineTaskEnv::new("inline-flow-floor");
        let mut app = AppState::default();

        app.rebon_tui.overlay.append_streaming_text("alpha");
        let one_line = desired_inline_height_for_test(&app, 12, 30, 0);

        app.rebon_tui.overlay.clear();
        app.rebon_tui.overlay.append_streaming_text("alpha\nbeta");
        let two_lines = desired_inline_height_for_test(&app, 12, 30, 0);

        assert_eq!(
            two_lines,
            one_line + 1,
            "each extra content row grows the idle viewport by exactly one, so the reservoir stays a single row"
        );
    }

    #[test]
    fn desired_inline_viewport_clamps_to_terminal_height() {
        let mut app = AppState::default();
        app.rebon_tui
            .overlay
            .append_streaming_text("line one\nline two\nline three\nline four\nline five");
        for idx in 0..10 {
            app.queued_commands
                .push(rebon_tui::promptinput::QueuedCommand {
                    mode: "prompt".into(),
                    value: rebon_tui::promptinput::QueuedCommandValue::Text(format!(
                        "queued {idx}"
                    )),
                });
        }

        assert_eq!(desired_inline_height_for_test(&app, 4, 8, 0), 8);
    }

    #[test]
    fn inline_queue_banner_stays_below_live_streaming_content() {
        let mut app = AppState::default();
        app.rebon_tui
            .overlay
            .append_streaming_text("streaming assistant output");
        app.queued_commands
            .push(rebon_tui::promptinput::QueuedCommand {
                mode: "prompt".into(),
                value: rebon_tui::promptinput::QueuedCommandValue::Text(
                    "queued while streaming".into(),
                ),
            });

        let rows = render_full_inline_frame_rows(app, 10, 0);
        let streaming_y = row_y_containing(&rows, "streaming assistant output");
        let queue_y = row_y_containing(&rows, "1 queued message");

        assert!(
            streaming_y < queue_y,
            "live transcript should stay above queued banner: {rows:?}"
        );
    }

    #[test]
    fn inline_queue_banner_hides_footer_on_tight_viewport() {
        let mut app = AppState::default();
        app.rebon_tui
            .overlay
            .append_streaming_text("streaming output");
        app.queued_commands
            .push(rebon_tui::promptinput::QueuedCommand {
                mode: "prompt".into(),
                value: rebon_tui::promptinput::QueuedCommandValue::Text("queued msg".into()),
            });

        let rows = render_full_inline_frame_rows(app, 6, 0);

        assert!(
            rows.iter().any(|row| row.contains("streaming output")),
            "transcript content should render: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains('┌')),
            "prompt border should be preserved: {rows:?}"
        );
    }

    #[test]
    fn inline_queue_preserves_prompt_border() {
        let mut app = AppState::default();
        app.rebon_tui.overlay.append_streaming_text("streamed text");
        app.queued_commands
            .push(rebon_tui::promptinput::QueuedCommand {
                mode: "prompt".into(),
                value: rebon_tui::promptinput::QueuedCommandValue::Text("queued".into()),
            });

        let rows = render_full_inline_frame_rows(app, 8, 0);

        assert!(
            rows.iter().any(|row| row.contains("1 queued")),
            "queue banner should render: {rows:?}"
        );
        assert!(
            rows.iter()
                .filter(|row| row.contains('┌') || row.contains('└'))
                .count()
                >= 2,
            "prompt should have both top and bottom border: {rows:?}"
        );
    }

    #[test]
    fn update_notice_does_not_leave_landing_prompt_for_empty_screen() {
        let mut app = AppState::default();
        app.set_update_notice(update_notice());

        assert!(should_render_landing_prompt(&app, false, 0, 0));
    }

    #[test]
    fn mcp_load_hint_does_not_leave_landing_prompt_for_empty_screen() {
        let mut app = AppState::default();
        app.set_mcp_load_hint("MCP unavailable: run /mcp for details");

        assert!(should_render_landing_prompt(&app, false, 0, 0));
        let rows = render_screen_rows(app, 100, 40);
        let title_y = row_y_containing(&rows, "What's the plan for today?");
        assert!(
            title_y > 10 && title_y < 30,
            "MCP warning hint should keep the landing prompt centered: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("MCP unavailable")),
            "MCP warning hint should render in prompt chrome: {rows:?}"
        );
    }

    #[test]
    fn screen_task_list_does_not_pull_landing_prompt_to_bottom() {
        let env = InlineTaskEnv::new("screen-landing-task-list-draft");
        env.create_task("Script task visible while idle");

        let idle_rows = render_screen_rows(AppState::default(), 100, 40);
        let idle_title_y = row_y_containing(&idle_rows, "What's the plan for today?");
        assert!(
            idle_title_y > 10 && idle_title_y < 30,
            "idle task/script list should keep the landing prompt centered: {idle_rows:?}"
        );
        assert!(
            idle_rows
                .iter()
                .any(|row| row.contains("Script task visible while idle")),
            "task/script list should remain visible while idle: {idle_rows:?}"
        );

        let mut typing_app = AppState::default();
        typing_app.input = "hello".into();
        typing_app.cursor_offset = typing_app.input.len();
        assert!(should_render_landing_prompt(&typing_app, false, 3, 0));

        let typing_rows = render_screen_rows(typing_app, 100, 40);
        let title_y = row_y_containing(&typing_rows, "What's the plan for today?");
        let prompt_y = row_y_containing(&typing_rows, "hello");
        assert!(
            title_y > 10 && prompt_y > title_y && prompt_y < 30,
            "typing a draft should keep the centered landing prompt before submit: {typing_rows:?}"
        );
        assert!(
            typing_rows
                .iter()
                .all(|row| !row.contains("Script task visible while idle")),
            "task/script list should not pull an active draft to the bottom: {typing_rows:?}"
        );
    }

    #[test]
    fn screen_bash_mode_keeps_task_list_landing_prompt_centered_before_submit() {
        let env = InlineTaskEnv::new("screen-bash-landing-task-list");
        env.create_task("Script task should wait for submit");
        let mut app = AppState::default();
        app.mode = "bash".into();
        app.input = "echo hi".into();
        app.cursor_offset = app.input.len();

        assert!(should_render_landing_prompt(&app, false, 3, 0));

        let rows = render_screen_rows(app, 100, 40);
        let title_y = row_y_containing(&rows, "What's the plan for today?");
        let prompt_y = row_y_containing(&rows, "bash");

        assert!(
            title_y > 10 && prompt_y > title_y && prompt_y < 30,
            "bash/script mode should stay in the centered landing prompt before submit: {rows:?}"
        );
        assert!(
            rows.iter()
                .all(|row| !row.contains("Script task should wait for submit")),
            "task/script list should not pull bash/script drafts to the bottom: {rows:?}"
        );
    }

    #[test]
    fn inline_update_notice_renders_as_idle_prompt_top_hint() {
        let mut app = AppState::default();
        app.set_update_notice(update_notice());

        let rows = render_full_inline_frame_rows(app, 6, 0);

        assert!(
            rows.iter().any(|row| row.contains("Update available")),
            "update notice did not render in inline prompt: {rows:?}"
        );
    }

    #[test]
    fn inline_verbose_replaces_input_with_transcript_status() {
        let mut app = AppState::default();
        app.input = "hidden draft".into();
        app.cursor_offset = app.input.len();
        app.tool_output_verbosity = ToolOutputVerbosity::Verbose;

        let (rows, cursor) =
            render_full_inline_frame_rows_with_loading_and_terminal_height_and_cursor(
                app, 6, 6, 0, false,
            );

        assert!(
            rows.iter().any(|row| row.chars().all(|ch| ch == '─')),
            "verbose separator did not render: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| {
                row.contains("Showing detailed transcript · ctrl+o to toggle · ctrl+e to show all")
                    && row.trim_end().ends_with("verbose")
            }),
            "verbose transcript status did not render: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("hidden draft")),
            "inline prompt input should be hidden in verbose mode: {rows:?}"
        );
        assert_eq!(cursor, None);
    }

    #[test]
    fn screen_verbose_keeps_the_prompt_input() {
        let mut app = AppState::default();
        app.input = "screen draft".into();
        app.cursor_offset = app.input.len();
        app.tool_output_verbosity = ToolOutputVerbosity::Verbose;

        let rows = render_screen_rows(app, 100, 20);

        assert!(
            rows.iter().any(|row| row.contains("screen draft")),
            "{rows:?}"
        );
        assert!(rows
            .iter()
            .all(|row| !row.contains("Showing detailed transcript")));
    }

    #[test]
    fn idle_prompt_top_hint_hides_while_loading_or_when_input_has_text() {
        let mut loading_app = AppState::default();
        loading_app.set_update_notice(update_notice());
        let loading_rows = render_full_inline_frame_rows_with_loading(loading_app, 6, 0, true);
        assert!(
            !loading_rows
                .iter()
                .any(|row| row.contains("Update available")),
            "idle prompt top hint should hide while loading: {loading_rows:?}"
        );

        let mut typing_app = AppState::default();
        typing_app.set_update_notice(update_notice());
        typing_app.input = "hello".into();
        typing_app.cursor_offset = typing_app.input.len();
        let typing_rows = render_full_inline_frame_rows(typing_app, 6, 0);
        assert!(
            !typing_rows
                .iter()
                .any(|row| row.contains("Update available")),
            "idle prompt top hint should hide when prompt has text: {typing_rows:?}"
        );
    }

    #[test]
    fn inline_live_slice_after_committed_prompt_has_one_boundary_row() {
        let mut app = AppState::default();
        app.rebon_tui.transcript = TranscriptStore::from_rows(vec![
            user("u1", "committed prompt"),
            assistant_text("a1", "later assistant answer"),
        ]);

        let rows = render_full_inline_frame_rows(app, 12, 1);
        let semantic = trim_blank_boundaries(&rows);
        let assistant_y = row_y_containing(&semantic, "later assistant answer");

        assert_eq!(assistant_y, 0, "{semantic:?}");
        assert!(!semantic[0].trim().is_empty(), "{semantic:?}");
    }

    #[test]
    fn inline_live_slice_after_committed_prompt_with_tool_group_has_normal_boundaries() {
        let mut app = AppState::default();
        app.rebon_tui.transcript = TranscriptStore::from_rows(vec![
            user("u1", "committed prompt"),
            assistant_text("a1", "first assistant text"),
            assistant_tool("t1", "tool-1", "Grep", "needle"),
            assistant_tool("t2", "tool-2", "Read", "src/lib.rs"),
            assistant_text("a2", "second assistant text"),
        ]);

        let rows = render_full_inline_frame_rows(app, 16, 1);
        let semantic = trim_blank_boundaries(&rows);
        let first_text_y = row_y_containing(&semantic, "first assistant text");
        let tool_y = row_y_containing(&semantic, "read");
        let second_text_y = row_y_containing(&semantic, "second assistant text");

        assert_eq!(first_text_y, 0, "{semantic:?}");
        assert_eq!(tool_y, first_text_y + 2, "{semantic:?}");
        assert_eq!(second_text_y, tool_y + 2, "{semantic:?}");
        assert!(semantic[first_text_y + 1].trim().is_empty(), "{semantic:?}");
        assert!(semantic[tool_y + 1].trim().is_empty(), "{semantic:?}");
    }

    #[test]
    fn inline_commit_tiny_transcript_height_prefers_live_content_over_blank_separator() {
        let mut app = AppState::default();
        app.rebon_tui.transcript = TranscriptStore::from_rows(vec![user("u1", "committed prompt")]);
        app.rebon_tui
            .overlay
            .append_streaming_thinking("tiny live content");
        app.input = "one\ntwo\nthree\nfour\nfive\nsix".into();
        app.cursor_offset = app.input.len();

        let rows = render_full_inline_frame_rows(app, 3, 1);
        let live_y = row_y_containing(&rows, "tiny live content");

        assert_eq!(
            live_y, 0,
            "single transcript row should show content: {rows:?}"
        );
    }

    #[test]
    fn inline_transcript_adds_blank_row_when_prompt_and_assistant_share_live_slice() {
        let rows = render_inline_rows(
            vec![
                user("u1", "帮我用 Explore 查一下我的 ACP 相关代码"),
                assistant_text("a1", "我用 Explore 做只读梳理，查 ACP 相关代码位置和关系。"),
            ],
            8,
        );
        let semantic = trim_blank_boundaries(&rows);
        let user_y = row_y_containing(&semantic, "Explore");
        let assistant_y = row_y_containing(&semantic, "只 读 梳 理");

        assert_eq!(assistant_y, user_y + 2, "{semantic:?}");
        assert!(semantic[user_y + 1].trim().is_empty(), "{semantic:?}");
    }

    #[test]
    fn inline_transcript_preserves_screen_like_spacing_between_prompt_text_and_tools() {
        let rows = render_inline_rows(
            vec![
                user("u1", "帮我查一下 acp 相关的代码"),
                assistant_text("a1", "我先搜一下代码里所有 acp 命中点。"),
                assistant_tool("t1", "tool-1", "Grep", "acp"),
                assistant_tool("t2", "tool-2", "Grep", "acp crate"),
                assistant_text("a2", "我找到了 ACP 文件，继续看入口和模块结构。"),
                assistant_tool("t3", "tool-3", "Grep", "entry"),
                assistant_tool("t4", "tool-4", "Read", "crates/rebon-acp/src/lib.rs"),
                assistant_text("a3", "查到了，ACP 相关代码主要分两块。"),
            ],
            24,
        );
        let semantic = trim_blank_boundaries(&rows);

        let user_y = row_y_containing(&semantic, "acp");
        let first_text_y = row_y_containing(&semantic, "命 中 点");
        let first_tool_y = row_y_containing(&semantic, "Searched");
        let second_text_y = row_y_containing(&semantic, "ACP 文 件");
        let second_tool_y = row_y_containing(&semantic, "read");
        let final_text_y = row_y_containing(&semantic, "两 块");

        assert_eq!(first_text_y, user_y + 2, "{semantic:?}");
        assert_eq!(first_tool_y, first_text_y + 2, "{semantic:?}");
        assert_eq!(second_text_y, first_tool_y + 2, "{semantic:?}");
        assert_eq!(second_tool_y, second_text_y + 2, "{semantic:?}");
        assert_eq!(final_text_y, second_tool_y + 2, "{semantic:?}");
    }

    #[test]
    fn inline_measured_height_counts_inter_block_spacing_for_bottom_anchor() {
        let rows = vec![
            user("u1", "帮我查一下 acp 相关的代码"),
            assistant_text("a1", "我先搜一下代码里所有 acp 命中点。"),
            assistant_tool("t1", "tool-1", "Grep", "acp"),
            assistant_tool("t2", "tool-2", "Grep", "acp crate"),
            assistant_text("a2", "我找到了 ACP 文件，继续看入口和模块结构。"),
            assistant_tool("t3", "tool-3", "Grep", "entry"),
            assistant_tool("t4", "tool-4", "Read", "crates/rebon-acp/src/lib.rs"),
            assistant_text("a3", "查到了，ACP 相关代码主要分两块。"),
        ];
        let measured = inline_transcript_height(rows.clone());
        let painted_semantic_len =
            trim_blank_boundaries(&render_inline_rows(rows, 32)).len() as u16;

        assert_eq!(measured, painted_semantic_len);
        assert!(
            measured >= 11,
            "expected spacing rows in height: {measured}"
        );

        let area = Rect::new(0, 0, 80, 10);
        let layout = inline_layout(area, 2, measured, false);
        assert_eq!(layout.footer.y + layout.footer.height, area.y + area.height);
        assert_eq!(layout.prompt.y + layout.prompt.height, layout.footer.y);
        assert_eq!(
            layout.transcript.height,
            area.height - layout.prompt.height - layout.footer.height
        );
    }

    #[test]
    fn inline_edit_diff_keeps_moderate_preview_full_in_compact_mode() {
        let old = "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta";
        let new = "ALPHA\nBETA\nGAMMA\nDELTA\nEPSILON\nZETA\nETA\nTHETA";
        let rows = vec![
            user("u1", "改一下这些常量"),
            assistant_edit_tool("a1", "tool-edit", "src/consts.rs", old, new),
        ];

        let compact_h =
            inline_transcript_height_with_verbosity(rows.clone(), ToolOutputVerbosity::Compact);
        let verbose_h =
            inline_transcript_height_with_verbosity(rows.clone(), ToolOutputVerbosity::Verbose);
        assert_eq!(
            compact_h, verbose_h,
            "moderate inline edit preview should stay full in compact mode: \
             compact={compact_h} verbose={verbose_h}"
        );

        let compact_text =
            render_inline_rows_with_verbosity(rows.clone(), 60, ToolOutputVerbosity::Compact)
                .join("\n");
        let verbose_text =
            render_inline_rows_with_verbosity(rows, 60, ToolOutputVerbosity::Verbose).join("\n");

        assert!(
            !compact_text.contains("\u{2026} +"),
            "moderate compact inline edit preview should not truncate:\n{compact_text}"
        );
        assert!(
            compact_text.contains("THETA"),
            "moderate compact inline edit preview should include every changed line:\n{compact_text}"
        );
        assert!(
            !verbose_text.contains("\u{2026} +"),
            "verbose inline render should not truncate:\n{verbose_text}"
        );
        assert!(
            verbose_text.contains("THETA"),
            "verbose inline render should include every changed line:\n{verbose_text}"
        );
    }

    #[test]
    fn inline_long_edit_diff_folds_like_prompt_until_verbose() {
        let old = "";
        let new = (1..=30)
            .map(|line| format!("added {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = vec![
            user("u1", "追加这些行"),
            assistant_edit_tool("a1", "tool-edit", "src/long.rs", old, &new),
        ];

        let compact_h =
            inline_transcript_height_with_verbosity(rows.clone(), ToolOutputVerbosity::Compact);
        let verbose_h =
            inline_transcript_height_with_verbosity(rows.clone(), ToolOutputVerbosity::Verbose);
        assert!(
            compact_h < verbose_h,
            "compact={compact_h} verbose={verbose_h}"
        );

        let compact_text =
            render_inline_rows_with_verbosity(rows.clone(), 80, ToolOutputVerbosity::Compact)
                .join("\n");
        let verbose_text =
            render_inline_rows_with_verbosity(rows, 80, ToolOutputVerbosity::Verbose).join("\n");

        assert!(
            compact_text.contains("──── (10 lines hidden) ─"),
            "{compact_text}"
        );
        assert!(compact_text.contains("added 10"), "{compact_text}");
        assert!(!compact_text.contains("added 11"), "{compact_text}");
        assert!(compact_text.contains("added 21"), "{compact_text}");
        assert!(compact_text.contains("added 30"), "{compact_text}");
        assert!(!verbose_text.contains("lines hidden"), "{verbose_text}");
        assert!(verbose_text.contains("added 11"), "{verbose_text}");
    }

    #[test]
    fn screen_long_edit_diff_folds_like_prompt_until_verbose() {
        let old = "";
        let new = (1..=30)
            .map(|line| format!("added {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = vec![
            user("u1", "追加这些行"),
            assistant_edit_tool("a1", "tool-edit", "src/long.rs", old, &new),
        ];
        let mut compact_app = AppState::default();
        compact_app.tool_output_verbosity = ToolOutputVerbosity::Compact;
        compact_app.rebon_tui.transcript = TranscriptStore::from_rows(rows.clone());
        let mut verbose_app = AppState::default();
        verbose_app.tool_output_verbosity = ToolOutputVerbosity::Verbose;
        verbose_app.rebon_tui.transcript = TranscriptStore::from_rows(rows);

        let compact_text = render_screen_rows(compact_app, 100, 80).join("\n");
        let verbose_text = render_screen_rows(verbose_app, 100, 80).join("\n");

        assert!(
            compact_text.contains("──── (10 lines hidden) ─"),
            "{compact_text}"
        );
        assert!(!compact_text.contains("added 11"), "{compact_text}");
        assert!(compact_text.contains("added 21"), "{compact_text}");
        assert!(compact_text.contains("added 30"), "{compact_text}");
        assert!(!verbose_text.contains("lines hidden"), "{verbose_text}");
        assert!(verbose_text.contains("added 11"), "{verbose_text}");
    }

    #[test]
    fn inline_ask_user_redraw_after_long_fenced_response_and_open_tool_does_not_overflow() {
        let mut app = AppState::default();
        app.ui_mode = crate::ui_config::UiMode::Inline;
        let fenced_body = (1..=24)
            .map(|line| format!("let value_{line} = {line};"))
            .collect::<Vec<_>>()
            .join("\n");
        app.rebon_tui.overlay.append_streaming_text(&format!(
            "Here is the requested code:\n```rust\n{fenced_body}\n```\n"
        ));
        app.rebon_tui
            .overlay
            .upsert_streaming_tool_use(rebon_tui::StreamingToolUse {
                call_id: "tool-ask".into(),
                tool_name: "AskUserQuestion".into(),
                kind: rebon_types::ToolKind::Other,
                status: ToolCallStatus::Pending,
                title: Some("Answer questions".into()),
                content: None,
                locations: None,
                raw_input: None,
                raw_output: None,
            });

        // Match the production overflow path: the long, closed assistant text
        // drains, while the permission's still-open tool call remains live and
        // cannot make progress through another force-drain.
        crate::tui::update::force_drain_overlay_sealed_prefix(&mut app);
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        assert!(matches!(
            app.rebon_tui.overlay.blocks.as_slice(),
            [rebon_tui::StreamingContentBlock::ToolUse(tool)]
                if tool.status == ToolCallStatus::Pending
        ));

        let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
        let mut pending_permission = Some(PendingPermission {
            view: ask_user_permission_view(false),
            outbound: OutboundPermissionQuery {
                id: 2,
                tool_name: "AskUserQuestion".into(),
                tool_call_id: "tool-ask".into(),
                session_id: "session".into(),
                title: "Answer questions".into(),
                message: "Answer the question".into(),
                tool_input: None,
                metadata: None,
                options: vec![PermissionQueryOption {
                    option_id: "allow_once".into(),
                    label: "Allow once".into(),
                    kind: PermissionOptionKind::AllowOnce,
                }],
                response_tx,
            },
        });
        crate::tui::runner::permission_flow::sync_permission_view(&mut app, &pending_permission);

        let unconstrained = InlineViewportHeightInput {
            width: 80,
            terminal_height: 200,
            base_height: 4,
            committed_rows: 0,
            elapsed_ms: 0,
        };
        let exact_fit = desired_inline_viewport_height(&app, &RenderTheme::plain(), unconstrained);
        assert!(exact_fit < unconstrained.terminal_height);
        let exact_fit_input = InlineViewportHeightInput {
            terminal_height: exact_fit,
            ..unconstrained
        };
        assert_eq!(
            desired_inline_viewport_height(&app, &RenderTheme::plain(), exact_fit_input),
            exact_fit
        );
        assert!(
            !inline_live_content_overflows_viewport(
                &app,
                &RenderTheme::plain(),
                exact_fit_input,
            ),
            "permission-active overflow geometry must hide the same chrome as desired-height/render"
        );

        let policy_store = rebon_core::policy::PolicyStore::new();
        crate::tui::runner::permission_flow::apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveNext,
            &policy_store,
            ".",
        );
        let (next_rows, next_cursor) =
            render_full_inline_frame_rows_with_loading_and_terminal_height_and_cursor(
                app.clone(),
                exact_fit,
                exact_fit,
                0,
                true,
            );
        assert!(
            next_rows
                .iter()
                .any(|row| row.contains("How should we proceed?")),
            "AskUserQuestion suffix should remain visible after MoveNext: {next_rows:?}"
        );
        assert!(
            next_cursor.is_some(),
            "MoveNext should focus Other and expose its cursor on the next frame"
        );

        crate::tui::runner::permission_flow::apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MovePrev,
            &policy_store,
            ".",
        );
        let (prev_rows, prev_cursor) =
            render_full_inline_frame_rows_with_loading_and_terminal_height_and_cursor(
                app, exact_fit, exact_fit, 0, true,
            );
        assert!(
            prev_rows.iter().any(|row| row.contains("Use default")),
            "MovePrev should redraw the option highlight frame: {prev_rows:?}"
        );
        assert_eq!(
            prev_cursor, None,
            "MovePrev should remove the Other text cursor on the next frame"
        );
    }

    #[test]
    fn inline_permission_hides_footer_and_agent_switcher() {
        let mut app = app_with_agent_switcher_rows();
        app.pending_permission_view = Some(permission_view());

        let rows = render_full_inline_frame_rows(app, 8, 0);
        assert!(
            rows.iter().any(|row| row.contains("Allow")),
            "permission options should be visible: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("Allow Bash?")),
            "permission title should be visible (not clipped): {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("background task")),
            "footer should be hidden when permission is pending: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("@Main")),
            "agent switcher should be hidden when permission is pending: {rows:?}"
        );
    }

    #[test]
    fn inline_render_shows_task_list_above_prompt() {
        let env = InlineTaskEnv::new("inline-render-task-list");
        env.create_task("Inline task visible");
        let app = AppState::default();

        let rows = render_full_inline_frame_rows(app, 12, 0);
        let task_y = row_y_containing(&rows, "Inline task visible");
        let prompt_y = row_y_containing(&rows, "┌");

        assert!(
            task_y < prompt_y,
            "task list should render above prompt in inline mode: {rows:?}"
        );
    }

    #[test]
    fn task_list_render_clears_stale_row_content() {
        let tasks = vec![ListTask {
            id: "1".into(),
            subject: "Fix".into(),
            status: TaskListStatus::InProgress,
            owner: None,
            blocked_by: Vec::new(),
        }];
        let theme = RenderTheme::plain();
        let backend = TestBackend::new(40, 4);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let area = Rect::new(0, 0, 40, 4);

        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new("  ■ not a task placeholder"),
                    Rect::new(0, 2, 40, 1),
                );
            })
            .expect("seed draw");
        terminal
            .draw(|frame| {
                render_task_list(frame, area, &tasks, &[], &theme, false, 20);
            })
            .expect("task draw");

        let rows = all_rows(terminal.backend().buffer());
        assert!(
            rows.iter().any(|row| row.contains("Fix")),
            "task row should render the task subject: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("not a task")),
            "task area should not retain prior prompt/transcript text: {rows:?}"
        );
    }

    #[test]
    fn inline_render_shows_ultraplan_activity_immediately_above_prompt() {
        let _env = InlineTaskEnv::new("inline-ultraplan-activity");
        let app = app_with_ultraplan_phase(UltraplanPhase::Researching);

        let rows = render_full_inline_frame_rows(app, 12, 0);
        let activity_y = row_y_containing(&rows, "Activity:");
        let prompt_y = row_y_containing(&rows, "┌");

        assert_eq!(
            activity_y + 1,
            prompt_y,
            "ultraplan activity should be immediately above the prompt: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("Ultraplan ·")),
            "ultraplan summary line should not render: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("test task")),
            "ultraplan task title should not render: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| {
                let trimmed = row.trim();
                trimmed.is_empty() || trimmed.chars().any(|ch| ch != '─')
            }),
            "ultraplan top border should not render: {rows:?}"
        );
    }

    #[test]
    fn inline_prompt_border_shows_coordinator_mode_badge() {
        let mut app = AppState::default();
        app.coordinator_mode = true;

        let buf = render_owned_inline_frame_buffer(app, 6, 0);
        let rows = all_rows(&buf);
        let cell = cell_at_text(&buf, "Coordinator Mode").expect("coordinator badge should render");

        assert!(
            rows.iter().any(|row| row.contains("Coordinator Mode")),
            "inline prompt should show coordinator badge: {rows:?}"
        );
        assert_eq!(cell.fg, Color::White);
        assert!(
            cell.bg != Color::Reset,
            "badge should set a background color"
        );
    }

    #[test]
    fn inline_prompt_border_shows_ultraplan_mode_badge() {
        let app = app_with_ultraplan_phase(UltraplanPhase::Researching);

        let buf = render_owned_inline_frame_buffer(app, 12, 0);
        let rows = all_rows(&buf);
        let cell = cell_at_text(&buf, "Ultraplan Mode").expect("ultraplan badge should render");

        assert!(
            rows.iter().any(|row| row.contains("Ultraplan Mode")),
            "inline prompt should show ultraplan badge: {rows:?}"
        );
        assert_eq!(cell.fg, Color::White);
        assert!(
            cell.bg != Color::Reset,
            "badge should set a background color"
        );
    }

    #[test]
    fn inline_agent_switcher_renders_below_footer_when_no_permission_modal() {
        let app = app_with_agent_switcher_rows();

        let rows = render_full_inline_frame_rows(app, 12, 0);
        let footer_y = row_y_containing(&rows, " cwd");
        let main_y = row_y_containing(&rows, "@Main");
        let verification_y = row_y_containing(&rows, "@verify-final-app-stability");

        assert!(
            rows[..footer_y].iter().any(|row| row.contains('┌')),
            "prompt should render above footer: {rows:?}"
        );
        assert!(
            footer_y < main_y,
            "footer should render above agent switcher: {rows:?}"
        );
        assert!(
            main_y < verification_y,
            "agent rows should retain order: {rows:?}"
        );
        assert!(
            rows[verification_y].contains(" · ↑ 30,600 tokens"),
            "agent row should show live elapsed time and tokens: {rows:?}"
        );
        assert!(
            rows[verification_y].trim_end().ends_with("↑ 30,600 tokens"),
            "agent metrics should be right-aligned: {rows:?}"
        );
    }

    #[test]
    fn inline_agent_switcher_renders_below_footer_with_live_content() {
        let mut app = app_with_agent_switcher_rows();
        app.rebon_tui
            .overlay
            .append_streaming_text("streaming assistant output");

        let rows = render_full_inline_frame_rows(app, 12, 0);
        let streaming_y = row_y_containing(&rows, "streaming assistant output");
        let footer_y = row_y_containing(&rows, " cwd");
        let main_y = row_y_containing(&rows, "@Main");

        assert!(
            streaming_y < footer_y,
            "live preview should stay above footer: {rows:?}"
        );
        assert!(
            footer_y < main_y,
            "agent rows should stay below footer: {rows:?}"
        );
    }

    #[test]
    fn inline_at_picker_expands_below_prompt_without_covering_live_preview() {
        let mut app = at_mention_picker_app();
        app.rebon_tui
            .overlay
            .append_streaming_text("streaming assistant output");

        let rows = render_full_inline_frame_rows(app, 12, 0);
        let streaming_y = row_y_containing(&rows, "streaming assistant output");
        let prompt_y = row_y_containing(&rows, "@ mentions");
        // Mention text is normalized to `/` on every platform so selections are
        // portable when copied between the CLI, desktop app, and mobile client.
        let picker_y = row_y_containing(&rows, "@src/");

        assert!(
            streaming_y < prompt_y,
            "live preview should remain above prompt/picker: {rows:?}"
        );
        assert!(
            prompt_y < picker_y,
            "@ picker should expand below prompt in inline mode: {rows:?}"
        );
    }

    #[test]
    fn inline_layout_keeps_footer_without_live_bottom_content() {
        let area = Rect::new(0, 0, 80, 10);
        let layout = inline_layout(area, 3, 0, true);

        assert_eq!(layout.footer.height, 1);
        assert_eq!(layout.footer.y, layout.prompt.y + layout.prompt.height);
        assert_eq!(layout.transcript.height, 0);
    }

    #[test]
    fn inline_layout_hides_footer_for_live_bottom_content() {
        let area = Rect::new(0, 0, 80, 10);
        let layout = inline_layout(area, 3, 4, false);

        assert_eq!(layout.footer.height, 0);
        assert_eq!(layout.footer.y, layout.prompt.y + layout.prompt.height);
        assert_eq!(layout.transcript.height, 4);
        assert_eq!(
            layout.transcript.height + layout.prompt.height + layout.footer.height,
            7
        );
    }

    #[test]
    fn inline_layout_places_prompt_immediately_after_initial_banner() {
        let area = Rect::new(0, 0, 80, 12);
        let layout = inline_layout(area, 1, 1, true);

        assert_eq!(layout.transcript.y, area.y);
        assert_eq!(layout.transcript.height, 1);
        assert_eq!(layout.prompt.y, area.y + 1);
        assert_eq!(layout.prompt.height, 1);
        assert_eq!(layout.footer.y, area.y + 2);
    }

    #[test]
    fn inline_layout_keeps_small_transcript_compact() {
        let area = Rect::new(0, 3, 80, 20);
        let layout = inline_layout(area, 3, 4, true);

        assert_eq!(layout.transcript.y, area.y);
        assert_eq!(layout.transcript.height, 4);
        assert_eq!(layout.prompt.y, area.y + 4);
        assert_eq!(layout.footer.y, area.y + 7);
        assert_eq!(layout.footer.y + layout.footer.height, area.y + 8);
    }

    #[test]
    fn inline_layout_bottom_anchors_when_content_exceeds_available_height() {
        let area = Rect::new(0, 0, 80, 12);
        let layout = inline_layout(area, 3, 40, true);

        assert_eq!(layout.footer.y + layout.footer.height, area.y + area.height);
        assert_eq!(layout.prompt.y + layout.prompt.height, layout.footer.y);
        assert_eq!(layout.transcript.y, area.y);
        assert_eq!(layout.transcript.height, 8);
        assert_eq!(
            layout.transcript.height + layout.prompt.height + layout.footer.height,
            area.height
        );
    }

    #[test]
    fn inline_layout_prompt_growth_expands_compact_block_until_bottom() {
        let area = Rect::new(0, 0, 80, 10);
        let small_prompt = inline_layout(area, 1, 3, true);
        let taller_prompt = inline_layout(area, 4, 3, true);
        let bottomed_prompt = inline_layout(area, 8, 3, true);

        assert_eq!(small_prompt.prompt.y, 3);
        assert_eq!(small_prompt.footer.y, 4);
        assert_eq!(taller_prompt.prompt.y, 3);
        assert_eq!(taller_prompt.footer.y, 7);
        assert_eq!(bottomed_prompt.prompt.y, 1);
        assert_eq!(bottomed_prompt.prompt.height, 8);
        assert_eq!(bottomed_prompt.footer.y, 9);
        assert_eq!(bottomed_prompt.footer.y + bottomed_prompt.footer.height, 10);
        assert_eq!(bottomed_prompt.transcript.height, 1);
    }

    #[test]
    fn inline_layout_shrinks_transcript_before_moving_prompt_off_bottom() {
        let area = Rect::new(0, 5, 80, 4);
        let layout = inline_layout(area, 3, 10, true);
        assert_eq!(layout.footer.y, 8);
        assert_eq!(layout.prompt.y, 5);
        assert_eq!(layout.prompt.height, 3);
        assert_eq!(layout.transcript.height, 0);
    }

    #[test]
    fn inline_render_owns_top_aligned_live_height_after_commits() {
        // Isolate the ambient task store: `collect_task_views()` reads the
        // global `REBON_TASK_LIST_ID`/`REBON_CONFIG_DIR`, so without this guard a
        // concurrent task-rendering test can leak its task list into this frame
        // and push the footer below the top rows this test asserts on.
        let _env = InlineTaskEnv::new("inline-owns-top-aligned");
        let mut app = AppState::default();
        app.rebon_tui.transcript =
            TranscriptStore::from_rows(vec![assistant_text("a1", "already committed")]);

        let buffer = render_owned_inline_frame_buffer(app, 12, 1);
        let rows = all_rows(&buffer);

        assert!(
            rows[..4].iter().any(|row| !row.trim().is_empty()),
            "compact live UI should render near the top of the viewport: {rows:?}"
        );
        assert!(
            rows[..4].iter().any(|row| row.contains("cwd")),
            "footer should render near the top of the viewport: {rows:?}"
        );
        assert!(
            rows[4..].iter().all(|row| row.trim().is_empty()),
            "viewport tail should be cleared below the compact live UI: {rows:?}"
        );
    }

    /// Natural flow: a short live tail (one the viewport never fills) flows from
    /// the top — the composer hugs it from below and never snaps to the bottom —
    /// with the reservoir staying blank below the footer.
    #[test]
    fn inline_flow_keeps_short_live_tail_top_aligned() {
        let _env = InlineTaskEnv::new("inline-flow-top-aligned");
        let mut app = AppState::default();
        app.rebon_tui
            .overlay
            .append_streaming_text("streaming assistant output");

        let rows = render_inline_rows_borrowed(&mut app, 12, 0, true);

        assert_eq!(
            row_y_containing(&rows, "streaming assistant output"),
            0,
            "a short live tail flows from the top instead of bottom-anchoring: {rows:?}"
        );
        let footer_y = row_y_containing(&rows, "cwd");
        assert!(
            rows[(footer_y + 1)..]
                .iter()
                .all(|row| row.trim().is_empty()),
            "the bottom reservoir below the footer stays blank while flowing: {rows:?}"
        );
    }

    /// Natural flow (the fix for "顶部收缩却 stick 在底部"): when the live tail
    /// shrinks, the composer must ride UP with it instead of staying pinned to
    /// the viewport bottom with a blank band above the prompt. The footer row
    /// strictly decreases as the tail goes from filling the viewport to a single
    /// short row.
    #[test]
    fn inline_flow_footer_rides_up_as_live_tail_shrinks() {
        let _env = InlineTaskEnv::new("inline-flow-rides-up");
        let mut app = AppState::default();

        fn footer_y_for(app: &mut AppState, text: &str) -> usize {
            app.rebon_tui.overlay.clear();
            app.rebon_tui.overlay.append_streaming_text(text);
            let rows = render_inline_rows_borrowed(app, 12, 0, false);
            row_y_containing(&rows, "cwd")
        }

        // A tall tail that fills the viewport puts the footer near the bottom.
        let filled = footer_y_for(&mut app, "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8");
        // Shrinking to a single short row must move the footer UP (natural flow).
        let short = footer_y_for(&mut app, "done");

        assert!(
            short < filled,
            "the composer must ride up as the live tail shrinks (natural flow), \
             got filled footer y={filled}, short footer y={short}"
        );
    }

    #[test]
    fn inline_render_expands_owned_area_for_live_preview() {
        let mut app = AppState::default();
        app.rebon_tui
            .overlay
            .append_streaming_text("streaming assistant output");
        app.queued_commands
            .push(rebon_tui::promptinput::QueuedCommand {
                mode: "prompt".into(),
                value: rebon_tui::promptinput::QueuedCommandValue::Text(
                    "queued while streaming".into(),
                ),
            });

        let buffer = render_owned_inline_frame_buffer(app, 12, 0);
        let rows = all_rows(&buffer);
        let streaming_y = row_y_containing(&rows, "streaming assistant output");
        let queue_y = row_y_containing(&rows, "1 queued message");

        assert!(
            streaming_y < queue_y,
            "live preview should expand the owned area above prompt/queue/footer: {rows:?}"
        );
        assert!(
            streaming_y < 8,
            "expanded live content should be allowed to use more than the compact bottom rows: {rows:?}"
        );
    }

    #[test]
    fn inline_render_uses_fresh_cache_for_sliced_transcript() {
        let source = include_str!("inline.rs");
        let inline_start = source
            .find("pub(in crate::tui::runner) fn render_inline_frame")
            .expect("render_inline_frame present");
        let layout_start = source
            .find("pub(in crate::tui::runner) fn inline_layout")
            .expect("inline_layout present");
        let inline_body = &source[inline_start..layout_start];

        // The sliced-transcript cache now lives in the tail memo: built
        // fresh per key (per committed-row window) inside
        // `ensure_inline_tail_measure` and reused only while that key
        // matches — never AppState's long-lived cache, whose revisions
        // collide across row windows.
        assert!(
            source.contains("let mut cache = rebon_tui::TranscriptMeasureCache::new();"),
            "the tail memo must build a fresh cache per row window"
        );
        assert!(
            inline_body.contains("probe_inline_tail("),
            "inline rendering must measure through the memoized tail probe"
        );
        assert!(
            !source.contains("&mut app.transcript_measure_cache"),
            "inline sliced transcript must not reuse AppState's long-lived measurement cache"
        );
    }
}

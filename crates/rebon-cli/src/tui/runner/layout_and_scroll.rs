//! Transcript layout, scroll math, and mouse routing. Owns the
//! pure geometric helpers (`viewport_height`, `transcript_area`,
//! `prompt_input_area`, `wrap_visual_rows`, `visual_lines_with_cursor`)
//! used to lay out the runner's three panels (transcript / queue
//! banner / prompt input), the scroll-offset bookkeeping
//! (`scroll_up_lines`, `scroll_down_lines`, `repin_transcript_to_bottom`,
//! `should_reroute_prompt_down_to_scroll`), and the
//! mouse-event/scroll dispatchers (`handle_mouse_event`,
//! `handle_scroll_action`).

use std::time::SystemTime;

use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use rebon_tui::layout::unseen_divider::{MessageKind, MessageLite, ScrollSnapshot};
use rebon_tui::promptinput::prompt_surface::TextRange;
use rebon_tui::promptinput::{
    build_queue_display, resolve_task_list_layout, QueueDisplayInput, QueueDisplayItem,
    TaskListLayoutInput, TaskListStatus,
};

use crate::tui::app::{AppState, SelectionOwner};
use crate::tui::event::{translate_key, KeyAction};

use super::footer_navigation::derive_footer_items;
use super::render::collect_task_views;

const MOUSE_WHEEL_SCROLL_LINES: usize = 3;

pub(super) fn scroll_up_lines(app: &mut AppState, lines: usize) {
    app.follow_transcript_tail = false;
    app.scroll_offset = app.scroll_offset.saturating_sub(lines);
}

pub(super) fn repin_transcript_to_bottom(app: &mut AppState) {
    app.follow_transcript_tail = true;
    app.scroll_offset = app
        .prev_frame_area
        .map(|area| {
            app.total_content_lines
                .saturating_sub(viewport_height(area))
        })
        .unwrap_or(app.total_content_lines);
    app.unseen_divider.on_repin();
}

pub(super) fn reset_transcript_page_state(app: &mut AppState) {
    app.follow_transcript_tail = true;
    app.scroll_offset = 0;
    app.prev_scroll_offset = 0;
    app.total_content_lines = 0;
    app.prev_frame_lines.clear();
    app.prev_frame_area = None;
    app.transcript_sticky_anchor = None;
    app.transcript_sticky_anchor_label = None;
    app.transcript_sticky_anchor_area = None;
    app.scroll_to_bottom_area = None;
    app.last_prompt_input_area = None;
    app.selection.clear();
    app.unseen_divider.on_repin();
}

pub(super) fn scroll_down_lines(app: &mut AppState, lines: usize, viewport_height: usize) {
    let max_offset = app.total_content_lines.saturating_sub(viewport_height);
    app.scroll_offset = app.scroll_offset.saturating_add(lines).min(max_offset);
    if app.scroll_offset >= max_offset {
        app.follow_transcript_tail = true;
        app.unseen_divider.on_repin();
    }
}

pub(super) fn viewport_height(transcript_area: Rect) -> usize {
    // The transcript area is borderless — full height is usable.
    transcript_area.height.max(1) as usize
}

pub(in crate::tui::runner) fn should_show_scroll_to_bottom(app: &AppState) -> bool {
    if app.ui_mode != crate::ui_config::UiMode::Screen || app.follow_transcript_tail {
        return false;
    }
    let viewport_height = app.prev_frame_area.map(viewport_height).unwrap_or(0);
    viewport_height > 0
        && app.scroll_offset < app.total_content_lines.saturating_sub(viewport_height)
}

pub(in crate::tui::runner) fn scroll_to_bottom_height(app: &AppState) -> u16 {
    u16::from(should_show_scroll_to_bottom(app))
}

/// Build a `ScrollSnapshot` from the current TUI scroll state.
/// `pending_delta` is always 0 — the TUI does synchronous scroll
/// mutations, applied immediately rather than batched.
pub(super) fn build_scroll_snapshot(app: &AppState, viewport_height: usize) -> ScrollSnapshot {
    ScrollSnapshot {
        scroll_top: app.scroll_offset as i64,
        pending_delta: 0,
        viewport_height: viewport_height as u64,
        scroll_height: app.total_content_lines as u64,
    }
}

/// Convert a `rebon_tui::Message` to the minimal `MessageLite` shape
/// that the unseen-divider turn counter needs.
pub(super) fn to_message_lite(msg: &rebon_tui::Message) -> MessageLite {
    match msg {
        rebon_tui::Message::User(u) => MessageLite::user(&u.uuid),
        rebon_tui::Message::Assistant(a) => {
            let has_visible_text = a.message.content.iter().any(|block| {
                matches!(
                    block,
                    rebon_tui::AssistantContentBlock::Text(t) if !t.text.trim().is_empty()
                )
            });
            if has_visible_text {
                MessageLite::assistant_text(&a.uuid)
            } else {
                MessageLite::assistant_tool_only(&a.uuid)
            }
        }
        rebon_tui::Message::Attachment(a) => MessageLite::attachment(&a.uuid),
        rebon_tui::Message::System(s) => MessageLite {
            uuid: s.uuid.clone(),
            kind: MessageKind::System,
            assistant_has_visible_text: false,
            is_null_rendering_attachment: false,
        },
        rebon_tui::Message::Unknown => MessageLite {
            uuid: String::new(),
            kind: MessageKind::System,
            assistant_has_visible_text: false,
            is_null_rendering_attachment: false,
        },
    }
}

pub(super) fn sync_follow_tail_after_updates(_app: &mut AppState) {
    // Follow-tail sync is handled during render_transcript_area when
    // we know the actual viewport dimensions and total content lines.
    // Nothing to do here.
}

pub(super) fn should_reroute_prompt_down_to_scroll(app: &AppState, transcript_area: Rect) -> bool {
    app.input.is_empty()
        && app.slash_picker.is_none()
        && app.at_mention_picker.is_none()
        && derive_footer_items(app).is_empty()
        && app.total_content_lines > viewport_height(transcript_area)
}

pub(super) fn prompt_input_should_repin_transcript(app: &AppState) -> bool {
    !app.follow_transcript_tail
}

pub(super) fn apply_prompt_input_repin(app: &mut AppState) {
    if prompt_input_should_repin_transcript(app) {
        repin_transcript_to_bottom(app);
        app.last_transcript_down_press_ms = 0;
    }
}

pub(super) fn should_non_empty_prompt_down_press_repin_transcript(
    app: &mut AppState,
    is_press: bool,
    now_ms: u64,
) -> bool {
    if !is_press || app.input.is_empty() {
        return false;
    }
    should_double_down_repin_transcript(app, now_ms)
}

pub(super) fn should_double_down_repin_transcript(app: &mut AppState, now_ms: u64) -> bool {
    if app.follow_transcript_tail {
        app.last_transcript_down_press_ms = 0;
        return false;
    }

    let is_double_down = app.last_transcript_down_press_ms != 0
        && now_ms.saturating_sub(app.last_transcript_down_press_ms) <= 400;
    app.last_transcript_down_press_ms = now_ms;
    if is_double_down {
        app.last_transcript_down_press_ms = 0;
        return true;
    }
    false
}

/// If `offset` is strictly inside a chip, snap to chip.start (left bias),
/// so leftward cursor movement never lands inside a chip.
pub(super) fn snap_cursor_left(chips: &[TextRange], offset: usize) -> usize {
    for chip in chips {
        if offset > chip.start && offset <= chip.end {
            return chip.start;
        }
    }
    offset
}

/// If `offset` is strictly inside a chip, snap to chip.end (right bias).
/// When `offset == chip.start`, cursor rests at the start so the chip can
/// render as "selected" (inverse highlight via `build_prompt_highlights`).
/// One more → press moves into the chip and snaps to chip.end.
pub(super) fn snap_cursor_right(chips: &[TextRange], offset: usize) -> usize {
    for chip in chips {
        if offset > chip.start && offset < chip.end {
            return chip.end;
        }
    }
    offset
}

// ── Mouse event handler ─────────────────────────────────────────
// Routes mouse events to scrolling or in-app text selection.
// Selection coordinates are in absolute screen-space (matching
// crossterm's MouseEvent row/column).

pub(super) fn handle_mouse_event(
    app: &mut AppState,
    mouse: MouseEvent,
    transcript_area: Rect,
    prompt_area: Rect,
) {
    let is_screen_mode = app.ui_mode == crate::ui_config::UiMode::Screen;
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            let vh = viewport_height(transcript_area);
            scroll_up_lines(app, MOUSE_WHEEL_SCROLL_LINES);
            app.unseen_divider
                .on_scroll_away(build_scroll_snapshot(app, vh));
        }
        MouseEventKind::ScrollDown => {
            let vh = viewport_height(transcript_area);
            scroll_down_lines(app, MOUSE_WHEEL_SCROLL_LINES, vh);
            app.unseen_divider
                .on_scroll_away(build_scroll_snapshot(app, vh));
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if is_screen_mode && point_in_rect(mouse.column, mouse.row, app.scroll_to_bottom_area) {
                repin_transcript_to_bottom(app);
                app.selection.clear();
                app.selection_owner = SelectionOwner::Transcript;
                return;
            }
            if is_screen_mode
                && point_in_rect(mouse.column, mouse.row, app.transcript_sticky_anchor_area)
            {
                if let Some(anchor) = app.transcript_sticky_anchor {
                    let vh = viewport_height(transcript_area);
                    let max_offset = app.total_content_lines.saturating_sub(vh);
                    app.follow_transcript_tail = false;
                    app.scroll_offset = anchor.scroll_offset.min(max_offset);
                    app.last_transcript_down_press_ms = 0;
                    app.selection.clear();
                    app.selection_owner = SelectionOwner::Transcript;
                    return;
                }
            }
            let in_prompt = prompt_area.width > 0
                && prompt_area.height > 0
                && mouse.column >= prompt_area.x
                && mouse.column < prompt_area.x.saturating_add(prompt_area.width)
                && mouse.row >= prompt_area.y
                && mouse.row < prompt_area.y.saturating_add(prompt_area.height);
            let in_transcript = transcript_area.width > 0
                && transcript_area.height > 0
                && mouse.column >= transcript_area.x
                && mouse.column < transcript_area.x.saturating_add(transcript_area.width)
                && mouse.row >= transcript_area.y
                && mouse.row < transcript_area.y.saturating_add(transcript_area.height);
            if in_prompt {
                app.selection.clear();
                app.selection_owner = SelectionOwner::PromptInput;
                app.selection.start(mouse.column, mouse.row);
            } else if in_transcript {
                app.selection.clear();
                app.selection_owner = SelectionOwner::Transcript;
                app.selection.start(mouse.column, mouse.row);
            } else {
                app.selection.clear();
                app.selection_owner = SelectionOwner::Transcript;
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            app.selection.update(mouse.column, mouse.row);
        }
        MouseEventKind::Up(MouseButton::Left) => {
            app.selection.finish();
            if app.selection.has_selection() {
                app.pending_copy = true;
            }
        }
        MouseEventKind::Down(MouseButton::Right | MouseButton::Middle) => {
            app.selection.clear();
            app.selection_owner = SelectionOwner::Transcript;
        }
        _ => {}
    }
}

fn point_in_rect(column: u16, row: u16, area: Option<Rect>) -> bool {
    let Some(area) = area else {
        return false;
    };
    area.width > 0
        && area.height > 0
        && column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

/// Map a key to a transcript scroll action while a permission view is
/// pending. The permission modal swallows every key, so only a small
/// whitelist is diverted to scrolling: PageUp/PageDown to page, Ctrl+Home /
/// Ctrl+End to jump to the start/end, and vim-style `j`/`k` for single-line
/// scroll. `j`/`k` are suppressed while the modal's extra-text field is
/// focused so they can still be typed there. Everything else returns `None`
/// and falls through to the modal handler. This is what makes a long
/// permission suffix (e.g. an `ExitPlanMode` plan taller than the inline
/// viewport, which cannot flow to native scrollback) readable in place.
pub(super) fn permission_suffix_scroll_action(key: &KeyEvent, app: &AppState) -> Option<KeyAction> {
    let extra_text_focused = app.pending_permission_view.as_ref().is_some_and(|view| {
        view.extra_text_focused
            || matches!(
                &view.kind,
                crate::tui::permission_modal::PermissionKind::WorkflowReview(review)
                    if review.mode == crate::tui::permission_modal::WorkflowReviewMode::Edit
            )
            || matches!(
                &view.kind,
                crate::tui::permission_modal::PermissionKind::AskUserQuestion {
                    questions,
                    answers,
                    active_question,
                    confirmation_active,
                    ..
                } if !*confirmation_active
                    && questions
                        .get(*active_question)
                        .zip(answers.get(*active_question))
                        .is_some_and(|(question, answer)| {
                            answer.highlighted_row == question.options.len()
                        })
            )
    });
    if !extra_text_focused && key.modifiers.is_empty() {
        match key.code {
            KeyCode::Char('j') => return Some(KeyAction::ScrollDown),
            KeyCode::Char('k') => return Some(KeyAction::ScrollUp),
            _ => {}
        }
    }
    match translate_key(key, app) {
        action @ (KeyAction::PageUp
        | KeyAction::PageDown
        | KeyAction::ScrollHome
        | KeyAction::ScrollEnd) => Some(action),
        _ => None,
    }
}

pub(super) fn handle_scroll_action(app: &mut AppState, action: KeyAction, transcript_area: Rect) {
    let vh = viewport_height(transcript_area);

    match action {
        KeyAction::ScrollUp => scroll_up_lines(app, 1),
        KeyAction::ScrollDown => scroll_down_lines(app, 1, vh),
        KeyAction::PageUp => scroll_up_lines(app, vh),
        KeyAction::PageDown => scroll_down_lines(app, vh, vh),
        KeyAction::ScrollHome => {
            app.follow_transcript_tail = false;
            app.scroll_offset = 0;
        }
        KeyAction::ScrollEnd => {
            app.follow_transcript_tail = true;
            app.scroll_offset = app.total_content_lines.saturating_sub(vh);
            app.unseen_divider.on_repin();
        }
        _ => {}
    }

    // Selection scroll compensation is handled in render_transcript_area
    // where prev_scroll_offset vs scroll_offset delta is detected. Doing
    // it there (not here) avoids double-shifting and ensures the capture
    // of scrolled-off rows has access to the prev-frame text snapshot.

    // After every scroll action, notify the divider state machine.
    // The guard inside on_scroll_away ensures it only snapshots once
    // (on the first break from sticky-bottom).
    let snap = build_scroll_snapshot(app, vh);
    app.unseen_divider.on_scroll_away(snap);
}

/// Simulate ratatui `Wrap { trim: false }` character-level line
/// breaking to count visual rows for one logical line. Accounts for
/// gaps left when a wide character (CJK, emoji) doesn't fit at the
/// end of a visual row.
fn wrap_visual_rows(line: &str, area_width: usize) -> usize {
    if line.is_empty() {
        return 1;
    }
    let mut row: usize = 1;
    let mut col: usize = 0;
    for ch in line.chars() {
        let w = rebon_width::char_width(ch).unwrap_or(0);
        if w > 0 && col + w > area_width {
            row += 1;
            col = 0;
        }
        col += w;
    }
    row
}

/// Count total visual rows for multi-line text, including an extra
/// row when the cursor sits at the exact wrap boundary of a line
/// (the cursor wraps to a new empty row that has no content).
pub(super) fn visual_lines_with_cursor(text: &str, cursor_byte: usize, area_width: usize) -> usize {
    if text.is_empty() {
        return 1;
    }
    let cursor_byte = cursor_byte.min(text.len());
    let aw = area_width.max(1);
    let mut total: usize = 0;
    let mut cursor_vrow: usize = 0;
    let mut cursor_found = false;
    let mut line_start: usize = 0;
    for line in text.split('\n') {
        let line_end = line_start + line.len();
        let vrows = wrap_visual_rows(line, aw);
        if !cursor_found && cursor_byte >= line_start && cursor_byte <= line_end {
            cursor_found = true;
            let cb = cursor_byte - line_start;
            let mut row: usize = 0;
            let mut col: usize = 0;
            for (bi, ch) in line.char_indices() {
                if bi >= cb {
                    break;
                }
                let w = rebon_width::char_width(ch).unwrap_or(0);
                if w > 0 && col + w > aw {
                    row += 1;
                    col = 0;
                }
                col += w;
            }
            // Cursor at end of line with row fully filled → wraps.
            if cb >= line.len() && col > 0 && col >= aw {
                row += 1;
            }
            cursor_vrow = total + row;
        }
        total += vrows;
        line_start = line_end + 1;
    }
    total.max(cursor_vrow + 1)
}

pub(super) fn mouse_transcript_area(app: &mut AppState, size: ratatui::layout::Size) -> Rect {
    if let Some(area) = app.prev_frame_area {
        area
    } else {
        transcript_area(app, size)
    }
}

pub(super) fn transcript_area(app: &mut AppState, size: ratatui::layout::Size) -> Rect {
    let frame_area = Rect::new(0, 0, size.width, size.height);
    // Estimate task list height for the scroll-area calculation.
    // collect_task_views is stateless — the completion-tracking
    // side effect lives in update_task_completion_timestamps which
    // is called from render_frame, not from this size-probe path.
    let task_h = {
        let views = collect_task_views();
        if views.is_empty() {
            0u16
        } else if app.task_list_collapsed {
            // Collapsed: top margin (1) + summary header (1) + in-progress tasks
            let in_progress_count = views
                .iter()
                .filter(|t| t.status == TaskListStatus::InProgress)
                .count() as u16;
            2 + in_progress_count
        } else {
            let layout = resolve_task_list_layout(&TaskListLayoutInput {
                tasks: &views,
                terminal_rows: size.height,
                terminal_columns: size.width,
                now_ms: SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                completion_timestamps: &[],
            });
            // +1 header, +1 top margin
            1 + 1
                + layout.visible.len() as u16
                + if layout.hidden_summary.text.is_empty() {
                    0
                } else {
                    1
                }
        }
    };

    let queue_banner_h = {
        let queue_items: Vec<QueueDisplayItem> = app
            .queued_commands
            .iter()
            .filter_map(|cmd| match &cmd.value {
                rebon_tui::promptinput::QueuedCommandValue::Text(t) => {
                    Some(QueueDisplayItem { text: t.clone() })
                }
                rebon_tui::promptinput::QueuedCommandValue::NonText => None,
            })
            .collect();
        let queue_layout = build_queue_display(&QueueDisplayInput {
            items: queue_items,
            max_width: size.width.saturating_sub(4) as usize,
        });
        if queue_layout.visible {
            1 + queue_layout.lines.len() as u16
        } else {
            0
        }
    };

    let prompt_inner_width = size.width.saturating_sub(4) as usize;
    let input_has_mode_prefix = app.input.starts_with('!');
    let display_for_height = if input_has_mode_prefix {
        &app.input[1..]
    } else {
        &app.input
    };
    let cursor_byte = if input_has_mode_prefix {
        app.cursor_offset.saturating_sub(1)
    } else {
        app.cursor_offset
    };
    let visual_lines =
        visual_lines_with_cursor(display_for_height, cursor_byte, prompt_inner_width);
    let prompt_h = ((visual_lines as u16) + 2).max(3).min(size.height / 3);

    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(task_h),
            Constraint::Length(queue_banner_h),
            Constraint::Length(scroll_to_bottom_height(app)),
            Constraint::Length(prompt_h),
            Constraint::Length(1),
        ])
        .split(frame_area)[1]
}

pub(super) fn mouse_prompt_area(app: &mut AppState, size: ratatui::layout::Size) -> Rect {
    if app.teams_dialog.is_some() || app.background_tasks_dialog.is_some() {
        Rect::default()
    } else if let Some(area) = app.last_prompt_input_area {
        area
    } else {
        prompt_input_area(app, size)
    }
}

fn prompt_input_area(app: &mut AppState, size: ratatui::layout::Size) -> Rect {
    let frame_area = Rect::new(0, 0, size.width, size.height);
    let task_h = {
        let views = collect_task_views();
        if views.is_empty() {
            0u16
        } else if app.task_list_collapsed {
            let in_progress_count = views
                .iter()
                .filter(|t| t.status == TaskListStatus::InProgress)
                .count() as u16;
            2 + in_progress_count
        } else {
            let layout = resolve_task_list_layout(&TaskListLayoutInput {
                tasks: &views,
                terminal_rows: size.height,
                terminal_columns: size.width,
                now_ms: SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                completion_timestamps: &[],
            });
            1 + 1
                + layout.visible.len() as u16
                + if layout.hidden_summary.text.is_empty() {
                    0
                } else {
                    1
                }
        }
    };

    let queue_banner_h = {
        let queue_items: Vec<QueueDisplayItem> = app
            .queued_commands
            .iter()
            .filter_map(|cmd| match &cmd.value {
                rebon_tui::promptinput::QueuedCommandValue::Text(t) => {
                    Some(QueueDisplayItem { text: t.clone() })
                }
                rebon_tui::promptinput::QueuedCommandValue::NonText => None,
            })
            .collect();
        let queue_layout = build_queue_display(&QueueDisplayInput {
            items: queue_items,
            max_width: size.width.saturating_sub(4) as usize,
        });
        if queue_layout.visible {
            1 + queue_layout.lines.len() as u16
        } else {
            0
        }
    };

    let prompt_inner_width = size.width.saturating_sub(4) as usize;
    let input_has_mode_prefix = app.input.starts_with('!');
    let display_for_height = if input_has_mode_prefix {
        &app.input[1..]
    } else {
        &app.input
    };
    let cursor_byte = if input_has_mode_prefix {
        app.cursor_offset.saturating_sub(1)
    } else {
        app.cursor_offset
    };
    let visual_lines =
        visual_lines_with_cursor(display_for_height, cursor_byte, prompt_inner_width);
    let prompt_h = ((visual_lines as u16) + 2).max(3).min(size.height / 3);

    let prompt_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(task_h),
            Constraint::Length(queue_banner_h),
            Constraint::Length(scroll_to_bottom_height(app)),
            Constraint::Length(prompt_h),
            Constraint::Length(1),
        ])
        .split(frame_area)[5];

    let inner = Rect {
        x: prompt_area.x.saturating_add(1),
        y: prompt_area.y.saturating_add(1),
        width: prompt_area.width.saturating_sub(2),
        height: prompt_area.height.saturating_sub(2),
    };

    let gutter_width = 2;
    Rect {
        x: inner.x.saturating_add(gutter_width),
        y: inner.y,
        width: inner.width.saturating_sub(gutter_width),
        height: inner.height,
    }
}

#[cfg(test)]
mod tests {
    use super::super::footer_navigation::derive_footer_items;
    use super::*;

    use rebon_width::WidthStr;
    use tempfile::TempDir;

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

    fn app_with_visible_footer_task() -> AppState {
        let mut app = AppState::new();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_test_task(
            &reg,
            "s1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = std::sync::Arc::new(reg);
        app
    }

    fn mk_key(code: KeyCode, mods: ratatui::crossterm::event::KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn mk_perm_view(extra_text_focused: bool) -> crate::tui::permission_modal::PermissionModalView {
        use crate::tui::permission_modal::{PermissionKind, PermissionModalView};
        PermissionModalView {
            query_id: 0,
            tool_call_id: String::new(),
            title: String::new(),
            summary: String::new(),
            options: Vec::new(),
            selected: 0,
            extra_text: String::new(),
            extra_text_focused,
            kind: PermissionKind::Generic,
        }
    }

    #[test]
    fn permission_scroll_whitelist_maps_paging_and_jump_keys() {
        use ratatui::crossterm::event::KeyModifiers;
        let app = AppState::new();
        assert!(matches!(
            permission_suffix_scroll_action(&mk_key(KeyCode::PageUp, KeyModifiers::empty()), &app),
            Some(KeyAction::PageUp)
        ));
        assert!(matches!(
            permission_suffix_scroll_action(
                &mk_key(KeyCode::PageDown, KeyModifiers::empty()),
                &app
            ),
            Some(KeyAction::PageDown)
        ));
        assert!(matches!(
            permission_suffix_scroll_action(&mk_key(KeyCode::Home, KeyModifiers::CONTROL), &app),
            Some(KeyAction::ScrollHome)
        ));
        assert!(matches!(
            permission_suffix_scroll_action(&mk_key(KeyCode::End, KeyModifiers::CONTROL), &app),
            Some(KeyAction::ScrollEnd)
        ));
    }

    #[test]
    fn permission_scroll_whitelist_maps_vim_jk() {
        use ratatui::crossterm::event::KeyModifiers;
        let app = AppState::new();
        assert!(matches!(
            permission_suffix_scroll_action(
                &mk_key(KeyCode::Char('j'), KeyModifiers::empty()),
                &app
            ),
            Some(KeyAction::ScrollDown)
        ));
        assert!(matches!(
            permission_suffix_scroll_action(
                &mk_key(KeyCode::Char('k'), KeyModifiers::empty()),
                &app
            ),
            Some(KeyAction::ScrollUp)
        ));
    }

    #[test]
    fn permission_scroll_whitelist_ignores_modal_navigation_keys() {
        use ratatui::crossterm::event::KeyModifiers;
        let app = AppState::new();
        // Arrow keys drive option selection in the modal — never scroll.
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Enter,
            KeyCode::Char('a'),
        ] {
            assert!(
                permission_suffix_scroll_action(&mk_key(code, KeyModifiers::empty()), &app)
                    .is_none(),
                "key {code:?} should fall through to the modal",
            );
        }
    }

    #[test]
    fn permission_scroll_jk_suppressed_when_extra_text_focused() {
        use ratatui::crossterm::event::KeyModifiers;
        let mut app = AppState::new();
        app.pending_permission_view = Some(mk_perm_view(true));
        // j/k must type into the focused extra-text field, not scroll.
        assert!(permission_suffix_scroll_action(
            &mk_key(KeyCode::Char('j'), KeyModifiers::empty()),
            &app
        )
        .is_none());
        assert!(permission_suffix_scroll_action(
            &mk_key(KeyCode::Char('k'), KeyModifiers::empty()),
            &app
        )
        .is_none());
        // Paging keys still scroll even while the field is focused.
        assert!(matches!(
            permission_suffix_scroll_action(&mk_key(KeyCode::PageUp, KeyModifiers::empty()), &app),
            Some(KeyAction::PageUp)
        ));
    }

    #[test]
    fn permission_scroll_jk_suppressed_when_ask_user_other_is_focused() {
        use crate::tui::permission_modal::{
            AskUserQuestionAnswer, AskUserQuestionEntry, AskUserQuestionOption, PermissionKind,
        };
        use ratatui::crossterm::event::KeyModifiers;

        let mut answer = AskUserQuestionAnswer::new();
        answer.highlighted_row = 1;
        let mut view = mk_perm_view(false);
        view.kind = PermissionKind::AskUserQuestion {
            questions: vec![AskUserQuestionEntry {
                question: "Continue?".into(),
                header: "Choice".into(),
                options: vec![AskUserQuestionOption {
                    label: "Yes".into(),
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
        };
        let mut app = AppState::new();
        app.pending_permission_view = Some(view);

        for ch in ['j', 'k'] {
            assert!(permission_suffix_scroll_action(
                &mk_key(KeyCode::Char(ch), KeyModifiers::empty()),
                &app
            )
            .is_none());
        }

        if let PermissionKind::AskUserQuestion { answers, .. } =
            &mut app.pending_permission_view.as_mut().unwrap().kind
        {
            answers[0].highlighted_row = 0;
        }
        assert!(matches!(
            permission_suffix_scroll_action(
                &mk_key(KeyCode::Char('j'), KeyModifiers::empty()),
                &app
            ),
            Some(KeyAction::ScrollDown)
        ));
    }

    #[test]
    fn repin_transcript_to_bottom_uses_last_viewport_when_available() {
        let mut app = AppState::new();
        app.total_content_lines = 50;
        app.prev_frame_area = Some(Rect::new(0, 0, 80, 12));
        app.follow_transcript_tail = false;
        app.scroll_offset = 3;

        repin_transcript_to_bottom(&mut app);

        assert!(app.follow_transcript_tail);
        assert_eq!(app.scroll_offset, 38);
    }

    #[test]
    fn scroll_to_bottom_height_requires_known_overflowing_scrolled_viewport() {
        let mut app = AppState::new();
        app.ui_mode = crate::ui_config::UiMode::Screen;
        app.follow_transcript_tail = false;
        app.total_content_lines = 10;

        assert_eq!(scroll_to_bottom_height(&app), 0);

        app.prev_frame_area = Some(Rect::new(0, 0, 80, 12));
        assert_eq!(scroll_to_bottom_height(&app), 0);

        app.total_content_lines = 20;
        assert_eq!(scroll_to_bottom_height(&app), 1);

        app.scroll_offset = 8;
        assert_eq!(scroll_to_bottom_height(&app), 0);
    }

    #[test]
    fn screen_scroll_to_bottom_click_repins_transcript() {
        let mut app = AppState::new();
        app.ui_mode = crate::ui_config::UiMode::Screen;
        app.total_content_lines = 50;
        app.prev_frame_area = Some(Rect::new(0, 0, 80, 12));
        app.follow_transcript_tail = false;
        app.scroll_offset = 10;
        app.scroll_to_bottom_area = Some(Rect::new(10, 8, 20, 1));

        handle_mouse_event(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 12,
                row: 8,
                modifiers: ratatui::crossterm::event::KeyModifiers::empty(),
            },
            Rect::new(0, 0, 80, 12),
            Rect::default(),
        );

        assert!(app.follow_transcript_tail);
        assert_eq!(app.scroll_offset, 38);
    }

    #[test]
    fn screen_sticky_anchor_click_jumps_to_anchor_offset() {
        let mut app = AppState::new();
        app.ui_mode = crate::ui_config::UiMode::Screen;
        app.total_content_lines = 50;
        app.follow_transcript_tail = false;
        app.scroll_offset = 10;
        app.transcript_sticky_anchor = Some(rebon_tui::TranscriptStickyAnchor {
            row_index: 0,
            scroll_offset: 7,
        });
        app.transcript_sticky_anchor_area = Some(Rect::new(0, 0, 30, 1));

        handle_mouse_event(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 0,
                modifiers: ratatui::crossterm::event::KeyModifiers::empty(),
            },
            Rect::new(0, 2, 80, 12),
            Rect::default(),
        );

        assert!(!app.follow_transcript_tail);
        assert_eq!(app.scroll_offset, 7);
    }

    #[test]
    fn mouse_wheel_scrolls_multiple_lines_per_tick() {
        let mut app = AppState::new();
        app.total_content_lines = 100;
        app.follow_transcript_tail = false;
        app.scroll_offset = 10;
        let area = Rect::new(0, 0, 80, 12);

        handle_mouse_event(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 0,
                modifiers: ratatui::crossterm::event::KeyModifiers::empty(),
            },
            area,
            Rect::default(),
        );
        assert_eq!(app.scroll_offset, 10 + MOUSE_WHEEL_SCROLL_LINES);

        handle_mouse_event(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: ratatui::crossterm::event::KeyModifiers::empty(),
            },
            area,
            Rect::default(),
        );
        assert_eq!(app.scroll_offset, 10);
    }

    #[test]
    fn scroll_down_to_bottom_repins_unseen_divider() {
        let mut app = AppState::new();
        app.total_content_lines = 20;
        app.scroll_offset = 14;
        app.unseen_divider.set_message_count(5);
        app.unseen_divider.on_scroll_away(ScrollSnapshot {
            scroll_top: 0,
            pending_delta: 0,
            viewport_height: 5,
            scroll_height: 20,
        });
        assert_eq!(app.unseen_divider.divider_index(), Some(5));

        scroll_down_lines(&mut app, 1, 5);

        assert!(app.follow_transcript_tail);
        assert_eq!(app.scroll_offset, 15);
        assert_eq!(app.unseen_divider.divider_index(), None);
    }

    #[test]
    fn scroll_actions_update_offset_and_follow_tail() {
        let mut app = AppState::new();
        for idx in 0..5 {
            rebon_tui::reducer(
                &mut app.rebon_tui,
                rebon_tui::Action::Commit(rebon_tui::Message::System(rebon_tui::SystemMessage {
                    uuid: format!("s-{idx}"),
                    timestamp: "t".into(),
                    subtype: "info".into(),
                    content: Some(format!("msg-{idx}")),
                    level: None,
                    is_meta: None,
                })),
            );
        }
        // Simulate having rendered once so total_content_lines is set.
        // Each system message is 1 line, so 5 messages = 5 lines.
        app.total_content_lines = 5;
        // viewport: area height 10 (borderless transcript area).
        let area = Rect::new(0, 0, 80, 10);
        let vh = viewport_height(area);

        // Start following tail.
        app.follow_transcript_tail = true;
        app.scroll_offset = app.total_content_lines.saturating_sub(vh);

        handle_scroll_action(&mut app, KeyAction::ScrollUp, area);
        assert!(!app.follow_transcript_tail);

        handle_scroll_action(&mut app, KeyAction::ScrollEnd, area);
        assert!(app.follow_transcript_tail);
        assert_eq!(
            app.scroll_offset,
            app.total_content_lines.saturating_sub(vh)
        );
    }

    #[test]
    fn prompt_input_repin_jumps_scrolled_transcript_to_bottom() {
        let mut app = AppState::new();
        app.total_content_lines = 50;
        app.prev_frame_area = Some(Rect::new(0, 0, 80, 12));
        app.follow_transcript_tail = false;
        app.scroll_offset = 12;
        app.last_transcript_down_press_ms = 123;

        apply_prompt_input_repin(&mut app);

        assert!(app.follow_transcript_tail);
        assert_eq!(app.scroll_offset, 38);
        assert_eq!(app.last_transcript_down_press_ms, 0);
    }

    #[test]
    fn double_down_repin_requires_two_non_empty_prompt_down_presses() {
        let mut app = AppState::new();
        app.input = "draft".into();
        app.follow_transcript_tail = false;

        assert!(!should_non_empty_prompt_down_press_repin_transcript(
            &mut app, true, 1_000
        ));
        assert_eq!(app.last_transcript_down_press_ms, 1_000);
        assert!(should_non_empty_prompt_down_press_repin_transcript(
            &mut app, true, 1_300
        ));
        assert_eq!(app.last_transcript_down_press_ms, 0);
    }

    #[test]
    fn non_empty_prompt_down_repeat_does_not_trigger_double_down_repin() {
        let mut app = AppState::new();
        app.input = "draft".into();
        app.follow_transcript_tail = false;

        assert!(!should_non_empty_prompt_down_press_repin_transcript(
            &mut app, true, 1_000
        ));
        assert!(!should_non_empty_prompt_down_press_repin_transcript(
            &mut app, false, 1_050
        ));
        assert_eq!(app.last_transcript_down_press_ms, 1_000);
        assert!(!app.follow_transcript_tail);
    }

    #[test]
    fn double_down_repin_ignores_empty_prompt_for_non_empty_path() {
        let mut app = AppState::new();
        app.follow_transcript_tail = false;

        assert!(!should_non_empty_prompt_down_press_repin_transcript(
            &mut app, true, 1_000
        ));
        assert_eq!(app.last_transcript_down_press_ms, 0);
    }

    #[test]
    fn sync_follow_tail_after_updates_is_noop() {
        let mut app = AppState::new();
        app.follow_transcript_tail = true;
        app.scroll_offset = 42;
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::Commit(rebon_tui::Message::System(rebon_tui::SystemMessage {
                uuid: "s-1".into(),
                timestamp: "t".into(),
                subtype: "info".into(),
                content: Some("msg".into()),
                level: None,
                is_meta: None,
            })),
        );
        sync_follow_tail_after_updates(&mut app);
        // sync is now a no-op; actual follow-tail happens in render.
        assert_eq!(app.scroll_offset, 42);
    }

    #[test]
    fn prompt_down_scroll_reroute_is_blocked_by_footer_items() {
        let mut screen_app = app_with_visible_footer_task();
        screen_app.total_content_lines = 20;
        assert!(!derive_footer_items(&screen_app).is_empty());
        assert!(screen_app.input.is_empty());

        let area = Rect::new(0, 0, 80, 5);
        assert!(
            !should_reroute_prompt_down_to_scroll(&screen_app, area),
            "screen-mode callers must leave Down in the prompt/footer path when footer pills are visible"
        );

        let mut inline_app = app_with_visible_footer_task();
        inline_app.total_content_lines = 20;
        assert!(!derive_footer_items(&inline_app).is_empty());
        assert!(
            !should_reroute_prompt_down_to_scroll(&inline_app, area),
            "inline callers must also leave Down in the prompt/footer path when footer pills are visible"
        );
    }

    #[test]
    fn prompt_down_scroll_reroute_is_allowed_without_footer_items() {
        let mut app = AppState::new();
        app.total_content_lines = 20;
        assert!(derive_footer_items(&app).is_empty());
        assert!(app.input.is_empty());

        let area = Rect::new(0, 0, 80, 5);
        assert!(should_reroute_prompt_down_to_scroll(&app, area));
    }

    #[test]
    fn prompt_down_scroll_reroute_requires_scrollable_transcript() {
        let mut app = AppState::new();
        app.total_content_lines = 5;
        assert!(derive_footer_items(&app).is_empty());

        let area = Rect::new(0, 0, 80, 5);
        assert!(!should_reroute_prompt_down_to_scroll(&app, area));
    }

    #[test]
    fn transcript_area_matches_render_frame_layout_with_queue_banner_and_prompt_wrap() {
        let _guard = crate::test_env::lock_env();
        let tmp = TempDir::new().unwrap();
        let previous_config_dir = std::env::var_os("REBON_CONFIG_DIR");
        let previous_task_list_id = std::env::var_os("REBON_TASK_LIST_ID");
        std::env::set_var("REBON_CONFIG_DIR", tmp.path());
        std::env::set_var("REBON_TASK_LIST_ID", "transcript-area-layout-test");

        let mut app = AppState::new();
        app.input = "line one\nline two\nline three".into();
        app.queued_commands = vec![rebon_tui::promptinput::QueuedCommand {
            mode: "prompt".into(),
            value: rebon_tui::promptinput::QueuedCommandValue::Text("queued item".into()),
        }];

        let size = ratatui::layout::Size {
            width: 80,
            height: 24,
        };
        let area = transcript_area(&mut app, size);

        let task_height = {
            let views = collect_task_views();
            if views.is_empty() {
                0u16
            } else if app.task_list_collapsed {
                2 + views
                    .iter()
                    .filter(|t| t.status == TaskListStatus::InProgress)
                    .count() as u16
            } else {
                let layout = resolve_task_list_layout(&TaskListLayoutInput {
                    tasks: &views,
                    terminal_rows: size.height,
                    terminal_columns: size.width,
                    now_ms: SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                    completion_timestamps: &[],
                });
                2 + layout.visible.len() as u16
                    + if layout.hidden_summary.text.is_empty() {
                        0
                    } else {
                        1
                    }
            }
        };
        let prompt_inner_width = size.width.saturating_sub(4) as usize;
        let visual_lines: usize = app
            .input
            .split('\n')
            .map(|l| {
                let w = l.width();
                if w == 0 {
                    1
                } else {
                    (w + prompt_inner_width.max(1) - 1) / prompt_inner_width.max(1)
                }
            })
            .sum();
        let prompt_height = ((visual_lines as u16) + 2).max(3).min(size.height / 3);
        let queue_banner_height = 2;
        let expected_height =
            size.height - 2 - task_height - queue_banner_height - prompt_height - 1;

        assert_eq!(area.y, 2);
        assert_eq!(area.height, expected_height);

        match previous_config_dir {
            Some(value) => std::env::set_var("REBON_CONFIG_DIR", value),
            None => std::env::remove_var("REBON_CONFIG_DIR"),
        }
        match previous_task_list_id {
            Some(value) => std::env::set_var("REBON_TASK_LIST_ID", value),
            None => std::env::remove_var("REBON_TASK_LIST_ID"),
        }
    }

    #[test]
    fn snap_cursor_left_jumps_to_chip_start() {
        let chips = vec![TextRange { start: 4, end: 20 }];
        // Inside chip → snap to start
        assert_eq!(snap_cursor_left(&chips, 10), 4);
        // At chip end → snap to start (left bias)
        assert_eq!(snap_cursor_left(&chips, 20), 4);
        // Before chip → unchanged
        assert_eq!(snap_cursor_left(&chips, 3), 3);
        // After chip → unchanged
        assert_eq!(snap_cursor_left(&chips, 21), 21);
    }

    #[test]
    fn snap_cursor_right_jumps_to_chip_end() {
        let chips = vec![TextRange { start: 4, end: 20 }];
        // Inside chip → snap to end
        assert_eq!(snap_cursor_right(&chips, 10), 20);
        // At chip start → rest at start so the chip renders selected.
        // A second → press lands strictly inside and will then snap to end.
        assert_eq!(snap_cursor_right(&chips, 4), 4);
        assert_eq!(snap_cursor_right(&chips, 5), 20);
        // Before chip → unchanged
        assert_eq!(snap_cursor_right(&chips, 3), 3);
        // After chip → unchanged
        assert_eq!(snap_cursor_right(&chips, 20), 20);
    }

    #[test]
    fn snap_handles_multiple_chips() {
        let chips = vec![
            TextRange { start: 0, end: 16 },
            TextRange { start: 17, end: 27 },
        ];
        assert_eq!(snap_cursor_left(&chips, 5), 0);
        assert_eq!(snap_cursor_right(&chips, 20), 27);
    }

    #[test]
    fn mouse_prompt_area_prefers_last_rendered_prompt_area() {
        let mut app = AppState::new();
        let size = ratatui::layout::Size {
            width: 80,
            height: 24,
        };
        let rendered = ratatui::layout::Rect::new(11, 7, 13, 2);
        app.last_prompt_input_area = Some(rendered);

        assert_eq!(mouse_prompt_area(&mut app, size), rendered);
    }

    #[test]
    fn mouse_transcript_area_prefers_prev_frame_area() {
        let mut app = AppState::new();
        let size = ratatui::layout::Size {
            width: 80,
            height: 24,
        };
        let rendered = ratatui::layout::Rect::new(5, 6, 12, 9);
        app.prev_frame_area = Some(rendered);

        assert_eq!(mouse_transcript_area(&mut app, size), rendered);
    }
}

use super::*;

/// Track task completion transitions so the layout resolver can keep
/// recently-completed tasks visible for `RECENT_COMPLETED_TTL_MS`.
pub(in crate::tui::runner) fn update_task_completion_timestamps(
    app: &mut AppState,
    views: &[ListTask],
) {
    use crate::tui::app::TaskCompletionEntry;
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    for v in views {
        if v.status == TaskListStatus::Completed {
            let was_completed = app
                .prev_task_snapshot
                .iter()
                .any(|(id, st)| id == &v.id && *st == TaskListStatus::Completed);
            if !was_completed && !app.task_completion_timestamps.iter().any(|e| e.id == v.id) {
                app.task_completion_timestamps.push(TaskCompletionEntry {
                    id: v.id.clone(),
                    completed_at_ms: now_ms,
                });
            }
        }
    }
    // Prune stale timestamps (older than 60s).
    app.task_completion_timestamps
        .retain(|e| now_ms.saturating_sub(e.completed_at_ms) < 60_000);
    app.prev_task_snapshot = views.iter().map(|v| (v.id.clone(), v.status)).collect();
}

/// Collect tasks from the active task system, converting them to
/// `ListTask` for the layout resolver.
///
/// Task and TodoWrite are mutually exclusive — only one system is
/// active at a time, controlled by `is_todo_v2_enabled()`.
pub(in crate::tui::runner) fn collect_task_views() -> Vec<ListTask> {
    let mut views = Vec::new();

    if rebon_tool::tasks::is_todo_v2_enabled() {
        // File-based Task system (interactive/TUI default).
        if let Ok(tasks) = rebon_tool::tasks::list_tasks(&rebon_tool::tasks::current_task_list_id())
        {
            for task in tasks {
                if rebon_tool::tasks::is_internal_task(&task) {
                    continue;
                }
                views.push(ListTask::from(&task));
            }
        }
    } else {
        // Legacy TodoWrite (non-interactive/SDK mode).
        let todos = rebon_tool::todo_write::get_todos("session");
        for (i, todo) in todos.iter().enumerate() {
            let status = TaskListStatus::from_str(todo.status.as_str()).unwrap_or_default();
            views.push(ListTask {
                id: format!("todo-{i}"),
                subject: todo.content.clone(),
                status,
                owner: None,
                blocked_by: Vec::new(),
            });
        }
    }

    views
}

pub(in crate::tui::runner) struct TaskListRenderState {
    pub(in crate::tui::runner) views: Vec<ListTask>,
    pub(in crate::tui::runner) completion_timestamps: Vec<CompletionTimestamp>,
    pub(in crate::tui::runner) height: u16,
}

pub(in crate::tui::runner) fn prepare_task_list_for_render(
    app: &mut AppState,
    terminal_rows: u16,
    terminal_columns: u16,
) -> TaskListRenderState {
    let mut views = collect_task_views();
    update_task_completion_timestamps(app, &views);
    apply_task_auto_hide(app, &mut views);

    let completion_timestamps = task_completion_timestamps_for_render(app);
    let current_task_count = views.len();
    if current_task_count > 6 && app.task_list_prev_count <= 6 {
        app.task_list_collapsed = true;
    }
    app.task_list_prev_count = current_task_count;

    let height = task_list_height_for_views(
        &views,
        &completion_timestamps,
        app.task_list_collapsed,
        terminal_rows,
        terminal_columns,
    );

    TaskListRenderState {
        views,
        completion_timestamps,
        height,
    }
}

pub(in crate::tui::runner) fn task_completion_timestamps_for_render(
    app: &AppState,
) -> Vec<CompletionTimestamp> {
    app.task_completion_timestamps
        .iter()
        .map(|e| CompletionTimestamp {
            id: e.id.clone(),
            completed_at_ms: e.completed_at_ms,
        })
        .collect()
}

pub(in crate::tui::runner) fn task_list_height_for_views(
    task_views: &[ListTask],
    completion_timestamps: &[CompletionTimestamp],
    collapsed: bool,
    terminal_rows: u16,
    terminal_columns: u16,
) -> u16 {
    if task_views.is_empty() {
        0
    } else if collapsed {
        let in_progress_count = task_views
            .iter()
            .filter(|t| t.status == TaskListStatus::InProgress)
            .count() as u16;
        2 + in_progress_count
    } else {
        let layout = resolve_task_list_layout(&TaskListLayoutInput {
            tasks: task_views,
            terminal_rows,
            terminal_columns,
            now_ms: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            completion_timestamps,
        });
        2 + layout.visible.len() as u16
            + if layout.hidden_summary.text.is_empty() {
                0
            } else {
                1
            }
    }
}

fn apply_task_auto_hide(app: &mut AppState, task_views: &mut Vec<ListTask>) {
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let has_incomplete = task_views
        .iter()
        .any(|t| t.status != TaskListStatus::Completed);
    if has_incomplete || task_views.is_empty() {
        app.task_hide_deadline_ms = None;
    } else if app.task_hide_deadline_ms.is_none() {
        app.task_hide_deadline_ms = Some(now_ms + 5_000);
    } else if let Some(deadline) = app.task_hide_deadline_ms {
        if now_ms >= deadline {
            let task_list_id = rebon_tool::tasks::current_task_list_id();
            let _ = rebon_tool::tasks::reset_task_list(&task_list_id);
            app.task_hide_deadline_ms = None;
            app.task_completion_timestamps.clear();
            app.prev_task_snapshot.clear();
            task_views.clear();
        }
    }
}

/// Render the task list panel above the prompt input.
/// Standalone task-list layout with summary header, task items with
/// status icons, bold/strikethrough/dim styling, blocked indicators,
/// and hidden summary.
pub(in crate::tui::runner) fn render_task_list(
    frame: &mut Frame,
    area: Rect,
    task_views: &[ListTask],
    completion_timestamps: &[CompletionTimestamp],
    _theme: &RenderTheme,
    collapsed: bool,
    terminal_rows: u16,
) {
    super::frame::clear_chunk_background(frame, area);

    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let layout = resolve_task_list_layout(&TaskListLayoutInput {
        tasks: task_views,
        terminal_rows,
        terminal_columns: area.width,
        now_ms,
        completion_timestamps,
    });

    let ds = rebon_design_system::theme::get_active_theme();
    let dim_style = Style::default().fg(parse_theme_color(ds.inactive));
    let mut y = area.y;

    // ── Top margin (1 blank line) ──────────────────────────────
    y += 1;

    // ── Summary header (matches standalone header) ─────────────
    // "N tasks (X done, Y in progress, Z open)"
    if y < area.y + area.height {
        let header_area = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        let total = layout.counts.total;
        let done = layout.counts.completed;
        let in_prog = layout.counts.in_progress;
        let open = layout.counts.pending;
        let mut parts = format!("  {total} tasks ({done} done, ");
        if in_prog > 0 {
            parts.push_str(&format!("{in_prog} in progress, "));
        }
        parts.push_str(&format!("{open} open)"));
        if collapsed {
            parts.push_str(" [collapsed]");
        }
        let line = Line::from(Span::styled(parts, dim_style));
        frame.render_widget(Paragraph::new(line), header_area);
        y += 1;
    }

    // When collapsed, show in-progress tasks then return.
    if collapsed {
        let max_subject_width = std::cmp::max(15, (area.width as usize).saturating_sub(15));
        for tv in task_views
            .iter()
            .filter(|t| t.status == TaskListStatus::InProgress)
        {
            if y >= area.y + area.height {
                break;
            }
            let row_area = Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            };
            let icon = task_icon(TaskListStatus::InProgress);
            let icon_color = parse_theme_color(ds.rebon);
            let display_subject = truncate_to_ellipsis(&tv.subject, max_subject_width);
            let subject_style = Style::default()
                .fg(parse_theme_color(ds.text))
                .add_modifier(Modifier::BOLD);
            let padded_icon = rebon_width::pad_wide_symbol(icon.icon);
            let spans = vec![
                Span::styled(format!("  {padded_icon} "), Style::default().fg(icon_color)),
                Span::styled(display_subject, subject_style),
            ];
            frame.render_widget(Paragraph::new(Line::from(spans)), row_area);
            y += 1;
        }
        return;
    }

    // ── Task items ─────────────────────────────────────────────
    for item in &layout.visible {
        if y >= area.y + area.height {
            break;
        }
        let row_area = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };

        let status = if item.is_completed {
            TaskListStatus::Completed
        } else if item.is_in_progress {
            TaskListStatus::InProgress
        } else {
            TaskListStatus::Pending
        };
        let icon = task_icon(status);
        let icon_color = match icon.color {
            Some("success") => parse_theme_color(ds.success),
            Some("rebon") => parse_theme_color(ds.rebon),
            _ => parse_theme_color(ds.inactive),
        };

        let task = task_views.iter().find(|t| t.id == item.task_id);
        let subject = task.map(|t| t.subject.as_str()).unwrap_or("");

        // Truncate subject to max_subject_width with an ellipsis.
        let display_subject = truncate_to_ellipsis(subject, item.max_subject_width);

        // Highlight in-progress subjects, strike through completed
        // subjects, and dim completed or blocked subjects.
        let mut subject_style = Style::default().fg(parse_theme_color(ds.text));
        if item.is_in_progress {
            subject_style = subject_style.add_modifier(Modifier::BOLD);
        }
        if item.is_completed {
            subject_style = subject_style
                .add_modifier(Modifier::CROSSED_OUT)
                .add_modifier(Modifier::DIM)
                .fg(parse_theme_color(ds.inactive));
        }
        if item.is_blocked {
            subject_style = subject_style
                .add_modifier(Modifier::DIM)
                .fg(parse_theme_color(ds.inactive));
        }

        let padded_icon = rebon_width::pad_wide_symbol(icon.icon);
        let mut spans = vec![
            Span::styled(format!("  {padded_icon} "), Style::default().fg(icon_color)),
            Span::styled(display_subject, subject_style),
        ];

        // Blocked rows append a sorted blocker summary.
        if item.is_blocked && !item.open_blockers.is_empty() {
            let mut blocker_ids: Vec<&str> =
                item.open_blockers.iter().map(|s| s.as_str()).collect();
            blocker_ids.sort_by(|a, b| match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(an), Ok(bn)) => an.cmp(&bn),
                _ => a.cmp(b),
            });
            let blockers_text = blocker_ids
                .iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", ");
            spans.push(Span::styled(
                format!(" \u{203A} blocked by {blockers_text}"),
                dim_style,
            ));
        }

        frame.render_widget(Paragraph::new(Line::from(spans)), row_area);
        y += 1;
    }

    // ── Hidden summary ─────────────────────────────────────────
    if !layout.hidden_summary.text.is_empty() && y < area.y + area.height {
        let summary_area = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        let line = Line::from(Span::styled(
            format!("  {}", layout.hidden_summary.text),
            dim_style,
        ));
        frame.render_widget(Paragraph::new(line), summary_area);
    }
}

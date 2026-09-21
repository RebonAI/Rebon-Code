use super::*;

/// What both painting paths measure before either of them knows which
/// one runs: the fullscreen layout projection and the four heights the
/// chunk list is built from, plus the task-list model itself.
struct FramePlan {
    zones: LayoutZones,
    prompt_height: u16,
    task_list: TaskListRenderState,
    ultraplan_height: u16,
    queue_layout: QueueDisplayLayout,
    queue_banner_height: u16,
}

/// One frame's worth of painting state: the target buffer, everything
/// read while painting into it, and the plan measured for this frame.
/// The two painting paths take the same eight inputs, so they take them
/// as one value rather than as eight parameters each.
struct FrameStage<'a, 'f, 's> {
    frame: &'a mut Frame<'f>,
    app: &'a mut AppState,
    runtime_state: &'a PromptInputRuntimeState,
    theme: &'a RenderTheme,
    is_loading: bool,
    status: &'a StatusBarInfo<'s>,
    session: Option<&'a TuiEngineSession>,
    cursor_hint: &'a mut Option<(u16, u16)>,
    plan: FramePlan,
}

pub(in crate::tui::runner) fn render_frame(
    frame: &mut Frame,
    app: &mut AppState,
    runtime_state: &PromptInputRuntimeState,
    theme: &RenderTheme,
    is_loading: bool,
    status: &StatusBarInfo<'_>,
    session: Option<&TuiEngineSession>,
    cursor_hint: &mut Option<(u16, u16)>,
) {
    let area = frame.area();
    app.transcript_sticky_anchor_area = None;
    app.scroll_to_bottom_area = None;

    // Top-level reset of the back buffer. Two paths below paint
    // partial coverage of `area` and historically left dirty cells:
    //
    //   * The landing-prompt early-return path (transcript empty,
    //     no overlay/permission/dialogs) renders a centered logo +
    //     prompt + footer and ignores the rest of `area`. When the
    //     previous frame had a populated transcript and the user runs
    //     `/clear` or rewinds back to an empty session, the old
    //     transcript glyphs survive in cells the landing surface
    //     never repaints.
    //   * The dialog overlay path centers a smaller widget and lets
    //     the surrounding rows show whatever the regular layout
    //     painted on its way through. That is intentional, but
    //     resetting the back buffer first guarantees the overlay
    //     never composes onto a frame older than this one.
    //
    // Resetting cells (vs. forcing a theme background) means cells
    // the renderers below do not subsequently paint inherit the
    // terminal's natural background, matching `clear_buffer_area`'s
    // invariant in `rebon-tui::render`.
    clear_rect(frame, area);

    if app.agent_view.is_some() {
        if let Some(dialog) = app.agent_view.as_mut() {
            dialog.render(frame, area, theme, cursor_hint);
        }
        return;
    }
    if app.background_tasks_dialog.is_some() {
        render_background_tasks_dialog(frame, area, app);
        return;
    }

    let plan = plan_frame(app, area);
    let landing = should_render_landing_prompt(
        app,
        is_loading,
        plan.task_list.height,
        plan.queue_banner_height,
    );
    let mut stage = FrameStage {
        frame,
        app,
        runtime_state,
        theme,
        is_loading,
        status,
        session,
        cursor_hint,
        plan,
    };
    if landing {
        stage.render_landing(area);
    } else {
        stage.render_chunked(area);
    }
}

/// Measure the frame: the unseen-message divider, the layout zones, and
/// the prompt / task-list / ultraplan / queue-banner heights. Nothing
/// here paints, and nothing here depends on which path paints.
fn plan_frame(app: &mut AppState, area: Rect) -> FramePlan {
    // ── Unseen divider computation ────────────────────────────────
    // Build the lightweight message list the divider state machine
    // needs, then compute pill visibility and unseen count.
    let messages_lite: Vec<unseen_divider::MessageLite> = app
        .rebon_tui
        .transcript
        .rows()
        .iter()
        .map(super::super::to_message_lite)
        .collect();

    let unseen =
        unseen_divider::compute_unseen_divider(&messages_lite, app.unseen_divider.divider_index());
    let unseen_count = unseen.as_ref().map(|u| u.count).unwrap_or(0);

    // Approximate viewport height for pill visibility check — refined
    // after the layout split, but good enough for the gate.
    let approx_vh = area.height.saturating_sub(6) as usize; // header+prompt+footer
    let snap = super::super::build_scroll_snapshot(app, approx_vh);
    let pill_vis = app.unseen_divider.pill_visible(snap);

    // Drive the layout decision through rebon-tui's fullscreen
    // projection (header / transcript / prompt / footer zones).
    let layout_input = LayoutInput {
        fullscreen_enabled: true,
        terminal_rows: area.height,
        terminal_columns: area.width,
        sticky: StickyPrompt::None,
        hide_sticky: true, // sticky prompt is not wired
        has_scrollable: true,
        has_overlay: app.pending_permission_view.is_some(),
        has_bottom: true,
        has_bottom_float: false,
        has_modal: app.global_search_dialog.is_some()
            || app.mcp_dialog.is_some()
            || app.goal_confirm_dialog.is_some()
            || app.teams_dialog.is_some()
            || app.onboarding_dialog.is_some()
            || app.dialogs.is_open(),
        hide_pill: false,
        pill_visible: pill_vis,
        new_message_count: unseen_count,
        has_suggestions_overlay: false,
        has_dialog_overlay: false,
    };
    let zones = fullscreen_layout::layout_zones(&layout_input);

    let prompt_height = app
        .resume_dialog
        .as_ref()
        .filter(|dialog| dialog.is_prompt_replacement())
        .map(|dialog| {
            dialog
                .desired_height()
                .min(area.height.saturating_sub(3).max(1))
        })
        .unwrap_or_else(|| prompt_height_for_width(app, area.width, area.height));

    let task_list = prepare_task_list_for_render(app, area.height, area.width);

    let ultraplan_height = progress_widget_height(app, area.height / 3);

    // Per-agent live status now renders inline under each Agent tool
    // use in the transcript (see rebon-tui/src/render.rs) plus the
    // background-tasks pill row in the footer. The former bottom
    // coordinator panel was redundant and has been removed.

    // Build queue display layout for the banner above the prompt.
    let queue_layout = queue_display_layout(app, area.width);
    let queue_banner_height = queue_banner_height(&queue_layout);

    FramePlan {
        zones,
        prompt_height,
        task_list,
        ultraplan_height,
        queue_layout,
        queue_banner_height,
    }
}

impl FrameStage<'_, '_, '_> {
    /// The landing surface: a centered logo, the prompt, and the footer
    /// stack. It covers only part of `area` on purpose — the rest is the
    /// reset `render_frame` already painted.
    fn render_landing(&mut self, area: Rect) {
        let agent_switcher_height = agent_switcher_height(self.app);
        let status_line_height = custom_status_line_height(self.app, area.width);
        let reserved_bottom = 1u16
            .saturating_add(status_line_height)
            .saturating_add(agent_switcher_height);
        let main_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: area.height.saturating_sub(reserved_bottom),
        };
        // Bound to a local first: the prompt surface borrows both the
        // frame and the app, and the task list painted inside the block
        // needs them back.
        let prompt_area = render_landing_prompt_surface(
            self.frame,
            main_area,
            self.app,
            self.runtime_state,
            self.theme,
            self.status,
            self.is_loading,
            self.status.elapsed_ms,
            self.cursor_hint,
        );
        if let Some(prompt_area) = prompt_area {
            let task_list_height = self.plan.task_list.height;
            if self.app.input.is_empty() && self.app.mode == "prompt" && task_list_height > 0 {
                let prompt_bottom = prompt_area.y.saturating_add(prompt_area.height);
                let task_y = main_area
                    .y
                    .saturating_add(main_area.height.saturating_sub(task_list_height));
                if task_y > prompt_bottom {
                    let task_area =
                        Rect::new(main_area.x, task_y, main_area.width, task_list_height);
                    self.render_planned_task_list(task_area, area.height);
                }
            }
            render_slash_picker_overlay(self.frame, prompt_area, area, self.app);
            render_at_mention_overlay(self.frame, prompt_area, area, self.app);
        }
        let footer_area = Rect::new(
            area.x,
            area.y
                .saturating_add(area.height)
                .saturating_sub(1)
                .saturating_sub(agent_switcher_height),
            area.width,
            u16::from(area.height > 0),
        );
        let status_line_area = Rect::new(
            area.x,
            footer_area.y.saturating_sub(status_line_height),
            area.width,
            status_line_height,
        );
        let switcher_area = Rect::new(
            area.x,
            footer_area.y.saturating_add(footer_area.height),
            area.width,
            agent_switcher_height,
        );
        self.render_footer_stack(status_line_area, footer_area, switcher_area);
        self.render_overlays(area);
    }

    /// The regular layout: header, transcript, and the stack of
    /// fixed-height chunks below it.
    fn render_chunked(&mut self, area: Rect) {
        let agent_switcher_height = agent_switcher_height(self.app);
        let footer_height: u16 = 1;
        let status_line_height = custom_status_line_height(self.app, area.width);
        let scroll_to_bottom_height =
            super::super::layout_and_scroll::scroll_to_bottom_height(self.app);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(self.plan.ultraplan_height),
                Constraint::Length(self.plan.task_list.height),
                Constraint::Length(self.plan.queue_banner_height),
                Constraint::Length(scroll_to_bottom_height),
                Constraint::Length(self.plan.prompt_height),
                Constraint::Length(status_line_height),
                Constraint::Length(footer_height),
                Constraint::Length(agent_switcher_height),
            ])
            .split(area);

        // Reset every cell each chunk owns before its renderer paints. The
        // sub-region renderers (header, ultraplan, task_list, queue banner,
        // prompt, footer) only paint cells they have content for — they
        // don't clear unfilled rows. When a chunk's height GROWS between
        // frames (task list expanding via Ctrl+O, queue banner appearing,
        // prompt input wrapping to a new line), cells that used to belong
        // to the transcript above are now part of the chunk. If the chunk
        // renderer leaves any of those rows untouched, the previous frame's
        // transcript glyphs survive as visible residue. The transcript
        // chunk is the only sub-region that already clears itself, so we
        // skip chunks[1] here. Doing this once at the layout level keeps
        // the individual renderers free of clear concerns.
        clear_chunk_background(self.frame, chunks[0]);
        for chunk in &chunks[2..] {
            clear_chunk_background(self.frame, *chunk);
        }

        // Advance the animation clock so the in-progress tool-use gutter
        // `●` breathes (dim ↔ bright at 1 Hz) via
        // `rebon_spinner::glyph::reduced_motion_dot_is_dim`. `status.elapsed_ms`
        // ticks while a prompt is active, which is exactly when streaming
        // tool uses are rendered.
        let animated_theme = RenderTheme {
            frame_time_ms: self.status.elapsed_ms,
            ..*self.theme
        };

        render_transcript_area(
            self.frame,
            chunks[1],
            self.app,
            &animated_theme,
            self.cursor_hint,
        );
        render_header(
            self.frame,
            chunks[0],
            self.status,
            self.app.coordinator_mode,
            self.app,
        );
        if self.plan.ultraplan_height > 0 {
            render_progress_widget(self.frame, chunks[2], self.app, self.theme);
        }
        if self.plan.task_list.height > 0 {
            self.render_planned_task_list(chunks[3], area.height);
        }
        if self.plan.queue_layout.visible {
            render_queue_banner(self.frame, chunks[4], &self.plan.queue_layout);
        }
        render_scroll_to_bottom(self.frame, chunks[5], self.app);
        // The background-tasks dialog is a modal replacement for the main
        // interaction surface. It needs more than the prompt box's 3-line
        // minimum so active agents can actually be listed and selected.
        if self.app.teams_dialog.is_some() {
            render_teams_dialog(self.frame, chunks[6], self.app);
        } else if self.app.background_tasks_dialog.is_some() {
            let dialog_y = chunks[1].y;
            let dialog_bottom = chunks[8].y;
            let dialog_area = Rect::new(
                area.x,
                dialog_y,
                area.width,
                dialog_bottom.saturating_sub(dialog_y),
            );
            render_background_tasks_dialog(self.frame, dialog_area, self.app);
        } else if let Some(dialog) = self
            .app
            .resume_dialog
            .as_ref()
            .filter(|dialog| dialog.is_prompt_replacement())
        {
            dialog.render(self.frame, chunks[6]);
            *self.cursor_hint = None;
        } else {
            render_prompt_surface(
                self.frame,
                chunks[6],
                self.app,
                self.runtime_state,
                self.theme,
                self.is_loading,
                self.status.elapsed_ms,
                None,
                self.cursor_hint,
            );
            render_slash_picker_overlay(self.frame, chunks[6], area, self.app);
            render_at_mention_overlay(self.frame, chunks[6], area, self.app);
        }
        self.render_footer_stack(chunks[7], chunks[8], chunks[9]);
        self.render_overlays(area);
    }

    /// Both paths paint the same task-list model; only the rows it lands
    /// on differ.
    fn render_planned_task_list(&mut self, rect: Rect, terminal_height: u16) {
        render_task_list(
            self.frame,
            rect,
            &self.plan.task_list.views,
            &self.plan.task_list.completion_timestamps,
            self.theme,
            self.app.task_list_collapsed,
            terminal_height,
        );
    }

    /// The three rows under the prompt, in both paths: the custom status
    /// line, the footer (or the custom line that replaces it), and the
    /// agent switcher.
    fn render_footer_stack(&mut self, status_line: Rect, footer: Rect, switcher: Rect) {
        render_custom_status_line(self.frame, status_line, self.app);
        if !render_custom_status_line_footer(self.frame, footer, self.app) {
            render_footer(
                self.frame,
                footer,
                self.app,
                self.status,
                !self.is_loading,
                &self.plan.zones,
            );
        }
        render_agent_switcher_rows(self.frame, switcher, self.app, !self.is_loading);
    }

    /// The overlays that compose on top of whichever path painted: the
    /// active dialog, then help.
    fn render_overlays(&mut self, area: Rect) {
        render_active_dialog_overlay(self.frame, area, self.app, self.session);
        if self.app.help_open {
            render_help_overlay(self.frame, area, self.app);
            *self.cursor_hint = None;
        }
    }
}

fn render_scroll_to_bottom(frame: &mut Frame, area: Rect, app: &mut AppState) {
    app.scroll_to_bottom_area = None;
    if area.width == 0
        || area.height == 0
        || !super::super::layout_and_scroll::should_show_scroll_to_bottom(app)
    {
        return;
    }

    let ds = rebon_design_system::theme::get_active_theme();
    let text = format!(
        " scroll to bottom {} ",
        rebon_tui::layout::new_messages_pill::ARROW_DOWN
    );
    let width = text.width().min(area.width as usize) as u16;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let pill_area = Rect {
        x,
        y: area.y,
        width,
        height: 1,
    };
    app.scroll_to_bottom_area = Some(pill_area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default()
                .fg(parse_theme_color(ds.professionalBlue))
                .bg(parse_theme_color(ds.userMessageBackground))
                .add_modifier(Modifier::BOLD),
        ))),
        pill_area,
    );
}

/// Reset every cell in `area` to the default empty cell. Sub-region
/// renderers in this module paint cells lazily — only the rows they
/// actually have content for — so when a chunk grows into rows that
/// the transcript previously owned (Ctrl+O expanding the task list,
/// the queue banner appearing, the prompt input gaining wrapped lines)
/// any rows the renderer skips would otherwise show stale transcript
/// glyphs. Calling this on every chunk before its renderer runs keeps
/// the layout boundary clean without forcing each individual renderer
/// to know about clearing.
pub(in crate::tui::runner) fn clear_chunk_background(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let buf = frame.buffer_mut();
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }
}

fn resume_dialog_rect(area: Rect) -> Rect {
    let base = centered_rect(area, 90, 70);
    let max_height = area.height.saturating_sub(2).max(1);
    let height = base
        .height
        .max(RESUME_DIALOG_MIN_HEIGHT_ROWS)
        .min(max_height);
    let y = base
        .y
        .min(area.y.saturating_add(area.height.saturating_sub(height)));
    Rect { y, height, ..base }
}

fn onboarding_dialog_rect(area: Rect) -> Rect {
    if area.width < 100 || area.height < 32 {
        area
    } else {
        centered_rect(area, 94, 84)
    }
}

pub(in crate::tui::runner) fn render_active_dialog_overlay(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    session: Option<&TuiEngineSession>,
) {
    refresh_settings_projection(app, session);
    // Keep this chain in the order keys are dispatched: the host takes them
    // first, then `maybe_handle_active_dialog_key`'s chain. When two dialogs
    // are open at once, the one that paints has to be the one that receives
    // the keys.
    let overlay = centered_rect(area, 90, 70);
    if crate::tui::dialog_host::render_screen(&mut app.dialogs, frame, area, overlay) {
        // Painted by the host; it also picked the placement.
    } else if let Some(dialog) = app.goal_confirm_dialog.as_ref() {
        dialog.render(frame, area, &RenderTheme::plain());
    } else if let Some(dialog) = app.global_search_dialog.as_ref() {
        dialog.render(frame, overlay);
    } else if let Some(dialog) = app.mcp_dialog.as_ref() {
        dialog.render(frame, overlay);
    } else if let Some(dialog) = app
        .resume_dialog
        .as_ref()
        .filter(|dialog| !dialog.is_prompt_replacement())
    {
        dialog.render(frame, resume_dialog_rect(area));
    } else if let Some(dialog) = app.rewind_dialog.as_ref() {
        dialog.render(frame, overlay);
    } else if let Some(dialog) = app.onboarding_dialog.as_ref() {
        crate::tui::onboarding_dialog::render(dialog, frame, onboarding_dialog_rect(area));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn a_list_dialog_renders_as_centered_screen_modal() {
        let mut app = AppState::default();
        app.dialogs
            .push(rebon_dialog::effort_dialog::EffortDialogState::open(
                "screen-model",
                None,
            ));
        let session = crate::tui::runner::test_support::make_test_tui_session();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| {
                render_active_dialog_overlay(frame, frame.area(), &mut app, Some(&session));
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Select Reasoning Level for screen-model"));
        assert!(rendered.contains("1. Low"));
    }
}

use crate::tui::prompt_tips::PromptTopHintTone;

use super::*;

pub(in crate::tui::runner) fn inline_verbose_prompt_active(app: &AppState) -> bool {
    matches!(
        app.tool_output_verbosity,
        rebon_tui::ToolOutputVerbosity::Verbose
    )
}

pub(in crate::tui::runner) fn render_inline_verbose_prompt(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let ds = rebon_design_system::theme::get_active_theme();
    let style = Style::default().fg(parse_theme_color(ds.inactive));
    Paragraph::new("─".repeat(area.width as usize))
        .style(style)
        .render(Rect::new(area.x, area.y, area.width, 1), frame.buffer_mut());

    if area.height < 2 {
        return;
    }

    const LABEL: &str = "Showing detailed transcript · ctrl+o to toggle · ctrl+e to show all";
    const MODE: &str = "verbose";
    let mode_width = MODE.width() as u16;
    let mode_x = area
        .x
        .saturating_add(area.width.saturating_sub(mode_width).saturating_sub(1));
    let label_x = area.x.saturating_add(2);
    let label_width = mode_x.saturating_sub(label_x).saturating_sub(1);
    let label = truncate_to_ellipsis(LABEL, label_width as usize);

    Paragraph::new(Span::styled(label, style)).render(
        Rect::new(label_x, area.y.saturating_add(1), label_width, 1),
        frame.buffer_mut(),
    );
    Paragraph::new(Span::styled(MODE, style.add_modifier(Modifier::BOLD))).render(
        Rect::new(mode_x, area.y.saturating_add(1), mode_width, 1),
        frame.buffer_mut(),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui::runner) struct PromptModeBadge {
    label: &'static str,
    background: Color,
}

pub(in crate::tui::runner) fn inline_prompt_mode_badge(app: &AppState) -> Option<PromptModeBadge> {
    let ds = rebon_design_system::theme::get_active_theme();
    if app.coordinator_mode {
        Some(PromptModeBadge {
            label: " Coordinator Mode ",
            background: parse_theme_color(ds.chromeYellow),
        })
    } else if app.ultraplan_status.is_some() {
        Some(PromptModeBadge {
            label: " Ultraplan Mode ",
            background: parse_theme_color(ds.planMode),
        })
    } else {
        None
    }
}

pub(in crate::tui::runner) fn render_prompt_surface(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    runtime_state: &rebon_tui::promptinput::PromptInputRuntimeState,
    theme: &RenderTheme,
    is_loading: bool,
    elapsed_ms: u64,
    mode_badge: Option<PromptModeBadge>,
    cursor_hint: &mut Option<(u16, u16)>,
) {
    let input_has_mode_prefix = app.input.starts_with('!');
    let is_bash = app.mode == "bash" || input_has_mode_prefix;
    let is_help = app.help_open;
    let (border_style, title) = prompt_chrome_line(
        is_loading,
        is_bash,
        is_help,
        elapsed_ms,
        &app.spinner_verb,
        app.retry_info,
        app.streaming_token_count,
    );
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(title);
    let mut deferred_hint = None;
    if let Some(hint) = app.idle_prompt_top_hint() {
        let text = format!(" {} ", hint.text);
        let style = prompt_top_hint_style(hint.tone);
        if mode_badge.is_some() {
            deferred_hint = Some((text, style));
        } else {
            let max_hint_width = area.width.saturating_sub(4) as usize;
            let text = truncate_to_ellipsis(&text, max_hint_width);
            if !text.is_empty() {
                block = block.title(Line::from(Span::styled(text, style)).right_aligned());
            }
        }
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if let Some((text, style)) = deferred_hint {
        render_prompt_top_right_text_before_badge(frame, area, &text, style, mode_badge);
    }
    if let Some(badge) = mode_badge {
        render_prompt_mode_badge(frame, area, badge);
    }

    // Mode indicator glyph: `! ` for bash, `? ` for help, `❯ ` for normal.
    // Colors derived from rebon-design-system palette.
    let ds = rebon_design_system::theme::get_active_theme();
    let gutter_width: u16 = 2;
    let (glyph, glyph_style) = if is_bash {
        (
            "! ",
            Style::default()
                .fg(parse_theme_color(ds.bashBorder))
                .add_modifier(Modifier::BOLD),
        )
    } else if is_help {
        (
            "? ",
            Style::default()
                .fg(parse_theme_color(ds.suggestion))
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            "❯ ",
            Style::default()
                .fg(parse_theme_color(ds.suggestion))
                .add_modifier(Modifier::BOLD),
        )
    };
    if inner.width > gutter_width && inner.height > 0 {
        let gutter_area = Rect {
            x: inner.x,
            y: inner.y,
            width: gutter_width,
            height: 1,
        };
        Paragraph::new(Span::styled(glyph.to_string(), glyph_style))
            .render(gutter_area, frame.buffer_mut());
    }

    // Prompt input area starts after the mode indicator.
    let input_area = Rect {
        x: inner.x.saturating_add(gutter_width),
        y: inner.y,
        width: inner.width.saturating_sub(gutter_width),
        height: inner.height,
    };
    app.last_prompt_input_area = Some(input_area);

    // For bash mode, adjust the cursor offset only when the live buffer
    // still carries a legacy `!` prefix. Normal `!` activation stores the
    // mode in `app.mode` and leaves the displayed input prefix-free.
    let cursor_offset = if input_has_mode_prefix {
        app.cursor_offset.saturating_sub(1)
    } else {
        app.cursor_offset
    };

    let result = render_prompt_input(
        runtime_state,
        cursor_offset,
        input_area,
        frame.buffer_mut(),
        theme,
    );

    // ── Argument hint (rebon_tui::input) ─────────────────────────────
    // When the user types a slash command without arguments, show
    // the command's input hint (e.g. "<message>") dimmed after the
    // cursor.
    let current_hint =
        super::super::slash_commands::resolve_argument_hint(&app.input, &app.slash_commands);
    if should_show_argument_hint(&ArgumentHintInput {
        argument_hint: current_hint.as_deref(),
        value: &app.input,
    }) {
        if let Some((cx, cy)) = result.cursor {
            let hint_text = current_hint.unwrap_or_default();
            let leading = if app.input.ends_with(' ') { "" } else { " " };
            let label = format!("{leading}{hint_text}");
            let remaining_w = input_area
                .width
                .saturating_sub(cx - input_area.x)
                .saturating_sub(1);
            if remaining_w > 0 {
                let hint_area = Rect {
                    x: cx,
                    y: cy,
                    width: remaining_w,
                    height: 1,
                };
                Paragraph::new(Span::styled(
                    label,
                    Style::default()
                        .fg(parse_theme_color(ds.inactive))
                        .add_modifier(Modifier::DIM),
                ))
                .render(hint_area, frame.buffer_mut());
            }
        }
    }

    // ── Prompt selection overlay / copy ────────────────────────
    if app.selection_owner == SelectionOwner::PromptInput {
        rebon_tui::apply_selection_overlay(&app.selection, frame.buffer_mut(), input_area);
        if app.pending_copy {
            app.pending_copy = false;
            let text = app
                .selection
                .get_selected_text(frame.buffer_mut(), input_area);
            if !text.is_empty() {
                rebon_tui::selection::copy_to_clipboard_osc52(&text);
            }
        }
    }

    // Deliberately do NOT call `frame.set_cursor_position(pos)` here.
    //
    // ratatui 0.29's `CrosstermBackend` implements `show_cursor` and
    // `set_cursor_position` as `execute!` — each one writes AND flushes
    // stdout. In `Terminal::draw`, the post-render order is
    // `show_cursor()` → `set_cursor_position(...)`, which leaves a brief
    // visible window where the cursor is shown at whatever random cell
    // the last cell-write landed on (cursor from `flush()` of the cell
    // buffer), BEFORE it's moved to the prompt caret. Windows IME
    // candidate windows anchor to the caret whenever it flushes, so
    // that wrong-position flash — repeated every animation tick — causes the
    // IME popup to drift all over the screen ("IME drifts everywhere").
    //
    // Instead we hand the caret position to the caller via
    // `cursor_hint` and let it emit `MoveTo + Show` in that order
    // (move while still hidden, then show at the right spot) after
    // `terminal.draw` returns. When `result.cursor` is `None` we
    // leave `cursor_hint` untouched so ratatui's default
    // "hide cursor at end of draw" kicks in unchanged.
    if app.pending_permission_view.is_none() {
        if let Some(pos) = result.cursor {
            *cursor_hint = Some(pos);
        }
    }
}

/// Informational hints sit quietly in the frame; only a warning earns weight.
pub(in crate::tui::runner) fn prompt_top_hint_style(tone: PromptTopHintTone) -> Style {
    let ds = rebon_design_system::theme::get_active_theme();
    match tone {
        PromptTopHintTone::Info => Style::default().fg(parse_theme_color(ds.inactive)),
        PromptTopHintTone::Warning => Style::default()
            .fg(parse_theme_color(ds.warning))
            .add_modifier(Modifier::BOLD),
    }
}

fn render_prompt_mode_badge(frame: &mut Frame, area: Rect, badge: PromptModeBadge) {
    let width = badge.label.width() as u16;
    if width == 0 || area.width <= width.saturating_add(1) || area.height == 0 {
        return;
    }
    let badge_area = Rect {
        x: area.x + area.width - width - 1,
        y: area.y,
        width,
        height: 1,
    };
    Paragraph::new(Span::styled(
        badge.label,
        Style::default()
            .fg(badge_foreground(badge.background))
            .bg(badge.background)
            .add_modifier(Modifier::BOLD),
    ))
    .render(badge_area, frame.buffer_mut());
}

/// Black or white, whichever reads on `background`. A fixed white label
/// disappears on the light palettes' yellow.
pub(in crate::tui::runner) fn badge_foreground(background: Color) -> Color {
    match background {
        Color::Rgb(r, g, b) => {
            let luma = 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b);
            if luma > 150.0 {
                Color::Black
            } else {
                Color::White
            }
        }
        Color::Yellow | Color::LightYellow | Color::LightGreen | Color::LightCyan => Color::Black,
        _ => Color::White,
    }
}

fn render_prompt_top_right_text_before_badge(
    frame: &mut Frame,
    area: Rect,
    text: &str,
    style: Style,
    mode_badge: Option<PromptModeBadge>,
) {
    let right_reserved = mode_badge
        .map(|badge| badge.label.width() as u16 + 1)
        .unwrap_or(1);
    let available_width = area.width.saturating_sub(4).saturating_sub(right_reserved) as usize;
    let text = truncate_to_ellipsis(text, available_width);
    if text.is_empty() {
        return;
    }
    let width = text.width() as u16;
    if width == 0 || area.width <= width.saturating_add(right_reserved) {
        return;
    }
    let hint_area = Rect {
        x: area.x + area.width - right_reserved - width,
        y: area.y,
        width,
        height: 1,
    };
    Paragraph::new(Span::styled(text, style)).render(hint_area, frame.buffer_mut());
}

/// Returns `(border_style, title)` for the prompt frame based on
/// loading state and prompt mode (`bash`) / help overlay state.
///
/// When loading, a random spinner verb from `spinner_verbs` is shown with an
/// animated spinner glyph from `rebon-spinner`. The title is layered: the
/// glyph and verb carry the brand accent, while elapsed time, retry progress
/// and the token count recede into the secondary tone so the eye lands on
/// what the agent is doing, not on the counters.
pub(in crate::tui::runner) fn prompt_chrome_line(
    is_loading: bool,
    is_bash: bool,
    is_help: bool,
    elapsed_ms: u64,
    spinner_verb: &str,
    retry_info: Option<rebon_api::RetryProgress>,
    token_count: u32,
) -> (Style, Line<'static>) {
    let ds = rebon_design_system::theme::get_active_theme();
    if is_loading {
        let glyph = rebon_spinner::glyph_for_frame(
            rebon_spinner::GlyphPlatform::Other,
            elapsed_ms / 80, // ~12.5 fps cycle
        );
        let accent = Style::default().fg(parse_theme_color(ds.rebon));
        let secondary = Style::default().fg(parse_theme_color(ds.inactive));
        let mut spans = vec![
            Span::raw(" "),
            Span::styled(
                glyph.to_string(),
                Style::default().fg(parse_theme_color(ds.rebonShimmer)),
            ),
            Span::raw(" "),
            Span::styled(
                format!("{spinner_verb}…"),
                accent.add_modifier(Modifier::BOLD),
            ),
        ];
        // Retry progress when the middleware is retrying a failed request
        // (e.g. "Retry 2/10") is the one counter that is news, so it keeps
        // the warning tone.
        if let Some(progress) = retry_info {
            spans.push(Span::styled(
                format!(" ({progress})"),
                Style::default().fg(parse_theme_color(ds.warning)),
            ));
        }
        if elapsed_ms >= 1000 {
            let duration_str = format_duration(
                elapsed_ms,
                DurationFormatOptions {
                    hide_trailing_zeros: true,
                    most_significant_only: true,
                },
            );
            spans.push(Span::styled(format!(" · {duration_str}"), secondary));
        }
        if token_count > 0 {
            spans.push(Span::styled(
                format!(" · {} tokens", fmt_tokens(token_count)),
                secondary,
            ));
        }
        spans.push(Span::raw(" "));
        return (accent, Line::from(spans));
    }
    if is_bash {
        let style = Style::default().fg(parse_theme_color(ds.bashBorder));
        return (
            style,
            Line::from(Span::styled(" bash ", style.add_modifier(Modifier::BOLD))),
        );
    }
    if is_help {
        let style = Style::default().fg(parse_theme_color(ds.suggestion));
        return (
            style,
            Line::from(Span::styled(" help ", style.add_modifier(Modifier::BOLD))),
        );
    }
    (
        Style::default().fg(parse_theme_color(ds.promptBorder)),
        Line::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both compare against the same get_active_theme() lookup the code under
    // test performs, so they hold under any process-wide active theme.

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn span_with<'a>(line: &'a Line<'_>, needle: &str) -> &'a Span<'a> {
        line.spans
            .iter()
            .find(|s| s.content.contains(needle))
            .unwrap_or_else(|| panic!("no span containing {needle:?} in {:?}", line_text(line)))
    }

    #[test]
    fn loading_spinner_line_uses_the_brand_accent_not_warning_yellow() {
        let ds = rebon_design_system::theme::get_active_theme();
        let (style, title) = prompt_chrome_line(true, false, false, 2_000, "Computing", None, 0);
        assert_eq!(style.fg, Some(parse_theme_color(ds.rebon)));
        let verb = span_with(&title, "Computing…");
        assert_eq!(verb.style.fg, Some(parse_theme_color(ds.rebon)));
        assert!(verb.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn loading_counters_recede_into_the_secondary_tone() {
        let ds = rebon_design_system::theme::get_active_theme();
        let (_, title) = prompt_chrome_line(true, false, false, 19_000, "Mustering", None, 721);
        assert_eq!(
            line_text(&title).trim_end(),
            format!(" {} Mustering… · 19s · 721 tokens", {
                rebon_spinner::glyph_for_frame(rebon_spinner::GlyphPlatform::Other, 19_000 / 80)
            })
        );
        for needle in [" · 19s", " · 721 tokens"] {
            let span = span_with(&title, needle);
            assert_eq!(
                span.style.fg,
                Some(parse_theme_color(ds.inactive)),
                "{needle}"
            );
            assert!(
                !span.style.add_modifier.contains(Modifier::BOLD),
                "{needle}"
            );
        }
    }

    #[test]
    fn loading_title_omits_the_clock_under_a_second_and_the_count_at_zero() {
        let (_, title) = prompt_chrome_line(true, false, false, 400, "Computing", None, 0);
        let text = line_text(&title);
        assert!(text.ends_with("Computing… "), "{text:?}");
        assert!(!text.contains('·'), "{text:?}");
    }

    #[test]
    fn idle_bash_and_help_titles() {
        let ds = rebon_design_system::theme::get_active_theme();
        let (idle, title) = prompt_chrome_line(false, false, false, 0, "", None, 0);
        assert_eq!(idle.fg, Some(parse_theme_color(ds.promptBorder)));
        assert!(title.spans.is_empty());

        let (bash, title) = prompt_chrome_line(false, true, false, 0, "", None, 0);
        assert_eq!(bash.fg, Some(parse_theme_color(ds.bashBorder)));
        assert_eq!(line_text(&title), " bash ");

        let (help, title) = prompt_chrome_line(false, false, true, 0, "", None, 0);
        assert_eq!(help.fg, Some(parse_theme_color(ds.suggestion)));
        assert_eq!(line_text(&title), " help ");
    }

    #[test]
    fn top_hint_tones_keep_info_quiet_and_warning_loud() {
        let ds = rebon_design_system::theme::get_active_theme();
        let info = prompt_top_hint_style(PromptTopHintTone::Info);
        assert_eq!(info.fg, Some(parse_theme_color(ds.inactive)));
        assert!(!info.add_modifier.contains(Modifier::BOLD));
        let warning = prompt_top_hint_style(PromptTopHintTone::Warning);
        assert_eq!(warning.fg, Some(parse_theme_color(ds.warning)));
        assert!(warning.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn badge_foreground_reads_on_light_and_dark_fills() {
        assert_eq!(badge_foreground(Color::Rgb(251, 188, 4)), Color::Black);
        assert_eq!(badge_foreground(Color::Rgb(0, 102, 102)), Color::White);
        assert_eq!(badge_foreground(Color::Rgb(72, 150, 140)), Color::White);
        assert_eq!(badge_foreground(Color::Yellow), Color::Black);
        assert_eq!(badge_foreground(Color::Blue), Color::White);
    }
}

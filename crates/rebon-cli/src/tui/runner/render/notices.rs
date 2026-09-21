use super::*;

pub(in crate::tui::runner) fn queue_display_layout(
    app: &AppState,
    width: u16,
) -> rebon_tui::promptinput::QueueDisplayLayout {
    let processed_queue = rebon_tui::promptinput::process_queued_commands(&app.queued_commands);
    let queue_items: Vec<QueueDisplayItem> = processed_queue
        .iter()
        .filter_map(|cmd| match &cmd.value {
            rebon_tui::promptinput::QueuedCommandValue::Text(t) => {
                Some(QueueDisplayItem { text: t.clone() })
            }
            rebon_tui::promptinput::QueuedCommandValue::NonText => None,
        })
        .collect();
    build_queue_display(&QueueDisplayInput {
        items: queue_items,
        max_width: width.saturating_sub(4) as usize,
    })
}

pub(in crate::tui::runner) fn queue_banner_height(
    layout: &rebon_tui::promptinput::QueueDisplayLayout,
) -> u16 {
    if layout.visible {
        1 + layout.lines.len() as u16
    } else {
        0
    }
}

/// Render the queued-message banner above the prompt.
///
/// Layout: one header line ("N queued message(s)") followed by each
/// queued message prefixed with the stacking glyph `⎿`.
pub(in crate::tui::runner) fn render_queue_banner(
    frame: &mut Frame,
    area: Rect,
    layout: &rebon_tui::promptinput::QueueDisplayLayout,
) {
    if !layout.visible || area.height == 0 {
        return;
    }
    let ds = rebon_design_system::theme::get_active_theme();
    let dim_style = Style::default()
        .fg(parse_theme_color(ds.inactive))
        .add_modifier(Modifier::DIM);

    // Header line: "N queued message(s)"
    if area.height > 0 {
        let header_area = Rect {
            x: area.x + 1,
            y: area.y,
            width: area.width.saturating_sub(1),
            height: 1,
        };
        Paragraph::new(Span::styled(&layout.header, dim_style))
            .render(header_area, frame.buffer_mut());
    }

    // Stacked message lines.
    for (i, line) in layout.lines.iter().enumerate() {
        let row = area.y + 1 + i as u16;
        if row >= area.y + area.height {
            break;
        }
        let line_area = Rect {
            x: area.x + 1,
            y: row,
            width: area.width.saturating_sub(1),
            height: 1,
        };
        let text = format!("{} {}", line.glyph, line.text);
        Paragraph::new(Span::styled(text, dim_style)).render(line_area, frame.buffer_mut());
    }
}

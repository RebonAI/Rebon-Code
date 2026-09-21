//! Workflow tool-call rendering (ratatui paint).
//!
//! The pure body/table builders moved to `rebon-render::workflow_body`
//! so the GPUI app shares them. This file keeps only the ratatui paint:
//! `render_workflow_tool`, the span stylers, and the live-card middle-elision.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use rebon_types::ToolCallStatus;

pub(super) use rebon_render::workflow_body::{
    workflow_body_lines, workflow_interruption_lines, workflow_tool_summary,
};

use super::{
    render_auto_mode_allowed_row, render_gutter_lines, tool_gutter_glyph, tool_gutter_style,
    tool_status_style, wrap_height_visible, RenderTheme, ToolOutputVerbosity, GUTTER,
};

pub(super) fn render_workflow_tool(
    tool: &crate::streaming::StreamingToolUse,
    area: Rect,
    buf: &mut Buffer,
    theme: &RenderTheme,
    verbosity: ToolOutputVerbosity,
    live_card_max_rows: Option<u16>,
    auto_mode_allowed: Option<rebon_types::AutoModeAllowSource>,
) -> u16 {
    let label = workflow_tool_summary(tool);
    let header_text = label
        .as_deref()
        .filter(|label| !label.trim().is_empty())
        .map(|label| format!("Workflow: {label}"))
        .unwrap_or_else(|| "Workflow".to_string());
    let tool_name_style = theme.tool_header.add_modifier(Modifier::BOLD);
    let header_line = if let Some(label) = label.as_ref().filter(|label| !label.trim().is_empty()) {
        Line::from(vec![
            Span::styled("Workflow".to_string(), tool_name_style),
            Span::styled(format!(": {label}"), theme.assistant_prefix),
        ])
    } else {
        Line::from(Span::styled("Workflow".to_string(), tool_name_style))
    };

    let is_failed = tool.status == rebon_types::ToolCallStatus::Failed;
    let is_in_progress = matches!(
        tool.status,
        rebon_types::ToolCallStatus::InProgress | rebon_types::ToolCallStatus::Pending
    );
    let mut consumed = render_gutter_lines(
        vec![header_line],
        &header_text,
        tool_gutter_glyph(theme, is_failed, is_in_progress),
        tool_gutter_style(theme, is_failed, is_in_progress),
        area,
        buf,
    );

    let mut body_text = workflow_body_lines(tool, verbosity).unwrap_or_default();
    if tool.status == ToolCallStatus::Failed {
        let mut interrupted = workflow_interruption_lines(tool);
        if !interrupted.is_empty() {
            interrupted.extend(body_text);
            body_text = interrupted;
        }
    }
    // A live card lives in the inline streaming overlay, which is
    // bottom-anchored and cannot drain an in-progress block: anything taller
    // than the viewport pushes the header (and run summary) above the top
    // edge, where it is neither visible nor in scrollback. Bound the live
    // body by eliding its middle — header + summary stay as context, the
    // freshest rows stay at the bottom. The terminal card that drains to
    // scrollback on completion always renders in full.
    if is_in_progress {
        if let Some(max_rows) = live_card_max_rows {
            let content_width = area.width.saturating_sub(GUTTER).max(1);
            let header_rows = wrap_height_visible(&header_text, content_width) as usize;
            let body_budget = (max_rows as usize)
                .saturating_sub(header_rows)
                .saturating_sub(usize::from(auto_mode_allowed.is_some()));
            body_text = elide_live_workflow_body_middle(body_text, body_budget, content_width);
        }
    }
    if !body_text.is_empty() && consumed < area.height {
        let sub = Rect {
            x: area.x,
            y: area.y.saturating_add(consumed),
            width: area.width,
            height: area.height.saturating_sub(consumed),
        };
        let body_lines = body_text
            .iter()
            .map(|line| styled_workflow_body_line(line, theme))
            .collect::<Vec<_>>();
        let body_text_joined = body_text.join("\n");
        consumed = consumed.saturating_add(render_gutter_lines(
            body_lines,
            &body_text_joined,
            "⎿",
            if is_failed {
                tool_status_style(theme, rebon_types::ToolCallStatus::Failed)
            } else {
                theme.assistant_prefix
            },
            sub,
            buf,
        ));
    }

    consumed.saturating_add(render_auto_mode_allowed_row(
        auto_mode_allowed,
        area,
        consumed,
        buf,
        theme,
    ))
}

fn styled_workflow_body_line(line: &str, theme: &RenderTheme) -> Line<'static> {
    let Some((left, right)) = line.split_once(" │ ") else {
        return Line::from(Span::styled(line.to_string(), theme.streaming));
    };

    let mut spans = Vec::new();
    push_phase_cell_spans(&mut spans, left, theme.streaming);
    spans.push(Span::styled(" │ ".to_string(), theme.streaming));
    push_agent_cell_spans(&mut spans, right, theme.streaming);
    Line::from(spans)
}

fn push_phase_cell_spans(spans: &mut Vec<Span<'static>>, text: &str, base_style: Style) {
    let leading_len = text.len() - text.trim_start().len();
    let rest = &text[leading_len..];
    let Some(state_len) = rest.find(char::is_whitespace) else {
        spans.push(Span::styled(text.to_string(), base_style));
        return;
    };
    let state = &rest[..state_len];
    let after_state = &rest[state_len..];
    let state_padding_len = after_state.len() - after_state.trim_start().len();
    let title_start = leading_len + state_len + state_padding_len;
    if title_start >= text.len() {
        spans.push(Span::styled(text.to_string(), base_style));
        return;
    }

    if leading_len > 0 {
        spans.push(Span::styled(text[..leading_len].to_string(), base_style));
    }
    let state_style = if state == "done" {
        base_style.fg(Color::Green)
    } else {
        base_style
    };
    spans.push(Span::styled(state.to_string(), state_style));
    if state_padding_len > 0 {
        spans.push(Span::styled(
            text[leading_len + state_len..title_start].to_string(),
            base_style,
        ));
    }
    spans.push(Span::styled(
        text[title_start..].to_string(),
        base_style.add_modifier(Modifier::BOLD),
    ));
}

fn push_agent_cell_spans(spans: &mut Vec<Span<'static>>, text: &str, base_style: Style) {
    if text.starts_with("  ") {
        spans.push(Span::styled(text.to_string(), base_style));
        return;
    }

    let leading_len = text.len() - text.trim_start().len();
    let rest = &text[leading_len..];
    let Some(state_len) = rest.find(char::is_whitespace) else {
        spans.push(Span::styled(text.to_string(), base_style));
        return;
    };
    let state = &rest[..state_len];
    let after_state = &rest[state_len..];
    let state_padding_len = after_state.len() - after_state.trim_start().len();
    let agent_start = leading_len + state_len + state_padding_len;
    if agent_start >= text.len() {
        spans.push(Span::styled(text.to_string(), base_style));
        return;
    }
    let agent_end = text[agent_start..]
        .find(" · ")
        .map(|idx| agent_start + idx)
        .unwrap_or(text.len());

    if leading_len > 0 {
        spans.push(Span::styled(text[..leading_len].to_string(), base_style));
    }
    let state_style = if state == "done" {
        base_style.fg(Color::Green)
    } else {
        base_style
    };
    spans.push(Span::styled(state.to_string(), state_style));
    if state_padding_len > 0 {
        spans.push(Span::styled(
            text[leading_len + state_len..agent_start].to_string(),
            base_style,
        ));
    }
    spans.push(Span::styled(
        text[agent_start..agent_end].to_string(),
        base_style.add_modifier(Modifier::BOLD),
    ));
    if agent_end < text.len() {
        spans.push(Span::styled(text[agent_end..].to_string(), base_style));
    }
}

/// Bound a live workflow body to `budget_rows` terminal rows by dropping
/// rows from the middle: the run-summary line (first body line) stays as
/// context, the freshest rows stay at the bottom, and one marker row says
/// how much is hidden. Row accounting is wrap-aware so the measure and
/// paint passes agree on the card height at any width.
fn elide_live_workflow_body_middle(
    lines: Vec<String>,
    budget_rows: usize,
    content_width: u16,
) -> Vec<String> {
    let row_counts: Vec<usize> = lines
        .iter()
        .map(|line| wrap_height_visible(line, content_width) as usize)
        .collect();
    let total_rows: usize = row_counts.iter().sum();
    if total_rows <= budget_rows {
        return lines;
    }

    let head_len = 1usize.min(lines.len());
    let head_rows: usize = row_counts[..head_len].iter().sum();
    let marker_rows = 1usize;
    let tail_budget = budget_rows.saturating_sub(head_rows + marker_rows);

    let mut tail_start = lines.len();
    let mut tail_rows = 0usize;
    while tail_start > head_len {
        let next_rows = row_counts[tail_start - 1];
        if tail_rows + next_rows > tail_budget {
            break;
        }
        tail_rows += next_rows;
        tail_start -= 1;
    }
    // Even when the budget cannot fit head + marker + a tail row, keep the
    // last line so the freshest state stays visible.
    if tail_start == lines.len() && lines.len() > head_len {
        tail_start = lines.len() - 1;
    }

    let hidden_rows: usize = row_counts[head_len..tail_start].iter().sum();
    if hidden_rows == 0 {
        return lines;
    }
    let mut elided = Vec::with_capacity(head_len + 1 + (lines.len() - tail_start));
    elided.extend_from_slice(&lines[..head_len]);
    elided.push(format!(
        "   … {hidden_rows} row{} hidden",
        if hidden_rows == 1 { "" } else { "s" }
    ));
    elided.extend_from_slice(&lines[tail_start..]);
    elided
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styled_workflow_body_line_bolds_phase_and_agent_and_colors_done() {
        let theme = RenderTheme::default();
        let line = styled_workflow_body_line(
            "done Implement │ done implement:workflow-styles · 1 tool · 12 tokens",
            &theme,
        );

        let spans = line.spans;
        assert!(spans.iter().any(|span| {
            span.content.as_ref() == "done" && span.style.fg == Some(Color::Green)
        }));
        assert!(spans.iter().any(|span| {
            span.content.as_ref() == "Implement" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(spans.iter().any(|span| {
            span.content.as_ref() == "implement:workflow-styles"
                && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(spans.iter().any(|span| {
            span.content.as_ref() == " · 1 tool · 12 tokens" && span.style == theme.streaming
        }));
    }
}

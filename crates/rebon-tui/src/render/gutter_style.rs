use ratatui::style::Style;

use super::{tool_status_style, RenderTheme};
use rebon_types::ToolCallStatus;

pub(super) fn tool_gutter_glyph(
    theme: &RenderTheme,
    is_failed: bool,
    is_in_progress: bool,
) -> &'static str {
    if !is_failed && is_in_progress {
        rebon_spinner::tool_call_spinner_glyph(theme.frame_time_ms)
    } else {
        "●"
    }
}

/// The gutter colour carries the call's outcome: red when it failed, the
/// brand accent while it runs, green once it is done.
pub(super) fn tool_gutter_style(
    theme: &RenderTheme,
    is_failed: bool,
    is_in_progress: bool,
) -> Style {
    if is_failed {
        tool_status_style(theme, ToolCallStatus::Failed)
    } else if is_in_progress {
        theme.tool_running
    } else {
        theme.tool_success
    }
}

#[cfg(test)]
mod outcome_tests {
    use ratatui::style::{Color, Style};

    use super::super::RenderTheme;
    use super::{tool_gutter_glyph, tool_gutter_style};

    fn theme() -> RenderTheme {
        RenderTheme {
            tool_success: Style::new().fg(Color::Green),
            tool_running: Style::new().fg(Color::Blue),
            system_error: Style::new().fg(Color::Red),
            assistant_prefix: Style::new().fg(Color::Gray),
            ..RenderTheme::plain()
        }
    }

    #[test]
    fn gutter_colour_carries_the_outcome() {
        let theme = theme();
        assert_eq!(
            tool_gutter_style(&theme, false, false).fg,
            Some(Color::Green)
        );
        assert_eq!(tool_gutter_style(&theme, false, true).fg, Some(Color::Blue));
        // Failure wins over "still running".
        assert_eq!(tool_gutter_style(&theme, true, true).fg, Some(Color::Red));
        assert_eq!(tool_gutter_style(&theme, true, false).fg, Some(Color::Red));
    }

    #[test]
    fn only_a_running_call_spins() {
        let theme = theme();
        assert_eq!(tool_gutter_glyph(&theme, false, false), "●");
        assert_eq!(tool_gutter_glyph(&theme, true, true), "●");
        assert_ne!(tool_gutter_glyph(&theme, false, true), "●");
    }

    #[test]
    fn styled_theme_maps_success_and_running_to_the_palette() {
        use rebon_design_system::theme::{get_theme, ThemeName};
        for name in [ThemeName::Light, ThemeName::Dark, ThemeName::DarkAnsi] {
            let palette = get_theme(name);
            let theme = RenderTheme::from_theme_name(name);
            assert_eq!(
                theme.tool_success.fg,
                Some(super::super::parse_theme_color(palette.success)),
                "{name:?}"
            );
            assert_eq!(
                theme.tool_running.fg,
                Some(super::super::parse_theme_color(palette.rebon)),
                "{name:?}"
            );
        }
        let plain = RenderTheme::plain();
        assert_eq!(plain.tool_success, Style::new());
        assert_eq!(plain.tool_running, Style::new());
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::HashMap;

    use ratatui::{buffer::Buffer, layout::Rect, style::Style};
    use serde_json::json;

    use super::super::{render_streaming_overlay, RenderTheme, ToolOutputVerbosity};
    use crate::streaming::{StreamingOverlay, StreamingToolUse};
    use rebon_types::{ToolCallStatus, ToolKind};

    fn new_buf(w: u16, h: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, w, h))
    }

    /// Helper: build a single-tool-use overlay for gutter-dot tests.
    fn overlay_with_in_progress_tool(status: ToolCallStatus) -> StreamingOverlay {
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: "tool-1".into(),
            tool_name: "Bash".into(),
            kind: ToolKind::Execute,
            status,
            title: Some("Bash running".into()),
            content: None,
            locations: None,
            raw_input: Some(HashMap::from([("command".into(), json!("sleep 5"))])),
            raw_output: None,
        });
        overlay
    }

    fn gutter_symbol(buf: &Buffer, y: u16) -> String {
        buf[(0, y)].symbol().to_string()
    }

    fn gutter_style(buf: &Buffer, y: u16) -> Style {
        buf[(0, y)].style()
    }

    #[test]
    fn in_progress_tool_use_gutter_uses_spinner_glyph() {
        let overlay = overlay_with_in_progress_tool(ToolCallStatus::InProgress);
        let theme = RenderTheme {
            frame_time_ms: 80,
            ..RenderTheme::plain()
        };
        let mut buf = new_buf(40, 4);
        let used = render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 40, 4),
            &mut buf,
            &theme,
            ToolOutputVerbosity::Compact,
        );
        assert!(used > 0);
        assert_eq!(gutter_symbol(&buf, 0), "⠙");
    }

    #[test]
    fn in_progress_tool_use_gutter_advances_spinner_frame() {
        let overlay = overlay_with_in_progress_tool(ToolCallStatus::InProgress);
        let theme = RenderTheme {
            frame_time_ms: 160,
            ..RenderTheme::plain()
        };
        let mut buf = new_buf(40, 4);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 40, 4),
            &mut buf,
            &theme,
            ToolOutputVerbosity::Compact,
        );
        assert_eq!(gutter_symbol(&buf, 0), "⠹");
    }

    #[test]
    fn pending_tool_use_also_uses_spinner_glyph() {
        let overlay = overlay_with_in_progress_tool(ToolCallStatus::Pending);
        let theme = RenderTheme {
            frame_time_ms: 80,
            ..RenderTheme::plain()
        };
        let mut buf = new_buf(40, 4);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 40, 4),
            &mut buf,
            &theme,
            ToolOutputVerbosity::Compact,
        );
        assert_eq!(gutter_symbol(&buf, 0), "⠙");
    }

    #[test]
    fn completed_tool_use_gutter_uses_static_dot() {
        let overlay = overlay_with_in_progress_tool(ToolCallStatus::Completed);
        let theme = RenderTheme {
            frame_time_ms: 160,
            ..RenderTheme::plain()
        };
        let mut buf = new_buf(40, 4);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 40, 4),
            &mut buf,
            &theme,
            ToolOutputVerbosity::Compact,
        );
        assert_eq!(gutter_symbol(&buf, 0), "●");
    }

    #[test]
    fn failed_tool_use_gutter_uses_failed_style_and_static_dot() {
        let overlay = overlay_with_in_progress_tool(ToolCallStatus::Failed);
        let theme = RenderTheme {
            frame_time_ms: 160,
            ..RenderTheme::default_styled()
        };
        let mut buf = new_buf(40, 4);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 40, 4),
            &mut buf,
            &theme,
            ToolOutputVerbosity::Compact,
        );
        let style = gutter_style(&buf, 0);
        assert_eq!(gutter_symbol(&buf, 0), "●");
        assert_ne!(
            style.fg, theme.assistant_prefix.fg,
            "failed gutter fg should differ from assistant prefix"
        );
    }
}

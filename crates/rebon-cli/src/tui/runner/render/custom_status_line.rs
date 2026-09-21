use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use rebon_width::WidthStr;
use unicode_segmentation::UnicodeSegmentation;

use crate::tui::app::AppState;
use crate::tui::runner::custom_status_line::{
    padded_status_line_lines, status_line_background_tasks_prefix, status_line_permission_prefix,
    STATUS_LINE_MAX_LINES,
};

pub(in crate::tui::runner) fn custom_status_line_height(app: &AppState, width: u16) -> u16 {
    status_line_rows(app, width).len().saturating_sub(1) as u16
}

pub(in crate::tui::runner) fn render_custom_status_line(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let rows = status_line_rows(app, area.width);
    let extra_rows = rows.len().saturating_sub(1).min(area.height as usize);
    frame.render_widget(
        Paragraph::new(Text::from(
            rows.into_iter().take(extra_rows).collect::<Vec<_>>(),
        )),
        area,
    );
}

pub(in crate::tui::runner) fn render_custom_status_line_footer(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
) -> bool {
    if area.width == 0 || area.height == 0 || app.custom_status_line.output.is_empty() {
        return false;
    }
    let Some(line) = status_line_rows(app, area.width).pop() else {
        return false;
    };
    frame.render_widget(Paragraph::new(line), area);
    true
}

fn status_line_rows(app: &AppState, width: u16) -> Vec<Line<'static>> {
    if width == 0 || app.custom_status_line.output.is_empty() {
        return Vec::new();
    }
    let padding = app
        .custom_status_line
        .config
        .as_ref()
        .map(|config| config.padding())
        .unwrap_or(0);
    let padded = padded_status_line_lines(&app.custom_status_line.output, padding);
    let Some(primary) = padded.first() else {
        return Vec::new();
    };

    let mut primary_line = ansi_line_to_ratatui(primary.clone());
    let mut primary_spans = status_line_prefix_spans(app);
    primary_spans.append(&mut primary_line.spans);
    primary_line.spans = primary_spans;
    let mut primary_rows = hard_wrap_line(primary_line, width);
    let mut extra_rows = padded
        .into_iter()
        .skip(1)
        .flat_map(|line| hard_wrap_line(ansi_line_to_ratatui(line), width))
        .take(STATUS_LINE_MAX_LINES.saturating_sub(1))
        .collect::<Vec<_>>();
    primary_rows.truncate(STATUS_LINE_MAX_LINES.saturating_sub(extra_rows.len()));
    extra_rows.extend(primary_rows);
    extra_rows
}

fn status_line_prefix_spans(app: &AppState) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if let Some(prefix) = status_line_permission_prefix(app) {
        spans.push(Span::styled(prefix, Style::default().fg(Color::Yellow)));
    }
    if let Some(prefix) = status_line_background_tasks_prefix(app) {
        let mut style = Style::default().fg(Color::Yellow);
        if super::tasks_footer_is_selected(app) {
            style = style
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(prefix, style));
    }
    spans
}

fn hard_wrap_line(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let line_style = line.style;
    let alignment = line.alignment;
    let width = usize::from(width);
    let mut rows = Vec::new();
    let mut row_spans = Vec::new();
    let mut row_width = 0usize;

    for span in line.spans {
        let style = span.style;
        let mut chunk = String::new();
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = WidthStr::width(grapheme);
            if grapheme_width > 0
                && row_width > 0
                && row_width.saturating_add(grapheme_width) > width
            {
                if !chunk.is_empty() {
                    row_spans.push(Span::styled(std::mem::take(&mut chunk), style));
                }
                rows.push(Line {
                    style: line_style,
                    alignment,
                    spans: std::mem::take(&mut row_spans),
                });
                row_width = 0;
            }
            chunk.push_str(grapheme);
            row_width = row_width.saturating_add(grapheme_width);
        }
        if !chunk.is_empty() {
            row_spans.push(Span::styled(chunk, style));
        }
    }

    if !row_spans.is_empty() || rows.is_empty() {
        rows.push(Line {
            style: line_style,
            alignment,
            spans: row_spans,
        });
    }
    rows
}

fn ansi_line_to_ratatui(input: String) -> Line<'static> {
    let mut spans = Vec::new();
    let mut style = Style::default();
    let mut buf = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && matches!(chars.peek(), Some('[')) {
            let _ = chars.next();
            let mut code = String::new();
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
                code.push(c);
            }
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), style));
            }
            apply_sgr(&mut style, &code);
        } else {
            buf.push(ch);
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style));
    }
    Line::from(spans)
}

fn apply_sgr(style: &mut Style, code: &str) {
    let mut codes = if code.is_empty() {
        vec![0]
    } else {
        code.split(';')
            .filter_map(|part| part.parse::<u16>().ok())
            .collect::<Vec<_>>()
    };
    if codes.is_empty() {
        codes.push(0);
    }
    let mut idx = 0;
    while idx < codes.len() {
        match codes[idx] {
            0 => *style = Style::default(),
            1 => style.add_modifier |= Modifier::BOLD,
            2 => style.add_modifier |= Modifier::DIM,
            22 => {
                style.add_modifier.remove(Modifier::BOLD | Modifier::DIM);
                style.sub_modifier |= Modifier::BOLD | Modifier::DIM;
            }
            3 => style.add_modifier |= Modifier::ITALIC,
            23 => {
                style.add_modifier.remove(Modifier::ITALIC);
                style.sub_modifier |= Modifier::ITALIC;
            }
            4 => style.add_modifier |= Modifier::UNDERLINED,
            24 => {
                style.add_modifier.remove(Modifier::UNDERLINED);
                style.sub_modifier |= Modifier::UNDERLINED;
            }
            7 => style.add_modifier |= Modifier::REVERSED,
            27 => {
                style.add_modifier.remove(Modifier::REVERSED);
                style.sub_modifier |= Modifier::REVERSED;
            }
            30..=37 => style.fg = basic_color(codes[idx] - 30, false),
            39 => style.fg = None,
            40..=47 => style.bg = basic_color(codes[idx] - 40, false),
            49 => style.bg = None,
            90..=97 => style.fg = basic_color(codes[idx] - 90, true),
            100..=107 => style.bg = basic_color(codes[idx] - 100, true),
            38 | 48 => {
                let is_fg = codes[idx] == 38;
                if idx + 2 < codes.len() && codes[idx + 1] == 5 {
                    if let Some(color) = indexed_color(codes[idx + 2]) {
                        if is_fg {
                            style.fg = Some(color);
                        } else {
                            style.bg = Some(color);
                        }
                    }
                    idx += 2;
                } else if idx + 4 < codes.len() && codes[idx + 1] == 2 {
                    let color = Color::Rgb(
                        codes[idx + 2] as u8,
                        codes[idx + 3] as u8,
                        codes[idx + 4] as u8,
                    );
                    if is_fg {
                        style.fg = Some(color);
                    } else {
                        style.bg = Some(color);
                    }
                    idx += 4;
                }
            }
            _ => {}
        }
        idx += 1;
    }
}

fn basic_color(index: u16, bright: bool) -> Option<Color> {
    Some(match (index, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        (7, true) => Color::White,
        _ => return None,
    })
}

fn indexed_color(index: u16) -> Option<Color> {
    match index {
        0..=15 => basic_color(index % 8, index >= 8),
        16..=255 => Some(Color::Indexed(index as u8)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_config::{StatusLineConfig, StatusLineKind};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use rebon_width::WidthStr;

    #[test]
    fn renders_extra_lines_with_padding() {
        let mut app = AppState::default();
        app.custom_status_line.config = Some(StatusLineConfig {
            kind: StatusLineKind::Command,
            command: String::from("ignored"),
            script: None,
            padding: Some(2),
            refresh_interval: Some(1),
            hide_vim_mode_indicator: Some(true),
        });
        app.custom_status_line.output = vec![String::from("alpha"), String::from("beta")];
        let backend = TestBackend::new(20, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_custom_status_line(frame, Rect::new(0, 0, 20, 2), &app))
            .unwrap();
        let buf = terminal.backend().buffer();
        let row = |y| {
            (0..20)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        assert_eq!(row(0), "  beta");
        assert_eq!(row(1), "");
    }

    #[test]
    fn renders_permission_tasks_and_custom_status_on_same_row() {
        let mut app = AppState::default();
        app.set_permission_mode(rebon_permissions::PermissionMode::Auto);
        app.custom_status_line.output = vec![String::from("● GPT 5.6 SOL   ◷ 2.7% 27k/1M")];

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

        let backend = TestBackend::new(100, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                assert!(render_custom_status_line_footer(
                    frame,
                    Rect::new(0, 0, 100, 1),
                    &app
                ));
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        let row = (0..100).map(|x| buf[(x, 0)].symbol()).collect::<String>();
        let permission = row.find("⏵⏵ Auto").expect(&row);
        let tasks = row.find("1 background shell running").expect(&row);
        let custom = row.find("● GPT 5.6 SOL").expect(&row);
        assert!(permission < tasks && tasks < custom, "{row:?}");

        let expected_width =
            WidthStr::width(" ⏵⏵ Auto ") + WidthStr::width(" 1 background shell running ");
        assert_eq!(
            crate::tui::runner::custom_status_line::status_line_reserved_prefix_width(&app)
                as usize,
            expected_width
        );
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().fold(String::new(), |mut text, span| {
            text.push_str(span.content.as_ref());
            text
        })
    }

    #[test]
    fn wraps_primary_status_across_reserved_rows() {
        let mut app = AppState::default();
        app.custom_status_line.output = vec![String::from("abcdefghij")];

        let rows = status_line_rows(&app, 6);
        assert_eq!(
            rows.iter().map(line_text).collect::<Vec<_>>(),
            ["abcdef", "ghij"]
        );
        assert_eq!(custom_status_line_height(&app, 6), 1);

        let backend = TestBackend::new(6, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_custom_status_line(frame, Rect::new(0, 0, 6, 1), &app);
                assert!(render_custom_status_line_footer(
                    frame,
                    Rect::new(0, 1, 6, 1),
                    &app
                ));
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let row = |y| {
            (0..6)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        assert_eq!(row(0), "abcdef");
        assert_eq!(row(1), "ghij");
    }

    #[test]
    fn wraps_unicode_by_terminal_width() {
        let mut app = AppState::default();
        app.custom_status_line.output = vec![String::from("界界界")];

        let rows = status_line_rows(&app, 4);

        assert_eq!(
            rows.iter().map(line_text).collect::<Vec<_>>(),
            ["界界", "界"]
        );
    }

    #[test]
    fn wrapping_keeps_emoji_grapheme_clusters_intact() {
        let mut app = AppState::default();
        app.custom_status_line.output = vec![String::from("👨‍👩‍👧‍👦X")];

        let rows = status_line_rows(&app, 2);

        assert_eq!(rows.iter().map(line_text).collect::<Vec<_>>(), ["👨‍👩‍👧‍👦", "X"]);
    }

    #[test]
    fn wrapping_preserves_ansi_style() {
        let mut app = AppState::default();
        app.custom_status_line.output = vec![String::from("\u{1b}[31mabcdef\u{1b}[0m")];

        let rows = status_line_rows(&app, 3);

        assert_eq!(
            rows.iter().map(line_text).collect::<Vec<_>>(),
            ["abc", "def"]
        );
        assert!(rows.iter().all(|line| {
            line.spans
                .iter()
                .filter(|span| !span.content.is_empty())
                .all(|span| span.style.fg == Some(Color::Red))
        }));
    }

    #[test]
    fn wrapped_status_is_capped_at_maximum_height() {
        let mut app = AppState::default();
        app.custom_status_line.output = vec![String::from("abcdefghijkl")];

        let rows = status_line_rows(&app, 2);

        assert_eq!(rows.len(), STATUS_LINE_MAX_LINES);
        assert_eq!(
            rows.iter().map(line_text).collect::<Vec<_>>(),
            ["ab", "cd", "ef", "gh", "ij"]
        );
        assert_eq!(custom_status_line_height(&app, 2), 4);
    }

    #[test]
    fn explicit_extra_lines_remain_above_wrapped_primary_status() {
        let mut app = AppState::default();
        app.custom_status_line.output = vec![String::from("abcdef"), String::from("extra")];

        let rows = status_line_rows(&app, 3);

        assert_eq!(
            rows.iter().map(line_text).collect::<Vec<_>>(),
            ["ext", "ra", "abc", "def"]
        );
    }

    #[test]
    fn explicit_extra_lines_keep_capacity_when_primary_wrap_hits_limit() {
        let mut app = AppState::default();
        app.custom_status_line.output = vec![String::from("abcdefghij"), String::from("extra")];

        let rows = status_line_rows(&app, 2);

        assert_eq!(
            rows.iter().map(line_text).collect::<Vec<_>>(),
            ["ex", "tr", "a", "ab", "cd"]
        );
    }

    #[test]
    fn renders_first_line_in_footer() {
        let mut app = AppState::default();
        app.custom_status_line.config = Some(StatusLineConfig {
            kind: StatusLineKind::Command,
            command: String::from("ignored"),
            script: None,
            padding: Some(1),
            refresh_interval: Some(1),
            hide_vim_mode_indicator: Some(true),
        });
        app.custom_status_line.output = vec![String::from("alpha"), String::from("beta")];
        let backend = TestBackend::new(20, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                assert!(render_custom_status_line_footer(
                    frame,
                    Rect::new(0, 0, 20, 1),
                    &app
                ));
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let row = (0..20)
            .map(|x| buf[(x, 0)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string();
        assert_eq!(row, " alpha");
    }
}

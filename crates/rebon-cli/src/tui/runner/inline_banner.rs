//! Inline 会话顶部的 Rebon banner；开始输出前，配置变化直接替换当前 banner。

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use rebon_permissions::PermissionMode;
use rebon_tui::parse_theme_color;
use rebon_width::WidthChar;

use crate::tui::app::AppState;
use crate::tui::runner::inline_commit_cursor::insert_before_cjk_safe;
use crate::tui::wiring::TuiEngineSession;

/// The startup banner for whatever the slot holds: the session's own once
/// it is in, the preview's until then.
pub(super) fn startup_banner_for_slot(
    slot: &super::SessionSlot,
    app: &AppState,
) -> InlineStartupBanner {
    match slot.session() {
        Some(session) => InlineStartupBanner::from_session(
            session,
            app.coordinator_mode,
            inline_banner_effort_display(app),
        ),
        None => InlineStartupBanner::from_preview(
            slot.preview(),
            app.permission_mode,
            app.coordinator_mode,
            inline_banner_effort_display(app),
        ),
    }
}

pub(super) fn inline_banner_effort_display(app: &AppState) -> String {
    app.effort_level
        .map(|level| {
            let label = app.effort_provider_kind.label();
            format!("{} {}", label, level.as_str())
        })
        .unwrap_or_default()
}

fn session_permission_mode(session: &TuiEngineSession) -> PermissionMode {
    session
        .engine_half
        .permission_mode_cell
        .lock()
        .map(|mode| *mode)
        .unwrap_or(PermissionMode::Default)
}

fn inline_banner_mode_display(permission_mode: PermissionMode, coordinator_mode: bool) -> String {
    let permission_label = if rebon_permissions::is_default_mode(Some(permission_mode)) {
        None
    } else {
        Some(rebon_permissions::permission_mode_short_title(
            permission_mode,
        ))
    };

    match (coordinator_mode, permission_label) {
        (true, Some(label)) => format!("coordinator · {label}"),
        (true, None) => "coordinator".to_string(),
        (false, Some(label)) => label.to_string(),
        (false, None) => "normal".to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InlineStartupBanner {
    pub(super) provider: String,
    pub(super) model: String,
    pub(super) fast_mode: bool,
    pub(super) directory: String,
    pub(super) mode: String,
    pub(super) effort_display: String,
}

impl InlineStartupBanner {
    const FULL_HEIGHT: u16 = 7;
    const COMPACT_HEIGHT: u16 = 5;
    const NARROW_HEIGHT: u16 = 1;
    const BOTTOM_GAP: u16 = 1;
    const MAX_FULL_WIDTH: u16 = 120;
    const MAX_FULL_LEFT_WIDTH: u16 = 46;
    const MIN_FULL_WIDTH: u16 = 72;
    const MIN_COMPACT_WIDTH: u16 = 32;

    pub(super) fn from_session(
        session: &TuiEngineSession,
        coordinator_mode: bool,
        effort_display: String,
    ) -> Self {
        Self {
            provider: session.model.provider_name.trim().to_string(),
            model: session.model.name.trim().to_string(),
            fast_mode: session.model.service_tier_available && session.model.service_tier.is_fast(),
            directory: session.cwd.trim().to_string(),
            mode: inline_banner_mode_display(session_permission_mode(session), coordinator_mode),
            effort_display,
        }
    }

    /// The banner for a session that is still being built: the same
    /// facts, from the startup preview (RFC-0004 §9 — the banner goes out
    /// with the first frame, and the first frame does not wait).
    pub(super) fn from_preview(
        preview: &super::StartupPreview,
        permission_mode: PermissionMode,
        coordinator_mode: bool,
        effort_display: String,
    ) -> Self {
        Self {
            provider: preview.provider_name.trim().to_string(),
            model: preview.model_name.trim().to_string(),
            fast_mode: preview.fast_mode,
            directory: preview.cwd.trim().to_string(),
            mode: inline_banner_mode_display(permission_mode, coordinator_mode),
            effort_display,
        }
    }

    pub(super) fn height_for_width(width: u16) -> u16 {
        if width >= Self::MIN_FULL_WIDTH {
            Self::FULL_HEIGHT
        } else if width >= Self::MIN_COMPACT_WIDTH {
            Self::COMPACT_HEIGHT
        } else {
            Self::NARROW_HEIGHT
        }
    }

    fn insert_height_for_size(width: u16, terminal_height: u16) -> u16 {
        let banner_height = Self::height_for_width(width).min(terminal_height);
        if terminal_height > banner_height.saturating_add(Self::BOTTOM_GAP) {
            banner_height.saturating_add(Self::BOTTOM_GAP)
        } else {
            banner_height
        }
    }

    fn provider_display(&self) -> &str {
        if self.provider.is_empty() {
            "unknown"
        } else {
            &self.provider
        }
    }

    fn model_display(&self) -> &str {
        if self.model.is_empty() {
            "unknown"
        } else {
            &self.model
        }
    }

    fn directory_display(&self) -> &str {
        if self.directory.is_empty() {
            "unknown"
        } else {
            &self.directory
        }
    }

    fn mode_display(&self) -> &str {
        if self.mode.is_empty() {
            "normal"
        } else {
            &self.mode
        }
    }

    pub(super) fn render(&self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if area.width >= Self::MIN_FULL_WIDTH && area.height >= Self::FULL_HEIGHT {
            self.render_full(area, buf);
        } else if area.width >= Self::MIN_COMPACT_WIDTH && area.height >= Self::COMPACT_HEIGHT {
            self.render_compact(area, buf);
        } else {
            self.render_narrow(area, buf);
        }
    }

    fn render_narrow(&self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let value_style = Style::default();
        let fast_style = Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD);
        let mut spans = vec![
            Span::styled("Rebon", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" · "),
            Span::styled(self.provider_display().to_string(), value_style),
            Span::raw(" · "),
            Span::styled(self.model_display().to_string(), value_style),
        ];
        if self.fast_mode {
            spans.push(Span::raw(" "));
            spans.push(Span::styled("[Fast]", fast_style));
        }
        Paragraph::new(Line::from(spans)).render(area, buf);
    }

    fn render_compact(&self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let theme = rebon_design_system::theme::get_active_theme();
        let rebon = parse_theme_color(theme.rebon);
        let brand_strong = parse_theme_color(theme.rebonShimmer);
        let border_style = Style::default().fg(brand_strong);
        let brand_style = Style::default().fg(rebon).add_modifier(Modifier::BOLD);
        let key_style = Style::default().fg(rebon);
        let value_style = Style::default().fg(Color::Rgb(230, 230, 230));
        let fast_style = Style::default()
            .fg(Color::Rgb(105, 220, 135))
            .add_modifier(Modifier::BOLD);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(ratatui::symbols::border::ROUNDED)
            .border_style(border_style)
            .title(Line::from(vec![
                Span::raw(" "),
                Span::styled("Rebon", brand_style),
                Span::raw(" · "),
                Span::styled(self.mode_display(), value_style),
                Span::raw(" "),
            ]));
        let inner = block.inner(area);
        block.render(area, buf);

        let padded = Rect {
            x: inner.x.saturating_add(1),
            width: inner.width.saturating_sub(2),
            ..inner
        };
        Paragraph::new(vec![
            kv_model_line(
                self.model_display(),
                self.fast_mode,
                key_style,
                value_style,
                fast_style,
            ),
            kv_line("provider", self.provider_display(), key_style, value_style),
            kv_line(
                "directory",
                self.directory_display(),
                key_style,
                value_style,
            ),
        ])
        .render(padded, buf);
    }

    fn render_full(&self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let area = Rect {
            width: area.width.min(Self::MAX_FULL_WIDTH),
            ..area
        };
        let theme = rebon_design_system::theme::get_active_theme();
        let rebon = parse_theme_color(theme.rebon);
        let brand_strong = parse_theme_color(theme.rebonShimmer);
        let border_style = Style::default().fg(brand_strong);
        let brand_style = Style::default().fg(rebon).add_modifier(Modifier::BOLD);
        let title_muted = Style::default().fg(Color::Rgb(160, 148, 140));
        let key_style = Style::default().fg(rebon).add_modifier(Modifier::BOLD);
        let readable_style = Style::default().fg(Color::Rgb(135, 135, 150));
        let command_style = Style::default().fg(Color::Rgb(120, 130, 245));
        let fast_style = Style::default()
            .fg(Color::Rgb(105, 220, 135))
            .add_modifier(Modifier::BOLD);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(ratatui::symbols::border::ROUNDED)
            .border_style(border_style)
            .title(Line::from(vec![
                Span::raw(" "),
                Span::styled("Rebon", brand_style),
                Span::styled(format!(" v{} ", env!("CARGO_PKG_VERSION")), title_muted),
            ]));
        let inner = block.inner(area);
        block.render(area, buf);

        let padded = Rect {
            x: inner.x.saturating_add(1),
            width: inner.width.saturating_sub(2),
            ..inner
        };
        let gap_width = if padded.width >= 72 { 3 } else { 2 };
        let min_actions_width = 36.min(padded.width.saturating_sub(gap_width));
        let available_left_width = padded
            .width
            .saturating_sub(gap_width.saturating_add(min_actions_width));
        let left_width = available_left_width.min(Self::MAX_FULL_LEFT_WIDTH);
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(left_width),
                Constraint::Length(gap_width),
                Constraint::Min(0),
            ])
            .split(padded);

        Paragraph::new(vec![
            Line::from(Span::styled("Session", key_style)),
            mode_line(
                self.mode_display(),
                &self.effort_display,
                key_style,
                readable_style,
                title_muted,
            ),
            kv_model_line(
                self.model_display(),
                self.fast_mode,
                key_style,
                readable_style,
                fast_style,
            ),
            kv_line(
                "provider",
                self.provider_display(),
                key_style,
                readable_style,
            ),
            kv_line(
                "directory",
                self.directory_display(),
                key_style,
                readable_style,
            ),
        ])
        .render(chunks[0], buf);

        let separator_text = if gap_width >= 3 { " │ " } else { "│" };
        Paragraph::new(
            (0..5)
                .map(|_| Line::from(Span::styled(separator_text, title_muted)))
                .collect::<Vec<_>>(),
        )
        .render(chunks[1], buf);

        Paragraph::new(vec![
            Line::from(Span::styled("Quick actions", key_style)),
            action_line("/ceo", "manager-lead agents", command_style, readable_style),
            action_line(
                "/ultrawork",
                "workflow orchestration",
                command_style,
                readable_style,
            ),
            action_line(
                "/ultraplan",
                "parallel read+plan",
                command_style,
                readable_style,
            ),
            action_line("/goal", "loop until done", command_style, readable_style),
        ])
        .render(chunks[2], buf);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui::runner) struct PageBanner {
    name: String,
}

impl PageBanner {
    const BOTTOM_GAP: u16 = 1;

    pub(in crate::tui::runner) fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    fn wrapped_lines(&self, width: u16) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        let width = width as usize;
        let mut lines = Vec::new();
        for logical_line in self.name.split('\n') {
            let mut line = String::new();
            let mut line_width: usize = 0;
            for ch in logical_line.chars() {
                let ch_width = WidthChar::width(ch).unwrap_or(0);
                if !line.is_empty() && line_width.saturating_add(ch_width) > width {
                    lines.push(std::mem::take(&mut line));
                    line_width = 0;
                }
                if line.is_empty() && ch_width > width {
                    lines.push(ch.to_string());
                    continue;
                }
                line.push(ch);
                line_width = line_width.saturating_add(ch_width);
            }
            lines.push(line);
        }
        lines
    }

    pub(in crate::tui::runner) fn height_for_width(&self, width: u16) -> u16 {
        self.wrapped_lines(width)
            .len()
            .try_into()
            .unwrap_or(u16::MAX)
    }

    fn insert_height_for_size(&self, width: u16, terminal_height: u16) -> u16 {
        let banner_height = self.height_for_width(width).min(terminal_height);
        if terminal_height > banner_height.saturating_add(Self::BOTTOM_GAP) {
            banner_height.saturating_add(Self::BOTTOM_GAP)
        } else {
            banner_height
        }
    }

    pub(in crate::tui::runner) fn render(&self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let theme = rebon_design_system::theme::get_active_theme();
        let style = Style::default()
            .fg(parse_theme_color(theme.rebon))
            .add_modifier(Modifier::BOLD);
        let lines = self
            .wrapped_lines(area.width)
            .into_iter()
            .map(|line| Line::from(Span::styled(line, style)))
            .collect::<Vec<_>>();
        Paragraph::new(lines).render(area, buf);
    }
}

fn mode_line<'a>(
    mode: &'a str,
    effort: &'a str,
    key_style: Style,
    value_style: Style,
    muted_style: Style,
) -> Line<'a> {
    let mut spans = vec![
        Span::styled(format!("{:<11}", "mode:"), key_style),
        Span::styled(mode, value_style),
    ];
    if !effort.is_empty() {
        spans.push(Span::styled("  ·  ", muted_style));
        spans.push(Span::styled(effort, value_style));
    }
    Line::from(spans)
}

fn kv_line<'a>(
    key: &'static str,
    value: &'a str,
    key_style: Style,
    value_style: Style,
) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{:<11}", format!("{key}:")), key_style),
        Span::styled(value, value_style),
    ])
}

fn kv_model_line<'a>(
    model: &'a str,
    fast_mode: bool,
    key_style: Style,
    value_style: Style,
    fast_style: Style,
) -> Line<'a> {
    let mut spans = vec![
        Span::styled(format!("{:<11}", "model:"), key_style),
        Span::styled(model, value_style),
    ];
    if fast_mode {
        spans.push(Span::raw(" "));
        spans.push(Span::styled("[Fast]", fast_style));
    }
    Line::from(spans)
}

fn action_line<'a>(
    command: &'static str,
    desc: &'static str,
    command_style: Style,
    muted_style: Style,
) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{:<12}", command), command_style),
        Span::styled(desc, muted_style),
    ])
}

pub(super) fn prepare_inline_viewport_for_startup_banner<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
) -> std::io::Result<u16> {
    let size = terminal.size()?;
    if size.width == 0 || size.height == 0 {
        return Ok(0);
    }
    let insert_height = InlineStartupBanner::insert_height_for_size(size.width, size.height);
    let viewport_height = size.height.saturating_sub(insert_height).max(1);
    terminal.set_viewport_height(viewport_height)?;
    Ok(viewport_height)
}

pub(in crate::tui::runner) fn prepare_inline_viewport_for_page_banner<
    B: ratatui::backend::Backend,
>(
    terminal: &mut ratatui::Terminal<B>,
    banner: &PageBanner,
) -> std::io::Result<u16> {
    let size = terminal.size()?;
    if size.width == 0 || size.height == 0 {
        return Ok(0);
    }
    let insert_height = banner.insert_height_for_size(size.width, size.height);
    let viewport_height = size.height.saturating_sub(insert_height).max(1);
    terminal.set_viewport_height(viewport_height)?;
    Ok(viewport_height)
}

pub(in crate::tui::runner) fn emit_inline_page_banner<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    banner: &PageBanner,
) -> std::io::Result<()> {
    let size = terminal.size()?;
    let insert_height = banner.insert_height_for_size(size.width, size.height);
    if insert_height == 0 {
        return Ok(());
    }
    let banner_height = banner.height_for_width(size.width).min(insert_height);
    insert_before_cjk_safe(terminal, insert_height, |buf| {
        banner.render(
            Rect {
                height: banner_height.min(buf.area.height),
                ..buf.area
            },
            buf,
        );
    })
}

/// Emit an already-built startup banner — the session's, or the preview's
/// while there is no session yet.
pub(super) fn emit_inline_startup_banner_from<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    banner: &InlineStartupBanner,
) -> anyhow::Result<()> {
    let size = terminal.size()?;
    let width = size.width;
    let insert_height = InlineStartupBanner::insert_height_for_size(width, size.height);
    if insert_height == 0 {
        return Ok(());
    }
    let banner_height = InlineStartupBanner::height_for_width(width).min(insert_height);
    insert_before_cjk_safe(terminal, insert_height, |buf| {
        let area = Rect {
            height: banner_height.min(buf.area.height),
            ..buf.area
        };
        banner.render(area, buf);
    })?;
    Ok(())
}
pub(super) fn refresh_inline_startup_banner<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    banner: &InlineStartupBanner,
) -> anyhow::Result<u16> {
    terminal.backend_mut().clear()?;
    terminal.backend_mut().set_cursor_position((0, 0))?;
    terminal.reseed_inline_viewport_after_clear()?;
    let height = prepare_inline_viewport_for_startup_banner(terminal)?;
    emit_inline_startup_banner_from(terminal, banner)?;
    Ok(height)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area;
        let mut out = String::new();
        for y in area.y..area.y.saturating_add(area.height) {
            for x in area.x..area.x.saturating_add(area.width) {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    fn cell_at_text<'a>(
        buf: &'a ratatui::buffer::Buffer,
        needle: &str,
    ) -> Option<&'a ratatui::buffer::Cell> {
        let area = buf.area;
        for y in area.y..area.y.saturating_add(area.height) {
            let mut line = String::new();
            for x in area.x..area.x.saturating_add(area.width) {
                if let Some(cell) = buf.cell((x, y)) {
                    line.push_str(cell.symbol());
                }
            }
            let Some(column) = line.find(needle) else {
                continue;
            };
            let x = area.x.saturating_add(column as u16);
            if let Some(cell) = buf.cell((x, y)) {
                return Some(cell);
            }
        }
        None
    }

    #[test]
    fn prepare_inline_viewport_for_startup_banner_places_inserted_banner_at_top() {
        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(4),
            },
        )
        .expect("inline terminal constructs");
        terminal
            .insert_before(16, |buf| {
                Paragraph::new(vec![Line::raw("history"); 16]).render(buf.area, buf);
            })
            .expect("move inline viewport to bottom");
        assert_eq!(terminal.get_frame().area().y, 16);

        let prepared_height = prepare_inline_viewport_for_startup_banner(&mut terminal)
            .expect("prepare viewport for banner");
        assert_eq!(prepared_height, 12);
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 8, 80, 12));

        insert_before_cjk_safe(&mut terminal, 8, |buf| {
            let area = Rect {
                height: 7,
                ..buf.area
            };
            Paragraph::new(
                (0..7)
                    .map(|idx| Line::raw(format!("banner-{idx}")))
                    .collect::<Vec<_>>(),
            )
            .render(area, buf);
        })
        .expect("insert banner");

        let rows = (0..8)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(rows[0].contains("banner-0"), "{rows:?}");
        assert!(rows[6].contains("banner-6"), "{rows:?}");
        assert!(rows[7].trim().is_empty(), "{rows:?}");
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 8, 80, 12));
    }

    #[test]
    fn refresh_inline_banner_replaces_old_values_without_duplicate_rows() {
        for width in [60, 120] {
            let mut terminal = ratatui::Terminal::with_options(
                ratatui::backend::TestBackend::new(width, 24),
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Inline(4),
                },
            )
            .unwrap();
            let mut banner = InlineStartupBanner {
                provider: "old-provider".into(),
                model: "old-model".into(),
                fast_mode: false,
                directory: "project".into(),
                mode: "normal".into(),
                effort_display: String::new(),
            };
            prepare_inline_viewport_for_startup_banner(&mut terminal).unwrap();
            emit_inline_startup_banner_from(&mut terminal, &banner).unwrap();
            banner.provider = "new-provider".into();
            banner.model = "new-model".into();
            banner.mode = "Plan".into();
            banner.fast_mode = true;
            for _ in 0..3 {
                refresh_inline_startup_banner(&mut terminal, &banner).unwrap();
                let text = buffer_text(terminal.backend().buffer());
                assert!(!text.contains("old-provider"), "{text}");
                assert!(!text.contains("old-model"), "{text}");
                assert_eq!(text.matches("new-model").count(), 1, "{text}");
                assert!(text.contains("new-provider"), "{text}");
                assert!(text.contains("Plan"), "{text}");
                assert!(text.contains("[Fast]"), "{text}");
            }
            banner.fast_mode = false;
            refresh_inline_startup_banner(&mut terminal, &banner).unwrap();
            assert!(!buffer_text(terminal.backend().buffer()).contains("[Fast]"));
        }
    }

    #[test]
    fn inline_startup_banner_from_session_tracks_provider_model_and_fast() {
        let mut session = crate::tui::runner::test_support::make_test_tui_session();
        session.model.provider_name = "next-provider".into();
        session.model.name = "next-model".into();
        session.model.service_tier_available = true;
        session.model.service_tier.set_fast(true);
        let banner = InlineStartupBanner::from_session(&session, false, String::new());
        assert_eq!(banner.provider, "next-provider");
        assert_eq!(banner.model, "next-model");
        assert!(banner.fast_mode);
        session.model.service_tier_available = false;
        assert!(!InlineStartupBanner::from_session(&session, false, String::new()).fast_mode);
    }

    #[test]
    fn inline_startup_banner_from_session_uses_permission_mode() {
        let session = crate::tui::runner::test_support::make_test_tui_session();

        let normal = InlineStartupBanner::from_session(&session, false, String::new());
        *session.engine_half.permission_mode_cell.lock().unwrap() =
            rebon_permissions::PermissionMode::Auto;
        let auto = InlineStartupBanner::from_session(&session, false, String::new());
        let coordinator = InlineStartupBanner::from_session(&session, true, String::new());

        assert_eq!(normal.mode, "normal");
        assert_eq!(auto.mode, "Auto");
        assert_eq!(coordinator.mode, "coordinator · Auto");
    }

    #[test]
    fn inline_startup_banner_full_layout_shows_session_details_and_actions() {
        let banner = InlineStartupBanner {
            provider: "openai".to_string(),
            model: "gpt-5.5".to_string(),
            fast_mode: false,
            directory: "D:\\own\\MyVault".to_string(),
            mode: "normal".to_string(),
            effort_display: "thinking xhigh".to_string(),
        };
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 120, 7));

        banner.render(buf.area, &mut buf);
        let rendered = buffer_text(&buf);

        assert!(rendered.contains("Session"), "{rendered}");
        assert!(rendered.contains("mode:"), "{rendered}");
        assert!(rendered.contains("normal"), "{rendered}");
        assert!(!rendered.contains("Rebon Agent"), "{rendered}");
        assert!(rendered.contains("│ Quick actions"), "{rendered}");
        assert!(rendered.contains("model:"), "{rendered}");
        assert!(rendered.contains("gpt-5.5"), "{rendered}");
        assert!(rendered.contains("provider:"), "{rendered}");
        assert!(rendered.contains("openai"), "{rendered}");
        assert!(rendered.contains("directory:"), "{rendered}");
        assert!(rendered.contains("D:\\own\\MyVault"), "{rendered}");
        assert!(rendered.contains("/ceo"), "{rendered}");
        assert!(rendered.contains("/ultrawork"), "{rendered}");
        assert!(rendered.contains("/ultraplan"), "{rendered}");
        assert!(rendered.contains("/goal"), "{rendered}");
        assert!(rendered.contains("thinking xhigh"), "{rendered}");
        assert!(!rendered.contains("[Fast]"), "{rendered}");
        assert!(!rendered.contains("ctx"), "{rendered}");
    }

    #[test]
    fn inline_startup_banner_full_layout_stays_left_aligned_when_limited() {
        let banner = InlineStartupBanner {
            provider: "openai".to_string(),
            model: "gpt-5.5".to_string(),
            fast_mode: false,
            directory: "D:\\own\\MyVault".to_string(),
            mode: "normal".to_string(),
            effort_display: "thinking xhigh".to_string(),
        };
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 140, 7));

        banner.render(buf.area, &mut buf);

        assert_eq!(buf.cell((0, 0)).expect("top-left cell").symbol(), "╭");
        let quick_action_x = 2 + InlineStartupBanner::MAX_FULL_LEFT_WIDTH + 3;
        assert_eq!(
            buf.cell((quick_action_x, 2))
                .expect("first quick action cell")
                .symbol(),
            "/"
        );
        assert_eq!(
            buf.cell((InlineStartupBanner::MAX_FULL_WIDTH, 0))
                .expect("cell after limited banner")
                .symbol(),
            " "
        );
    }

    #[test]
    fn inline_startup_banner_fast_on_places_fast_in_model_line() {
        let banner = InlineStartupBanner {
            provider: "openai".to_string(),
            model: "gpt-5.5".to_string(),
            fast_mode: true,
            directory: "D:\\own\\MyVault".to_string(),
            mode: "normal".to_string(),
            effort_display: String::new(),
        };
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, 7));

        banner.render(buf.area, &mut buf);
        let rendered = buffer_text(&buf);
        let model_row = rendered
            .lines()
            .find(|line| line.contains("gpt-5.5"))
            .expect("model row should render");

        assert!(model_row.contains("gpt-5.5 [Fast]"), "{rendered}");
        assert!(!rendered.contains("[Normal]"), "{rendered}");
    }

    #[test]
    fn inline_startup_banner_full_layout_model_value_matches_action_description_style() {
        let banner = InlineStartupBanner {
            provider: "openai".to_string(),
            model: "gpt-5.5".to_string(),
            fast_mode: false,
            directory: "D:\\own\\MyVault".to_string(),
            mode: "normal".to_string(),
            effort_display: String::new(),
        };
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, 7));

        banner.render(buf.area, &mut buf);

        let model_cell = cell_at_text(&buf, "gpt-5.5").expect("model value should render");
        let action_desc_cell = cell_at_text(&buf, "manager-lead agents")
            .expect("quick action description should render");
        let command_cell = cell_at_text(&buf, "/ceo").expect("command token should render");

        assert_eq!(model_cell.style(), action_desc_cell.style());
        assert_ne!(model_cell.style(), command_cell.style());
    }

    #[test]
    fn inline_startup_banner_compact_and_narrow_fallbacks_keep_core_details() {
        let fast_off = InlineStartupBanner {
            provider: "openai".to_string(),
            model: "gpt-5.5".to_string(),
            fast_mode: false,
            directory: "D:\\own\\MyVault".to_string(),
            mode: "normal".to_string(),
            effort_display: String::new(),
        };
        let fast_on = InlineStartupBanner {
            fast_mode: true,
            ..fast_off.clone()
        };
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 45, 5));

        fast_on.render(buf.area, &mut buf);
        let rendered = buffer_text(&buf);

        assert_eq!(InlineStartupBanner::height_for_width(20), 1);
        assert_eq!(InlineStartupBanner::height_for_width(45), 5);
        assert_eq!(InlineStartupBanner::height_for_width(80), 7);
        assert!(rendered.contains("model:"), "{rendered}");
        assert!(rendered.contains("provider:"), "{rendered}");
        assert!(rendered.contains("directory:"), "{rendered}");
        assert!(rendered.contains("[Fast]"), "{rendered}");
        assert!(!rendered.contains("Quick actions"), "{rendered}");
    }

    #[test]
    fn page_banner_reseed_places_banner_at_top_and_viewport_below_it() {
        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(4),
            },
        )
        .expect("inline terminal constructs");
        terminal
            .insert_before(10, |buf| {
                Paragraph::new(vec![Line::raw("old-page"); 10]).render(buf.area, buf);
            })
            .expect("seed old page");
        terminal
            .set_cursor_position(ratatui::layout::Position::ORIGIN)
            .expect("home cursor after clear");
        terminal
            .reseed_inline_viewport_after_clear()
            .expect("reseed cleared viewport");

        let banner = PageBanner::new("Agent: Beta");
        let prepared = prepare_inline_viewport_for_page_banner(&mut terminal, &banner)
            .expect("prepare page banner viewport");
        emit_inline_page_banner(&mut terminal, &banner).expect("emit page banner");

        assert_eq!(prepared, 18);
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 2, 80, 18));
        let top = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        assert!(top.starts_with("Agent: Beta"), "{top:?}");
    }

    #[test]
    fn page_banner_wraps_and_reports_height_in_narrow_terminal() {
        let banner = PageBanner::new("Agent: Beta Worker");
        assert_eq!(banner.height_for_width(8), 3);

        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 8, 3));
        banner.render(buf.area, &mut buf);
        let rendered = buffer_text(&buf);
        let reconstructed = rendered.lines().map(str::trim_end).collect::<String>();
        assert_eq!(reconstructed, "Agent: Beta Worker");

        let word_boundary = PageBanner::new("Agent: AAAAA AAAAA");
        assert_eq!(word_boundary.height_for_width(9), 2);
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 9, 2));
        word_boundary.render(buf.area, &mut buf);
        let reconstructed = buffer_text(&buf)
            .lines()
            .map(str::trim_end)
            .collect::<String>();
        assert_eq!(reconstructed, "Agent: AAAAA AAAAA");
    }
}

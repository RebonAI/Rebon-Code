//! Full-screen transcript pager used from inline (non-altscreen)
//! mode. Pauses the active prompt's surface and lets the user
//! scroll through the transcript with vim-style keys.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use rebon_tui::RenderTheme;
use tokio::runtime::Handle;

use crate::tui::app::AppState;
use crate::tui::permission_modal::PendingPermission;
use crate::tui::terminal::AltScreenOverlayGuard;
use crate::tui::wiring::TuiEngineSession;

use super::{drain_ui_channels, maybe_update_loading_state, ActivePrompt};

pub(super) fn maybe_update_loading_state_for_pager(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
) {
    let mut retry_after = Instant::now();
    maybe_update_loading_state(app, session, handle, active_prompt, &mut retry_after);
}

pub(super) fn clamp_pager_scroll(
    scroll_offset: usize,
    total_lines: usize,
    viewport_height: usize,
) -> usize {
    scroll_offset.min(total_lines.saturating_sub(viewport_height))
}

pub(super) fn initial_inline_pager_verbosity(
    current: rebon_tui::ToolOutputVerbosity,
) -> rebon_tui::ToolOutputVerbosity {
    match current {
        rebon_tui::ToolOutputVerbosity::Verbose => rebon_tui::ToolOutputVerbosity::Verbose,
        rebon_tui::ToolOutputVerbosity::Compact | rebon_tui::ToolOutputVerbosity::Normal => {
            rebon_tui::ToolOutputVerbosity::Verbose
        }
    }
}

pub(super) fn initial_inline_pager_scroll_offset() -> usize {
    usize::MAX
}

pub(super) fn render_inline_transcript_pager_frame(
    snapshot: &rebon_tui::AppState,
    area: Rect,
    buf: &mut ratatui::buffer::Buffer,
    theme: &RenderTheme,
    scroll_offset: usize,
    verbosity: rebon_tui::ToolOutputVerbosity,
    cache: &mut rebon_tui::TranscriptMeasureCache,
) -> (usize, usize) {
    let block = Block::default()
        .title(" Transcript pager ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.width == 0 || inner.height == 0 {
        return (0, 0);
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    let transcript_area = chunks[0];
    let help_area = chunks[1];

    let mut total_lines = 0usize;
    if transcript_area.height > 0 {
        let result = rebon_tui::render_transcript_cached_with_running_hints(
            snapshot,
            transcript_area,
            buf,
            theme,
            scroll_offset,
            verbosity,
            0,
            None,
            cache,
            false,
            rebon_tui::TranscriptRenderExtras {
                render_thinking_only_rows: true,
                force_verbose_edit_tool_previews: true,
                expand_thinking_rows: false,
                ..rebon_tui::TranscriptRenderExtras::empty()
            },
        );
        total_lines = result.total_lines;

        if snapshot.transcript.is_empty() && snapshot.overlay.is_empty() {
            Paragraph::new("No transcript yet")
                .style(Style::default().fg(Color::DarkGray))
                .render(transcript_area, buf);
            total_lines = total_lines.max(1);
        }
    }

    let help = Line::from(vec![
        Span::styled(
            "q/Esc/Ctrl+O to close",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  •  "),
        Span::raw("Ctrl+E toggle verbosity"),
        Span::raw("  •  "),
        Span::raw("↑/↓ j/k PgUp/PgDn scroll"),
    ]);
    Paragraph::new(help)
        .style(Style::default().fg(Color::DarkGray))
        .render(help_area, buf);

    (total_lines, transcript_area.height as usize)
}

pub(super) fn run_inline_transcript_pager(
    app: &mut AppState,
    theme: &mut RenderTheme,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    pending_permission: &mut Option<PendingPermission>,
) -> anyhow::Result<()> {
    let mut guard = AltScreenOverlayGuard::enter()?;
    let snapshot = app.rebon_tui.clone();
    let mut verbosity = initial_inline_pager_verbosity(app.tool_output_verbosity);
    let mut scroll_offset = initial_inline_pager_scroll_offset();
    let mut measure_cache = rebon_tui::TranscriptMeasureCache::new();
    let mut viewport_height = 0usize;
    let mut total_lines = 0usize;
    let mut needs_redraw = true;
    loop {
        drain_ui_channels(
            app,
            session,
            pending_permission,
            active_prompt,
        );
        maybe_update_loading_state_for_pager(app, session, handle, active_prompt);

        if needs_redraw {
            guard.terminal().draw(|frame| {
                let area = frame.area();
                (total_lines, viewport_height) = render_inline_transcript_pager_frame(
                    &snapshot,
                    area,
                    frame.buffer_mut(),
                    theme,
                    scroll_offset,
                    verbosity,
                    &mut measure_cache,
                );
            })?;
            scroll_offset = clamp_pager_scroll(scroll_offset, total_lines, viewport_height);
            needs_redraw = false;
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => break,
                KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    verbosity = match verbosity {
                        rebon_tui::ToolOutputVerbosity::Verbose => {
                            rebon_tui::ToolOutputVerbosity::Compact
                        }
                        _ => rebon_tui::ToolOutputVerbosity::Verbose,
                    };
                    needs_redraw = true;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    let next = scroll_offset.saturating_sub(1);
                    if next != scroll_offset {
                        scroll_offset = next;
                        needs_redraw = true;
                    }
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    let next = clamp_pager_scroll(
                        scroll_offset.saturating_add(1),
                        total_lines,
                        viewport_height,
                    );
                    if next != scroll_offset {
                        scroll_offset = next;
                        needs_redraw = true;
                    }
                }
                KeyCode::PageUp => {
                    let next = scroll_offset.saturating_sub(viewport_height.max(1));
                    if next != scroll_offset {
                        scroll_offset = next;
                        needs_redraw = true;
                    }
                }
                KeyCode::PageDown => {
                    let next = clamp_pager_scroll(
                        scroll_offset.saturating_add(viewport_height.max(1)),
                        total_lines,
                        viewport_height,
                    );
                    if next != scroll_offset {
                        scroll_offset = next;
                        needs_redraw = true;
                    }
                }
                _ => {}
            },
            Event::Resize(_, _) => {
                scroll_offset = clamp_pager_scroll(scroll_offset, total_lines, viewport_height);
                needs_redraw = true;
            }
            _ => {}
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use serde_json::json;

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

    #[test]
    fn inline_transcript_pager_empty_state_renders_chrome_and_help() {
        let snapshot = rebon_tui::AppState::new();
        let theme = RenderTheme::default();
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, 8));

        let (total_lines, viewport_height) = render_inline_transcript_pager_frame(
            &snapshot,
            buf.area,
            &mut buf,
            &theme,
            0,
            rebon_tui::ToolOutputVerbosity::Compact,
            &mut rebon_tui::TranscriptMeasureCache::new(),
        );
        let rendered = buffer_text(&buf);

        assert_eq!(viewport_height, 5);
        assert!(total_lines >= 1);
        assert!(rendered.contains("Transcript pager"), "{rendered}");
        assert!(rendered.contains("No transcript yet"), "{rendered}");
        assert!(rendered.contains("q/Esc/Ctrl+O to close"), "{rendered}");
        assert!(rendered.contains("Ctrl+E toggle verbosity"), "{rendered}");
        assert!(rendered.contains("PgUp/PgDn scroll"), "{rendered}");
    }

    #[test]
    fn inline_transcript_pager_opens_expanded() {
        assert_eq!(
            initial_inline_pager_verbosity(rebon_tui::ToolOutputVerbosity::Compact),
            rebon_tui::ToolOutputVerbosity::Verbose
        );
        assert_eq!(
            initial_inline_pager_verbosity(rebon_tui::ToolOutputVerbosity::Normal),
            rebon_tui::ToolOutputVerbosity::Verbose
        );
        assert_eq!(
            initial_inline_pager_verbosity(rebon_tui::ToolOutputVerbosity::Verbose),
            rebon_tui::ToolOutputVerbosity::Verbose
        );
    }

    #[test]
    fn inline_transcript_pager_initial_scroll_starts_at_bottom() {
        assert_eq!(initial_inline_pager_scroll_offset(), usize::MAX);
    }

    #[test]
    fn inline_transcript_pager_renders_thinking_only_rows() {
        let mut snapshot = rebon_tui::AppState::new();
        rebon_tui::reducer(
            &mut snapshot,
            rebon_tui::Action::Commit(rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
                uuid: "a-think".into(),
                timestamp: "t".into(),
                message: rebon_tui::AssistantMessageInner {
                    role: rebon_tui::AssistantRole::Assistant,
                    content: vec![rebon_tui::AssistantContentBlock::Thinking(
                        rebon_tui::AssistantThinkingBlock {
                            thinking: "pager visible reasoning".into(),
                            signature: None,
                        },
                    )],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            })),
        );
        let theme = RenderTheme::default();
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, 8));

        render_inline_transcript_pager_frame(
            &snapshot,
            buf.area,
            &mut buf,
            &theme,
            0,
            rebon_tui::ToolOutputVerbosity::Compact,
            &mut rebon_tui::TranscriptMeasureCache::new(),
        );
        let rendered = buffer_text(&buf);

        assert!(rendered.contains("pager visible reasoning"), "{rendered}");
    }

    #[test]
    fn inline_transcript_pager_compact_uses_thinking_headers() {
        let mut snapshot = rebon_tui::AppState::new();
        for (uuid, thinking) in [
            ("a-think-1", "first header\nfirst body"),
            ("a-think-2", "second header\nsecond body"),
        ] {
            rebon_tui::reducer(
                &mut snapshot,
                rebon_tui::Action::Commit(rebon_tui::Message::Assistant(
                    rebon_tui::AssistantMessage {
                        uuid: uuid.into(),
                        timestamp: "t".into(),
                        message: rebon_tui::AssistantMessageInner {
                            role: rebon_tui::AssistantRole::Assistant,
                            content: vec![rebon_tui::AssistantContentBlock::Thinking(
                                rebon_tui::AssistantThinkingBlock {
                                    thinking: thinking.into(),
                                    signature: None,
                                },
                            )],
                        },
                        is_api_error_message: None,
                        advisor_model: None,
                        is_stream_continuation: None,
                    },
                )),
            );
        }
        let theme = RenderTheme::default();
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, 16));

        render_inline_transcript_pager_frame(
            &snapshot,
            buf.area,
            &mut buf,
            &theme,
            0,
            rebon_tui::ToolOutputVerbosity::Compact,
            &mut rebon_tui::TranscriptMeasureCache::new(),
        );
        let rendered = buffer_text(&buf);

        assert!(rendered.contains("first header"), "{rendered}");
        assert!(rendered.contains("second header"), "{rendered}");
        assert!(!rendered.contains("first body"), "{rendered}");
        assert!(!rendered.contains("second body"), "{rendered}");
        assert!(!rendered.contains("∴ Thinking"), "{rendered}");
        assert!(rendered.contains("Ctrl+O to expand"), "{rendered}");
    }

    #[test]
    fn inline_transcript_pager_compact_keeps_read_search_collapsed() {
        let mut snapshot = rebon_tui::AppState::new();
        rebon_tui::reducer(
            &mut snapshot,
            rebon_tui::Action::Commit(rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
                uuid: "a-read".into(),
                timestamp: "t".into(),
                message: rebon_tui::AssistantMessageInner {
                    role: rebon_tui::AssistantRole::Assistant,
                    content: vec![rebon_tui::AssistantContentBlock::ToolUse(
                        rebon_tui::AssistantToolUseBlock {
                            id: "toolu-read".into(),
                            name: "Read".into(),
                            input: json!({ "file_path": "src/lib.rs" }),
                            tool_call_content: None,
                            raw_output: None,
                            title: None,
                            locations: None,
                            status: None,
                        },
                    )],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            })),
        );
        rebon_tui::reducer(
            &mut snapshot,
            rebon_tui::Action::Commit(rebon_tui::Message::User(rebon_tui::UserMessage {
                uuid: "u-results".into(),
                timestamp: "t".into(),
                message: rebon_tui::UserMessageInner {
                    role: rebon_tui::UserRole::User,
                    content: vec![rebon_tui::UserContentBlock::ToolResult(
                        rebon_tui::UserToolResultBlock {
                            tool_use_id: "toolu-read".into(),
                            content: rebon_tui::ToolResultContent::Text(
                                "file body should stay hidden".into(),
                            ),
                            is_error: Some(false),
                        },
                    )],
                },
                is_compact_summary: None,
                is_meta: None,
                is_visible_in_transcript_only: None,
                image_paste_ids: None,
                plan_content: None,
            })),
        );
        rebon_tui::reducer(
            &mut snapshot,
            rebon_tui::Action::Commit(rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
                uuid: "a-grep".into(),
                timestamp: "t".into(),
                message: rebon_tui::AssistantMessageInner {
                    role: rebon_tui::AssistantRole::Assistant,
                    content: vec![rebon_tui::AssistantContentBlock::ToolUse(
                        rebon_tui::AssistantToolUseBlock {
                            id: "toolu-grep".into(),
                            name: "Grep".into(),
                            input: json!({ "pattern": "needle" }),
                            tool_call_content: None,
                            raw_output: None,
                            title: None,
                            locations: None,
                            status: None,
                        },
                    )],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            })),
        );
        let theme = RenderTheme::default();
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 100, 10));

        render_inline_transcript_pager_frame(
            &snapshot,
            buf.area,
            &mut buf,
            &theme,
            0,
            rebon_tui::ToolOutputVerbosity::Compact,
            &mut rebon_tui::TranscriptMeasureCache::new(),
        );
        let rendered = buffer_text(&buf);

        assert!(
            rendered.contains("Searched") || rendered.contains("Read"),
            "{rendered}"
        );
        assert!(rendered.contains("Ctrl+O to expand"), "{rendered}");
        assert!(
            !rendered.contains("file body should stay hidden"),
            "{rendered}"
        );
        assert!(!rendered.contains("● Read"), "{rendered}");
        assert!(!rendered.contains("● Grep"), "{rendered}");
    }
}

//! TUI dialog for resuming a previous session (`/resume`).
//!
//! Lists on-disk sessions for the current cwd and lets the user pick
//! one. The selected session id is fed back to the runner which loads
//! the transcript and replays it into the TUI.

use std::cell::Cell;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use rebon_customselect::{
    NavigationAction, NavigationProps, NavigationState, OptionWithDescription,
};
use rebon_design_system::theme::get_active_theme;
use rebon_tui::parse_theme_color;

use crate::session::resume_listing::SessionEntry;
use crate::tui::dialog_support::format_relative_age;
use crate::tui::dialog_support::truncate_to_width;

const TITLE: &str = "Resume Session";
const PLACEHOLDER: &str = "type to filter...";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeMode {
    Summary,
    FullHistory,
}

#[derive(Debug, Clone)]
enum ResumeDialogPhase {
    /// Nothing to show yet. Which discovery stream we are waiting for
    /// lives in `active_load`, because a browse target leaves this phase
    /// on its first batch while the stream keeps running.
    Loading,
    Sessions,
    ModeChoice {
        entry: SessionEntry,
        selected: usize,
    },
    Preparing {
        generation: u64,
        entry: SessionEntry,
        mode: ResumeMode,
    },
    PrepareError {
        entry: SessionEntry,
        mode: ResumeMode,
        message: String,
    },
    Summarizing {
        generation: u64,
        entry: SessionEntry,
    },
    SummaryError {
        entry: SessionEntry,
        message: String,
    },
    LoadError {
        generation: u64,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResumeLoadRequest {
    pub generation: u64,
    pub exact_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResumePrepareRequest {
    pub generation: u64,
    pub entry: SessionEntry,
    pub mode: ResumeMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResumeSummaryRequest {
    pub generation: u64,
    pub entry: SessionEntry,
}

/// Result of handling a key inside the resume dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeDialogOutcome {
    None,
    UseFullHistory,
    CancelResume,
    CopySessionId(String),
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResumeDialogTarget {
    Browse,
    Exact(String),
    Latest,
}

/// Cloneable render + reducer state for the resume dialog.
#[derive(Debug, Clone)]
pub struct ResumeDialogState {
    pub query: String,
    target: ResumeDialogTarget,
    phase: ResumeDialogPhase,
    pending_load: Option<ResumeLoadRequest>,
    pending_prepare: Option<ResumePrepareRequest>,
    pending_summary: Option<ResumeSummaryRequest>,
    next_prepare_generation: u64,
    next_summary_generation: u64,
    /// Generation of the discovery stream still feeding `entries`. Batches
    /// from any other generation belong to a superseded load and are
    /// dropped; `None` means discovery finished (or never started).
    active_load: Option<u64>,
    entries: Vec<SessionEntry>,
    filtered: Vec<SessionEntry>,
    nav: Option<NavigationState<String>>,
    visible_option_count: Cell<Option<usize>>,
}

impl ResumeDialogState {
    pub fn open() -> Self {
        Self::open_target(ResumeDialogTarget::Browse)
    }

    pub fn open_exact(session_id: String) -> Self {
        Self::open_target(ResumeDialogTarget::Exact(session_id))
    }

    pub fn open_latest() -> Self {
        Self::open_target(ResumeDialogTarget::Latest)
    }

    fn open_target(target: ResumeDialogTarget) -> Self {
        let generation = 1;
        let exact_session_id = match &target {
            ResumeDialogTarget::Exact(session_id) => Some(session_id.clone()),
            ResumeDialogTarget::Browse | ResumeDialogTarget::Latest => None,
        };
        Self {
            query: String::new(),
            target,
            phase: ResumeDialogPhase::Loading,
            pending_load: Some(ResumeLoadRequest {
                generation,
                exact_session_id,
            }),
            pending_prepare: None,
            pending_summary: None,
            next_prepare_generation: 1,
            next_summary_generation: 1,
            active_load: Some(generation),
            entries: Vec::new(),
            filtered: Vec::new(),
            nav: None,
            visible_option_count: Cell::new(None),
        }
    }

    pub fn is_startup_resume(&self) -> bool {
        !matches!(self.target, ResumeDialogTarget::Browse)
    }

    pub(crate) fn maybe_take_load_request(&mut self) -> Option<ResumeLoadRequest> {
        self.pending_load.take()
    }

    pub(crate) fn begin_prepare(&mut self, entry: SessionEntry, mode: ResumeMode) {
        let generation = self.next_prepare_generation;
        self.next_prepare_generation = self.next_prepare_generation.saturating_add(1);
        self.phase = ResumeDialogPhase::Preparing {
            generation,
            entry: entry.clone(),
            mode,
        };
        self.pending_prepare = Some(ResumePrepareRequest {
            generation,
            entry,
            mode,
        });
    }

    pub(crate) fn maybe_take_prepare_request(&mut self) -> Option<ResumePrepareRequest> {
        self.pending_prepare.take()
    }

    pub(crate) fn apply_prepare_success(&mut self, generation: u64) {
        let ResumeDialogPhase::Preparing {
            generation: expected,
            entry,
            ..
        } = self.phase.clone()
        else {
            return;
        };
        if generation == expected {
            self.phase = ResumeDialogPhase::ModeChoice { entry, selected: 0 };
        }
    }

    fn begin_summary(&mut self, entry: SessionEntry) {
        let generation = self.next_summary_generation;
        self.next_summary_generation = self.next_summary_generation.saturating_add(1);
        self.phase = ResumeDialogPhase::Summarizing {
            generation,
            entry: entry.clone(),
        };
        self.pending_summary = Some(ResumeSummaryRequest { generation, entry });
    }

    pub(crate) fn maybe_take_summary_request(&mut self) -> Option<ResumeSummaryRequest> {
        self.pending_summary.take()
    }

    pub(crate) fn accepts_summary_success(&self, generation: u64) -> bool {
        matches!(
            &self.phase,
            ResumeDialogPhase::Summarizing {
                generation: expected,
                ..
            } if generation == *expected
        )
    }

    pub(crate) fn apply_summary_failure(&mut self, generation: u64, message: String) {
        let ResumeDialogPhase::Summarizing {
            generation: expected,
            entry,
        } = self.phase.clone()
        else {
            return;
        };
        if generation == expected {
            self.phase = ResumeDialogPhase::SummaryError { entry, message };
        }
    }

    pub fn is_prompt_replacement(&self) -> bool {
        matches!(
            &self.phase,
            ResumeDialogPhase::ModeChoice { .. }
                | ResumeDialogPhase::Summarizing { .. }
                | ResumeDialogPhase::SummaryError { .. }
        )
    }

    pub fn desired_height(&self) -> u16 {
        if self.is_prompt_replacement() {
            10
        } else {
            30
        }
    }

    pub(crate) fn apply_prepare_failure(&mut self, generation: u64, message: String) {
        let ResumeDialogPhase::Preparing {
            generation: expected,
            entry,
            mode,
        } = self.phase.clone()
        else {
            return;
        };
        if generation == expected {
            self.phase = ResumeDialogPhase::PrepareError {
                entry,
                mode,
                message,
            };
        }
    }

    /// Whether discovery is still shipping batches into the list.
    pub(crate) fn is_loading_more(&self) -> bool {
        self.active_load.is_some()
    }

    /// Fold one discovery batch into the list.
    ///
    /// Browse shows its first batch immediately and grows underneath it,
    /// so the picker is usable while the tail of a long history is still
    /// being hydrated. The startup targets keep their all-or-nothing
    /// semantics: `Exact` and `Latest` both pick out of the *complete*
    /// set, so they only decide once `complete` lands.
    pub(crate) fn apply_load_chunk(
        &mut self,
        generation: u64,
        entries: Vec<SessionEntry>,
        complete: bool,
    ) {
        if self.active_load != Some(generation) {
            return;
        }
        if complete {
            self.active_load = None;
        }
        self.entries.extend(entries);
        self.entries.sort_by(|a, b| {
            b.created_at_ms
                .cmp(&a.created_at_ms)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });

        match self.target.clone() {
            ResumeDialogTarget::Browse => {
                if matches!(self.phase, ResumeDialogPhase::Loading) {
                    self.phase = ResumeDialogPhase::Sessions;
                }
                if matches!(self.phase, ResumeDialogPhase::Sessions) {
                    // `refresh` re-derives the filter and re-anchors the
                    // navigation on the focused session id, so appending a
                    // batch never moves the user's selection.
                    self.refresh();
                }
            }
            ResumeDialogTarget::Exact(session_id) => {
                if !complete {
                    return;
                }
                if let Some(entry) = self
                    .entries
                    .iter()
                    .find(|entry| entry.session_id == session_id)
                    .cloned()
                {
                    self.begin_prepare(entry, ResumeMode::FullHistory);
                } else {
                    self.phase = ResumeDialogPhase::LoadError {
                        generation,
                        message: format!("Session {session_id} was not found or is still active."),
                    };
                }
            }
            ResumeDialogTarget::Latest => {
                if !complete {
                    return;
                }
                if let Some(entry) = self.entries.first().cloned() {
                    self.begin_prepare(entry, ResumeMode::FullHistory);
                } else {
                    self.phase = ResumeDialogPhase::LoadError {
                        generation,
                        message: "No stopped session found to continue.".to_string(),
                    };
                }
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn apply_load_results(&mut self, generation: u64, entries: Vec<SessionEntry>) {
        self.apply_load_chunk(generation, entries, true);
    }

    pub(crate) fn apply_load_failure(&mut self, generation: u64, message: String) {
        if self.active_load != Some(generation) {
            return;
        }
        self.active_load = None;
        // A failure after the first batch already painted keeps the rows
        // the user can see; only a load that produced nothing turns into
        // the retryable error screen.
        if matches!(self.phase, ResumeDialogPhase::Loading) {
            self.phase = ResumeDialogPhase::LoadError {
                generation,
                message,
            };
        }
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> ResumeDialogOutcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ResumeDialogOutcome::None;
        }
        match self.phase.clone() {
            ResumeDialogPhase::Loading => {
                return if key.code == KeyCode::Esc {
                    ResumeDialogOutcome::Close
                } else {
                    ResumeDialogOutcome::None
                };
            }
            ResumeDialogPhase::Preparing { .. } => {
                return if key.code == KeyCode::Esc {
                    ResumeDialogOutcome::Close
                } else {
                    ResumeDialogOutcome::None
                };
            }
            ResumeDialogPhase::PrepareError {
                entry,
                mode,
                message: _,
            } => {
                return match key.code {
                    KeyCode::Esc => ResumeDialogOutcome::Close,
                    KeyCode::Enter => {
                        self.begin_prepare(entry, mode);
                        ResumeDialogOutcome::None
                    }
                    _ => ResumeDialogOutcome::None,
                };
            }
            ResumeDialogPhase::Summarizing { .. } => {
                return if key.code == KeyCode::Esc {
                    ResumeDialogOutcome::CancelResume
                } else {
                    ResumeDialogOutcome::None
                };
            }
            ResumeDialogPhase::SummaryError { entry, .. } => {
                return match key.code {
                    KeyCode::Esc => ResumeDialogOutcome::CancelResume,
                    KeyCode::Enter => {
                        self.begin_summary(entry);
                        ResumeDialogOutcome::None
                    }
                    KeyCode::Char('f') => ResumeDialogOutcome::UseFullHistory,
                    _ => ResumeDialogOutcome::None,
                };
            }
            ResumeDialogPhase::LoadError { generation, .. } => {
                return match key.code {
                    KeyCode::Esc => ResumeDialogOutcome::Close,
                    KeyCode::Enter => {
                        let generation = generation.saturating_add(1);
                        self.phase = ResumeDialogPhase::Loading;
                        let exact_session_id = match &self.target {
                            ResumeDialogTarget::Exact(session_id) => Some(session_id.clone()),
                            ResumeDialogTarget::Browse | ResumeDialogTarget::Latest => None,
                        };
                        // A retry restarts discovery from scratch, so drop
                        // whatever the failed stream had already delivered.
                        self.entries.clear();
                        self.filtered.clear();
                        self.nav = None;
                        self.active_load = Some(generation);
                        self.pending_load = Some(ResumeLoadRequest {
                            generation,
                            exact_session_id,
                        });
                        ResumeDialogOutcome::None
                    }
                    _ => ResumeDialogOutcome::None,
                };
            }
            ResumeDialogPhase::ModeChoice { entry, selected } => {
                return match key.code {
                    KeyCode::Esc => ResumeDialogOutcome::CancelResume,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        ResumeDialogOutcome::CopySessionId(entry.session_id)
                    }
                    KeyCode::Up => {
                        let selected = if selected == 0 { 2 } else { selected - 1 };
                        self.phase = ResumeDialogPhase::ModeChoice { entry, selected };
                        ResumeDialogOutcome::None
                    }
                    KeyCode::Down => {
                        self.phase = ResumeDialogPhase::ModeChoice {
                            entry,
                            selected: (selected + 1) % 3,
                        };
                        ResumeDialogOutcome::None
                    }
                    KeyCode::Enter => match selected {
                        0 => {
                            self.begin_summary(entry);
                            ResumeDialogOutcome::None
                        }
                        1 => ResumeDialogOutcome::UseFullHistory,
                        _ => ResumeDialogOutcome::CancelResume,
                    },
                    _ => ResumeDialogOutcome::None,
                };
            }
            ResumeDialogPhase::Sessions => {}
        }

        self.ensure_nav_visible_option_count();
        match key.code {
            KeyCode::Esc => ResumeDialogOutcome::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self
                .selected_entry()
                .map(|e| ResumeDialogOutcome::CopySessionId(e.session_id.clone()))
                .unwrap_or(ResumeDialogOutcome::None),
            KeyCode::Enter => {
                if let Some(entry) = self.selected_entry().cloned() {
                    self.begin_prepare(entry, ResumeMode::FullHistory);
                }
                ResumeDialogOutcome::None
            }
            KeyCode::Up => {
                self.scroll_up();
                ResumeDialogOutcome::None
            }
            KeyCode::Down => {
                self.scroll_down();
                ResumeDialogOutcome::None
            }
            KeyCode::PageUp => {
                self.dispatch_nav(NavigationAction::FocusPreviousPage);
                ResumeDialogOutcome::None
            }
            KeyCode::PageDown => {
                self.dispatch_nav(NavigationAction::FocusNextPage);
                ResumeDialogOutcome::None
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.refresh();
                ResumeDialogOutcome::None
            }
            KeyCode::Delete => {
                self.query.clear();
                self.refresh();
                ResumeDialogOutcome::None
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
            {
                self.query.push(ch);
                self.refresh();
                ResumeDialogOutcome::None
            }
            _ => ResumeDialogOutcome::None,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let ds = get_active_theme();
        let border = Style::default().fg(parse_theme_color(ds.subtle));
        let title_style = Style::default()
            .fg(parse_theme_color(ds.text))
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(parse_theme_color(ds.inactive));
        let selected = Style::default()
            .fg(parse_theme_color(ds.suggestion))
            .add_modifier(Modifier::BOLD);
        let normal = Style::default().fg(parse_theme_color(ds.text));

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .title(Span::styled(format!(" {TITLE} "), title_style));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        match &self.phase {
            ResumeDialogPhase::Loading => {
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(" Loading sessions…", normal),
                        Span::styled("  Esc ", selected),
                        Span::styled("cancel", dim),
                    ])),
                    inner,
                );
                return;
            }
            ResumeDialogPhase::ModeChoice {
                entry,
                selected: choice,
            } => {
                self.render_mode_choice(frame, inner, entry, *choice, selected, normal, dim);
                return;
            }
            ResumeDialogPhase::Preparing { entry, .. } => {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(Span::styled(
                            format!(
                                " {} · {}",
                                entry.title,
                                format_jsonl_size(entry.jsonl_bytes)
                            ),
                            normal,
                        )),
                        Line::from(Span::styled(" Opening session…", normal)),
                        Line::from(vec![
                            Span::styled(" Esc ", selected),
                            Span::styled("cancel", dim),
                        ]),
                    ]),
                    inner,
                );
                return;
            }
            ResumeDialogPhase::PrepareError { message, .. } => {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(Span::styled(format!(" {message}"), normal)),
                        Line::from(vec![
                            Span::styled(" Enter ", selected),
                            Span::styled("retry", dim),
                            Span::styled(" · Esc ", selected),
                            Span::styled("cancel", dim),
                        ]),
                    ]),
                    inner,
                );
                return;
            }
            ResumeDialogPhase::Summarizing { entry, .. } => {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(Span::styled(
                            format!(
                                " {} · {}",
                                entry.title,
                                format_jsonl_size(entry.jsonl_bytes)
                            ),
                            normal,
                        )),
                        Line::from(Span::styled(" Generating resume summary…", normal)),
                        Line::from(vec![
                            Span::styled(" Esc ", selected),
                            Span::styled("cancel resume and start new", dim),
                        ]),
                    ]),
                    inner,
                );
                return;
            }
            ResumeDialogPhase::SummaryError { message, .. } => {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(Span::styled(format!(" {message}"), normal)),
                        Line::from(vec![
                            Span::styled(" Enter ", selected),
                            Span::styled("retry", dim),
                            Span::styled(" · F ", selected),
                            Span::styled("full history", dim),
                            Span::styled(" · Esc ", selected),
                            Span::styled("new session", dim),
                        ]),
                    ]),
                    inner,
                );
                return;
            }
            ResumeDialogPhase::LoadError { message, .. } => {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(Span::styled(format!(" {message}"), normal)),
                        Line::from(vec![
                            Span::styled(" Enter ", selected),
                            Span::styled("retry", dim),
                            Span::styled(" · Esc ", selected),
                            Span::styled("close", dim),
                        ]),
                    ]),
                    inner,
                );
                return;
            }
            ResumeDialogPhase::Sessions => {}
        }

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(4),
                Constraint::Length(1),
            ])
            .split(inner);

        // Query line.
        let query_line = if self.query.is_empty() {
            Line::from(vec![
                Span::styled(" Filter: ", dim),
                Span::styled(PLACEHOLDER, dim),
            ])
        } else {
            Line::from(vec![
                Span::styled(" Filter: ", dim),
                Span::styled(self.query.clone(), normal),
            ])
        };
        frame.render_widget(Paragraph::new(query_line), sections[0]);

        // Session list.
        self.render_list(frame, sections[1], selected, normal, dim);

        // Footer.
        let count = self.filtered.len();
        let total = self.entries.len();
        let mut footer = vec![
            Span::styled(" Enter ", selected),
            Span::styled(self.enter_verb(), dim),
            Span::styled(" · Esc ", selected),
            Span::styled("close", dim),
            Span::styled(" · Ctrl+C ", selected),
            Span::styled("copy id", dim),
            Span::styled(format!("  ({count}/{total} sessions)"), dim),
        ];
        if self.is_loading_more() {
            footer.push(Span::styled(" loading more…", dim));
        }
        let footer = Line::from(footer);
        frame.render_widget(Paragraph::new(footer), sections[2]);
    }

    fn render_mode_choice(
        &self,
        frame: &mut Frame,
        area: Rect,
        entry: &SessionEntry,
        selected_index: usize,
        selected_style: Style,
        normal: Style,
        dim: Style,
    ) {
        let options = [
            (
                "Resume with summary",
                "Generate a fresh handoff summary before the next model turn.",
            ),
            (
                "Resume with full history",
                "Use canonical history subject to the model context limit.",
            ),
            ("Cancel resume", "Start a new empty session."),
        ];
        let mut lines = vec![
            Line::from(Span::styled(
                format!(
                    " {} · {} · {}",
                    entry.title,
                    format_relative_age(entry.created_at_ms),
                    format_jsonl_size(entry.jsonl_bytes)
                ),
                normal,
            )),
            Line::from(Span::styled(format!(" Session {}", entry.session_id), dim)),
            Line::from(""),
        ];
        for (index, (label, description)) in options.iter().enumerate() {
            let is_selected = index == selected_index;
            let style = if is_selected { selected_style } else { normal };
            let prefix = if is_selected { ">" } else { " " };
            lines.push(Line::from(vec![
                Span::styled(format!(" {prefix} {label}"), style),
                Span::styled(format!("  {description}"), dim),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" Enter ", selected_style),
            Span::styled("select", dim),
            Span::styled(" · Esc ", selected_style),
            Span::styled("cancel", dim),
            Span::styled(" · Ctrl+C ", selected_style),
            Span::styled("copy id", dim),
        ]));
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn refresh(&mut self) {
        let query_lower = self.query.to_lowercase();
        self.filtered = if query_lower.is_empty() {
            self.entries.clone()
        } else {
            self.entries
                .iter()
                .filter(|e| {
                    e.title.to_lowercase().contains(&query_lower)
                        || e.session_id.to_lowercase().contains(&query_lower)
                })
                .cloned()
                .collect()
        };

        let focus = self.selected_entry().map(|e| e.session_id.clone());
        if self.filtered.is_empty() {
            self.nav = None;
            return;
        }
        let options = self
            .filtered
            .iter()
            .map(|e| OptionWithDescription::text(build_row_label(e), e.session_id.clone()))
            .collect();
        self.nav = Some(NavigationState::new(NavigationProps {
            visible_option_count: Some(self.visible_option_count()),
            options,
            initial_focus_value: None,
            focus_value: focus,
        }));
    }

    pub fn scroll_up(&mut self) {
        self.dispatch_nav(NavigationAction::FocusPreviousOption);
    }

    pub fn scroll_down(&mut self) {
        self.dispatch_nav(NavigationAction::FocusNextOption);
    }

    fn visible_option_count(&self) -> usize {
        self.visible_option_count.get().unwrap_or(20).max(1)
    }

    fn ensure_nav_visible_option_count(&mut self) {
        let desired = self.visible_option_count();
        let needs_reset = self
            .nav
            .as_ref()
            .is_some_and(|nav| nav.visible_option_count() != desired);
        if needs_reset {
            let focus = self
                .nav
                .as_ref()
                .and_then(NavigationState::validated_focused_value);
            self.reset_nav(focus);
        }
    }

    fn reset_nav(&mut self, focus: Option<String>) {
        if self.filtered.is_empty() {
            self.nav = None;
            return;
        }
        let options = self
            .filtered
            .iter()
            .map(|e| OptionWithDescription::text(build_row_label(e), e.session_id.clone()))
            .collect();
        self.nav = Some(NavigationState::new(NavigationProps {
            visible_option_count: Some(self.visible_option_count()),
            options,
            initial_focus_value: None,
            focus_value: focus,
        }));
    }

    fn selected_entry(&self) -> Option<&SessionEntry> {
        let focused = self
            .nav
            .as_ref()
            .and_then(NavigationState::validated_focused_value)?;
        self.filtered.iter().find(|e| e.session_id == focused)
    }

    fn dispatch_nav(&mut self, action: NavigationAction<String>) {
        self.ensure_nav_visible_option_count();
        rebon_dialog::model::dispatch_nav(&mut self.nav, action);
    }

    /// What Enter does to the row under the cursor. A session a live
    /// worker holds is joined — the picker hands it to `rebon attach`'s
    /// path — and everything else is resumed here.
    fn enter_verb(&self) -> &'static str {
        if self.selected_entry().is_some_and(|entry| entry.joinable) {
            "join"
        } else {
            "resume"
        }
    }

    fn render_list(
        &self,
        frame: &mut Frame,
        area: Rect,
        selected: Style,
        normal: Style,
        dim: Style,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let block = Block::default().borders(Borders::ALL).border_style(dim);
        let inner = block.inner(area);
        self.visible_option_count
            .set(Some(inner.height.max(1) as usize));
        frame.render_widget(block, area);

        if self.filtered.is_empty() {
            let message = if self.entries.is_empty() {
                "No previous sessions found."
            } else {
                "No matching sessions."
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(message, dim))),
                inner,
            );
            return;
        }

        let focused = self
            .nav
            .as_ref()
            .and_then(NavigationState::validated_focused_value);
        let rows = self
            .nav
            .as_ref()
            .map(NavigationState::visible_options)
            .unwrap_or_default();
        for (idx, row) in rows.into_iter().enumerate() {
            if idx as u16 >= inner.height {
                break;
            }
            let session_id = row.option.value();
            let Some(entry) = self
                .filtered
                .iter()
                .find(|entry| &entry.session_id == session_id)
            else {
                continue;
            };
            let is_focused = focused.as_ref() == Some(session_id);
            let style = if is_focused { selected } else { normal };
            let prefix = if is_focused { ">" } else { " " };
            let label = build_row_label(entry);
            let line = Line::from(vec![
                Span::styled(format!("{prefix} "), style),
                Span::styled(
                    truncate_to_width(&label, inner.width.saturating_sub(2) as usize),
                    style,
                ),
            ]);
            let row_area = Rect::new(inner.x, inner.y + idx as u16, inner.width, 1);
            frame.render_widget(Paragraph::new(line), row_area);
        }
    }
}

fn build_row_label(entry: &SessionEntry) -> String {
    // The badge sits ahead of the title so a long title cannot truncate
    // it off the row: which sessions are live is what the user scans for.
    let badge = if entry.joinable {
        "active · joinable  "
    } else {
        ""
    };
    format!(
        "{:>6}  {:>9}  {badge}{}",
        format_relative_age(entry.created_at_ms),
        format_jsonl_size(entry.jsonl_bytes),
        entry.title
    )
}

fn format_jsonl_size(bytes: Option<u64>) -> String {
    let Some(bytes) = bytes else {
        return "—".to_string();
    };
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * KIB;
    const GIB: f64 = 1024.0 * MIB;
    let bytes_f = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes_f < MIB {
        format!("{:.1} KiB", bytes_f / KIB)
    } else if bytes_f < GIB {
        format!("{:.1} MiB", bytes_f / MIB)
    } else {
        format!("{:.1} GiB", bytes_f / GIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_runtime_fields() -> rebon_session_host::BackgroundRuntimeFields {
        rebon_session_host::BackgroundRuntimeFields {
            provider: None,
            model: None,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            permission_mode: None,
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        }
    }
    use crate::session::resume_listing::{discover_entries, discover_exact_entry};
    use crate::session::resume_resolution::{
        resolve_resume_transcript_cwd, ResumeTranscriptResolution,
    };

    fn entry(id: &str, age_ms: u64, title: &str) -> SessionEntry {
        SessionEntry {
            session_id: id.into(),
            transcript_cwd: "/repo".into(),
            title: title.into(),
            created_at_ms: age_ms,
            jsonl_bytes: Some(1024),
            joinable: false,
        }
    }

    fn joinable_entry(id: &str, age_ms: u64, title: &str) -> SessionEntry {
        SessionEntry {
            joinable: true,
            ..entry(id, age_ms, title)
        }
    }

    #[test]
    fn build_row_label_format() {
        let e = entry("sess-0-100", 100, "hello world");
        let label = build_row_label(&e);
        assert!(!label.contains("sess-0-100"));
        assert!(label.contains("1.0 KiB"));
        assert!(label.contains("hello world"));
    }

    #[test]
    fn format_jsonl_size_uses_iec_units() {
        assert_eq!(format_jsonl_size(None), "—");
        assert_eq!(format_jsonl_size(Some(0)), "0 B");
        assert_eq!(format_jsonl_size(Some(1023)), "1023 B");
        assert_eq!(format_jsonl_size(Some(1024)), "1.0 KiB");
        assert_eq!(format_jsonl_size(Some(1024 * 1024)), "1.0 MiB");
        assert_eq!(format_jsonl_size(Some(1024 * 1024 * 1024)), "1.0 GiB");
    }

    #[test]
    fn browse_paints_first_batch_before_discovery_finishes() {
        let mut dialog = ResumeDialogState::open();
        let request = dialog.maybe_take_load_request().unwrap();

        dialog.apply_load_chunk(
            request.generation,
            vec![entry("newest", 300, "first page")],
            false,
        );
        // The list is live and selectable while the tail is still loading.
        assert!(dialog.is_loading_more());
        let prepare = open_selected_session(&mut dialog);
        assert_eq!(prepare.entry.session_id, "newest");
    }

    #[test]
    fn later_batches_append_without_moving_the_selection() {
        let mut dialog = ResumeDialogState::open();
        let request = dialog.maybe_take_load_request().unwrap();
        dialog.apply_load_chunk(
            request.generation,
            vec![entry("newest", 300, "a"), entry("middle", 200, "b")],
            false,
        );
        dialog.scroll_down();
        assert_eq!(dialog.selected_entry().unwrap().session_id, "middle");

        // A batch that sorts *above* the focused row still must not steal
        // the selection out from under the user.
        dialog.apply_load_chunk(
            request.generation,
            vec![
                entry("newer-straggler", 400, "c"),
                entry("oldest", 100, "d"),
            ],
            true,
        );

        assert!(!dialog.is_loading_more());
        assert_eq!(dialog.selected_entry().unwrap().session_id, "middle");
        assert_eq!(
            dialog
                .filtered
                .iter()
                .map(|entry| entry.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["newer-straggler", "newest", "middle", "oldest"]
        );
    }

    #[test]
    fn latest_target_waits_for_the_complete_stream_before_choosing() {
        let mut dialog = ResumeDialogState::open_latest();
        let request = dialog.maybe_take_load_request().unwrap();

        // A partial batch must not commit: a newer session can still
        // arrive in a later batch.
        dialog.apply_load_chunk(request.generation, vec![entry("older", 100, "old")], false);
        assert!(dialog.maybe_take_prepare_request().is_none());

        dialog.apply_load_chunk(request.generation, vec![entry("newer", 200, "new")], true);
        let prepare = dialog.maybe_take_prepare_request().unwrap();
        assert_eq!(prepare.entry.session_id, "newer");
    }

    #[test]
    fn batches_from_a_superseded_load_are_ignored() {
        let mut dialog = ResumeDialogState::open();
        let request = dialog.maybe_take_load_request().unwrap();
        dialog.apply_load_chunk(
            request.generation.saturating_add(7),
            vec![entry("stale", 100, "stale")],
            true,
        );

        assert!(dialog.entries.is_empty());
        assert!(dialog.is_loading_more());
    }

    #[test]
    fn discovery_failure_after_a_painted_batch_keeps_the_visible_rows() {
        let mut dialog = ResumeDialogState::open();
        let request = dialog.maybe_take_load_request().unwrap();
        dialog.apply_load_chunk(request.generation, vec![entry("kept", 100, "kept")], false);

        dialog.apply_load_failure(request.generation, "disk went away".to_string());

        assert!(!dialog.is_loading_more());
        assert!(matches!(dialog.phase, ResumeDialogPhase::Sessions));
        assert_eq!(dialog.selected_entry().unwrap().session_id, "kept");
    }

    #[test]
    fn loading_dialog_only_accepts_cancel() {
        let mut dialog = ResumeDialogState::open();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(dialog.handle_key(&enter), ResumeDialogOutcome::None);
        assert_eq!(dialog.handle_key(&esc), ResumeDialogOutcome::Close);
    }

    fn open_selected_session(dialog: &mut ResumeDialogState) -> ResumePrepareRequest {
        assert_eq!(
            dialog.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ResumeDialogOutcome::None
        );
        dialog.maybe_take_prepare_request().unwrap()
    }

    fn finish_opening(dialog: &mut ResumeDialogState, request: &ResumePrepareRequest) {
        dialog.apply_prepare_success(request.generation);
        assert!(dialog.is_prompt_replacement());
    }

    #[test]
    fn session_selection_opens_locally_before_summary_choice() {
        let mut dialog = state_with_entries(1);
        let request = open_selected_session(&mut dialog);
        assert_eq!(request.entry, entry("sess-0", 1, "title 0"));
        assert_eq!(request.mode, ResumeMode::FullHistory);

        finish_opening(&mut dialog, &request);
        assert_eq!(
            dialog.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ResumeDialogOutcome::None
        );
        let summary = dialog.maybe_take_summary_request().unwrap();
        assert_eq!(summary.entry, request.entry);
    }

    #[test]
    fn mode_choice_supports_full_history_and_new_session_cancel() {
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let mut full = state_with_entries(1);
        let request = open_selected_session(&mut full);
        finish_opening(&mut full, &request);
        full.handle_key(&down);
        assert_eq!(full.handle_key(&enter), ResumeDialogOutcome::UseFullHistory);

        let mut cancel = state_with_entries(1);
        let request = open_selected_session(&mut cancel);
        finish_opening(&mut cancel, &request);
        cancel.handle_key(&down);
        cancel.handle_key(&down);
        assert_eq!(cancel.handle_key(&enter), ResumeDialogOutcome::CancelResume);
        assert_eq!(
            cancel.handle_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ResumeDialogOutcome::CancelResume
        );
    }

    #[test]
    fn exact_and_latest_targets_prepare_session_immediately_after_loading() {
        let entries = vec![entry("older", 1, "old"), entry("newer", 2, "new")];

        let mut exact = ResumeDialogState::open_exact("older".to_string());
        let request = exact.maybe_take_load_request().unwrap();
        assert_eq!(request.exact_session_id.as_deref(), Some("older"));
        exact.apply_load_results(request.generation, entries.clone());
        let prepare = exact.maybe_take_prepare_request().unwrap();
        assert_eq!(prepare.entry, entry("older", 1, "old"));
        assert_eq!(prepare.mode, ResumeMode::FullHistory);

        let mut latest = ResumeDialogState::open_latest();
        let request = latest.maybe_take_load_request().unwrap();
        assert_eq!(request.exact_session_id, None);
        latest.apply_load_results(request.generation, entries);
        let prepare = latest.maybe_take_prepare_request().unwrap();
        assert_eq!(prepare.entry, entry("newer", 2, "new"));
        assert_eq!(prepare.mode, ResumeMode::FullHistory);
    }

    fn state_with_entries(count: usize) -> ResumeDialogState {
        let entries = (0..count)
            .map(|n| entry(&format!("sess-{n}"), n as u64 + 1, &format!("title {n}")))
            .collect::<Vec<_>>();
        let mut state = ResumeDialogState {
            query: String::new(),
            target: ResumeDialogTarget::Browse,
            phase: ResumeDialogPhase::Sessions,
            pending_load: None,
            pending_prepare: None,
            pending_summary: None,
            next_prepare_generation: 1,
            next_summary_generation: 1,
            active_load: None,
            entries: entries.clone(),
            filtered: entries,
            nav: None,
            visible_option_count: Cell::new(None),
        };
        state.refresh();
        state
    }

    #[test]
    fn exact_discovery_finds_session_outside_current_cwd() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "exact-cross-cwd";
        let path =
            rebon_session::ensure_session_file_path(root.path(), "/other", session_id).unwrap();
        std::fs::write(
            path,
            serde_json::json!({
                "type": "user",
                "uuid": "u1",
                "message": {"role": "user", "content": "cross cwd"}
            })
            .to_string(),
        )
        .unwrap();

        let entries = discover_exact_entry(
            root.path(),
            "/current",
            &rebon_acp::ServerState::new(),
            session_id,
        )
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].transcript_cwd, "/other");
        assert_eq!(entries[0].session_id, session_id);
    }

    /// RFC-0004 §8.1: an active session used to vanish from the picker,
    /// which after hosted-by-default hid every session with a worker still
    /// up — including the one the user just closed a terminal on. It is
    /// listed now, and says it is joined rather than resumed.
    #[test]
    fn open_lists_an_active_session_a_live_worker_holds() {
        let root = tempfile::tempdir().unwrap();
        let projects_root = root.path().join("projects");
        let cwd = "/repo/joinable";
        let session_id = "sess-hosted-live";
        let transcript =
            rebon_session::ensure_session_file_path(&projects_root, cwd, session_id).unwrap();
        std::fs::write(transcript, b"{}\n").unwrap();

        let store = rebon_session_host::BackgroundStore::new(root.path());
        let mut job = store
            .create_job("hosted prompt".into(), cwd.into(), test_runtime_fields())
            .unwrap();
        job.identity.session_id = Some(session_id.into());
        job.process.status = rebon_session_host::BackgroundJobStatus::Idle;
        job.process.pid = Some(std::process::id());
        store.write_state(&job).unwrap();

        let _worker_lock =
            rebon_session::try_acquire_session_active_lock(&projects_root, cwd, session_id)
                .unwrap()
                .unwrap();

        let entries = discover_entries(
            &projects_root,
            cwd,
            &rebon_acp::ServerState::new(),
            "current",
        )
        .unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.session_id == session_id)
            .expect("a session a live worker holds is joinable, not hidden");
        assert!(entry.joinable);
        assert!(build_row_label(entry).contains("active · joinable"));
    }

    #[test]
    fn enter_joins_a_live_row_and_resumes_a_stopped_one() {
        let mut dialog = ResumeDialogState::open();
        let request = dialog.maybe_take_load_request().unwrap();
        dialog.apply_load_results(
            request.generation,
            vec![
                joinable_entry("live", 200, "hosted work"),
                entry("stopped", 100, "yesterday"),
            ],
        );
        assert_eq!(dialog.enter_verb(), "join");
        dialog.scroll_down();
        assert_eq!(dialog.enter_verb(), "resume");
    }

    #[test]
    fn resume_prefers_newer_origin_transcript_that_supersedes_worktree_copy() {
        let root = tempfile::tempdir().unwrap();
        let projects_root = root.path().join("projects");
        let cwd = "/repo/project";
        let worktree_cwd = "/repo/project/.rebon/worktrees/old";
        let session_id = "sess-migrated-worktree";
        let source =
            rebon_session::ensure_session_file_path(&projects_root, worktree_cwd, session_id)
                .unwrap();
        std::fs::write(&source, b"first\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let target =
            rebon_session::ensure_session_file_path(&projects_root, cwd, session_id).unwrap();
        std::fs::write(&target, b"first\nsecond\n").unwrap();

        assert_eq!(
            resolve_resume_transcript_cwd(&projects_root, cwd, session_id),
            ResumeTranscriptResolution::Unique(cwd.into())
        );

        let _active_lock = rebon_session::try_acquire_session_active_lock(
            &projects_root,
            worktree_cwd,
            session_id,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            resolve_resume_transcript_cwd(&projects_root, cwd, session_id),
            ResumeTranscriptResolution::Ambiguous
        );
    }

    #[test]
    fn keyboard_navigation_uses_rendered_list_height() {
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::layout::Rect;
        use ratatui::Terminal;

        let mut state = state_with_entries(8);
        let backend = TestBackend::new(80, 9);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| state.render(frame, Rect::new(0, 0, 80, 9)))
            .unwrap();

        assert_eq!(state.visible_option_count.get(), Some(3));
        for _ in 0..3 {
            state.handle_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let nav = state.nav.as_ref().unwrap();
        assert_eq!(nav.focused_index(), 4);
        assert_eq!(nav.visible_from_index(), 1);
    }
}

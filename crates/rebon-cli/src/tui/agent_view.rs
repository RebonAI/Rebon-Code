use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use rebon_plugin_tasks::runtime::{TaskData, TaskKind, TaskSnapshot, TaskStatus};
use rebon_width::{truncate_to_ellipsis, WidthStr};
use std::time::{Duration, Instant};

use crate::background::{
    BackgroundIpcEndpoint, BackgroundJobState, BackgroundJobStatus, BackgroundPullRequestDotStatus,
    BackgroundPullRequestStatus, BackgroundStore,
};
use rebon_plugin_tasks::ui::tasks_view;
#[cfg(test)]
use rebon_session_host::BackgroundPermissionOptionSnapshot;
use rebon_session_host::BackgroundPermissionQuerySnapshot;

const COMPLETED_GROUP_VISIBLE_LIMIT: usize = 5;
const AGENT_VIEW_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentViewGroup {
    Pinned,
    ReadyForReview,
    NeedsInput,
    Working,
    Idle,
    Completed,
    Failed,
    Stopped,
}

impl AgentViewGroup {
    fn label(self) -> &'static str {
        match self {
            Self::Pinned => "Pinned",
            Self::ReadyForReview => "Ready for review",
            Self::NeedsInput => "Needs input",
            Self::Working => "Working",
            Self::Idle => "Idle",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Stopped => "Stopped",
        }
    }

    fn state_filter(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::ReadyForReview => "ready-for-review",
            Self::NeedsInput => "needs-input",
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    /// Glyph + color for the leading state icon, per Agent Views spec:
    /// shape `✻` / `✽` (alive), `∙` (exited), `✢` (looping); color by
    /// status. The TUI rendering is static so the "animated `✽`" form
    /// becomes a plain `✽` glyph — animation is left to a future tick.
    fn state_icon(self) -> (&'static str, Style) {
        match self {
            // Pinned rows can be in any underlying state; the asterisk
            // marker still distinguishes them, so reuse the working
            // glyph in cyan to convey "watching".
            Self::Pinned => ("✻", Style::default().fg(Color::Cyan)),
            // PR ready to review — process likely exited but waiting on
            // human action. Green dot matches the PR-pass status.
            Self::ReadyForReview => ("∙", Style::default().fg(Color::Green)),
            Self::NeedsInput => ("✻", Style::default().fg(Color::Yellow)),
            Self::Working => ("✽", Style::default().fg(Color::Cyan)),
            Self::Idle => (
                "✻",
                Style::default().fg(Color::Gray).add_modifier(Modifier::DIM),
            ),
            Self::Completed => ("∙", Style::default().fg(Color::Green)),
            Self::Failed => ("∙", Style::default().fg(Color::Red)),
            Self::Stopped => ("∙", Style::default().fg(Color::DarkGray)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentViewProcessShape {
    Alive,
    Active,
    Exited,
}

impl AgentViewProcessShape {
    fn glyph(self) -> &'static str {
        match self {
            Self::Alive => "✻",
            Self::Active => "✽",
            Self::Exited => "∙",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentViewRowSource {
    Job,
    Task,
}

#[derive(Debug, Clone)]
pub struct AgentViewRow {
    pub id: String,
    pub source: AgentViewRowSource,
    pub group: AgentViewGroup,
    /// A `Foreground` job is a session some terminal, window or tab drove
    /// — the hosted default makes one for every `rebon`.
    /// While somebody is on it, it is their open window, not a background
    /// task, and is not listed; one whose worker runs for nobody is listed
    /// under "Sessions" (§19.9). Task rows are `Background`.
    pub placement: rebon_session_host::JobPlacement,
    pub name: String,
    pub detail: String,
    pub agent_name: Option<String>,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub pinned: bool,
    pub order_key: i64,
    pub peek_lines: Vec<String>,
    pub pending_permission: Option<BackgroundPermissionQuerySnapshot>,
    pub process_shape: AgentViewProcessShape,
    /// Trailing pull-request status dot, if the session has an open PR.
    /// Rendered right-aligned per Agent Views spec.
    pub pr_status: Option<PullRequestDotStatus>,
    /// Pull request count — when > 1 the count is rendered before the
    /// dot. Always 0 when `pr_status` is `None`.
    pub pr_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentViewInputMode {
    Dispatch,
    Filter,
    ReplyJob(String),
    ReplyTask(String),
    RenameJob(String),
}

impl AgentViewInputMode {
    fn status(&self) -> &'static str {
        match self {
            Self::Dispatch => "dispatch prompt, Enter to start background job",
            Self::Filter => "filter sessions by a:<agent>, s:<state>, #<number>, or PR URL",
            Self::ReplyJob(_) => "reply to selected background job (prefix `!` for Bash)",
            Self::ReplyTask(_) => {
                "reply to selected in-process agent (`/interrupt <text>` to redirect)"
            }
            Self::RenameJob(_) => "rename selected background job",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentViewState {
    pub rows: Vec<AgentViewRow>,
    pub selected_index: usize,
    selected_visual_index: usize,
    row_scroll_offset: usize,
    pub filter: String,
    input: String,
    input_cursor: usize,
    input_images: Vec<rebon_types::PromptPasteContent>,
    next_paste_id: u32,
    input_focused: bool,
    input_mode: AgentViewInputMode,
    peek_visible: bool,
    pub status: Option<String>,
    cwd_scope: Option<String>,
    grouping_mode: AgentViewGrouping,
    /// The job this terminal's own session lives in, if it lives in one.
    /// Its row is never listed — this list is on that session's screen —
    /// and the id keeps it out during the seconds before the session's
    /// own lease has landed.
    this_terminal_job_id: Option<String>,
    collapsed_groups: Vec<VisualGroup>,
    expanded_limited_groups: Vec<VisualGroup>,
    pending_remove_job: Option<(String, Instant)>,
    pending_remove_group: Option<(VisualGroup, Instant)>,
    last_render_area: Option<Rect>,
    /// Cached subagent name list for the dispatch-input Tab picker.
    /// Populated lazily on the first Tab; reset to empty when the
    /// dispatch input is closed.
    agent_picker_names: Vec<String>,
    /// Current cycle position in `agent_picker_names`. `None` means
    /// "next Tab starts at index 0".
    agent_picker_index: Option<usize>,
    /// True while the empty-dispatch Tab browser is visible.
    agent_picker_visible: bool,
    last_refresh_at: Instant,
}

/// Where a dispatched agent runs, and who watches it.
///
/// Three genuinely different answers, so they are three variants rather
/// than a pair of booleans that can spell nonsense:
/// - `Detached` (Enter): a worker runs it, the view tracks it, nobody
///   mirrors it. Survives the terminal closing.
/// - `Here` (Shift+Enter): this process runs it, with a job record so it
///   shows up in the view and can be handed over later. Starts instantly;
///   dies with the terminal.
/// - `Hosted` (Ctrl+N): a worker runs it and this TUI mirrors it. Costs a
///   worker start, and then behaves like a session you can walk away from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchPlacement {
    Detached,
    Here,
    Hosted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentViewKeyOutcome {
    Consumed,
    ReturnToSession,
    Dismiss,
    CancelOrExit,
    GroupingChanged(AgentViewGroupingPreference),
    FilterChanged,
    DispatchPrompt(String),
    DispatchPromptWithImages {
        prompt: String,
        images: Vec<rebon_types::PromptPasteContent>,
    },
    DispatchPromptAndAttach(String),
    DispatchPromptAndAttachWithImages {
        prompt: String,
        images: Vec<rebon_types::PromptPasteContent>,
    },
    DispatchPromptHosted(String),
    DispatchPromptHostedWithImages {
        prompt: String,
        images: Vec<rebon_types::PromptPasteContent>,
    },
    EditInputExternally {
        text: String,
    },
    ReplyToTask {
        task_id: String,
        message: String,
    },
    ReplyToJob {
        job_id: String,
        message: String,
    },
    /// Reply prefixed with `!` — interpret the remainder as a Bash command
    /// to run inside the target background session.
    BashCommandToJob {
        job_id: String,
        command: String,
    },
    /// Reply prefixed with `!` — interpret the remainder as a Bash command
    /// to run inside the target in-process teammate task.
    BashCommandToTask {
        task_id: String,
        command: String,
    },
    AnswerJobPermission {
        job_id: String,
        query_id: u64,
        turn_generation: u64,
        endpoint: Option<BackgroundIpcEndpoint>,
        option_id: String,
    },
    AnswerTaskChoice {
        task_id: String,
        option_index: usize,
    },
    RenameJob {
        job_id: String,
        name: String,
    },
    TogglePinJob {
        job_id: String,
    },
    MoveJobUp {
        job_id: String,
    },
    MoveJobDown {
        job_id: String,
    },
    OpenTask {
        task_id: String,
    },
    AttachJob {
        job_id: String,
    },
    WarmPeekJob {
        job_id: String,
    },
    RefreshPeek,
    RemoveGroup {
        group_label: String,
        job_ids: Vec<String>,
    },
    StopJob {
        job_id: String,
    },
    StopTask {
        task_id: String,
    },
    RespawnJob {
        job_id: String,
    },
    RemoveJob {
        job_id: String,
    },
    RemoveTask {
        task_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentViewGroupingPreference {
    State,
    Directory,
}

impl AgentViewGroupingPreference {
    pub fn from_config(value: &str) -> Self {
        match value.trim() {
            "directory" => Self::Directory,
            _ => Self::State,
        }
    }

    pub fn as_config(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Directory => "directory",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentViewGrouping {
    State,
    Directory,
}

impl From<AgentViewGroupingPreference> for AgentViewGrouping {
    fn from(value: AgentViewGroupingPreference) -> Self {
        match value {
            AgentViewGroupingPreference::State => Self::State,
            AgentViewGroupingPreference::Directory => Self::Directory,
        }
    }
}

impl AgentViewGrouping {
    fn as_preference(self) -> AgentViewGroupingPreference {
        match self {
            Self::State => AgentViewGroupingPreference::State,
            Self::Directory => AgentViewGroupingPreference::Directory,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VisualGroup {
    /// The foreground sessions nobody is on, kept apart from the agents
    /// and shown first: a worker staying up for nobody is exactly the
    /// row a user has to be able to see. Sessions being
    /// driven — this terminal's included — are those people's open
    /// windows, not background tasks, and are not listed (§8.1).
    Sessions,
    State(AgentViewGroup),
    Directory(String),
}

impl VisualGroup {
    fn label(&self) -> String {
        match self {
            Self::Sessions => "Sessions".to_string(),
            Self::State(group) => group.label().to_string(),
            Self::Directory(cwd) if cwd.is_empty() => "(no cwd)".to_string(),
            Self::Directory(cwd) => format!("Directory: {cwd}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VisualItem {
    Group(VisualGroup),
    Row(usize),
    More { group: VisualGroup, hidden: usize },
}

impl AgentViewState {
    #[cfg(test)]
    pub fn open(
        store: &BackgroundStore,
        jobs: Vec<BackgroundJobState>,
        tasks: &[TaskSnapshot],
    ) -> Self {
        Self::open_with_grouping(
            store,
            jobs,
            tasks,
            None,
            AgentViewGroupingPreference::State,
            None,
        )
    }

    #[cfg(test)]
    pub fn open_with_cwd_scope(
        store: &BackgroundStore,
        jobs: Vec<BackgroundJobState>,
        tasks: &[TaskSnapshot],
        cwd_scope: Option<String>,
    ) -> Self {
        Self::open_with_grouping(
            store,
            jobs,
            tasks,
            cwd_scope,
            AgentViewGroupingPreference::State,
            None,
        )
    }

    /// Open the list. `this_terminal_job_id` is the job the session on
    /// screen lives in, when it lives in one; that job is left out.
    pub fn open_with_grouping(
        store: &BackgroundStore,
        jobs: Vec<BackgroundJobState>,
        tasks: &[TaskSnapshot],
        cwd_scope: Option<String>,
        grouping: AgentViewGroupingPreference,
        this_terminal_job_id: Option<String>,
    ) -> Self {
        let rows = build_rows(
            filter_jobs_by_cwd(jobs, cwd_scope.as_deref()),
            tasks,
            "",
            &RowContext::now(this_terminal_job_id.as_deref()),
        );
        let status = cwd_scope
            .as_deref()
            .map(|cwd| format!("showing background jobs in {cwd}"));
        let mut this = Self {
            rows,
            selected_index: 0,
            selected_visual_index: 0,
            row_scroll_offset: 0,
            filter: String::new(),
            input: String::new(),
            input_cursor: 0,
            input_images: Vec::new(),
            next_paste_id: 1,
            input_focused: true,
            input_mode: AgentViewInputMode::Dispatch,
            peek_visible: false,
            status,
            cwd_scope,
            grouping_mode: grouping.into(),
            this_terminal_job_id,
            collapsed_groups: Vec::new(),
            expanded_limited_groups: Vec::new(),
            pending_remove_job: None,
            pending_remove_group: None,
            last_render_area: None,
            agent_picker_names: Vec::new(),
            agent_picker_index: None,
            agent_picker_visible: false,
            last_refresh_at: Instant::now(),
        };
        this.sort_rows_for_grouping();
        this.selected_visual_index = this.first_row_visual_index().unwrap_or(0);
        this.clamp_selection();
        this.refresh_selected_job_peek(store);
        this
    }

    #[cfg(test)]
    pub fn set_render_area_for_test(&mut self, area: Rect) {
        self.last_render_area = Some(area);
    }

    /// Select the task-backed row with the given coordinator task id. Returns
    /// false when no such row exists. Test-only: production selection is driven
    /// by keyboard/mouse navigation.
    #[cfg(test)]
    pub fn select_task_row_for_test(&mut self, task_id: &str) -> bool {
        if let Some(index) = self
            .rows
            .iter()
            .position(|row| row.source == AgentViewRowSource::Task && row.id == task_id)
        {
            self.select_row_index(index);
            true
        } else {
            false
        }
    }

    pub(crate) fn refresh_is_due(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_refresh_at) >= AGENT_VIEW_REFRESH_INTERVAL
    }

    pub fn refresh(
        &mut self,
        store: &BackgroundStore,
        jobs: Vec<BackgroundJobState>,
        tasks: &[TaskSnapshot],
    ) {
        self.last_refresh_at = Instant::now();
        let selected_id = self.selected_row().map(|row| (row.source, row.id.clone()));
        let selected_group = self.selected_visual_item().and_then(|item| match item {
            VisualItem::Group(group) | VisualItem::More { group, .. } => Some(group),
            VisualItem::Row(_) => None,
        });
        let filtered_jobs = filter_jobs_by_cwd(jobs, self.cwd_scope.as_deref());
        self.rows = build_rows(
            filtered_jobs,
            tasks,
            &self.filter,
            &RowContext::now(self.this_terminal_job_id.as_deref()),
        );
        self.sort_rows_for_grouping();
        if let Some((source, id)) = selected_id {
            if let Some(index) = self
                .rows
                .iter()
                .position(|row| row.source == source && row.id == id)
            {
                self.selected_index = index;
                self.selected_visual_index = self.visual_index_for_row(index).unwrap_or(0);
            }
        } else if let Some(group) = selected_group {
            self.selected_visual_index = self.visual_index_for_group(group).unwrap_or(0);
        }
        self.clamp_selection();
        self.refresh_selected_job_peek(store);
    }

    fn refresh_selected_job_peek(&mut self, store: &BackgroundStore) {
        if !self.peek_visible {
            return;
        }
        let Some(VisualItem::Row(index)) = self.selected_visual_item() else {
            return;
        };
        let Some(row) = self.rows.get(index) else {
            return;
        };
        if row.source != AgentViewRowSource::Job {
            return;
        }
        let job_id = row.id.clone();
        let peek_lines = store.read_peek_lines(&job_id, 10);
        if let Some(row) = self.rows.get_mut(index) {
            row.peek_lines.extend(peek_lines);
        }
    }

    pub fn grouping_preference(&self) -> AgentViewGroupingPreference {
        self.grouping_mode.as_preference()
    }

    pub fn selected_row(&self) -> Option<&AgentViewRow> {
        match self.selected_visual_item()? {
            VisualItem::Row(index) => self.rows.get(index),
            VisualItem::Group(_) | VisualItem::More { .. } => None,
        }
    }

    pub fn selected_dispatch_cwd(&self) -> Option<&str> {
        if self.grouping_mode == AgentViewGrouping::Directory {
            match self.selected_visual_item()? {
                VisualItem::Group(VisualGroup::Directory(cwd))
                | VisualItem::More {
                    group: VisualGroup::Directory(cwd),
                    ..
                } => {
                    let cwd = cwd.trim();
                    if cwd.is_empty() {
                        return None;
                    }
                    return self.rows.iter().find_map(|row| {
                        let row_cwd = row.cwd.as_deref()?;
                        (row_cwd == cwd).then_some(row_cwd)
                    });
                }
                VisualItem::Row(index) => return self.rows.get(index)?.cwd.as_deref(),
                _ => {}
            }
        }
        self.selected_row().and_then(|row| row.cwd.as_deref())
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> AgentViewKeyOutcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return AgentViewKeyOutcome::Consumed;
        }
        if self.input_focused {
            return self.handle_input_key(key);
        }
        self.handle_table_key(key)
    }

    fn handle_table_key(&mut self, key: &KeyEvent) -> AgentViewKeyOutcome {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.clear_input_or_cancel_session()
            }
            KeyCode::Esc => {
                if self.peek_visible {
                    self.peek_visible = false;
                    self.status = Some("peek panel hidden".to_string());
                    AgentViewKeyOutcome::Consumed
                } else {
                    AgentViewKeyOutcome::Dismiss
                }
            }
            KeyCode::Up if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.select_previous();
                AgentViewKeyOutcome::Consumed
            }
            KeyCode::Down if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.select_next();
                AgentViewKeyOutcome::Consumed
            }
            KeyCode::Char(ch)
                if key.modifiers.contains(KeyModifiers::ALT) && ch.is_ascii_digit() =>
            {
                self.open_numbered_row_in_current_group(ch)
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_grouping_mode();
                AgentViewKeyOutcome::GroupingChanged(self.grouping_preference())
            }
            KeyCode::Char(' ') => {
                self.peek_visible = !self.peek_visible;
                self.status = Some(if self.peek_visible {
                    "peek panel shown".to_string()
                } else {
                    "peek panel hidden".to_string()
                });
                if self.peek_visible {
                    if let Some(row) = self.selected_row() {
                        if row.source == AgentViewRowSource::Job {
                            if row.process_shape == AgentViewProcessShape::Exited
                                && row.group != AgentViewGroup::Stopped
                            {
                                return AgentViewKeyOutcome::WarmPeekJob {
                                    job_id: row.id.clone(),
                                };
                            }
                            return AgentViewKeyOutcome::RefreshPeek;
                        }
                    }
                }
                AgentViewKeyOutcome::Consumed
            }
            KeyCode::Char(ch) if ch.is_ascii_digit() && key.modifiers.is_empty() => {
                self.answer_selected_multiple_choice(ch)
            }
            KeyCode::Char('?') => {
                self.status = Some("keys: ↑/↓ move · Enter attach/open · → return · Space peek · d dispatch · r reply · Ctrl+S group · Ctrl+T pin · Ctrl+R rename · Ctrl+X stop/rm · ? help".to_string());
                AgentViewKeyOutcome::Consumed
            }
            KeyCode::Char('/') => {
                self.filter.clear();
                self.start_filter_input();
                AgentViewKeyOutcome::FilterChanged
            }
            KeyCode::Char('d') | KeyCode::Char('n') => {
                self.input_focused = true;
                self.input_mode = AgentViewInputMode::Dispatch;
                self.input.clear();
                self.input_images.clear();
                self.next_paste_id = 1;
                self.input_cursor = 0;
                self.status = Some(self.input_mode.status().to_string());
                AgentViewKeyOutcome::Consumed
            }
            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let Some(row) = self.selected_row() else {
                    return AgentViewKeyOutcome::Consumed;
                };
                match row.source {
                    AgentViewRowSource::Job => AgentViewKeyOutcome::TogglePinJob {
                        job_id: row.id.clone(),
                    },
                    AgentViewRowSource::Task => {
                        self.status =
                            Some("pin is only implemented for job-backed rows".to_string());
                        AgentViewKeyOutcome::Consumed
                    }
                }
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let selected = self
                    .selected_row()
                    .map(|row| (row.source, row.id.clone(), row.name.clone()));
                let Some((source, id, name)) = selected else {
                    return AgentViewKeyOutcome::Consumed;
                };
                if source == AgentViewRowSource::Job {
                    self.input_focused = true;
                    self.input_mode = AgentViewInputMode::RenameJob(id);
                    self.input.clear();
                    self.input_images.clear();
                    self.next_paste_id = 1;
                    self.input.push_str(&name);
                    self.input_cursor = self.input.len();
                    self.status = Some(self.input_mode.status().to_string());
                } else {
                    self.status =
                        Some("rename is only implemented for job-backed rows".to_string());
                }
                AgentViewKeyOutcome::Consumed
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                let Some(row) = self.selected_row() else {
                    return AgentViewKeyOutcome::Consumed;
                };
                match row.source {
                    AgentViewRowSource::Job => AgentViewKeyOutcome::MoveJobUp {
                        job_id: row.id.clone(),
                    },
                    AgentViewRowSource::Task => {
                        self.status =
                            Some("reorder is only implemented for job-backed rows".to_string());
                        AgentViewKeyOutcome::Consumed
                    }
                }
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                let Some(row) = self.selected_row() else {
                    return AgentViewKeyOutcome::Consumed;
                };
                match row.source {
                    AgentViewRowSource::Job => AgentViewKeyOutcome::MoveJobDown {
                        job_id: row.id.clone(),
                    },
                    AgentViewRowSource::Task => {
                        self.status =
                            Some("reorder is only implemented for job-backed rows".to_string());
                        AgentViewKeyOutcome::Consumed
                    }
                }
            }
            KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(group) = self.selected_visual_item().and_then(|item| match item {
                    VisualItem::Group(group) => Some(group),
                    VisualItem::Row(_) | VisualItem::More { .. } => None,
                }) {
                    return self.confirm_remove_group_outcome(group);
                }
                let Some(row) = self.selected_row() else {
                    return AgentViewKeyOutcome::Consumed;
                };
                match row.source {
                    AgentViewRowSource::Job => self.stop_or_remove_job_outcome(row.id.clone()),
                    AgentViewRowSource::Task => self.stop_or_remove_task_outcome(row.id.clone()),
                }
            }
            KeyCode::Char('R') => {
                let Some(row) = self.selected_row() else {
                    return AgentViewKeyOutcome::Consumed;
                };
                match row.source {
                    AgentViewRowSource::Job => AgentViewKeyOutcome::RespawnJob {
                        job_id: row.id.clone(),
                    },
                    AgentViewRowSource::Task => {
                        self.status =
                            Some("respawn is only implemented for job-backed rows".to_string());
                        AgentViewKeyOutcome::Consumed
                    }
                }
            }
            KeyCode::Delete => self.confirm_remove_selected_job_outcome(),
            KeyCode::Char('r') => {
                let Some(source) = self.selected_row().map(|row| row.source) else {
                    return AgentViewKeyOutcome::Consumed;
                };
                self.input_focused = true;
                self.input_mode = match source {
                    AgentViewRowSource::Job => {
                        let job_id = self
                            .selected_row()
                            .map(|row| row.id.clone())
                            .unwrap_or_default();
                        AgentViewInputMode::ReplyJob(job_id)
                    }
                    AgentViewRowSource::Task => {
                        let task_id = self
                            .selected_row()
                            .map(|row| row.id.clone())
                            .unwrap_or_default();
                        AgentViewInputMode::ReplyTask(task_id)
                    }
                };
                self.input.clear();
                self.input_images.clear();
                self.next_paste_id = 1;
                self.input_cursor = 0;
                self.status = Some(self.input_mode.status().to_string());
                AgentViewKeyOutcome::Consumed
            }
            KeyCode::Enter => self.open_selected_outcome(),
            KeyCode::Right => AgentViewKeyOutcome::ReturnToSession,
            KeyCode::Backspace if self.filter.is_empty() => {
                self.confirm_remove_selected_job_outcome()
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.clamp_selection();
                AgentViewKeyOutcome::Consumed
            }
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.filter.push(ch);
                self.clamp_selection();
                AgentViewKeyOutcome::Consumed
            }
            _ => AgentViewKeyOutcome::Consumed,
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> AgentViewKeyOutcome {
        let Some(area) = self.last_render_area else {
            return AgentViewKeyOutcome::Consumed;
        };
        if !rect_contains(area, mouse.column, mouse.row) {
            return AgentViewKeyOutcome::Consumed;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = self.visual_item_index_at(area, mouse.column, mouse.row) {
                    self.selected_visual_index = index;
                    self.clamp_selection();
                    self.open_selected_outcome()
                } else {
                    AgentViewKeyOutcome::Consumed
                }
            }
            MouseEventKind::ScrollUp => {
                self.select_previous();
                AgentViewKeyOutcome::Consumed
            }
            MouseEventKind::ScrollDown => {
                self.select_next();
                AgentViewKeyOutcome::Consumed
            }
            _ => AgentViewKeyOutcome::Consumed,
        }
    }

    fn open_selected_outcome(&mut self) -> AgentViewKeyOutcome {
        match self.selected_visual_item() {
            Some(VisualItem::Group(group)) => {
                self.toggle_group(group);
                AgentViewKeyOutcome::Consumed
            }
            Some(VisualItem::More { group, .. }) => {
                self.expand_limited_group(group);
                AgentViewKeyOutcome::Consumed
            }
            Some(VisualItem::Row(index)) => {
                let Some(row) = self.rows.get(index) else {
                    return AgentViewKeyOutcome::Consumed;
                };
                match row.source {
                    AgentViewRowSource::Task => AgentViewKeyOutcome::OpenTask {
                        task_id: row.id.clone(),
                    },
                    AgentViewRowSource::Job => AgentViewKeyOutcome::AttachJob {
                        job_id: row.id.clone(),
                    },
                }
            }
            None => AgentViewKeyOutcome::Consumed,
        }
    }

    fn visual_item_index_at(&self, area: Rect, column: u16, row_y: u16) -> Option<usize> {
        let rows_area = self.rows_area(area)?;
        if !rect_contains(rows_area, column, row_y) {
            return None;
        }
        let relative_y = row_y.saturating_sub(rows_area.y) as usize;
        let target_line = self.row_scroll_offset.saturating_add(relative_y);
        let items = self.visual_items();
        let mut visual_row = 0usize;
        let mut previous_was_group = false;
        for (idx, item) in items.iter().enumerate() {
            if idx > 0 && matches!(item, VisualItem::Group(_)) && !previous_was_group {
                visual_row += 1;
            }
            previous_was_group = matches!(item, VisualItem::Group(_));
            if target_line == visual_row {
                return Some(idx);
            }
            visual_row += 1;
        }
        None
    }

    fn selected_visual_item(&self) -> Option<VisualItem> {
        self.visual_items().get(self.selected_visual_index).cloned()
    }

    fn visual_items(&self) -> Vec<VisualItem> {
        let mut items = Vec::new();
        let mut last_group: Option<VisualGroup> = None;
        for row in self.rows.iter() {
            let group = self.visual_group_for_row(row);
            if last_group.as_ref() == Some(&group) {
                continue;
            }
            items.push(VisualItem::Group(group.clone()));
            last_group = Some(group.clone());
            if self.is_group_collapsed(&group) {
                continue;
            }
            let (visible, hidden) = self.visible_row_indices_for_group(&group);
            items.extend(visible.into_iter().map(VisualItem::Row));
            if hidden > 0 {
                items.push(VisualItem::More { group, hidden });
            }
        }
        items
    }

    fn visual_index_for_row(&self, row_index: usize) -> Option<usize> {
        self.visual_items()
            .iter()
            .position(|item| *item == VisualItem::Row(row_index))
    }

    fn visual_index_for_group(&self, group: VisualGroup) -> Option<usize> {
        self.visual_items()
            .iter()
            .position(|item| *item == VisualItem::Group(group.clone()))
    }

    fn select_row_index(&mut self, row_index: usize) {
        if row_index >= self.rows.len() {
            return;
        }
        let group = self.visual_group_for_row(&self.rows[row_index]);
        self.collapsed_groups
            .retain(|collapsed| collapsed != &group);
        if self.is_group_limited(&group) && !self.expanded_limited_groups.contains(&group) {
            self.expanded_limited_groups.push(group);
        }
        self.selected_index = row_index;
        self.selected_visual_index = self.visual_index_for_row(row_index).unwrap_or(0);
        self.clamp_selection();
    }

    fn first_row_visual_index(&self) -> Option<usize> {
        self.visual_items()
            .iter()
            .position(|item| matches!(item, VisualItem::Row(_)))
    }

    fn is_group_collapsed(&self, group: &VisualGroup) -> bool {
        self.collapsed_groups.contains(group)
    }

    fn is_group_limited(&self, group: &VisualGroup) -> bool {
        !self.collapsed_groups.contains(group)
            && !self.expanded_limited_groups.contains(group)
            && matches!(group, VisualGroup::State(AgentViewGroup::Completed))
            && self.filter.trim().is_empty()
    }

    fn visible_row_indices_for_group(&self, group: &VisualGroup) -> (Vec<usize>, usize) {
        let indices = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| (self.visual_group_for_row(row) == *group).then_some(index))
            .collect::<Vec<_>>();
        if self.is_group_limited(group) && indices.len() > COMPLETED_GROUP_VISIBLE_LIMIT {
            let hidden = indices.len() - COMPLETED_GROUP_VISIBLE_LIMIT;
            (
                indices
                    .into_iter()
                    .take(COMPLETED_GROUP_VISIBLE_LIMIT)
                    .collect(),
                hidden,
            )
        } else {
            (indices, 0)
        }
    }

    fn expand_limited_group(&mut self, group: VisualGroup) {
        if !self.expanded_limited_groups.contains(&group) {
            self.expanded_limited_groups.push(group.clone());
        }
        self.status = Some(format!("showing all rows in {}", group.label()));
        self.selected_visual_index = self.visual_index_for_group(group).unwrap_or(0);
        self.clamp_selection();
    }

    fn toggle_group(&mut self, group: VisualGroup) {
        if let Some(index) = self
            .collapsed_groups
            .iter()
            .position(|collapsed| *collapsed == group)
        {
            self.collapsed_groups.remove(index);
            self.status = Some(format!("expanded {}", group.label()));
        } else {
            self.collapsed_groups.push(group.clone());
            self.status = Some(format!("collapsed {}", group.label()));
        }
        self.selected_visual_index = self.visual_index_for_group(group).unwrap_or(0);
        self.clamp_selection();
    }

    fn open_numbered_row_in_current_group(&mut self, ch: char) -> AgentViewKeyOutcome {
        let Some(number) = ch.to_digit(10).filter(|number| *number > 0) else {
            return AgentViewKeyOutcome::Consumed;
        };
        let Some(group) = self.current_group_for_numbered_shortcut() else {
            return AgentViewKeyOutcome::Consumed;
        };
        let (visible, _) = self.visible_row_indices_for_group(&group);
        let Some(row_index) = visible.into_iter().nth(number as usize - 1) else {
            self.status = Some(format!("no row {number} in {}", group.label()));
            return AgentViewKeyOutcome::Consumed;
        };
        self.selected_visual_index = self.visual_index_for_row(row_index).unwrap_or(0);
        self.clamp_selection();
        self.open_selected_outcome()
    }

    fn current_group_for_numbered_shortcut(&self) -> Option<VisualGroup> {
        match self.selected_visual_item()? {
            VisualItem::Group(group) => Some(group),
            VisualItem::More { group, .. } => Some(group),
            VisualItem::Row(index) => self
                .rows
                .get(index)
                .map(|row| self.visual_group_for_row(row)),
        }
    }

    fn answer_selected_multiple_choice(&mut self, ch: char) -> AgentViewKeyOutcome {
        let Some(number) = ch.to_digit(10).filter(|number| *number > 0) else {
            return AgentViewKeyOutcome::Consumed;
        };
        let Some(row) = self.selected_row() else {
            return AgentViewKeyOutcome::Consumed;
        };
        if row.group != AgentViewGroup::NeedsInput {
            return AgentViewKeyOutcome::Consumed;
        }
        match row.source {
            AgentViewRowSource::Job => {
                let Some(permission) = row.pending_permission.as_ref() else {
                    return AgentViewKeyOutcome::Consumed;
                };
                let Some(option) = permission.options.get(number as usize - 1) else {
                    return AgentViewKeyOutcome::Consumed;
                };
                AgentViewKeyOutcome::AnswerJobPermission {
                    job_id: row.id.clone(),
                    query_id: permission.query_id,
                    turn_generation: permission.turn_generation,
                    endpoint: permission.endpoint.clone(),
                    option_id: option.option_id.clone(),
                }
            }
            AgentViewRowSource::Task => AgentViewKeyOutcome::AnswerTaskChoice {
                task_id: row.id.clone(),
                option_index: number as usize - 1,
            },
        }
    }

    fn visual_group_for_row(&self, row: &AgentViewRow) -> VisualGroup {
        match self.grouping_mode {
            AgentViewGrouping::State if is_session_row(row) => VisualGroup::Sessions,
            AgentViewGrouping::State => VisualGroup::State(row.group),
            AgentViewGrouping::Directory => VisualGroup::Directory(
                row.cwd
                    .as_deref()
                    .filter(|cwd| !cwd.trim().is_empty())
                    .unwrap_or("in-process")
                    .to_string(),
            ),
        }
    }

    fn toggle_grouping_mode(&mut self) {
        self.grouping_mode = match self.grouping_mode {
            AgentViewGrouping::State => AgentViewGrouping::Directory,
            AgentViewGrouping::Directory => AgentViewGrouping::State,
        };
        self.collapsed_groups.clear();
        self.expanded_limited_groups.clear();
        self.sort_rows_for_grouping();
        self.selected_visual_index = self.first_row_visual_index().unwrap_or(0);
        self.clamp_selection();
        self.status = Some(match self.grouping_mode {
            AgentViewGrouping::State => "grouping by state".to_string(),
            AgentViewGrouping::Directory => "grouping by directory".to_string(),
        });
    }

    fn sort_rows_for_grouping(&mut self) {
        match self.grouping_mode {
            AgentViewGrouping::State => self.rows.sort_by(state_grouping_order),
            AgentViewGrouping::Directory => self.rows.sort_by(|a, b| {
                directory_group_key(a)
                    .cmp(&directory_group_key(b))
                    .then_with(|| group_order(a.group).cmp(&group_order(b.group)))
                    .then_with(|| b.order_key.cmp(&a.order_key))
            }),
        }
    }

    fn confirm_remove_selected_job_outcome(&mut self) -> AgentViewKeyOutcome {
        let Some(row) = self.selected_row() else {
            return AgentViewKeyOutcome::Consumed;
        };
        match row.source {
            AgentViewRowSource::Job => self.confirm_remove_job_outcome(row.id.clone()),
            AgentViewRowSource::Task => {
                self.status = Some("remove is only implemented for job-backed rows".to_string());
                AgentViewKeyOutcome::Consumed
            }
        }
    }

    fn clear_input_or_cancel_session(&mut self) -> AgentViewKeyOutcome {
        if !self.input.is_empty() || !self.input_images.is_empty() {
            self.input.clear();
            self.input_images.clear();
            self.next_paste_id = 1;
            self.input_cursor = 0;
            self.agent_picker_index = None;
            self.agent_picker_visible = false;
            if matches!(self.input_mode, AgentViewInputMode::Filter) {
                self.filter.clear();
                self.clamp_selection();
                self.status = Some("filter cleared".to_string());
                return AgentViewKeyOutcome::FilterChanged;
            }
            self.status = Some("input cleared".to_string());
            return AgentViewKeyOutcome::Consumed;
        }
        if self.input_mode != AgentViewInputMode::Dispatch {
            self.input_mode = AgentViewInputMode::Dispatch;
            self.status = Some("input mode cancelled".to_string());
            return AgentViewKeyOutcome::Consumed;
        }
        AgentViewKeyOutcome::CancelOrExit
    }

    fn confirm_remove_job_outcome(&mut self, job_id: String) -> AgentViewKeyOutcome {
        let now = Instant::now();
        let should_remove = self
            .pending_remove_job
            .as_ref()
            .is_some_and(|(pending_id, deadline)| pending_id == &job_id && now <= *deadline);
        if should_remove {
            self.pending_remove_job = None;
            AgentViewKeyOutcome::RemoveJob { job_id }
        } else {
            self.pending_remove_job = Some((job_id.clone(), now + Duration::from_secs(2)));
            self.status = Some(format!("press remove again within 2s to remove {job_id}"));
            AgentViewKeyOutcome::Consumed
        }
    }

    fn confirm_remove_group_outcome(&mut self, group: VisualGroup) -> AgentViewKeyOutcome {
        // Removing a group stops and forgets every job in it. The sessions
        // are hosts someone may yet come back to, not a batch of finished
        // work; each is stopped from its own row, on purpose.
        if group == VisualGroup::Sessions {
            self.status =
                Some("sessions are stopped one at a time (Ctrl+X on the row)".to_string());
            return AgentViewKeyOutcome::Consumed;
        }
        let job_ids = self
            .rows
            .iter()
            .filter(|row| row.source == AgentViewRowSource::Job)
            .filter(|row| self.visual_group_for_row(row) == group)
            .map(|row| row.id.clone())
            .collect::<Vec<_>>();
        if job_ids.is_empty() {
            self.status = Some(format!("no job-backed rows in {}", group.label()));
            return AgentViewKeyOutcome::Consumed;
        }
        let now = Instant::now();
        let should_remove = self
            .pending_remove_group
            .as_ref()
            .is_some_and(|(pending, deadline)| pending == &group && now <= *deadline);
        if should_remove {
            self.pending_remove_group = None;
            AgentViewKeyOutcome::RemoveGroup {
                group_label: group.label(),
                job_ids,
            }
        } else {
            let label = group.label();
            self.pending_remove_group = Some((group, now + Duration::from_secs(2)));
            self.status = Some(format!(
                "press Ctrl+X again within 2s to remove {} job(s) in {label}",
                job_ids.len()
            ));
            AgentViewKeyOutcome::Consumed
        }
    }

    fn stop_or_remove_job_outcome(&mut self, job_id: String) -> AgentViewKeyOutcome {
        let Some(row) = self.selected_row() else {
            return AgentViewKeyOutcome::Consumed;
        };
        if matches!(
            row.group,
            AgentViewGroup::Completed | AgentViewGroup::Failed | AgentViewGroup::Stopped
        ) {
            return self.confirm_remove_job_outcome(job_id);
        }
        let now = Instant::now();
        let should_remove = self
            .pending_remove_job
            .as_ref()
            .is_some_and(|(pending_id, deadline)| pending_id == &job_id && now <= *deadline);
        if should_remove {
            self.pending_remove_job = None;
            AgentViewKeyOutcome::RemoveJob { job_id }
        } else {
            self.pending_remove_job = Some((job_id.clone(), now + Duration::from_secs(2)));
            self.status = Some(format!("press Ctrl+X again within 2s to remove {job_id}"));
            AgentViewKeyOutcome::StopJob { job_id }
        }
    }

    fn stop_or_remove_task_outcome(&mut self, task_id: String) -> AgentViewKeyOutcome {
        let Some(row) = self.selected_row() else {
            return AgentViewKeyOutcome::Consumed;
        };
        let now = Instant::now();
        let should_remove = self
            .pending_remove_job
            .as_ref()
            .is_some_and(|(pending_id, deadline)| pending_id == &task_id && now <= *deadline);
        if matches!(
            row.group,
            AgentViewGroup::Completed | AgentViewGroup::Failed | AgentViewGroup::Stopped
        ) && !should_remove
        {
            self.pending_remove_job = Some((task_id.clone(), now + Duration::from_secs(2)));
            self.status = Some(format!("press Ctrl+X again within 2s to remove {task_id}"));
            return AgentViewKeyOutcome::Consumed;
        }
        if should_remove {
            self.pending_remove_job = None;
            AgentViewKeyOutcome::RemoveTask { task_id }
        } else {
            self.pending_remove_job = Some((task_id.clone(), now + Duration::from_secs(2)));
            self.status = Some(format!("press Ctrl+X again within 2s to remove {task_id}"));
            AgentViewKeyOutcome::StopTask { task_id }
        }
    }

    fn rows_area(&self, area: Rect) -> Option<Rect> {
        let inner = agent_view_inner_area(area)?;
        let chunks = agent_view_chunks(inner);
        let body_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(chunks[1]);
        let rows = body_chunks[0];
        let rows_inner = Rect {
            x: rows.x.saturating_add(1),
            y: rows.y.saturating_add(1),
            width: rows.width.saturating_sub(2),
            height: rows.height.saturating_sub(2),
        };
        (rows_inner.width > 0 && rows_inner.height > 0).then_some(rows_inner)
    }

    fn handle_input_key(&mut self, key: &KeyEvent) -> AgentViewKeyOutcome {
        if matches!(self.input_mode, AgentViewInputMode::Dispatch) && self.agent_picker_visible {
            if let Some(outcome) = self.handle_agent_picker_key(key) {
                return outcome;
            }
        }
        if matches!(self.input_mode, AgentViewInputMode::Dispatch) {
            if self.input.trim().is_empty() && self.input_images.is_empty() {
                if matches!(
                    key.code,
                    KeyCode::Up
                        | KeyCode::Down
                        | KeyCode::Delete
                        | KeyCode::Backspace
                        | KeyCode::Char(' ')
                        | KeyCode::Char('?')
                        | KeyCode::Char('/')
                        | KeyCode::Char('R')
                ) || matches!(
                    key.code,
                    KeyCode::Char(ch)
                        if key.modifiers.contains(KeyModifiers::ALT) && ch.is_ascii_digit()
                ) || matches!(
                    key.code,
                    KeyCode::Char('s')
                        | KeyCode::Char('x')
                        | KeyCode::Char('t')
                        | KeyCode::Char('r')
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                ) {
                    return self.handle_table_key(key);
                }
                if let KeyCode::Char(ch) = key.code {
                    if ch.is_ascii_digit()
                        && key.modifiers.is_empty()
                        && self
                            .selected_row()
                            .is_some_and(|row| row.group == AgentViewGroup::NeedsInput)
                    {
                        return self.answer_selected_multiple_choice(ch);
                    }
                }
                match key.code {
                    KeyCode::Enter => return self.open_selected_outcome(),
                    KeyCode::Right => return AgentViewKeyOutcome::ReturnToSession,
                    KeyCode::Char('r') if key.modifiers.is_empty() => {
                        if self.peek_visible {
                            let Some(source) = self.selected_row().map(|row| row.source) else {
                                return AgentViewKeyOutcome::Consumed;
                            };
                            self.input_mode = match source {
                                AgentViewRowSource::Job => AgentViewInputMode::ReplyJob(
                                    self.selected_row()
                                        .map(|row| row.id.clone())
                                        .unwrap_or_default(),
                                ),
                                AgentViewRowSource::Task => AgentViewInputMode::ReplyTask(
                                    self.selected_row()
                                        .map(|row| row.id.clone())
                                        .unwrap_or_default(),
                                ),
                            };
                            self.status = Some(self.input_mode.status().to_string());
                            return AgentViewKeyOutcome::Consumed;
                        }
                    }
                    _ => {}
                }
            }
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.clear_input_or_cancel_session()
            }
            KeyCode::Esc => {
                if self.input_mode != AgentViewInputMode::Dispatch {
                    self.input_mode = AgentViewInputMode::Dispatch;
                    self.input.clear();
                    self.input_images.clear();
                    self.next_paste_id = 1;
                    self.input_cursor = 0;
                    self.agent_picker_index = None;
                    self.agent_picker_visible = false;
                    self.status = Some("reply/rename cancelled".to_string());
                    return AgentViewKeyOutcome::Consumed;
                }
                if !self.input.is_empty() || !self.input_images.is_empty() {
                    self.input.clear();
                    self.input_images.clear();
                    self.next_paste_id = 1;
                    self.input_cursor = 0;
                    self.agent_picker_index = None;
                    self.agent_picker_visible = false;
                    self.status = Some("input cleared".to_string());
                    return AgentViewKeyOutcome::Consumed;
                }
                if self.peek_visible {
                    self.peek_visible = false;
                    self.status = Some("peek panel hidden".to_string());
                    return AgentViewKeyOutcome::Consumed;
                }
                AgentViewKeyOutcome::Dismiss
            }
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                let images = self.dispatch_images_for_submit(&text);
                let placement = if key.modifiers.contains(KeyModifiers::SHIFT) {
                    DispatchPlacement::Here
                } else {
                    DispatchPlacement::Detached
                };
                let mode = self.input_mode.clone();
                if matches!(mode, AgentViewInputMode::Filter) {
                    self.apply_filter_text(text);
                    return AgentViewKeyOutcome::FilterChanged;
                }
                if matches!(mode, AgentViewInputMode::Dispatch)
                    && text.is_empty()
                    && images.is_empty()
                {
                    self.input_focused = true;
                    self.input_mode = AgentViewInputMode::Dispatch;
                    return self.open_selected_outcome();
                }
                if matches!(mode, AgentViewInputMode::Dispatch) && images.is_empty() {
                    if self.select_existing_pr_row_for_dispatch(&text) {
                        self.input_focused = true;
                        self.input_mode = AgentViewInputMode::Dispatch;
                        self.input.clear();
                        self.input_images.clear();
                        self.next_paste_id = 1;
                        self.input_cursor = 0;
                        self.agent_picker_index = None;
                        self.agent_picker_visible = false;
                        return AgentViewKeyOutcome::Consumed;
                    }
                    if dispatch_text_is_filter_query(&text) {
                        self.apply_filter_text(text);
                        return AgentViewKeyOutcome::FilterChanged;
                    }
                }
                self.input_focused = true;
                self.input_mode = AgentViewInputMode::Dispatch;
                self.input.clear();
                self.input_images.clear();
                self.next_paste_id = 1;
                self.input_cursor = 0;
                self.agent_picker_index = None;
                self.agent_picker_visible = false;
                if text.is_empty() && images.is_empty() {
                    self.status = Some("input was empty".to_string());
                    return AgentViewKeyOutcome::Consumed;
                }
                self.submit_input_text(text, images, placement, mode)
            }
            // Ctrl+N: dispatch into a worker and mirror it. Deliberately
            // not Ctrl+Enter (most terminals cannot send it at all, and a key
            // that silently does nothing is worse than no key), not Ctrl+B
            // (tmux eats it), and not Ctrl+O (which already toggles tool
            // output in the session view — one key, two unrelated meanings).
            KeyCode::Char('n')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(self.input_mode, AgentViewInputMode::Dispatch) =>
            {
                let text = self.input.trim().to_string();
                let images = self.dispatch_images_for_submit(&text);
                if text.is_empty() && images.is_empty() {
                    self.status = Some("input was empty".to_string());
                    return AgentViewKeyOutcome::Consumed;
                }
                self.input_focused = true;
                self.input.clear();
                self.input_images.clear();
                self.next_paste_id = 1;
                self.input_cursor = 0;
                self.agent_picker_index = None;
                self.agent_picker_visible = false;
                self.submit_input_text(
                    text,
                    images,
                    DispatchPlacement::Hosted,
                    AgentViewInputMode::Dispatch,
                )
            }
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                AgentViewKeyOutcome::EditInputExternally {
                    text: self.input.clone(),
                }
            }
            KeyCode::Tab => {
                let mode = self.input_mode.clone();
                match &mode {
                    AgentViewInputMode::ReplyJob(id) | AgentViewInputMode::ReplyTask(id) => {
                        // Peek-panel suggested reply (Agent Views spec):
                        // "press `Tab` to fill the input with a suggested
                        // reply you can edit before sending."
                        let source = match mode {
                            AgentViewInputMode::ReplyJob(_) => AgentViewRowSource::Job,
                            AgentViewInputMode::ReplyTask(_) => AgentViewRowSource::Task,
                            _ => unreachable!(),
                        };
                        if let Some(row) = self
                            .rows
                            .iter()
                            .find(|row| row.source == source && row.id == *id)
                        {
                            let suggestion = suggested_reply_for_row(row);
                            self.input.clear();
                            self.input.push_str(&suggestion);
                            self.input_cursor = self.input.len();
                            self.status = Some(format!(
                                "filled suggested reply: {} (edit and press Enter)",
                                suggestion
                            ));
                        } else {
                            self.status =
                                Some("no row matches the active reply target".to_string());
                        }
                    }
                    AgentViewInputMode::Dispatch => {
                        self.open_or_cycle_dispatch_agent_picker();
                    }
                    AgentViewInputMode::Filter => {
                        self.status = Some("Tab is not supported in filter mode".to_string());
                    }
                    AgentViewInputMode::RenameJob(_) => {
                        self.status = Some("Tab is not supported in rename mode".to_string());
                    }
                }
                AgentViewKeyOutcome::Consumed
            }
            KeyCode::Left => {
                self.input_cursor = previous_char_boundary(&self.input, self.input_cursor);
                AgentViewKeyOutcome::Consumed
            }
            KeyCode::Right => {
                self.input_cursor = next_char_boundary(&self.input, self.input_cursor);
                AgentViewKeyOutcome::Consumed
            }
            KeyCode::Backspace => {
                if self.input_cursor > 0 {
                    let end = next_char_boundary_if_inside(&self.input, self.input_cursor);
                    let start = previous_char_boundary(&self.input, end);
                    self.input.replace_range(start..end, "");
                    self.input_cursor = start;
                }
                self.agent_picker_index = None;
                self.agent_picker_visible = false;
                if matches!(self.input_mode, AgentViewInputMode::Filter) {
                    self.filter = self.input.trim().to_string();
                    self.clamp_selection();
                    return AgentViewKeyOutcome::FilterChanged;
                }
                AgentViewKeyOutcome::Consumed
            }
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.input_cursor = clamp_to_char_boundary(&self.input, self.input_cursor);
                self.input.insert(self.input_cursor, ch);
                self.input_cursor += ch.len_utf8();
                self.agent_picker_index = None;
                self.agent_picker_visible = false;
                if matches!(self.input_mode, AgentViewInputMode::Filter) {
                    self.filter = self.input.trim().to_string();
                    self.clamp_selection();
                    return AgentViewKeyOutcome::FilterChanged;
                }
                AgentViewKeyOutcome::Consumed
            }
            _ => AgentViewKeyOutcome::Consumed,
        }
    }

    pub fn is_input_focused(&self) -> bool {
        self.input_focused
    }

    pub fn replace_input_from_external_editor(&mut self, input: String) {
        self.input = input;
        self.input_cursor = self.input.len();
        self.input_focused = true;
        self.agent_picker_index = None;
        self.agent_picker_visible = false;
        self.status = Some(match self.input_mode {
            AgentViewInputMode::Dispatch => "edited dispatch input; Enter to submit".to_string(),
            AgentViewInputMode::Filter => "edited filter input; Enter to apply".to_string(),
            AgentViewInputMode::ReplyJob(_) | AgentViewInputMode::ReplyTask(_) => {
                "edited reply input; Enter to submit".to_string()
            }
            AgentViewInputMode::RenameJob(_) => "edited rename input; Enter to submit".to_string(),
        });
    }

    pub fn paste_image(&mut self, image: crate::tui::clipboard_image::ClipboardImage) {
        let id = self.next_paste_id;
        let content = rebon_types::PromptPasteContent {
            id,
            kind: String::from("image"),
            content: image.data,
            media_type: Some(image.media_type),
            filename: image
                .filename
                .or_else(|| Some(String::from("Pasted image"))),
            source_path: image.source_path,
        };
        let chip = format!("[Image #{id}]");
        self.input_cursor = clamp_to_char_boundary(&self.input, self.input_cursor);
        self.input.insert_str(self.input_cursor, &chip);
        self.input_cursor += chip.len();
        self.input_images.push(content);
        self.next_paste_id = self.next_paste_id.saturating_add(1);
        self.input_focused = true;
        self.agent_picker_index = None;
        self.agent_picker_visible = false;
        if matches!(self.input_mode, AgentViewInputMode::Dispatch) {
            self.status = Some("pasted image into dispatch input".to_string());
        } else {
            self.input_mode = AgentViewInputMode::Dispatch;
            self.status = Some("pasted image into new dispatch input".to_string());
        }
    }

    pub fn paste_text(&mut self, text: &str) {
        self.input_cursor = clamp_to_char_boundary(&self.input, self.input_cursor);
        self.input.insert_str(self.input_cursor, text);
        self.input_cursor += text.len();
        self.input_focused = true;
        self.agent_picker_index = None;
        self.agent_picker_visible = false;
        if matches!(self.input_mode, AgentViewInputMode::Filter) {
            self.filter = self.input.trim().to_string();
            self.clamp_selection();
        }
    }

    fn start_filter_input(&mut self) {
        self.input_focused = true;
        self.input_mode = AgentViewInputMode::Filter;
        self.input.clear();
        self.input.push_str(&self.filter);
        self.input_images.clear();
        self.next_paste_id = 1;
        self.input_cursor = self.input.len();
        self.agent_picker_index = None;
        self.agent_picker_visible = false;
        self.status = Some(self.input_mode.status().to_string());
    }

    fn apply_filter_text(&mut self, filter: String) {
        self.filter = filter.trim().to_string();
        self.input_focused = true;
        self.input_mode = AgentViewInputMode::Dispatch;
        self.input.clear();
        self.input_images.clear();
        self.next_paste_id = 1;
        self.input_cursor = 0;
        self.agent_picker_index = None;
        self.agent_picker_visible = false;
        self.clamp_selection();
        self.status = Some(if self.filter.is_empty() {
            "filter cleared".to_string()
        } else {
            format!("filter applied: {}", self.filter)
        });
    }

    fn dispatch_images_for_submit(&self, text: &str) -> Vec<rebon_types::PromptPasteContent> {
        let referenced_ids = image_reference_ids(text);
        let text_is_empty = text.trim().is_empty();
        self.input_images
            .iter()
            .filter(|image| text_is_empty || referenced_ids.contains(&image.id))
            .cloned()
            .collect()
    }

    /// Lazily build (or reuse) the subagent name list used by the
    /// dispatch Tab picker. Pulls from `AgentRegistry::load` against
    /// the agent view's cwd scope (if set) or the process cwd.
    fn ensure_agent_picker_names(&mut self) {
        if self.agent_picker_names.is_empty() {
            let cwd = self
                .cwd_scope
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    // Mid-session picker: a vanished cwd empties the project
                    // agents rather than breaking the dialog.
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
                });
            let home = crate::rebon_config::config_home_dir();
            let registry = rebon_tool::AgentRegistry::load(&cwd, &home);
            self.agent_picker_names = registry
                .active()
                .map(|def| def.agent_type.clone())
                .collect();
        }
    }

    /// Open or advance the dispatch subagent browser. Empty dispatch
    /// input opens the visible browser; non-empty input keeps the older
    /// inline token-cycle behavior.
    fn open_or_cycle_dispatch_agent_picker(&mut self) {
        self.ensure_agent_picker_names();
        if self.agent_picker_names.is_empty() {
            self.status = Some("no subagents are configured for this directory".to_string());
            return;
        }
        if self.input.trim().is_empty() && self.input_images.is_empty() {
            self.agent_picker_visible = true;
        }
        self.cycle_dispatch_agent_picker(1);
    }

    /// Cycle the leading `@<agent>` token in the dispatch input. If
    /// the input is empty or has no `@` prefix, the first cycle inserts
    /// `@<first-agent> ` so subsequent typing becomes the prompt body.
    fn cycle_dispatch_agent_picker(&mut self, step: usize) {
        let total = self.agent_picker_names.len();
        let next_index = match self.agent_picker_index {
            Some(idx) => (idx + step) % total,
            None => {
                if let Some(current) = leading_agent_token(&self.input) {
                    self.agent_picker_names
                        .iter()
                        .position(|name| name == current)
                        .map(|idx| (idx + step) % total)
                        .unwrap_or(0)
                } else {
                    0
                }
            }
        };
        self.apply_dispatch_agent_picker_index(next_index);
    }

    fn apply_dispatch_agent_picker_index(&mut self, next_index: usize) {
        let total = self.agent_picker_names.len();
        let chosen = self.agent_picker_names[next_index].clone();
        let rest = strip_leading_agent_token(&self.input).to_string();
        let rest_trimmed = rest.trim_start();
        self.input = if rest_trimmed.is_empty() {
            format!("@{chosen} ")
        } else {
            format!("@{chosen} {rest_trimmed}")
        };
        self.input_cursor = self.input.len();
        self.agent_picker_index = Some(next_index);
        if self.agent_picker_visible {
            self.status = Some(format!(
                "subagents: @{chosen} ({}/{}) · Enter select · Tab/↓ next · ↑ previous · Esc close",
                next_index + 1,
                total
            ));
        } else {
            self.status = Some(format!(
                "subagent: @{chosen} ({}/{}) · Tab to cycle",
                next_index + 1,
                total
            ));
        }
    }

    fn handle_agent_picker_key(&mut self, key: &KeyEvent) -> Option<AgentViewKeyOutcome> {
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                self.cycle_dispatch_agent_picker(1);
                Some(AgentViewKeyOutcome::Consumed)
            }
            KeyCode::Up => {
                let total = self.agent_picker_names.len();
                if total > 0 {
                    self.cycle_dispatch_agent_picker(total.saturating_sub(1));
                }
                Some(AgentViewKeyOutcome::Consumed)
            }
            KeyCode::Enter => {
                self.agent_picker_visible = false;
                if self.agent_picker_index.is_none() && !self.agent_picker_names.is_empty() {
                    self.apply_dispatch_agent_picker_index(0);
                }
                self.status = Some("subagent selected; type a prompt to dispatch".to_string());
                Some(AgentViewKeyOutcome::Consumed)
            }
            KeyCode::Esc => {
                self.agent_picker_visible = false;
                self.agent_picker_index = None;
                self.status = Some("subagent browser closed".to_string());
                Some(AgentViewKeyOutcome::Consumed)
            }
            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete => {
                self.agent_picker_visible = false;
                None
            }
            _ => None,
        }
    }

    fn submit_input_text(
        &self,
        text: String,
        images: Vec<rebon_types::PromptPasteContent>,
        placement: DispatchPlacement,
        mode: AgentViewInputMode,
    ) -> AgentViewKeyOutcome {
        match mode {
            AgentViewInputMode::Filter => AgentViewKeyOutcome::FilterChanged,
            AgentViewInputMode::RenameJob(job_id) => {
                AgentViewKeyOutcome::RenameJob { job_id, name: text }
            }
            AgentViewInputMode::ReplyTask(task_id) => {
                if let Some(command) = strip_bash_prefix(&text) {
                    AgentViewKeyOutcome::BashCommandToTask { task_id, command }
                } else {
                    AgentViewKeyOutcome::ReplyToTask {
                        task_id,
                        message: text,
                    }
                }
            }
            AgentViewInputMode::ReplyJob(job_id) => {
                if let Some(command) = strip_bash_prefix(&text) {
                    AgentViewKeyOutcome::BashCommandToJob { job_id, command }
                } else {
                    AgentViewKeyOutcome::ReplyToJob {
                        job_id,
                        message: text,
                    }
                }
            }
            AgentViewInputMode::Dispatch => match (placement, images.is_empty()) {
                (DispatchPlacement::Here, true) => {
                    AgentViewKeyOutcome::DispatchPromptAndAttach(text)
                }
                (DispatchPlacement::Here, false) => {
                    AgentViewKeyOutcome::DispatchPromptAndAttachWithImages {
                        prompt: text,
                        images,
                    }
                }
                (DispatchPlacement::Hosted, true) => {
                    AgentViewKeyOutcome::DispatchPromptHosted(text)
                }
                (DispatchPlacement::Hosted, false) => {
                    AgentViewKeyOutcome::DispatchPromptHostedWithImages {
                        prompt: text,
                        images,
                    }
                }
                (DispatchPlacement::Detached, true) => AgentViewKeyOutcome::DispatchPrompt(text),
                (DispatchPlacement::Detached, false) => {
                    AgentViewKeyOutcome::DispatchPromptWithImages {
                        prompt: text,
                        images,
                    }
                }
            },
        }
    }

    fn select_existing_pr_row_for_dispatch(&mut self, text: &str) -> bool {
        let needles = pr_reference_needles(text);
        if needles.is_empty() {
            return false;
        }
        for needle in needles {
            if let Some(row_index) = self.rows.iter().position(|row| {
                row.source == AgentViewRowSource::Job && row_matches_pr_reference(row, &needle)
            }) {
                let name = self.rows[row_index].name.clone();
                self.select_row_index(row_index);
                self.status = Some(format!("selected existing PR #{needle}: {name}"));
                self.peek_visible = true;
                return true;
            }
        }
        false
    }

    fn select_previous(&mut self) {
        let total = self.visual_items().len();
        if total == 0 {
            self.selected_visual_index = 0;
        } else if self.selected_visual_index == 0 {
            self.selected_visual_index = total - 1;
        } else {
            self.selected_visual_index -= 1;
        }
        self.clamp_selection();
    }

    fn select_next(&mut self) {
        let total = self.visual_items().len();
        if total == 0 {
            self.selected_visual_index = 0;
        } else {
            self.selected_visual_index = (self.selected_visual_index + 1) % total;
        }
        self.clamp_selection();
    }

    fn visual_line_index_for_item(items: &[VisualItem], visual_index: usize) -> usize {
        let mut line_index = 0usize;
        for (index, item) in items.iter().enumerate() {
            if index > 0 && matches!(item, VisualItem::Group(_)) {
                line_index = line_index.saturating_add(1);
            }
            if index == visual_index {
                break;
            }
            line_index = line_index.saturating_add(1);
        }
        line_index
    }

    fn keep_selection_visible(&mut self, visible_height: usize) {
        if visible_height == 0 {
            self.row_scroll_offset = 0;
            return;
        }
        let items = self.visual_items();
        if items.is_empty() {
            self.row_scroll_offset = 0;
            return;
        }
        let selected_line = Self::visual_line_index_for_item(&items, self.selected_visual_index);
        if selected_line < self.row_scroll_offset {
            self.row_scroll_offset = selected_line;
        } else {
            let bottom = self.row_scroll_offset.saturating_add(visible_height);
            if selected_line >= bottom {
                self.row_scroll_offset = selected_line
                    .saturating_add(1)
                    .saturating_sub(visible_height);
            }
        }
    }

    fn clamp_selection(&mut self) {
        let items = self.visual_items();
        if items.is_empty() {
            self.selected_index = 0;
            self.selected_visual_index = 0;
            self.row_scroll_offset = 0;
            return;
        }
        if self.selected_visual_index >= items.len() {
            self.selected_visual_index = items.len() - 1;
        }
        match &items[self.selected_visual_index] {
            VisualItem::Row(index) => self.selected_index = *index,
            VisualItem::More { group, .. } | VisualItem::Group(group) => {
                if let Some(index) = self
                    .rows
                    .iter()
                    .position(|row| self.visual_group_for_row(row) == *group)
                {
                    self.selected_index = index;
                } else if !self.rows.is_empty() {
                    self.selected_index = self.selected_index.min(self.rows.len() - 1);
                } else {
                    self.selected_index = 0;
                }
            }
        }
        if let Some(area) = self.last_render_area.and_then(|area| self.rows_area(area)) {
            self.keep_selection_visible(area.height as usize);
        }
    }

    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &rebon_tui::RenderTheme,
        cursor_hint: &mut Option<(u16, u16)>,
    ) {
        self.last_render_area = Some(area);
        frame.render_widget(Clear, area);
        let block = Block::default().title(" Agent View ").borders(Borders::ALL);
        frame.render_widget(block, area);
        let Some(inner) = agent_view_inner_area(area) else {
            return;
        };
        let chunks = agent_view_chunks(inner);
        let header = vec![
            Line::from(vec![
                Span::styled("filter ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(if self.filter.is_empty() {
                    "(none)"
                } else {
                    &self.filter
                }),
            ]),
            Line::from(self.contextual_hint()),
        ];
        frame.render_widget(Paragraph::new(header), chunks[0]);

        let body_chunks = if self.peek_visible {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(chunks[1])
        } else {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(100)])
                .split(chunks[1])
        };
        self.render_rows(frame, body_chunks[0]);
        if self.peek_visible {
            self.render_peek(frame, body_chunks[1]);
        }

        if self.input_focused {
            let block = Block::default().borders(Borders::ALL).title("input");
            let input_area = block.inner(chunks[2]);
            frame.render_widget(block, chunks[2]);
            let runtime = rebon_tui::promptinput::derive_prompt_input_runtime_state(
                &rebon_tui::promptinput::PromptInputRuntimeInput {
                    mode: "prompt".to_string(),
                    input: self.input.clone(),
                    history_match_display: None,
                    is_searching_history: false,
                    is_modal_overlay_active: false,
                    footer_item_selected: false,
                    cursor_offset: self.input_cursor,
                    suggestion_count: 0,
                    default_placeholder: Some(self.input_placeholder().to_string()),
                    prompt_suggestion: None,
                    prompt_suggestion_state: rebon_tui::promptinput::PromptSuggestionState {
                        text: None,
                        shown_at: 0,
                    },
                    viewing_agent_task_id_present: false,
                    can_undo: false,
                    history_failed_match: false,
                    history_query_length: 0,
                    btw_triggers: Vec::new(),
                    slash_command_triggers: Vec::new(),
                    token_budget_triggers: Vec::new(),
                    slack_channel_triggers: Vec::new(),
                    member_mention_highlights: Vec::new(),
                    voice_interim_range: None,
                    think_triggers: Vec::new(),
                    ultraplan_triggers: Vec::new(),
                    ultrareview_triggers: Vec::new(),
                    ultrathink_enabled: false,
                    ultraplan_enabled: false,
                },
                |_, _, _| "ansi:cyan".to_string(),
            );
            let result = rebon_tui::render_prompt_input(
                &runtime,
                self.input_cursor,
                input_area,
                frame.buffer_mut(),
                theme,
            );
            if let Some(cursor) = result.cursor {
                *cursor_hint = Some(cursor);
            }
        } else {
            let status = self.status.clone().unwrap_or_else(|| {
                "Enter attaches rows; → returns to your session; Enter/click group headers collapses them; logs/stop are also available from shell".to_string()
            });
            frame.render_widget(
                Paragraph::new(status)
                    .block(Block::default().borders(Borders::ALL).title("status")),
                chunks[2],
            );
        }
        self.render_agent_picker_browser(frame, area);
    }

    fn input_placeholder(&self) -> &'static str {
        match self.input_mode {
            AgentViewInputMode::Dispatch => "Type a task to dispatch a session alongside this one",
            AgentViewInputMode::Filter => "Filter sessions: a:<agent>, s:<state>, #<number>",
            AgentViewInputMode::ReplyJob(_) | AgentViewInputMode::ReplyTask(_) => "Reply message",
            AgentViewInputMode::RenameJob(_) => "New job name",
        }
    }

    fn contextual_hint(&self) -> &'static str {
        if self.input_focused {
            match self.input_mode {
                AgentViewInputMode::RenameJob(_) => {
                    return "keys: Enter save rename · Ctrl+G editor · ←/→ cursor · Esc cancel";
                }
                AgentViewInputMode::Filter => {
                    return "keys: type to filter · Enter keep filter · Backspace edit · Esc close filter input";
                }
                AgentViewInputMode::ReplyJob(_) | AgentViewInputMode::ReplyTask(_) => {
                    return "keys: Enter send reply · Tab suggestion · Ctrl+G editor · Esc cancel";
                }
                AgentViewInputMode::Dispatch => {
                    if self.input.is_empty() && self.input_images.is_empty() {
                        return "keys: type to dispatch · Enter attach selected · → return · ↑/↓ select · Space peek · Ctrl+X stop/rm · Esc close";
                    }
                    return "keys: Enter dispatch · Ctrl+N dispatch+watch · Shift+Enter run here · Ctrl+G editor · paste image/text · Esc clear";
                }
            }
        }
        if self.peek_visible {
            return "keys: Space/Esc close peek · r reply · 1..9 answer choice · ↑/↓ next row · Enter attach · → return";
        }
        match self.selected_visual_item() {
            Some(VisualItem::Group(_)) => {
                "keys: Enter collapse/expand group · ↑/↓ move · Ctrl+S group by directory/state · Esc close"
            }
            Some(VisualItem::More { .. }) => {
                "keys: Enter show older completed jobs · ↑/↓ move · Esc close"
            }
            Some(VisualItem::Row(index)) => match self.rows.get(index).map(|row| row.group) {
                Some(AgentViewGroup::NeedsInput) => {
                    "keys: 1..9 answer · r reply · Space peek · Enter attach/open · → return · Ctrl+X stop · Esc close"
                }
                Some(AgentViewGroup::Completed | AgentViewGroup::Failed | AgentViewGroup::Stopped) => {
                    "keys: Enter attach/open · → return · Space peek · r follow up · R respawn · Del rm · Esc close"
                }
                _ => "keys: Enter attach/open · → return · Space peek · r reply · Ctrl+T pin · Ctrl+R rename · Ctrl+X stop · Esc close",
            },
            None => "keys: d dispatch · / filter · Ctrl+S group by directory/state · Esc close",
        }
    }

    fn render_rows(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title(" agents ");
        frame.render_widget(block, area);
        let inner = Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(1),
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };
        let mut lines = Vec::new();
        if self.input_focused
            && matches!(self.input_mode, AgentViewInputMode::Dispatch)
            && self.input.is_empty()
            && self.input_images.is_empty()
        {
            lines.push(Line::from(Span::styled(
                "Sessions keep running even after you close the terminal. Try: paste a PR or issue URL · \"investigate why the auth test is flaky\" · \"address the review comments on #1234\"",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(""));
        }
        let mut line_index = 0usize;
        let mut previous_was_group = false;
        let height = inner.height as usize;
        for (visual_index, item) in self.visual_items().into_iter().enumerate() {
            if visual_index > 0 && matches!(item, VisualItem::Group(_)) && !previous_was_group {
                if line_index >= self.row_scroll_offset && lines.len() < height {
                    lines.push(Line::from(""));
                }
                line_index = line_index.saturating_add(1);
            }
            previous_was_group = matches!(item, VisualItem::Group(_));
            let visible = line_index >= self.row_scroll_offset && lines.len() < height;
            if !visible {
                line_index = line_index.saturating_add(1);
                continue;
            }
            match item {
                VisualItem::Group(group) => {
                    let selected = visual_index == self.selected_visual_index;
                    let marker = if self.is_group_collapsed(&group) {
                        "▸"
                    } else {
                        "▾"
                    };
                    let label = format!("{marker} {}", group.label());
                    let style = if selected {
                        Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
                    } else {
                        Style::default().add_modifier(Modifier::BOLD)
                    };
                    lines.push(Line::from(Span::styled(label, style)));
                }
                VisualItem::Row(row_index) => {
                    let Some(row) = self.rows.get(row_index) else {
                        continue;
                    };
                    let selected = visual_index == self.selected_visual_index;
                    lines.push(render_agent_row_line(row, selected, inner.width as usize));
                }
                VisualItem::More { hidden, .. } => {
                    let selected = visual_index == self.selected_visual_index;
                    let prefix = if selected { ">" } else { " " };
                    let text = truncate_to_ellipsis(
                        &format!("{prefix} … {hidden} more completed jobs"),
                        inner.width as usize,
                    );
                    let style = if selected {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                    lines.push(Line::from(Span::styled(text, style)));
                }
            }
            line_index = line_index.saturating_add(1);
        }
        if lines.is_empty() {
            lines.push(Line::from("No agents or background jobs match the filter."));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_peek(&self, frame: &mut Frame, area: Rect) {
        let title = self
            .selected_row()
            .map(|row| format!(" peek {} ", row.id))
            .unwrap_or_else(|| " peek ".to_string());
        let block = Block::default().borders(Borders::ALL).title(title);
        frame.render_widget(block, area);
        let inner = Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(1),
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };
        let lines = self
            .selected_row()
            .map(|row| {
                let mut lines = vec![
                    Line::from(format!("source: {:?}", row.source)),
                    Line::from(format!("state: {}", row.group.state_filter())),
                    Line::from(format!("name: {}", row.name)),
                ];
                if let Some(session_id) = &row.session_id {
                    lines.push(Line::from(format!("session: {session_id}")));
                }
                if let Some(cwd) = &row.cwd {
                    lines.push(Line::from(format!("cwd: {cwd}")));
                }
                for line in &row.peek_lines {
                    lines.push(Line::from(truncate_to_ellipsis(line, inner.width as usize)));
                }
                lines
            })
            .unwrap_or_else(|| vec![Line::from("No selection")]);
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_agent_picker_browser(&self, frame: &mut Frame, area: Rect) {
        if !self.agent_picker_visible
            || !self.input_focused
            || !matches!(self.input_mode, AgentViewInputMode::Dispatch)
            || self.agent_picker_names.is_empty()
        {
            return;
        }
        if area.width < 24 || area.height < 8 {
            return;
        }

        let selected = self
            .agent_picker_index
            .unwrap_or(0)
            .min(self.agent_picker_names.len().saturating_sub(1));
        let max_visible = (area.height.saturating_sub(6) as usize).clamp(1, 8);
        let visible_count = self.agent_picker_names.len().min(max_visible);
        let height = (visible_count + 2) as u16;
        let max_name_width = self
            .agent_picker_names
            .iter()
            .map(|name| name.width())
            .max()
            .unwrap_or(10);
        let desired_width = (max_name_width + 10).clamp(24, 48) as u16;
        let width = desired_width.min(area.width.saturating_sub(4));
        if width < 20 {
            return;
        }
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + area.height.saturating_sub(height + 4);
        let popup = Rect {
            x,
            y,
            width,
            height,
        };
        frame.render_widget(Clear, popup);
        let block = Block::default().borders(Borders::ALL).title(" subagents ");
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let capacity = inner.height as usize;
        if capacity == 0 {
            return;
        }
        let mut start = selected.saturating_sub(capacity / 2);
        let max_start = self.agent_picker_names.len().saturating_sub(capacity);
        start = start.min(max_start);
        let end = (start + capacity).min(self.agent_picker_names.len());
        let lines = self.agent_picker_names[start..end]
            .iter()
            .enumerate()
            .map(|(offset, name)| {
                let index = start + offset;
                let selected_line = index == selected;
                let prefix = if selected_line { ">" } else { " " };
                let text = truncate_to_ellipsis(&format!("{prefix} @{name}"), inner.width as usize);
                let style = if selected_line {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(text, style))
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// Build the styled line for one agent row: leading state icon, name +
/// detail, and the right-aligned pull-request dot (when present).
/// `total_width` is the inner display width to fit within. Returns a
/// `Line<'static>` so callers can store it without lifetime gymnastics.
fn render_agent_row_line(row: &AgentViewRow, selected: bool, total_width: usize) -> Line<'static> {
    let (_, icon_style) = row.group.state_icon();
    let icon_glyph = row.process_shape.glyph();
    let prefix = if selected { ">" } else { " " };
    let pin = if row.pinned { "*" } else { " " };
    let badge = match row.source {
        AgentViewRowSource::Job if is_session_row(row) => "session",
        AgentViewRowSource::Job => "job",
        AgentViewRowSource::Task => "task",
    };
    let base_style = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    let icon_style = if selected {
        icon_style.add_modifier(Modifier::REVERSED)
    } else {
        icon_style
    };

    let leading = format!("{prefix}{pin} ");
    let trailing_text = format!(" [{badge}] {} — {}", row.name, row.detail);
    let dot_text = match row.pr_status {
        Some(_) if row.pr_count > 1 => format!("  {} PR ●", row.pr_count),
        Some(_) => "  PR ●".to_string(),
        None => String::new(),
    };

    let leading_w = leading.width();
    let icon_w = icon_glyph.width();
    let dot_w = dot_text.width();
    let used_fixed = leading_w + icon_w + dot_w;
    let middle_budget = total_width.saturating_sub(used_fixed);
    let trailing = truncate_to_ellipsis(&trailing_text, middle_budget);
    let trailing_pad = middle_budget.saturating_sub(trailing.width());

    let mut spans = vec![
        Span::styled(leading, base_style),
        Span::styled(icon_glyph.to_string(), icon_style),
        Span::styled(trailing, base_style),
    ];
    if trailing_pad > 0 {
        spans.push(Span::styled(" ".repeat(trailing_pad), base_style));
    }
    if let Some(status) = row.pr_status {
        let dot_style = if selected {
            status.dot_style().add_modifier(Modifier::REVERSED)
        } else {
            status.dot_style()
        };
        spans.push(Span::styled(dot_text, dot_style));
    }
    Line::from(spans)
}

fn agent_view_inner_area(area: Rect) -> Option<Rect> {
    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    (inner.width > 0 && inner.height > 0).then_some(inner)
}

fn agent_view_chunks(inner: Rect) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(4),
        ])
        .split(inner)
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn filter_jobs_by_cwd(
    jobs: Vec<BackgroundJobState>,
    cwd_scope: Option<&str>,
) -> Vec<BackgroundJobState> {
    let Some(cwd_scope) = cwd_scope else {
        return jobs;
    };
    jobs.into_iter()
        .filter(|job| job.identity.cwd == cwd_scope)
        .collect()
}

/// What a row needs to know beyond the job it is made from: the time (a
/// lingering host is described by how long it has left) and which job is
/// this terminal's own.
struct RowContext<'a> {
    now_ms: u64,
    this_terminal_job_id: Option<&'a str>,
}

impl<'a> RowContext<'a> {
    fn now(this_terminal_job_id: Option<&'a str>) -> Self {
        Self {
            now_ms: rebon_session_host::now_ms(),
            this_terminal_job_id,
        }
    }
}

fn build_rows(
    jobs: Vec<BackgroundJobState>,
    tasks: &[TaskSnapshot],
    filter: &str,
    context: &RowContext<'_>,
) -> Vec<AgentViewRow> {
    let mut rows = Vec::new();
    for job in jobs {
        if job.lease.placement == rebon_session_host::JobPlacement::Foreground
            && !session_is_listed(&job, context)
        {
            continue;
        }
        let row = row_from_job(job, context);
        if row_matches_filter(&row, filter) {
            rows.push(row);
        }
    }
    for task in tasks {
        if !matches!(
            task.kind,
            TaskKind::LocalAgent | TaskKind::InProcessTeammate | TaskKind::LocalWorkflow
        ) {
            continue;
        }
        let row = row_from_task(task);
        if row_matches_filter(&row, filter) {
            rows.push(row);
        }
    }
    rows.sort_by(state_grouping_order);
    rows
}

fn is_session_row(row: &AgentViewRow) -> bool {
    row.source == AgentViewRowSource::Job
        && row.placement == rebon_session_host::JobPlacement::Foreground
}

/// True while any client's lease on the job is fresh: somebody — a
/// terminal, the app, the web — has the session open right now.
fn has_live_client_lease(job: &BackgroundJobState, now_ms: u64) -> bool {
    job.lease.client_leases.iter().any(|lease| {
        now_ms.saturating_sub(lease.updated_at_ms) < rebon_session_host::CLIENT_LEASE_TTL_MS
    })
}

/// Which foreground jobs the list shows: a session
/// somebody is on is that person's open window, not a background task,
/// and a record without a live worker is the resume dialog's business.
/// What remains is a worker running for nobody — lingering after its
/// last client left, or finishing a turn unattended — and §19.9 wants
/// exactly those listed: the processes a user may not know they have.
fn session_is_listed(job: &BackgroundJobState, context: &RowContext<'_>) -> bool {
    // This terminal's own session: checked by id, not lease — at startup
    // its first lease can be seconds away from landing.
    if context.this_terminal_job_id == Some(job.identity.job_id.as_str()) {
        return false;
    }
    job.process.pid.is_some()
        && job.process.status != BackgroundJobStatus::Stopped
        && !has_live_client_lease(job, context.now_ms)
}

/// The order rows take when grouped by state: sessions first, then the
/// state groups in their fixed order, newest first within each.
fn state_grouping_order(a: &AgentViewRow, b: &AgentViewRow) -> std::cmp::Ordering {
    is_session_row(b)
        .cmp(&is_session_row(a))
        .then_with(|| group_order(a.group).cmp(&group_order(b.group)))
        .then_with(|| b.order_key.cmp(&a.order_key))
}

/// A duration in the unit a person would say it in.
fn brief_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 3600 {
        format!("{} h", secs / 3600)
    } else if secs >= 60 {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    }
}

/// What a listed session's row says after its state: how long its host
/// stays up for whoever left to come back. A listed session has nobody on
/// it by definition (`session_is_listed`); mid-turn the state word is the
/// whole story.
///
/// RFC-0004 §19.9: a user may not know a process is still alive for a
/// terminal they closed. This is where they find out.
fn session_presence(job: &BackgroundJobState, now_ms: u64) -> Option<String> {
    match job.process.status {
        // A worker with nobody watching, between turns: it is lingering,
        // and leaves when the linger runs out.
        BackgroundJobStatus::Queued
        | BackgroundJobStatus::Idle
        | BackgroundJobStatus::Succeeded
        | BackgroundJobStatus::Failed
            if job.process.pid.is_some() =>
        {
            let idle_since = job
                .process
                .completed_at_ms
                .unwrap_or(job.process.updated_at_ms);
            let left = job.linger_deadline_ms(idle_since).saturating_sub(now_ms);
            Some(if left == 0 {
                "leaving".to_string()
            } else {
                format!("lingers {}", brief_duration(left))
            })
        }
        _ => None,
    }
}

/// The state word on a session's row. A foreground job's worker parks
/// between turns whatever the record says it last did, so `Queued` with a
/// worker, `Idle` and `Succeeded` are all "idle"; `Queued` without one is
/// still starting.
fn session_state_word(job: &BackgroundJobState) -> &'static str {
    match job.process.status {
        BackgroundJobStatus::Queued if job.process.pid.is_none() => "starting",
        BackgroundJobStatus::Queued
        | BackgroundJobStatus::Idle
        | BackgroundJobStatus::Succeeded => "idle",
        BackgroundJobStatus::Running => "running",
        BackgroundJobStatus::NeedsInput => "needs input",
        BackgroundJobStatus::Failed => "failed",
        BackgroundJobStatus::Stopped => "stopped",
    }
}

fn session_row_detail(job: &BackgroundJobState, context: &RowContext<'_>) -> String {
    let mut detail = session_state_word(job).to_string();
    if let Some(presence) = session_presence(job, context.now_ms) {
        detail.push_str(" · ");
        detail.push_str(&presence);
    }
    if let Some(summary) = job.outcome.summary.as_deref().and_then(non_empty_trimmed) {
        detail.push_str(" · ");
        detail.push_str(summary);
    }
    detail
}

fn row_from_job(job: BackgroundJobState, context: &RowContext<'_>) -> AgentViewRow {
    let group = if job.outcome.pending_permission.is_some() {
        AgentViewGroup::NeedsInput
    } else if job.identity.pinned {
        AgentViewGroup::Pinned
    } else {
        match job.process.status {
            BackgroundJobStatus::Succeeded if job_has_pr(&job) => AgentViewGroup::ReadyForReview,
            BackgroundJobStatus::NeedsInput => AgentViewGroup::NeedsInput,
            BackgroundJobStatus::Queued | BackgroundJobStatus::Idle => AgentViewGroup::Idle,
            BackgroundJobStatus::Running => AgentViewGroup::Working,
            BackgroundJobStatus::Succeeded => AgentViewGroup::Completed,
            BackgroundJobStatus::Failed => AgentViewGroup::Failed,
            BackgroundJobStatus::Stopped => AgentViewGroup::Stopped,
        }
    };
    let detail = if job.lease.placement == rebon_session_host::JobPlacement::Foreground {
        session_row_detail(&job, context)
    } else if let Some(summary) = job.outcome.summary.as_deref().and_then(non_empty_trimmed) {
        format!(
            "{} · agent {} · {}",
            job.process.status.as_str(),
            job.identity.agent_type.as_deref().unwrap_or("default"),
            summary,
        )
    } else {
        format!(
            "{} · agent {} · session {}",
            job.process.status.as_str(),
            job.identity.agent_type.as_deref().unwrap_or("default"),
            job.identity.session_id.as_deref().unwrap_or("pending")
        )
    };
    let pr_status = pr_status_from_cached_statuses(&job.outcome.pull_requests).or_else(|| {
        job.outcome
            .summary
            .as_deref()
            .and_then(pr_status_from_summary)
    });
    let pr_count = if pr_status.is_some() {
        pr_count_from_cached_statuses(&job.outcome.pull_requests)
            .or_else(|| job.outcome.summary.as_deref().map(pr_count_from_summary))
            .unwrap_or(0)
    } else {
        0
    };
    let mut peek_lines = Vec::new();
    if let Some(summary) = job.outcome.summary.as_deref().and_then(non_empty_trimmed) {
        let mut header_lines = vec![format!("summary: {summary}")];
        let mut pr_urls = pr_urls_from_summary(&summary);
        header_lines.extend(pr_urls.iter().map(|url| format!("pr: {url}")));
        for status in &job.outcome.pull_requests {
            if !pr_urls
                .iter()
                .any(|url| url.eq_ignore_ascii_case(&status.url))
            {
                header_lines.push(format!("pr: {}", status.url));
                pr_urls.push(status.url.clone());
            }
            if let Some(line) = pr_status_peek_line(status) {
                header_lines.push(line);
            }
        }
        header_lines.extend(peek_lines);
        peek_lines = header_lines;
    } else if !job.outcome.pull_requests.is_empty() {
        let mut header_lines = Vec::new();
        for status in &job.outcome.pull_requests {
            header_lines.push(format!("pr: {}", status.url));
            if let Some(line) = pr_status_peek_line(status) {
                header_lines.push(line);
            }
        }
        header_lines.extend(peek_lines);
        peek_lines = header_lines;
    }
    let process_shape = process_shape_from_job(&job);
    AgentViewRow {
        id: job.identity.job_id.clone(),
        source: AgentViewRowSource::Job,
        group,
        placement: job.lease.placement,
        name: job.identity.name,
        detail,
        agent_name: job.identity.agent_type.clone(),
        session_id: job.identity.session_id,
        cwd: Some(job.identity.cwd),
        pinned: job.identity.pinned,
        order_key: job.identity.sort_order,
        peek_lines,
        pending_permission: job.outcome.pending_permission,
        process_shape,
        pr_status,
        pr_count,
    }
}

fn process_shape_from_job(job: &BackgroundJobState) -> AgentViewProcessShape {
    if job.process.pid.is_some() {
        return match job.process.status {
            BackgroundJobStatus::Running => AgentViewProcessShape::Active,
            _ => AgentViewProcessShape::Alive,
        };
    }
    match job.process.status {
        BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput => {
            AgentViewProcessShape::Active
        }
        BackgroundJobStatus::Queued => AgentViewProcessShape::Alive,
        BackgroundJobStatus::Idle
        | BackgroundJobStatus::Succeeded
        | BackgroundJobStatus::Failed
        | BackgroundJobStatus::Stopped => AgentViewProcessShape::Exited,
    }
}

fn push_agent_peek_text(lines: &mut Vec<String>, prefix: &str, text: &str) {
    lines.extend(
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| format!("{prefix}: {line}")),
    );
}

fn external_acp_task_peek_lines(task: &TaskSnapshot) -> Vec<String> {
    use rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry;

    if !crate::tui::agent_switcher::is_external_acp_agent_snapshot(task) {
        return Vec::new();
    }
    let TaskData::LocalAgent(data) = &task.data else {
        return Vec::new();
    };

    let streaming_assistant_index = data.streaming_text.as_deref().and_then(|streaming| {
        data.transcript.last().and_then(|entry| match entry {
            LocalAgentTranscriptEntry::Assistant { text } if streaming.starts_with(text) => {
                Some(data.transcript.len() - 1)
            }
            _ => None,
        })
    });
    let mut lines = Vec::new();
    for (index, entry) in data.transcript.iter().enumerate() {
        match entry {
            LocalAgentTranscriptEntry::User { text } => {
                push_agent_peek_text(&mut lines, "user", text);
            }
            LocalAgentTranscriptEntry::Thinking { text } => {
                push_agent_peek_text(&mut lines, "thinking", text);
            }
            LocalAgentTranscriptEntry::Assistant { text }
                if streaming_assistant_index != Some(index) =>
            {
                push_agent_peek_text(&mut lines, "assistant", text);
            }
            LocalAgentTranscriptEntry::Assistant { .. } => {}
            LocalAgentTranscriptEntry::ToolStart { name, activity, .. } => {
                lines.push(format!("tool: {name} · {activity}"));
            }
            LocalAgentTranscriptEntry::ToolProgress { name, message, .. } => {
                lines.push(format!("tool: {name} · {message}"));
            }
            LocalAgentTranscriptEntry::ToolFinish {
                name, ok, summary, ..
            } => {
                let status = if *ok { "completed" } else { "failed" };
                lines.push(format!("tool: {name} · {status} · {summary}"));
            }
        }
    }
    if let Some(streaming) = data.streaming_text.as_deref() {
        push_agent_peek_text(&mut lines, "assistant", streaming);
    }
    if lines.len() > 10 {
        lines.drain(..lines.len() - 10);
    }
    lines
}

fn row_from_task(task: &TaskSnapshot) -> AgentViewRow {
    let group = match &task.data {
        TaskData::InProcessTeammate(data) if data.awaiting_plan_approval => {
            AgentViewGroup::NeedsInput
        }
        _ if task.status == TaskStatus::Running
            && rebon_plugin_tasks::runtime::is_agent_snapshot_idle(task) =>
        {
            AgentViewGroup::Idle
        }
        _ => match task.status {
            TaskStatus::Pending | TaskStatus::Running => AgentViewGroup::Working,
            TaskStatus::Completed => AgentViewGroup::Completed,
            TaskStatus::Failed => AgentViewGroup::Failed,
            TaskStatus::Killed => AgentViewGroup::Stopped,
        },
    };
    let agent_name = match &task.data {
        TaskData::LocalAgent(data) => Some(data.agent_type.clone()),
        TaskData::InProcessTeammate(data) => Some(data.identity.agent_name.clone()),
        TaskData::LocalWorkflow(data) => Some(data.workflow_name.clone()),
        _ => None,
    };
    let mut peek_lines = vec![format!("title: {}", task.title)];
    if let Some(progress) = &task.last_progress {
        peek_lines.push(format!("progress: {progress}"));
    }
    if let Some(error) = &task.error {
        peek_lines.push(format!("error: {error}"));
    }
    if matches!(&task.data, TaskData::InProcessTeammate(data) if data.awaiting_plan_approval) {
        peek_lines.push("1. approve plan".to_string());
        peek_lines.push("2. reject / request changes".to_string());
    }
    peek_lines.extend(external_acp_task_peek_lines(task));
    peek_lines.push(format!("label: {}", tasks_view::snapshot_label(task)));
    AgentViewRow {
        id: task.id.to_string(),
        source: AgentViewRowSource::Task,
        group,
        placement: rebon_session_host::JobPlacement::Background,
        name: agent_name.clone().unwrap_or_else(|| task.title.clone()),
        // A parked worker keeps `status = Running` on purpose so it stays
        // resumable, so the raw word contradicts the Idle group this row was
        // just sorted into. Say what the row means, in the same vocabulary
        // the background wire uses.
        detail: format!(
            "{} · {}",
            task.kind.as_str(),
            if task.status == TaskStatus::Running
                && rebon_plugin_tasks::runtime::is_agent_snapshot_idle(task)
            {
                "idle"
            } else {
                task.status.as_str()
            }
        ),
        agent_name,
        session_id: None,
        cwd: None,
        pinned: false,
        order_key: task.start_time_ms as i64,
        peek_lines,
        pending_permission: None,
        process_shape: match task.status {
            TaskStatus::Running if rebon_plugin_tasks::runtime::is_agent_snapshot_idle(task) => {
                AgentViewProcessShape::Alive
            }
            TaskStatus::Pending | TaskStatus::Running => AgentViewProcessShape::Active,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Killed => {
                AgentViewProcessShape::Exited
            }
        },
        pr_status: None,
        pr_count: 0,
    }
}

fn row_matches_filter(row: &AgentViewRow, filter: &str) -> bool {
    let filter = filter.trim();
    if filter.is_empty() {
        return true;
    }
    for term in filter.split_whitespace() {
        if let Some(state) = term.strip_prefix("s:") {
            let state = match state {
                "blocked" => "needs-input",
                "ready" | "review" => "ready-for-review",
                other => other,
            };
            if row.group.state_filter() != state {
                return false;
            }
        } else if let Some(agent) = term.strip_prefix("a:") {
            let hay = row
                .agent_name
                .as_deref()
                .unwrap_or(&row.name)
                .to_ascii_lowercase();
            if !hay.contains(&agent.to_ascii_lowercase()) {
                return false;
            }
        } else {
            let hay = format!("{} {} {}", row.id, row.name, row.detail).to_ascii_lowercase();
            let needle = pr_filter_needle(term).unwrap_or_else(|| term.to_ascii_lowercase());
            if !hay.contains(&needle) {
                return false;
            }
        }
    }
    true
}

fn pr_filter_needle(term: &str) -> Option<String> {
    pr_reference_needles(term).into_iter().next()
}

fn pr_reference_needles(text: &str) -> Vec<String> {
    let mut needles = Vec::new();
    let lower = text.to_ascii_lowercase();

    let mut rest = lower.as_str();
    while let Some(idx) = rest.find("/pull/") {
        let after = &rest[idx + "/pull/".len()..];
        let number = after
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if number.is_empty() {
            rest = after;
        } else {
            push_pr_needle(&mut needles, number.clone());
            rest = &after[number.len()..];
        }
    }

    let mut rest = lower.as_str();
    while let Some(idx) = rest.find('#') {
        let after = &rest[idx + 1..];
        let number = after
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if number.is_empty() {
            rest = after;
        } else {
            push_pr_needle(&mut needles, number.clone());
            rest = &after[number.len()..];
        }
    }

    needles
}

fn push_pr_needle(needles: &mut Vec<String>, number: String) {
    if !needles.contains(&number) {
        needles.push(number);
    }
}

fn dispatch_text_is_filter_query(text: &str) -> bool {
    let mut saw_term = false;
    for term in text.split_whitespace() {
        saw_term = true;
        let is_agent_filter = term
            .strip_prefix("a:")
            .is_some_and(|agent| !agent.trim().is_empty());
        let is_state_filter = term
            .strip_prefix("s:")
            .is_some_and(|state| !state.trim().is_empty());
        if !is_agent_filter && !is_state_filter && pr_filter_needle(term).is_none() {
            return false;
        }
    }
    saw_term
}

fn row_matches_pr_reference(row: &AgentViewRow, needle: &str) -> bool {
    let mut texts = Vec::with_capacity(2 + row.peek_lines.len());
    texts.push(row.name.as_str());
    texts.push(row.detail.as_str());
    texts.extend(row.peek_lines.iter().map(String::as_str));
    texts.into_iter().any(|text| {
        pr_reference_needles(text)
            .into_iter()
            .any(|candidate| candidate == needle)
    })
}

fn job_has_pr(job: &BackgroundJobState) -> bool {
    if !job.outcome.pull_requests.is_empty() {
        return true;
    }
    job.outcome
        .summary
        .as_deref()
        .is_some_and(|summary| !pr_reference_needles(summary).is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestDotStatus {
    Waiting,
    Ready,
    Merged,
    Inactive,
}

impl PullRequestDotStatus {
    /// Color used to render the trailing dot on a row, per Agent
    /// Views spec (yellow / green / purple / grey).
    pub fn dot_style(self) -> Style {
        let color = match self {
            Self::Waiting => Color::Yellow,
            Self::Ready => Color::Green,
            // Magenta is the standard ANSI approximation for the spec's
            // purple — terminals without a full 256-color palette will
            // still render a distinct hue.
            Self::Merged => Color::Magenta,
            Self::Inactive => Color::DarkGray,
        };
        Style::default().fg(color)
    }
}

impl From<BackgroundPullRequestDotStatus> for PullRequestDotStatus {
    fn from(value: BackgroundPullRequestDotStatus) -> Self {
        match value {
            BackgroundPullRequestDotStatus::Waiting => Self::Waiting,
            BackgroundPullRequestDotStatus::Ready => Self::Ready,
            BackgroundPullRequestDotStatus::Merged => Self::Merged,
            BackgroundPullRequestDotStatus::Inactive => Self::Inactive,
        }
    }
}

fn pr_status_from_cached_statuses(
    statuses: &[BackgroundPullRequestStatus],
) -> Option<PullRequestDotStatus> {
    let dots = statuses
        .iter()
        .filter_map(|status| status.dot.map(PullRequestDotStatus::from))
        .collect::<Vec<_>>();
    combine_pr_statuses(&dots)
}

fn combine_pr_statuses(statuses: &[PullRequestDotStatus]) -> Option<PullRequestDotStatus> {
    if statuses.is_empty() {
        return None;
    }
    if statuses
        .iter()
        .all(|status| *status == PullRequestDotStatus::Merged)
    {
        return Some(PullRequestDotStatus::Merged);
    }
    if statuses.contains(&PullRequestDotStatus::Waiting) {
        return Some(PullRequestDotStatus::Waiting);
    }
    if statuses.contains(&PullRequestDotStatus::Inactive) {
        return Some(PullRequestDotStatus::Inactive);
    }
    Some(PullRequestDotStatus::Ready)
}

fn pr_count_from_cached_statuses(statuses: &[BackgroundPullRequestStatus]) -> Option<usize> {
    if statuses.is_empty() {
        return None;
    }
    let mut keys = Vec::new();
    for status in statuses {
        let key = (
            status.owner.to_ascii_lowercase(),
            status.repo.to_ascii_lowercase(),
            status.number,
        );
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Some(keys.len().max(1))
}

fn pr_status_peek_line(status: &BackgroundPullRequestStatus) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(dot) = status.dot {
        parts.push(dot.as_str().to_string());
    }
    if let Some(state) = status.state.as_deref().and_then(non_empty_trimmed) {
        parts.push(state.to_ascii_lowercase());
    }
    if let Some(checks) = status.checks_summary.as_deref().and_then(non_empty_trimmed) {
        parts.push(checks.to_string());
    }
    if let Some(review) = status
        .review_decision
        .as_deref()
        .and_then(non_empty_trimmed)
    {
        parts.push(format!("review {}", review.to_ascii_lowercase()));
    }
    if let Some(error) = status.error.as_deref().and_then(non_empty_trimmed) {
        parts.push(format!("unavailable: {error}"));
    }
    (!parts.is_empty()).then(|| format!("pr status: #{} {}", status.number, parts.join(" · ")))
}

fn pr_status_from_summary(summary: &str) -> Option<PullRequestDotStatus> {
    if pr_reference_needles(summary).is_empty() {
        return None;
    }
    let lower = summary.to_ascii_lowercase();
    Some(if lower.contains("merged") {
        PullRequestDotStatus::Merged
    } else if lower.contains("draft") || lower.contains("closed") {
        PullRequestDotStatus::Inactive
    } else if lower.contains("passed")
        || lower.contains("passing")
        || lower.contains("approved")
        || lower.contains("ready")
    {
        PullRequestDotStatus::Ready
    } else {
        PullRequestDotStatus::Waiting
    })
}

fn pr_count_from_summary(summary: &str) -> usize {
    pr_reference_needles(summary).len().max(1)
}

fn pr_urls_from_summary(summary: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for term in summary.split_whitespace() {
        let trimmed = term.trim_matches(|ch: char| {
            matches!(
                ch,
                ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '<' | '>'
            )
        });
        let lower = trimmed.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            continue;
        }
        if !lower.contains("/pull/") || pr_filter_needle(trimmed).is_none() {
            continue;
        }
        let url = trimmed.to_string();
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    urls
}

fn image_reference_ids(text: &str) -> std::collections::HashSet<u32> {
    let mut ids = std::collections::HashSet::new();
    let mut rest = text;
    while let Some(idx) = rest.find("[Image #") {
        let after = &rest[idx + "[Image #".len()..];
        let digits = after
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if !digits.is_empty()
            && after
                .as_bytes()
                .get(digits.len())
                .is_some_and(|byte| *byte == b']')
        {
            if let Ok(id) = digits.parse::<u32>() {
                ids.insert(id);
            }
        }
        rest = after;
    }
    ids
}

fn group_order(group: AgentViewGroup) -> u8 {
    match group {
        AgentViewGroup::Pinned => 0,
        AgentViewGroup::ReadyForReview => 1,
        AgentViewGroup::NeedsInput => 2,
        AgentViewGroup::Working => 3,
        AgentViewGroup::Idle => 4,
        AgentViewGroup::Completed => 5,
        AgentViewGroup::Failed => 6,
        AgentViewGroup::Stopped => 7,
    }
}

fn directory_group_key(row: &AgentViewRow) -> &str {
    row.cwd
        .as_deref()
        .filter(|cwd| !cwd.trim().is_empty())
        .unwrap_or("in-process")
}

fn non_empty_trimmed(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn clamp_to_char_boundary(value: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(value.len());
    while cursor > 0 && !value.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

/// Extract the leading `@<token>` agent name (without the `@`) from
/// an input string. Returns `None` if the input does not start with
/// `@` or the token is empty. Whitespace after the token is not
/// consumed — only the agent identifier is returned.
fn leading_agent_token(input: &str) -> Option<&str> {
    let trimmed = input.trim_start();
    let rest = trimmed.strip_prefix('@')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let token = &rest[..end];
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Return the input with the leading `@<token>` (and the whitespace
/// immediately after it) removed. If there is no `@<token>` prefix
/// the original input is returned unchanged.
fn strip_leading_agent_token(input: &str) -> &str {
    let trimmed = input.trim_start();
    let Some(rest) = trimmed.strip_prefix('@') else {
        return input;
    };
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    if end == 0 {
        return input;
    }
    rest[end..].trim_start()
}

/// Pick a default reply text for the peek-panel Tab-fill action.
///
/// Strategy, in order:
/// 1. If the most recent peek line ends with a question mark, echo
///    "yes" — likely a yes/no prompt.
/// 2. Otherwise pick a state-based stock answer (`approve`, `continue`,
///    `ship it`, etc.) tuned for the row's `AgentViewGroup`.
///
/// The suggestion is just a starting point — the user is expected to
/// edit before pressing Enter.
fn suggested_reply_for_row(row: &AgentViewRow) -> String {
    if let Some(line) = row
        .peek_lines
        .iter()
        .rev()
        .find(|line| line.trim_end().ends_with('?'))
    {
        let cleaned = line.trim_start();
        let cleaned = cleaned
            .split_once(':')
            .map(|(_, rest)| rest.trim())
            .unwrap_or(cleaned)
            .to_string();
        if !cleaned.is_empty() {
            return format!("yes — {cleaned}");
        }
    }
    match row.group {
        AgentViewGroup::NeedsInput => "yes, please proceed".to_string(),
        AgentViewGroup::ReadyForReview => "ship it".to_string(),
        AgentViewGroup::Idle => "continue".to_string(),
        AgentViewGroup::Working => "status update?".to_string(),
        AgentViewGroup::Completed => "follow up: ".to_string(),
        AgentViewGroup::Failed => "retry with: ".to_string(),
        AgentViewGroup::Stopped => "resume: ".to_string(),
        AgentViewGroup::Pinned => "continue".to_string(),
    }
}

/// Detect the Bash-mode prefix used by Agent View reply inputs.
///
/// Per the Agent Views spec: "Prefix a reply with `!` to send a Bash
/// command instead." A leading `!` is consumed and the remainder
/// (after trimming surrounding whitespace) is returned. An empty
/// command after stripping returns `None` so the caller falls back
/// to the regular reply path with a "input was empty" status.
fn strip_bash_prefix(text: &str) -> Option<String> {
    let rest = text.strip_prefix('!')?;
    let command = rest.trim().to_string();
    if command.is_empty() {
        None
    } else {
        Some(command)
    }
}

fn previous_char_boundary(value: &str, cursor: usize) -> usize {
    let cursor = clamp_to_char_boundary(value, cursor);
    if cursor == 0 {
        return 0;
    }
    value[..cursor]
        .char_indices()
        .last()
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

fn next_char_boundary(value: &str, cursor: usize) -> usize {
    let cursor = cursor.min(value.len());
    if cursor == value.len() {
        return value.len();
    }
    if value.is_char_boundary(cursor) {
        let Some(ch) = value[cursor..].chars().next() else {
            return value.len();
        };
        return cursor + ch.len_utf8();
    }
    next_char_boundary_if_inside(value, cursor)
}

fn next_char_boundary_if_inside(value: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(value.len());
    while cursor < value.len() && !value.is_char_boundary(cursor) {
        cursor += 1;
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_plugin_tasks::runtime::{
        InProcessTeammateData, LocalAgentData, LocalAgentTranscriptEntry, TaskData, TaskId,
        TeammateIdentity,
    };
    use std::path::PathBuf;

    fn test_runtime_fields() -> crate::background::BackgroundRuntimeFields {
        crate::background::BackgroundRuntimeFields {
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

    #[test]
    fn groups_jobs_by_status_and_filters_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "ship it".into(),
                PathBuf::from("."),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();
        job.process.status = BackgroundJobStatus::Running;
        store.write_state(&job).unwrap();

        let view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        assert_eq!(view.rows[0].group, AgentViewGroup::Working);

        let mut filtered = view.clone();
        filtered.filter = "s:completed".into();
        filtered.refresh(&store, store.list_jobs().unwrap(), &[]);
        assert!(filtered.rows.is_empty());
    }

    #[test]
    fn refresh_is_throttled_until_interval_elapses() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let view = AgentViewState::open(&store, Vec::new(), &[]);

        assert!(!view.refresh_is_due(view.last_refresh_at));
        assert!(!view.refresh_is_due(
            view.last_refresh_at + AGENT_VIEW_REFRESH_INTERVAL - Duration::from_millis(1)
        ));
        assert!(view.refresh_is_due(view.last_refresh_at + AGENT_VIEW_REFRESH_INTERVAL));
    }

    #[test]
    fn pr_filters_match_number_or_url() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "ship it".into(),
                PathBuf::from("."),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();
        job.outcome.summary = Some("opened https://example.com/org/repo/pull/2048".into());
        store.write_state(&job).unwrap();

        let mut hash = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        hash.filter = "#2048".into();
        hash.refresh(&store, store.list_jobs().unwrap(), &[]);
        assert_eq!(hash.rows.len(), 1);

        let mut url = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        url.filter = "https://example.com/org/repo/pull/2048".into();
        url.refresh(&store, store.list_jobs().unwrap(), &[]);
        assert_eq!(url.rows.len(), 1);
    }

    #[test]
    fn dispatch_pr_reference_selects_existing_job_instead_of_dispatching() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut other = store
            .create_job("other pr".into(), PathBuf::from("."), test_runtime_fields())
            .unwrap();
        other.process.status = BackgroundJobStatus::Succeeded;
        other.outcome.summary = Some("opened https://example.com/org/repo/pull/9999".into());
        other.identity.sort_order = 20;
        store.write_state(&other).unwrap();
        let mut matching = store
            .create_job(
                "target pr".into(),
                PathBuf::from("."),
                test_runtime_fields(),
            )
            .unwrap();
        matching.process.status = BackgroundJobStatus::Succeeded;
        matching.outcome.summary = Some("opened https://example.com/org/repo/pull/2048".into());
        matching.identity.sort_order = 10;
        store.write_state(&matching).unwrap();

        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        assert_eq!(view.selected_row().unwrap().id, other.identity.job_id);
        for ch in "review #2048, please".chars() {
            view.handle_key(&KeyEvent::from(KeyCode::Char(ch)));
        }

        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::Consumed
        );
        assert_eq!(view.selected_row().unwrap().id, matching.identity.job_id);
        assert!(view.peek_visible);
        assert!(view.input.is_empty());
        assert_eq!(
            view.status.as_deref(),
            Some("selected existing PR #2048: target pr")
        );
    }

    #[test]
    fn pr_summary_jobs_group_as_ready_for_review() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "ship it".into(),
                PathBuf::from("."),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();
        job.process.status = BackgroundJobStatus::Succeeded;
        job.outcome.summary = Some("opened https://example.com/org/repo/pull/2048".into());
        store.write_state(&job).unwrap();

        let view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);

        assert_eq!(view.rows[0].group, AgentViewGroup::ReadyForReview);
        // The trailing dot is rendered separately by `render_agent_row_line`
        // — the detail string itself is no longer expected to embed it.
        assert_eq!(view.rows[0].pr_status, Some(PullRequestDotStatus::Waiting));
        assert_eq!(view.rows[0].pr_count, 1);
        assert_eq!(
            view.rows[0].peek_lines.get(1).map(String::as_str),
            Some("pr: https://example.com/org/repo/pull/2048")
        );
    }

    #[test]
    fn cached_pr_status_overrides_summary_heuristic() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job("ship it".into(), PathBuf::from("."), test_runtime_fields())
            .unwrap();
        job.process.status = BackgroundJobStatus::Succeeded;
        job.outcome.summary = Some("opened https://github.com/org/repo/pull/2048".into());
        job.outcome.pull_requests = vec![BackgroundPullRequestStatus {
            url: "https://github.com/org/repo/pull/2048".into(),
            owner: "org".into(),
            repo: "repo".into(),
            number: 2048,
            dot: Some(BackgroundPullRequestDotStatus::Ready),
            state: Some("OPEN".into()),
            merge_state: Some("CLEAN".into()),
            review_decision: Some("APPROVED".into()),
            checks_summary: Some("3/3 checks passed".into()),
            error: None,
            updated_at_ms: 42,
        }];
        store.write_state(&job).unwrap();

        let view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);

        assert_eq!(view.rows[0].group, AgentViewGroup::ReadyForReview);
        assert_eq!(view.rows[0].pr_status, Some(PullRequestDotStatus::Ready));
        assert_eq!(view.rows[0].pr_count, 1);
        assert!(view.rows[0].peek_lines.iter().any(|line| {
            line == "pr status: #2048 ready · open · 3/3 checks passed · review approved"
        }));
    }

    #[test]
    fn pr_summary_counts_distinct_hash_and_url_references() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job("ship it".into(), PathBuf::from("."), test_runtime_fields())
            .unwrap();
        job.process.status = BackgroundJobStatus::Succeeded;
        job.outcome.summary =
            Some("ready #2048 and https://example.com/org/repo/pull/2049; duplicate #2048".into());
        store.write_state(&job).unwrap();

        let view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);

        assert_eq!(view.rows[0].group, AgentViewGroup::ReadyForReview);
        assert_eq!(view.rows[0].pr_status, Some(PullRequestDotStatus::Ready));
        assert_eq!(view.rows[0].pr_count, 2);
        assert!(view.rows[0]
            .peek_lines
            .iter()
            .any(|line| line == "pr: https://example.com/org/repo/pull/2049"));
        assert!(row_matches_filter(&view.rows[0], "#2048"));
        assert!(row_matches_filter(
            &view.rows[0],
            "https://example.com/org/repo/pull/2049"
        ));
    }

    #[test]
    fn pr_marker_renders_label_and_count() {
        let row = AgentViewRow {
            id: "j-pr".into(),
            source: AgentViewRowSource::Job,
            group: AgentViewGroup::ReadyForReview,
            placement: rebon_session_host::JobPlacement::Background,
            name: "review".into(),
            detail: "succeeded".into(),
            agent_name: None,
            session_id: None,
            cwd: None,
            pinned: false,
            order_key: 0,
            peek_lines: Vec::new(),
            pending_permission: None,
            process_shape: AgentViewProcessShape::Exited,
            pr_status: Some(PullRequestDotStatus::Ready),
            pr_count: 2,
        };

        let line = render_agent_row_line(&row, false, 80);
        let rendered = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.ends_with("2 PR ●"), "got: {rendered}");
    }

    #[test]
    fn leading_agent_token_picks_up_at_prefix_only() {
        assert_eq!(leading_agent_token("@explore find foo"), Some("explore"));
        assert_eq!(leading_agent_token("  @plan"), Some("plan"));
        assert_eq!(leading_agent_token("hello @plan"), None);
        assert_eq!(leading_agent_token("@"), None);
        assert_eq!(leading_agent_token(""), None);
    }

    #[test]
    fn strip_leading_agent_token_removes_token_and_one_separator() {
        assert_eq!(strip_leading_agent_token("@plan ship it"), "ship it");
        assert_eq!(strip_leading_agent_token("@plan"), "");
        assert_eq!(strip_leading_agent_token("ship it"), "ship it");
        assert_eq!(
            strip_leading_agent_token("@ leading-empty"),
            "@ leading-empty"
        );
    }

    #[test]
    fn tab_on_empty_dispatch_opens_subagent_browser() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        view.agent_picker_names = vec!["explore".into(), "reviewer".into()];

        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Tab)),
            AgentViewKeyOutcome::Consumed
        );
        assert!(view.agent_picker_visible);
        assert_eq!(view.agent_picker_index, Some(0));
        assert_eq!(view.input, "@explore ");

        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Tab)),
            AgentViewKeyOutcome::Consumed
        );
        assert!(view.agent_picker_visible);
        assert_eq!(view.agent_picker_index, Some(1));
        assert_eq!(view.input, "@reviewer ");
    }

    #[test]
    fn enter_in_subagent_browser_selects_without_dispatching() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        view.agent_picker_names = vec!["explore".into()];

        view.handle_key(&KeyEvent::from(KeyCode::Tab));
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::Consumed
        );
        assert!(!view.agent_picker_visible);
        assert_eq!(view.input, "@explore ");

        view.handle_key(&KeyEvent::from(KeyCode::Char('s')));
        assert_eq!(view.input, "@explore s");
    }

    #[test]
    fn suggested_reply_for_row_prefers_question_in_peek_lines() {
        let row = AgentViewRow {
            id: "j1".into(),
            source: AgentViewRowSource::Job,
            group: AgentViewGroup::NeedsInput,
            placement: rebon_session_host::JobPlacement::Background,
            name: "r".into(),
            detail: "".into(),
            agent_name: None,
            session_id: None,
            cwd: None,
            pinned: false,
            order_key: 0,
            peek_lines: vec!["summary: thinking".into(), "ask: ready to merge?".into()],
            pending_permission: None,
            process_shape: AgentViewProcessShape::Alive,
            pr_status: None,
            pr_count: 0,
        };
        let reply = suggested_reply_for_row(&row);
        assert!(reply.starts_with("yes"), "got: {reply}");
        assert!(reply.contains("ready to merge?"), "got: {reply}");
    }

    #[test]
    fn suggested_reply_for_row_falls_back_to_state_stock_answer() {
        let row = AgentViewRow {
            id: "j2".into(),
            source: AgentViewRowSource::Job,
            group: AgentViewGroup::ReadyForReview,
            placement: rebon_session_host::JobPlacement::Background,
            name: "r".into(),
            detail: "".into(),
            agent_name: None,
            session_id: None,
            cwd: None,
            pinned: false,
            order_key: 0,
            peek_lines: vec!["summary: done".into()],
            pending_permission: None,
            process_shape: AgentViewProcessShape::Exited,
            pr_status: None,
            pr_count: 0,
        };
        assert_eq!(suggested_reply_for_row(&row), "ship it");
    }

    #[test]
    fn strip_bash_prefix_extracts_command_or_returns_none() {
        assert_eq!(strip_bash_prefix("!ls"), Some("ls".to_string()));
        assert_eq!(
            strip_bash_prefix("! cargo check"),
            Some("cargo check".to_string())
        );
        assert_eq!(
            strip_bash_prefix("ls -la"),
            None,
            "no leading `!` keeps reply path"
        );
        assert_eq!(strip_bash_prefix("!"), None, "bare `!` is treated as empty");
        assert_eq!(
            strip_bash_prefix("!   "),
            None,
            "`!` with only whitespace is empty"
        );
    }

    /// The three dispatch keys must stay three different things. Enter
    /// fires a worker nobody watches, Shift+Enter runs it in this process,
    /// and Ctrl+O fires a worker this TUI mirrors — mixing any two of them
    /// up means an agent either dies with the terminal when the user
    /// expected it to survive, or costs a worker start when they wanted it
    /// to begin instantly.
    #[test]
    fn the_three_dispatch_keys_pick_three_different_placements() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let typed = "look at the parser";

        let dispatch_with = |key: KeyEvent| {
            let mut view = AgentViewState::open(&store, Vec::new(), &[]);
            view.input_focused = true;
            view.input_mode = AgentViewInputMode::Dispatch;
            for ch in typed.chars() {
                view.handle_key(&KeyEvent::from(KeyCode::Char(ch)));
            }
            view.handle_key(&key)
        };

        assert_eq!(
            dispatch_with(KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::DispatchPrompt(typed.to_string())
        );
        assert_eq!(
            dispatch_with(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            AgentViewKeyOutcome::DispatchPromptAndAttach(typed.to_string())
        );
        assert_eq!(
            dispatch_with(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            AgentViewKeyOutcome::DispatchPromptHosted(typed.to_string())
        );
    }

    /// Ctrl+N clears the composer like the other dispatch keys, and an
    /// empty one says so instead of starting a worker to do nothing.
    #[test]
    fn hosted_dispatch_clears_the_input_and_refuses_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, Vec::new(), &[]);
        view.input_focused = true;
        view.input_mode = AgentViewInputMode::Dispatch;

        assert_eq!(
            view.handle_key(&KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            AgentViewKeyOutcome::Consumed
        );
        assert_eq!(view.status.as_deref(), Some("input was empty"));

        for ch in "ship it".chars() {
            view.handle_key(&KeyEvent::from(KeyCode::Char(ch)));
        }
        assert_eq!(
            view.handle_key(&KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            AgentViewKeyOutcome::DispatchPromptHosted("ship it".to_string())
        );
        assert!(view.input.is_empty());
    }

    /// In a reply composer Ctrl+N is not a dispatch key at all — replying
    /// to a job has no "where does this run" question.
    #[test]
    fn ctrl_n_is_not_a_dispatch_key_while_replying() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, Vec::new(), &[]);
        view.input_focused = true;
        view.input_mode = AgentViewInputMode::ReplyJob("bg-1".into());
        for ch in "thanks".chars() {
            view.handle_key(&KeyEvent::from(KeyCode::Char(ch)));
        }

        let outcome = view.handle_key(&KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));

        assert!(
            !matches!(
                outcome,
                AgentViewKeyOutcome::DispatchPromptHosted(_)
                    | AgentViewKeyOutcome::DispatchPromptHostedWithImages { .. }
            ),
            "a reply must not be dispatched as a new hosted agent: {outcome:?}"
        );
    }

    fn build_outcome(
        state: &AgentViewState,
        mode: AgentViewInputMode,
        text: &str,
    ) -> AgentViewKeyOutcome {
        state.submit_input_text(
            text.to_string(),
            Vec::new(),
            DispatchPlacement::Detached,
            mode,
        )
    }

    #[test]
    fn submit_routes_bang_prefixed_reply_to_bash_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let state = AgentViewState::open(&store, Vec::new(), &[]);

        let outcome = build_outcome(
            &state,
            AgentViewInputMode::ReplyJob("job-1".into()),
            "!ls -la",
        );
        match outcome {
            AgentViewKeyOutcome::BashCommandToJob { job_id, command } => {
                assert_eq!(job_id, "job-1");
                assert_eq!(command, "ls -la");
            }
            other => panic!("expected BashCommandToJob, got {other:?}"),
        }

        let outcome = build_outcome(
            &state,
            AgentViewInputMode::ReplyTask("task-7".into()),
            "! cargo test",
        );
        match outcome {
            AgentViewKeyOutcome::BashCommandToTask { task_id, command } => {
                assert_eq!(task_id, "task-7");
                assert_eq!(command, "cargo test");
            }
            other => panic!("expected BashCommandToTask, got {other:?}"),
        }
    }

    #[test]
    fn submit_keeps_normal_reply_when_no_bang_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let state = AgentViewState::open(&store, Vec::new(), &[]);

        let outcome = build_outcome(
            &state,
            AgentViewInputMode::ReplyJob("job-2".into()),
            "please continue",
        );
        match outcome {
            AgentViewKeyOutcome::ReplyToJob { job_id, message } => {
                assert_eq!(job_id, "job-2");
                assert_eq!(message, "please continue");
            }
            other => panic!("expected ReplyToJob, got {other:?}"),
        }
    }

    #[test]
    fn state_icon_glyph_matches_agent_views_spec() {
        // Alive sessions use `✻` / animated `✽`. Exited / completed
        // sessions use `∙`. (The `✢` looping glyph applies to /loop
        // sessions, which the agent view doesn't surface as a separate
        // group yet.)
        assert_eq!(AgentViewGroup::Working.state_icon().0, "✽");
        assert_eq!(AgentViewGroup::NeedsInput.state_icon().0, "✻");
        assert_eq!(AgentViewGroup::Idle.state_icon().0, "✻");
        assert_eq!(AgentViewGroup::Pinned.state_icon().0, "✻");
        assert_eq!(AgentViewGroup::ReadyForReview.state_icon().0, "∙");
        assert_eq!(AgentViewGroup::Completed.state_icon().0, "∙");
        assert_eq!(AgentViewGroup::Failed.state_icon().0, "∙");
        assert_eq!(AgentViewGroup::Stopped.state_icon().0, "∙");
    }

    #[test]
    fn process_shape_tracks_lingering_completed_worker() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "follow up".into(),
                PathBuf::from("."),
                test_runtime_fields(),
            )
            .unwrap();
        job.process.status = BackgroundJobStatus::Succeeded;
        job.process.pid = Some(std::process::id());
        store.write_state(&job).unwrap();

        let view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);

        assert_eq!(view.rows[0].group, AgentViewGroup::Completed);
        assert_eq!(view.rows[0].process_shape, AgentViewProcessShape::Alive);
    }

    #[test]
    fn pr_dot_marks_ready_merged_and_inactive_states() {
        assert_eq!(
            pr_status_from_summary("PR https://example.com/o/r/pull/1 checks passed"),
            Some(PullRequestDotStatus::Ready)
        );
        assert_eq!(
            pr_status_from_summary("PR #1 checks passed"),
            Some(PullRequestDotStatus::Ready)
        );
        assert_eq!(
            pr_status_from_summary("merged https://example.com/o/r/pull/1"),
            Some(PullRequestDotStatus::Merged)
        );
        assert_eq!(
            pr_status_from_summary("draft https://example.com/o/r/pull/1"),
            Some(PullRequestDotStatus::Inactive)
        );
    }

    #[test]
    fn job_summary_is_used_for_row_detail_and_peek() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "ship it".into(),
                PathBuf::from("."),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();
        job.process.status = BackgroundJobStatus::Succeeded;
        job.outcome.summary = Some("completed: ship it".into());
        store.write_state(&job).unwrap();

        let view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);

        assert!(view.rows[0].detail.contains("completed: ship it"));
        assert_eq!(view.rows[0].peek_lines[0], "summary: completed: ship it");
    }

    #[test]
    fn builds_task_rows_and_agent_filter() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let task = TaskSnapshot::new_pending(
            TaskId::new("a1"),
            "Review code".into(),
            TaskData::LocalAgent(LocalAgentData {
                prompt: "review".into(),
                agent_type: "reviewer".into(),
                model: None,
                system: None,
                allowed_tools: None,
                token_count: 0,
                tool_use_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
                pending_messages: Vec::new(),
                retrieved: false,
            }),
        );
        let view = AgentViewState::open(&store, Vec::new(), &[task]);
        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].agent_name.as_deref(), Some("reviewer"));
        assert!(row_matches_filter(&view.rows[0], "a:review"));
        assert!(!row_matches_filter(&view.rows[0], "a:writer"));
    }

    #[test]
    fn external_acp_task_peek_shows_live_agent_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut task = TaskSnapshot::new_pending(
            TaskId::new("acp-agent"),
            "Inspect ACP flow".into(),
            TaskData::LocalAgent(LocalAgentData {
                prompt: "inspect".into(),
                agent_type: "claude-advisor".into(),
                model: Some("acp:claude:agent-default".into()),
                system: None,
                allowed_tools: None,
                token_count: 0,
                tool_use_count: 1,
                transcript: vec![
                    LocalAgentTranscriptEntry::Thinking {
                        text: "Tracing ACP routing".into(),
                    },
                    LocalAgentTranscriptEntry::Assistant {
                        text: "I found the routing gap".into(),
                    },
                    LocalAgentTranscriptEntry::ToolStart {
                        tool_use_id: "tool-1".into(),
                        name: "Read".into(),
                        input: serde_json::json!({"file_path": "src/lib.rs"}),
                        activity: "Reading src/lib.rs".into(),
                    },
                ],
                streaming_text: None,
                pending_messages: Vec::new(),
                retrieved: false,
            }),
        );
        task.status = TaskStatus::Running;

        let view = AgentViewState::open(&store, Vec::new(), &[task]);

        assert!(view.rows[0]
            .peek_lines
            .iter()
            .any(|line| line == "thinking: Tracing ACP routing"));
        assert!(view.rows[0]
            .peek_lines
            .iter()
            .any(|line| line == "assistant: I found the routing gap"));
        assert!(view.rows[0]
            .peek_lines
            .iter()
            .any(|line| line == "tool: Read · Reading src/lib.rs"));
    }

    #[test]
    fn cwd_scope_filters_jobs_but_keeps_tasks_on_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let scoped = store
            .create_job(
                "ship scoped".into(),
                PathBuf::from("/repo/a"),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();
        let other = store
            .create_job(
                "ship other".into(),
                PathBuf::from("/repo/b"),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();
        let task = TaskSnapshot::new_pending(
            TaskId::new("a1"),
            "Review code".into(),
            TaskData::LocalAgent(LocalAgentData {
                prompt: "review".into(),
                agent_type: "reviewer".into(),
                model: None,
                system: None,
                allowed_tools: None,
                token_count: 0,
                tool_use_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
                pending_messages: Vec::new(),
                retrieved: false,
            }),
        );

        let mut view = AgentViewState::open_with_cwd_scope(
            &store,
            store.list_jobs().unwrap(),
            &[task.clone()],
            Some("/repo/a".into()),
        );
        assert!(view.rows.iter().any(|row| row.id == scoped.identity.job_id));
        assert!(view.rows.iter().any(|row| row.id == "a1"));
        assert!(!view.rows.iter().any(|row| row.id == other.identity.job_id));

        view.refresh(&store, store.list_jobs().unwrap(), &[task]);
        assert!(view.rows.iter().any(|row| row.id == scoped.identity.job_id));
        assert!(view.rows.iter().any(|row| row.id == "a1"));
        assert!(!view.rows.iter().any(|row| row.id == other.identity.job_id));
    }

    #[test]
    fn group_headers_can_be_selected_and_collapsed() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "ship it".into(),
                PathBuf::from("."),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();
        job.process.status = BackgroundJobStatus::Running;
        store.write_state(&job).unwrap();

        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        assert!(view.selected_row().is_some());
        assert_eq!(
            view.handle_table_key(&KeyEvent::from(KeyCode::Up)),
            AgentViewKeyOutcome::Consumed
        );
        assert!(view.selected_row().is_none());

        assert_eq!(
            view.handle_table_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::Consumed
        );
        assert!(view
            .collapsed_groups
            .contains(&VisualGroup::State(AgentViewGroup::Working)));
        assert_eq!(
            view.visual_items(),
            vec![VisualItem::Group(VisualGroup::State(
                AgentViewGroup::Working
            ))]
        );

        assert_eq!(
            view.handle_table_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::Consumed
        );
        assert!(!view
            .collapsed_groups
            .contains(&VisualGroup::State(AgentViewGroup::Working)));
        assert_eq!(
            view.handle_table_key(&KeyEvent::from(KeyCode::Down)),
            AgentViewKeyOutcome::Consumed
        );
        assert_eq!(
            view.handle_table_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::AttachJob {
                job_id: job.identity.job_id
            }
        );
    }

    #[test]
    fn completed_group_collapses_older_rows_until_more_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        for index in 0..(COMPLETED_GROUP_VISIBLE_LIMIT + 2) {
            let mut job = store
                .create_job(
                    format!("done {index}"),
                    PathBuf::from("."),
                    test_runtime_fields(),
                )
                .unwrap();
            job.process.status = BackgroundJobStatus::Succeeded;
            job.identity.sort_order = index as i64;
            store.write_state(&job).unwrap();
        }

        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        let completed = VisualGroup::State(AgentViewGroup::Completed);
        assert_eq!(
            view.visual_items()
                .iter()
                .filter(|item| matches!(item, VisualItem::Row(_)))
                .count(),
            COMPLETED_GROUP_VISIBLE_LIMIT
        );
        assert!(view.visual_items().iter().any(|item| matches!(
            item,
            VisualItem::More { group, hidden: 2 } if *group == completed
        )));

        view.selected_visual_index = view
            .visual_items()
            .iter()
            .position(|item| matches!(item, VisualItem::More { .. }))
            .unwrap();
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::Consumed
        );
        assert_eq!(
            view.visual_items()
                .iter()
                .filter(|item| matches!(item, VisualItem::Row(_)))
                .count(),
            COMPLETED_GROUP_VISIBLE_LIMIT + 2
        );
    }

    #[test]
    fn ctrl_s_switches_between_state_and_directory_grouping() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let job = store
            .create_job(
                "ship it".into(),
                PathBuf::from("/repo/a"),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();

        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        assert!(matches!(
            view.visual_items()[0],
            VisualItem::Group(VisualGroup::State(_))
        ));
        assert_eq!(
            view.handle_table_key(&KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            AgentViewKeyOutcome::GroupingChanged(AgentViewGroupingPreference::Directory)
        );
        assert_eq!(
            view.visual_items()[0],
            VisualItem::Group(VisualGroup::Directory("/repo/a".into()))
        );
        assert_eq!(
            view.handle_table_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::AttachJob {
                job_id: job.identity.job_id
            }
        );
    }

    #[test]
    fn alt_number_opens_nth_row_in_current_group_and_blocked_filter_aliases_needs_input() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let first = store
            .create_job(
                "first".into(),
                PathBuf::from("."),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();
        let mut second = store
            .create_job(
                "second".into(),
                PathBuf::from("."),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();
        second.identity.sort_order = first.identity.sort_order + 1;
        store.write_state(&second).unwrap();

        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        assert_eq!(
            view.handle_table_key(&KeyEvent::new(KeyCode::Char('2'), KeyModifiers::ALT)),
            AgentViewKeyOutcome::AttachJob {
                job_id: first.identity.job_id
            }
        );

        let needs_input = AgentViewRow {
            id: "needs".into(),
            source: AgentViewRowSource::Task,
            group: AgentViewGroup::NeedsInput,
            placement: rebon_session_host::JobPlacement::Background,
            name: "blocked".into(),
            detail: String::new(),
            agent_name: None,
            session_id: None,
            cwd: None,
            pinned: false,
            order_key: 0,
            peek_lines: Vec::new(),
            pending_permission: None,
            process_shape: AgentViewProcessShape::Active,
            pr_status: None,
            pr_count: 0,
        };
        assert!(row_matches_filter(&needs_input, "s:blocked"));
        assert!(row_matches_filter(&needs_input, "s:needs-input"));
    }

    #[test]
    fn mouse_click_row_attaches_job() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "ship it".into(),
                PathBuf::from("."),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();
        job.process.status = BackgroundJobStatus::Running;
        store.write_state(&job).unwrap();

        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        view.set_render_area_for_test(Rect::new(0, 0, 100, 24));

        let outcome = view.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 5,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(
            outcome,
            AgentViewKeyOutcome::AttachJob {
                job_id: job.identity.job_id
            }
        );
    }

    #[test]
    fn job_management_keys_emit_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let job = store
            .create_job(
                "ship it".into(),
                PathBuf::from("."),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();

        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Char('R'))),
            AgentViewKeyOutcome::RespawnJob {
                job_id: job.identity.job_id.clone()
            }
        );
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Delete)),
            AgentViewKeyOutcome::Consumed
        );
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Delete)),
            AgentViewKeyOutcome::RemoveJob {
                job_id: job.identity.job_id.clone()
            }
        );
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Backspace)),
            AgentViewKeyOutcome::Consumed
        );
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Backspace)),
            AgentViewKeyOutcome::RemoveJob {
                job_id: job.identity.job_id
            }
        );
    }

    #[test]
    fn ctrl_x_stops_then_removes_same_job_on_second_press() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let job = store
            .create_job(
                "ship it".into(),
                PathBuf::from("."),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();

        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(
            view.handle_key(&ctrl_x),
            AgentViewKeyOutcome::StopJob {
                job_id: job.identity.job_id.clone()
            }
        );
        assert_eq!(
            view.handle_key(&ctrl_x),
            AgentViewKeyOutcome::RemoveJob {
                job_id: job.identity.job_id
            }
        );
    }

    #[test]
    fn ctrl_x_stops_then_removes_same_task_on_second_press() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut task = TaskSnapshot::new_pending(
            TaskId::new("agent-stuck"),
            "Stuck agent".into(),
            TaskData::LocalAgent(LocalAgentData {
                prompt: "keep working".into(),
                agent_type: "general-purpose".into(),
                model: None,
                system: None,
                allowed_tools: None,
                token_count: 0,
                tool_use_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
                pending_messages: Vec::new(),
                retrieved: false,
            }),
        );
        task.status = TaskStatus::Running;

        let mut view = AgentViewState::open(&store, Vec::new(), &[task]);
        let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(
            view.handle_key(&ctrl_x),
            AgentViewKeyOutcome::StopTask {
                task_id: "agent-stuck".into()
            }
        );
        assert_eq!(
            view.handle_key(&ctrl_x),
            AgentViewKeyOutcome::RemoveTask {
                task_id: "agent-stuck".into()
            }
        );
    }

    #[test]
    fn ctrl_x_group_header_removes_all_job_rows_in_group_after_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut first = store
            .create_job("ship one".into(), PathBuf::from("."), test_runtime_fields())
            .unwrap();
        first.process.status = BackgroundJobStatus::Running;
        first.identity.sort_order = 10;
        store.write_state(&first).unwrap();
        let mut second = store
            .create_job("ship two".into(), PathBuf::from("."), test_runtime_fields())
            .unwrap();
        second.process.status = BackgroundJobStatus::Running;
        second.identity.sort_order = 20;
        store.write_state(&second).unwrap();

        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        assert!(view.selected_row().is_some());
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Up)),
            AgentViewKeyOutcome::Consumed
        );
        assert!(view.selected_row().is_none());

        let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(view.handle_key(&ctrl_x), AgentViewKeyOutcome::Consumed);
        assert_eq!(
            view.status.as_deref(),
            Some("press Ctrl+X again within 2s to remove 2 job(s) in Working")
        );
        match view.handle_key(&ctrl_x) {
            AgentViewKeyOutcome::RemoveGroup {
                group_label,
                mut job_ids,
            } => {
                assert_eq!(group_label, "Working");
                job_ids.sort();
                let mut expected = vec![first.identity.job_id, second.identity.job_id];
                expected.sort();
                assert_eq!(job_ids, expected);
            }
            other => panic!("expected RemoveGroup, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_input_emits_prompt_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, Vec::new(), &[]);
        assert!(view.is_input_focused());
        assert!(!view.peek_visible);
        for ch in "hello".chars() {
            view.handle_key(&KeyEvent::from(KeyCode::Char(ch)));
        }
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::DispatchPrompt("hello".into())
        );
    }

    #[test]
    fn slash_focuses_filter_input_and_updates_filter() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut running = store
            .create_job("ship run".into(), PathBuf::from("."), test_runtime_fields())
            .unwrap();
        running.process.status = BackgroundJobStatus::Running;
        store.write_state(&running).unwrap();
        let mut completed = store
            .create_job(
                "ship done".into(),
                PathBuf::from("."),
                test_runtime_fields(),
            )
            .unwrap();
        completed.process.status = BackgroundJobStatus::Succeeded;
        store.write_state(&completed).unwrap();

        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Char('/'))),
            AgentViewKeyOutcome::FilterChanged
        );
        for ch in "s:completed".chars() {
            assert_eq!(
                view.handle_key(&KeyEvent::from(KeyCode::Char(ch))),
                AgentViewKeyOutcome::FilterChanged
            );
        }
        assert_eq!(view.filter, "s:completed");
        view.refresh(&store, store.list_jobs().unwrap(), &[]);
        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].id, completed.identity.job_id);
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::FilterChanged
        );
        assert!(view.input.is_empty());
        assert!(view.is_input_focused());
    }

    #[test]
    fn dispatch_filter_syntax_applies_filter_instead_of_dispatching() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut running = store
            .create_job("ship run".into(), PathBuf::from("."), test_runtime_fields())
            .unwrap();
        running.process.status = BackgroundJobStatus::Running;
        store.write_state(&running).unwrap();
        let mut completed = store
            .create_job(
                "ship done".into(),
                PathBuf::from("."),
                test_runtime_fields(),
            )
            .unwrap();
        completed.process.status = BackgroundJobStatus::Succeeded;
        store.write_state(&completed).unwrap();

        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        for ch in "s:completed".chars() {
            view.handle_key(&KeyEvent::from(KeyCode::Char(ch)));
        }

        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::FilterChanged
        );
        assert_eq!(view.filter, "s:completed");
        assert_eq!(view.status.as_deref(), Some("filter applied: s:completed"));
        view.refresh(&store, store.list_jobs().unwrap(), &[]);
        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].id, completed.identity.job_id);
        assert!(view.input.is_empty());
    }

    #[test]
    fn ctrl_c_clears_input_then_requests_session_cancel_or_exit() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, Vec::new(), &[]);
        for ch in "hello".chars() {
            view.handle_key(&KeyEvent::from(KeyCode::Char(ch)));
        }
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert_eq!(view.handle_key(&ctrl_c), AgentViewKeyOutcome::Consumed);
        assert!(view.input.is_empty());
        assert_eq!(view.status.as_deref(), Some("input cleared"));
        assert_eq!(view.handle_key(&ctrl_c), AgentViewKeyOutcome::CancelOrExit);
    }

    #[test]
    fn input_ignores_release_events_to_avoid_ime_echo() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, Vec::new(), &[]);
        for ch in "ni你".chars() {
            view.handle_key(&KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()));
            view.handle_key(&KeyEvent::new_with_kind(
                KeyCode::Char(ch),
                KeyModifiers::empty(),
                KeyEventKind::Release,
            ));
        }
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::DispatchPrompt("ni你".into())
        );
    }

    #[test]
    fn ctrl_g_requests_external_editor_for_input() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, Vec::new(), &[]);
        for ch in "hello".chars() {
            view.handle_key(&KeyEvent::from(KeyCode::Char(ch)));
        }
        assert_eq!(
            view.handle_key(&KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            AgentViewKeyOutcome::EditInputExternally {
                text: "hello".into()
            }
        );

        view.replace_input_from_external_editor("edited prompt".into());
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::DispatchPrompt("edited prompt".into())
        );
    }

    #[test]
    fn dispatch_input_with_image_emits_image_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, Vec::new(), &[]);
        view.paste_image(crate::tui::clipboard_image::ClipboardImage {
            data: "abc".into(),
            media_type: "image/png".into(),
            filename: Some("shot.png".into()),
            source_path: None,
        });
        for ch in " inspect".chars() {
            view.handle_key(&KeyEvent::from(KeyCode::Char(ch)));
        }

        match view.handle_key(&KeyEvent::from(KeyCode::Enter)) {
            AgentViewKeyOutcome::DispatchPromptWithImages { prompt, images } => {
                assert_eq!(prompt, "[Image #1] inspect");
                assert_eq!(images.len(), 1);
                assert_eq!(images[0].content, "abc");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn shift_enter_dispatch_input_emits_attach_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, Vec::new(), &[]);
        for ch in "hello".chars() {
            view.handle_key(&KeyEvent::from(KeyCode::Char(ch)));
        }
        assert_eq!(
            view.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            AgentViewKeyOutcome::DispatchPromptAndAttach("hello".into())
        );
    }

    #[test]
    fn space_toggles_peek_panel_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, Vec::new(), &[]);

        assert!(!view.peek_visible);
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Char(' '))),
            AgentViewKeyOutcome::Consumed
        );
        assert!(view.peek_visible);
        assert_eq!(view.status.as_deref(), Some("peek panel shown"));
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Char(' '))),
            AgentViewKeyOutcome::Consumed
        );
        assert!(!view.peek_visible);
    }

    #[test]
    fn space_on_exited_job_requests_peek_warm() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job("done".into(), PathBuf::from("."), test_runtime_fields())
            .unwrap();
        job.process.status = BackgroundJobStatus::Succeeded;
        job.identity.session_id = Some("sess-done".into());
        store.write_state(&job).unwrap();
        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);

        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Char(' '))),
            AgentViewKeyOutcome::WarmPeekJob {
                job_id: job.identity.job_id
            }
        );
        assert!(view.peek_visible);
    }

    #[test]
    fn job_peek_io_is_lazy_until_panel_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job("running".into(), PathBuf::from("."), test_runtime_fields())
            .unwrap();
        job.process.status = BackgroundJobStatus::Running;
        store.write_state(&job).unwrap();
        store
            .append_log_line(&job.identity.job_id, "live output")
            .unwrap();
        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);

        assert!(view.rows[0].peek_lines.is_empty());
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Char(' '))),
            AgentViewKeyOutcome::RefreshPeek
        );
        view.refresh(&store, store.list_jobs().unwrap(), &[]);
        assert!(view.rows[0]
            .peek_lines
            .iter()
            .any(|line| line == "log: live output"));
    }

    #[test]
    fn right_returns_to_session_while_enter_opens_selected_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job("ship it".into(), PathBuf::from("."), test_runtime_fields())
            .unwrap();
        job.process.status = BackgroundJobStatus::Running;
        store.write_state(&job).unwrap();

        let mut right_view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        assert_eq!(
            right_view.handle_key(&KeyEvent::from(KeyCode::Right)),
            AgentViewKeyOutcome::ReturnToSession
        );

        let mut enter_view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        assert_eq!(
            enter_view.handle_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::AttachJob {
                job_id: job.identity.job_id
            }
        );
    }

    #[test]
    fn dispatch_hint_mentions_right_return_and_agents_command() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let view = AgentViewState::open(&store, Vec::new(), &[]);

        let hint = view.contextual_hint();
        assert!(hint.contains("→ return"));
        assert!(hint.contains("type to dispatch"));
        assert!(hint.contains("Enter attach"));
    }

    #[test]
    fn esc_hides_peek_before_dismissing_agent_view() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, Vec::new(), &[]);
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Char(' '))),
            AgentViewKeyOutcome::Consumed
        );
        assert!(view.peek_visible);

        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Esc)),
            AgentViewKeyOutcome::Consumed
        );
        assert!(!view.peek_visible);
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Esc)),
            AgentViewKeyOutcome::Dismiss
        );
    }

    #[test]
    fn question_mark_shows_shortcuts() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, Vec::new(), &[]);

        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Char('?'))),
            AgentViewKeyOutcome::Consumed
        );
        let status = view.status.as_deref().unwrap_or_default();
        assert!(status.contains("Enter attach"));
        assert!(status.contains("? help"));
    }

    #[test]
    fn input_backspace_handles_multibyte_characters() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, Vec::new(), &[]);
        for ch in "nih你你好好".chars() {
            view.handle_key(&KeyEvent::from(KeyCode::Char(ch)));
        }
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Backspace)),
            AgentViewKeyOutcome::Consumed
        );
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::DispatchPrompt("nih你你好".into())
        );
    }

    #[test]
    fn input_cursor_moves_by_char_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, Vec::new(), &[]);
        for ch in "a好b".chars() {
            view.handle_key(&KeyEvent::from(KeyCode::Char(ch)));
        }
        view.handle_key(&KeyEvent::from(KeyCode::Left));
        view.handle_key(&KeyEvent::from(KeyCode::Backspace));
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::DispatchPrompt("ab".into())
        );
    }

    #[test]
    fn contextual_hint_changes_for_input_and_peek() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut view = AgentViewState::open(&store, Vec::new(), &[]);

        assert!(view.contextual_hint().contains("→ return"));
        view.handle_key(&KeyEvent::from(KeyCode::Char('h')));
        assert!(view.contextual_hint().contains("Enter dispatch"));
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Esc)),
            AgentViewKeyOutcome::Consumed
        );
        assert!(view.contextual_hint().contains("→ return"));
    }

    #[test]
    fn job_reply_input_emits_reply_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "ship it".into(),
                PathBuf::from("."),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();
        job.process.status = BackgroundJobStatus::Idle;
        job.identity.session_id = Some("sess-one".into());
        store.write_state(&job).unwrap();

        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Char(' '))),
            AgentViewKeyOutcome::WarmPeekJob {
                job_id: job.identity.job_id.clone()
            }
        );
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Char('r'))),
            AgentViewKeyOutcome::Consumed
        );
        for ch in "continue".chars() {
            view.handle_key(&KeyEvent::from(KeyCode::Char(ch)));
        }
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Enter)),
            AgentViewKeyOutcome::ReplyToJob {
                job_id: job.identity.job_id,
                message: "continue".into()
            }
        );
    }

    #[test]
    fn number_key_answers_selected_needs_input_job_permission() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "ship it".into(),
                PathBuf::from("."),
                crate::background::BackgroundRuntimeFields {
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
                },
            )
            .unwrap();
        job.process.status = BackgroundJobStatus::Succeeded;
        job.identity.session_id = Some("sess-one".into());
        let endpoint = BackgroundIpcEndpoint {
            pid: 42,
            port: 4000,
            token: "endpoint-generation".into(),
        };
        job.outcome.pending_permission = Some(BackgroundPermissionQuerySnapshot {
            query_id: 77,
            turn_generation: 1,
            endpoint: Some(endpoint.clone()),
            tool: Some("Bash".into()),
            tool_call_id: Some("tool-1".into()),
            session_id: Some("sess-one".into()),
            title: Some("Run command".into()),
            message: None,
            tool_input: None,
            metadata: None,
            options: vec![
                BackgroundPermissionOptionSnapshot {
                    option_id: "allow_once".into(),
                    label: "Allow once".into(),
                    kind: "AllowOnce".into(),
                },
                BackgroundPermissionOptionSnapshot {
                    option_id: "deny".into(),
                    label: "Deny".into(),
                    kind: "Deny".into(),
                },
            ],
        });
        store.write_state(&job).unwrap();

        let mut view = AgentViewState::open(&store, store.list_jobs().unwrap(), &[]);
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Char('2'))),
            AgentViewKeyOutcome::AnswerJobPermission {
                job_id: job.identity.job_id,
                query_id: 77,
                turn_generation: 1,
                endpoint: Some(endpoint),
                option_id: "deny".into(),
            }
        );
    }

    #[test]
    fn number_key_answers_selected_needs_input_task_choice() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let task = TaskSnapshot::new_pending(
            TaskId::new("team-task"),
            "Plan review".into(),
            TaskData::InProcessTeammate(Box::new(InProcessTeammateData {
                identity: TeammateIdentity {
                    agent_id: "reviewer@team".into(),
                    agent_name: "reviewer".into(),
                    team_name: "team".into(),
                    color: None,
                    plan_mode_required: true,
                    parent_session_id: "sess-main".into(),
                },
                prompt: "make a plan".into(),
                model: None,
                model_profile: None,
                permission_mode: "plan".into(),
                awaiting_plan_approval: true,
                is_idle: false,
                shutdown_requested: false,
                pending_user_messages: Vec::new(),
                tool_use_count: 0,
                token_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
            })),
        );
        let mut view = AgentViewState::open(&store, Vec::new(), &[task]);

        assert_eq!(view.rows[0].group, AgentViewGroup::NeedsInput);
        assert!(view.rows[0]
            .peek_lines
            .iter()
            .any(|line| line == "1. approve plan"));
        assert_eq!(
            view.handle_key(&KeyEvent::from(KeyCode::Char('2'))),
            AgentViewKeyOutcome::AnswerTaskChoice {
                task_id: "team-task".into(),
                option_index: 1
            }
        );
    }

    fn foreground_session_job(store: &BackgroundStore, name: &str) -> BackgroundJobState {
        let mut job = store
            .create_job(name.into(), PathBuf::from("."), test_runtime_fields())
            .unwrap();
        job.lease.placement = rebon_session_host::JobPlacement::Foreground;
        job.identity.session_id = Some(format!("sess-{}", name.replace(' ', "-")));
        job
    }

    /// A worker that `list_jobs` will find alive: this very process.
    fn give_live_worker(job: &mut BackgroundJobState) {
        let pid = std::process::id();
        job.process.pid = Some(pid);
        job.process.pid_identity = rebon_session_host::process_identity(pid);
    }

    fn rendered_text(row: &AgentViewRow) -> String {
        render_agent_row_line(row, false, 100)
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    /// Every `rebon` makes a foreground job. One being
    /// driven — this terminal's own, or anyone's with a fresh lease — is
    /// an open window, not a background task, and is not listed. One
    /// whose worker runs for nobody is, first, under "Sessions" (§19.9).
    #[test]
    fn a_session_being_driven_is_not_listed_and_a_lingering_one_comes_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let now = rebon_session_host::now_ms();
        let mut agent = store
            .create_job("ship it".into(), PathBuf::from("."), test_runtime_fields())
            .unwrap();
        agent.process.status = BackgroundJobStatus::Running;
        store.write_state(&agent).unwrap();
        let mut mine = foreground_session_job(&store, "session mine");
        mine.process.status = BackgroundJobStatus::Running;
        give_live_worker(&mut mine);
        store.write_state(&mine).unwrap();
        let mut watched = foreground_session_job(&store, "session watched");
        watched.process.status = BackgroundJobStatus::Running;
        give_live_worker(&mut watched);
        watched.touch_client_lease(
            rebon_session_host::ClientLease {
                client_id: "tui-2".into(),
                kind: rebon_session_host::ClientLeaseKind::Tui,
                pid: None,
                updated_at_ms: now,
            },
            now,
        );
        store.write_state(&watched).unwrap();
        let mut alone = foreground_session_job(&store, "session alone");
        alone.process.status = BackgroundJobStatus::Idle;
        give_live_worker(&mut alone);
        store.write_state(&alone).unwrap();

        let view = AgentViewState::open_with_grouping(
            &store,
            store.list_jobs().unwrap(),
            &[],
            None,
            AgentViewGroupingPreference::State,
            Some(mine.identity.job_id.clone()),
        );

        assert!(
            view.rows.iter().all(|row| row.id != mine.identity.job_id),
            "this terminal's own session is its screen, not a task"
        );
        assert!(
            view.rows
                .iter()
                .all(|row| row.id != watched.identity.job_id),
            "a session with a fresh lease is somebody's window"
        );
        let items = view.visual_items();
        assert_eq!(items[0], VisualItem::Group(VisualGroup::Sessions));
        assert!(matches!(items[1], VisualItem::Row(_)));
        assert!(
            is_session_row(&view.rows[0]),
            "the lingering session comes first"
        );
        assert!(items.contains(&VisualItem::Group(VisualGroup::State(
            AgentViewGroup::Working
        ))));
        let alone_row = view
            .rows
            .iter()
            .find(|row| row.id == alone.identity.job_id)
            .unwrap();
        assert!(
            alone_row.detail.starts_with("idle · lingers"),
            "{}",
            alone_row.detail
        );
        assert_eq!(
            alone_row.group,
            AgentViewGroup::Idle,
            "the state group is still the row's state, for filters and icons"
        );
        assert!(rendered_text(alone_row).contains("[session]"));
        let agent_row = view
            .rows
            .iter()
            .find(|row| row.id == agent.identity.job_id)
            .unwrap();
        assert!(!is_session_row(agent_row));
        assert_eq!(agent_row.group, AgentViewGroup::Working);
        assert!(rendered_text(agent_row).contains("[job]"));
    }

    /// A listed session's row says how long its worker lingers before it
    /// leaves; past the deadline it is leaving, and one
    /// still finishing a turn just says its state.
    #[test]
    fn a_session_row_says_how_long_its_worker_lingers() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let now = rebon_session_host::now_ms();
        let mut job = foreground_session_job(&store, "session alone");
        job.process.status = BackgroundJobStatus::Idle;
        job.process.pid = Some(4242);
        job.process.completed_at_ms = Some(now.saturating_sub(60_000));
        let context = RowContext {
            now_ms: now,
            this_terminal_job_id: None,
        };
        assert_eq!(session_row_detail(&job, &context), "idle · lingers 9 min");
        // Past its deadline the worker is on its way out.
        job.process.completed_at_ms = Some(now.saturating_sub(11 * 60 * 1000));
        assert_eq!(session_row_detail(&job, &context), "idle · leaving");
        job.process.status = BackgroundJobStatus::NeedsInput;
        assert_eq!(session_row_detail(&job, &context), "needs input");
    }

    /// What earns a session its row: a worker running for nobody. A fresh
    /// lease means somebody is on it, this terminal's own is the screen
    /// the list is on, and a record without a worker is the resume
    /// dialog's business — none of those are background tasks (§8.1).
    #[test]
    fn a_session_is_listed_only_while_its_worker_runs_for_nobody() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let now = rebon_session_host::now_ms();
        let context = RowContext {
            now_ms: now,
            this_terminal_job_id: None,
        };
        let mut job = foreground_session_job(&store, "session alone");
        job.process.status = BackgroundJobStatus::Idle;
        job.process.pid = Some(4242);
        assert!(session_is_listed(&job, &context));

        // A lease somebody keeps renewing: their window, not a task.
        job.touch_client_lease(
            rebon_session_host::ClientLease {
                client_id: "tui-1".into(),
                kind: rebon_session_host::ClientLeaseKind::Tui,
                pid: None,
                updated_at_ms: now,
            },
            now,
        );
        assert!(!session_is_listed(&job, &context));
        // A lease nobody renewed is not a presence.
        job.lease.client_leases.clear();
        job.lease
            .client_leases
            .push(rebon_session_host::ClientLease {
                client_id: "tui-stale".into(),
                kind: rebon_session_host::ClientLeaseKind::Tui,
                pid: None,
                updated_at_ms: now.saturating_sub(rebon_session_host::CLIENT_LEASE_TTL_MS + 1),
            });
        assert!(session_is_listed(&job, &context));

        // This terminal's own stays out even before its first lease lands.
        let own = RowContext {
            now_ms: now,
            this_terminal_job_id: Some(job.identity.job_id.as_str()),
        };
        assert!(!session_is_listed(&job, &own));

        // A turn running unattended still shows — it is spending with
        // nobody watching; a stopped or dead worker does not.
        job.process.status = BackgroundJobStatus::Running;
        assert!(session_is_listed(&job, &context));
        job.process.status = BackgroundJobStatus::Stopped;
        assert!(!session_is_listed(&job, &context));
        job.process.status = BackgroundJobStatus::Idle;
        job.process.pid = None;
        assert!(!session_is_listed(&job, &context));
    }

    /// Enter on a lingering session attaches to it, like any job. And the
    /// group as a whole cannot be removed with one key: these are hosts
    /// someone may come back to, not a pile of finished jobs.
    #[test]
    fn a_lingering_session_attaches_and_its_group_resists_removal() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut other = foreground_session_job(&store, "session other");
        other.process.status = BackgroundJobStatus::Idle;
        give_live_worker(&mut other);
        store.write_state(&other).unwrap();

        let mut view = AgentViewState::open_with_grouping(
            &store,
            store.list_jobs().unwrap(),
            &[],
            None,
            AgentViewGroupingPreference::State,
            None,
        );
        let other_index = view
            .rows
            .iter()
            .position(|row| row.id == other.identity.job_id)
            .unwrap();
        view.select_row_index(other_index);
        assert_eq!(
            view.open_selected_outcome(),
            AgentViewKeyOutcome::AttachJob {
                job_id: other.identity.job_id.clone()
            }
        );

        view.selected_visual_index = 0;
        assert_eq!(
            view.selected_visual_item(),
            Some(VisualItem::Group(VisualGroup::Sessions))
        );
        let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(view.handle_key(&ctrl_x), AgentViewKeyOutcome::Consumed);
        assert!(view
            .status
            .as_deref()
            .is_some_and(|status| status.contains("one at a time")));
        assert_eq!(
            view.handle_key(&ctrl_x),
            AgentViewKeyOutcome::Consumed,
            "no confirmation window opens for the sessions group"
        );
    }

    /// Grouped by directory, a session sits under its directory like any
    /// job — the directory is the point of that view — and still wears
    /// its badge.
    #[test]
    fn directory_grouping_keeps_sessions_under_their_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut session = foreground_session_job(&store, "session here");
        session.process.status = BackgroundJobStatus::Idle;
        give_live_worker(&mut session);
        store.write_state(&session).unwrap();

        let view = AgentViewState::open_with_grouping(
            &store,
            store.list_jobs().unwrap(),
            &[],
            None,
            AgentViewGroupingPreference::Directory,
            None,
        );
        let items = view.visual_items();
        assert!(matches!(
            items[0],
            VisualItem::Group(VisualGroup::Directory(_))
        ));
        assert!(!items.contains(&VisualItem::Group(VisualGroup::Sessions)));
        assert!(rendered_text(&view.rows[0]).contains("[session]"));
    }
}

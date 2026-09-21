//! Inline permission overlay for the local TUI.
//!
//! Permissions render as an **inline overlay** inside the scroll region
//! (transcript area), not a centered popup:
//!
//! * a rounded top border only (no left/right/bottom)
//! * one blank line of top margin
//! * sits after the scrollable message content in the fullscreen layout
//!
//! The runner keeps the non-cloneable response sender locally;
//! [`PermissionModalView`] is the cloneable render snapshot stored on
//! [`crate::tui::app::AppState`].

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    prelude::Widget,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use rebon_core::permission::{OutboundPermissionQuery, PermissionOptionKind};
use rebon_design_system::list_item::POINTER_GLYPH;
use rebon_permissions::chrome::permission_dialog_view;
use rebon_permissions::types::ThemeColor;
use rebon_tools_core::{
    WorkflowGraph, WorkflowGraphEdgeKind, WorkflowGraphNode, WorkflowGraphNodeStatus,
    WorkflowGraphNodeType,
};
use rebon_tui::{layout_prompt_input_line, parse_theme_color};
use rebon_width::WidthStr;

/// The profile prompt's contents are `rebon-plugin-profile`'s — only the drawing of
/// them is this file's. Re-exported so the call sites here and in
/// `runner::permission_flow` keep reading as one module.
pub use rebon_plugin_profile::{ProfileProposal, ProfileProposalAction};

/// Tool-specific permission dialog variant, dispatched per tool kind.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionKind {
    /// Generic "Allow <tool>?" dialog with Allow/Reject buttons.
    Generic,
    /// Workflow review — render full structured review before execution.
    WorkflowReview(WorkflowReviewState),
    /// "Enter plan mode?" — two choices: yes / no.
    EnterPlanMode,
    /// AskUserQuestion — render questions with selectable options.
    AskUserQuestion {
        questions: Vec<AskUserQuestionEntry>,
        /// Per-question answer/highlight/free-text state.
        answers: Vec<AskUserQuestionAnswer>,
        /// Which question is currently focused (for multi-question).
        active_question: usize,
        /// Multi-question dialogs end with a confirmation tab.
        confirmation_active: bool,
        /// Selected row on the confirmation tab: confirm or cancel.
        confirmation_selected: usize,
        /// Initial tool input JSON (needed to build updated_input).
        original_input: serde_json::Value,
    },
    /// ExitPlanMode — show the plan and offer response options.
    ExitPlanMode { plan: String },
    /// ProfileSwitch / ProfileSave — show what the model is proposing to
    /// change, field by field, before any of it happens.
    Profile(ProfileProposal),
}

/// Local workflow-review graph state used by the permission modal.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowReviewState {
    pub graph: WorkflowGraph,
    original_details: Vec<Option<String>>,
    pub focused_node: usize,
    pub actions_focused: bool,
    pub mode: WorkflowReviewMode,
    pub edit_buffer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowReviewMode {
    Browse,
    Edit,
}

impl WorkflowReviewState {
    pub fn new(mut graph: WorkflowGraph) -> Self {
        // Reorder nodes into the visual order the two-column table
        // renders (each phase immediately followed by the calls it
        // contains). Navigation walks node indices, so node order must
        // match row order or ↑/↓ focus would jump around the table.
        let order = visual_review_order(&graph);
        if order.iter().enumerate().any(|(pos, &idx)| pos != idx) {
            let mut nodes: Vec<Option<WorkflowGraphNode>> =
                graph.nodes.into_iter().map(Some).collect();
            graph.nodes = order
                .iter()
                .map(|&idx| {
                    nodes[idx]
                        .take()
                        .expect("visual order places each node once")
                })
                .collect();
        }
        let original_details = graph
            .nodes
            .iter()
            .map(|node| Self::normalized_detail(node.detail.as_deref()))
            .collect();
        Self {
            graph,
            original_details,
            focused_node: 0,
            actions_focused: false,
            mode: WorkflowReviewMode::Browse,
            edit_buffer: String::new(),
        }
    }

    pub fn has_nodes(&self) -> bool {
        !self.graph.nodes.is_empty()
    }

    pub fn has_action_focus(&self) -> bool {
        self.actions_focused || self.graph.nodes.is_empty()
    }

    fn original_detail(&self, index: usize) -> Option<String> {
        self.original_details.get(index).cloned().flatten()
    }

    fn normalized_detail(detail: Option<&str>) -> Option<String> {
        detail
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    fn sync_node_modified(&mut self, index: usize) {
        let original = self.original_detail(index);
        if let Some(node) = self.graph.focused_node_mut(index) {
            node.modified = node.detail != original;
        }
    }

    pub fn move_prev(&mut self) {
        if self.mode == WorkflowReviewMode::Browse {
            if self.has_action_focus() {
                return;
            }
            self.focused_node = self.focused_node.saturating_sub(1);
        }
    }

    pub fn move_next(&mut self) {
        if self.mode == WorkflowReviewMode::Browse {
            if self.has_action_focus() {
                return;
            }
            self.focused_node =
                (self.focused_node + 1).min(self.graph.nodes.len().saturating_sub(1));
        }
    }

    pub fn enter_edit(&mut self) {
        if self.mode != WorkflowReviewMode::Browse || self.actions_focused {
            return;
        }
        let Some(node) = self.graph.focused_node(self.focused_node) else {
            return;
        };
        if !node.editable {
            return;
        }
        self.edit_buffer = node.detail.clone().unwrap_or_default();
        self.mode = WorkflowReviewMode::Edit;
    }

    pub fn cancel_edit(&mut self) {
        if self.mode == WorkflowReviewMode::Edit {
            self.edit_buffer.clear();
            self.mode = WorkflowReviewMode::Browse;
        }
    }

    pub fn save_edit(&mut self) {
        if self.mode != WorkflowReviewMode::Edit {
            return;
        }
        let index = self.focused_node;
        let updated_detail = Self::normalized_detail(Some(&self.edit_buffer));
        if let Some(node) = self.graph.focused_node_mut(index) {
            node.detail = updated_detail;
        }
        self.sync_node_modified(index);
        self.edit_buffer.clear();
        self.mode = WorkflowReviewMode::Browse;
    }

    pub fn insert_char(&mut self, ch: char) {
        if self.mode == WorkflowReviewMode::Edit && !ch.is_control() {
            self.edit_buffer.push(ch);
        }
    }

    pub fn insert_newline(&mut self) {
        if self.mode == WorkflowReviewMode::Edit {
            self.edit_buffer.push('\n');
        }
    }

    pub fn backspace(&mut self) {
        if self.mode == WorkflowReviewMode::Edit {
            self.edit_buffer.pop();
        }
    }

    pub fn has_pending_edits(&self) -> bool {
        self.graph.has_modified_nodes()
    }
}

/// Compute the visual node order for the two-column review table:
/// overview first, then each phase immediately followed by the calls
/// it contains (each agent trailed by its schema nodes), then
/// phase-less calls in script order, then warnings/errors. Containment
/// comes from the graph edges; `Sequence` edges only express phase
/// ordering and are ignored here.
fn visual_review_order(graph: &WorkflowGraph) -> Vec<usize> {
    use std::collections::HashMap;

    fn place(idx: usize, order: &mut Vec<usize>, placed: &mut [bool]) {
        if !placed[idx] {
            placed[idx] = true;
            order.push(idx);
        }
    }

    let index_of: HashMap<&str, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(idx, node)| (node.id.as_str(), idx))
        .collect();
    let mut parent: HashMap<usize, usize> = HashMap::new();
    for edge in &graph.edges {
        if edge.kind == WorkflowGraphEdgeKind::Sequence {
            continue;
        }
        let (Some(&from), Some(&to)) = (
            index_of.get(edge.from.as_str()),
            index_of.get(edge.to.as_str()),
        ) else {
            continue;
        };
        parent.entry(to).or_insert(from);
    }

    // phase → contained calls; agent → its schema nodes (node order).
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut orphan_calls: Vec<usize> = Vec::new();
    for (idx, node) in graph.nodes.iter().enumerate() {
        match node.node_type {
            WorkflowGraphNodeType::Agent
            | WorkflowGraphNodeType::Call
            | WorkflowGraphNodeType::Log => {
                match parent
                    .get(&idx)
                    .copied()
                    .filter(|&p| graph.nodes[p].node_type == WorkflowGraphNodeType::Phase)
                {
                    Some(phase) => children.entry(phase).or_default().push(idx),
                    None => orphan_calls.push(idx),
                }
            }
            WorkflowGraphNodeType::Schema => match parent.get(&idx).copied() {
                Some(owner) => children.entry(owner).or_default().push(idx),
                None => orphan_calls.push(idx),
            },
            _ => {}
        }
    }

    let mut order = Vec::with_capacity(graph.nodes.len());
    let mut placed = vec![false; graph.nodes.len()];
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.node_type == WorkflowGraphNodeType::Overview {
            place(idx, &mut order, &mut placed);
        }
    }
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.node_type != WorkflowGraphNodeType::Phase {
            continue;
        }
        place(idx, &mut order, &mut placed);
        for &call in children.get(&idx).into_iter().flatten() {
            place(call, &mut order, &mut placed);
            for &schema in children.get(&call).into_iter().flatten() {
                place(schema, &mut order, &mut placed);
            }
        }
    }
    for &idx in &orphan_calls {
        place(idx, &mut order, &mut placed);
        for &schema in children.get(&idx).into_iter().flatten() {
            place(schema, &mut order, &mut placed);
        }
    }
    for idx in 0..graph.nodes.len() {
        place(idx, &mut order, &mut placed);
    }
    order
}

/// One visual row of the workflow-review body. `Cells` rows pair an
/// optional phase node (left column) with an optional call/agent node
/// (right column); `Full` rows span both columns (overview, warnings,
/// errors). Rendering and height measurement build the same rows so
/// the modal's measured height always matches what is painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowReviewRow {
    Full(usize),
    TableHeader,
    TableRule,
    Cells {
        left: Option<usize>,
        right: Option<usize>,
    },
}

fn workflow_review_rows(graph: &WorkflowGraph) -> Vec<WorkflowReviewRow> {
    let mut rows = Vec::new();
    let mut table = Vec::new();
    let mut trailing = Vec::new();
    let mut pending_phase: Option<usize> = None;
    for (idx, node) in graph.nodes.iter().enumerate() {
        match node.node_type {
            WorkflowGraphNodeType::Overview => rows.push(WorkflowReviewRow::Full(idx)),
            WorkflowGraphNodeType::Warning | WorkflowGraphNodeType::Error => {
                trailing.push(WorkflowReviewRow::Full(idx));
            }
            WorkflowGraphNodeType::Phase => {
                if let Some(left) = pending_phase.take() {
                    table.push(WorkflowReviewRow::Cells {
                        left: Some(left),
                        right: None,
                    });
                }
                pending_phase = Some(idx);
            }
            _ => table.push(WorkflowReviewRow::Cells {
                left: pending_phase.take(),
                right: Some(idx),
            }),
        }
    }
    if let Some(left) = pending_phase.take() {
        table.push(WorkflowReviewRow::Cells {
            left: Some(left),
            right: None,
        });
    }
    if !table.is_empty() {
        rows.push(WorkflowReviewRow::TableHeader);
        rows.push(WorkflowReviewRow::TableRule);
        rows.append(&mut table);
    }
    rows.append(&mut trailing);
    rows
}

/// One question in an AskUserQuestion dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskUserQuestionEntry {
    pub question: String,
    pub header: String,
    pub options: Vec<AskUserQuestionOption>,
    pub multi_select: bool,
}

/// One selectable option in an AskUserQuestion dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskUserQuestionOption {
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
}

/// Per-question local answer state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskUserQuestionAnswer {
    pub highlighted_row: usize,
    pub selected_options: Vec<usize>,
    pub other_text: String,
    pub other_cursor_offset: usize,
}

impl AskUserQuestionAnswer {
    pub fn new() -> Self {
        Self {
            highlighted_row: 0,
            selected_options: Vec::new(),
            other_text: String::new(),
            other_cursor_offset: 0,
        }
    }

    pub fn has_answer(&self, question: &AskUserQuestionEntry) -> bool {
        self.selected_options
            .iter()
            .any(|idx| *idx < question.options.len())
            || !self.other_text.trim().is_empty()
    }
}

/// Cloneable render state for the permission modal.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionModalView {
    pub query_id: u64,
    pub tool_call_id: String,
    pub title: String,
    pub summary: String,
    pub options: Vec<PermissionOptionView>,
    pub selected: usize,
    pub extra_text: String,
    pub extra_text_focused: bool,
    /// Tool-specific dialog variant.
    pub kind: PermissionKind,
}

/// Renderable option row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOptionView {
    pub option_id: String,
    pub label: String,
    pub kind: PermissionOptionKind,
}

/// Runner-local pending permission state.
#[derive(Debug)]
pub struct PendingPermission {
    pub view: PermissionModalView,
    pub outbound: OutboundPermissionQuery,
}

/// Modal-local key action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionModalAction {
    None,
    MovePrev,
    MoveNext,
    MoveTabPrev,
    MoveTabNext,
    Confirm,
    Toggle,
    Cancel,
    FocusExtraText,
    SaveEdit,
    InsertNewline,
    TypeChar(char),
    Backspace,
    Delete,
    CursorHome,
    CursorEnd,
}

impl PendingPermission {
    pub fn from_query(
        outbound: OutboundPermissionQuery,
        title: String,
        summary: String,
        kind: PermissionKind,
    ) -> Self {
        let options = outbound
            .options
            .iter()
            .map(|option| PermissionOptionView {
                option_id: option.option_id.clone(),
                label: option.label.clone(),
                kind: option.kind,
            })
            .collect();
        let view = PermissionModalView {
            query_id: outbound.id,
            tool_call_id: outbound.tool_call_id.clone(),
            title,
            summary,
            options,
            selected: 0,
            extra_text: String::new(),
            extra_text_focused: false,
            kind,
        };
        Self { view, outbound }
    }
}

/// Parse `AskUserQuestion` tool input into renderable question entries.
pub fn parse_ask_user_questions(input: &serde_json::Value) -> Vec<AskUserQuestionEntry> {
    let questions = match input.get("questions").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return vec![],
    };
    questions
        .iter()
        .filter_map(|q| {
            let question = q.get("question")?.as_str()?.to_string();
            let header = q
                .get("header")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let multi_select = q
                .get("multiSelect")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let options = q
                .get("options")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|opt| {
                            let label = opt.get("label")?.as_str()?.to_string();
                            let description = opt
                                .get("description")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let preview = opt
                                .get("preview")
                                .and_then(|v| v.as_str())
                                .map(str::to_string);
                            Some(AskUserQuestionOption {
                                label,
                                description,
                                preview,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(AskUserQuestionEntry {
                question,
                header,
                options,
                multi_select,
            })
        })
        .collect()
}

/// Build the `updated_input` JSON for an AskUserQuestion response.
/// Merges collected answers into the original input.
pub fn build_ask_user_updated_input(
    original_input: &serde_json::Value,
    questions: &[AskUserQuestionEntry],
    answers: &[AskUserQuestionAnswer],
) -> serde_json::Value {
    let mut input = original_input.clone();
    let mut answer_map = serde_json::Map::new();
    let mut annotations_map = serde_json::Map::new();

    for (q, answer) in questions.iter().zip(answers.iter()) {
        let other_text = answer.other_text.trim();
        let labels: Vec<String> = answer
            .selected_options
            .iter()
            .filter_map(|idx| q.options.get(*idx).map(|opt| opt.label.clone()))
            .collect();

        let answer_text = if labels.is_empty() {
            if other_text.is_empty() {
                continue;
            }
            other_text.to_string()
        } else if q.multi_select {
            labels.join(", ")
        } else {
            labels[0].clone()
        };
        answer_map.insert(q.question.clone(), serde_json::Value::String(answer_text));

        let mut annotation = serde_json::Map::new();
        if !q.multi_select {
            if let Some(idx) = answer.selected_options.first() {
                if let Some(preview) = q.options.get(*idx).and_then(|opt| opt.preview.as_ref()) {
                    annotation.insert("preview".into(), serde_json::Value::String(preview.clone()));
                }
            }
        }
        if !labels.is_empty() && !other_text.is_empty() {
            annotation.insert(
                "notes".into(),
                serde_json::Value::String(other_text.to_string()),
            );
        }
        if !annotation.is_empty() {
            annotations_map.insert(q.question.clone(), serde_json::Value::Object(annotation));
        }
    }

    if let Some(obj) = input.as_object_mut() {
        obj.insert("answers".into(), serde_json::Value::Object(answer_map));
        if !annotations_map.is_empty() {
            obj.insert(
                "annotations".into(),
                serde_json::Value::Object(annotations_map),
            );
        }
    }
    input
}

impl PermissionModalView {
    pub fn move_prev(&mut self) {
        if let PermissionKind::WorkflowReview(review) = &mut self.kind {
            if review.has_action_focus() {
                self.selected = self.selected.saturating_sub(1);
            } else {
                review.move_prev();
            }
            return;
        }
        if self.options.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_next(&mut self) {
        if let PermissionKind::WorkflowReview(review) = &mut self.kind {
            if review.has_action_focus() {
                if self.options.is_empty() {
                    self.selected = 0;
                } else {
                    self.selected = (self.selected + 1).min(self.options.len() - 1);
                }
            } else {
                review.move_next();
            }
            return;
        }
        if self.options.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected + 1).min(self.options.len() - 1);
    }

    pub fn selected_option(&self) -> Option<&PermissionOptionView> {
        self.options.get(self.selected)
    }
}

/// Build the permission dialog title from a raw tool name string.
///
/// Wire rebon-tools-core: uses [`ToolId`] to canonicalize the tool
/// name before rendering the title. `ToolId::as_str()` is the
/// display-safe form used for tool-name matching.
pub fn permission_title(tool_name: &str) -> String {
    let id = rebon_tools_core::ToolId::new(tool_name);
    format!("Allow {}?", id.as_str())
}

pub fn translate_modal_key(key: &ratatui::crossterm::event::KeyEvent) -> PermissionModalAction {
    use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return PermissionModalAction::None;
    }

    match key.code {
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
            PermissionModalAction::SaveEdit
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            PermissionModalAction::InsertNewline
        }
        KeyCode::Up => PermissionModalAction::MovePrev,
        KeyCode::Down => PermissionModalAction::MoveNext,
        KeyCode::Left => PermissionModalAction::MoveTabPrev,
        KeyCode::Right => PermissionModalAction::MoveTabNext,
        KeyCode::Tab => PermissionModalAction::FocusExtraText,
        KeyCode::BackTab => PermissionModalAction::MovePrev,
        KeyCode::Enter => PermissionModalAction::Confirm,
        KeyCode::Char(' ') => PermissionModalAction::Toggle,
        KeyCode::Backspace => PermissionModalAction::Backspace,
        KeyCode::Delete => PermissionModalAction::Delete,
        KeyCode::Home => PermissionModalAction::CursorHome,
        KeyCode::End => PermissionModalAction::CursorEnd,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            PermissionModalAction::Cancel
        }
        KeyCode::Char(ch) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            PermissionModalAction::TypeChar(ch)
        }
        KeyCode::Esc => PermissionModalAction::Cancel,
        _ => PermissionModalAction::None,
    }
}

#[derive(Debug)]
struct AskUserOtherLayout {
    rows: Vec<String>,
    prefix_width: u16,
    cursor: Option<(u16, u16)>,
    placeholder: bool,
}

fn ask_user_other_layout(
    answer: &AskUserQuestionAnswer,
    display_row: usize,
    width: u16,
    focused: bool,
) -> AskUserOtherLayout {
    let marker = if focused { POINTER_GLYPH } else { " " };
    let prefix = format!("{marker} {display_row}. ");
    let prefix_width = WidthStr::width(prefix.as_str()).min(u16::MAX as usize) as u16;
    let input_width = width.saturating_sub(prefix_width);
    let placeholder = answer.other_text.is_empty();
    let displayed = if placeholder {
        "Type something."
    } else {
        answer.other_text.as_str()
    };
    let cursor_offset = (focused && !placeholder).then_some(answer.other_cursor_offset);
    let mut layout = layout_prompt_input_line(displayed, cursor_offset, input_width);
    if layout.rows.is_empty() {
        layout.rows.push(String::new());
    }
    let cursor = if focused && input_width > 0 {
        if placeholder {
            Some((0, 0))
        } else {
            layout.cursor
        }
    } else {
        None
    };

    AskUserOtherLayout {
        rows: layout.rows,
        prefix_width,
        cursor,
        placeholder,
    }
}

/// Measure the height of the inline permission dialog.
///
/// Layout: one blank margin line, chrome lines, then body lines. AskUserQuestion
/// uses a top rule plus a questionnaire heading and ends with one blank line;
/// other dialogs use one title rule.
pub fn measure_permission_inline(view: &PermissionModalView, width: u16) -> u16 {
    let chrome = if matches!(&view.kind, PermissionKind::AskUserQuestion { .. }) {
        2
    } else {
        1
    };
    chrome + 1 + permission_body_line_count(view, width)
}

/// Result of rendering an inline permission dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionInlineRenderResult {
    pub lines_painted: u16,
    pub cursor: Option<(u16, u16)>,
}

impl PermissionInlineRenderResult {
    const EMPTY: Self = Self {
        lines_painted: 0,
        cursor: None,
    };
}

/// Render the permission dialog inline and return the number of painted rows.
#[cfg(test)]
pub fn render_permission_inline(
    view: &PermissionModalView,
    area: Rect,
    buf: &mut Buffer,
    skip_lines: usize,
) -> u16 {
    render_permission_inline_with_cursor(view, area, buf, skip_lines).lines_painted
}

/// Render the permission dialog inline:
///
/// * top border only (no left/right/bottom)
/// * one blank line above
/// * one cell of horizontal padding
///
/// This is an inline overlay, not a centered popup with a full border
/// and screen clear.
///
/// `skip_lines` is the number of leading lines to clip (for partial
/// scroll visibility).
pub fn render_permission_inline_with_cursor(
    view: &PermissionModalView,
    area: Rect,
    buf: &mut Buffer,
    skip_lines: usize,
) -> PermissionInlineRenderResult {
    if area.width == 0 || area.height == 0 {
        return PermissionInlineRenderResult::EMPTY;
    }

    // Reset every cell in the modal's owned rect before painting. The
    // body shrinks and grows as the user navigates between questions
    // (AskUserQuestion → Generic, multi-select toggle, plan preview
    // expand/collapse, option list change), and we paint a `sub`
    // rect sized to `painted` rows below — any rows the previous
    // frame painted that the current frame no longer covers would
    // otherwise keep their glyphs and bleed through. Clearing the full
    // `area` here pins the invariant locally so callers cannot forget.
    rebon_tui::render::buffer_util::clear_buffer_area(buf, area);

    let chrome = permission_dialog_view(
        &view.title,
        None,
        None,
        Some(ThemeColor::Permission),
        Some(1),
        None,
        false,
    );

    let ds = rebon_design_system::theme::get_active_theme();
    let styles = PermissionInlineStyles {
        border: Style::default().fg(map_theme_color(chrome.border_color)),
        title: Style::default()
            .fg(map_theme_color(chrome.title.color))
            .add_modifier(Modifier::BOLD),
        inactive: Style::default().fg(parse_theme_color(ds.inactive)),
        focused: Style::default()
            .fg(parse_theme_color(ds.suggestion))
            .add_modifier(Modifier::BOLD),
    };

    // Build all virtual lines: margin + border + body.
    let mut all_lines: Vec<Line<'_>> = Vec::new();
    let mut virtual_cursor: Option<(u16, usize)> = None;

    // margin_top = 1 blank line
    all_lines.push(Line::from(""));

    push_permission_heading_lines(view, area.width, styles, &mut all_lines);

    // Body: tool-specific content.
    let pad = " ";
    match &view.kind {
        PermissionKind::AskUserQuestion {
            questions,
            answers,
            active_question,
            confirmation_active,
            confirmation_selected,
            ..
        } => {
            if *confirmation_active && questions.len() > 1 {
                push_ask_user_confirmation_lines(*confirmation_selected, styles, &mut all_lines);
            } else if let Some(q) = questions.get(*active_question) {
                let answer = answers
                    .get(*active_question)
                    .cloned()
                    .unwrap_or_else(AskUserQuestionAnswer::new);
                virtual_cursor =
                    push_ask_user_options_lines(q, &answer, area, styles, pad, &mut all_lines);
            }
            all_lines.push(Line::from(""));
        }
        PermissionKind::ExitPlanMode { plan } => {
            // Render ExitPlanMode: show the full plan text, then options.
            for line in plan.lines() {
                all_lines.push(Line::from(vec![
                    Span::raw(pad),
                    Span::raw(line.to_string()),
                ]));
            }
            all_lines.push(Line::from(""));
            // When the plan + chrome is taller than the visible area the view
            // scrolls (PageUp/PageDown · j/k · Ctrl+Home/End — routed by
            // permission_suffix_scroll_action). Advertise it inline so the
            // affordance is discoverable. The hint shares the proceed line, so
            // it never changes the line count permission_body_line_count
            // measures.
            let overflows = measure_permission_inline(view, area.width) > area.height;
            let proceed = if overflows {
                "Would you like to proceed?  (PgUp/PgDn · j/k scroll plan)"
            } else {
                "Would you like to proceed?"
            };
            all_lines.push(Line::from(vec![
                Span::raw(pad),
                Span::styled(proceed, styles.inactive),
            ]));
            all_lines.push(Line::from(""));
            render_standard_options(view, &mut all_lines);
        }
        PermissionKind::Profile(proposal) => {
            render_profile_proposal_lines(view, proposal, pad, &mut all_lines);
        }
        PermissionKind::WorkflowReview(review) => {
            render_workflow_review_lines(view, review, &mut all_lines);
        }
        PermissionKind::EnterPlanMode | PermissionKind::Generic => {
            // Generic / EnterPlanMode: summary + options.
            for line in permission_summary_rows(&view.summary, area.width) {
                all_lines.push(Line::from(vec![Span::raw(pad), Span::raw(line)]));
            }
            all_lines.push(Line::from(""));
            render_standard_options(view, &mut all_lines);
        }
    }

    // Apply skip_lines clipping and render visible portion.
    let visible_lines: Vec<Line<'_>> = all_lines
        .into_iter()
        .skip(skip_lines)
        .take(area.height as usize)
        .collect();

    let painted = visible_lines.len() as u16;
    if painted > 0 {
        let paragraph = Paragraph::new(visible_lines);
        let sub = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: painted.min(area.height),
        };
        paragraph.render(sub, buf);
    }

    let cursor = virtual_cursor.and_then(|(column, row)| {
        let visible_row = row.checked_sub(skip_lines)?;
        (visible_row < usize::from(painted) && column < area.width).then_some((
            area.x.saturating_add(column),
            area.y.saturating_add(visible_row as u16),
        ))
    });

    PermissionInlineRenderResult {
        lines_painted: painted,
        cursor,
    }
}

/// The four styles the inline permission surface draws with, resolved
/// once from the chrome and the active theme.
#[derive(Debug, Clone, Copy)]
struct PermissionInlineStyles {
    border: Style,
    title: Style,
    inactive: Style,
    /// The highlighted row: whichever option the cursor is on.
    focused: Style,
}

/// The rule and heading above the body.
///
/// AskUserQuestion uses a standalone top rule and compact questionnaire
/// heading. Other permission dialogs keep the horizontal title rule.
fn push_permission_heading_lines<'a>(
    view: &'a PermissionModalView,
    width: u16,
    styles: PermissionInlineStyles,
    all_lines: &mut Vec<Line<'a>>,
) {
    if let PermissionKind::AskUserQuestion {
        questions,
        active_question,
        confirmation_active,
        ..
    } = &view.kind
    {
        all_lines.push(Line::from(Span::styled(
            "─".repeat(width as usize),
            styles.border,
        )));
        let mut heading_spans = vec![Span::raw(" ")];
        if questions.len() <= 1 {
            let heading = questions
                .first()
                .map(|question| question.header.trim())
                .filter(|heading| !heading.is_empty())
                .unwrap_or(view.title.as_str());
            heading_spans.push(Span::styled("☐ ", styles.border));
            heading_spans.push(Span::styled(heading.to_string(), styles.title));
        } else {
            let active_tab = if *confirmation_active {
                questions.len()
            } else {
                (*active_question).min(questions.len().saturating_sub(1))
            };
            let mut tab_labels = questions
                .iter()
                .enumerate()
                .map(|(idx, question)| {
                    if question.header.trim().is_empty() {
                        format!("Question {}", idx + 1)
                    } else {
                        question.header.trim().to_string()
                    }
                })
                .collect::<Vec<_>>();
            tab_labels.push("Confirm".to_string());

            let available_width = (width as usize).saturating_sub(1);
            let tab_width = |idx: usize| {
                WidthStr::width(tab_labels[idx].as_str()) + if idx == active_tab { 2 } else { 0 }
            };
            let mut first_visible = 0;
            let mut width_to_active = (0..=active_tab)
                .map(|idx| tab_width(idx) + usize::from(idx > 0) * 2)
                .sum::<usize>();
            while first_visible < active_tab && width_to_active > available_width {
                width_to_active = width_to_active.saturating_sub(tab_width(first_visible) + 2);
                first_visible += 1;
            }

            let mut used_width = 0;
            for (idx, label) in tab_labels.iter().enumerate().skip(first_visible) {
                let separator_width = usize::from(used_width > 0) * 2;
                let marker_width = if idx == active_tab { 2 } else { 0 };
                let remaining_width = available_width
                    .saturating_sub(used_width)
                    .saturating_sub(separator_width)
                    .saturating_sub(marker_width);
                if remaining_width == 0 {
                    break;
                }
                let visible_label =
                    crate::tui::dialog_support::truncate_to_width(label, remaining_width);
                if used_width > 0 {
                    heading_spans.push(Span::raw("  "));
                }
                if idx == active_tab {
                    heading_spans.push(Span::styled("☐ ", styles.title));
                }
                let style = if idx == active_tab {
                    styles.title
                } else {
                    styles.inactive
                };
                heading_spans.push(Span::styled(visible_label.clone(), style));
                used_width +=
                    separator_width + marker_width + WidthStr::width(visible_label.as_str());
                if visible_label.as_str() != label.as_str() {
                    break;
                }
            }
        }
        all_lines.push(Line::from(heading_spans));
    } else {
        let title_text = format!(" {} ", view.title);
        let title_len = title_text.len();
        let rule_remaining = (width as usize).saturating_sub(title_len + 1);
        let mut border_spans = vec![
            Span::styled("─", styles.border),
            Span::styled(title_text, styles.title),
        ];
        if rule_remaining > 0 {
            border_spans.push(Span::styled("─".repeat(rule_remaining), styles.border));
        }
        all_lines.push(Line::from(border_spans));
    }
}

/// The confirmation tab a multi-question dialog ends on.
fn push_ask_user_confirmation_lines(
    confirmation_selected: usize,
    styles: PermissionInlineStyles,
    all_lines: &mut Vec<Line<'_>>,
) {
    all_lines.push(Line::from(""));
    all_lines.push(Line::from("Submit your answers?"));
    all_lines.push(Line::from(""));
    for (idx, label) in ["Confirm", "Cancel"].iter().enumerate() {
        let highlighted = confirmation_selected == idx;
        let style = if highlighted {
            styles.focused
        } else {
            Style::default()
        };
        all_lines.push(Line::from(vec![
            Span::styled(if highlighted { POINTER_GLYPH } else { " " }, style),
            Span::raw(" "),
            Span::styled(format!("{}. {label}", idx + 1), style),
        ]));
    }
}

/// One question's rows: the question, its options, the free-text
/// "other" field, an optional preview, and the "chat about this" row.
/// Returns the virtual cursor position inside the free-text field.
fn push_ask_user_options_lines<'a>(
    q: &AskUserQuestionEntry,
    answer: &AskUserQuestionAnswer,
    area: Rect,
    styles: PermissionInlineStyles,
    pad: &'a str,
    all_lines: &mut Vec<Line<'a>>,
) -> Option<(u16, usize)> {
    let mut virtual_cursor = None;
    all_lines.push(Line::from(""));
    all_lines.push(Line::from(q.question.clone()));
    all_lines.push(Line::from(""));

    let option_count = q.options.len();
    let focused_style = styles.focused;

    for (idx, opt) in q.options.iter().enumerate() {
        let highlighted = idx == answer.highlighted_row;
        let checked = answer.selected_options.contains(&idx);
        let row_style = if highlighted {
            focused_style
        } else {
            Style::default()
        };
        let marker = if checked { "[✓]" } else { "[ ]" };
        let label = format!("{}. {marker} {}", idx + 1, opt.label);
        all_lines.push(Line::from(vec![
            Span::styled(if highlighted { POINTER_GLYPH } else { " " }, row_style),
            Span::raw(" "),
            Span::styled(label, row_style),
        ]));
        if !opt.description.is_empty() {
            let indent = " ".repeat(2 + (idx + 1).to_string().len() + 2);
            all_lines.push(Line::from(vec![
                Span::raw(indent),
                Span::styled(opt.description.clone(), styles.inactive),
            ]));
        }
    }

    let other_row = option_count;
    let other_highlighted = answer.highlighted_row == other_row;
    let other_style = if other_highlighted {
        focused_style
    } else {
        Style::default()
    };
    let AskUserOtherLayout {
        rows,
        prefix_width,
        cursor,
        placeholder,
    } = ask_user_other_layout(answer, other_row + 1, area.width, other_highlighted);
    let other_text_style = if placeholder {
        styles.inactive
    } else {
        Style::default()
    };
    let other_start_row = all_lines.len();
    for (row_index, text) in rows.into_iter().enumerate() {
        if row_index == 0 {
            all_lines.push(Line::from(vec![
                Span::styled(
                    if other_highlighted {
                        POINTER_GLYPH
                    } else {
                        " "
                    },
                    other_style,
                ),
                Span::raw(" "),
                Span::styled(format!("{}. ", other_row + 1), other_style),
                Span::styled(text, other_text_style),
            ]));
        } else {
            all_lines.push(Line::from(vec![
                Span::raw(" ".repeat(usize::from(prefix_width))),
                Span::styled(text, other_text_style),
            ]));
        }
    }
    if let Some((column, row)) = cursor {
        virtual_cursor = Some((
            prefix_width.saturating_add(column),
            other_start_row.saturating_add(usize::from(row)),
        ));
    }

    if !q.multi_select {
        if let Some(idx) = answer.selected_options.first() {
            if let Some(preview) = q.options.get(*idx).and_then(|opt| opt.preview.as_ref()) {
                all_lines.push(Line::from(""));
                all_lines.push(Line::from(vec![
                    Span::raw(pad),
                    Span::styled("Preview:", styles.inactive),
                ]));
                for line in preview.lines().take(8) {
                    all_lines.push(Line::from(vec![
                        Span::raw("   "),
                        Span::raw(line.to_string()),
                    ]));
                }
            }
        }
    }

    all_lines.push(Line::from(Span::styled(
        "─".repeat(area.width as usize),
        styles.border,
    )));
    let chat_row = other_row + 1;
    let chat_highlighted = answer.highlighted_row == chat_row;
    let chat_style = if chat_highlighted {
        focused_style
    } else {
        Style::default()
    };
    all_lines.push(Line::from(vec![
        Span::styled(
            if chat_highlighted { POINTER_GLYPH } else { " " },
            chat_style,
        ),
        Span::raw(" "),
        Span::styled(format!("{}. Chat about this", chat_row + 1), chat_style),
    ]));
    virtual_cursor
}

/// Render a profile prompt: the model's reason, then one row per field with
/// what it is now and what it would become.
///
/// The permission-mode row is drawn in the warning colour rather than being
/// one more `from → to` line. It is the only field that changes what else can
/// happen without asking again, and a user skimming five rows of provider and
/// model names should not have to notice it on their own.
fn render_profile_proposal_lines<'a>(
    view: &PermissionModalView,
    proposal: &ProfileProposal,
    pad: &'a str,
    all_lines: &mut Vec<Line<'a>>,
) {
    let ds = rebon_design_system::theme::get_active_theme();
    let muted = Style::default().fg(parse_theme_color(ds.inactive));
    let warn = Style::default()
        .fg(parse_theme_color(ds.warning))
        .add_modifier(Modifier::BOLD);

    if let Some(reason) = proposal.reason.as_deref().filter(|r| !r.trim().is_empty()) {
        for line in reason.lines() {
            all_lines.push(Line::from(vec![
                Span::raw(pad),
                Span::raw(line.to_string()),
            ]));
        }
        all_lines.push(Line::from(""));
    }

    if proposal.rows.is_empty() {
        // A switch whose every field already matches the session. Saying so is
        // the whole content of the prompt — approving it would be a no-op, and
        // the user deserves to know that before deciding.
        all_lines.push(Line::from(vec![
            Span::raw(pad),
            Span::styled(
                format!(
                    "Nothing would change — this session already matches \"{}\".",
                    proposal.label
                ),
                muted,
            ),
        ]));
        all_lines.push(Line::from(""));
    } else {
        let width = proposal
            .rows
            .iter()
            .map(|row| row.field.chars().count())
            .max()
            .unwrap_or(0);
        for row in &proposal.rows {
            let label = format!("{:width$}", row.field, width = width);
            let value = match row.from.as_deref() {
                Some(from) => format!("{from}  →  {}", row.to),
                None => row.to.clone(),
            };
            let style = if row.highlight {
                warn
            } else {
                Style::default()
            };
            all_lines.push(Line::from(vec![
                Span::raw(pad),
                Span::styled(label, muted),
                Span::raw("  "),
                Span::styled(value, style),
            ]));
        }
        all_lines.push(Line::from(""));
    }

    for note in &proposal.notes {
        all_lines.push(Line::from(vec![
            Span::raw(pad),
            Span::styled(note.clone(), muted),
        ]));
    }
    if !proposal.notes.is_empty() {
        all_lines.push(Line::from(""));
    }

    all_lines.push(Line::from(vec![
        Span::raw(pad),
        Span::styled(
            format!("Let Rebon {} \"{}\"?", proposal.verb(), proposal.label),
            muted,
        ),
    ]));
    all_lines.push(Line::from(""));
    render_standard_options(view, all_lines);
}

fn render_workflow_review_lines<'a>(
    view: &PermissionModalView,
    review: &WorkflowReviewState,
    all_lines: &mut Vec<Line<'a>>,
) {
    let ds = rebon_design_system::theme::get_active_theme();
    let pending_edits = review.has_pending_edits();
    all_lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            if review.mode == WorkflowReviewMode::Edit {
                "Workflow graph — editing selected node detail"
            } else {
                "Workflow graph"
            },
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));
    if review.graph.nodes.is_empty() {
        let fallback = review
            .graph
            .fallback_text
            .as_deref()
            .unwrap_or(&view.summary);
        for line in fallback.lines() {
            let style = if is_workflow_review_heading(line) {
                Style::default().add_modifier(Modifier::BOLD)
            } else if line.starts_with("- warning:") || line.starts_with("- error:") {
                Style::default().fg(parse_theme_color(ds.warning))
            } else if line.contains("implementation detail for audit") {
                Style::default().fg(parse_theme_color(ds.inactive))
            } else {
                Style::default()
            };
            all_lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(line.to_string(), style),
            ]));
        }
        all_lines.push(Line::from(""));
        all_lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "Run this workflow?",
                Style::default().fg(parse_theme_color(ds.inactive)),
            ),
        ]));
        all_lines.push(Line::from(""));
        render_standard_options(view, all_lines);
        return;
    }
    let rows = workflow_review_rows(&review.graph);
    let (left_width, rule_width) = workflow_review_column_widths(&review.graph, &rows);

    // The focused-node detail box spans the same columns as the table
    // (`left │ right`), so the expansion reads as part of the table.
    let detail_box_width = left_width + 3 + rule_width;
    let dim = Style::default().fg(parse_theme_color(ds.inactive));
    let focused_style = Style::default()
        .fg(parse_theme_color(ds.inverseText))
        .bg(parse_theme_color(ds.text))
        .add_modifier(Modifier::BOLD);
    let focused_idx = (!review.actions_focused).then_some(review.focused_node);
    for row in &rows {
        match row {
            WorkflowReviewRow::Full(idx) => {
                let node = &review.graph.nodes[*idx];
                let focused = focused_idx == Some(*idx);
                let prefix = if focused { " › " } else { "   " };
                let modified = if node.modified { " *" } else { "" };
                let readonly = if node.editable { "" } else { " (read-only)" };
                let style = if focused {
                    focused_style
                } else {
                    workflow_node_style(node.node_type, node.status)
                };
                all_lines.push(Line::from(Span::styled(
                    format!(
                        "{prefix}{} {}{}{}",
                        rebon_tools_core::workflow_graph::status_label(node.status),
                        workflow_node_title(node),
                        modified,
                        readonly
                    ),
                    style,
                )));
                if focused {
                    push_focused_workflow_detail(review, node, detail_box_width, all_lines);
                }
            }
            WorkflowReviewRow::TableHeader => {
                all_lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        format!("{} │ Agents", workflow_review_pad_cell("Phase", left_width)),
                        dim,
                    ),
                ]));
            }
            WorkflowReviewRow::TableRule => {
                all_lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        format!("{}─┼─{}", "─".repeat(left_width), "─".repeat(rule_width)),
                        dim,
                    ),
                ]));
            }
            WorkflowReviewRow::Cells { left, right } => {
                let (left, right) = (*left, *right);
                let left_focused = left.is_some() && focused_idx == left;
                let right_focused = right.is_some() && focused_idx == right;
                let cell_style = |idx: usize, focused: bool| {
                    if focused {
                        focused_style
                    } else {
                        let node = &review.graph.nodes[idx];
                        workflow_node_style(node.node_type, node.status)
                    }
                };
                let mut spans = vec![Span::raw(" ")];
                let left_text = left
                    .map(|idx| workflow_cell_text(&review.graph.nodes[idx], left_focused))
                    .unwrap_or_default();
                spans.push(Span::styled(
                    workflow_review_pad_cell(&left_text, left_width),
                    left.map(|idx| cell_style(idx, left_focused)).unwrap_or(dim),
                ));
                spans.push(Span::styled(" │ ".to_string(), dim));
                match right {
                    Some(idx) => spans.push(Span::styled(
                        workflow_cell_text(&review.graph.nodes[idx], right_focused),
                        cell_style(idx, right_focused),
                    )),
                    None => spans.push(Span::styled("—".to_string(), dim)),
                }
                all_lines.push(Line::from(spans));
                if let Some(idx) = left.filter(|_| left_focused) {
                    push_focused_workflow_detail(
                        review,
                        &review.graph.nodes[idx],
                        detail_box_width,
                        all_lines,
                    );
                } else if let Some(idx) = right.filter(|_| right_focused) {
                    push_focused_workflow_detail(
                        review,
                        &review.graph.nodes[idx],
                        detail_box_width,
                        all_lines,
                    );
                }
            }
        }
    }

    all_lines.push(Line::from(""));
    if review.mode == WorkflowReviewMode::Edit {
        all_lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "Edit detail: Enter saves · Shift+Enter adds line · Esc cancels",
                Style::default().fg(parse_theme_color(ds.inactive)),
            ),
        ]));
        return;
    }

    if pending_edits {
        all_lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "Pending edits: choose Reject once to request revision feedback.",
                Style::default().fg(parse_theme_color(ds.warning)),
            ),
        ]));
    } else {
        all_lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "Run this workflow?",
                Style::default().fg(parse_theme_color(ds.inactive)),
            ),
        ]));
    }
    all_lines.push(Line::from(""));
    render_workflow_action_options(view, review, all_lines);
    all_lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "Workflow graph: ↑/↓ focus node · Enter edit/send · Tab action/note · Esc cancel",
            Style::default().fg(parse_theme_color(ds.inactive)),
        ),
    ]));
}

fn render_workflow_action_options<'a>(
    view: &PermissionModalView,
    review: &WorkflowReviewState,
    all_lines: &mut Vec<Line<'a>>,
) {
    let ds = rebon_design_system::theme::get_active_theme();
    if view.options.is_empty() {
        all_lines.push(Line::from(vec![
            Span::raw(" "),
            Span::raw("(no permission options)"),
        ]));
    } else {
        for (idx, option) in view.options.iter().enumerate() {
            let is_selected =
                (review.actions_focused || view.extra_text_focused) && idx == view.selected;
            all_lines.push(option_line(view, option, is_selected));
        }
    }
    all_lines.push(Line::from(""));
    all_lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "Tab: graph/actions/note · Enter: edit node or submit action · Esc: cancel",
            Style::default().fg(parse_theme_color(ds.inactive)),
        ),
    ]));
}

/// Hard cap on the left (phase) column of the review table so a long
/// phase title cannot push every call cell past the wrap margin.
const WORKFLOW_REVIEW_PHASE_COLUMN_MAX_WIDTH: usize = 36;
/// The rule under `Agents` tracks the widest call cell but stops here
/// so the separator line itself stays within narrow modals.
const WORKFLOW_REVIEW_AGENTS_RULE_MAX_WIDTH: usize = 48;

/// One table cell: a 2-column focus-marker slot, then
/// `[status] title` plus the modified/read-only markers. The marker
/// slot keeps cell width identical whether or not the cell is focused.
fn workflow_cell_text(node: &WorkflowGraphNode, focused: bool) -> String {
    format!(
        "{}{} {}{}{}",
        if focused { "› " } else { "  " },
        rebon_tools_core::workflow_graph::status_label(node.status),
        workflow_node_title(node),
        if node.modified { " *" } else { "" },
        if node.editable { "" } else { " (read-only)" }
    )
}

/// Pad `text` with spaces to exactly `width` display columns,
/// truncating when it does not fit.
fn workflow_review_pad_cell(text: &str, width: usize) -> String {
    let mut cell = crate::tui::dialog_support::truncate_to_width(text, width);
    cell.push_str(&" ".repeat(width.saturating_sub(WidthStr::width(cell.as_str()))));
    cell
}

/// Column widths of the review table (phase column, agents rule). Shared by
/// the renderer and the height measurement so the focused-detail box wraps
/// its rows at the same width on both sides.
fn workflow_review_column_widths(
    graph: &WorkflowGraph,
    rows: &[WorkflowReviewRow],
) -> (usize, usize) {
    let left_width = rows
        .iter()
        .filter_map(|row| match row {
            WorkflowReviewRow::Cells {
                left: Some(idx), ..
            } => Some(WidthStr::width(
                workflow_cell_text(&graph.nodes[*idx], false).as_str(),
            )),
            _ => None,
        })
        .chain(std::iter::once("Phase".len()))
        .max()
        .unwrap_or(0)
        .min(WORKFLOW_REVIEW_PHASE_COLUMN_MAX_WIDTH);
    let rule_width = rows
        .iter()
        .filter_map(|row| match row {
            WorkflowReviewRow::Cells {
                right: Some(idx), ..
            } => Some(WidthStr::width(
                workflow_cell_text(&graph.nodes[*idx], false).as_str(),
            )),
            _ => None,
        })
        .chain(std::iter::once("Agents".len()))
        .max()
        .unwrap_or(0)
        .min(WORKFLOW_REVIEW_AGENTS_RULE_MAX_WIDTH);
    (left_width, rule_width)
}

/// Hard cap on the free-text detail body inside the focused-node box; the
/// structured agent facts render in full below it.
const WORKFLOW_DETAIL_BODY_MAX_LINES: usize = 10;

/// Greedy wrap by display columns, so a row longer than the box still shows
/// every character across continuation lines instead of truncating. Breaks
/// mid-token by design: the wrapped rows concatenate back to the original.
fn wrap_display_columns(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in text.chars() {
        let ch_width = rebon_width::char_width(ch).unwrap_or(0);
        if current_width + ch_width > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn permission_summary_rows(summary: &str, width: u16) -> Vec<String> {
    let content_width = usize::from(width.saturating_sub(1)).max(1);
    summary
        .split('\n')
        .flat_map(|line| wrap_display_columns(line, content_width))
        .collect()
}

/// Content rows of the focused node's Browse-mode detail box, in render
/// order: the node detail (with `${args.…}` placeholders expanded against
/// the launch args), then the structured `agent()` facts — dynamic-prompt
/// marker, model, provider, model profile, isolation, agent type, schema.
/// The `bool` marks rows rendered dim (annotations vs. detail text).
///
/// Annotation rows wrap to `wrap_width` instead of truncating: the exact
/// model/provider/schema identifiers are what revision feedback must copy
/// verbatim, so clipping them defeats the review. Body (prompt) rows keep
/// the render-side truncation.
///
/// Render (`push_focused_workflow_detail`) and height measurement
/// (`permission_body_line_count`) must agree on the row count, so both
/// build the rows here with the same `wrap_width`.
fn workflow_detail_content_rows(
    graph: &WorkflowGraph,
    node: &WorkflowGraphNode,
    wrap_width: usize,
) -> Vec<(String, bool)> {
    let push_wrapped = |rows: &mut Vec<(String, bool)>, text: &str| {
        rows.extend(
            wrap_display_columns(text, wrap_width)
                .into_iter()
                .map(|line| (line, true)),
        );
    };
    let push_fact = |rows: &mut Vec<(String, bool)>, label: &str, value: Option<&str>| {
        if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
            push_wrapped(rows, &format!("{label}: {value}"));
        }
    };

    let args = graph.args.as_ref();
    let mut rows: Vec<(String, bool)> = Vec::new();
    let detail = node.detail.as_deref().unwrap_or("");
    if !detail.trim().is_empty() {
        let expanded =
            rebon_tools_core::workflow_graph::expand_workflow_template_text(detail, args);
        rows.extend(
            expanded
                .lines()
                .take(WORKFLOW_DETAIL_BODY_MAX_LINES)
                .map(|line| (line.to_string(), false)),
        );
    }
    if let Some(meta) = node.agent.as_ref() {
        // A prompt the static scan classified as a script expression carries
        // no detail — but a template made purely of resolvable `${args.…}`
        // placeholders trips that heuristic too, and the reviewer should see
        // the actual argument values, not the source expression. Whether the
        // result is still dynamic comes from the expansion's own counters —
        // never from the lexical shape of the output, which misreads a
        // resolved `foo.bar` as an expression and a CJK-embedded
        // `${lens.key}` remainder as plain prose.
        if detail.trim().is_empty() {
            if let Some(prompt) = meta
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|prompt| !prompt.is_empty())
            {
                let expansion =
                    rebon_tools_core::workflow_graph::expand_workflow_template(prompt, args);
                if expansion.unresolved_placeholders == 0 && expansion.resolved_placeholders > 0 {
                    rows.extend(
                        expansion
                            .text
                            .lines()
                            .take(WORKFLOW_DETAIL_BODY_MAX_LINES)
                            .map(|line| (line.to_string(), false)),
                    );
                } else {
                    push_wrapped(&mut rows, &format!("Dynamic prompt: {}", expansion.text));
                }
            }
        }
        push_fact(&mut rows, "Model", meta.model.as_deref());
        push_fact(&mut rows, "Provider", meta.provider.as_deref());
        push_fact(&mut rows, "Model profile", meta.model_profile.as_deref());
        push_fact(&mut rows, "Isolation", meta.isolation.as_deref());
        push_fact(&mut rows, "Agent type", meta.agent_type.as_deref());
        if let Some(schema) = meta
            .schema_name
            .as_deref()
            .map(str::trim)
            .filter(|schema| !schema.is_empty())
        {
            push_wrapped(&mut rows, &format!("Schema: {schema}"));
        } else if meta.has_schema {
            rows.push(("Schema: structured output required".to_string(), true));
        }
    }
    if rows.is_empty() {
        rows.push(("(no detail)".to_string(), true));
    }
    rows
}

/// Detail lines under the focused node's row — the node detail plus agent
/// facts in Browse mode, the live edit buffer in Edit mode — boxed with
/// borders on all four sides and aligned with the review table
/// (`box_width` is the table's full width) so the expansion stays visually
/// contained instead of bleeding to the modal edge. Height measurement
/// (`permission_body_line_count`) counts the same lines: content rows
/// plus the two border rows.
fn push_focused_workflow_detail<'a>(
    review: &WorkflowReviewState,
    node: &WorkflowGraphNode,
    box_width: usize,
    all_lines: &mut Vec<Line<'a>>,
) {
    let ds = rebon_design_system::theme::get_active_theme();
    let dim = Style::default().fg(parse_theme_color(ds.inactive));
    // `│ ` + content + ` │`; the table is never narrower than its
    // `Phase │ Agents` header, but keep the subtraction safe anyway.
    let box_width = box_width.max(8);
    let inner_width = box_width - 4;
    let horizontal = "─".repeat(box_width - 2);
    let rows: Vec<(String, bool)> = if review.mode == WorkflowReviewMode::Edit {
        let buffer = review.edit_buffer.as_str();
        if buffer.trim().is_empty() {
            vec![("(no detail)".to_string(), true)]
        } else {
            buffer
                .lines()
                .take(WORKFLOW_DETAIL_BODY_MAX_LINES)
                .map(|line| (line.to_string(), false))
                .collect()
        }
    } else {
        workflow_detail_content_rows(&review.graph, node, inner_width)
    };
    let boxed_row = |text: &str, style: Style| {
        Line::from(vec![
            Span::raw(" "),
            Span::styled("│ ".to_string(), dim),
            Span::styled(workflow_review_pad_cell(text, inner_width), style),
            Span::styled(" │".to_string(), dim),
        ])
    };
    all_lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(format!("┌{horizontal}┐"), dim),
    ]));
    for (text, dimmed) in &rows {
        all_lines.push(boxed_row(
            text,
            if *dimmed { dim } else { Style::default() },
        ));
    }
    all_lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(format!("└{horizontal}┘"), dim),
    ]));
}

fn workflow_node_title(node: &rebon_tools_core::WorkflowGraphNode) -> String {
    match node.node_type {
        WorkflowGraphNodeType::Overview => node.title.clone(),
        WorkflowGraphNodeType::Phase => node.title.clone(),
        WorkflowGraphNodeType::Agent => node.title.clone(),
        WorkflowGraphNodeType::Call => node.title.clone(),
        WorkflowGraphNodeType::Warning => {
            format!("Warning: {}", node.detail.as_deref().unwrap_or(&node.title))
        }
        WorkflowGraphNodeType::Error => {
            format!("Error: {}", node.detail.as_deref().unwrap_or(&node.title))
        }
        WorkflowGraphNodeType::Schema => node.title.clone(),
        WorkflowGraphNodeType::Log => node.title.clone(),
    }
}

fn workflow_node_style(node_type: WorkflowGraphNodeType, status: WorkflowGraphNodeStatus) -> Style {
    let ds = rebon_design_system::theme::get_active_theme();
    match status {
        WorkflowGraphNodeStatus::Failed => Style::default().fg(parse_theme_color(ds.error)),
        WorkflowGraphNodeStatus::Warning => Style::default().fg(parse_theme_color(ds.warning)),
        WorkflowGraphNodeStatus::Completed => Style::default().fg(parse_theme_color(ds.success)),
        WorkflowGraphNodeStatus::Running => Style::default().fg(parse_theme_color(ds.suggestion)),
        WorkflowGraphNodeStatus::Info => Style::default().fg(parse_theme_color(ds.inactive)),
        WorkflowGraphNodeStatus::Pending => match node_type {
            WorkflowGraphNodeType::Warning => Style::default().fg(parse_theme_color(ds.warning)),
            WorkflowGraphNodeType::Error => Style::default().fg(parse_theme_color(ds.error)),
            WorkflowGraphNodeType::Schema => Style::default().fg(parse_theme_color(ds.inactive)),
            _ => Style::default(),
        },
    }
}

fn is_workflow_review_heading(line: &str) -> bool {
    matches!(
        line,
        "Overview"
            | "Phases"
            | "Execution graph"
            | "Agent calls"
            | "Warnings/errors"
            | "Script details"
            | "Script excerpt"
            | "Mermaid source (copyable)"
    )
}

/// Render one permission option row. When the note (extra text) is
/// active on the selected option, the label gains a `", "` suffix with
/// either the typed note or — while still empty — a gray placeholder
/// inviting the user to type (mirrors AskUserQuestion's
/// `Other: <type answer>` affordance).
fn option_line<'a>(
    view: &PermissionModalView,
    option: &PermissionOptionView,
    is_selected: bool,
) -> Line<'a> {
    let ds = rebon_design_system::theme::get_active_theme();
    let prefix = if is_selected { " › " } else { "   " };
    let style = if is_selected {
        Style::default()
            .fg(parse_theme_color(ds.inverseText))
            .bg(parse_theme_color(ds.text))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(option_color(option.kind))
    };
    let mut label = option.label.clone();
    let note_visible = is_selected && (view.extra_text_focused || !view.extra_text.is_empty());
    if note_visible {
        label.push_str(", ");
        label.push_str(&view.extra_text);
    }
    let mut spans = vec![Span::styled(format!("{prefix}{label}"), style)];
    if note_visible && view.extra_text.is_empty() {
        spans.push(Span::styled(
            "<type note>",
            Style::default().fg(parse_theme_color(ds.inactive)),
        ));
    }
    Line::from(spans)
}

fn render_standard_options<'a>(view: &PermissionModalView, all_lines: &mut Vec<Line<'a>>) {
    let options = &view.options;
    let selected = view.selected;
    let ds = rebon_design_system::theme::get_active_theme();
    if options.is_empty() {
        all_lines.push(Line::from(vec![
            Span::raw(" "),
            Span::raw("(no permission options)"),
        ]));
    } else {
        for (idx, option) in options.iter().enumerate() {
            all_lines.push(option_line(view, option, idx == selected));
        }
    }

    all_lines.push(Line::from(""));
    all_lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "Tab: append text · Enter: submit · ↑/↓: choose option · Esc: cancel",
            Style::default().fg(parse_theme_color(ds.inactive)),
        ),
    ]));
}

fn permission_body_line_count(view: &PermissionModalView, width: u16) -> u16 {
    match &view.kind {
        PermissionKind::AskUserQuestion {
            questions,
            answers,
            active_question,
            confirmation_active,
            ..
        } => {
            if *confirmation_active && questions.len() > 1 {
                // blank + prompt + blank + confirm/cancel + trailing blank
                6
            } else if let Some(q) = questions.get(*active_question) {
                // blank + question + blank + options/descs + custom answer + preview + rule + chat + trailing blank
                let opt_lines: u16 = q
                    .options
                    .iter()
                    .map(|opt| if opt.description.is_empty() { 1 } else { 2 })
                    .sum();
                let other_lines = answers
                    .get(*active_question)
                    .map(|answer| {
                        ask_user_other_layout(
                            answer,
                            q.options.len() + 1,
                            width,
                            answer.highlighted_row == q.options.len(),
                        )
                        .rows
                        .len()
                        .min(u16::MAX as usize) as u16
                    })
                    .unwrap_or(1);
                let preview_lines = if !q.multi_select {
                    answers
                        .get(*active_question)
                        .and_then(|answer| answer.selected_options.first())
                        .and_then(|idx| q.options.get(*idx))
                        .and_then(|opt| opt.preview.as_ref())
                        .map(|preview| 1 + 1 + preview.lines().take(8).count() as u16)
                        .unwrap_or(0)
                } else {
                    0
                };
                6u16.saturating_add(opt_lines)
                    .saturating_add(other_lines)
                    .saturating_add(preview_lines)
            } else {
                4
            }
        }
        PermissionKind::ExitPlanMode { plan } => {
            let plan_lines = plan.lines().count().min(u16::MAX as usize) as u16;
            let option_lines = view.options.len().max(1).min(u16::MAX as usize) as u16;
            // plan + blank + prompt + blank + options + blank + hint
            plan_lines.saturating_add(5).saturating_add(option_lines)
        }
        PermissionKind::Profile(proposal) => {
            // Must match `render_profile_proposal_lines` exactly: an
            // undercount clips the options off the bottom, which is how a
            // prompt becomes unanswerable.
            let reason_lines = proposal
                .reason
                .as_deref()
                .filter(|reason| !reason.trim().is_empty())
                .map(|reason| reason.lines().count().saturating_add(1))
                .unwrap_or(0);
            let row_lines = proposal.rows.len().max(1).saturating_add(1);
            let note_lines = if proposal.notes.is_empty() {
                0
            } else {
                proposal.notes.len().saturating_add(1)
            };
            let option_lines = view.options.len().max(1);
            // reason + rows + notes + prompt + blank + options
            reason_lines
                .saturating_add(row_lines)
                .saturating_add(note_lines)
                .saturating_add(2)
                .saturating_add(option_lines)
                .min(u16::MAX as usize) as u16
        }
        PermissionKind::WorkflowReview(review) => {
            if review.graph.nodes.is_empty() {
                let review_lines = review
                    .graph
                    .fallback_text
                    .as_deref()
                    .unwrap_or(&view.summary)
                    .lines()
                    .count()
                    .min(u16::MAX as usize) as u16;
                let option_lines = view.options.len().max(1).min(u16::MAX as usize) as u16;
                return review_lines.saturating_add(6).saturating_add(option_lines);
            }
            // Content rows + 2 for the detail box's top/bottom borders
            // (mirrors push_focused_workflow_detail).
            let focused_detail_lines = if review.mode == WorkflowReviewMode::Edit {
                review
                    .graph
                    .focused_node(review.focused_node)
                    .map(|_| {
                        review
                            .edit_buffer
                            .lines()
                            .take(WORKFLOW_DETAIL_BODY_MAX_LINES)
                            .count()
                            .max(1) as u16
                            + 2
                    })
                    .unwrap_or(0)
            } else if review.actions_focused {
                0
            } else {
                review
                    .graph
                    .focused_node(review.focused_node)
                    .map(|node| {
                        // Mirror the renderer's box geometry so wrapped
                        // annotation rows count identically.
                        let rows = workflow_review_rows(&review.graph);
                        let (left_width, rule_width) =
                            workflow_review_column_widths(&review.graph, &rows);
                        let box_width = (left_width + 3 + rule_width).max(8);
                        workflow_detail_content_rows(&review.graph, node, box_width - 4)
                            .len()
                            .max(1) as u16
                            + 2
                    })
                    .unwrap_or(0)
            };
            let graph_lines = workflow_review_rows(&review.graph)
                .len()
                .min(u16::MAX as usize) as u16;
            if review.mode == WorkflowReviewMode::Edit {
                return graph_lines
                    .saturating_add(focused_detail_lines)
                    .saturating_add(3);
            }
            let option_lines = view.options.len().max(1).min(u16::MAX as usize) as u16;
            graph_lines
                .saturating_add(focused_detail_lines)
                .saturating_add(option_lines)
                .saturating_add(7)
        }
        _ => {
            let summary_lines = permission_summary_rows(&view.summary, width)
                .len()
                .min(u16::MAX as usize) as u16;
            let option_lines = view.options.len().max(1).min(u16::MAX as usize) as u16;
            // summary + blank + options + blank + hint
            summary_lines.saturating_add(option_lines).saturating_add(3)
        }
    }
}

fn map_theme_color(color: ThemeColor) -> Color {
    // Derive colors from the design-system palette instead of
    // hardcoded ratatui constants.
    let ds = rebon_design_system::theme::get_active_theme();
    match color {
        ThemeColor::Permission => parse_theme_color(ds.permission),
        ThemeColor::Success => parse_theme_color(ds.success),
        ThemeColor::Warning => parse_theme_color(ds.warning),
        ThemeColor::Error => parse_theme_color(ds.error),
    }
}

fn option_color(kind: PermissionOptionKind) -> Color {
    let ds = rebon_design_system::theme::get_active_theme();
    match kind {
        PermissionOptionKind::AllowOnce => parse_theme_color(ds.success),
        PermissionOptionKind::AllowAlways => parse_theme_color(ds.suggestion),
        PermissionOptionKind::RejectOnce => parse_theme_color(ds.warning),
        PermissionOptionKind::RejectAlways => parse_theme_color(ds.error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use rebon_core::permission::PermissionQueryOption;
    use tokio::sync::oneshot;

    fn sample_query() -> OutboundPermissionQuery {
        let (tx, _rx) = oneshot::channel();
        OutboundPermissionQuery {
            id: 7,
            tool_name: "Read".into(),
            tool_call_id: "toolu_01".into(),
            session_id: "sess-1".into(),
            title: "Read files".into(),
            message: "Read(path=\"Cargo.toml\")".into(),
            tool_input: None,
            metadata: None,
            options: vec![
                PermissionQueryOption {
                    option_id: "allow_once".into(),
                    label: "Allow once".into(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                PermissionQueryOption {
                    option_id: "reject_once".into(),
                    label: "Reject once".into(),
                    kind: PermissionOptionKind::RejectOnce,
                },
            ],
            response_tx: tx,
        }
    }

    fn single_question_permission(answer: AskUserQuestionAnswer) -> PendingPermission {
        PendingPermission::from_query(
            sample_query(),
            "Next step".into(),
            String::new(),
            PermissionKind::AskUserQuestion {
                questions: vec![AskUserQuestionEntry {
                    question: "How should we proceed?".into(),
                    header: "Next step".into(),
                    options: vec![AskUserQuestionOption {
                        label: "Use the default".into(),
                        description: String::new(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
                answers: vec![answer],
                active_question: 0,
                confirmation_active: false,
                confirmation_selected: 0,
                original_input: serde_json::json!({}),
            },
        )
    }

    #[test]
    fn pending_permission_builds_cloneable_view() {
        let pending = PendingPermission::from_query(
            sample_query(),
            permission_title("Read"),
            "Read(path=\"Cargo.toml\")".into(),
            PermissionKind::Generic,
        );
        assert_eq!(pending.view.tool_call_id, "toolu_01");
        assert_eq!(pending.view.options.len(), 2);
        assert_eq!(pending.view.selected, 0);
        assert!(pending.view.extra_text.is_empty());
        assert!(!pending.view.extra_text_focused);
    }

    #[test]
    fn translate_modal_key_handles_confirm_cancel_and_navigation() {
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        let home = KeyEvent::new(KeyCode::Home, KeyModifiers::NONE);
        let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
        let delete = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(translate_modal_key(&enter), PermissionModalAction::Confirm);
        assert_eq!(translate_modal_key(&space), PermissionModalAction::Toggle);
        assert_eq!(translate_modal_key(&esc), PermissionModalAction::Cancel);
        assert_eq!(translate_modal_key(&down), PermissionModalAction::MoveNext);
        assert_eq!(
            translate_modal_key(&left),
            PermissionModalAction::MoveTabPrev
        );
        assert_eq!(
            translate_modal_key(&right),
            PermissionModalAction::MoveTabNext
        );
        assert_eq!(
            translate_modal_key(&home),
            PermissionModalAction::CursorHome
        );
        assert_eq!(translate_modal_key(&end), PermissionModalAction::CursorEnd);
        assert_eq!(translate_modal_key(&delete), PermissionModalAction::Delete);
        assert_eq!(
            translate_modal_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            PermissionModalAction::FocusExtraText
        );
        assert_eq!(
            translate_modal_key(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)),
            PermissionModalAction::MovePrev
        );
        assert_eq!(translate_modal_key(&ctrl_c), PermissionModalAction::Cancel);
    }

    #[test]
    fn parse_ask_user_questions_preserves_preview() {
        let input = serde_json::json!({
            "questions": [{
                "question": "Pick one?",
                "header": "Choice",
                "options": [{
                    "label": "A",
                    "description": "Alpha",
                    "preview": "Preview text"
                }]
            }]
        });
        let questions = parse_ask_user_questions(&input);
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].options[0].label, "A");
        assert_eq!(questions[0].options[0].description, "Alpha");
        assert_eq!(
            questions[0].options[0].preview.as_deref(),
            Some("Preview text")
        );
    }

    #[test]
    fn build_ask_user_updated_input_adds_single_select_preview_annotation() {
        let original = serde_json::json!({"questions": []});
        let questions = vec![AskUserQuestionEntry {
            question: "Pick one?".into(),
            header: "Choice".into(),
            options: vec![AskUserQuestionOption {
                label: "A".into(),
                description: "Alpha".into(),
                preview: Some("Preview text".into()),
            }],
            multi_select: false,
        }];
        let mut answer = AskUserQuestionAnswer::new();
        answer.selected_options.push(0);
        let updated = build_ask_user_updated_input(&original, &questions, &[answer]);
        assert_eq!(updated["answers"]["Pick one?"], "A");
        assert_eq!(
            updated["annotations"]["Pick one?"]["preview"],
            "Preview text"
        );
    }

    #[test]
    fn build_ask_user_updated_input_joins_multi_select_answers() {
        let original = serde_json::json!({"questions": []});
        let questions = vec![AskUserQuestionEntry {
            question: "Pick many?".into(),
            header: "Choice".into(),
            options: vec![
                AskUserQuestionOption {
                    label: "A".into(),
                    description: "".into(),
                    preview: None,
                },
                AskUserQuestionOption {
                    label: "B".into(),
                    description: "".into(),
                    preview: None,
                },
            ],
            multi_select: true,
        }];
        let mut answer = AskUserQuestionAnswer::new();
        answer.selected_options.extend([0, 1]);
        let updated = build_ask_user_updated_input(&original, &questions, &[answer]);
        assert_eq!(updated["answers"]["Pick many?"], "A, B");
    }

    #[test]
    fn build_ask_user_updated_input_uses_other_text_as_answer_only() {
        let original = serde_json::json!({"questions": []});
        let questions = vec![AskUserQuestionEntry {
            question: "Other?".into(),
            header: "Choice".into(),
            options: vec![AskUserQuestionOption {
                label: "A".into(),
                description: "".into(),
                preview: None,
            }],
            multi_select: false,
        }];
        let mut answer = AskUserQuestionAnswer::new();
        answer.other_text = " custom answer ".into();
        let updated = build_ask_user_updated_input(&original, &questions, &[answer]);
        assert_eq!(updated["answers"]["Other?"], "custom answer");
        assert!(updated["annotations"].get("Other?").is_none());
    }

    #[test]
    fn build_ask_user_updated_input_keeps_selected_answer_and_adds_note() {
        let original = serde_json::json!({"questions": []});
        let questions = vec![AskUserQuestionEntry {
            question: "Pick one?".into(),
            header: "Choice".into(),
            options: vec![AskUserQuestionOption {
                label: "A".into(),
                description: "".into(),
                preview: None,
            }],
            multi_select: false,
        }];
        let mut answer = AskUserQuestionAnswer::new();
        answer.selected_options.push(0);
        answer.other_text = " extra context ".into();
        let updated = build_ask_user_updated_input(&original, &questions, &[answer]);
        assert_eq!(updated["answers"]["Pick one?"], "A");
        assert_eq!(
            updated["annotations"]["Pick one?"]["notes"],
            "extra context"
        );
    }

    #[test]
    fn render_permission_inline_paints_title_and_options() {
        let pending = PendingPermission::from_query(
            sample_query(),
            permission_title("Read"),
            "Read(path=\"Cargo.toml\")".into(),
            PermissionKind::Generic,
        );
        let area = Rect::new(0, 0, 80, 12);
        let mut buf = Buffer::empty(area);
        render_permission_inline(&pending.view, area, &mut buf, 0);
        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("Allow Read?"),
            "title missing: {rendered}"
        );
        assert!(
            rendered.contains("Allow once"),
            "allow option missing: {rendered}"
        );
        assert!(
            rendered.contains("Reject once"),
            "reject option missing: {rendered}"
        );
        assert!(
            !rendered.contains("Add note: <Tab to type>"),
            "add-note row should not be rendered: {rendered}"
        );
        assert!(
            rendered.contains("Tab: append text"),
            "append-text hint missing: {rendered}"
        );
    }

    #[test]
    fn render_ask_user_question_matches_numbered_card_layout() {
        let mut answer = AskUserQuestionAnswer::new();
        answer.highlighted_row = 3;
        let question = AskUserQuestionEntry {
            question: "How should we proceed?".into(),
            header: "Next step".into(),
            options: vec![
                AskUserQuestionOption {
                    label: "Add safeguards first".into(),
                    description: "Implement the safeguards now".into(),
                    preview: None,
                },
                AskUserQuestionOption {
                    label: "Restore local memos first".into(),
                    description: "Recover the local memos".into(),
                    preview: None,
                },
                AskUserQuestionOption {
                    label: "Reset the server and push cleanly".into(),
                    description: "Clean up and push again".into(),
                    preview: None,
                },
                AskUserQuestionOption {
                    label: "Do not change anything yet".into(),
                    description: "Keep the current state".into(),
                    preview: None,
                },
            ],
            multi_select: false,
        };
        let pending = PendingPermission::from_query(
            sample_query(),
            "Next step".into(),
            String::new(),
            PermissionKind::AskUserQuestion {
                questions: vec![question],
                answers: vec![answer],
                active_question: 0,
                confirmation_active: false,
                confirmation_selected: 0,
                original_input: serde_json::json!({}),
            },
        );
        let area = Rect::new(0, 0, 100, measure_permission_inline(&pending.view, 100));
        let mut buf = Buffer::empty(area);
        let painted = render_permission_inline(&pending.view, area, &mut buf, 0);
        let top_rule = (0..area.width)
            .map(|x| buf[(x, 1)].symbol())
            .collect::<String>();
        let heading = (0..area.width)
            .map(|x| buf[(x, 2)].symbol())
            .collect::<String>();
        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let bottom_row = (0..area.width)
            .map(|x| buf[(x, area.height - 1)].symbol())
            .collect::<String>();

        assert_eq!(painted, area.height);
        assert!(
            bottom_row.trim().is_empty(),
            "AskUserQuestion should end with a blank row: {bottom_row:?}"
        );
        assert!(
            top_rule.chars().all(|ch| ch == '─'),
            "top rule missing: {top_rule}"
        );
        assert!(
            heading.contains("☐ Next step"),
            "heading missing: {heading}"
        );
        assert!(rendered.contains("How should we proceed?"));
        assert!(rendered.contains("  1. [ ] Add safeguards first"));
        assert!(rendered.contains("     Implement the safeguards now"));
        assert!(rendered.contains("❯ 4. [ ] Do not change anything yet"));
        assert!(rendered.contains("  5. Type something."));
        assert!(rendered.contains("────────"));
        assert!(rendered.contains("  6. Chat about this"));
        assert!(!rendered.contains("Other answer"));
        assert!(!rendered.contains("Space: select"));
    }

    #[test]
    fn render_ask_user_question_custom_text_is_not_fully_highlighted() {
        let mut answer = AskUserQuestionAnswer::new();
        answer.highlighted_row = 1;
        answer.other_text = "custom answer".into();
        let question = AskUserQuestionEntry {
            question: "How should we proceed?".into(),
            header: "Next step".into(),
            options: vec![AskUserQuestionOption {
                label: "Use the default".into(),
                description: String::new(),
                preview: None,
            }],
            multi_select: false,
        };
        let pending = PendingPermission::from_query(
            sample_query(),
            "Next step".into(),
            String::new(),
            PermissionKind::AskUserQuestion {
                questions: vec![question],
                answers: vec![answer],
                active_question: 0,
                confirmation_active: false,
                confirmation_selected: 0,
                original_input: serde_json::json!({}),
            },
        );
        let area = Rect::new(0, 0, 80, measure_permission_inline(&pending.view, 80));
        let mut buf = Buffer::empty(area);
        render_permission_inline(&pending.view, area, &mut buf, 0);

        assert!(buf[(0, 7)].style().add_modifier.contains(Modifier::BOLD));
        assert!(!buf[(5, 7)].style().add_modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(5, 7)].style().fg, Some(Color::Reset));
    }

    #[test]
    fn render_ask_user_question_other_wraps_and_reports_cursor() {
        let mut answer = AskUserQuestionAnswer::new();
        answer.highlighted_row = 1;
        answer.other_text = "hello world".into();
        answer.other_cursor_offset = answer.other_text.len();
        let pending = single_question_permission(answer);
        let width = 12;
        let measured = measure_permission_inline(&pending.view, width);
        assert_eq!(measured, measure_permission_inline(&pending.view, 40) + 1);

        let area = Rect::new(3, 4, width, measured);
        let mut buf = Buffer::empty(area);
        let result = render_permission_inline_with_cursor(&pending.view, area, &mut buf, 0);
        let rows = (area.y..area.y + area.height)
            .map(|y| {
                (area.x..area.x + area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let first_input_row = rows
            .iter()
            .position(|row| row.contains("❯ 2. hello"))
            .expect("first input row");
        let continuation_row = rows
            .iter()
            .position(|row| row.contains("world"))
            .expect("continuation row");

        assert_eq!(continuation_row, first_input_row + 1);
        assert!(rows[continuation_row].starts_with("     world"));
        assert_eq!(result.lines_painted, measured);
        assert_eq!(
            result.cursor,
            Some((area.x + 10, area.y + continuation_row as u16))
        );
    }

    #[test]
    fn render_ask_user_question_empty_other_reports_cursor_before_placeholder() {
        let mut answer = AskUserQuestionAnswer::new();
        answer.highlighted_row = 1;
        let pending = single_question_permission(answer);
        let width = 24;
        let measured = measure_permission_inline(&pending.view, width);
        let area = Rect::new(2, 3, width, measured);
        let mut buf = Buffer::empty(area);
        let result = render_permission_inline_with_cursor(&pending.view, area, &mut buf, 0);
        let placeholder_row = (area.y..area.y + area.height)
            .position(|y| {
                (area.x..area.x + area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .contains("Type something.")
            })
            .expect("placeholder row");

        assert_eq!(
            result.cursor,
            Some((area.x + 5, area.y + placeholder_row as u16))
        );
    }

    #[test]
    fn render_ask_user_question_cursor_tracks_suffix_clipping() {
        let mut answer = AskUserQuestionAnswer::new();
        answer.highlighted_row = 1;
        answer.other_text = "hello world".into();
        answer.other_cursor_offset = answer.other_text.len();
        let pending = single_question_permission(answer);
        let width = 12;
        let measured = measure_permission_inline(&pending.view, width);

        let full_area = Rect::new(0, 0, width, measured);
        let mut full_buf = Buffer::empty(full_area);
        let full = render_permission_inline_with_cursor(&pending.view, full_area, &mut full_buf, 0);
        let (cursor_x, cursor_y) = full.cursor.expect("full cursor");

        let clipped_area = Rect::new(0, 0, width, measured - 2);
        let mut clipped_buf = Buffer::empty(clipped_area);
        let clipped =
            render_permission_inline_with_cursor(&pending.view, clipped_area, &mut clipped_buf, 2);
        assert_eq!(clipped.cursor, Some((cursor_x, cursor_y - 2)));

        let hidden_area = Rect::new(0, 0, width, 2);
        let mut hidden_buf = Buffer::empty(hidden_area);
        let hidden = render_permission_inline_with_cursor(
            &pending.view,
            hidden_area,
            &mut hidden_buf,
            usize::from(cursor_y) + 1,
        );
        assert_eq!(hidden.cursor, None);
    }

    #[test]
    fn render_single_select_keeps_selection_visible_off_highlight() {
        let mut answer = AskUserQuestionAnswer::new();
        answer.highlighted_row = 1;
        answer.selected_options.push(0);
        let pending = PendingPermission::from_query(
            sample_query(),
            "Next step".into(),
            String::new(),
            PermissionKind::AskUserQuestion {
                questions: vec![AskUserQuestionEntry {
                    question: "How should we proceed?".into(),
                    header: "Next step".into(),
                    options: vec![AskUserQuestionOption {
                        label: "Use the default".into(),
                        description: String::new(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
                answers: vec![answer],
                active_question: 0,
                confirmation_active: false,
                confirmation_selected: 0,
                original_input: serde_json::json!({}),
            },
        );
        let area = Rect::new(0, 0, 80, measure_permission_inline(&pending.view, 80));
        let mut buf = Buffer::empty(area);
        render_permission_inline(&pending.view, area, &mut buf, 0);
        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("  1. [✓] Use the default"), "{rendered}");
        assert!(rendered.contains("❯ 2. Type something."), "{rendered}");
    }

    #[test]
    fn render_multi_question_heading_keeps_active_tab_visible_on_narrow_terminals() {
        let question = |header: &str| AskUserQuestionEntry {
            question: format!("{header}?"),
            header: header.into(),
            options: vec![AskUserQuestionOption {
                label: "Yes".into(),
                description: String::new(),
                preview: None,
            }],
            multi_select: false,
        };
        let mut pending = PendingPermission::from_query(
            sample_query(),
            "Question".into(),
            String::new(),
            PermissionKind::AskUserQuestion {
                questions: vec![
                    question("First question heading"),
                    question("Second question heading"),
                    question("Third question heading"),
                ],
                answers: vec![
                    AskUserQuestionAnswer::new(),
                    AskUserQuestionAnswer::new(),
                    AskUserQuestionAnswer::new(),
                ],
                active_question: 1,
                confirmation_active: false,
                confirmation_selected: 0,
                original_input: serde_json::json!({}),
            },
        );
        let area = Rect::new(0, 0, 24, measure_permission_inline(&pending.view, 24));
        let mut buf = Buffer::empty(area);
        let painted = render_permission_inline(&pending.view, area, &mut buf, 0);
        let heading = (0..area.width)
            .map(|x| buf[(x, 2)].symbol())
            .collect::<String>();
        let bottom_row = (0..area.width)
            .map(|x| buf[(x, area.height - 1)].symbol())
            .collect::<String>();
        assert_eq!(painted, area.height);
        assert!(bottom_row.trim().is_empty(), "{bottom_row:?}");
        assert!(heading.contains("☐ Second"), "{heading}");
        assert!(!heading.contains("First"), "{heading}");

        let PermissionKind::AskUserQuestion {
            confirmation_active,
            ..
        } = &mut pending.view.kind
        else {
            unreachable!();
        };
        *confirmation_active = true;
        let confirmation_area = Rect::new(0, 0, 24, measure_permission_inline(&pending.view, 24));
        let mut buf = Buffer::empty(confirmation_area);
        let painted = render_permission_inline(&pending.view, confirmation_area, &mut buf, 0);
        let heading = (0..confirmation_area.width)
            .map(|x| buf[(x, 2)].symbol())
            .collect::<String>();
        let bottom_row = (0..confirmation_area.width)
            .map(|x| buf[(x, confirmation_area.height - 1)].symbol())
            .collect::<String>();
        assert_eq!(painted, confirmation_area.height);
        assert!(bottom_row.trim().is_empty(), "{bottom_row:?}");
        assert!(heading.contains("☐ Confirm"), "{heading}");
    }

    #[test]
    fn render_multi_question_confirmation_tab() {
        let question = |header: &str| AskUserQuestionEntry {
            question: format!("{header}?"),
            header: header.into(),
            options: vec![AskUserQuestionOption {
                label: "Yes".into(),
                description: String::new(),
                preview: None,
            }],
            multi_select: false,
        };
        let pending = PendingPermission::from_query(
            sample_query(),
            "Question".into(),
            String::new(),
            PermissionKind::AskUserQuestion {
                questions: vec![question("First"), question("Second")],
                answers: vec![AskUserQuestionAnswer::new(), AskUserQuestionAnswer::new()],
                active_question: 1,
                confirmation_active: true,
                confirmation_selected: 1,
                original_input: serde_json::json!({}),
            },
        );
        let area = Rect::new(0, 0, 80, measure_permission_inline(&pending.view, 80));
        let mut buf = Buffer::empty(area);
        let painted = render_permission_inline(&pending.view, area, &mut buf, 0);
        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let bottom_row = (0..area.width)
            .map(|x| buf[(x, area.height - 1)].symbol())
            .collect::<String>();

        assert_eq!(painted, area.height);
        assert!(
            bottom_row.trim().is_empty(),
            "confirmation should end with a blank row: {bottom_row:?}"
        );
        assert!(rendered.contains("First  Second  ☐ Confirm"));
        assert!(!rendered.contains("3/3"));
        assert!(rendered.contains("Submit your answers?"));
        assert!(rendered.contains("  1. Confirm"));
        assert!(rendered.contains("❯ 2. Cancel"));
    }

    #[test]
    fn render_permission_inline_paints_workflow_review_and_options() {
        let review = "Overview\n- Name: demo\n\nPhases\n1. Run\n\nAgent calls\n1. line 3: prompt \"do work\"\n\nWarnings/errors\n- none\n\nScript details\n- JavaScript source is an implementation detail for audit. Review the plan above first.\n\nScript excerpt\n  1| export const meta = {}";
        let pending = PendingPermission::from_query(
            sample_query(),
            "Review workflow before running".into(),
            review.into(),
            PermissionKind::WorkflowReview(WorkflowReviewState::new(WorkflowGraph {
                fallback_text: Some(review.into()),
                ..Default::default()
            })),
        );
        let area = Rect::new(0, 0, 100, measure_permission_inline(&pending.view, 100) + 2);
        let mut buf = Buffer::empty(area);
        render_permission_inline(&pending.view, area, &mut buf, 0);
        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Review workflow before running"));
        assert!(rendered.contains("Overview"));
        assert!(rendered.contains("Agent calls"));
        assert!(rendered.contains("Script details"));
        assert!(rendered.contains("Run this workflow?"));
        assert!(rendered.contains("Reject once"));
        assert!(measure_permission_inline(&pending.view, 100) > 12);
    }

    /// A structured review graph renders as a two-column table: each
    /// phase shares a row with its first contained call, childless
    /// phases get a `—` placeholder, and node order is regrouped so
    /// ↑/↓ navigation follows the table rows.
    #[test]
    fn workflow_review_renders_phase_agent_table() {
        let metadata = serde_json::json!({
            "kind": "workflowReview",
            "name": "demo",
            "title": "Demo Workflow",
            "description": "Review files",
            "phases": [
                {"title": "Inspect", "detail": "Find files"},
                {"title": "Verify", "detail": "Run checks"}
            ],
            "calls": [
                {"kind": "agent", "line": 12, "summary": "prompt \"inspect\"", "phase": "Inspect"}
            ]
        });
        let graph = WorkflowGraph::from_permission_metadata(&metadata).expect("graph");
        let review = WorkflowReviewState::new(graph);
        // Visual reorder: the Inspect agent moves between its phase and
        // Verify so focus order matches the rendered rows.
        assert!(
            review.graph.nodes[1].title.contains("Inspect"),
            "{:?}",
            review.graph.nodes
        );
        assert!(
            review.graph.nodes[2].title.contains("Agent 1"),
            "{:?}",
            review.graph.nodes
        );
        assert!(
            review.graph.nodes[3].title.contains("Verify"),
            "{:?}",
            review.graph.nodes
        );

        let pending = PendingPermission::from_query(
            sample_query(),
            "Review workflow before running".into(),
            "summary".into(),
            PermissionKind::WorkflowReview(review),
        );
        let measured = measure_permission_inline(&pending.view, 120);
        let area = Rect::new(0, 0, 120, measured + 2);
        let mut buf = Buffer::empty(area);
        let painted = render_permission_inline(&pending.view, area, &mut buf, 0);
        assert_eq!(
            measured, painted,
            "measured height must match painted height"
        );
        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("│ Agents"), "{rendered}");
        assert!(rendered.contains("─┼─"), "{rendered}");
        let inspect_row = rendered
            .lines()
            .find(|line| line.contains("Phase 1: Inspect"))
            .expect("inspect row");
        assert!(
            inspect_row.contains("Agent 1"),
            "phase shares its row with the first call: {rendered}"
        );
        let verify_row = rendered
            .lines()
            .find(|line| line.contains("Phase 2: Verify"))
            .expect("verify row");
        assert!(
            verify_row.contains('—'),
            "childless phase shows a placeholder: {rendered}"
        );
    }

    /// The focused node's detail expands inside a bordered box that
    /// spans the same columns as the review table, and over-long detail
    /// lines truncate at the box edge instead of bleeding across the
    /// modal.
    #[test]
    fn workflow_review_focused_detail_renders_bordered_box() {
        let metadata = serde_json::json!({
            "kind": "workflowReview",
            "name": "demo",
            "title": "Demo Workflow",
            "description": "Review files",
            "phases": [
                {"title": "Inspect", "detail": "Find files"}
            ],
            "calls": [
                {"kind": "agent", "line": 12, "summary": "prompt \"inspect\"", "phase": "Inspect"}
            ]
        });
        let graph = WorkflowGraph::from_permission_metadata(&metadata).expect("graph");
        let mut review = WorkflowReviewState::new(graph);
        let agent_idx = review
            .graph
            .nodes
            .iter()
            .position(|node| node.title.contains("Agent 1"))
            .expect("agent node");
        review.graph.nodes[agent_idx].detail = Some(format!("prompt {}", "x".repeat(200)));
        review.focused_node = agent_idx;

        let pending = PendingPermission::from_query(
            sample_query(),
            "Review workflow before running".into(),
            "summary".into(),
            PermissionKind::WorkflowReview(review),
        );
        let measured = measure_permission_inline(&pending.view, 120);
        let area = Rect::new(0, 0, 120, measured + 2);
        let mut buf = Buffer::empty(area);
        let painted = render_permission_inline(&pending.view, area, &mut buf, 0);
        assert_eq!(
            measured, painted,
            "measured height must match painted height"
        );
        let lines: Vec<String> = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let rendered = lines.join("\n");

        let rule = lines
            .iter()
            .find(|line| line.contains("─┼─"))
            .expect("table rule");
        let top = lines
            .iter()
            .find(|line| line.contains('┌') && line.contains('┐'))
            .expect("detail box top border");
        let bottom = lines
            .iter()
            .find(|line| line.contains('└') && line.contains('┘'))
            .expect("detail box bottom border");
        assert_eq!(
            rule.trim_end().len(),
            top.trim_end().len(),
            "detail box aligns with the table columns: {rendered}"
        );
        assert_eq!(top.trim_end().len(), bottom.trim_end().len(), "{rendered}");
        let detail_row = lines
            .iter()
            .find(|line| line.contains("prompt xxx"))
            .expect("detail content row");
        assert!(
            detail_row.contains("│ prompt"),
            "detail row carries a left border: {rendered}"
        );
        assert!(
            detail_row.trim_end().ends_with("... │"),
            "over-long detail truncates inside the right border: {rendered}"
        );
    }

    fn render_workflow_review_to_text(review: WorkflowReviewState) -> String {
        let pending = PendingPermission::from_query(
            sample_query(),
            "Review workflow before running".into(),
            "summary".into(),
            PermissionKind::WorkflowReview(review),
        );
        let measured = measure_permission_inline(&pending.view, 120);
        let area = Rect::new(0, 0, 120, measured + 2);
        let mut buf = Buffer::empty(area);
        let painted = render_permission_inline(&pending.view, area, &mut buf, 0);
        assert_eq!(
            measured, painted,
            "measured height must match painted height"
        );
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn agent_meta_review_graph() -> WorkflowGraph {
        let metadata = serde_json::json!({
            "kind": "workflowReview",
            "name": "demo",
            "title": "Demo Workflow",
            "description": "Review files",
            "args": {"topic": "caching", "angles": ["speed", "polish"]},
            "phases": [{"title": "Inspect"}],
            "calls": [
                {"kind": "agent", "line": 12, "summary": "prompt \"inspect ${args.topic}\"",
                 "phase": "Inspect",
                 "agent": {"label": "inspector", "prompt": "inspect ${args.topic}",
                           "model": "m1", "provider": "p1", "modelProfile": "large",
                           "isolation": "worktree", "agentType": "Explore",
                           "hasSchema": true, "schemaName": "REVIEW_SCHEMA"}},
                {"kind": "agent", "line": 20, "summary": "prompt lens.prompt (dynamic)",
                 "phase": "Inspect",
                 "agent": {"label": "lens", "prompt": "lens.prompt"}},
                {"kind": "agent", "line": 30, "summary": "prompt \"${args.topic}\"",
                 "phase": "Inspect",
                 "agent": {"label": "templated", "prompt": "${args.topic}"}},
                {"kind": "agent", "line": 40, "summary": "prompt \"${args.angles[1]}\"",
                 "phase": "Inspect",
                 "agent": {"label": "indexed", "prompt": "${args.angles[1]}"}}
            ]
        });
        WorkflowGraph::from_permission_metadata(&metadata).expect("graph")
    }

    fn focus_agent(review: &mut WorkflowReviewState, label: &str) {
        review.focused_node = review
            .graph
            .nodes
            .iter()
            .position(|node| node.title.contains(label))
            .unwrap_or_else(|| panic!("{label} node"));
    }

    /// A prompt template made purely of resolvable `${args.…}` placeholders
    /// trips the dynamic-expression heuristic (detail is cleared), but the
    /// reviewer must see the actual argument values, not the raw source.
    #[test]
    fn workflow_review_focused_detail_resolves_pure_placeholder_prompts() {
        let mut review = WorkflowReviewState::new(agent_meta_review_graph());
        focus_agent(&mut review, "templated");
        let rendered = render_workflow_review_to_text(review);
        assert!(rendered.contains("│ caching"), "{rendered}");
        assert!(!rendered.contains("Dynamic prompt"), "{rendered}");

        let mut review = WorkflowReviewState::new(agent_meta_review_graph());
        focus_agent(&mut review, "indexed");
        let rendered = render_workflow_review_to_text(review);
        assert!(rendered.contains("│ polish"), "{rendered}");
        assert!(!rendered.contains("Dynamic prompt"), "{rendered}");
    }

    /// The dynamic-marker judgement follows the expansion's placeholder
    /// counters, not the output's lexical shape: a partial expansion whose
    /// resolved half is CJK keeps the marker, and a fully resolved value
    /// that happens to look like an expression renders as prompt text.
    #[test]
    fn workflow_review_dynamic_marker_follows_placeholder_resolution() {
        let metadata = serde_json::json!({
            "kind": "workflowReview",
            "name": "demo",
            "description": "Review files",
            "args": {"topic": "缓存策略", "path": "foo.bar"},
            "phases": [{"title": "Inspect"}],
            "calls": [
                {"kind": "agent", "line": 5, "summary": "prompt \"${args.topic}-${lens.key}\"",
                 "phase": "Inspect",
                 "agent": {"label": "partial", "prompt": "${args.topic}-${lens.key}"}},
                {"kind": "agent", "line": 6, "summary": "prompt \"${args.path}\"",
                 "phase": "Inspect",
                 "agent": {"label": "shapelike", "prompt": "${args.path}"}}
            ]
        });
        let graph = WorkflowGraph::from_permission_metadata(&metadata).expect("graph");

        let mut review = WorkflowReviewState::new(graph.clone());
        focus_agent(&mut review, "partial");
        let rendered = render_workflow_review_to_text(review);
        assert!(rendered.contains("Dynamic prompt"), "{rendered}");
        // Cell-by-cell buffer extraction leaves a filler cell after every
        // wide (CJK) glyph; strip spaces before matching (the needle has
        // none of its own).
        assert!(
            rendered.replace(' ', "").contains("缓存策略-${lens.key}"),
            "{rendered}"
        );

        let mut review = WorkflowReviewState::new(graph);
        focus_agent(&mut review, "shapelike");
        let rendered = render_workflow_review_to_text(review);
        assert!(rendered.contains("│ foo.bar"), "{rendered}");
        assert!(!rendered.contains("Dynamic prompt"), "{rendered}");
    }

    /// Long exact identifiers (provider/model/schema) wrap across box rows
    /// instead of truncating — revision feedback must copy them verbatim,
    /// so no character may be clipped away.
    #[test]
    fn workflow_review_focused_detail_wraps_long_identifiers() {
        let provider = "deepseek-responses-relay-openai-compatible-alpha-endpoint";
        let metadata = serde_json::json!({
            "kind": "workflowReview",
            "name": "demo",
            "description": "Review files",
            "phases": [{"title": "Go"}],
            "calls": [
                {"kind": "agent", "line": 5, "summary": "prompt \"x\"", "phase": "Go",
                 "agent": {"label": "wrapped-agent", "prompt": "x", "provider": provider}}
            ]
        });
        let graph = WorkflowGraph::from_permission_metadata(&metadata).expect("graph");
        let mut review = WorkflowReviewState::new(graph);
        focus_agent(&mut review, "wrapped-agent");
        let rendered = render_workflow_review_to_text(review);
        // Reassemble the detail-box content rows; the identifier has no
        // spaces, so its characters survive strip-and-concat intact.
        let joined: String = rendered
            .lines()
            .filter(|line| line.trim_start().starts_with('│') && line.trim_end().ends_with('│'))
            .map(|line| {
                line.trim_start()
                    .trim_start_matches('│')
                    .trim_start_matches(' ')
                    .trim_end()
                    .trim_end_matches('│')
                    .trim_end()
                    .to_string()
            })
            .collect();
        assert!(
            joined.contains(provider),
            "full identifier must survive wrapping: {rendered}"
        );
    }

    /// The focused agent's detail box surfaces the structured routing and
    /// contract facts (model, provider, profile, isolation, agent type,
    /// schema) and expands `${args.…}` placeholders against the launch
    /// args — approving a workflow means approving exactly this config.
    #[test]
    fn workflow_review_focused_detail_shows_agent_meta_and_expands_args() {
        let graph = agent_meta_review_graph();
        let mut review = WorkflowReviewState::new(graph);
        review.focused_node = review
            .graph
            .nodes
            .iter()
            .position(|node| node.title.contains("inspector"))
            .expect("inspector node");

        let rendered = render_workflow_review_to_text(review);
        assert!(rendered.contains("inspect caching"), "{rendered}");
        assert!(rendered.contains("Model: m1"), "{rendered}");
        assert!(rendered.contains("Provider: p1"), "{rendered}");
        assert!(rendered.contains("Model profile: large"), "{rendered}");
        assert!(rendered.contains("Isolation: worktree"), "{rendered}");
        assert!(rendered.contains("Agent type: Explore"), "{rendered}");
        assert!(rendered.contains("Schema: REVIEW_SCHEMA"), "{rendered}");
    }

    /// A dynamic (script-expression) prompt renders as an explicit marker
    /// instead of "(no detail)".
    #[test]
    fn workflow_review_focused_detail_marks_dynamic_prompts() {
        let graph = agent_meta_review_graph();
        let mut review = WorkflowReviewState::new(graph);
        review.focused_node = review
            .graph
            .nodes
            .iter()
            .position(|node| node.title.contains("lens"))
            .expect("lens node");

        let rendered = render_workflow_review_to_text(review);
        assert!(
            rendered.contains("Dynamic prompt: lens.prompt"),
            "{rendered}"
        );
        assert!(!rendered.contains("(no detail)"), "{rendered}");
    }

    #[test]
    fn render_permission_inline_shows_extra_text() {
        let mut pending = PendingPermission::from_query(
            sample_query(),
            permission_title("Read"),
            "Read(path=\"Cargo.toml\")".into(),
            PermissionKind::Generic,
        );
        pending.view.extra_text = "use workspace root".into();
        pending.view.extra_text_focused = true;
        let area = Rect::new(0, 0, 80, 12);
        let mut buf = Buffer::empty(area);
        render_permission_inline(&pending.view, area, &mut buf, 0);
        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("Allow once, use workspace root"),
            "extra text missing from selected option: {rendered}"
        );
        assert!(
            !rendered.contains("Add note: use workspace root"),
            "add-note row should not be rendered: {rendered}"
        );
        assert!(
            !rendered.contains("<type note>"),
            "placeholder must disappear once a note is typed: {rendered}"
        );
    }

    #[test]
    fn render_permission_inline_shows_note_placeholder_when_note_empty() {
        let mut pending = PendingPermission::from_query(
            sample_query(),
            permission_title("Read"),
            "Read(path=\"Cargo.toml\")".into(),
            PermissionKind::Generic,
        );
        pending.view.extra_text_focused = true;
        let area = Rect::new(0, 0, 80, 12);
        let mut buf = Buffer::empty(area);
        render_permission_inline(&pending.view, area, &mut buf, 0);
        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("Allow once, <type note>"),
            "empty focused note should render gray placeholder: {rendered}"
        );
    }

    #[test]
    fn render_permission_inline_workflow_note_shows_placeholder_when_empty() {
        let graph = WorkflowGraph {
            nodes: vec![rebon_tools_core::WorkflowGraphNode {
                id: "phase-1".into(),
                title: "Inspect".into(),
                detail: Some("Find relevant files".into()),
                node_type: WorkflowGraphNodeType::Phase,
                status: WorkflowGraphNodeStatus::Pending,
                editable: true,
                modified: false,
                agent_id: None,
                agent: None,
            }],
            ..Default::default()
        };
        let mut review = WorkflowReviewState::new(graph);
        review.actions_focused = true;
        let mut pending = PendingPermission::from_query(
            sample_query(),
            "Review workflow before running".into(),
            "raw workflow".into(),
            PermissionKind::WorkflowReview(review),
        );
        pending.view.extra_text_focused = true;
        let area = Rect::new(0, 0, 100, measure_permission_inline(&pending.view, 100) + 2);
        let mut buf = Buffer::empty(area);
        render_permission_inline(&pending.view, area, &mut buf, 0);
        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("Allow once, <type note>"),
            "workflow note focus should render gray placeholder: {rendered}"
        );
    }

    #[test]
    fn render_permission_inline_has_top_border_only() {
        let pending = PendingPermission::from_query(
            sample_query(),
            permission_title("Read"),
            "Read(path=\"Cargo.toml\")".into(),
            PermissionKind::Generic,
        );
        let area = Rect::new(0, 0, 60, 10);
        let mut buf = Buffer::empty(area);
        render_permission_inline(&pending.view, area, &mut buf, 0);
        let rows: Vec<String> = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        // Row 0 = blank margin. Row 1 = top border with "─".
        assert!(
            rows[0].trim().is_empty(),
            "margin row should be blank: {:?}",
            rows[0]
        );
        assert!(
            rows[1].contains("─"),
            "border row should have ─: {:?}",
            rows[1]
        );
        assert!(
            rows[1].contains("Allow Read?"),
            "border row should have title: {:?}",
            rows[1]
        );
        // No vertical borders on body rows.
        let body_row = &rows[2]; // summary line
        assert!(
            !body_row.starts_with("│"),
            "no left border on body: {body_row:?}"
        );
    }

    #[test]
    fn render_exit_plan_mode_shows_full_plan() {
        let plan = (1..=35)
            .map(|idx| format!("plan line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        let pending = PendingPermission::from_query(
            sample_query(),
            "Plan ready for review".into(),
            "Review the proposed plan and choose how to proceed.".into(),
            PermissionKind::ExitPlanMode { plan },
        );
        let measured = measure_permission_inline(&pending.view, 80);
        let area = Rect::new(0, 0, 80, measured + 2);
        let mut buf = Buffer::empty(area);
        let painted = render_permission_inline(&pending.view, area, &mut buf, 0);
        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("plan line 35"),
            "last plan line missing: {rendered}"
        );
        assert!(
            !rendered.contains("truncated"),
            "plan should not render a truncation marker: {rendered}"
        );
        assert_eq!(painted, measured);
    }

    #[test]
    fn render_permission_inline_wraps_generic_summary_without_truncating() {
        let summary = concat!(
            "Bash(command=\"repo='/repo/.rebon/worktrees/example'; ",
            "target=\"$repo/target/example-worktree\"; rm -rf -- \"$target\"; ",
            "cargo test --target-dir \"$target\" --workspace --all-targets\")"
        );
        let pending = PendingPermission::from_query(
            sample_query(),
            permission_title("Bash"),
            summary.into(),
            PermissionKind::Generic,
        );
        let width = 48;
        let summary_rows = permission_summary_rows(summary, width);
        let measured = measure_permission_inline(&pending.view, width);
        let area = Rect::new(0, 0, width, measured);
        let mut buf = Buffer::empty(area);
        let painted = render_permission_inline(&pending.view, area, &mut buf, 0);
        let rendered_summary = (2..2 + summary_rows.len() as u16)
            .map(|y| {
                let row = (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>();
                row.trim_end()
                    .strip_prefix(' ')
                    .unwrap_or(row.trim_end())
                    .to_string()
            })
            .collect::<String>();

        assert_eq!(rendered_summary, summary);
        assert_eq!(painted, measured);
    }

    #[test]
    fn measure_permission_inline_matches_render_output() {
        let pending = PendingPermission::from_query(
            sample_query(),
            permission_title("Read"),
            "Read(path=\"Cargo.toml\")".into(),
            PermissionKind::Generic,
        );
        let measured = measure_permission_inline(&pending.view, 60);
        let area = Rect::new(0, 0, 60, measured + 5);
        let mut buf = Buffer::empty(area);
        let painted = render_permission_inline(&pending.view, area, &mut buf, 0);
        assert_eq!(
            measured, painted,
            "measured height must match painted height"
        );
    }

    #[test]
    fn workflow_review_save_without_changes_does_not_mark_modified() {
        let graph = WorkflowGraph {
            nodes: vec![rebon_tools_core::WorkflowGraphNode {
                id: "overview".into(),
                title: "Workflow".into(),
                detail: Some("Requirements: keep scope".into()),
                node_type: WorkflowGraphNodeType::Overview,
                status: WorkflowGraphNodeStatus::Pending,
                editable: true,
                modified: false,
                agent_id: None,
                agent: None,
            }],
            ..Default::default()
        };
        let mut review = WorkflowReviewState::new(graph);
        review.enter_edit();
        review.save_edit();
        assert!(!review.has_pending_edits());

        review.enter_edit();
        review.edit_buffer = "  Requirements: keep scope  ".into();
        review.save_edit();
        assert!(!review.has_pending_edits());

        review.enter_edit();
        review.edit_buffer = "Requirements: narrow scope".into();
        review.save_edit();
        assert!(review.has_pending_edits());

        review.enter_edit();
        review.edit_buffer = "Requirements: keep scope".into();
        review.save_edit();
        assert!(!review.has_pending_edits());
    }

    #[test]
    fn workflow_review_fallback_navigation_moves_options() {
        let mut pending = PendingPermission::from_query(
            sample_query(),
            "Review workflow before running".into(),
            "Overview".into(),
            PermissionKind::WorkflowReview(WorkflowReviewState::new(WorkflowGraph {
                fallback_text: Some("Overview".into()),
                ..Default::default()
            })),
        );

        pending.view.move_next();
        assert_eq!(pending.view.selected, 1);
        pending.view.move_prev();
        assert_eq!(pending.view.selected, 0);
    }

    #[test]
    fn workflow_review_measure_matches_graph_render_states() {
        let graph = WorkflowGraph {
            nodes: vec![
                rebon_tools_core::WorkflowGraphNode {
                    id: "overview".into(),
                    title: "Workflow".into(),
                    detail: Some("one\ntwo".into()),
                    node_type: WorkflowGraphNodeType::Overview,
                    status: WorkflowGraphNodeStatus::Pending,
                    editable: true,
                    modified: false,
                    agent_id: None,
                    agent: None,
                },
                rebon_tools_core::WorkflowGraphNode {
                    id: "phase".into(),
                    title: "Phase".into(),
                    detail: None,
                    node_type: WorkflowGraphNodeType::Phase,
                    status: WorkflowGraphNodeStatus::Pending,
                    editable: true,
                    modified: false,
                    agent_id: None,
                    agent: None,
                },
            ],
            ..Default::default()
        };
        let mut pending = PendingPermission::from_query(
            sample_query(),
            "Review workflow before running".into(),
            "Overview".into(),
            PermissionKind::WorkflowReview(WorkflowReviewState::new(graph)),
        );

        let assert_measured = |view: &PermissionModalView| {
            let measured = measure_permission_inline(view, 120);
            let area = Rect::new(0, 0, 120, measured + 2);
            let mut buf = Buffer::empty(area);
            let painted = render_permission_inline(view, area, &mut buf, 0);
            assert_eq!(measured, painted);
        };

        assert_measured(&pending.view);
        if let PermissionKind::WorkflowReview(review) = &mut pending.view.kind {
            review.focused_node = 1;
        }
        assert_measured(&pending.view);
        if let PermissionKind::WorkflowReview(review) = &mut pending.view.kind {
            review.focused_node = 0;
            review.actions_focused = true;
        }
        assert_measured(&pending.view);
        if let PermissionKind::WorkflowReview(review) = &mut pending.view.kind {
            review.actions_focused = false;
            review.enter_edit();
            review.edit_buffer = "edited\nlines".into();
        }
        assert_measured(&pending.view);
    }

    #[test]
    fn render_permission_inline_skip_lines_clips_top() {
        let pending = PendingPermission::from_query(
            sample_query(),
            permission_title("Read"),
            "Read(path=\"Cargo.toml\")".into(),
            PermissionKind::Generic,
        );
        let area = Rect::new(0, 0, 60, 10);
        let mut buf = Buffer::empty(area);
        // Skip margin + border (2 lines).
        let painted = render_permission_inline(&pending.view, area, &mut buf, 2);
        let first_row: String = (0..area.width).map(|x| buf[(x, 0)].symbol()).collect();
        // First visible row should be the body (summary line), not the border.
        assert!(
            first_row.contains("Read(path="),
            "first visible row after skip: {first_row:?}"
        );
        let measured = measure_permission_inline(&pending.view, 60);
        assert_eq!(painted, measured - 2);
    }
}

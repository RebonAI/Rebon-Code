//! The rows the `/tasks` panel wants painted.
//!
//! Everything here turns a [`TaskSnapshot`] into [`PanelRow`]s: the
//! list of selectable entries, and the per-kind detail pane behind each
//! one. It lives with the plugin because every value on these rows is
//! this plugin's — the projectors that read a snapshot, the workflow
//! progress entries, the shell tail — and because a surface that cannot
//! draw ratatui still has to show them.
//!
//! Nothing here paints. A row is text plus a [`RowTone`] role, and what
//! a role looks like is the surface's to decide.

use rebon_dialog::model::{PanelRow, RowTone, TextSpan};
use rebon_width::{truncate_to_width, WidthStr};
use std::collections::HashMap;

use crate::runtime::{
    LocalWorkflowData, TaskData, TaskSnapshot, TaskStatus, WorkflowProgressEntry,
};
use crate::ui::task_activity;
use crate::ui::tasks::common::SemanticColor;
use crate::ui::tasks::shell_detail;
use crate::ui::tasks_view;

/// Columns the activity preview leaves for its `" Activity: "` label
/// and a margin.
const ACTIVITY_LABEL_COLUMNS: usize = 12;

/// Columns a list row leaves for its caret and status tag.
const LIST_ACTIVITY_COLUMNS: usize = 8;

/// Indent every wrapped block of prose is drawn at.
const PROSE_INDENT: usize = 2;

/// Which role a projector's [`SemanticColor`] reads as.
///
/// `Background` has no role of its own: painting a status word in the
/// surface's own background colour is how the web front end hides it,
/// and the nearest thing a row can say is "supporting detail".
fn semantic_tone(color: SemanticColor) -> RowTone {
    match color {
        SemanticColor::Success => RowTone::Success,
        SemanticColor::Error => RowTone::Error,
        SemanticColor::Warning => RowTone::Warning,
        SemanticColor::Background => RowTone::Dim,
    }
}

/// A row of one run.
fn row(text: impl Into<String>, tone: RowTone) -> PanelRow {
    PanelRow::one(TextSpan::new(text, tone))
}

/// A heading inside a detail pane.
fn heading(text: impl Into<String>) -> PanelRow {
    row(text, RowTone::Strong)
}

/// A `" Label: "` run followed by its value.
fn labelled(label: &str, value: impl Into<String>, value_tone: RowTone) -> PanelRow {
    PanelRow::spans(vec![TextSpan::dim(label), TextSpan::new(value, value_tone)])
}

/// Wrap `text` to `width` columns and indent every row it produces.
///
/// A char-count wrap rather than a display-width one: prompts are
/// mostly ASCII, and this is what the pane has always done.
fn wrapped_rows(text: &str, tone: RowTone, indent: usize, width: usize) -> Vec<PanelRow> {
    let wrap_width = width.saturating_sub(indent + 1).max(1);
    let prefix = " ".repeat(indent);
    let mut rows = Vec::new();
    for raw_line in text.split('\n') {
        let mut remaining = raw_line;
        while !remaining.is_empty() {
            let take = remaining.chars().take(wrap_width).collect::<String>();
            rows.push(row(format!("{prefix}{take}"), tone));
            let consumed = take.chars().count();
            remaining = remaining
                .char_indices()
                .nth(consumed)
                .map(|(index, _)| &remaining[index..])
                .unwrap_or("");
        }
        if raw_line.is_empty() {
            rows.push(PanelRow::blank());
        }
    }
    rows
}

/// The status tag ahead of a list row's label.
pub fn status_tag(status: crate::ui::tasks::common::TaskStatus) -> &'static str {
    use crate::ui::tasks::common::TaskStatus as UiStatus;
    match status {
        UiStatus::Running => "[running]",
        UiStatus::Pending => "[pending]",
        UiStatus::Completed => "[done]",
        UiStatus::Failed => "[error]",
        UiStatus::Killed => "[stopped]",
    }
}

/// One row of the selectable list.
///
/// The whole row reverses when it is the selection; the status tag
/// keeps its own dim role underneath, which is what tells a running
/// task from a finished one at a glance.
pub fn list_row(
    label: &str,
    status: crate::ui::tasks::common::TaskStatus,
    activity: Option<&str>,
    selected: bool,
    width: usize,
) -> PanelRow {
    let tone = if selected {
        RowTone::Focus
    } else {
        RowTone::Normal
    };
    let activity_tone = if selected {
        RowTone::Focus
    } else {
        RowTone::Dim
    };
    let caret = if selected { "> " } else { "  " };
    let mut spans = vec![
        TextSpan::new(format!(" {caret}"), tone),
        TextSpan::dim(format!("{} ", status_tag(status))),
        TextSpan::new(label.to_string(), tone),
    ];
    if let Some(activity) = activity {
        let preview_width = width.saturating_sub(LIST_ACTIVITY_COLUMNS);
        spans.push(TextSpan::new(" - ", activity_tone));
        spans.push(TextSpan::new(
            truncate_to_width(activity, preview_width),
            activity_tone,
        ));
    }
    let built = PanelRow::spans(spans);
    if selected {
        built.highlighted()
    } else {
        built
    }
}

/// The `" Activity: "` row above a detail pane, when the registry has
/// reported one for the selection.
pub fn activity_row(activity: &str, width: usize) -> PanelRow {
    let preview_width = width.saturating_sub(ACTIVITY_LABEL_COLUMNS);
    labelled(
        " Activity: ",
        truncate_to_width(activity, preview_width),
        RowTone::Normal,
    )
}

/// The detail pane for one task, dispatched by kind.
///
/// The six kinds with no pane of their own take the generic one, which
/// reads the same projectors the list rows and the footer pill do.
pub fn detail_rows(snap: &TaskSnapshot, now_ms: u64, width: usize) -> Vec<PanelRow> {
    match snap.kind {
        crate::runtime::TaskKind::LocalShell => shell_detail_rows(snap, now_ms),
        crate::runtime::TaskKind::LocalAgent => async_agent_detail_rows(snap, now_ms, width),
        _ => generic_detail_rows(snap, now_ms, width),
    }
}

/// A shell task: status, runtime, the command, and the tail of stdout.
fn shell_detail_rows(snap: &TaskSnapshot, now_ms: u64) -> Vec<PanelRow> {
    let Some(detail) = tasks_view::project_shell_detail(snap, now_ms) else {
        return Vec::new();
    };
    let mut rows = vec![heading(format!(" {}", detail.title))];

    let status_tone = match detail.status_row.color.as_str() {
        "success" => RowTone::Success,
        "error" => RowTone::Error,
        _ => RowTone::Dim,
    };
    let mut status = vec![
        TextSpan::dim(" Status: "),
        TextSpan::new(detail.status_row.status.clone(), status_tone),
    ];
    if let Some(suffix) = detail.status_row.exit_code_suffix.as_ref() {
        status.push(TextSpan::dim(suffix.clone()));
    }
    rows.push(PanelRow::spans(status));

    rows.push(labelled(
        " Runtime: ",
        tasks_view::format_elapsed_ms(detail.runtime_ms),
        RowTone::Normal,
    ));
    rows.push(labelled(
        &format!(" {} ", detail.command_label),
        detail.command_body.clone(),
        RowTone::Normal,
    ));
    rows.push(PanelRow::blank());
    rows.push(heading(" Output:"));

    match tasks_view::extract_shell_stdout(&snap.result) {
        Some((stdout, bytes_total)) => {
            let tail = shell_detail::extract_tail_lines(&stdout, bytes_total);
            for line in tail.lines.iter() {
                rows.push(row(format!("  {line}"), RowTone::Normal));
            }
            rows.push(row(
                format!(
                    "  {}",
                    shell_detail::format_tail_summary(tail.lines.len(), None)
                ),
                RowTone::Dim,
            ));
        }
        None => rows.push(row("  (no output yet)", RowTone::Dim)),
    }
    rows
}

/// An async agent: title, the subtitle's status and counters, the
/// prompt or plan, an error, and the tail of ACP output.
fn async_agent_detail_rows(snap: &TaskSnapshot, now_ms: u64, width: usize) -> Vec<PanelRow> {
    use crate::ui::tasks::async_agent_detail::AsyncAgentPromptBlock;

    let Some(detail) = tasks_view::project_async_agent_detail(snap, now_ms) else {
        return Vec::new();
    };
    let mut rows = vec![heading(format!(" {}", detail.title))];

    let mut subtitle = vec![TextSpan::normal(" ")];
    if let Some((label, color)) = detail.subtitle.status_label.as_ref() {
        subtitle.push(TextSpan::new(label.clone(), semantic_tone(*color)));
    }
    subtitle.push(TextSpan::dim(detail.subtitle.elapsed_time.clone()));
    for suffix in [
        detail.subtitle.tokens_suffix.as_ref(),
        detail.subtitle.tools_suffix.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        subtitle.push(TextSpan::dim(suffix.clone()));
    }
    rows.push(PanelRow::spans(subtitle));

    if let Some(activity) = task_activity::format_snapshot_activity(snap) {
        rows.push(activity_row(&activity, width));
    }
    rows.push(PanelRow::blank());

    let (label, body) = match &detail.prompt_block {
        AsyncAgentPromptBlock::Plan(plan) => (" Plan:", plan),
        AsyncAgentPromptBlock::Prompt(prompt) => (" Prompt:", prompt),
    };
    rows.push(heading(label));
    rows.extend(wrapped_rows(body, RowTone::Normal, PROSE_INDENT, width));

    if let Some(error) = detail.error.as_ref() {
        rows.push(PanelRow::blank());
        rows.push(row(" Error:", RowTone::Error));
        rows.extend(wrapped_rows(error, RowTone::Error, PROSE_INDENT, width));
    }
    if let Some(acp) = detail.acp_output.as_ref() {
        rows.push(PanelRow::blank());
        rows.push(heading(" Recent output:"));
        rows.extend(wrapped_rows(acp, RowTone::Dim, PROSE_INDENT, width));
    }
    rows
}

/// The kinds with no pane of their own: a title, a status and runtime
/// row, the one-line projected label, then whatever that kind adds.
fn generic_detail_rows(snap: &TaskSnapshot, now_ms: u64, width: usize) -> Vec<PanelRow> {
    let title = match &snap.data {
        TaskData::RemoteAgent(d) if d.is_ultraplan => "Ultraplan details",
        TaskData::RemoteAgent(d) if d.is_remote_review => "Ultrareview details",
        TaskData::RemoteAgent(_) => "Remote session details",
        TaskData::InProcessTeammate(_) => "Teammate details",
        TaskData::LocalWorkflow(_) => "Workflow details",
        TaskData::Monitor(_) => "Monitor details",
        TaskData::MonitorMcp(_) => "MCP monitor details",
        TaskData::Dream(_) => "Memory consolidation",
        _ => "Task details",
    };
    let mut rows = vec![heading(format!(" {title}"))];

    let end = snap.end_time_ms.unwrap_or(now_ms);
    let runtime = tasks_view::format_elapsed_ms(end.saturating_sub(snap.start_time_ms));
    let (status_label, status_tone) = match snap.status {
        TaskStatus::Running => ("running", RowTone::Dim),
        TaskStatus::Pending => ("pending", RowTone::Dim),
        TaskStatus::Completed => ("completed", RowTone::Success),
        TaskStatus::Failed => ("failed", RowTone::Error),
        TaskStatus::Killed => ("stopped", RowTone::Warning),
    };
    rows.push(PanelRow::spans(vec![
        TextSpan::dim(" Status: "),
        TextSpan::new(status_label, status_tone),
        TextSpan::dim(" · "),
        TextSpan::normal(runtime),
    ]));
    rows.push(row(
        format!(" {}", tasks_view::snapshot_label(snap)),
        RowTone::Normal,
    ));

    match &snap.data {
        TaskData::RemoteAgent(d) => {
            rows.push(labelled(
                " Session: ",
                d.session_id.clone(),
                RowTone::Normal,
            ));
            if let Some(progress) = d.review_progress.as_ref() {
                let stage = progress
                    .stage
                    .map(|stage| stage.as_str())
                    .unwrap_or("pending");
                rows.push(row(
                    format!(
                        " Review: {stage} · {} found · {} verified · {} refuted",
                        progress.bugs_found, progress.bugs_verified, progress.bugs_refuted
                    ),
                    RowTone::Dim,
                ));
            }
        }
        TaskData::InProcessTeammate(d) => {
            rows.push(labelled(
                " Team: ",
                d.identity.team_name.clone(),
                RowTone::Normal,
            ));
            rows.push(heading(" Prompt:"));
            rows.extend(wrapped_rows(
                &d.prompt,
                RowTone::Normal,
                PROSE_INDENT,
                width,
            ));
        }
        TaskData::LocalWorkflow(d) => rows.extend(local_workflow_rows(d)),
        TaskData::Monitor(d) => {
            rows.push(PanelRow::spans(vec![
                TextSpan::dim(" Source: "),
                TextSpan::normal(d.source.as_str()),
                TextSpan::dim(" · Target: "),
                TextSpan::normal(d.redacted_target.clone()),
            ]));
            let mut counters = vec![
                TextSpan::dim(" Events: "),
                TextSpan::normal(d.event_count.to_string()),
                TextSpan::dim(" · Suppressed: "),
                TextSpan::normal(d.suppressed_count.to_string()),
            ];
            if let Some(reason) = d.end_reason {
                counters.push(TextSpan::dim(" · End: "));
                counters.push(TextSpan::normal(reason.as_str()));
            }
            rows.push(PanelRow::spans(counters));
        }
        TaskData::MonitorMcp(d) => {
            rows.push(labelled(
                " Server: ",
                d.server_name.clone(),
                RowTone::Normal,
            ));
        }
        TaskData::Dream(d) => {
            rows.push(PanelRow::spans(vec![
                TextSpan::dim(" Phase: "),
                TextSpan::normal(format!("{:?}", d.phase).to_lowercase()),
                TextSpan::dim(format!(
                    " · reviewing {} session{} · {} files touched",
                    d.sessions_reviewing,
                    if d.sessions_reviewing == 1 { "" } else { "s" },
                    d.files_touched.len()
                )),
            ]));
        }
        _ => {}
    }
    rows
}

/// A local workflow run: its name and counters, then the phase/agent
/// table the transcript card draws with the same layout.
fn local_workflow_rows(d: &LocalWorkflowData) -> Vec<PanelRow> {
    let mut rows = vec![PanelRow::spans(vec![
        TextSpan::dim(" Workflow: "),
        TextSpan::normal(d.workflow_name.clone()),
        TextSpan::dim(format!(" · run {}", d.run_id)),
    ])];

    let mut stats = vec![format!("{} agents", d.agent_count)];
    if d.token_count > 0 {
        stats.push(format!("{} tokens", d.token_count));
    }
    if d.tool_use_count > 0 {
        stats.push(format!("{} tool calls", d.tool_use_count));
    }
    rows.push(labelled(" Stats: ", stats.join(" · "), RowTone::Normal));

    if let Some(summary) = d
        .summary
        .as_deref()
        .filter(|summary| !summary.trim().is_empty())
    {
        rows.push(labelled(" Summary: ", summary, RowTone::Normal));
    }
    for (label, value) in [
        (" Output: ", d.output_path.as_deref()),
        (" Script: ", d.script_path.as_deref()),
    ] {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            rows.push(labelled(label, value, RowTone::Normal));
        }
    }

    let tree = build_workflow_detail_tree(&d.progress_entries);
    if !tree.logs.is_empty() {
        rows.push(heading(" Logs:"));
        for log in tree.logs {
            rows.push(row(format!("   • {log}"), RowTone::Normal));
        }
    }
    if !tree.phases.is_empty() {
        rows.push(heading(" Phases:"));
        for line in workflow_detail_table_lines(&tree.phases) {
            rows.push(workflow_detail_table_row(&line));
        }
    }
    rows
}

#[derive(Debug, Clone)]
struct WorkflowDetailAgentRow {
    index: u64,
    agent_id: Option<String>,
    state: String,
    label: String,
    tokens: u64,
    tool_calls: u64,
    duration_ms: Option<u64>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct WorkflowDetailPhaseGroup {
    title: String,
    state: String,
    agents: Vec<WorkflowDetailAgentRow>,
}

#[derive(Debug, Clone)]
struct WorkflowDetailTree {
    phases: Vec<WorkflowDetailPhaseGroup>,
    logs: Vec<String>,
}

fn build_workflow_detail_tree(entries: &[WorkflowProgressEntry]) -> WorkflowDetailTree {
    fn phase_key(title: &str, phase_id: Option<&str>) -> String {
        phase_id
            .map(|phase_id| format!("id:{phase_id}"))
            .unwrap_or_else(|| format!("title:{title}"))
    }

    let mut phases: Vec<WorkflowDetailPhaseGroup> = Vec::new();
    let mut phase_index: HashMap<String, usize> = HashMap::new();
    let mut agents_by_id: HashMap<String, (usize, usize)> = HashMap::new();
    let mut logs = Vec::new();
    let mut current_phase: Option<String> = None;

    for entry in entries {
        match entry {
            WorkflowProgressEntry::Phase {
                title,
                state,
                phase_id,
            } => {
                let key = phase_key(title, phase_id.as_deref());
                current_phase = Some(key.clone());
                if let Some(index) = phase_index.get(&key).copied() {
                    phases[index].state = state.clone();
                } else {
                    phase_index.insert(key, phases.len());
                    phases.push(WorkflowDetailPhaseGroup {
                        title: title.clone(),
                        state: state.clone(),
                        agents: Vec::new(),
                    });
                }
            }
            WorkflowProgressEntry::Agent {
                index,
                state,
                phase_title,
                phase_id,
                label,
                tokens,
                tool_calls,
                duration_ms,
                error,
                agent_id,
                ..
            } => {
                let title = phase_title
                    .clone()
                    .unwrap_or_else(|| "Workflow".to_string());
                let explicit_key = phase_id
                    .as_deref()
                    .map(|phase_id| phase_key(&title, Some(phase_id)))
                    .or_else(|| phase_title.as_deref().map(|title| phase_key(title, None)));
                let key = explicit_key
                    .or_else(|| current_phase.clone())
                    .unwrap_or_else(|| phase_key("Workflow", None));
                let existing_by_id = agent_id
                    .as_ref()
                    .and_then(|agent_id| agents_by_id.get(agent_id).copied());
                let phase_pos = existing_by_id
                    .map(|(phase_pos, _)| phase_pos)
                    .or_else(|| phase_index.get(&key).copied());
                let phase_pos = if let Some(index) = phase_pos {
                    index
                } else {
                    phase_index.insert(key, phases.len());
                    phases.push(WorkflowDetailPhaseGroup {
                        title: title.clone(),
                        state: "start".to_string(),
                        agents: Vec::new(),
                    });
                    phases.len() - 1
                };
                let row = WorkflowDetailAgentRow {
                    index: *index,
                    agent_id: agent_id.clone(),
                    state: state.clone(),
                    label: label.clone(),
                    tokens: *tokens,
                    tool_calls: *tool_calls,
                    duration_ms: *duration_ms,
                    error: error.clone(),
                };
                if let Some((existing_phase, existing_agent)) = existing_by_id {
                    phases[existing_phase].agents[existing_agent] = row;
                    continue;
                }
                let agents = &mut phases[phase_pos].agents;
                let without_agent_id = || {
                    agents.iter().position(|agent| {
                        agent.agent_id.is_none()
                            && ((agent.index > 0 && agent.index == row.index)
                                || agent.label == row.label)
                    })
                };
                let agent_pos = if let Some(existing) = without_agent_id() {
                    agents[existing] = row;
                    existing
                } else {
                    agents.push(row);
                    agents.len() - 1
                };
                if let Some(agent_id) = agents[agent_pos].agent_id.as_ref() {
                    agents_by_id.insert(agent_id.clone(), (phase_pos, agent_pos));
                }
            }
            WorkflowProgressEntry::Log { message } => {
                logs.push(message.clone());
            }
        }
    }

    for phase in &mut phases {
        phase.agents.sort_by_key(|agent| agent.index);
    }
    WorkflowDetailTree { phases, logs }
}

fn workflow_detail_agent_line(agent: &WorkflowDetailAgentRow, state_width: usize) -> String {
    let mut parts = vec![format!(
        "{:<state_width$} {}",
        workflow_detail_state_label(&agent.state),
        agent.label
    )];
    if let Some(duration) = agent.duration_ms.filter(|duration| *duration > 0) {
        parts.push(rebon_render::workflow_body::format_duration_ms(duration));
    }
    if agent.tool_calls > 0 {
        parts.push(format!(
            "{} {}",
            agent.tool_calls,
            if agent.tool_calls == 1 {
                "tool"
            } else {
                "tools"
            }
        ));
    }
    if agent.tokens > 0 {
        parts.push(format!("{} tokens", agent.tokens));
    }
    if let Some(error) = agent
        .error
        .as_deref()
        .map(str::trim)
        .filter(|error| !error.is_empty())
    {
        parts.push(format!("error: {error}"));
    }
    parts.join(" · ")
}

/// Two-column phase/agents table for the workflow detail pane — the
/// same layout as the transcript workflow card: a phase's first agent
/// shares its row, further agents continue with an empty phase cell,
/// and an agent-less phase shows a `—` placeholder.
fn workflow_detail_table_lines(phases: &[WorkflowDetailPhaseGroup]) -> Vec<String> {
    const PHASE_COLUMN_MAX_WIDTH: usize = 28;
    const AGENTS_RULE_MAX_WIDTH: usize = 48;
    if phases.is_empty() {
        return Vec::new();
    }
    let phase_state_width = phases
        .iter()
        .map(|phase| workflow_detail_state_label(&phase.state).len())
        .max()
        .unwrap_or(0);
    let agent_state_width = phases
        .iter()
        .flat_map(|phase| phase.agents.iter())
        .map(|agent| workflow_detail_state_label(&agent.state).len())
        .max()
        .unwrap_or(0);
    let rows: Vec<(String, Vec<String>)> = phases
        .iter()
        .map(|phase| {
            let phase_cell = format!(
                "{:<phase_state_width$} {}",
                workflow_detail_state_label(&phase.state),
                phase.title
            );
            let mut agent_cells: Vec<String> = phase
                .agents
                .iter()
                .map(|agent| workflow_detail_agent_line(agent, agent_state_width))
                .collect();
            if agent_cells.is_empty() {
                agent_cells.push("—".to_string());
            }
            (phase_cell, agent_cells)
        })
        .collect();
    let phase_width = rows
        .iter()
        .map(|(cell, _)| WidthStr::width(cell.as_str()))
        .chain(std::iter::once("Phase".len()))
        .max()
        .unwrap_or(0)
        .min(PHASE_COLUMN_MAX_WIDTH);
    let rule_width = rows
        .iter()
        .flat_map(|(_, cells)| cells.iter())
        .map(|cell| WidthStr::width(cell.as_str()))
        .chain(std::iter::once("Agents".len()))
        .max()
        .unwrap_or(0)
        .min(AGENTS_RULE_MAX_WIDTH);
    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(format!(
        "   {} │ Agents",
        workflow_detail_pad_cell("Phase", phase_width)
    ));
    lines.push(format!(
        "   {}─┼─{}",
        "─".repeat(phase_width),
        "─".repeat(rule_width)
    ));
    for (phase_cell, agent_cells) in rows {
        for (row_idx, agent_cell) in agent_cells.into_iter().enumerate() {
            let left = if row_idx == 0 {
                phase_cell.as_str()
            } else {
                ""
            };
            lines.push(format!(
                "   {} │ {agent_cell}",
                workflow_detail_pad_cell(left, phase_width)
            ));
        }
    }
    lines
}

/// One table line, split back into the runs that carry its meaning: a
/// finished state reads as a success, a phase title and an agent name
/// read as content, and the separators stay plain.
fn workflow_detail_table_row(line: &str) -> PanelRow {
    let Some((left, right)) = line.split_once(" │ ") else {
        return row(line.to_string(), RowTone::Normal);
    };
    let mut spans = Vec::new();
    push_workflow_detail_phase_cell_spans(&mut spans, left);
    spans.push(TextSpan::normal(" │ "));
    push_workflow_detail_agent_cell_spans(&mut spans, right);
    PanelRow::spans(spans)
}

fn push_workflow_detail_phase_cell_spans(spans: &mut Vec<TextSpan>, text: &str) {
    let leading_len = text.len() - text.trim_start().len();
    let rest = &text[leading_len..];
    let Some(state_len) = rest.find(char::is_whitespace) else {
        spans.push(TextSpan::normal(text.to_string()));
        return;
    };
    let state = &rest[..state_len];
    let after_state = &rest[state_len..];
    let state_padding_len = after_state.len() - after_state.trim_start().len();
    let title_start = leading_len + state_len + state_padding_len;
    if title_start >= text.len() {
        spans.push(TextSpan::normal(text.to_string()));
        return;
    }

    if leading_len > 0 {
        spans.push(TextSpan::normal(text[..leading_len].to_string()));
    }
    spans.push(TextSpan::new(state.to_string(), state_tone(state)));
    if state_padding_len > 0 {
        spans.push(TextSpan::normal(
            text[leading_len + state_len..title_start].to_string(),
        ));
    }
    spans.push(TextSpan::strong(text[title_start..].to_string()));
}

fn push_workflow_detail_agent_cell_spans(spans: &mut Vec<TextSpan>, text: &str) {
    if text.starts_with("  ") {
        spans.push(TextSpan::normal(text.to_string()));
        return;
    }

    let leading_len = text.len() - text.trim_start().len();
    let rest = &text[leading_len..];
    let Some(state_len) = rest.find(char::is_whitespace) else {
        spans.push(TextSpan::normal(text.to_string()));
        return;
    };
    let state = &rest[..state_len];
    let after_state = &rest[state_len..];
    let state_padding_len = after_state.len() - after_state.trim_start().len();
    let agent_start = leading_len + state_len + state_padding_len;
    if agent_start >= text.len() {
        spans.push(TextSpan::normal(text.to_string()));
        return;
    }
    let agent_end = text[agent_start..]
        .find(" · ")
        .map(|idx| agent_start + idx)
        .unwrap_or(text.len());

    if leading_len > 0 {
        spans.push(TextSpan::normal(text[..leading_len].to_string()));
    }
    spans.push(TextSpan::new(state.to_string(), state_tone(state)));
    if state_padding_len > 0 {
        spans.push(TextSpan::normal(
            text[leading_len + state_len..agent_start].to_string(),
        ));
    }
    spans.push(TextSpan::strong(text[agent_start..agent_end].to_string()));
    if agent_end < text.len() {
        spans.push(TextSpan::normal(text[agent_end..].to_string()));
    }
}

/// A finished cell reads as a success; everything else is content.
fn state_tone(state: &str) -> RowTone {
    if state == "done" {
        RowTone::Success
    } else {
        RowTone::Normal
    }
}

/// Pad `text` with spaces to exactly `width` display columns,
/// truncating when it does not fit.
fn workflow_detail_pad_cell(text: &str, width: usize) -> String {
    let mut cell = truncate_to_width(text, width);
    cell.push_str(&" ".repeat(width.saturating_sub(WidthStr::width(cell.as_str()))));
    cell
}

fn workflow_detail_state_label(state: &str) -> &str {
    match state {
        "completed" | "done" => "done",
        "error" | "failed" => "failed",
        "start" | "started" | "running" => "running",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(index: u64, state: &str, label: &str) -> WorkflowProgressEntry {
        WorkflowProgressEntry::Agent {
            index,
            state: state.into(),
            phase_title: Some("design".into()),
            phase_id: None,
            label: label.into(),
            tokens: 0,
            tool_calls: 0,
            tool_call_details: Vec::new(),
            duration_ms: None,
            error: None,
            agent_id: None,
        }
    }

    #[test]
    fn a_wrapped_block_indents_every_row_and_keeps_its_blank_lines() {
        let rows = wrapped_rows("abcdefgh\n\nij", RowTone::Normal, 2, 8);
        // width 8, indent 2 → 5 columns of text per row.
        assert_eq!(
            rows.iter().map(PanelRow::text).collect::<Vec<_>>(),
            vec!["  abcde", "  fgh", "", "  ij"]
        );
    }

    #[test]
    fn the_selected_list_row_reverses_and_keeps_its_status_tag_readable() {
        use crate::ui::tasks::common::TaskStatus as UiStatus;
        let selected = list_row("build", UiStatus::Running, Some("cargo test"), true, 60);
        assert!(selected.highlighted);
        assert_eq!(selected.spans[1].text, "[running] ");
        assert_eq!(selected.spans[1].tone, RowTone::Dim);
        assert_eq!(selected.spans[2].tone, RowTone::Focus);
        assert!(
            selected.text().contains("cargo test"),
            "{:?}",
            selected.text()
        );

        let unselected = list_row("build", UiStatus::Completed, None, false, 60);
        assert!(!unselected.highlighted);
        assert_eq!(unselected.spans[0].text, "   ");
        assert_eq!(unselected.spans[1].text, "[done] ");
        assert_eq!(unselected.spans[2].tone, RowTone::Normal);
    }

    #[test]
    fn a_finished_state_in_the_phase_table_reads_as_a_success() {
        let tree = build_workflow_detail_tree(&[
            WorkflowProgressEntry::Phase {
                title: "design".into(),
                state: "completed".into(),
                phase_id: None,
            },
            agent(1, "completed", "ui-design"),
        ]);
        let lines = workflow_detail_table_lines(&tree.phases);
        let body = lines.last().expect("one body row");
        let row = workflow_detail_table_row(body);
        let done: Vec<_> = row
            .spans
            .iter()
            .filter(|span| span.text == "done")
            .collect();
        assert_eq!(done.len(), 2, "{:?}", row.spans);
        assert!(done.iter().all(|span| span.tone == RowTone::Success));
        assert!(row
            .spans
            .iter()
            .any(|span| span.text.contains("design") && span.tone == RowTone::Strong));
    }

    #[test]
    fn a_table_line_without_the_column_rule_stays_one_run() {
        let row = workflow_detail_table_row("   no rule here");
        assert_eq!(row.spans.len(), 1);
        assert_eq!(row.spans[0].tone, RowTone::Normal);
    }
}

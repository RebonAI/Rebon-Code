//! Workflow tool body builders: the Phase/Agents progress table and run
//! summary, as `Vec<String>` lines. Every renderer shares them, so a workflow
//! card body is built once. Painting it — span stylers, live-card elision —
//! stays with the consumer.

use std::collections::{HashMap, HashSet};

use rebon_types::{ContentBlock, ToolCallContent};
use rebon_width::{WidthChar, WidthStr};
use serde_json::Value;

use crate::kind::ToolOutputVerbosity;
use crate::streaming::WORKFLOW_INTERRUPTED_MESSAGE;
use crate::summary::compact_json_map;

#[derive(Debug, Clone)]
struct WorkflowProgressEvent {
    entry: WorkflowEntry,
}

#[derive(Debug, Clone)]
enum WorkflowEntry {
    Phase {
        title: String,
        state: String,
        phase_id: Option<String>,
    },
    Agent {
        index: u64,
        state: String,
        phase_title: Option<String>,
        phase_id: Option<String>,
        label: String,
        tokens: u64,
        tool_calls: u64,
        tool_call_details: Vec<Value>,
        duration_ms: Option<u64>,
        error: Option<String>,
        agent_id: Option<String>,
    },
    Log {
        message: String,
    },
}

#[derive(Debug)]
struct WorkflowPhaseGroup {
    title: String,
    state: String,
    agents: Vec<WorkflowAgentRow>,
}

#[derive(Debug, Clone)]
struct WorkflowAgentRow {
    index: u64,
    state: String,
    label: String,
    tokens: u64,
    tool_calls: u64,
    tool_call_details: Vec<Value>,
    duration_ms: Option<u64>,
    error: Option<String>,
    agent_id: Option<String>,
}

#[derive(Debug)]
struct WorkflowTree {
    phases: Vec<WorkflowPhaseGroup>,
    logs: Vec<String>,
    agent_count: usize,
}

pub fn workflow_tool_summary(tool: &crate::streaming::StreamingToolUse) -> Option<String> {
    if tool.tool_name != "Workflow" {
        return None;
    }
    workflow_label(tool).filter(|label| !label.trim().is_empty())
}

pub fn workflow_interruption_lines(tool: &crate::streaming::StreamingToolUse) -> Vec<String> {
    let Some(content) = tool.content.as_ref() else {
        return Vec::new();
    };
    let user_interrupted = content.iter().any(|item| {
        let ToolCallContent::Content(content) = item else {
            return false;
        };
        let ContentBlock::Text(text) = &content.content else {
            return false;
        };
        matches!(
            text.text.trim(),
            WORKFLOW_INTERRUPTED_MESSAGE | "Interrupted by user"
        )
    });
    if user_interrupted {
        vec![WORKFLOW_INTERRUPTED_MESSAGE.to_string()]
    } else {
        Vec::new()
    }
}

pub fn workflow_body_lines(
    tool: &crate::streaming::StreamingToolUse,
    verbosity: ToolOutputVerbosity,
) -> Option<Vec<String>> {
    let raw_output = tool.raw_output.as_ref();
    let events = workflow_events(raw_output?);
    if events.is_empty() {
        // No streaming progress entries survived into this result — e.g. a
        // transcript recorded before `workflowProgress` was persisted, or a
        // run that finished before emitting any event. Synthesize a concise
        // summary from the terminal schema so the card isn't blank.
        let fallback = workflow_fallback_lines(raw_output, verbosity);
        return (!fallback.is_empty()).then_some(fallback);
    }
    let tree = build_workflow_tree(&events);
    let mut lines = Vec::new();
    if let Some(status) = workflow_status(raw_output) {
        lines.push(workflow_run_summary(raw_output, status, &tree));
        if matches!(
            verbosity,
            ToolOutputVerbosity::Normal | ToolOutputVerbosity::Verbose
        ) {
            lines.push(String::new());
        }
    }
    lines.extend(workflow_table_lines(&tree, verbosity));
    lines.extend(tree.logs.iter().map(|log| format!("   Log: {log}")));

    if matches!(verbosity, ToolOutputVerbosity::Verbose) {
        lines.extend(workflow_verbose_lines(tool, raw_output));
    }

    Some(lines)
}

fn workflow_label(tool: &crate::streaming::StreamingToolUse) -> Option<String> {
    tool.raw_output
        .as_ref()
        .and_then(|raw_output| {
            workflow_progress_object(Some(raw_output)).and_then(|progress| {
                string_field(progress, "workflowName").or_else(|| string_field(progress, "summary"))
            })
        })
        .or_else(|| {
            tool.raw_output
                .as_ref()
                .and_then(|raw_output| hash_string_field(raw_output, "workflowName"))
        })
        .or_else(|| {
            tool.raw_input.as_ref().and_then(|input| {
                hash_string_field(input, "name")
                    .or_else(|| hash_string_field(input, "title"))
                    .or_else(|| hash_string_field(input, "description"))
            })
        })
}

fn workflow_progress_object(
    raw_output: Option<&HashMap<String, Value>>,
) -> Option<&serde_json::Map<String, Value>> {
    raw_output?
        .get("workflowProgress")
        .and_then(Value::as_object)
}

fn workflow_events(raw_output: &HashMap<String, Value>) -> Vec<WorkflowProgressEvent> {
    let Some(progress) = workflow_progress_object(Some(raw_output)) else {
        return Vec::new();
    };
    progress
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_workflow_event)
        .collect()
}

fn parse_workflow_event(value: &Value) -> Option<WorkflowProgressEvent> {
    let object = value.as_object()?;
    object.get("sequence").and_then(Value::as_u64)?;
    let entry = object.get("entry")?.as_object()?;
    let entry_type = entry.get("type").and_then(Value::as_str)?;
    let entry = match entry_type {
        "phase" => WorkflowEntry::Phase {
            title: string_field(entry, "title")?,
            state: string_field(entry, "state").unwrap_or_else(|| "start".to_string()),
            phase_id: string_field(entry, "phaseInstanceId")
                .or_else(|| string_field(entry, "phaseId")),
        },
        "agent" => WorkflowEntry::Agent {
            index: entry.get("index").and_then(Value::as_u64).unwrap_or(0),
            state: string_field(entry, "state").unwrap_or_else(|| "start".to_string()),
            phase_title: string_field(entry, "phaseTitle"),
            phase_id: string_field(entry, "phaseInstanceId")
                .or_else(|| string_field(entry, "phaseId")),
            label: string_field(entry, "label").unwrap_or_else(|| "agent".to_string()),
            tokens: entry.get("tokens").and_then(Value::as_u64).unwrap_or(0),
            tool_calls: entry
                .get("toolCalls")
                .or_else(|| entry.get("tool_calls"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            tool_call_details: entry
                .get("toolCallDetails")
                .or_else(|| entry.get("subAgentToolCalls"))
                .or_else(|| entry.get("sub_agent_tool_calls"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            duration_ms: entry
                .get("durationMs")
                .or_else(|| entry.get("duration_ms"))
                .and_then(Value::as_u64),
            error: string_field(entry, "error"),
            agent_id: string_field(entry, "agentId"),
        },
        "log" => WorkflowEntry::Log {
            message: string_field(entry, "message")?,
        },
        _ => return None,
    };
    Some(WorkflowProgressEvent { entry })
}

fn workflow_phase_key(title: &str, phase_id: Option<&str>) -> String {
    phase_id
        .map(|phase_id| format!("id:{phase_id}"))
        .unwrap_or_else(|| format!("title:{title}"))
}

fn build_workflow_tree(events: &[WorkflowProgressEvent]) -> WorkflowTree {
    let mut phases: Vec<WorkflowPhaseGroup> = Vec::new();
    let mut phase_index: HashMap<String, usize> = HashMap::new();
    let mut agents_by_id: HashMap<String, (usize, usize)> = HashMap::new();
    let mut logs = Vec::new();
    let mut current_phase: Option<(String, String)> = None;

    for event in events {
        match &event.entry {
            WorkflowEntry::Phase {
                title,
                state,
                phase_id,
            } => {
                let key = workflow_phase_key(title, phase_id.as_deref());
                current_phase = Some((key.clone(), title.clone()));
                if let Some(index) = phase_index.get(&key).copied() {
                    phases[index].state = state.clone();
                } else {
                    phase_index.insert(key, phases.len());
                    phases.push(WorkflowPhaseGroup {
                        title: title.clone(),
                        state: state.clone(),
                        agents: Vec::new(),
                    });
                }
            }
            WorkflowEntry::Agent {
                index,
                state,
                phase_title,
                phase_id,
                label,
                tokens,
                tool_calls,
                tool_call_details,
                duration_ms,
                error,
                agent_id,
            } => {
                let title = phase_title
                    .clone()
                    .or_else(|| current_phase.as_ref().map(|(_, title)| title.clone()))
                    .unwrap_or_else(|| "Workflow".to_string());
                let phase_key = phase_id
                    .as_deref()
                    .map(|phase_id| workflow_phase_key(&title, Some(phase_id)))
                    .or_else(|| {
                        current_phase
                            .as_ref()
                            .filter(|(_, current_title)| current_title == &title)
                            .map(|(key, _)| key.clone())
                    })
                    .unwrap_or_else(|| workflow_phase_key(&title, None));
                let phase_pos = if let Some(index) = phase_index.get(&phase_key).copied() {
                    index
                } else {
                    phase_index.insert(phase_key, phases.len());
                    phases.push(WorkflowPhaseGroup {
                        title: title.clone(),
                        state: "start".to_string(),
                        agents: Vec::new(),
                    });
                    phases.len() - 1
                };
                let mut row = WorkflowAgentRow {
                    index: *index,
                    state: state.clone(),
                    label: label.clone(),
                    tokens: *tokens,
                    tool_calls: *tool_calls,
                    tool_call_details: tool_call_details.clone(),
                    duration_ms: *duration_ms,
                    error: error.clone(),
                    agent_id: agent_id.clone(),
                };

                if let Some((existing_phase, existing_agent)) = agent_id
                    .as_ref()
                    .and_then(|agent_id| agents_by_id.get(agent_id).copied())
                {
                    phases[existing_phase].agents[existing_agent] = row;
                    continue;
                }

                let existing_agent = phases[phase_pos].agents.iter().position(|agent| {
                    (agent.index > 0 && agent.index == row.index) || agent.label == row.label
                });
                let agent_pos = if let Some(agent_pos) = existing_agent {
                    if row.agent_id.is_none() {
                        row.agent_id = phases[phase_pos].agents[agent_pos].agent_id.clone();
                    }
                    phases[phase_pos].agents[agent_pos] = row;
                    agent_pos
                } else {
                    phases[phase_pos].agents.push(row);
                    phases[phase_pos].agents.len() - 1
                };
                if let Some(agent_id) = phases[phase_pos].agents[agent_pos].agent_id.as_ref() {
                    agents_by_id.insert(agent_id.clone(), (phase_pos, agent_pos));
                }
            }
            WorkflowEntry::Log { message } => logs.push(message.clone()),
        }
    }

    for phase in &mut phases {
        phase.agents.sort_by_key(|agent| agent.index);
    }
    let agent_count = phases.iter().map(|phase| phase.agents.len()).sum();
    WorkflowTree {
        phases,
        logs,
        agent_count,
    }
}

/// Hard cap on the left (phase) column so a pathological phase title
/// cannot push every agent cell past the wrap margin; longer titles
/// are truncated with `…`.
const WORKFLOW_PHASE_COLUMN_MAX_WIDTH: usize = 28;
/// The rule under `Agents` tracks the widest agent cell but stops here
/// so the separator line itself cannot wrap on narrow terminals.
const WORKFLOW_AGENTS_RULE_MAX_WIDTH: usize = 48;

/// Render the phase/agent tree as a two-column table — phases on the
/// left, their agents on the right. A phase's first agent shares its
/// row; further agents continue on rows with an empty phase cell.
/// This runs on every frame while a workflow card is visible, so it
/// materializes per-agent tool detail rows only at Verbose.
fn workflow_table_lines(tree: &WorkflowTree, verbosity: ToolOutputVerbosity) -> Vec<String> {
    if tree.phases.is_empty() {
        return Vec::new();
    }
    let include_details = matches!(verbosity, ToolOutputVerbosity::Verbose);
    let phase_state_width = tree
        .phases
        .iter()
        .map(|phase| workflow_state_label(&phase.state).len())
        .max()
        .unwrap_or(0);
    let agent_state_width = tree
        .phases
        .iter()
        .flat_map(|phase| phase.agents.iter())
        .map(|agent| workflow_state_label(&agent.state).len())
        .max()
        .unwrap_or(0);

    let rows: Vec<(String, Vec<String>)> = tree
        .phases
        .iter()
        .map(|phase| {
            let phase_cell = format!(
                "{:<phase_state_width$} {}",
                workflow_state_label(&phase.state),
                phase.title
            );
            let mut agent_cells = Vec::new();
            for agent in &phase.agents {
                agent_cells.push(workflow_agent_cell(agent, agent_state_width));
                if include_details {
                    agent_cells.extend(
                        workflow_agent_tool_detail_lines(agent)
                            .into_iter()
                            .map(|line| format!("  {line}")),
                    );
                }
            }
            if agent_cells.is_empty() {
                agent_cells.push("—".to_string());
            }
            (phase_cell, agent_cells)
        })
        .collect();

    let phase_width = rows
        .iter()
        .map(|(cell, _)| cell.width())
        .chain(std::iter::once("Phase".len()))
        .max()
        .unwrap_or(0)
        .min(WORKFLOW_PHASE_COLUMN_MAX_WIDTH);
    let rule_width = rows
        .iter()
        .flat_map(|(_, cells)| cells.iter())
        .map(|cell| cell.width())
        .chain(std::iter::once("Agents".len()))
        .max()
        .unwrap_or(0)
        .min(WORKFLOW_AGENTS_RULE_MAX_WIDTH);

    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(format!("   {} │ Agents", pad_cell("Phase", phase_width)));
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
            lines.push(format!("   {} │ {agent_cell}", pad_cell(left, phase_width)));
        }
    }
    lines
}

/// One agent cell: `state label · duration · tools · tokens · error`,
/// with the state padded so labels line up down the column.
fn workflow_agent_cell(agent: &WorkflowAgentRow, state_width: usize) -> String {
    let mut parts = vec![format!(
        "{:<state_width$} {}",
        workflow_state_label(&agent.state),
        agent.label
    )];
    if let Some(duration) = agent.duration_ms.filter(|duration| *duration > 0) {
        parts.push(format_duration_ms(duration));
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
        parts.push(format_token_count(agent.tokens));
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

/// Pad `text` with spaces to exactly `width` display columns,
/// truncating with `…` when it does not fit.
fn pad_cell(text: &str, width: usize) -> String {
    let mut cell = if text.width() > width {
        let mut truncated = String::new();
        let mut used = 0;
        for ch in text.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if used + ch_width > width.saturating_sub(1) {
                break;
            }
            truncated.push(ch);
            used += ch_width;
        }
        truncated.push('…');
        truncated
    } else {
        text.to_string()
    };
    cell.push_str(&" ".repeat(width.saturating_sub(cell.width())));
    cell
}

fn workflow_agent_tool_detail_lines(agent: &WorkflowAgentRow) -> Vec<String> {
    agent
        .tool_call_details
        .iter()
        .map(workflow_agent_tool_call_line)
        .filter(|line| !line.trim().is_empty())
        .collect()
}

fn workflow_agent_tool_call_line(call: &Value) -> String {
    let Some(call) = call.as_object() else {
        return format!("- {}", compact_value(call));
    };
    let name = call.get("name").and_then(Value::as_str).unwrap_or("tool");
    let summary = call
        .get("input")
        .or_else(|| call.get("raw_input"))
        .or_else(|| call.get("rawInput"))
        .filter(|value| !value_is_empty(value))
        .map(compact_value);
    let mut line = summary
        .filter(|summary| !summary.trim().is_empty())
        .map(|summary| format!("- {name}({summary})"))
        .unwrap_or_else(|| format!("- {name}"));
    if !call.get("ok").and_then(Value::as_bool).unwrap_or(true) {
        line.push_str(" [failed]");
    }
    line
}

fn value_is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

fn workflow_run_summary(
    raw_output: Option<&HashMap<String, Value>>,
    status: &str,
    tree: &WorkflowTree,
) -> String {
    let run_id = raw_output
        .and_then(|raw_output| workflow_progress_object(Some(raw_output)))
        .and_then(|progress| string_field(progress, "runId"))
        .or_else(|| raw_output.and_then(|raw_output| hash_string_field(raw_output, "runId")));
    let mut parts = Vec::new();
    if let Some(run_id) = run_id {
        parts.push(format!("run {run_id}"));
    }
    parts.push(status.to_string());
    parts.push(format!("{} phases", tree.phases.len()));
    parts.push(agent_count_label(tree.agent_count));
    format!("   {}", parts.join(" · "))
}

/// Build a body from the terminal `WorkflowLaunchStatus` schema when no
/// streaming progress entries are available. Reads only fields that survive
/// into the persisted result (`status`, `runId`, `agentCount`, `phases`,
/// `summary`, `error`, `result.logs`) so old transcripts still render.
fn workflow_fallback_lines(
    raw_output: Option<&HashMap<String, Value>>,
    verbosity: ToolOutputVerbosity,
) -> Vec<String> {
    let Some(raw_output) = raw_output else {
        return Vec::new();
    };

    let mut lines = Vec::new();

    let mut summary_parts = Vec::new();
    if let Some(run_id) = hash_string_field(raw_output, "runId") {
        summary_parts.push(format!("run {run_id}"));
    }
    if let Some(status) = workflow_status(Some(raw_output)) {
        summary_parts.push(status.to_string());
    }
    let phase_titles = workflow_fallback_phase_titles(raw_output);
    if !phase_titles.is_empty() {
        summary_parts.push(format!("{} phases", phase_titles.len()));
    }
    if let Some(agent_count) = workflow_fallback_agent_count(raw_output) {
        summary_parts.push(agent_count_label(agent_count));
    }
    if !summary_parts.is_empty() {
        lines.push(format!("   {}", summary_parts.join(" · ")));
    }

    for title in phase_titles {
        lines.push(format!("   • {title}"));
    }

    if matches!(
        verbosity,
        ToolOutputVerbosity::Normal | ToolOutputVerbosity::Verbose
    ) {
        if let Some(summary) = hash_string_field(raw_output, "summary") {
            lines.push(format!("   {summary}"));
        }
    }

    if matches!(verbosity, ToolOutputVerbosity::Verbose) {
        for log in workflow_fallback_logs(raw_output) {
            lines.push(format!("   • {log}"));
        }
    }

    if let Some(error) = hash_string_field(raw_output, "error") {
        lines.push(format!("   error: {error}"));
    }

    lines
}

fn workflow_fallback_agent_count(raw_output: &HashMap<String, Value>) -> Option<usize> {
    raw_output
        .get("agentCount")
        .and_then(Value::as_u64)
        .or_else(|| {
            raw_output
                .get("result")
                .and_then(Value::as_object)
                .and_then(|result| result.get("agentCount"))
                .and_then(Value::as_u64)
        })
        .map(|count| count as usize)
}

fn workflow_fallback_phase_titles(raw_output: &HashMap<String, Value>) -> Vec<String> {
    raw_output
        .get("phases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|phase| match phase {
            Value::String(title) => {
                let title = title.trim();
                (!title.is_empty()).then(|| title.to_string())
            }
            Value::Object(map) => string_field(map, "title"),
            _ => None,
        })
        .collect()
}

fn workflow_fallback_logs(raw_output: &HashMap<String, Value>) -> Vec<String> {
    raw_output
        .get("result")
        .and_then(Value::as_object)
        .and_then(|result| result.get("logs"))
        .or_else(|| raw_output.get("logs"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|log| !log.is_empty())
        .map(str::to_string)
        .collect()
}

fn workflow_verbose_lines(
    tool: &crate::streaming::StreamingToolUse,
    raw_output: Option<&HashMap<String, Value>>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut seen = HashSet::new();
    for (label, value) in [
        (
            "Run id",
            raw_output
                .and_then(|raw_output| workflow_progress_object(Some(raw_output)))
                .and_then(|progress| string_field(progress, "runId"))
                .or_else(|| {
                    raw_output.and_then(|raw_output| hash_string_field(raw_output, "runId"))
                }),
        ),
        (
            "Output",
            raw_output.and_then(|raw_output| {
                hash_string_field(raw_output, "transcriptDir")
                    .or_else(|| hash_string_field(raw_output, "outputPath"))
            }),
        ),
        (
            "Script",
            raw_output
                .and_then(|raw_output| hash_string_field(raw_output, "scriptPath"))
                .or_else(|| {
                    tool.raw_input
                        .as_ref()
                        .and_then(|input| hash_string_field(input, "scriptPath"))
                }),
        ),
        (
            "Result",
            raw_output.and_then(|raw_output| raw_output.get("result").map(compact_value)),
        ),
        (
            "Error",
            raw_output.and_then(|raw_output| hash_string_field(raw_output, "error")),
        ),
    ] {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            let key = format!("{label}:{value}");
            if seen.insert(key) {
                lines.push(format!("   {label}: {value}"));
            }
        }
    }
    lines
}

fn workflow_status(raw_output: Option<&HashMap<String, Value>>) -> Option<&str> {
    raw_output?
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| {
            raw_output?
                .get("workflowProgress")
                .and_then(Value::as_object)?
                .get("status")
                .and_then(Value::as_str)
        })
}

fn workflow_state_label(state: &str) -> &str {
    match state {
        "completed" | "done" => "done",
        "error" | "failed" => "failed",
        "start" | "started" | "running" => "running",
        other => other,
    }
}

fn agent_count_label(count: usize) -> String {
    format!("{count} {}", if count == 1 { "agent" } else { "agents" })
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000 {
        format!("{:.1}k tokens", tokens as f64 / 1_000.0)
    } else {
        format!("{tokens} tokens")
    }
}

pub fn format_duration_ms(duration_ms: u64) -> String {
    if duration_ms >= 1000 {
        let whole = duration_ms / 1000;
        let tenths = (duration_ms % 1000) / 100;
        if whole >= 10 || tenths == 0 {
            format!("{whole}s")
        } else {
            format!("{whole}.{tenths}s")
        }
    } else {
        format!("{duration_ms}ms")
    }
}

fn string_field(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn hash_string_field(map: &HashMap<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn compact_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(map) => compact_json_map(
            &map.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<HashMap<_, _>>(),
        ),
        _ => value.to_string(),
    }
}

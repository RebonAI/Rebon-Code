//! How a local workflow run's progress becomes the JSON its readers see.
//!
//! Three readers, one projection. The live overlay reads a per-entry
//! `workflow_progress` payload as it streams; the terminal tool result
//! carries the whole `workflowProgress` object so a replayed transcript
//! still draws the phase/agent tree; and the background task bridge
//! projects the same object into a task snapshot.
//!
//! Here rather than with the runtime that produces the entries: the entries
//! are [`WorkflowProgressEntry`] values held in a [`TaskRegistry`], the
//! runtime lives in `rebon-plugin-workflow`, and the front end's background
//! task bridge — which may not depend on a plugin it does not host — needs
//! [`workflow_progress_preview_value`]. This crate is the one both sides
//! already depend on, and it owns the data being projected.
//!
//! Everything here bounds its output. A progress entry carries model-written
//! text and tool inputs, and three of these payloads end up inside a JSON
//! blob with a size ceiling — so the bound is counted in *encoded* characters
//! ([`bounded_workflow_text`]), not in `char`s, or an escape-heavy string
//! would slip past it.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::runtime::{LocalWorkflowData, TaskData, TaskId, TaskRegistry, WorkflowProgressEntry};

/// Per-kind ceilings on how much of a run's history a preview repeats.
///
/// A preview keeps the *latest* state of each agent and phase, so these bound
/// how many distinct agents and phases survive, not how many events did.
const WORKFLOW_PROGRESS_PREVIEW_MAX_AGENTS: usize = 48;
const WORKFLOW_PROGRESS_PREVIEW_MAX_PHASES: usize = 32;
const WORKFLOW_PROGRESS_PREVIEW_MAX_LOGS: usize = 8;

pub struct WorkflowProgressMetadata {
    pub run_id: String,
    pub workflow_name: String,
    pub summary: Option<String>,
}

pub fn workflow_progress_metadata(
    registry: &TaskRegistry,
    id: &TaskId,
) -> Option<WorkflowProgressMetadata> {
    registry.snapshot(id).and_then(|snap| match snap.data {
        TaskData::LocalWorkflow(data) => Some(WorkflowProgressMetadata {
            run_id: data.run_id,
            workflow_name: data.workflow_name,
            summary: data.summary,
        }),
        _ => None,
    })
}

pub fn bounded_workflow_text(value: &str, max_json_chars: usize) -> String {
    let mut chars = value.chars().peekable();
    let mut text = String::new();
    let mut encoded_chars = 0;
    while let Some(ch) = chars.next() {
        let escaped_width = match ch {
            '"' | '\\' | '\u{0008}' | '\u{000c}' | '\n' | '\r' | '\t' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => 1,
        };
        let limit = if chars.peek().is_some() {
            max_json_chars.saturating_sub(1)
        } else {
            max_json_chars
        };
        if encoded_chars + escaped_width > limit {
            if encoded_chars < max_json_chars {
                text.push('…');
            }
            break;
        }
        text.push(ch);
        encoded_chars += escaped_width;
    }
    text
}

pub fn bounded_workflow_tool_call_details(details: &[Value]) -> Vec<Value> {
    const MAX_DETAILS: usize = 8;
    const MAX_DETAIL_CHARS: usize = 512;

    details
        .iter()
        .take(MAX_DETAILS)
        .map(|detail| {
            let encoded = detail.to_string();
            if encoded.chars().count() <= MAX_DETAIL_CHARS {
                return detail.clone();
            }
            let Some(object) = detail.as_object() else {
                return json!({
                    "summary": bounded_workflow_text(&encoded, MAX_DETAIL_CHARS),
                    "truncated": true,
                });
            };
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .map(|name| bounded_workflow_text(name, 64))
                .unwrap_or_else(|| "tool".to_string());
            let input = object
                .get("input")
                .or_else(|| object.get("raw_input"))
                .or_else(|| object.get("rawInput"))
                .map(|input| bounded_workflow_text(&input.to_string(), 320));
            json!({
                "name": name,
                "input": input,
                "ok": object.get("ok").and_then(Value::as_bool).unwrap_or(true),
                "truncated": true,
            })
        })
        .collect()
}

pub fn workflow_progress_entry_payload(entry: &WorkflowProgressEntry) -> Value {
    match entry {
        WorkflowProgressEntry::Agent {
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
            let mut payload = json!({
                "type": "agent",
                "index": index,
                "state": bounded_workflow_text(state, 32),
                "phaseTitle": phase_title
                    .as_deref()
                    .map(|title| bounded_workflow_text(title, 96)),
                "label": bounded_workflow_text(label, 128),
                "tokens": tokens,
                "toolCalls": tool_calls,
                "toolCallDetails": bounded_workflow_tool_call_details(tool_call_details),
                "durationMs": duration_ms,
                "error": error
                    .as_deref()
                    .map(|error| bounded_workflow_text(error, 192)),
            });
            if let Some(phase_id) = phase_id {
                payload["phaseInstanceId"] = Value::String(bounded_workflow_text(phase_id, 96));
            }
            if let Some(agent_id) = agent_id {
                payload["agentId"] = Value::String(bounded_workflow_text(agent_id, 96));
            }
            payload
        }
        WorkflowProgressEntry::Phase {
            title,
            state,
            phase_id,
        } => {
            let mut payload = json!({
                "type": "phase",
                "title": bounded_workflow_text(title, 96),
                "state": bounded_workflow_text(state, 32),
            });
            if let Some(phase_id) = phase_id {
                payload["phaseInstanceId"] = Value::String(bounded_workflow_text(phase_id, 96));
            }
            payload
        }
        WorkflowProgressEntry::Log { message } => json!({
            "type": "log",
            "message": bounded_workflow_text(message, 256),
        }),
    }
}

pub fn workflow_progress_payload(
    entry: &WorkflowProgressEntry,
    sequence: u64,
    metadata: Option<&WorkflowProgressMetadata>,
) -> Value {
    let (run_id, workflow_name, summary) = metadata
        .map(|metadata| {
            (
                Some(bounded_workflow_text(&metadata.run_id, 96)),
                Some(bounded_workflow_text(&metadata.workflow_name, 192)),
                metadata
                    .summary
                    .as_deref()
                    .map(|summary| bounded_workflow_text(summary, 384)),
            )
        })
        .unwrap_or((None, None, None));
    json!({
        "type": "workflow_progress",
        "runId": run_id,
        "workflowName": workflow_name,
        "summary": summary,
        "sequence": sequence,
        "entry": workflow_progress_entry_payload(entry),
    })
}

/// Build the render-facing `workflowProgress` object (the
/// `runId`/`workflowName`/`summary`/`entries` shape the TUI reads) from a
/// run's accumulated progress entries.
///
/// This is the same payload the `status: "running"` query returns; it is
/// also attached to the *terminal* `WorkflowLaunchStatus` so the workflow
/// card keeps its phase/agent tree after the turn commits. The live
/// streaming overlay accumulates these entries from delta events, but that
/// overlay is discarded on commit — transcript replay only sees the
/// persisted tool result, so the entries must live there too.
pub fn workflow_progress_object_value(
    run_id: &str,
    workflow_name: &str,
    summary: Option<&str>,
    entries: &[WorkflowProgressEntry],
) -> Value {
    workflow_progress_preview_from_parts(run_id, workflow_name, summary, entries)
}

pub fn workflow_progress_preview_value(data: &LocalWorkflowData) -> Value {
    workflow_progress_preview_from_parts(
        &data.run_id,
        &data.workflow_name,
        data.summary.as_deref(),
        &data.progress_entries,
    )
}

fn workflow_progress_preview_from_parts(
    run_id: &str,
    workflow_name: &str,
    summary: Option<&str>,
    progress_entries: &[WorkflowProgressEntry],
) -> Value {
    fn agent_key(entry: &WorkflowProgressEntry) -> Option<String> {
        let WorkflowProgressEntry::Agent {
            index,
            phase_title,
            phase_id,
            label,
            agent_id,
            ..
        } = entry
        else {
            return None;
        };
        Some(agent_id.clone().unwrap_or_else(|| {
            format!(
                "{}\0{}\0{}",
                phase_id.as_deref().unwrap_or_default(),
                phase_title.as_deref().unwrap_or_default(),
                if *index > 0 {
                    index.to_string()
                } else {
                    label.clone()
                }
            )
        }))
    }

    fn phase_key(entry: &WorkflowProgressEntry) -> Option<String> {
        let WorkflowProgressEntry::Phase {
            title, phase_id, ..
        } = entry
        else {
            return None;
        };
        Some(phase_id.clone().unwrap_or_else(|| format!("title:{title}")))
    }

    let mut latest_agents = HashMap::new();
    let mut latest_phases = HashMap::new();
    for (position, entry) in progress_entries.iter().enumerate() {
        if let Some(key) = agent_key(entry) {
            if latest_agents.contains_key(&key)
                || latest_agents.len() < WORKFLOW_PROGRESS_PREVIEW_MAX_AGENTS
            {
                latest_agents.insert(key, position);
            }
        }
        if let Some(key) = phase_key(entry) {
            if latest_phases.contains_key(&key)
                || latest_phases.len() < WORKFLOW_PROGRESS_PREVIEW_MAX_PHASES
            {
                latest_phases.insert(key, position);
            }
        }
    }

    let mut log_events = 0;
    let entries: Vec<_> = progress_entries
        .iter()
        .enumerate()
        .filter_map(|(position, entry)| match entry {
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
            } if agent_key(entry)
                .as_ref()
                .and_then(|key| latest_agents.get(key))
                == Some(&position) =>
            {
                Some(WorkflowProgressEntry::Agent {
                    index: *index,
                    state: state.clone(),
                    phase_title: phase_title
                        .as_deref()
                        .map(|title| bounded_workflow_text(title, 96)),
                    phase_id: phase_id
                        .as_deref()
                        .map(|phase_id| bounded_workflow_text(phase_id, 96)),
                    label: bounded_workflow_text(label, 128),
                    tokens: *tokens,
                    tool_calls: *tool_calls,
                    tool_call_details: Vec::new(),
                    duration_ms: *duration_ms,
                    error: error
                        .as_deref()
                        .map(|error| bounded_workflow_text(error, 192)),
                    agent_id: agent_id
                        .as_deref()
                        .map(|agent_id| bounded_workflow_text(agent_id, 96)),
                })
            }
            WorkflowProgressEntry::Phase {
                title,
                state,
                phase_id,
            } if phase_key(entry)
                .as_ref()
                .and_then(|key| latest_phases.get(key))
                == Some(&position) =>
            {
                Some(WorkflowProgressEntry::Phase {
                    title: bounded_workflow_text(title, 96),
                    state: bounded_workflow_text(state, 32),
                    phase_id: phase_id
                        .as_deref()
                        .map(|phase_id| bounded_workflow_text(phase_id, 96)),
                })
            }
            WorkflowProgressEntry::Log { message }
                if log_events < WORKFLOW_PROGRESS_PREVIEW_MAX_LOGS =>
            {
                log_events += 1;
                Some(WorkflowProgressEntry::Log {
                    message: bounded_workflow_text(message, 256),
                })
            }
            _ => None,
        })
        .collect();
    let run_id = bounded_workflow_text(run_id, 96);
    let workflow_name = bounded_workflow_text(workflow_name, 192);
    let summary = summary.map(|summary| bounded_workflow_text(summary, 384));

    let entries: Vec<Value> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            json!({
                "sequence": index as u64 + 1,
                "entry": workflow_progress_entry_payload(entry),
            })
        })
        .collect();
    json!({
        "runId": run_id,
        "workflowName": workflow_name,
        "summary": summary,
        "entries": entries,
    })
}

/// Read the accumulated `workflowProgress` for a task from the registry,
/// if it is a local workflow task. Used to enrich the terminal launch
/// status so the TUI workflow card survives transcript replay.
pub fn workflow_progress_for_task(registry: &TaskRegistry, id: &TaskId) -> Option<Value> {
    let snapshot = registry.snapshot(id)?;
    let TaskData::LocalWorkflow(data) = &snapshot.data else {
        return None;
    };
    Some(workflow_progress_object_value(
        &data.run_id,
        &data.workflow_name,
        data.summary.as_deref(),
        &data.progress_entries,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_progress_payload_contains_structured_entry_and_metadata() {
        let entry = WorkflowProgressEntry::Agent {
            index: 2,
            state: "completed".into(),
            phase_title: Some("design".into()),
            phase_id: Some("phase-2".into()),
            label: "ui-design".into(),
            tokens: 42,
            tool_calls: 1,
            tool_call_details: vec![json!({ "name": "Read", "ok": true })],
            duration_ms: Some(1200),
            error: None,
            agent_id: Some("agent-abc123".into()),
        };
        let metadata = WorkflowProgressMetadata {
            run_id: "wf_test".into(),
            workflow_name: "markdown-previewer".into(),
            summary: Some("Preview markdown".into()),
        };
        let payload = workflow_progress_payload(&entry, 7, Some(&metadata));

        assert_eq!(payload["type"], "workflow_progress");
        assert_eq!(payload["runId"], "wf_test");
        assert_eq!(payload["workflowName"], "markdown-previewer");
        assert_eq!(payload["summary"], "Preview markdown");
        assert_eq!(payload["sequence"], 7);
        assert_eq!(payload["entry"]["type"], "agent");
        assert_eq!(payload["entry"]["phaseTitle"], "design");
        assert_eq!(payload["entry"]["phaseInstanceId"], "phase-2");
        assert_eq!(payload["entry"]["label"], "ui-design");
        assert_eq!(payload["entry"]["durationMs"], 1200);
        assert_eq!(payload["entry"]["toolCallDetails"][0]["name"], "Read");
        assert_eq!(payload["entry"]["agentId"], "agent-abc123");
    }

    #[test]
    fn workflow_progress_payload_omits_agent_id_when_unknown() {
        let entry = WorkflowProgressEntry::Agent {
            index: 1,
            state: "start".into(),
            phase_title: None,
            phase_id: None,
            label: "finder".into(),
            tokens: 0,
            tool_calls: 0,
            tool_call_details: Vec::new(),
            duration_ms: None,
            error: None,
            agent_id: None,
        };
        let payload = workflow_progress_payload(&entry, 1, None);
        assert!(payload["entry"].get("agentId").is_none());
        assert!(payload["entry"].get("phaseInstanceId").is_none());
    }

    #[test]
    fn workflow_progress_payload_carries_phase_instance_id() {
        let entry = WorkflowProgressEntry::Phase {
            title: "Review".into(),
            state: "completed".into(),
            phase_id: Some("phase-2".into()),
        };

        let payload = workflow_progress_payload(&entry, 4, None);

        assert_eq!(payload["entry"]["type"], "phase");
        assert_eq!(payload["entry"]["phaseInstanceId"], "phase-2");
    }

    #[test]
    fn workflow_progress_payload_bounds_live_text_and_tool_details() {
        let long = "\"\n\\payload".repeat(1_000);
        let entry = WorkflowProgressEntry::Agent {
            index: 1,
            state: long.clone(),
            phase_title: Some(long.clone()),
            phase_id: Some(long.clone()),
            label: long.clone(),
            tokens: 1,
            tool_calls: 12,
            tool_call_details: (0..12)
                .map(|_| {
                    json!({
                        "name": long.clone(),
                        "input": { "value": long.clone() },
                        "ok": false,
                    })
                })
                .collect(),
            duration_ms: Some(1),
            error: Some(long.clone()),
            agent_id: Some(long.clone()),
        };
        let metadata = WorkflowProgressMetadata {
            run_id: long.clone(),
            workflow_name: long.clone(),
            summary: Some(long),
        };

        let payload = workflow_progress_payload(&entry, 1, Some(&metadata));
        let assert_string_budget = |value: &Value, max_json_chars: usize| {
            assert!(
                serde_json::to_string(value)
                    .expect("encode bounded workflow string")
                    .chars()
                    .count()
                    <= max_json_chars + 2
            );
        };

        assert_string_budget(&payload["runId"], 96);
        assert_string_budget(&payload["workflowName"], 192);
        assert_string_budget(&payload["summary"], 384);
        assert_string_budget(&payload["entry"]["state"], 32);
        assert_string_budget(&payload["entry"]["phaseTitle"], 96);
        assert_string_budget(&payload["entry"]["phaseInstanceId"], 96);
        assert_string_budget(&payload["entry"]["label"], 128);
        assert_string_budget(&payload["entry"]["error"], 192);
        assert_string_budget(&payload["entry"]["agentId"], 96);
        let details = payload["entry"]["toolCallDetails"]
            .as_array()
            .expect("bounded tool details");
        assert_eq!(details.len(), 8);
        assert!(details.iter().all(|detail| {
            detail["truncated"] == true && detail.to_string().chars().count() <= 512
        }));
    }

    #[test]
    fn workflow_progress_preview_stays_structured_within_terminal_budget() {
        let long = "\"\\\n".repeat(1_000);
        let mut progress_entries = Vec::new();
        for index in 1..=60 {
            let phase_id = format!("phase-{index}-{long}");
            progress_entries.push(WorkflowProgressEntry::Phase {
                title: long.clone(),
                state: "start".into(),
                phase_id: Some(phase_id.clone()),
            });
            progress_entries.push(WorkflowProgressEntry::Agent {
                index,
                state: "start".into(),
                phase_title: Some(long.clone()),
                phase_id: Some(phase_id.clone()),
                label: long.clone(),
                tokens: 0,
                tool_calls: 1,
                tool_call_details: vec![json!({ "name": "Read", "input": long.clone() })],
                duration_ms: None,
                error: None,
                agent_id: Some(format!("agent-{index}-{long}")),
            });
            progress_entries.push(WorkflowProgressEntry::Agent {
                index,
                state: "completed".into(),
                phase_title: Some(long.clone()),
                phase_id: Some(phase_id.clone()),
                label: long.clone(),
                tokens: 1,
                tool_calls: 1,
                tool_call_details: vec![json!({ "name": "Read", "input": long.clone() })],
                duration_ms: Some(1),
                error: Some(long.clone()),
                agent_id: Some(format!("agent-{index}-{long}")),
            });
            progress_entries.push(WorkflowProgressEntry::Phase {
                title: long.clone(),
                state: "completed".into(),
                phase_id: Some(phase_id),
            });
        }
        progress_entries.extend((0..20).map(|_| WorkflowProgressEntry::Log {
            message: long.clone(),
        }));
        let data = LocalWorkflowData {
            run_id: long.clone(),
            workflow_name: long.clone(),
            summary: Some(long.clone()),
            agent_count: 60,
            progress_entries,
            token_count: 60,
            tool_use_count: 60,
            output_path: None,
            script_path: None,
            args: None,
        };

        let preview = workflow_progress_preview_value(&data);
        assert!(preview.is_object());
        assert!(preview.to_string().chars().count() <= 65_536);
        let entries = preview["entries"].as_array().expect("structured entries");
        assert_eq!(entries.len(), 88);
        assert_eq!(
            entries
                .iter()
                .filter(|event| event["entry"]["type"] == "agent")
                .count(),
            WORKFLOW_PROGRESS_PREVIEW_MAX_AGENTS
        );
        assert!(entries
            .iter()
            .filter(|event| event["entry"]["type"] == "agent")
            .all(|event| {
                event["entry"]["state"] == "completed"
                    && event["entry"]["toolCallDetails"] == json!([])
            }));
    }
}

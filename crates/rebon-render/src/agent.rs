//! Agent (sub-agent) tool-call body rendering.
//!
//! Builds the Agent card body every surface shares: the prompt, the sub-agent's
//! tool calls, its response, and its status line, shaped
//! `{status} ({N tools used · K tokens})` — `Done (3 tools used · 1.2k tokens)`
//! for a finished sub-agent. Pure —
//! serde_json plus the in-crate summary/content/text/json_compact helpers, with
//! no IO and no tool registry.

use std::collections::HashMap;

use rebon_types::ToolCallContent;
use serde_json::Value;

use crate::content::render_tool_call_content;
use crate::json_compact::{compact_json_value, is_default_json_value};
use crate::summary::streaming_tool_summary;
use crate::text::expand_tabs_for_tui;

pub fn agent_description(input: Option<&HashMap<String, Value>>) -> Option<String> {
    agent_string_field(input, &["description", "name"])
        .or_else(|| agent_string_field(input, &["prompt"]).map(first_agent_prompt_line))
}

pub fn agent_full_detail_lines(
    input: Option<&HashMap<String, Value>>,
    output: Option<&HashMap<String, Value>>,
    content: Option<&[ToolCallContent]>,
) -> Vec<String> {
    let mut lines = Vec::new();
    append_agent_prompt_lines(&mut lines, input);

    if let Some(output) = output.filter(|output| is_background_teammate_result(output)) {
        if let Some(status) = agent_background_result_line(input, output) {
            lines.push(status);
            return lines;
        }
    }

    let rendered_tool_calls = output
        .map(|output| append_agent_tool_call_lines(&mut lines, output))
        .unwrap_or(false);
    if !rendered_tool_calls
        && !output.is_some_and(|output| structured_agent_result_status(output).is_some())
    {
        append_agent_content_tool_lines(&mut lines, content);
    }

    if let Some(output) = output {
        append_agent_response_lines(&mut lines, output);
        append_agent_done_line(&mut lines, output);
    }
    lines
}

/// Whether an Agent result represents work that continues outside this tool call.
pub fn is_background_agent_result(output: &HashMap<String, Value>) -> bool {
    matches!(
        structured_agent_result_status(output),
        Some("async_launched")
    ) || is_background_teammate_result(output)
}

fn is_background_teammate_result(output: &HashMap<String, Value>) -> bool {
    matches!(
        structured_agent_result_status(output),
        Some("teammate_spawned" | "teammate_dispatched")
    ) && !has_agent_handoff(output)
}

/// Human-readable lifecycle text for a background Agent result.
pub fn agent_background_result_line(
    input: Option<&HashMap<String, Value>>,
    output: &HashMap<String, Value>,
) -> Option<String> {
    if !is_background_agent_result(output) {
        return None;
    }
    let name = agent_string_field(input, &["name"])
        .or_else(|| agent_string_field(Some(output), &["display_name", "displayName"]));
    match structured_agent_result_status(output)? {
        "teammate_spawned" => Some(match name {
            Some(name) => format!("Teammate @{name} launched"),
            None => "Teammate launched".to_string(),
        }),
        "teammate_dispatched" => Some(match name {
            Some(name) => format!("Message sent to teammate @{name}"),
            None => "Message sent to teammate".to_string(),
        }),
        "async_launched" => Some("background agent launched".to_string()),
        _ => None,
    }
}

fn structured_agent_result_status(output: &HashMap<String, Value>) -> Option<&str> {
    output
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| {
            matches!(
                *status,
                "async_launched" | "teammate_spawned" | "teammate_dispatched"
            )
        })
}

fn has_agent_handoff(output: &HashMap<String, Value>) -> bool {
    output.get("handoff").is_some_and(|handoff| {
        handoff
            .as_object()
            .is_some_and(|handoff| !handoff.is_empty())
    })
}

/// Format a token count compactly: `1.2k tokens` / `840 tokens`.
pub fn format_agent_token_count(tokens: u64) -> String {
    if tokens >= 1_000 {
        format!("{:.1}k tokens", tokens as f64 / 1_000.0)
    } else {
        format!("{tokens} tokens")
    }
}

fn agent_string_field(input: Option<&HashMap<String, Value>>, keys: &[&str]) -> Option<String> {
    input
        .and_then(|input| keys.iter().find_map(|key| input.get(*key)))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| expand_tabs_for_tui(&text.replace('\r', "")))
}

fn first_agent_prompt_line(prompt: String) -> String {
    prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or(prompt.trim())
        .to_string()
}

fn append_agent_prompt_lines(lines: &mut Vec<String>, input: Option<&HashMap<String, Value>>) {
    let Some(prompt) = agent_string_field(input, &["prompt"]) else {
        return;
    };
    append_agent_block(lines, "Prompt", &prompt);
}

fn append_agent_response_lines(lines: &mut Vec<String>, output: &HashMap<String, Value>) {
    let response = agent_string_field(Some(output), &["final_text", "finalText"]).or_else(|| {
        output
            .get("handoff")
            .and_then(Value::as_object)
            .and_then(|handoff| handoff.get("summary"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| expand_tabs_for_tui(&text.replace('\r', "")))
    });
    let Some(response) = response else {
        return;
    };
    append_agent_block(lines, "Response", &response);
}

fn append_agent_block(lines: &mut Vec<String>, label: &str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    lines.push(format!("{label}:"));
    lines.extend(text.lines().map(|line| format!("  {line}")));
}

fn append_agent_content_tool_lines(
    lines: &mut Vec<String>,
    content: Option<&[ToolCallContent]>,
) -> bool {
    let Some(content) = content else {
        return false;
    };
    let mut rendered_any = false;
    for item in content {
        for line in render_tool_call_content(item).split('\n') {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            lines.push(line.to_string());
            rendered_any = true;
        }
    }
    rendered_any
}

fn append_agent_tool_call_lines(lines: &mut Vec<String>, output: &HashMap<String, Value>) -> bool {
    let Some(calls) = output
        .get("sub_agent_tool_calls")
        .or_else(|| output.get("subAgentToolCalls"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    if calls.is_empty() {
        return false;
    }
    for call in calls {
        lines.push(agent_tool_call_line(call));
    }
    true
}

fn agent_tool_call_line(call: &Value) -> String {
    let Some(call) = call.as_object() else {
        return compact_json_value(call);
    };
    let name = call.get("name").and_then(Value::as_str).unwrap_or("tool");
    let mut line = agent_tool_call_summary(name, call)
        .filter(|summary| !summary.trim().is_empty())
        .map(|summary| format!("{name}({summary})"))
        .unwrap_or_else(|| name.to_string());
    let ok = call.get("ok").and_then(Value::as_bool).unwrap_or(true);
    if !ok {
        line.push_str(" [failed]");
    }
    line
}

fn agent_tool_call_summary(name: &str, call: &serde_json::Map<String, Value>) -> Option<String> {
    let input = call
        .get("input")
        .or_else(|| call.get("raw_input"))
        .or_else(|| call.get("rawInput"))
        .filter(|value| !agent_detail_value_is_empty(value))?;
    if let Some(input) = input.as_object() {
        let input = json_object_to_hash_map(input);
        let summary = streaming_tool_summary(name, &input);
        if !summary.trim().is_empty() {
            return Some(summary);
        }
    }
    Some(agent_detail_value(input))
}

fn json_object_to_hash_map(map: &serde_json::Map<String, Value>) -> HashMap<String, Value> {
    map.iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn append_agent_done_line(lines: &mut Vec<String>, output: &HashMap<String, Value>) {
    let Some(status) = agent_status_label(output) else {
        return;
    };
    let mut parts = Vec::new();
    if let Some(count) = agent_tool_call_count(output) {
        parts.push(format!(
            "{count} {} used",
            if count == 1 { "tool" } else { "tools" }
        ));
    }
    if let Some(tokens) = agent_total_tokens(output) {
        parts.push(format_agent_token_count(tokens));
    }
    if parts.is_empty() && status == "Done" {
        return;
    }
    if parts.is_empty() {
        lines.push(status.to_string());
    } else {
        lines.push(format!("{status} ({})", parts.join(" · ")));
    }
}

fn agent_status_label(output: &HashMap<String, Value>) -> Option<&'static str> {
    match output.get("status").and_then(Value::as_str) {
        Some("failed") => Some("Failed"),
        Some("cancelled") | Some("canceled") => Some("Cancelled"),
        Some("async_launched") => Some("Launched"),
        Some("teammate_spawned" | "teammate_dispatched") => match output
            .get("handoff")
            .and_then(Value::as_object)
            .and_then(|handoff| handoff.get("status"))
            .and_then(Value::as_str)
        {
            Some("failed") => Some("Failed"),
            Some("interrupted" | "cancelled" | "canceled") => Some("Stopped"),
            _ => Some("Done"),
        },
        Some(_) => Some("Done"),
        None if agent_tool_call_count(output).is_some()
            || agent_total_tokens(output).is_some()
            || agent_string_field(Some(output), &["final_text", "finalText"]).is_some() =>
        {
            Some("Done")
        }
        None => None,
    }
}

fn agent_tool_call_count(output: &HashMap<String, Value>) -> Option<u64> {
    output
        .get("tool_call_count")
        .or_else(|| output.get("toolCallCount"))
        .and_then(Value::as_u64)
        .or_else(|| {
            output
                .get("sub_agent_tool_calls")
                .or_else(|| output.get("subAgentToolCalls"))
                .and_then(Value::as_array)
                .map(|calls| calls.len() as u64)
        })
}

fn agent_total_tokens(output: &HashMap<String, Value>) -> Option<u64> {
    output
        .get("total_tokens")
        .or_else(|| output.get("totalTokens"))
        .and_then(Value::as_u64)
        .or_else(|| {
            let usage = output.get("usage")?.as_object()?;
            let total = ["input_tokens", "output_tokens"]
                .iter()
                .filter_map(|key| usage.get(*key).and_then(Value::as_u64))
                .sum::<u64>();
            (total > 0).then_some(total)
        })
}

fn agent_detail_value_is_empty(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        Value::String(text) => text.trim().is_empty(),
        _ => is_default_json_value(value),
    }
}

fn agent_detail_value(value: &Value) -> String {
    match value {
        Value::String(text) => expand_tabs_for_tui(&text.replace('\r', "")),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string_pretty(value).expect("serde_json::Value always serializes")
        }
        other => compact_json_value(other),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn map(entries: &[(&str, Value)]) -> HashMap<String, Value> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn background_teammate_results_render_lifecycle_text_without_protocol_fields() {
        let input = map(&[
            ("name", json!("epub-native-audit")),
            ("prompt", json!("Audit native EPUB support")),
        ]);
        for (status, expected) in [
            ("teammate_spawned", "Teammate @epub-native-audit launched"),
            (
                "teammate_dispatched",
                "Message sent to teammate @epub-native-audit",
            ),
        ] {
            let output = map(&[
                ("status", json!(status)),
                ("handoff", Value::Null),
                ("agentId", json!("agent-secret")),
            ]);
            let lines = agent_full_detail_lines(Some(&input), Some(&output), None);
            assert!(lines.iter().any(|line| line == expected), "{lines:?}");
            assert!(!lines.join("\n").contains("handoff"));
            assert!(!lines.join("\n").contains("agent-secret"));
        }
    }

    #[test]
    fn foreground_teammate_handoff_renders_as_response() {
        let input = map(&[("prompt", json!("Audit native EPUB support"))]);
        let output = map(&[
            ("status", json!("teammate_spawned")),
            (
                "handoff",
                json!({
                    "status": "completed",
                    "summary": "Native parsing is complete",
                    "error": null
                }),
            ),
        ]);

        let lines = agent_full_detail_lines(Some(&input), Some(&output), None);
        assert!(lines.iter().any(|line| line == "Response:"));
        assert!(lines
            .iter()
            .any(|line| line == "  Native parsing is complete"));
        assert!(!lines.join("\n").contains("handoff="));
    }
}

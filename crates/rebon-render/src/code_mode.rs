//! `run_code` bodies: the program a Code Mode call ran.
//!
//! Read from the call's own input rather than from anything the tool reports
//! back. The program is already persisted there — it is what the model sent —
//! so it renders identically while the call is in flight, after it finishes,
//! and after the session is reopened tomorrow. A tool that reported its own
//! source would be paying for the same text twice, once in the request and
//! once in the result the model then has to read back.

use serde_json::Value;

/// The tool whose input is a program.
pub const RUN_CODE_TOOL_NAME: &str = "run_code";

/// The program, or nothing when this is not a Code Mode call.
///
/// Plain, not fenced. A tool body is already a monospace panel and does not
/// parse markdown, so a fence adds three backticks above the code and three
/// below — punctuation the reader has to look past to reach the thing they
/// opened the row for.
pub fn program_text(tool_name: &str, input: &Value) -> Option<String> {
    if tool_name != RUN_CODE_TOOL_NAME {
        return None;
    }
    let code = input.get("code").and_then(Value::as_str)?.trim_end();
    if code.is_empty() {
        return None;
    }
    Some(code.to_string())
}

/// Account for all observed nested calls in a completed sequence. Sequence ids
/// pair starts and finishes (including parallel, out-of-order completion), not
/// array positions or the text of a tool's progress message. A caught nested
/// failure must not turn into a claim that every call succeeded.
pub fn completed_summary(content: &[rebon_types::ToolCallContent]) -> String {
    let mut calls = std::collections::BTreeMap::new();
    for metadata in content
        .iter()
        .filter_map(crate::tool_output::tool_progress_metadata)
    {
        if !matches!(
            metadata.get("kind").and_then(Value::as_str),
            Some("code_mode/dispatch-start" | "code_mode/dispatch")
        ) {
            continue;
        }
        let Some(payload) = metadata.get("payload") else {
            continue;
        };
        let (Some(sequence), Some(tool)) = (
            payload.get("seq").and_then(Value::as_u64),
            payload.get("tool").and_then(Value::as_str),
        ) else {
            continue;
        };
        let failed = payload.get("isError").and_then(Value::as_bool) == Some(true);
        let entry = calls.entry(sequence).or_insert((tool, false));
        entry.1 |= failed;
    }
    let mut summary = "Completed".to_string();
    if calls.is_empty() {
        return summary;
    }
    let unit = if calls.len() == 1 { "call" } else { "calls" };
    summary.push_str(&format!(" · {} {unit}", calls.len()));
    let failed = calls.values().filter(|(_, failed)| *failed).count();
    if failed > 0 {
        summary.push_str(&format!(" · {failed} failed"));
    }
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for (tool, _) in calls.values() {
        if let Some((_, count)) = counts.iter_mut().find(|(name, _)| name == tool) {
            *count += 1;
        } else {
            counts.push((tool, 1));
        }
    }
    for (tool, count) in counts {
        summary.push_str(&format!(" · {tool} ×{count}"));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn completion_summary_counts_calls_not_progress_events_or_last_sequence_number() {
        let event = |kind, message, payload| {
            crate::tool_output::tool_progress_update_content(
                &rebon_tools_core::ToolProgressUpdate::new(kind)
                    .with_message(message)
                    .with_payload(payload),
            )
            .unwrap()
            .remove(0)
        };
        let content = vec![
            event(
                "code_mode/dispatch-start",
                "→ Bash (first)",
                json!({"seq": 0, "tool": "Bash"}),
            ),
            event(
                "code_mode/dispatch-start",
                "→ Read (parallel)",
                json!({"seq": 1, "tool": "Read"}),
            ),
            event(
                "code_mode/dispatch",
                "← Read failed: denied",
                json!({"seq": 1, "tool": "Read", "isError": true}),
            ),
            event(
                "code_mode/dispatch",
                "← Bash succeeded",
                json!({"seq": 0, "tool": "Bash", "isError": false}),
            ),
            event(
                "code_mode/dispatch",
                "← Bash succeeded",
                json!({"seq": 0, "tool": "Bash", "isError": false}),
            ),
            event(
                "other/progress",
                "not a nested dispatch",
                json!({"seq": 9, "tool": "Other"}),
            ),
        ];
        assert_eq!(
            completed_summary(&content),
            "Completed · 2 calls · 1 failed · Bash ×1 · Read ×1"
        );
        assert_eq!(completed_summary(&[]), "Completed");
        assert_eq!(
            completed_summary(&content[..1]),
            "Completed · 1 call · Bash ×1"
        );
    }

    #[test]
    fn the_program_comes_from_the_call_that_asked_for_it() {
        let input = json!({ "code": "return 1 + 1;\n\n", "description": "add" });
        assert_eq!(
            program_text(RUN_CODE_TOOL_NAME, &input).as_deref(),
            Some("return 1 + 1;"),
            "no fence: a tool body is a monospace panel, not markdown, so a \
             fence is three backticks the reader has to look past"
        );
    }

    #[test]
    fn nothing_to_show_is_nothing_rendered() {
        // Another tool's input is not a program, whatever it happens to hold.
        assert_eq!(program_text("Bash", &json!({ "code": "ls" })), None);
        // And a Code Mode call with no program is a malformed call, not an
        // empty code block.
        assert_eq!(program_text(RUN_CODE_TOOL_NAME, &json!({})), None);
        assert_eq!(
            program_text(RUN_CODE_TOOL_NAME, &json!({ "code": "   \n" })),
            None
        );
    }
}

use rebon_render::{format_sleep_duration_ms, plan_ledger_summary, PLAN_LEDGER_TOOL_NAME};
use serde_json::Value;

use super::{compact_json_value, is_default_json_value, truncate_param_value, SUMMARY_MAX_CHARS};

pub(super) fn compact_json_map_value_for_tool(
    tool_name: Option<&str>,
    map: &serde_json::Map<String, Value>,
) -> String {
    if tool_name == Some(PLAN_LEDGER_TOOL_NAME) {
        return plan_ledger_summary(
            map.get("operation").and_then(Value::as_str),
            map.get("items"),
            None,
        );
    }
    if tool_name == Some("InvokeDeferredTool") {
        let inner_tool = map.get("tool_name").and_then(Value::as_str).unwrap_or("");
        if let Some(inner_args) = map.get("arguments").and_then(Value::as_object) {
            return compact_json_map_value_for_tool(Some(inner_tool), inner_args);
        }
    }
    if tool_name == Some("WebSearch") {
        return map
            .get("query")
            .filter(|value| !is_default_json_value(value))
            .map(Value::to_string)
            .unwrap_or_default();
    }
    if tool_name == Some("Sleep") {
        if let Some(duration_ms) = map.get("duration_ms").and_then(Value::as_u64) {
            return format_sleep_duration_ms(duration_ms);
        }
    }
    if tool_name == Some("SendMessage") {
        let to = map.get("to").and_then(Value::as_str).unwrap_or("");
        let preview = map
            .get("summary")
            .and_then(Value::as_str)
            .or_else(|| map.get("message").and_then(Value::as_str))
            .unwrap_or("");
        return if preview.is_empty() {
            format!("\u{2192} {to}")
        } else {
            format!("\u{2192} {to}: {preview}")
        };
    }
    if tool_name == Some("ResolveEscalation") {
        let agent = map.get("agent_id").and_then(Value::as_str).unwrap_or("");
        let answer = map.get("answer").and_then(Value::as_str).unwrap_or("");
        return if answer.is_empty() {
            format!("\u{2192} {agent}")
        } else {
            format!("\u{2192} {agent}: {answer}")
        };
    }
    if let Some(keys) = tool_name.and_then(rebon_tools_core::primary_display_params) {
        if keys.is_empty() {
            return String::new();
        }
        let parts: Vec<String> = keys
            .iter()
            .filter_map(|key| map.get(*key))
            .filter(|value| !is_default_json_value(value))
            .map(compact_json_value)
            .collect();
        if !parts.is_empty() {
            return parts.join(", ");
        }
    }
    let hash: std::collections::HashMap<String, Value> =
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    super::compact_json_map(&hash)
}

/// Earlier revisions showed only the first entry, which was fine for
/// single-arg tools like `Read(file_path=…)` but silently dropped
/// critical fields for multi-arg tools — notably `Grep`, where the
/// alphabetical first key is often `glob` / `output_mode` and the
/// actual `pattern` never made it into the header.
pub(super) fn compact_json_object(map: &serde_json::Map<String, Value>) -> String {
    compact_json_object_for_tool(None, map)
}

fn compact_json_object_for_tool(
    tool_name: Option<&str>,
    map: &serde_json::Map<String, Value>,
) -> String {
    if tool_name.is_some() {
        return compact_json_map_value_for_tool(tool_name, map);
    }
    let mut entries: Vec<_> = map
        .iter()
        .filter(|(_, v)| !is_default_json_value(v))
        .collect();
    // Content-rich params first so the most informative fields survive
    // truncation.
    entries.sort_by(|(ka, va), (kb, vb)| {
        let la = compact_json_value(va).chars().count();
        let lb = compact_json_value(vb).chars().count();
        lb.cmp(&la).then_with(|| ka.cmp(kb))
    });
    let mut parts: Vec<String> = Vec::new();
    let mut total: usize = 0;
    for (k, v) in entries {
        let raw = compact_json_value(v);
        let val = truncate_param_value(k, &raw);
        let entry = format!("{k}={val}");
        let entry_chars = entry.chars().count();
        let needed = if parts.is_empty() {
            entry_chars
        } else {
            entry_chars + 2
        };
        if total + needed > SUMMARY_MAX_CHARS && !parts.is_empty() {
            parts.push("\u{2026}".into());
            break;
        }
        total += needed;
        parts.push(entry);
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect};
    use serde_json::json;

    use super::super::{render_message, RenderTheme, ToolOutputVerbosity};
    use super::compact_json_map_value_for_tool;
    use crate::message::{
        AssistantContentBlock, AssistantMessage, AssistantMessageInner, AssistantRole,
        AssistantTextBlock, AssistantToolUseBlock, Message,
    };

    fn new_buf(w: u16, h: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, w, h))
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        let mut s = String::new();
        for x in 0..buf.area().width {
            s.push_str(buf[(x, y)].symbol());
        }
        s.trim_end().to_string()
    }

    fn all_text(buf: &Buffer) -> String {
        (0..buf.area().height)
            .map(|y| row_text(buf, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn leaking_tool_content() -> Vec<rebon_types::ToolCallContent> {
        vec![
            rebon_types::ToolCallContent::Content(rebon_types::RegularContent {
                content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: "Leaking content description".into(),
                    annotations: None,
                }),
            }),
            rebon_types::ToolCallContent::Diff(rebon_types::DiffContent {
                path: "Leaking diff path".into(),
                old_text: None,
                new_text: "Leaking diff content".into(),
            }),
        ]
    }

    #[test]
    fn assistant_tool_use_renders_name_and_single_input_key() {
        let msg = Message::Assistant(AssistantMessage {
            uuid: "a1".into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                    id: "toolu_1".into(),
                    name: "Bash".into(),
                    input: json!({ "command": "ls -la" }),
                    tool_call_content: None,
                    raw_output: None,
                    title: None,
                    locations: None,
                    status: None,
                })],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        });
        let mut buf = new_buf(50, 4);
        render_message(
            &msg,
            Rect::new(0, 0, 50, 4),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Verbose,
        );
        let snap = all_text(&buf);
        assert!(snap.contains("Bash"), "expected tool name: {snap:?}");
        assert!(snap.contains("ls -la"), "expected input summary: {snap:?}");
    }

    #[test]
    fn assistant_sleep_tool_use_renders_seconds_summary_without_detail_body() {
        let msg = Message::Assistant(AssistantMessage {
            uuid: "a1".into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                    id: "toolu_sleep".into(),
                    name: "Sleep".into(),
                    input: json!({ "duration_ms": 20_000 }),
                    tool_call_content: Some(vec![rebon_types::ToolCallContent::Content(
                        rebon_types::RegularContent {
                            content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                                text: "sleeping 20000ms\nslept 20002ms".into(),
                                annotations: None,
                            }),
                        },
                    )]),
                    raw_output: Some(json!({ "durationMs": 20_002 })),
                    title: None,
                    locations: None,
                    status: Some(rebon_types::ToolCallStatus::Completed),
                })],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        });
        let mut buf = new_buf(50, 4);
        render_message(
            &msg,
            Rect::new(0, 0, 50, 4),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Verbose,
        );
        let snap = all_text(&buf);
        assert!(snap.contains("Sleep"), "expected tool name: {snap:?}");
        assert!(snap.contains("20s"), "expected duration summary: {snap:?}");
        assert!(!snap.contains("sleeping 20000ms"), "{snap:?}");
        assert!(!snap.contains("slept 20002ms"), "{snap:?}");
        assert!(!snap.contains("durationMs=20002"), "{snap:?}");
    }

    #[test]
    fn assistant_web_search_tool_use_shows_quoted_query_and_keeps_result() {
        let msg = Message::Assistant(AssistantMessage {
            uuid: "a1".into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![
                    AssistantContentBlock::Text(AssistantTextBlock {
                        text: "Assistant preface".into(),
                    }),
                    AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                        id: "toolu_search".into(),
                        name: "WebSearch".into(),
                        input: json!({ "query": "Windows Terminal synchronized output" }),
                        tool_call_content: Some(leaking_tool_content()),
                        raw_output: Some(json!({
                            "query": "Windows Terminal synchronized output",
                            "answer": "Search answer",
                            "results": [{
                                "title": "Leaking result title",
                                "url": "https://example.com",
                                "snippet": "Leaking result snippet"
                            }],
                            "serverToolUses": [{"name": "web_search"}]
                        })),
                        title: Some("Windows Terminal synchronized output".into()),
                        locations: Some(vec![rebon_types::ToolCallLocation {
                            path: "Leaking location path".into(),
                            line: Some(42),
                        }]),
                        status: None,
                    }),
                ],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        });
        for verbosity in [
            ToolOutputVerbosity::Compact,
            ToolOutputVerbosity::Normal,
            ToolOutputVerbosity::Verbose,
        ] {
            let mut buf = new_buf(80, 8);
            render_message(
                &msg,
                Rect::new(0, 0, 80, 8),
                &mut buf,
                &RenderTheme::plain(),
                verbosity,
            );
            let snap = all_text(&buf);
            assert!(
                snap.contains("WebSearch (\"Windows Terminal synchronized output\")"),
                "{verbosity:?}: {snap:?}"
            );
            assert!(snap.contains("Search answer"), "{verbosity:?}: {snap:?}");
            for hidden in [
                "Leaking content description",
                "Leaking diff path",
                "Leaking diff content",
                "Leaking location path",
                "Leaking result title",
                "Leaking result snippet",
                "serverToolUses",
            ] {
                assert!(!snap.contains(hidden), "{verbosity:?}: {snap:?}");
            }
        }
    }

    #[test]
    fn assistant_deferred_web_search_tool_use_shows_quoted_query() {
        let msg = Message::Assistant(AssistantMessage {
            uuid: "a1".into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![
                    AssistantContentBlock::Text(AssistantTextBlock {
                        text: "Assistant preface".into(),
                    }),
                    AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                        id: "toolu_search".into(),
                        name: "InvokeDeferredTool".into(),
                        input: json!({
                            "tool_name": "WebSearch",
                            "arguments": { "query": "Windows Terminal synchronized output" }
                        }),
                        tool_call_content: Some(leaking_tool_content()),
                        raw_output: Some(json!({
                            "query": "Windows Terminal synchronized output",
                            "answer": "Search answer",
                            "results": [{
                                "title": "Leaking result title",
                                "url": "https://example.com",
                                "snippet": "Leaking result snippet"
                            }],
                            "serverToolUses": [{"name": "web_search"}]
                        })),
                        title: Some("Windows Terminal synchronized output".into()),
                        locations: Some(vec![rebon_types::ToolCallLocation {
                            path: "Leaking location path".into(),
                            line: Some(42),
                        }]),
                        status: Some(rebon_types::ToolCallStatus::Completed),
                    }),
                ],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        });
        for verbosity in [
            ToolOutputVerbosity::Compact,
            ToolOutputVerbosity::Normal,
            ToolOutputVerbosity::Verbose,
        ] {
            let mut buf = new_buf(80, 8);
            render_message(
                &msg,
                Rect::new(0, 0, 80, 8),
                &mut buf,
                &RenderTheme::plain(),
                verbosity,
            );
            let snap = all_text(&buf);
            assert!(
                snap.contains("WebSearch (\"Windows Terminal synchronized output\")"),
                "{verbosity:?}: {snap:?}"
            );
            assert!(
                !snap.contains("InvokeDeferredTool"),
                "{verbosity:?}: {snap:?}"
            );
            assert!(snap.contains("Search answer"), "{verbosity:?}: {snap:?}");
            for hidden in [
                "Leaking content description",
                "Leaking diff path",
                "Leaking diff content",
                "Leaking location path",
                "Leaking result title",
                "Leaking result snippet",
                "serverToolUses",
            ] {
                assert!(!snap.contains(hidden), "{verbosity:?}: {snap:?}");
            }
        }
    }

    #[test]
    fn assistant_web_search_without_answer_hides_other_output() {
        let msg = Message::Assistant(AssistantMessage {
            uuid: "a1".into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![
                    AssistantContentBlock::Text(AssistantTextBlock {
                        text: "Assistant preface".into(),
                    }),
                    AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                        id: "toolu_search".into(),
                        name: "WebSearch".into(),
                        input: json!({ "query": "secret query" }),
                        tool_call_content: Some(leaking_tool_content()),
                        raw_output: Some(json!({
                            "query": "secret query",
                            "results": [{"title": "Leaking result title"}]
                        })),
                        title: Some("Leaking title".into()),
                        locations: Some(vec![rebon_types::ToolCallLocation {
                            path: "Leaking location path".into(),
                            line: Some(42),
                        }]),
                        status: None,
                    }),
                ],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        });
        for verbosity in [
            ToolOutputVerbosity::Compact,
            ToolOutputVerbosity::Normal,
            ToolOutputVerbosity::Verbose,
        ] {
            let mut buf = new_buf(80, 8);
            render_message(
                &msg,
                Rect::new(0, 0, 80, 8),
                &mut buf,
                &RenderTheme::plain(),
                verbosity,
            );
            let snap = all_text(&buf);
            assert!(
                snap.contains("WebSearch (\"secret query\")"),
                "{verbosity:?}: {snap:?}"
            );
            for hidden in [
                "Leaking title",
                "Leaking content description",
                "Leaking diff path",
                "Leaking diff content",
                "Leaking location path",
                "Leaking result title",
            ] {
                assert!(!snap.contains(hidden), "{verbosity:?}: {snap:?}");
            }
        }
    }

    #[test]
    fn tool_search_summary_still_shows_query() {
        let map = json!({ "query": "deferred tool schema" })
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            compact_json_map_value_for_tool(Some("ToolSearch"), &map),
            "deferred tool schema"
        );
    }

    #[test]
    fn assistant_bash_tool_use_deduplicates_output_and_hides_raw_metadata() {
        let text_content = || {
            rebon_types::ToolCallContent::Content(rebon_types::RegularContent {
                content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: "exit=101".into(),
                    annotations: None,
                }),
            })
        };
        let msg = Message::Assistant(AssistantMessage {
            uuid: "a1".into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                    id: "toolu_bash".into(),
                    name: "Bash".into(),
                    input: json!({ "command": "cargo test" }),
                    tool_call_content: Some(vec![text_content(), text_content()]),
                    raw_output: Some(json!({
                        "stdout": "exit=101",
                        "stderr": "",
                        "exitCode": 101,
                        "command": "cargo test"
                    })),
                    title: None,
                    locations: None,
                    status: Some(rebon_types::ToolCallStatus::Completed),
                })],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        });
        let mut buf = new_buf(80, 6);
        render_message(
            &msg,
            Rect::new(0, 0, 80, 6),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        let snap = all_text(&buf);
        assert_eq!(snap.matches("exit=101").count(), 1, "{snap:?}");
        assert!(!snap.contains("exitCode"), "{snap:?}");
        assert!(!snap.contains("stdout="), "{snap:?}");
        assert!(!snap.contains("command="), "{snap:?}");
    }

    #[test]
    fn assistant_tool_use_renders_all_input_keys_for_multi_arg_tool() {
        // Regression: Grep calls with multiple params used to render
        // only one alphabetically-first key (e.g. `glob="*.rs"`),
        // dropping the `pattern` entirely. Every provided field
        // should surface in the header.
        let msg = Message::Assistant(AssistantMessage {
            uuid: "a1".into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                    id: "toolu_grep".into(),
                    name: "Grep".into(),
                    input: json!({
                        "pattern": "needle",
                        "glob": "*.rs",
                        "output_mode": "content",
                    }),
                    tool_call_content: None,
                    raw_output: None,
                    title: None,
                    locations: None,
                    status: None,
                })],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        });
        let mut buf = new_buf(120, 4);
        render_message(
            &msg,
            Rect::new(0, 0, 120, 4),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Verbose,
        );
        let snap = all_text(&buf);
        assert!(snap.contains("Grep"), "expected tool name: {snap:?}");
        assert!(
            snap.contains("pattern=needle"),
            "pattern must appear in header: {snap:?}"
        );
        assert!(
            snap.contains("glob=*.rs"),
            "glob must appear in header: {snap:?}"
        );
        assert!(
            snap.contains("output_mode=content"),
            "output_mode must appear in header: {snap:?}"
        );
    }
}

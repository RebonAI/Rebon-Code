use std::collections::HashMap;

use rebon_render::agent::{agent_background_result_line, is_background_agent_result};
use serde_json::Value;

use super::{
    format_shortcut_hint, streaming_tool_summary, LiveAgentToolStatus, TranscriptRenderExtras,
};

fn is_async_agent_launch_tool(
    tool_name: &str,
    raw_output: Option<&HashMap<String, Value>>,
) -> bool {
    tool_name == "Agent" && raw_output.is_some_and(is_background_agent_result)
}

pub(super) fn agent_display_name<'a>(
    tool: &'a crate::streaming::StreamingToolUse,
) -> Option<&'a str> {
    fn non_empty_string(value: Option<&Value>) -> Option<&str> {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }

    if tool.tool_name != "Agent" {
        return None;
    }

    let output = tool.raw_output.as_ref();
    let input = tool.raw_input.as_ref();
    non_empty_string(output.and_then(|output| output.get("display_name")))
        .or_else(|| non_empty_string(output.and_then(|output| output.get("displayName"))))
        .or_else(|| {
            non_empty_string(
                input
                    .and_then(|input| input.get("metadata"))
                    .and_then(Value::as_object)
                    .and_then(|metadata| metadata.get("display_name")),
            )
        })
        .or_else(|| {
            non_empty_string(
                input
                    .and_then(|input| input.get("metadata"))
                    .and_then(Value::as_object)
                    .and_then(|metadata| metadata.get("displayName")),
            )
        })
        .or_else(|| non_empty_string(input.and_then(|input| input.get("name"))))
}

pub(super) fn async_agent_launch_display(
    tool: &crate::streaming::StreamingToolUse,
    extras: TranscriptRenderExtras<'_>,
) -> Option<String> {
    if !is_async_agent_launch_tool(&tool.tool_name, tool.raw_output.as_ref()) {
        return None;
    }
    let activity = extras.live_agent_tool_activity.get(&tool.call_id);
    if activity.is_none() {
        if let Some(display) = tool
            .raw_output
            .as_ref()
            .and_then(|output| agent_background_result_line(tool.raw_input.as_ref(), output))
        {
            return Some(display);
        }
    }
    Some(match activity.map(|activity| activity.status) {
        Some(LiveAgentToolStatus::Running) => activity
            .and_then(|activity| activity.text.as_ref())
            .filter(|text| !text.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| "background agent running".to_string()),
        Some(LiveAgentToolStatus::Completed) => "background agent completed".to_string(),
        Some(LiveAgentToolStatus::Failed) => activity
            .and_then(|activity| activity.text.as_deref())
            .filter(|error| !error.trim().is_empty())
            .map(|error| format!("background agent failed: {error}"))
            .unwrap_or_else(|| "background agent failed".to_string()),
        Some(LiveAgentToolStatus::Cancelled) => "background agent stopped".to_string(),
        Some(LiveAgentToolStatus::Unknown) | None => "background agent launched".to_string(),
    })
}

pub(super) fn async_agent_launch_header_summary(
    tool: &crate::streaming::StreamingToolUse,
    extras: TranscriptRenderExtras<'_>,
) -> Option<String> {
    if !is_async_agent_launch_tool(&tool.tool_name, tool.raw_output.as_ref()) {
        return None;
    }

    let activity = extras.live_agent_tool_activity.get(&tool.call_id);
    let mut parts = Vec::new();
    if let Some(title) = activity
        .and_then(|activity| activity.title.as_deref())
        .or_else(|| {
            tool.raw_input
                .as_ref()
                .and_then(|input| input.get("description"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            tool.raw_input
                .as_ref()
                .and_then(|input| input.get("prompt"))
                .and_then(Value::as_str)
        })
        .or(tool.title.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        parts.push(title.replace('\r', " ").replace('\n', " "));
    } else if let Some(summary) = tool
        .raw_input
        .as_ref()
        .map(|input| streaming_tool_summary(&tool.tool_name, input))
        .filter(|summary| !summary.is_empty())
    {
        parts.push(summary.replace('\r', " ").replace('\n', " "));
    } else {
        parts.push("background agent".to_string());
    }

    if let Some(tool_uses) = activity.and_then(|activity| activity.tool_use_count) {
        parts.push(format_agent_tool_use_count(tool_uses));
    }
    if let Some(tokens) = activity.and_then(|activity| activity.token_count) {
        parts.push(format_agent_token_count(tokens));
    }

    let manage_hint = format_shortcut_hint("↓", "manage", true, false).plain_text;
    Some(format!("{} {manage_hint}", parts.join(" · ")))
}

// format_agent_token_count moved to `rebon-render::agent`. Re-exported
// so `async_agent_launch_display` and the other render callers resolve unchanged.
pub(super) use rebon_render::agent::format_agent_token_count;

fn format_agent_tool_use_count(tool_uses: u64) -> String {
    format!(
        "{tool_uses} {}",
        if tool_uses == 1 {
            "tool use"
        } else {
            "tool uses"
        }
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ratatui::layout::Rect;
    use serde_json::json;

    use super::super::tests::{all_text, new_buf};
    use super::super::{
        render_transcript_cached_with_running_hints, LiveAgentToolActivity, LiveAgentToolStatus,
        RenderTheme, ToolOutputVerbosity, TranscriptMeasureCache, TranscriptRenderExtras,
    };
    use crate::message::{
        AssistantContentBlock, AssistantMessage, AssistantMessageInner, AssistantRole,
        AssistantToolUseBlock, Message,
    };
    use crate::state::{reducer, Action, AppState};
    use crate::streaming::StreamingToolUse;
    use rebon_types::{ToolCallStatus, ToolKind};

    fn assistant_async_agent_launch(uuid: &str, call_id: &str) -> Message {
        Message::Assistant(AssistantMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                    id: call_id.into(),
                    name: "Agent".into(),
                    input: json!({ "description": "Research external file inputs" }),
                    tool_call_content: None,
                    raw_output: Some(json!({
                        "status": "async_launched",
                        "task_id": "agent-task-1",
                        "agent_id": "agent-task-1",
                        "description": "Research external file inputs"
                    })),
                    title: None,
                    locations: None,
                    status: Some(ToolCallStatus::Completed),
                })],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    fn assistant_background_teammate(status: &str, call_id: &str) -> Message {
        Message::Assistant(AssistantMessage {
            uuid: format!("a-{status}"),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                    id: call_id.into(),
                    name: "Agent".into(),
                    input: json!({
                        "name": "epub-web-audit",
                        "description": "审查 Web EPUB 链路",
                        "prompt": "Audit the web EPUB path"
                    }),
                    tool_call_content: None,
                    raw_output: Some(json!({
                        "status": status,
                        "agentId": "agent-secret",
                        "taskId": "task-secret",
                        "handoff": null
                    })),
                    title: None,
                    locations: None,
                    status: Some(ToolCallStatus::Completed),
                })],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    fn async_agent_launch_tool(call_id: &str) -> StreamingToolUse {
        StreamingToolUse {
            call_id: call_id.into(),
            tool_name: "Agent".into(),
            kind: ToolKind::Other,
            status: ToolCallStatus::Completed,
            title: None,
            content: None,
            locations: None,
            raw_input: None,
            raw_output: Some(HashMap::from([(
                "status".to_string(),
                json!("async_launched"),
            )])),
        }
    }

    fn display_for_activity(activity: LiveAgentToolActivity) -> String {
        let call_id = "toolu_agent";
        let tool = async_agent_launch_tool(call_id);
        let live = HashMap::from([(call_id.to_string(), activity)]);

        super::async_agent_launch_display(
            &tool,
            TranscriptRenderExtras {
                live_agent_tool_activity: &live,
                ..TranscriptRenderExtras::empty()
            },
        )
        .expect("async Agent launch should have a compact display")
    }

    fn background_teammate_tool(status: &str) -> StreamingToolUse {
        StreamingToolUse {
            call_id: format!("toolu_{status}"),
            tool_name: "Agent".into(),
            kind: ToolKind::Other,
            status: ToolCallStatus::Completed,
            title: None,
            content: None,
            locations: None,
            raw_input: Some(HashMap::from([(
                "name".to_string(),
                json!("epub-web-audit"),
            )])),
            raw_output: Some(HashMap::from([
                ("status".to_string(), json!(status)),
                ("handoff".to_string(), serde_json::Value::Null),
            ])),
        }
    }

    #[test]
    fn teammate_launch_and_dispatch_use_background_agent_layout() {
        let launched = super::async_agent_launch_display(
            &background_teammate_tool("teammate_spawned"),
            TranscriptRenderExtras::empty(),
        );
        let dispatched = super::async_agent_launch_display(
            &background_teammate_tool("teammate_dispatched"),
            TranscriptRenderExtras::empty(),
        );

        assert_eq!(
            launched.as_deref(),
            Some("Teammate @epub-web-audit launched")
        );
        assert_eq!(
            dispatched.as_deref(),
            Some("Message sent to teammate @epub-web-audit")
        );
    }

    #[test]
    fn committed_teammate_spawn_and_dispatch_hide_raw_protocol_output() {
        for (status, expected) in [
            ("teammate_spawned", "Teammate @epub-web-audit launched"),
            (
                "teammate_dispatched",
                "Message sent to teammate @epub-web-audit",
            ),
        ] {
            let mut state = AppState::new();
            reducer(
                &mut state,
                Action::Commit(assistant_background_teammate(status, "toolu-teammate")),
            );
            let mut buf = new_buf(120, 8);
            let mut cache = TranscriptMeasureCache::new();
            render_transcript_cached_with_running_hints(
                &state,
                Rect::new(0, 0, 120, 8),
                &mut buf,
                &RenderTheme::plain(),
                0,
                ToolOutputVerbosity::Compact,
                0,
                None,
                &mut cache,
                false,
                TranscriptRenderExtras::empty(),
            );
            let snap = all_text(&buf);
            assert!(snap.contains(expected), "missing {expected:?}: {snap:?}");
            for leaked in ["handoff", "agent-secret", "task-secret", status] {
                assert!(!snap.contains(leaked), "leaked {leaked:?}: {snap:?}");
            }
        }
    }

    #[test]
    fn failed_async_agent_launch_renders_actual_error() {
        let error = "unsupported model `gpt-unsupported` for provider `test`";
        let display = display_for_activity(LiveAgentToolActivity {
            text: Some(error.to_string()),
            status: LiveAgentToolStatus::Failed,
            title: None,
            display_name: None,
            start_time_ms: None,
            end_time_ms: None,
            tool_use_count: None,
            token_count: None,
            terminal_result: None,
        });

        assert_eq!(display, format!("background agent failed: {error}"));
    }

    #[test]
    fn failed_async_agent_launch_without_error_keeps_generic_text() {
        let display = display_for_activity(LiveAgentToolActivity {
            text: Some("  \n ".to_string()),
            status: LiveAgentToolStatus::Failed,
            title: None,
            display_name: None,
            start_time_ms: None,
            end_time_ms: None,
            tool_use_count: None,
            token_count: None,
            terminal_result: None,
        });

        assert_eq!(display, "background agent failed");
    }

    #[test]
    fn committed_async_agent_launch_renders_live_activity_instead_of_raw_output() {
        let mut s = AppState::new();
        reducer(
            &mut s,
            Action::Commit(assistant_async_agent_launch("a-agent", "toolu_agent")),
        );
        let mut live = HashMap::new();
        live.insert(
            "toolu_agent".to_string(),
            LiveAgentToolActivity {
                text: Some("reading crates/rebon-cli/src/tui/update.rs".to_string()),
                status: LiveAgentToolStatus::Running,
                title: Some("Research external file inputs".to_string()),
                display_name: None,
                start_time_ms: None,
                end_time_ms: None,
                tool_use_count: Some(2),
                token_count: Some(12_345),
                terminal_result: None,
            },
        );

        let mut buf = new_buf(120, 8);
        let mut cache = TranscriptMeasureCache::new();
        render_transcript_cached_with_running_hints(
            &s,
            Rect::new(0, 0, 120, 8),
            &mut buf,
            &RenderTheme::plain(),
            0,
            ToolOutputVerbosity::Compact,
            0,
            None,
            &mut cache,
            false,
            TranscriptRenderExtras {
                live_agent_tool_activity: &live,
                live_activity_revision: 1,
                ..TranscriptRenderExtras::empty()
            },
        );
        let snap = all_text(&buf);

        assert!(
            snap.contains(
                "Agent: Research external file inputs · 2 tool uses · 12.3k tokens (↓ to manage)"
            ),
            "live header metadata missing: {snap:?}"
        );
        assert!(
            snap.contains("reading crates/rebon-cli/src/tui/update.rs"),
            "live activity missing: {snap:?}"
        );
        assert!(
            !snap.contains("async_launched") && !snap.contains("agent-task-1"),
            "raw async launch payload leaked: {snap:?}"
        );
    }

    #[test]
    fn committed_async_agent_launch_uses_terminal_compact_status_without_raw_output() {
        let mut s = AppState::new();
        reducer(
            &mut s,
            Action::Commit(assistant_async_agent_launch("a-agent", "toolu_agent")),
        );
        let mut live = HashMap::new();
        live.insert(
            "toolu_agent".to_string(),
            LiveAgentToolActivity {
                text: None,
                status: LiveAgentToolStatus::Completed,
                title: Some("Research external file inputs".to_string()),
                display_name: None,
                start_time_ms: None,
                end_time_ms: None,
                tool_use_count: Some(2),
                token_count: Some(12_345),
                terminal_result: None,
            },
        );

        let mut buf = new_buf(120, 8);
        let mut cache = TranscriptMeasureCache::new();
        render_transcript_cached_with_running_hints(
            &s,
            Rect::new(0, 0, 120, 8),
            &mut buf,
            &RenderTheme::plain(),
            0,
            ToolOutputVerbosity::Compact,
            0,
            None,
            &mut cache,
            false,
            TranscriptRenderExtras {
                live_agent_tool_activity: &live,
                live_activity_revision: 1,
                ..TranscriptRenderExtras::empty()
            },
        );
        let snap = all_text(&buf);

        assert!(
            snap.contains(
                "Agent: Research external file inputs · 2 tool uses · 12.3k tokens (↓ to manage)"
            ),
            "terminal header metadata missing: {snap:?}"
        );
        assert!(
            snap.contains("background agent completed"),
            "terminal compact status missing: {snap:?}"
        );
        assert!(
            !snap.contains("async_launched") && !snap.contains("agent-task-1"),
            "raw async launch payload leaked after completion: {snap:?}"
        );
    }

    #[test]
    fn unrelated_live_activity_keeps_async_agent_layout_and_row_cache_hot() {
        let mut state = AppState::new();
        reducer(
            &mut state,
            Action::Commit(assistant_async_agent_launch("a-agent", "toolu_agent")),
        );
        let mut live = HashMap::from([(
            "toolu_other".to_string(),
            LiveAgentToolActivity {
                text: Some("first update".into()),
                status: LiveAgentToolStatus::Running,
                title: None,
                display_name: None,
                start_time_ms: None,
                end_time_ms: None,
                tool_use_count: None,
                token_count: None,
                terminal_result: None,
            },
        )]);
        let area = Rect::new(0, 0, 80, 2);
        let mut buf = new_buf(80, 2);
        let mut cache = TranscriptMeasureCache::new();

        render_transcript_cached_with_running_hints(
            &state,
            area,
            &mut buf,
            &RenderTheme::plain(),
            1,
            ToolOutputVerbosity::Compact,
            0,
            None,
            &mut cache,
            false,
            TranscriptRenderExtras {
                live_agent_tool_activity: &live,
                live_activity_revision: 1,
                leading_segment_margin: true,
                ..TranscriptRenderExtras::empty()
            },
        );
        let row_cache_len = cache.row_heights.len();
        assert_eq!(cache.layout_full_builds, 1);
        assert_eq!(cache.clipped_segment_hits, 0);

        live.get_mut("toolu_other").unwrap().text = Some("second update".into());
        render_transcript_cached_with_running_hints(
            &state,
            area,
            &mut buf,
            &RenderTheme::plain(),
            1,
            ToolOutputVerbosity::Compact,
            0,
            None,
            &mut cache,
            false,
            TranscriptRenderExtras {
                live_agent_tool_activity: &live,
                live_activity_revision: 2,
                leading_segment_margin: true,
                ..TranscriptRenderExtras::empty()
            },
        );

        assert_eq!(cache.layout_cache_hits, 1);
        assert_eq!(cache.layout_full_builds, 1);
        assert_eq!(cache.layout_activity_updates, 0);
        assert_eq!(cache.clipped_segment_hits, 1);
        assert_eq!(cache.row_heights.len(), row_cache_len);
    }

    #[test]
    fn matching_live_activity_remeasures_only_the_async_agent_segment() {
        let mut state = AppState::new();
        reducer(
            &mut state,
            Action::Commit(assistant_async_agent_launch("a-agent", "toolu_agent")),
        );
        let mut live = HashMap::from([(
            "toolu_agent".to_string(),
            LiveAgentToolActivity {
                text: Some("reading source".into()),
                status: LiveAgentToolStatus::Running,
                title: Some("Research external file inputs".into()),
                display_name: None,
                start_time_ms: None,
                end_time_ms: None,
                tool_use_count: Some(1),
                token_count: Some(100),
                terminal_result: None,
            },
        )]);
        let area = Rect::new(0, 0, 48, 16);
        let mut buf = new_buf(48, 16);
        let mut cache = TranscriptMeasureCache::new();

        let first = render_transcript_cached_with_running_hints(
            &state,
            area,
            &mut buf,
            &RenderTheme::plain(),
            0,
            ToolOutputVerbosity::Compact,
            0,
            None,
            &mut cache,
            false,
            TranscriptRenderExtras {
                live_agent_tool_activity: &live,
                live_activity_revision: 1,
                ..TranscriptRenderExtras::empty()
            },
        );
        assert_eq!(cache.layout_full_builds, 1);
        assert_eq!(cache.row_heights.len(), 1);

        live.get_mut("toolu_agent").unwrap().text =
            Some("reading a much longer source path that wraps onto several terminal rows".into());
        let second = render_transcript_cached_with_running_hints(
            &state,
            area,
            &mut buf,
            &RenderTheme::plain(),
            0,
            ToolOutputVerbosity::Compact,
            0,
            None,
            &mut cache,
            false,
            TranscriptRenderExtras {
                live_agent_tool_activity: &live,
                live_activity_revision: 2,
                ..TranscriptRenderExtras::empty()
            },
        );
        let snap = all_text(&buf);

        assert!(
            snap.contains("reading a much longer source path"),
            "{snap:?}"
        );
        assert!(second.total_lines > first.total_lines);
        assert_eq!(cache.layout_activity_updates, 1);
        assert_eq!(cache.layout_full_builds, 1);
        assert_eq!(cache.row_heights.len(), 1);
    }
}

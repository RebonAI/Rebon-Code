use super::super::*;
use super::common::*;
use crate::message::{
    AssistantMessage, AssistantMessageInner, AssistantRole, AssistantTextBlock,
    AssistantThinkingBlock, AssistantToolUseBlock, UserMessage, UserMessageInner, UserRole,
    UserTextBlock,
};
use crate::state::{reducer, Action};
use serde_json::json;

#[test]
fn streaming_thinking_incremental_frames_match_full_repaint() {
    let text = "The user asked me to count .rs files under crates/rebon-cli/src/tui/";
    for width in [24, 80, 120] {
        for verbosity in [
            ToolOutputVerbosity::Compact,
            ToolOutputVerbosity::Normal,
            ToolOutputVerbosity::Verbose,
        ] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 16))
                    .expect("test terminal");
            let mut thinking = crate::streaming::StreamingThinking {
                thinking: String::new(),
                is_streaming: true,
                streaming_ended_at: None,
            };
            for ch in text.chars() {
                thinking.thinking.push(ch);
                let area = Rect::new(0, 0, width, 16);
                let mut expected = new_buf(width, 16);
                let theme = RenderTheme::plain();
                super::super::streaming::render_streaming_thinking(
                    &thinking,
                    area,
                    &mut expected,
                    &theme,
                    verbosity,
                    StreamingOverlayRenderMode::Paint,
                    false,
                );
                terminal
                    .draw(|frame| {
                        super::super::streaming::render_streaming_thinking(
                            &thinking,
                            area,
                            frame.buffer_mut(),
                            &theme,
                            verbosity,
                            StreamingOverlayRenderMode::Paint,
                            false,
                        );
                    })
                    .expect("draw thinking");
                assert_eq!(
                    terminal.backend().buffer(),
                    &expected,
                    "{width} {verbosity:?}: {}",
                    thinking.thinking
                );
                if width == 120 {
                    assert!(
                        all_text(&expected).contains(thinking.thinking.trim()),
                        "{}",
                        all_text(&expected)
                    );
                }
            }
        }
    }
}

#[test]
fn committed_assistant_text_between_tool_uses_breaks_the_run() {
    // An assistant message that mixes text with tool_use (or is
    // pure text) must NOT be absorbed into a collapsed group.
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a1",
            vec![("toolu_1", "Grep", json!({ "pattern": "foo" }))],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results("u1", vec![("toolu_1", "matches", None)])),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_text("a2", "let me look at something else")),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a3",
            vec![("toolu_2", "Grep", json!({ "pattern": "bar" }))],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results("u2", vec![("toolu_2", "matches", None)])),
    );

    let mut buf = new_buf(120, 16);
    render_transcript(
        &s,
        Rect::new(0, 0, 120, 16),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    // Two individual Greps, not a collapsed summary.
    assert!(
        !snap.contains("Searched for 2 patterns"),
        "text between tool_uses must prevent aggregation: {snap:?}"
    );
    assert!(
        snap.contains("let me look at something else"),
        "intervening assistant text must be visible: {snap:?}"
    );
}

#[test]
fn inline_extras_render_thinking_only_committed_rows_but_default_stays_transparent() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_thinking("a-think", "visible reasoning")),
    );

    let mut default_buf = new_buf(80, 6);
    render_transcript(
        &s,
        Rect::new(0, 0, 80, 6),
        &mut default_buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let default_snap = all_text(&default_buf);
    assert!(
        !default_snap.contains("visible reasoning"),
        "screen/default compact residue should stay transparent: {default_snap:?}"
    );

    let mut inline_buf = new_buf(80, 6);
    let mut cache = TranscriptMeasureCache::new();
    render_transcript_cached_with_running_hints(
        &s,
        Rect::new(0, 0, 80, 6),
        &mut inline_buf,
        &RenderTheme::plain(),
        usize::MAX,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        TranscriptRenderExtras {
            render_thinking_only_rows: true,
            ..TranscriptRenderExtras::empty()
        },
    );
    let inline_snap = all_text(&inline_buf);
    assert!(
        inline_snap.contains("visible reasoning"),
        "inline committed thinking title missing: {inline_snap:?}"
    );
    assert!(
        inline_snap.contains("Ctrl+O to expand"),
        "inline committed thinking must expose an expand hint: {inline_snap:?}"
    );
}

#[test]
fn inline_committed_hidden_tool_keeps_thinking_in_one_reasoning_group() {
    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_thinking(
            "a-think-1",
            "Updating task progress status",
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a-tool",
            vec![(
                "task-update-1",
                "TaskUpdate",
                json!({ "taskId": "2", "status": "completed" }),
            )],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u-tool",
            vec![("task-update-1", "updated", None)],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_thinking(
            "a-think-2",
            "Deploying docs and landing site",
        )),
    );

    let mut buf = new_buf(100, 10);
    let mut cache = TranscriptMeasureCache::new();
    render_transcript_cached_with_running_hints(
        &s,
        Rect::new(0, 0, 100, 10),
        &mut buf,
        &RenderTheme::plain(),
        usize::MAX,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        TranscriptRenderExtras {
            render_thinking_only_rows: true,
            ..TranscriptRenderExtras::empty()
        },
    );
    let snap = all_text(&buf);

    assert!(snap.contains("Reasoning (2 steps)"), "{snap:?}");
    assert!(snap.contains("├ Updating task progress status"), "{snap:?}");
    assert!(
        snap.contains("└ Deploying docs and landing site"),
        "{snap:?}"
    );
    assert_eq!(snap.matches("Ctrl+O to expand").count(), 1, "{snap:?}");
    assert!(!snap.contains("TaskUpdate"), "{snap:?}");
}

#[test]
fn inline_committed_visible_tool_splits_thinking_groups() {
    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_thinking("a-think-1", "first reasoning")),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a-tool",
            vec![("bash-1", "Bash", json!({ "command": "printf output" }))],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u-tool",
            vec![("bash-1", "output", None)],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_thinking("a-think-2", "second reasoning")),
    );

    let mut buf = new_buf(100, 12);
    let mut cache = TranscriptMeasureCache::new();
    render_transcript_cached_with_running_hints(
        &s,
        Rect::new(0, 0, 100, 12),
        &mut buf,
        &RenderTheme::plain(),
        usize::MAX,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        TranscriptRenderExtras {
            render_thinking_only_rows: true,
            ..TranscriptRenderExtras::empty()
        },
    );
    let snap = all_text(&buf);

    assert!(!snap.contains("Reasoning (2 steps)"), "{snap:?}");
    assert!(snap.contains("· first reasoning"), "{snap:?}");
    assert!(snap.contains("· second reasoning"), "{snap:?}");
    assert!(snap.contains("Bash"), "{snap:?}");
}

#[test]
fn compact_transcript_keeps_thinking_preview_before_coordinator_notifications() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_thinking(
            "a-think",
            "hidden reasoning\nvisible coordinator reasoning",
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user(
            "u-task",
            "<task-notification>\n<summary>background agent launched</summary>\n<status>running</status>\n</task-notification>",
        )),
    );

    let mut buf = new_buf(100, 10);
    render_transcript(
        &s,
        Rect::new(0, 0, 100, 10),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    assert!(
        snap.contains("hidden reasoning") && !snap.contains("visible coordinator reasoning"),
        "only the first thinking line should be visible in compact view: {snap:?}"
    );
    assert!(snap.contains("Ctrl+O to expand"), "{snap:?}");
    assert!(
        snap.contains("background agent launched"),
        "notification row should still render: {snap:?}"
    );
}

#[test]
fn compact_transcript_keeps_thinking_preview_before_teammate_messages() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_thinking(
            "a-think",
            "hidden reasoning\nvisible teammate reasoning",
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user(
            "u-teammate",
            "<teammate-message from=\"researcher\">agent started</teammate-message>",
        )),
    );

    let mut buf = new_buf(100, 10);
    render_transcript(
        &s,
        Rect::new(0, 0, 100, 10),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    assert!(
        snap.contains("hidden reasoning") && !snap.contains("visible teammate reasoning"),
        "only the first thinking line should be visible in compact view: {snap:?}"
    );
    assert!(snap.contains("Ctrl+O to expand"), "{snap:?}");
}

#[test]
fn compact_transcript_still_hides_thinking_before_plain_user_prompt() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_thinking("a-think", "old reasoning")),
    );
    reducer(&mut s, Action::Commit(user("u-next", "new user prompt")));

    let mut buf = new_buf(100, 10);
    render_transcript(
        &s,
        Rect::new(0, 0, 100, 10),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    assert!(
        !snap.contains("old reasoning"),
        "plain user prompt should remain a thinking boundary: {snap:?}"
    );
    assert!(snap.contains("new user prompt"), "{snap:?}");
}

#[test]
fn inline_committed_thinking_uses_direct_compact_preview() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_thinking("a-think", "closed reasoning")),
    );

    let mut inline_buf = new_buf(80, 6);
    let mut cache = TranscriptMeasureCache::new();
    render_transcript_cached_with_running_hints(
        &s,
        Rect::new(0, 0, 80, 6),
        &mut inline_buf,
        &RenderTheme::plain(),
        usize::MAX,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        TranscriptRenderExtras {
            render_thinking_only_rows: true,
            ..TranscriptRenderExtras::empty()
        },
    );
    let inline_snap = all_text(&inline_buf);
    assert!(
        inline_snap.contains("closed reasoning"),
        "inline committed thinking title missing: {inline_snap:?}"
    );
    assert!(inline_snap.contains("Ctrl+O to expand"), "{inline_snap:?}");
}

#[test]
fn compact_transcript_keeps_thinking_preview_before_agent_tool_rows() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_and_tool_uses(
            "a-agent",
            "hidden agent reasoning\nvisible agent reasoning",
            vec![(
                "agent-1",
                "Agent",
                json!({
                    "description": "Review ACP code",
                    "prompt": "Review ACP code",
                    "subagent_type": "Explore"
                }),
            )],
        )),
    );

    let mut buf = new_buf(100, 10);
    render_transcript(
        &s,
        Rect::new(0, 0, 100, 10),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let rows = semantic_rows(&buf);
    let snap = rows.join("\n");

    assert!(
        snap.contains("hidden agent reasoning") && !snap.contains("visible agent reasoning"),
        "only the first thinking line should be visible in compact view: {snap:?}"
    );
    assert!(snap.contains("Ctrl+O to expand"), "{snap:?}");
    // The row is named by the shared display-name map, so an Agent is shown
    // under its subagent type here exactly as the streaming card shows it.
    assert!(snap.contains("Explore ("), "{snap:?}");
    assert!(snap.contains("Review ACP code"), "{snap:?}");
    assert_no_adjacent_blank_rows(&rows);
}

#[test]
fn committed_rows_name_run_code_from_the_shared_display_name_map() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_and_tool_uses(
            "a-code",
            "sequence reasoning",
            vec![(
                "code-1",
                "run_code",
                json!({
                    "description": "Inspect tasks",
                    "code": "return await tools.TaskList();"
                }),
            )],
        )),
    );

    let mut buf = new_buf(100, 12);
    render_transcript(
        &s,
        Rect::new(0, 0, 100, 12),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Verbose,
        0,
        None,
    );
    let snap = semantic_rows(&buf).join("\n");

    assert!(snap.contains("Run sequence ("), "{snap:?}");
    assert!(!snap.contains("run_code ("), "{snap:?}");
}

#[test]
fn inline_render_keeps_active_thinking_visible_before_later_text() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::AppendStreamingThinking("first reasoning".into()),
    );
    reducer(&mut s, Action::SetStreamingText("later answer".into()));

    let mut buf = new_buf(80, 10);
    let mut cache = TranscriptMeasureCache::new();
    render_transcript_cached_with_running_hints(
        &s,
        Rect::new(0, 0, 80, 10),
        &mut buf,
        &RenderTheme::plain(),
        usize::MAX,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        TranscriptRenderExtras {
            render_thinking_only_rows: true,
            ..TranscriptRenderExtras::empty()
        },
    );
    let snap = all_text(&buf);
    assert!(snap.contains("first reasoning"), "{snap:?}");
    assert!(snap.contains("Ctrl+O to expand"), "{snap:?}");
    assert!(
        snap.contains("later answer"),
        "later text missing: {snap:?}"
    );
}

#[test]
fn inline_compact_keeps_prior_thinking_preview_after_later_thinking() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(&mut s, Action::Commit(user("u1", "question")));
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_then_text(
            "a-first",
            "first hidden reasoning\nfirst visible reasoning",
            "first assistant text",
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_then_text(
            "a-second",
            "second hidden reasoning\nsecond visible reasoning",
            "second assistant text",
        )),
    );

    let mut buf = new_buf(100, 16);
    let mut cache = TranscriptMeasureCache::new();
    render_transcript_cached_with_running_hints(
        &s,
        Rect::new(0, 0, 100, 16),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        TranscriptRenderExtras {
            render_thinking_only_rows: true,
            ..TranscriptRenderExtras::empty()
        },
    );
    let snap = all_text(&buf);
    assert!(snap.contains("first hidden reasoning"), "{snap:?}");
    assert!(snap.contains("first assistant text"), "{snap:?}");
    assert!(snap.contains("second assistant text"), "{snap:?}");
    assert!(snap.contains("second hidden reasoning"), "{snap:?}");
    assert!(!snap.contains("first visible reasoning"), "{snap:?}");
    assert!(!snap.contains("second visible reasoning"), "{snap:?}");
    assert!(snap.contains("Ctrl+O to expand"), "{snap:?}");
}

#[test]
fn compact_and_normal_transcript_show_thinking_before_assistant_text() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_then_text(
            "a-think-text",
            "first early reasoning\nfull early reasoning",
            "final answer",
        )),
    );

    let mut compact_buf = new_buf(80, 10);
    render_transcript(
        &s,
        Rect::new(0, 0, 80, 10),
        &mut compact_buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let compact_snap = all_text(&compact_buf);
    assert!(compact_snap.contains("final answer"), "{compact_snap:?}");
    assert!(
        compact_snap.contains("first early reasoning")
            && !compact_snap.contains("full early reasoning"),
        "compact should show only first thinking line: {compact_snap:?}"
    );
    assert!(
        compact_snap.contains("Ctrl+O to expand"),
        "{compact_snap:?}"
    );

    let mut normal_buf = new_buf(80, 12);
    render_transcript(
        &s,
        Rect::new(0, 0, 80, 12),
        &mut normal_buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Normal,
        0,
        None,
    );
    let normal_snap = all_text(&normal_buf);
    assert!(
        normal_snap.contains("first early reasoning"),
        "{normal_snap:?}"
    );
    assert!(
        normal_snap.contains("full early reasoning"),
        "{normal_snap:?}"
    );
    assert!(normal_snap.contains("final answer"), "{normal_snap:?}");
    assert!(!normal_snap.contains("Thinking"), "{normal_snap:?}");
    assert!(!normal_snap.contains("Ctrl+O to expand"), "{normal_snap:?}");
}

#[test]
fn normal_transcript_reveals_multiple_thinking_blocks() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_only("a1", "first hidden\nfirst visible")),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_only(
            "a2",
            "second hidden\nsecond visible",
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_text("a-final", "final answer")),
    );

    let mut compact_buf = new_buf(80, 12);
    render_transcript(
        &s,
        Rect::new(0, 0, 80, 12),
        &mut compact_buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let compact_snap = all_text(&compact_buf);
    assert!(
        compact_snap.contains("Reasoning (2 steps)"),
        "{compact_snap:?}"
    );
    assert!(compact_snap.contains("first hidden"), "{compact_snap:?}");
    assert!(compact_snap.contains("second hidden"), "{compact_snap:?}");
    assert!(!compact_snap.contains("first visible"), "{compact_snap:?}");
    assert!(!compact_snap.contains("second visible"), "{compact_snap:?}");
    assert_eq!(
        compact_snap.matches("Ctrl+O to expand").count(),
        1,
        "the thinking group should own a single expand hint: {compact_snap:?}"
    );
    assert!(compact_snap.contains("├ first hidden"), "{compact_snap:?}");
    assert!(compact_snap.contains("└ second hidden"), "{compact_snap:?}");

    let mut normal_buf = new_buf(80, 16);
    render_transcript(
        &s,
        Rect::new(0, 0, 80, 16),
        &mut normal_buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Normal,
        0,
        None,
    );
    let normal_snap = all_text(&normal_buf);
    assert!(normal_snap.contains("first hidden"), "{normal_snap:?}");
    assert!(normal_snap.contains("first visible"), "{normal_snap:?}");
    assert!(normal_snap.contains("second hidden"), "{normal_snap:?}");
    assert!(normal_snap.contains("second visible"), "{normal_snap:?}");
    assert!(normal_snap.contains("final answer"), "{normal_snap:?}");
    assert!(!normal_snap.contains("Thinking"), "{normal_snap:?}");
    assert!(!normal_snap.contains("Ctrl+O to expand"), "{normal_snap:?}");
}

#[test]
fn committed_tool_message_groups_its_thinking_as_reasoning() {
    let mut state = AppState::new();
    reducer(
        &mut state,
        Action::Commit(Message::Assistant(AssistantMessage {
            uuid: "a-grouped-bash".into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![
                    AssistantContentBlock::Thinking(AssistantThinkingBlock {
                        thinking: "first reasoning".into(),
                        signature: None,
                    }),
                    AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                        id: "toolu-bash".into(),
                        name: "Bash".into(),
                        input: json!({"command": "printf output"}),
                        tool_call_content: None,
                        raw_output: Some(json!({
                            "stdout": "one\ntwo\nthree\nfour\nfive\nsix",
                            "stderr": "",
                            "exitCode": 0
                        })),
                        title: None,
                        locations: None,
                        status: Some(ToolCallStatus::Completed),
                    }),
                    AssistantContentBlock::Thinking(AssistantThinkingBlock {
                        thinking: "second reasoning".into(),
                        signature: None,
                    }),
                ],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })),
    );

    let mut buf = new_buf(100, 16);
    render_transcript(
        &state,
        Rect::new(0, 0, 100, 16),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    assert!(snap.contains("Reasoning (2 steps)"), "{snap:?}");
    assert!(snap.contains("├ first reasoning"), "{snap:?}");
    assert!(snap.contains("└ second reasoning"), "{snap:?}");
    assert!(snap.contains("● Bash (printf output)"), "{snap:?}");
    assert_eq!(snap.matches("Ctrl+O to expand").count(), 1, "{snap:?}");
}

#[test]
fn compact_transcript_keeps_thinking_only_anchor_before_assistant_text() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_only(
            "a-think-only",
            "ordinary reasoning\nvisible reasoning",
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_text("a-final", "final answer")),
    );

    let mut buf = new_buf(80, 10);
    render_transcript(
        &s,
        Rect::new(0, 0, 80, 10),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);
    assert!(snap.contains("ordinary reasoning"), "{snap:?}");
    assert!(!snap.contains("visible reasoning"), "{snap:?}");
    assert!(snap.contains("Ctrl+O to expand"), "{snap:?}");
    assert!(snap.contains("final answer"), "{snap:?}");
}

/// Build an assistant message whose content is a leading Text block
/// followed by one or more ToolUse blocks — the shape produced by
/// [`commit_streaming_content_and_clear`] when the model emits a
/// narrated turn ("Let me search…" + N × ToolUse).
fn assistant_mixed_text_and_tool_uses(
    uuid: &str,
    preamble: &str,
    tools: Vec<(&str, &str, serde_json::Value)>,
) -> Message {
    let mut content = vec![AssistantContentBlock::Text(AssistantTextBlock {
        text: preamble.into(),
    })];
    for (id, name, input) in tools {
        content.push(AssistantContentBlock::ToolUse(AssistantToolUseBlock {
            id: id.into(),
            name: name.into(),
            input,
            tool_call_content: None,
            raw_output: None,
            title: None,
            locations: None,
            status: None,
        }));
    }
    Message::Assistant(AssistantMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content,
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}

#[test]
fn committed_whitespace_text_assistant_message_still_collapses_live_bundle_shape() {
    // The load-bearing regression test for the live path.
    // `commit_streaming_content_and_clear` can include whitespace
    // text around ToolUse blocks. Whitespace-only text is transparent
    // to collapse; non-empty narration is covered by the regression
    // test below and must remain visible instead.
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_mixed_text_and_tool_uses(
            "a1",
            " \n\t ",
            vec![
                ("toolu_1", "Grep", json!({ "pattern": "foo" })),
                ("toolu_2", "Grep", json!({ "pattern": "bar" })),
                ("toolu_3", "Read", json!({ "file_path": "src/lib.rs" })),
            ],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u1",
            vec![
                ("toolu_1", "matches for foo", None),
                ("toolu_2", "matches for bar", None),
                ("toolu_3", "contents of src/lib.rs", None),
            ],
        )),
    );

    let mut buf = new_buf(120, 10);
    render_transcript(
        &s,
        Rect::new(0, 0, 120, 10),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    assert!(
        snap.contains("Searched for 2 patterns"),
        "mixed message did not trigger collapse: {snap:?}"
    );
    assert!(
        snap.contains("read 1 file"),
        "read counter missing from collapsed mixed message: {snap:?}"
    );
    // Whitespace text is hidden in the collapsed compact view.
    assert!(
        !snap.contains("\t"),
        "whitespace text leaked into collapsed view: {snap:?}"
    );
    // Per-tool cards must be suppressed.
    assert!(
        !snap.contains("Grep(pattern=foo)"),
        "Grep card leaked from mixed message: {snap:?}"
    );
}

#[test]
fn committed_leading_text_before_tool_uses_breaks_collapse() {
    // Non-empty narration before a tool is user-visible assistant
    // content and must not be folded into a compact tool summary.
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_mixed_text_and_tool_uses(
            "a1",
            "Important narration before reading files.",
            vec![
                ("toolu_1", "Grep", json!({ "pattern": "foo" })),
                ("toolu_2", "Read", json!({ "file_path": "src/lib.rs" })),
            ],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u1",
            vec![
                ("toolu_1", "matches for foo", None),
                ("toolu_2", "contents of src/lib.rs", None),
            ],
        )),
    );

    let mut buf = new_buf(120, 10);
    render_transcript(
        &s,
        Rect::new(0, 0, 120, 10),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    assert!(
        snap.contains("Important narration"),
        "leading assistant narration must remain visible: {snap:?}"
    );
    assert!(
        !snap.contains("Searched for 1 pattern, read 1 file"),
        "leading narration message was incorrectly folded into a summary: {snap:?}"
    );
}

#[test]
fn committed_trailing_text_after_tool_uses_breaks_collapse() {
    // A bundled turn with real answer text AFTER tool uses —
    // `[Read, Grep, "Here is what I found…"]` — must NOT collapse:
    // absorbing the trailing text would hide the user-visible output
    // behind a "Searched / read …" summary.
    use crate::state::{reducer, Action};
    let mut s = AppState::new();

    let mut content = Vec::new();
    content.push(AssistantContentBlock::ToolUse(AssistantToolUseBlock {
        id: "toolu_1".into(),
        name: "Read".into(),
        input: json!({ "file_path": "src/lib.rs" }),
        tool_call_content: None,
        raw_output: None,
        title: None,
        locations: None,
        status: None,
    }));
    content.push(AssistantContentBlock::ToolUse(AssistantToolUseBlock {
        id: "toolu_2".into(),
        name: "Grep".into(),
        input: json!({ "pattern": "foo" }),
        tool_call_content: None,
        raw_output: None,
        title: None,
        locations: None,
        status: None,
    }));
    content.push(AssistantContentBlock::Text(AssistantTextBlock {
        text: "Here is what I found in the code.".into(),
    }));
    let msg = Message::Assistant(AssistantMessage {
        uuid: "a1".into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content,
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    });
    reducer(&mut s, Action::Commit(msg));
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u1",
            vec![("toolu_1", "contents", None), ("toolu_2", "matches", None)],
        )),
    );

    let mut buf = new_buf(120, 16);
    render_transcript(
        &s,
        Rect::new(0, 0, 120, 16),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    assert!(
        snap.contains("Here is what I found"),
        "trailing answer text must stay visible: {snap:?}"
    );
    assert!(
        !snap.contains("Searched for 1 pattern, read 1 file"),
        "mixed-with-trailing-text message must not collapse: {snap:?}"
    );
}

/// Build an assistant message whose content leads with a Thinking
/// block followed by ToolUse blocks — the shape produced when
/// extended thinking is on and the model reasons before calling a tool.
fn assistant_thinking_and_tool_uses(
    uuid: &str,
    thinking: &str,
    tools: Vec<(&str, &str, serde_json::Value)>,
) -> Message {
    let mut content = vec![AssistantContentBlock::Thinking(AssistantThinkingBlock {
        thinking: thinking.into(),
        signature: None,
    })];
    for (id, name, input) in tools {
        content.push(AssistantContentBlock::ToolUse(AssistantToolUseBlock {
            id: id.into(),
            name: name.into(),
            input,
            tool_call_content: None,
            raw_output: None,
            title: None,
            locations: None,
            status: None,
        }));
    }
    Message::Assistant(AssistantMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content,
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}

/// Matches the residue left by `strip_hidden_tool_uses` on a
/// `[thinking, TaskCreate]`-shaped persisted message: thinking
/// survives, tool_use is removed.
fn assistant_thinking_only(uuid: &str, thinking: &str) -> Message {
    Message::Assistant(AssistantMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: vec![AssistantContentBlock::Thinking(AssistantThinkingBlock {
                thinking: thinking.into(),
                signature: None,
            })],
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}

#[test]
fn trailing_collapsible_tool_run_spans_results_and_meta_rows() {
    let rows = vec![
        assistant_text("a0", "visible boundary"),
        assistant_tool_uses("a1", vec![("t1", "Grep", json!({ "pattern": "first" }))]),
        user_tool_results("u1", vec![("t1", "matches", None)]),
        meta_user_reminder("u-meta", "runtime update"),
        assistant_tool_uses("a2", vec![("t2", "Read", json!({ "file_path": "a.rs" }))]),
        user_tool_results("u2", vec![("t2", "contents", None)]),
    ];

    assert_eq!(trailing_collapsible_tool_run_start(&rows, true), Some(1));
}

#[test]
fn trailing_collapsible_tool_run_respects_visible_thinking_boundary() {
    let rows = vec![
        assistant_tool_uses("a1", vec![("t1", "Grep", json!({ "pattern": "first" }))]),
        assistant_thinking_only("a-thinking", "visible inline reasoning"),
        assistant_tool_uses("a2", vec![("t2", "Read", json!({ "file_path": "a.rs" }))]),
    ];

    assert_eq!(trailing_collapsible_tool_run_start(&rows, true), Some(2));
    assert_eq!(trailing_collapsible_tool_run_start(&rows, false), Some(0));
}

#[test]
fn trailing_collapsible_tool_run_stops_at_non_collapsible_tail() {
    let rows = vec![
        assistant_tool_uses("a1", vec![("t1", "Grep", json!({ "pattern": "first" }))]),
        user_tool_results("u1", vec![("t1", "matches", None)]),
        assistant_tool_uses(
            "a2",
            vec![("t2", "Bash", json!({ "command": "cargo test" }))],
        ),
    ];

    assert_eq!(trailing_collapsible_tool_run_start(&rows, true), None);
}

#[test]
fn thinking_only_assistant_rows_are_transparent_to_collapse() {
    // Repro of the user-reported bug: on resume, persisted rows
    // shaped `[thinking, TaskUpdate]` become `[thinking]` only
    // after `strip_hidden_tool_uses`. If those residual thinking
    // rows broke the collapse run, the transcript rendered a
    // ladder of individual Read/Glob cards with visible "∴ Thinking…"
    // blocks between clusters (see the screenshot in the bug
    // report). The walk must treat them as transparent — same as
    // `isMeta: true` attachment reminders.
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    // Interleave collapsible rows with thinking-only residue, the
    // exact shape produced by a reasoning model that alternates
    // Read/Glob with TaskCreate/TaskUpdate progress updates.
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a1",
            vec![("t1", "Read", json!({ "file_path": "a.rs" }))],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_only("a2", "planning the next step")),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a3",
            vec![("t2", "Glob", json!({ "pattern": "src/**/*" }))],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_only(
            "a4",
            "another intermediate thought",
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a5",
            vec![("t3", "Read", json!({ "file_path": "b.rs" }))],
        )),
    );

    let mut buf = new_buf(140, 20);
    render_transcript(
        &s,
        Rect::new(0, 0, 140, 20),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    // All three collapsible rows must fold into one summary.
    assert!(
        snap.contains("read 2 files") || snap.contains("Read 2 files"),
        "thinking-only residue broke the Read cluster: {snap:?}"
    );
    assert!(
        snap.contains("searched for 1 pattern") || snap.contains("Searched for 1 pattern"),
        "thinking-only residue broke the Glob cluster: {snap:?}"
    );
    // The reasoning residue stays inside the collapsed tool run instead
    // of being promoted into a separate Reasoning group.
    assert!(!snap.contains("Reasoning ("), "{snap:?}");
    assert!(!snap.contains("planning the next step"), "{snap:?}");
    assert!(!snap.contains("another intermediate thought"), "{snap:?}");
    assert_eq!(
        snap.matches("Ctrl+O to expand").count(),
        1,
        "the collapsed tool summary should own the expansion hint: {snap:?}"
    );
    // And no per-tool card should have survived the collapse.
    assert!(
        !snap.contains("● Read") && !snap.contains("● Glob"),
        "per-tool card leaked despite successful collapse: {snap:?}"
    );
}

#[test]
fn committed_thinking_is_absorbed_into_collapse_run() {
    // Persisted assistant messages from reasoning-capable models
    // (gpt-5.x, extended-thinking Claude) always carry a `Thinking`
    // block alongside their tool_uses — see the shape
    // `rebon-core/src/query/` writes to disk. If `Thinking` were
    // a run-breaker, *every* resumed session would fail to collapse
    // because every assistant row would be rejected by the
    // tool_only predicate. Assert the opposite: a thinking-prefixed
    // tool_use message joins the collapse run like any other
    // tool-carrying row, its reasoning folded into the hidden body
    // that Ctrl+O surfaces on expansion.
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a1",
            vec![("toolu_1", "Read", json!({ "file_path": "src/a.rs" }))],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u1",
            vec![("toolu_1", "body of a.rs", None)],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_and_tool_uses(
            "a2",
            "I should now look at b.rs because …",
            vec![("toolu_2", "Read", json!({ "file_path": "src/b.rs" }))],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u2",
            vec![("toolu_2", "body of b.rs", None)],
        )),
    );

    let mut buf = new_buf(120, 20);
    render_transcript(
        &s,
        Rect::new(0, 0, 120, 20),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    // Collapse MUST fire — thinking is absorbed, not a run-breaker.
    assert!(
        snap.contains("Read 2 files") || snap.contains("read 2 files"),
        "thinking between reads broke the collapse — expected 'read 2 files' summary: {snap:?}"
    );
    // Per-tool card targets must be suppressed in compact view.
    assert!(
        !snap.contains("src/a.rs"),
        "first Read card leaked into collapsed view: {snap:?}"
    );
    assert!(
        !snap.contains("src/b.rs"),
        "second Read card leaked into collapsed view: {snap:?}"
    );
    // Thinking body does not surface in the summary line.
    assert!(
        !snap.contains("I should now look"),
        "thinking body leaked into compact summary: {snap:?}"
    );
}

#[test]
fn committed_collapsed_group_expands_under_normal_verbosity() {
    // The `(ctrl+o to expand)` hint is a contract: toggling
    // verbosity past Compact (Ctrl+O → Normal) must surface the
    // underlying per-tool cards the summary was hiding. Render the
    // same run once in Compact and once in Normal; only Compact
    // should carry the past-tense summary, and only Normal should
    // expose the tool call targets inline.
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a1",
            vec![
                ("toolu_1", "Grep", json!({ "pattern": "foo" })),
                ("toolu_2", "Grep", json!({ "pattern": "bar" })),
            ],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u1",
            vec![
                ("toolu_1", "matches for foo", None),
                ("toolu_2", "matches for bar", None),
            ],
        )),
    );

    // ── Compact: summary-only ─────────────────────────────────
    let mut buf_compact = new_buf(120, 10);
    render_transcript(
        &s,
        Rect::new(0, 0, 120, 10),
        &mut buf_compact,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let compact_snap = all_text(&buf_compact);
    assert!(
        compact_snap.contains("Searched for 2 patterns"),
        "compact mode missing past-tense summary: {compact_snap:?}"
    );
    assert!(
        !compact_snap.contains("pattern=foo"),
        "compact mode leaked per-tool card: {compact_snap:?}"
    );

    // ── Normal: expanded to per-tool cards ───────────────────
    let mut buf_normal = new_buf(120, 20);
    render_transcript(
        &s,
        Rect::new(0, 0, 120, 20),
        &mut buf_normal,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Normal,
        0,
        None,
    );
    let normal_snap = all_text(&buf_normal);
    assert!(
        !normal_snap.contains("Searched for 2 patterns"),
        "normal mode still showed collapsed summary: {normal_snap:?}"
    );
    assert!(
        normal_snap.contains("pattern=foo"),
        "normal mode did not expand first Grep card: {normal_snap:?}"
    );
    assert!(
        normal_snap.contains("pattern=bar"),
        "normal mode did not expand second Grep card: {normal_snap:?}"
    );
}

#[test]
fn committed_finalize_keeps_read_run_collapsed_and_preserves_trailing_text() {
    // Regression: finalize used to flatten `[Read, Read, "answer"]`
    // into one assistant row, which made the committed transcript
    // lose the compact collapsed summary and expand the tool cards.
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::StartToolUse {
            call_id: "toolu_1".into(),
            tool_name: "Read".into(),
            kind: ToolKind::Read,
            initial_status: ToolCallStatus::Completed,
            initial_title: None,
            raw_input: Some(HashMap::from([("file_path".into(), json!("src/a.rs"))])),
            content: None,
            locations: None,
            raw_output: None,
        },
    );
    reducer(
        &mut s,
        Action::StartToolUse {
            call_id: "toolu_2".into(),
            tool_name: "Read".into(),
            kind: ToolKind::Read,
            initial_status: ToolCallStatus::Completed,
            initial_title: None,
            raw_input: Some(HashMap::from([("file_path".into(), json!("src/b.rs"))])),
            content: None,
            locations: None,
            raw_output: None,
        },
    );
    reducer(
        &mut s,
        Action::SetStreamingText("Here is what I found.".into()),
    );
    reducer(
        &mut s,
        Action::FinalizeTurn {
            commit_uuid: "a-final".into(),
            commit_timestamp: "t".into(),
        },
    );

    let mut buf = new_buf(120, 12);
    render_transcript(
        &s,
        Rect::new(0, 0, 120, 12),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    assert!(
        snap.contains("Read 2 files") || snap.contains("read 2 files"),
        "finalized read run lost compact collapsed summary: {snap:?}"
    );
    assert!(
        snap.contains("Here is what I found."),
        "trailing assistant text disappeared after finalize: {snap:?}"
    );
    assert!(
        !snap.contains("src/a.rs"),
        "first per-tool Read card leaked after finalize: {snap:?}"
    );
    assert!(
        !snap.contains("src/b.rs"),
        "second per-tool Read card leaked after finalize: {snap:?}"
    );
}

#[test]
fn committed_collapsed_group_propagates_tool_result_errors() {
    // When any tool_result in the run has `is_error: true`, the
    // collapsed summary's gutter should switch to the error style.
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a1",
            vec![
                ("toolu_1", "Grep", json!({ "pattern": "foo" })),
                ("toolu_2", "Read", json!({ "file_path": "missing.rs" })),
            ],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u1",
            vec![
                ("toolu_1", "ok", Some(false)),
                ("toolu_2", "ENOENT", Some(true)),
            ],
        )),
    );

    // Paint with a theme where `system_error` carries a
    // distinguishing color so the gutter style is observable —
    // `RenderTheme::plain()` would zero every style and make the
    // assertion meaningless.
    let theme = RenderTheme {
        system_error: Style::default().fg(Color::Red),
        ..RenderTheme::plain()
    };
    let mut buf = new_buf(120, 10);
    render_transcript(
        &s,
        Rect::new(0, 0, 120, 10),
        &mut buf,
        &theme,
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    // Summary still produced.
    assert!(
        snap.contains("Searched for 1 pattern") || snap.contains("read 1 file"),
        "collapsed summary missing even on error: {snap:?}"
    );
    // The summary gutter `●` must carry the failed-tool color. We
    // scan every `●` in column 0 — the leading blank margin may
    // push the summary to y=1.
    let mut saw_red_dot = false;
    for y in 0..buf.area().height {
        let cell = &buf[(0, y)];
        if cell.symbol() == "●" && cell.style().fg == Some(Color::Red) {
            saw_red_dot = true;
            break;
        }
    }
    assert!(
        saw_red_dot,
        "expected failed-tool gutter style on collapsed summary: {snap:?}"
    );
}

/// Build a user message flagged `isMeta: true` with a single text
/// block — the shape `AttachmentInjected` persists via
/// `query.rs` (TODO item reminders, skill listings, etc.).
fn meta_user_reminder(uuid: &str, text: &str) -> Message {
    Message::User(UserMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: UserMessageInner {
            role: UserRole::User,
            content: vec![UserContentBlock::Text(UserTextBlock { text: text.into() })],
        },
        is_compact_summary: None,
        is_meta: Some(true),
        is_visible_in_transcript_only: None,
        image_paste_ids: None,
        plan_content: None,
    })
}

#[test]
fn resumed_transcript_post_replay_shape_collapses() {
    // The EXACT shape `replay_transcript_entries` leaves in the
    // committed rows: tool_result user messages are filtered out
    // (runner/mod.rs:3580 `tool_result_id().is_some() → skip`) and
    // hidden tools like TaskCreate/TaskUpdate are stripped from
    // each assistant (runner/mod.rs:3679 `strip_hidden_tool_uses`).
    // The result is a back-to-back sequence of
    // `Assistant [thinking, tool_use×N]` rows with nothing between
    // them. This is what a real resumed reasoning-model session
    // looks like — and exactly what the bug report showed
    // rendering as individual cards instead of a collapsed group.
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    // Match the exact resumed row shape from
    // `sess-18a70ed280d44c7c-0.jsonl` rows 4, 6, 8, 10 post-strip:
    // each carries `[thinking, collapsible_tool×N]` with no
    // tool_result user messages in between.
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_and_tool_uses(
            "a1",
            "rationale for reads and globs",
            vec![
                ("t1", "Read", json!({ "file_path": "a.rs" })),
                ("t2", "Glob", json!({ "pattern": "src/**/*" })),
                ("t3", "Glob", json!({ "pattern": "crates/**/*" })),
            ],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_and_tool_uses(
            "a2",
            "rationale for more reads",
            vec![
                ("t4", "Read", json!({ "file_path": "b.rs" })),
                ("t5", "Read", json!({ "file_path": "c.rs" })),
                ("t6", "Read", json!({ "file_path": "d.rs" })),
            ],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_and_tool_uses(
            "a3",
            "rationale for greps",
            vec![
                ("t7", "Grep", json!({ "pattern": "foo" })),
                ("t8", "Grep", json!({ "pattern": "bar" })),
            ],
        )),
    );

    let mut buf = new_buf(140, 20);
    render_transcript(
        &s,
        Rect::new(0, 0, 140, 20),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    // All 8 tool_uses across 3 thinking-prefixed rows must fold
    // into one summary — "searched for 4 patterns, read 4 files".
    assert!(
        snap.contains("searched for 4 patterns") || snap.contains("Searched for 4 patterns"),
        "post-replay rows didn't collapse Globs/Greps: {snap:?}"
    );
    assert!(
        snap.contains("read 4 files") || snap.contains("Read 4 files"),
        "post-replay rows didn't collapse Reads: {snap:?}"
    );
    // Reasoning remains owned by the collapsed run and stays hidden in compact mode.
    assert!(!snap.contains("Reasoning ("), "{snap:?}");
    assert!(!snap.contains("rationale for reads and globs"), "{snap:?}");
    assert!(!snap.contains("rationale for more reads"), "{snap:?}");
    assert!(!snap.contains("rationale for greps"), "{snap:?}");
    assert_eq!(
        snap.matches("Ctrl+O to expand").count(),
        1,
        "the collapsed tool summary should own the expansion hint: {snap:?}"
    );
    // Per-tool card inputs must not surface.
    assert!(
        !snap.contains("a.rs") && !snap.contains("b.rs") && !snap.contains("foo"),
        "per-tool cards leaked from collapsed group: {snap:?}"
    );
}

#[test]
fn resumed_transcript_shape_collapses_multi_tool_turns() {
    // Exact shape of what lands in `rows` after a session resume
    // of a reasoning-model transcript (see
    // `$HOME/.rebon/projects/.../sess-*.jsonl`): every assistant
    // turn is a single `[thinking, tool_use×N]` entry and every
    // user turn is a batched `[tool_result×N]`. Before the
    // `tool_only_assistant` fix, the thinking prefix made every
    // assistant row fail the collapse predicate — each persisted
    // tool_use rendered as its own card, producing the ladder of
    // `● Read (...)` / `● Glob (...)` cards the bug report showed.
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    // Turn 1: two Globs + two Reads bundled with a thinking
    // preamble, exactly as persisted.
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_and_tool_uses(
            "a1",
            "I need to locate the acp module and inspect its files.",
            vec![
                ("toolu_1", "Glob", json!({ "pattern": "src/acp/**/*" })),
                (
                    "toolu_2",
                    "Glob",
                    json!({ "pattern": "rebon/crates/rebon-acp/src/**/*" }),
                ),
                ("toolu_3", "Read", json!({ "file_path": "F:/x/Cargo.toml" })),
                ("toolu_4", "Read", json!({ "file_path": "F:/x/index.ts" })),
            ],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u1",
            vec![
                ("toolu_1", "ok", None),
                ("toolu_2", "ok", None),
                ("toolu_3", "ok", None),
                ("toolu_4", "ok", None),
            ],
        )),
    );
    // Turn 2: another thinking + tool_use bundle, still part of
    // the same read/search chain.
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_and_tool_uses(
            "a2",
            "Now I'll open the library entry.",
            vec![("toolu_5", "Read", json!({ "file_path": "F:/x/lib.rs" }))],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results("u2", vec![("toolu_5", "ok", None)])),
    );

    let mut buf = new_buf(140, 20);
    render_transcript(
        &s,
        Rect::new(0, 0, 140, 20),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    // The whole chain must fold into one summary — two Globs are
    // "Searched for 2 patterns", three Reads are "read 3 files".
    assert!(
        snap.contains("Searched for 2 patterns") || snap.contains("searched for 2 patterns"),
        "resumed transcript didn't collapse Globs: {snap:?}"
    );
    assert!(
        snap.contains("read 3 files") || snap.contains("Read 3 files"),
        "resumed transcript didn't collapse Reads: {snap:?}"
    );
    // Per-tool card inputs must be suppressed in compact view.
    assert!(
        !snap.contains("Cargo.toml"),
        "per-tool Read card leaked into compact collapsed view: {snap:?}"
    );
    assert!(
        !snap.contains("lib.rs"),
        "per-tool Read card leaked into compact collapsed view: {snap:?}"
    );
}

#[test]
fn committed_collapse_run_survives_meta_user_between_tools() {
    // The engine's attachment poller injects an `isMeta: true` user
    // message between every iteration (query.rs AttachmentInjected
    // handler) — a system-reminder the API sees but the TUI null-
    // renders. Before the fix these rows were treated as run-breakers
    // by `build_transcript_segments`, so a long read/search chain
    // fragmented into single-tool cards instead of collapsing. This
    // test asserts the meta row is transparent to the grouping logic.
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a1",
            vec![("toolu_1", "Read", json!({ "file_path": "src/a.rs" }))],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u1",
            vec![("toolu_1", "body of a.rs", None)],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(meta_user_reminder(
            "u-attach-1",
            "<system-reminder>…poller output…</system-reminder>",
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a2",
            vec![("toolu_2", "Glob", json!({ "pattern": "src/**/*.rs" }))],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u2",
            vec![("toolu_2", "match list", None)],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(meta_user_reminder(
            "u-attach-2",
            "<system-reminder>…poller output…</system-reminder>",
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a3",
            vec![("toolu_3", "Read", json!({ "file_path": "src/b.rs" }))],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u3",
            vec![("toolu_3", "body of b.rs", None)],
        )),
    );

    let mut buf = new_buf(140, 20);
    render_transcript(
        &s,
        Rect::new(0, 0, 140, 20),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_text(&buf);

    // The whole chain must fold into one past-tense summary.
    assert!(
        snap.contains("Read 2 files") || snap.contains("read 2 files"),
        "meta user broke the collapse run — expected 'read 2 files' summary: {snap:?}"
    );
    assert!(
        snap.contains("Searched for 1 pattern") || snap.contains("searched for 1 pattern"),
        "meta user broke the collapse run — expected 'searched for 1 pattern' in summary: {snap:?}"
    );
    // Per-tool cards must be suppressed.
    assert!(
        !snap.contains("src/a.rs"),
        "per-tool card leaked — meta-user walk regression: {snap:?}"
    );
    assert!(
        !snap.contains("src/b.rs"),
        "per-tool card leaked — meta-user walk regression: {snap:?}"
    );
    // Meta reminder body must never surface in the committed view.
    assert!(
        !snap.contains("system-reminder"),
        "meta user message leaked into rendered transcript: {snap:?}"
    );
    assert!(
        !snap.contains("[unknown message type]"),
        "meta user fell through to unknown-type fallback: {snap:?}"
    );
}

// ── row_heights cache wiring ─────────────────────────────────
//
// These tests pin the scroll-perf fix: `measure_message_height` now
// reads/writes `TranscriptMeasureCache.row_heights` keyed on
// (uuid, width, row_revision, add_margin, thinking_id). The behaviour
// we lock in here is the one that matters for very long transcripts:
//   * identical re-render does not grow the cache and yields the same
//     pixels — proving each row was a HIT, not a re-measure;
//   * appending a new row leaves existing entries intact (only the new
//     row's key is added);
//   * upserting a row writes a NEW row_revision key — the upserted
//     row misses while everyone else continues to hit;
//   * width changes coexist as distinct keys (no width collisions);
//   * verbosity changes nuke the cache via `prepare`.

fn render_into_at_scroll(
    state: &AppState,
    cache: &mut TranscriptMeasureCache,
    width: u16,
    height: u16,
    verbosity: ToolOutputVerbosity,
    scroll_offset: usize,
) -> (Buffer, TranscriptRenderResult) {
    let area = Rect::new(0, 0, width, height);
    let mut buf = new_buf(width, height);
    let result = render_transcript_cached_with_running_hints(
        state,
        area,
        &mut buf,
        &RenderTheme::plain(),
        scroll_offset,
        verbosity,
        0,
        None,
        cache,
        false,
        TranscriptRenderExtras::empty(),
    );
    (buf, result)
}

fn render_into(
    state: &AppState,
    cache: &mut TranscriptMeasureCache,
    width: u16,
    verbosity: ToolOutputVerbosity,
) -> Buffer {
    render_into_at_scroll(state, cache, width, 24, verbosity, 0).0
}

fn populated_state() -> AppState {
    let mut s = AppState::new();
    reducer(&mut s, Action::Commit(user("u1", "first question")));
    reducer(
        &mut s,
        Action::Commit(assistant_text("a1", "first answer goes here")),
    );
    reducer(&mut s, Action::Commit(user("u2", "second question")));
    reducer(
        &mut s,
        Action::Commit(assistant_text("a2", "second answer goes here")),
    );
    s
}

#[test]
fn repeated_render_with_same_state_hits_cache_for_every_row() {
    let s = populated_state();
    let mut cache = TranscriptMeasureCache::new();

    let buf_first = render_into(&s, &mut cache, 80, ToolOutputVerbosity::Compact);
    let len_after_first = cache.row_heights.len();
    assert!(
        len_after_first > 0,
        "first render should have populated row_heights"
    );
    assert_eq!(cache.layout_full_builds, 1);

    let buf_second = render_into(&s, &mut cache, 80, ToolOutputVerbosity::Compact);
    assert_eq!(
        cache.row_heights.len(),
        len_after_first,
        "second render with identical state must not add new entries — every row should hit cache"
    );
    assert_eq!(
        cache.layout_cache_hits, 1,
        "second identical render should reuse the virtual layout without walking segments"
    );
    assert_eq!(
        cache.layout_full_builds, 1,
        "identical render must not rebuild the virtual layout"
    );
    assert_eq!(
        all_text(&buf_first),
        all_text(&buf_second),
        "cached render must produce identical output"
    );
}

#[test]
fn appending_second_inline_thinking_rebuilds_cross_boundary_group() {
    let mut state = AppState::new();
    let mut cache = TranscriptMeasureCache::new();
    let extras = TranscriptRenderExtras {
        render_thinking_only_rows: true,
        ..TranscriptRenderExtras::empty()
    };
    reducer(
        &mut state,
        Action::Commit(assistant_thinking_only("a1", "first step\nfirst detail")),
    );

    let area = Rect::new(0, 0, 80, 10);
    let mut first_buf = new_buf(80, 10);
    render_transcript_cached_with_running_hints(
        &state,
        area,
        &mut first_buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        extras,
    );
    assert!(!all_text(&first_buf).contains("Reasoning (2 steps)"));
    assert_eq!(cache.layout_full_builds, 1);

    reducer(
        &mut state,
        Action::Commit(assistant_thinking_only("a2", "second step\nsecond detail")),
    );
    let mut second_buf = new_buf(80, 10);
    render_transcript_cached_with_running_hints(
        &state,
        area,
        &mut second_buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        extras,
    );
    let snap = all_text(&second_buf);

    assert!(snap.contains("Reasoning (2 steps)"), "{snap:?}");
    assert_eq!(snap.matches("Ctrl+O to expand").count(), 1, "{snap:?}");
    assert_eq!(cache.layout_incremental_appends, 0);
    assert_eq!(cache.layout_full_builds, 2);
}

#[test]
fn appending_a_row_preserves_existing_cache_entries() {
    let mut s = populated_state();
    let mut cache = TranscriptMeasureCache::new();

    render_into(&s, &mut cache, 80, ToolOutputVerbosity::Compact);
    let len_before = cache.row_heights.len();
    assert_eq!(cache.layout_full_builds, 1);

    reducer(&mut s, Action::Commit(user("u3", "third question")));
    render_into(&s, &mut cache, 80, ToolOutputVerbosity::Compact);

    assert_eq!(
        cache.row_heights.len(),
        len_before + 1,
        "appending one row should add exactly one cache entry — not invalidate prior rows"
    );
    assert_eq!(
        cache.layout_incremental_appends, 1,
        "append should extend the virtual layout instead of rebuilding all segments"
    );
    assert_eq!(
        cache.layout_full_builds, 1,
        "append fast path must avoid a second full virtual layout build"
    );
}

#[test]
fn upserting_a_row_only_invalidates_that_rows_entry() {
    let mut s = populated_state();
    let mut cache = TranscriptMeasureCache::new();

    render_into(&s, &mut cache, 80, ToolOutputVerbosity::Compact);
    let len_before = cache.row_heights.len();
    let row_rev_before_upsert = s.transcript.row_revision(1);
    let entries: Vec<_> = cache
        .row_heights
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    let a1_entry = entries
        .iter()
        .find(|(k, _)| k.uuid == "a1")
        .unwrap_or_else(|| panic!("no a1 entry in cache. entries={entries:?}"));
    assert_eq!(a1_entry.0.row_revision, row_rev_before_upsert);
    let cached_add_margin = a1_entry.0.add_margin;

    reducer(
        &mut s,
        Action::Commit(assistant_text("a1", "first answer, but edited")),
    );
    let row_rev_after_upsert = s.transcript.row_revision(1);
    assert_ne!(
        row_rev_before_upsert, row_rev_after_upsert,
        "upsert must bump the row's revision"
    );

    render_into(&s, &mut cache, 80, ToolOutputVerbosity::Compact);

    let after_entries: Vec<_> = cache
        .row_heights
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    assert_eq!(
        cache.row_heights.len(),
        len_before,
        "upsert should replace the old a1 entry (stale revision evicted); other rows keep hitting. entries={after_entries:?}"
    );
    let new_a1 = after_entries
        .iter()
        .find(|(k, _)| k.uuid == "a1" && k.row_revision == row_rev_after_upsert)
        .unwrap_or_else(|| {
            panic!("no a1 entry at new revision {row_rev_after_upsert}. entries={after_entries:?}")
        });
    assert_eq!(new_a1.0.add_margin, cached_add_margin);
}

#[test]
fn last_row_revision_update_reuses_virtual_layout_prefix() {
    let mut s = populated_state();
    let mut cache = TranscriptMeasureCache::new();

    render_into(&s, &mut cache, 80, ToolOutputVerbosity::Compact);
    let len_before = cache.row_heights.len();
    let row_rev_before_upsert = s.transcript.row_revision(3);

    reducer(
        &mut s,
        Action::Commit(assistant_text("a2", "second answer, but edited")),
    );
    let row_rev_after_upsert = s.transcript.row_revision(3);
    assert_ne!(row_rev_before_upsert, row_rev_after_upsert);

    render_into(&s, &mut cache, 80, ToolOutputVerbosity::Compact);

    assert_eq!(cache.row_heights.len(), len_before);
    assert_eq!(
        cache.layout_tail_updates, 1,
        "last-row upsert should remeasure the tail segment without rebuilding the whole virtual layout"
    );
    assert_eq!(
        cache.layout_full_builds, 1,
        "last-row upsert fast path must avoid a second full virtual layout build"
    );
}

#[test]
fn different_widths_coexist_as_distinct_cache_entries() {
    let s = populated_state();
    let mut cache = TranscriptMeasureCache::new();

    render_into(&s, &mut cache, 80, ToolOutputVerbosity::Compact);
    let len_at_80 = cache.row_heights.len();

    render_into(&s, &mut cache, 120, ToolOutputVerbosity::Compact);
    assert_eq!(
        cache.row_heights.len(),
        len_at_80 * 2,
        "rendering at a new width must produce a fresh key per row alongside the existing width-80 entries"
    );

    // Third render at the input width should hit the input
    // entries — no growth.
    render_into(&s, &mut cache, 80, ToolOutputVerbosity::Compact);
    assert_eq!(
        cache.row_heights.len(),
        len_at_80 * 2,
        "returning to width 80 must hit the original entries, not re-measure"
    );
}

#[test]
fn long_transcript_scroll_reuses_layout_and_clipped_segment_cache() {
    let mut s = AppState::new();
    reducer(&mut s, Action::Commit(user("u-long", "show a long answer")));
    let long_answer = (0..240)
        .map(|i| format!("line {i:03} with enough content to render"))
        .collect::<Vec<_>>()
        .join("\n");
    reducer(
        &mut s,
        Action::Commit(assistant_text("a-long", &long_answer)),
    );
    reducer(&mut s, Action::Commit(user("u-after", "after long answer")));

    let mut cache = TranscriptMeasureCache::new();
    let (_, first) =
        render_into_at_scroll(&s, &mut cache, 100, 12, ToolOutputVerbosity::Compact, 0);
    assert!(
        first.total_lines > 40,
        "test transcript should be scrollable"
    );
    assert_eq!(cache.layout_full_builds, 1);

    let (_, scrolled_one) =
        render_into_at_scroll(&s, &mut cache, 100, 12, ToolOutputVerbosity::Compact, 8);
    assert_eq!(scrolled_one.total_lines, first.total_lines);
    assert_eq!(
        cache.layout_cache_hits, 1,
        "scroll-only render should reuse the virtual layout"
    );
    assert_eq!(
        cache.clipped_segment_hits, 0,
        "first clipped render populates the scratch cache"
    );

    let (_, scrolled_two) =
        render_into_at_scroll(&s, &mut cache, 100, 12, ToolOutputVerbosity::Compact, 9);
    assert_eq!(scrolled_two.total_lines, first.total_lines);
    assert_eq!(
        cache.layout_cache_hits, 2,
        "subsequent scroll-only render should still reuse the virtual layout"
    );
    assert_eq!(
        cache.layout_full_builds, 1,
        "scroll-only renders must not rebuild the virtual layout"
    );
    assert_eq!(
        cache.clipped_segment_hits, 1,
        "scrolling within the same clipped segment should reuse the scratch render"
    );

    let area = Rect::new(0, 0, 100, 12);
    let mut buf = new_buf(100, 12);
    let animated_theme = RenderTheme {
        frame_time_ms: 1_250,
        ..RenderTheme::plain()
    };
    let scrolled_with_animation_clock = render_transcript_cached_with_running_hints(
        &s,
        area,
        &mut buf,
        &animated_theme,
        10,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        TranscriptRenderExtras::empty(),
    );
    assert_eq!(scrolled_with_animation_clock.total_lines, first.total_lines);
    assert_eq!(
        cache.layout_cache_hits, 3,
        "animation clock changes should not invalidate static transcript layout"
    );
    assert_eq!(
        cache.clipped_segment_hits, 2,
        "animation clock changes should not invalidate static clipped segment renders"
    );
}

fn thinking_tool_step(thinking: &str, tool_first: bool) -> Message {
    let Message::Assistant(mut step) =
        assistant_tool("margin-step", "tool-margin", "Bash", "cargo test")
    else {
        unreachable!()
    };
    step.message.content.insert(
        usize::from(tool_first),
        AssistantContentBlock::Thinking(AssistantThinkingBlock {
            thinking: thinking.into(),
            signature: None,
        }),
    );
    Message::Assistant(step)
}

#[test]
fn committed_thinking_margin_follows_first_and_later_block_contract() {
    for verbosity in [ToolOutputVerbosity::Compact, ToolOutputVerbosity::Normal] {
        for add_margin in [false, true] {
            for tool_first in [false, true] {
                let msg = thinking_tool_step("Planning verification", tool_first);
                let mut buf = new_buf(100, 20);
                let used = render_committed_assistant_tool_step(
                    &msg,
                    buf.area,
                    &mut buf,
                    &RenderTheme::plain(),
                    verbosity,
                    add_margin,
                    None,
                    TranscriptRenderExtras::empty(),
                )
                .unwrap();
                let rows = all_rows(&buf);
                let thinking_y = row_y_containing(&rows, "Planning verification");
                let tool_y = row_y_containing(&rows, "Bash");
                let (first, second) = if tool_first {
                    (tool_y, thinking_y)
                } else {
                    (thinking_y, tool_y)
                };
                assert_eq!(first, usize::from(add_margin), "{rows:?}");
                assert!(second > first + 1, "{rows:?}");
                assert!(rows[second - 1].is_empty(), "{rows:?}");
                assert!(!rows[second - 2].is_empty(), "{rows:?}");
                assert_eq!(
                    used as usize,
                    rows.iter().rposition(|row| !row.is_empty()).unwrap() + 1,
                    "{rows:?}"
                );
                assert_no_adjacent_blank_rows(&semantic_rows(&buf));
            }
        }
    }
}

#[test]
fn committed_thinking_margin_skips_empty_and_hidden_blocks() {
    for verbosity in [ToolOutputVerbosity::Compact, ToolOutputVerbosity::Normal] {
        for add_margin in [false, true] {
            for tool_first in [false, true] {
                for (text, latest) in [
                    ("", None),
                    (" \n\t ", None),
                    ("hidden reasoning", Some("no-thinking")),
                    ("hidden reasoning", Some(THINKING_GROUP_SUPPRESSION_ID)),
                    ("hidden reasoning", Some("another-message:0")),
                ] {
                    let msg = thinking_tool_step(text, tool_first);
                    let Message::Assistant(mut tool_only) = msg.clone() else {
                        unreachable!()
                    };
                    tool_only.message.content.remove(usize::from(tool_first));
                    let mut expected = new_buf(80, 20);
                    let expected_height = render_committed_assistant_tool_step(
                        &Message::Assistant(tool_only),
                        expected.area,
                        &mut expected,
                        &RenderTheme::plain(),
                        verbosity,
                        add_margin,
                        latest,
                        TranscriptRenderExtras::empty(),
                    );
                    let mut actual = new_buf(80, 20);
                    let actual_height = render_committed_assistant_tool_step(
                        &msg,
                        actual.area,
                        &mut actual,
                        &RenderTheme::plain(),
                        verbosity,
                        add_margin,
                        latest,
                        TranscriptRenderExtras::empty(),
                    );
                    assert_eq!(actual_height, expected_height, "{text:?}, {latest:?}");
                    assert_eq!(actual, expected, "{text:?}, {latest:?}");
                }
            }
        }
    }
}

#[test]
fn committed_thinking_margin_clipping_matches_height_and_paint() {
    let msg = thinking_tool_step(
        "Planning verification with wrapped text\n\nNext paragraph",
        false,
    );
    for verbosity in [ToolOutputVerbosity::Compact, ToolOutputVerbosity::Normal] {
        for add_margin in [false, true] {
            for width in [24, 100] {
                let mut full = new_buf(width + 4, 64);
                let area = Rect::new(2, 1, width, 60);
                let height = render_message_inner_with_context(
                    &msg,
                    area,
                    &mut full,
                    &RenderTheme::plain(),
                    verbosity,
                    add_margin,
                    Some("margin-step:0"),
                    true,
                    TranscriptRenderExtras::empty(),
                );
                assert_eq!(row_text(&full, 1).is_empty(), add_margin);
                let mut measure_buf = new_buf(width + 4, 64);
                let measured = measure_message_height(
                    std::slice::from_ref(&msg),
                    &[1],
                    0,
                    area,
                    &mut measure_buf,
                    &RenderTheme::plain(),
                    verbosity,
                    add_margin,
                    &mut TranscriptMeasureCache::new(),
                    Some("margin-step:0"),
                    TranscriptRenderExtras::empty(),
                );
                assert_eq!(height, measured);
                for clip in [0, 1, 2, 3, height - 1, height, height + 1] {
                    let mut actual = new_buf(width + 4, 64);
                    let clipped = Rect::new(2, 1, width, clip);
                    let used = render_message_inner_with_context(
                        &msg,
                        clipped,
                        &mut actual,
                        &RenderTheme::plain(),
                        verbosity,
                        add_margin,
                        Some("margin-step:0"),
                        true,
                        TranscriptRenderExtras::empty(),
                    );
                    assert_eq!(
                        used,
                        height.min(clip),
                        "{verbosity:?}, {add_margin}, {width}, {clip}"
                    );
                    for y in 0..actual.area.height {
                        for x in 0..actual.area.width {
                            if clipped.contains((x, y).into()) {
                                assert_eq!(
                                    actual[(x, y)],
                                    full[(x, y)],
                                    "{verbosity:?}, {add_margin}, {width}, {clip}, ({x}, {y})"
                                );
                            } else {
                                assert_eq!(actual[(x, y)].symbol(), " ");
                            }
                        }
                    }
                }
                let mut empty = new_buf(4, 4);
                assert_eq!(
                    render_message_inner_with_context(
                        &msg,
                        Rect::new(2, 1, 0, 3),
                        &mut empty,
                        &RenderTheme::plain(),
                        verbosity,
                        add_margin,
                        None,
                        true,
                        TranscriptRenderExtras::empty(),
                    ),
                    0
                );
                assert_eq!(empty, new_buf(4, 4));
            }
        }
    }
}

#[test]
fn committed_thinking_margin_keeps_gap_after_collapsed_group() {
    let mut state = AppState::new();
    reducer(
        &mut state,
        Action::Commit(assistant_tool_uses(
            "a1",
            vec![
                ("toolu_1", "Read", json!({ "file_path": "a.rs" })),
                ("toolu_2", "Read", json!({ "file_path": "b.rs" })),
            ],
        )),
    );
    reducer(
        &mut state,
        Action::Commit(user_tool_results(
            "u1",
            vec![("toolu_1", "a", None), ("toolu_2", "b", None)],
        )),
    );
    reducer(
        &mut state,
        Action::Commit(thinking_tool_step("Planning verification", false)),
    );
    let mut buf = new_buf(100, 20);
    render_transcript(
        &state,
        buf.area,
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let rows = semantic_rows(&buf);
    let group_y = rows
        .iter()
        .position(|row| row.to_lowercase().contains("read 2 files"))
        .unwrap_or_else(|| panic!("collapsed Read summary missing: {rows:?}"));
    let thinking_y = row_y_containing(&rows, "Planning verification");
    let tool_y = row_y_containing(&rows, "Bash");
    assert!(
        thinking_y > group_y + 1 && tool_y > thinking_y + 1,
        "{rows:?}"
    );
    assert!(rows[thinking_y - 1].is_empty(), "{rows:?}");
    assert!(rows[tool_y - 1].is_empty(), "{rows:?}");
    assert_no_adjacent_blank_rows(&rows);
}

#[test]
fn changing_verbosity_clears_the_cache_wholesale() {
    let s = populated_state();
    let mut cache = TranscriptMeasureCache::new();

    render_into(&s, &mut cache, 80, ToolOutputVerbosity::Compact);
    assert!(cache.row_heights.len() > 0);

    render_into(&s, &mut cache, 80, ToolOutputVerbosity::Verbose);
    // After verbosity flip, `prepare` nukes the cache; the verbose
    // render then re-populates from scratch. Net effect: len equals
    // the verbose render's own entry count — never the sum of both.
    let len_after_verbose = cache.row_heights.len();
    assert!(
        len_after_verbose > 0,
        "verbose render must populate cache after the flip-induced clear"
    );

    // Sanity: re-rendering at verbose hits cache (no growth).
    render_into(&s, &mut cache, 80, ToolOutputVerbosity::Verbose);
    assert_eq!(
        cache.row_heights.len(),
        len_after_verbose,
        "re-render at the same verbosity must not grow the cache"
    );
}

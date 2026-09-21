use super::super::*;
use super::common::*;
use crate::message::{
    AssistantMessage, AssistantMessageInner, AssistantRole, AssistantThinkingBlock,
    AssistantToolUseBlock, ToolResultContent, UserMessage, UserMessageInner, UserRole,
    UserToolResultBlock,
};
use crate::state::{reducer, Action, SealedPrefixFlushPolicy};
use crate::StreamingToolUse;
use ratatui::style::{Color, Modifier, Style};
use rebon_types::ToolCallLocation;
use serde_json::json;

fn streaming_tool(
    call_id: &str,
    tool_name: &str,
    kind: ToolKind,
    status: ToolCallStatus,
    raw_input: Vec<(&str, Value)>,
) -> StreamingToolUse {
    StreamingToolUse {
        call_id: call_id.into(),
        tool_name: tool_name.into(),
        kind,
        status,
        title: None,
        content: None,
        locations: None,
        raw_input: Some(
            raw_input
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        ),
        raw_output: None,
    }
}

fn streaming_agent_tool_with_history(mut tool: StreamingToolUse) -> StreamingToolUse {
    tool.raw_output.get_or_insert_with(HashMap::new).insert(
        "sub_agent_tool_calls".into(),
        json!([
            { "tool_use_id": "read-1", "name": "Read", "ok": true },
            { "tool_use_id": "grep-1", "name": "Grep", "ok": true },
            { "tool_use_id": "read-2", "name": "Read", "ok": true },
            { "tool_use_id": "read-failed", "name": "Read", "ok": false }
        ]),
    );
    tool
}

fn streaming_tool_with_content(
    mut tool: StreamingToolUse,
    content: Vec<ToolCallContent>,
) -> StreamingToolUse {
    tool.content = Some(content);
    tool
}

fn text_tool_content(text: &str) -> ToolCallContent {
    ToolCallContent::Content(rebon_types::RegularContent {
        content: ContentBlock::Text(rebon_types::TextContent {
            text: text.into(),
            annotations: None,
        }),
    })
}

fn streaming_tool_with_raw_output(
    mut tool: StreamingToolUse,
    raw_output: Vec<(&str, Value)>,
) -> StreamingToolUse {
    let mut merged = raw_output
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect::<HashMap<_, _>>();
    if let Some(existing) = tool.raw_output.take() {
        for (key, value) in existing {
            merged.entry(key).or_insert(value);
        }
    }
    tool.raw_output = Some(merged);
    tool
}

fn code_progress_content(kind: &str, message: &str, payload: Value) -> ToolCallContent {
    rebon_render::tool_output::tool_progress_update_content(
        &rebon_tools_core::ToolProgressUpdate::new(kind)
            .with_message(message)
            .with_payload(payload),
    )
    .expect("progress text")
    .remove(0)
}

fn run_code_sequence_overlay() -> StreamingOverlay {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "sequence",
        "run_code",
        ToolKind::Execute,
        ToolCallStatus::InProgress,
        vec![
            ("description", json!("Inspect and check files")),
            ("code", json!("const a = await tools.Read({file_path: 'a'});\nconsole.log('first console output');\nawait tools.Bash({command: 'check'});\nconsole.log('second console output');\nawait tools.Bash({command: 'verify'});\nreturn 'returned value';")),
        ],
    ));
    let events = [
        (
            "code_mode/program",
            "Running JavaScript (6 lines)",
            json!({"language": "javascript"}),
        ),
        (
            "code_mode/dispatch-start",
            "→ Read (a)",
            json!({"seq": 0, "tool": "Read"}),
        ),
        (
            "code_mode/dispatch",
            "← Read succeeded",
            json!({"seq": 0, "tool": "Read", "isError": false}),
        ),
        (
            "code_mode/dispatch-start",
            "→ Bash (check)",
            json!({"seq": 1, "tool": "Bash"}),
        ),
        (
            "code_mode/dispatch",
            "← Bash succeeded",
            json!({"seq": 1, "tool": "Bash", "isError": false}),
        ),
        (
            "code_mode/dispatch-start",
            "→ Bash (verify)",
            json!({"seq": 2, "tool": "Bash"}),
        ),
        (
            "code_mode/dispatch",
            "← Bash succeeded",
            json!({"seq": 2, "tool": "Bash", "isError": false}),
        ),
    ];
    for (kind, message, payload) in events {
        assert!(overlay.update_streaming_tool_use(
            "sequence",
            Some(ToolCallStatus::InProgress),
            None,
            Some(vec![code_progress_content(kind, message, payload.clone())]),
            None,
            Some(serde_json::from_value(payload).expect("progress payload map")),
        ));
    }
    overlay
}

fn render_code_sequence(
    overlay: &StreamingOverlay,
    verbosity: ToolOutputVerbosity,
    width: u16,
) -> String {
    let mut buf = new_buf(width, 160);
    render_streaming_overlay(
        overlay,
        Rect::new(0, 0, width, 160),
        &mut buf,
        &RenderTheme::plain(),
        verbosity,
    );
    all_text(&buf)
}

#[test]
fn run_code_completion_retains_multiple_calls_console_outputs_and_return_value() {
    let mut overlay = run_code_sequence_overlay();
    // The actual kernel returns a JSON string; use the engine's projection, not
    // a manually supplied final text block that can mask a missing result.
    let output = json!("first console output\nsecond console output\nreturned value");
    overlay.update_streaming_tool_use(
        "sequence",
        Some(ToolCallStatus::Completed),
        None,
        rebon_render::tool_result_update_content(&output),
        None,
        None,
    );
    for verbosity in [
        ToolOutputVerbosity::Compact,
        ToolOutputVerbosity::Normal,
        ToolOutputVerbosity::Verbose,
    ] {
        for width in [64, 120] {
            let snap = render_code_sequence(&overlay, verbosity, width);
            assert!(snap.contains("Completed · 3 calls"), "{snap}");
            assert!(snap.contains("Read ×1"), "{snap}");
            assert!(snap.contains("Bash ×2"), "{snap}");
            for output in [
                "first console output",
                "second console output",
                "returned value",
            ] {
                assert!(snap.contains(output), "{snap}");
            }
            assert_eq!(
                snap.contains("Running JavaScript"),
                verbosity == ToolOutputVerbosity::Verbose,
                "{snap}"
            );
            assert_eq!(
                snap.contains("→ Bash (check)"),
                verbosity == ToolOutputVerbosity::Verbose,
                "{snap}"
            );
            assert_eq!(
                snap.matches("Ctrl+O").count(),
                usize::from(verbosity != ToolOutputVerbosity::Verbose),
                "{snap}"
            );
        }
    }
}

#[test]
fn run_code_progress_only_completion_is_a_sequence_not_the_last_nested_success() {
    let mut overlay = run_code_sequence_overlay();
    overlay.update_streaming_tool_use(
        "sequence",
        Some(ToolCallStatus::Completed),
        None,
        None,
        None,
        None,
    );
    for verbosity in [
        ToolOutputVerbosity::Compact,
        ToolOutputVerbosity::Normal,
        ToolOutputVerbosity::Verbose,
    ] {
        let snap = render_code_sequence(&overlay, verbosity, 120);
        assert!(snap.contains("Completed · 3 calls"), "{snap}");
        assert!(snap.contains("Read ×1"), "{snap}");
        assert!(snap.contains("Bash ×2"), "{snap}");
        assert!(snap.contains("No final output received"), "{snap}");
        assert_eq!(
            snap.contains("← Bash succeeded"),
            verbosity == ToolOutputVerbosity::Verbose,
            "{snap}"
        );
    }
}

#[test]
fn run_code_final_multiblock_output_does_not_classify_text_as_progress() {
    let mut overlay = run_code_sequence_overlay();
    overlay.update_streaming_tool_use(
        "sequence",
        Some(ToolCallStatus::Completed),
        None,
        Some(vec![
            text_tool_content("first final block"),
            text_tool_content("← Bash succeeded"),
            text_tool_content("third final block"),
        ]),
        None,
        None,
    );
    for verbosity in [
        ToolOutputVerbosity::Compact,
        ToolOutputVerbosity::Normal,
        ToolOutputVerbosity::Verbose,
    ] {
        let snap = render_code_sequence(&overlay, verbosity, 120);
        for text in [
            "Completed · 3 calls",
            "first final block",
            "← Bash succeeded",
            "third final block",
        ] {
            assert!(snap.contains(text), "{snap}");
        }
        assert!(!snap.contains("No final output received"), "{snap}");
    }
}

#[test]
fn run_code_empty_completion_and_explicit_no_output_are_distinct() {
    for output in [None, Some(""), Some("(run_code completed with no output)")] {
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(streaming_tool(
            "empty",
            "run_code",
            ToolKind::Execute,
            ToolCallStatus::Completed,
            vec![
                ("description", json!("Pure computation")),
                ("code", json!("1 + 1;")),
            ],
        ));
        overlay.update_streaming_tool_use(
            "empty",
            None,
            None,
            output.and_then(|output| rebon_render::tool_result_update_content(&json!(output))),
            None,
            None,
        );
        for verbosity in [
            ToolOutputVerbosity::Compact,
            ToolOutputVerbosity::Normal,
            ToolOutputVerbosity::Verbose,
        ] {
            let snap = render_code_sequence(&overlay, verbosity, 120);
            assert!(snap.contains("Completed"), "{snap}");
            assert!(!snap.contains("calls"), "{snap}");
            assert_eq!(
                snap.contains("No final output received"),
                output.is_none_or(str::is_empty),
                "{snap}"
            );
            if output.is_some_and(|s| !s.is_empty()) {
                assert!(
                    snap.contains("(run_code completed with no output)"),
                    "{snap}"
                );
            }
        }
    }
}

#[test]
fn run_code_runtime_progress_and_failure_do_not_claim_sequence_completion() {
    for status in [ToolCallStatus::InProgress, ToolCallStatus::Failed] {
        let mut overlay = run_code_sequence_overlay();
        if status == ToolCallStatus::Failed {
            overlay.update_streaming_tool_use(
                "sequence",
                Some(status),
                None,
                Some(vec![text_tool_content(
                    "program failed: denied\nlogs before failure:\nfirst console output",
                )]),
                None,
                None,
            );
        }
        for verbosity in [
            ToolOutputVerbosity::Compact,
            ToolOutputVerbosity::Normal,
            ToolOutputVerbosity::Verbose,
        ] {
            let snap = render_code_sequence(&overlay, verbosity, 120);
            assert!(!snap.contains("Completed"), "{snap}");
            assert!(!snap.contains("No final output received"), "{snap}");
            if status == ToolCallStatus::InProgress {
                assert!(snap.contains("Running JavaScript"), "{snap}");
            } else {
                assert!(snap.contains("program failed: denied"), "{snap}");
                assert_eq!(
                    snap.contains("logs before failure:"),
                    verbosity == ToolOutputVerbosity::Verbose,
                    "{snap}"
                );
            }
            assert_eq!(
                snap.matches("Ctrl+O").count(),
                usize::from(verbosity != ToolOutputVerbosity::Verbose),
                "{snap}"
            );
        }
    }
}

#[test]
fn run_code_completed_preview_is_bounded_and_expansion_preserves_every_block() {
    let mut overlay = run_code_sequence_overlay();
    let mut output = vec![text_tool_content("first console output")];
    output.extend(
        (0..40).map(|index| text_tool_content(&format!("console {index}: {}", "界".repeat(100)))),
    );
    output.push(text_tool_content("returned value"));
    overlay.update_streaming_tool_use(
        "sequence",
        Some(ToolCallStatus::Completed),
        None,
        Some(output),
        None,
        None,
    );
    for width in [32, 64, 120] {
        for verbosity in [ToolOutputVerbosity::Compact, ToolOutputVerbosity::Normal] {
            let snap = render_code_sequence(&overlay, verbosity, width);
            assert!(snap.contains("Completed · 3 calls"), "{snap}");
            assert!(snap.contains("first console output"), "{snap}");
            assert!(snap.contains("returned value"), "{snap}");
            assert!(snap.contains("… +"), "{snap}");
            assert_eq!(snap.matches("Ctrl+O").count(), 1, "{snap}");
            // The header can wrap independently; bound the body to one summary,
            // four output rows, one omission row and one expansion affordance.
            assert!(
                snap.lines()
                    .skip_while(|line| !line.contains("⎿"))
                    .filter(|line| !line.trim().is_empty())
                    .count()
                    <= TOOL_PREVIEW_MAX_LINES + 3,
                "{snap}"
            );
        }
    }
    let expanded = render_code_sequence(&overlay, ToolOutputVerbosity::Verbose, 240);
    for index in 0..40 {
        assert!(
            expanded.contains(&format!("console {index}:")),
            "missing console {index}"
        );
    }
    for text in [
        "→ Read (a)",
        "→ Bash (check)",
        "→ Bash (verify)",
        "first console output",
        "returned value",
        "JavaScript:",
    ] {
        assert!(expanded.contains(text), "missing {text}");
    }
    assert!(!expanded.contains("Ctrl+O"));
}

#[test]
fn run_code_source_is_expandable_without_obscuring_status_or_output() {
    let code = "const first = await tools.TaskList();\nreturn first;";
    for status in [
        ToolCallStatus::InProgress,
        ToolCallStatus::Completed,
        ToolCallStatus::Failed,
    ] {
        for verbosity in [
            ToolOutputVerbosity::Compact,
            ToolOutputVerbosity::Normal,
            ToolOutputVerbosity::Verbose,
        ] {
            for width in [48, 120] {
                let output = if status == ToolCallStatus::Failed {
                    "program failed: denied"
                } else {
                    "→ TaskList"
                };
                let tool = streaming_tool_with_content(
                    streaming_tool(
                        "code-1",
                        "run_code",
                        ToolKind::Execute,
                        status,
                        vec![
                            ("description", json!("Inspect current tasks")),
                            ("code", json!(code)),
                        ],
                    ),
                    vec![text_tool_content(output)],
                );
                let mut overlay = StreamingOverlay::new();
                overlay.upsert_streaming_tool_use(tool);
                let mut buf = new_buf(width, 30);
                render_streaming_overlay(
                    &overlay,
                    Rect::new(0, 0, width, 30),
                    &mut buf,
                    &RenderTheme::plain(),
                    verbosity,
                );
                let snap = (0..30)
                    .map(|y| row_text(&buf, y))
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(snap.contains("Inspect current tasks"), "{snap}");
                assert!(snap.contains(output), "{snap}");
                if verbosity == ToolOutputVerbosity::Verbose {
                    assert!(
                        snap.contains("const first = await tools.TaskList();"),
                        "{snap}"
                    );
                    assert!(snap.contains("return first;"), "{snap}");
                    assert!(!snap.to_ascii_lowercase().contains("ctrl+o"), "{snap}");
                    assert_eq!(snap.matches("const first").count(), 1, "{snap}");
                } else {
                    assert!(!snap.contains("const first"), "{snap}");
                    assert!(snap.to_ascii_lowercase().contains("ctrl+o"), "{snap}");
                }
            }
        }
    }
}

#[test]
fn run_code_completion_previews_final_output_not_early_progress() {
    for verbosity in [
        ToolOutputVerbosity::Compact,
        ToolOutputVerbosity::Normal,
        ToolOutputVerbosity::Verbose,
    ] {
        for width in [48, 120] {
            let mut overlay = StreamingOverlay::new();
            overlay.upsert_streaming_tool_use(streaming_tool_with_content(
                streaming_tool(
                    "code-final",
                    "run_code",
                    ToolKind::Execute,
                    ToolCallStatus::InProgress,
                    vec![
                        ("description", json!("Inspect tasks")),
                        ("code", json!("return await tools.TaskList();")),
                    ],
                ),
                vec![
                    code_progress_content(
                        "code_mode/program",
                        "Running JavaScript (1 line)",
                        json!({"language": "javascript"}),
                    ),
                    code_progress_content(
                        "code_mode/dispatch-start",
                        "→ Read (a.txt)",
                        json!({"seq": 0, "tool": "Read"}),
                    ),
                    code_progress_content(
                        "code_mode/dispatch",
                        "← Read",
                        json!({"seq": 0, "tool": "Read", "isError": false}),
                    ),
                    code_progress_content(
                        "code_mode/dispatch-start",
                        "→ TaskList",
                        json!({"seq": 1, "tool": "TaskList"}),
                    ),
                    code_progress_content(
                        "code_mode/dispatch",
                        "← TaskList",
                        json!({"seq": 1, "tool": "TaskList", "isError": false}),
                    ),
                ],
            ));
            let mut cache = StreamingOverlayRenderCache::new();
            let mut buf = new_buf(width, 30);
            render_streaming_overlay_cached(
                &overlay,
                Rect::new(0, 0, width, 30),
                &mut buf,
                &RenderTheme::plain(),
                verbosity,
                &mut cache,
                TranscriptRenderExtras::empty(),
            );
            assert!(overlay.update_streaming_tool_use(
                "code-final",
                Some(ToolCallStatus::Completed),
                None,
                Some(vec![text_tool_content("Final result: 3 tasks")]),
                None,
                None
            ));
            buf.reset();
            render_streaming_overlay_cached(
                &overlay,
                Rect::new(0, 0, width, 30),
                &mut buf,
                &RenderTheme::plain(),
                verbosity,
                &mut cache,
                TranscriptRenderExtras::empty(),
            );
            let snap = all_text(&buf);
            assert!(snap.contains("Run sequence (Inspect tasks)"), "{snap}");
            assert!(snap.contains("Final result: 3 tasks"), "{snap}");
            assert_eq!(
                snap.contains("Running JavaScript"),
                verbosity == ToolOutputVerbosity::Verbose,
                "{snap}"
            );
            assert_eq!(
                snap.contains("return await tools.TaskList();"),
                verbosity == ToolOutputVerbosity::Verbose,
                "{snap}"
            );
            assert!(snap.matches("Ctrl+O").count() <= 1, "{snap}");
            let crate::StreamingContentBlock::ToolUse(tool) = &overlay.blocks[0] else {
                panic!("expected tool")
            };
            assert_eq!(tool.tool_name, "run_code");
            assert_eq!(tool.content.as_ref().expect("content preserved").len(), 6);
        }
    }
}

#[test]
fn run_code_long_output_has_only_one_expansion_hint() {
    for status in [
        ToolCallStatus::InProgress,
        ToolCallStatus::Completed,
        ToolCallStatus::Failed,
    ] {
        for verbosity in [
            ToolOutputVerbosity::Compact,
            ToolOutputVerbosity::Normal,
            ToolOutputVerbosity::Verbose,
        ] {
            for suppress in [false, true] {
                let output = (1..=6)
                    .map(|i| format!("output {i}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let tool = streaming_tool_with_content(
                    streaming_tool(
                        "code-long",
                        "run_code",
                        ToolKind::Execute,
                        status,
                        vec![
                            ("description", json!("Inspect")),
                            ("code", json!("console.log(1);")),
                        ],
                    ),
                    vec![text_tool_content(&output)],
                );
                let mut buf = new_buf(120, 30);
                render_streaming_tool_use_with_content(
                    &tool,
                    None,
                    Rect::new(0, 0, 120, 30),
                    &mut buf,
                    &RenderTheme::plain(),
                    verbosity,
                    TranscriptRenderExtras::empty(),
                    suppress,
                );
                let snap = all_text(&buf);
                assert_eq!(
                    snap.matches("Ctrl+O").count(),
                    usize::from(!suppress && verbosity != ToolOutputVerbosity::Verbose),
                    "{status:?} {verbosity:?}: {snap}"
                );
            }
        }
    }
}

#[test]
fn run_code_missing_source_does_not_offer_an_empty_expansion() {
    for input in [vec![], vec![("code", json!(" \n"))]] {
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(streaming_tool(
            "code-1",
            "run_code",
            ToolKind::Execute,
            ToolCallStatus::InProgress,
            input,
        ));
        let mut buf = new_buf(80, 10);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 80, 10),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        let snap = (0..10)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!snap.to_ascii_lowercase().contains("ctrl+o"), "{snap}");
        assert!(!snap.contains("JavaScript:"), "{snap}");
    }
}

#[test]
fn run_code_failure_collapses_logs_into_a_header_expansion() {
    let error = "program failed: runtime: permission denied\nlogs before failure:\nlarge logged payload\n  at script.js:9";
    for verbosity in [
        ToolOutputVerbosity::Compact,
        ToolOutputVerbosity::Normal,
        ToolOutputVerbosity::Verbose,
    ] {
        let tool = streaming_tool_with_content(
            streaming_tool(
                "code-error",
                "run_code",
                ToolKind::Execute,
                ToolCallStatus::Failed,
                vec![
                    ("description", json!("Inspect tasks")),
                    ("code", json!("await tools.TaskList();")),
                ],
            ),
            vec![text_tool_content(error)],
        );
        let mut buf = new_buf(120, 20);
        let mut theme = RenderTheme::plain();
        theme.system_error = Style::default().fg(Color::Red);
        render_streaming_tool_use(
            &tool,
            Rect::new(0, 0, 120, 20),
            &mut buf,
            &theme,
            verbosity,
            TranscriptRenderExtras::empty(),
        );
        let header = row_text(&buf, 0);
        let snap = (0..20)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            header.contains("Run sequence (Inspect tasks) failed"),
            "{snap}"
        );
        assert!(
            snap.contains("program failed: runtime: permission denied"),
            "{snap}"
        );
        assert!(
            (0..GUTTER).any(|x| buf[(x, 0)].fg == Color::Red),
            "failed gutter must be red"
        );
        if verbosity == ToolOutputVerbosity::Verbose {
            assert!(snap.contains("large logged payload"), "{snap}");
            assert!(snap.contains("at script.js:9"), "{snap}");
            assert!(snap.contains("await tools.TaskList();"), "{snap}");
        } else {
            assert!(header.contains("Ctrl+O to expand"), "{snap}");
            assert!(!snap.contains("logs before failure"), "{snap}");
            assert!(!snap.contains("large logged payload"), "{snap}");
            assert!(!snap.contains("script.js:9"), "{snap}");
            assert!(!snap.contains("await tools"), "{snap}");
            assert_eq!(
                (0..20)
                    .filter(|y| !row_text(&buf, *y).trim().is_empty())
                    .count(),
                2,
                "{snap}"
            );
        }
    }
}

#[test]
fn run_code_failure_bounds_a_single_long_reason_at_terminal_width() {
    let tool = streaming_tool_with_content(
        streaming_tool(
            "code-error",
            "run_code",
            ToolKind::Execute,
            ToolCallStatus::Failed,
            vec![("description", json!("Inspect"))],
        ),
        vec![text_tool_content(&format!(
            "program failed: {}\nlogs before failure:\nsecret payload",
            "错误".repeat(400)
        ))],
    );
    let mut buf = new_buf(48, 20);
    render_streaming_tool_use(
        &tool,
        Rect::new(0, 0, 48, 20),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
        TranscriptRenderExtras::empty(),
    );
    let rows: Vec<_> = (0..20)
        .map(|y| row_text(&buf, y))
        .filter(|row| !row.trim().is_empty())
        .collect();
    assert!(rows.len() <= 3, "{rows:?}");
    assert!(rows.last().unwrap().contains('…'), "{rows:?}");
    assert!(!rows.join("\n").contains("secret payload"), "{rows:?}");
}

#[test]
fn run_code_failure_uses_final_error_instead_of_progress_and_respects_hint_suppression() {
    for raw_error in [false, true] {
        for suppress_hints in [false, true] {
            let error = "permission denied\nlogs before failure:\nlarge payload";
            let mut content = vec![
                text_tool_content("Running JavaScript (1 line)"),
                text_tool_content("→ Read (a.txt)"),
            ];
            if !raw_error {
                content.push(text_tool_content(error));
            }
            let mut tool = streaming_tool_with_content(
                streaming_tool(
                    "code-error",
                    "run_code",
                    ToolKind::Execute,
                    ToolCallStatus::Failed,
                    vec![
                        ("description", json!("Inspect tasks")),
                        ("code", json!("await tools.Read({file_path: 'a.txt'});")),
                    ],
                ),
                content,
            );
            if raw_error {
                tool = streaming_tool_with_raw_output(
                    tool,
                    vec![
                        ("error", json!(error)),
                        ("stderr", json!(error)),
                        ("stdout", json!("")),
                    ],
                );
            }
            let mut buf = new_buf(120, 12);
            render_streaming_tool_use_with_content(
                &tool,
                None,
                Rect::new(0, 0, 120, 12),
                &mut buf,
                &RenderTheme::plain(),
                ToolOutputVerbosity::Compact,
                TranscriptRenderExtras::empty(),
                suppress_hints,
            );
            let snap = (0..12)
                .map(|y| row_text(&buf, y))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(snap.contains("permission denied"), "{snap}");
            assert_eq!(snap.contains("Ctrl+O to expand"), !suppress_hints, "{snap}");
            assert!(!snap.contains("Running JavaScript"), "{snap}");
            assert!(!snap.contains("→ Read"), "{snap}");
            assert!(!snap.contains("large payload"), "{snap}");
            assert!(!snap.contains("await tools"), "{snap}");
        }
    }
}

fn workflow_test_tool() -> StreamingToolUse {
    streaming_tool_with_raw_output(
        streaming_tool(
            "workflow-1",
            "Workflow",
            ToolKind::Other,
            ToolCallStatus::InProgress,
            vec![("script", json!("export const meta = { name: 'hidden' };"))],
        ),
        vec![
            ("status", json!("running")),
            (
                "workflowProgress",
                json!({
                    "runId": "wf_test",
                    "workflowName": "markdown-previewer",
                    "summary": "Preview markdown",
                    "entries": [
                        { "sequence": 1, "entry": { "type": "phase", "title": "design", "state": "start" } },
                        { "sequence": 2, "entry": { "type": "agent", "index": 1, "state": "start", "phaseTitle": "design", "label": "ui-design", "tokens": 0, "toolCalls": 0 } },
                        { "sequence": 3, "entry": { "type": "agent", "index": 1, "state": "completed", "phaseTitle": "design", "label": "ui-design", "tokens": 1200, "toolCalls": 1, "toolCallDetails": [{ "name": "Read", "input": { "file_path": "src/ui.rs" }, "ok": true }], "durationMs": 1200 } },
                        { "sequence": 4, "entry": { "type": "phase", "title": "review", "state": "start" } },
                        { "sequence": 5, "entry": { "type": "agent", "index": 2, "state": "error", "phaseTitle": "review", "label": "security-review", "tokens": 0, "toolCalls": 0, "error": "1 finding" } },
                        { "sequence": 6, "entry": { "type": "log", "message": "narrator note" } }
                    ]
                }),
            ),
        ],
    )
}

fn render_workflow_tool_snapshot(verbosity: ToolOutputVerbosity, height: u16) -> String {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(workflow_test_tool());
    let mut buf = new_buf(120, height);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, height),
        &mut buf,
        &RenderTheme::plain(),
        verbosity,
    );
    (0..height)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}

fn auto_mode_snapshot(
    tool: StreamingToolUse,
    allowed: &[(&str, rebon_types::AutoModeAllowSource)],
    height: u16,
) -> String {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(tool);
    let allowed: std::collections::HashMap<String, rebon_types::AutoModeAllowSource> = allowed
        .iter()
        .map(|(id, source)| ((*id).to_string(), *source))
        .collect();
    let mut cache = StreamingOverlayRenderCache::new();
    let mut buf = new_buf(80, height);
    render_streaming_overlay_cached(
        &overlay,
        Rect::new(0, 0, 80, height),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
        &mut cache,
        TranscriptRenderExtras {
            auto_mode_allowed_tool_ids: &allowed,
            ..TranscriptRenderExtras::empty()
        },
    );
    (0..height)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The note is per-call, and it has to survive the card shapes that return
/// before the generic body block — an Edit row paints its own diff card.
#[test]
fn auto_mode_allowed_cards_say_so_and_others_stay_silent() {
    use rebon_types::AutoModeAllowSource as Source;
    let bash = || {
        streaming_tool(
            "bash-1",
            "Bash",
            ToolKind::Execute,
            ToolCallStatus::InProgress,
            vec![("command", json!("git status"))],
        )
    };
    let edit = || {
        streaming_tool(
            "edit-1",
            "Edit",
            ToolKind::Edit,
            ToolCallStatus::InProgress,
            vec![("file_path", json!("src/main.rs"))],
        )
    };

    let announced = auto_mode_snapshot(bash(), &[("bash-1", Source::Classifier)], 4);
    assert!(
        announced.contains("\u{23bf} Allowed by auto mode classifier"),
        "{announced:?}"
    );

    let unannounced = auto_mode_snapshot(bash(), &[], 4);
    assert!(!unannounced.contains("Allowed by"), "{unannounced:?}");

    let other_call = auto_mode_snapshot(bash(), &[("some-other-call", Source::Classifier)], 4);
    assert!(!other_call.contains("Allowed by"), "{other_call:?}");

    let announced_edit = auto_mode_snapshot(edit(), &[("edit-1", Source::Classifier)], 4);
    assert!(
        announced_edit.contains("Allowed by auto mode classifier"),
        "{announced_edit:?}"
    );
}

/// Each source gets its own wording, and the one the *user* approved must not
/// be credited to auto mode. A pre-source transcript keeps the old note.
#[test]
fn the_note_names_which_part_of_the_gate_allowed_the_call() {
    use rebon_types::AutoModeAllowSource as Source;
    let bash = || {
        streaming_tool(
            "bash-1",
            "Bash",
            ToolKind::Execute,
            ToolCallStatus::InProgress,
            vec![("command", json!("git status"))],
        )
    };

    for (source, expected) in [
        (Source::Classifier, "Allowed by auto mode classifier"),
        (
            Source::CachedVerdict,
            "Allowed by a cached auto mode verdict",
        ),
        (Source::UserExemption, "Allowed by your earlier approval"),
        (Source::Unspecified, "Auto allowed by rebon's auto mode"),
    ] {
        let snap = auto_mode_snapshot(bash(), &[("bash-1", source)], 4);
        assert!(snap.contains(expected), "{source:?}: {snap:?}");
    }

    let by_user = auto_mode_snapshot(bash(), &[("bash-1", Source::UserExemption)], 4);
    assert!(!by_user.contains("auto mode classifier"), "{by_user:?}");
}

#[test]
fn tool_footer_orders_output_timeout_and_allowance_across_states() {
    let allowed = HashMap::from([(
        "footer-1".to_string(),
        rebon_types::AutoModeAllowSource::Classifier,
    )]);
    for name in ["Bash", "PowerShell", "ShellOutput"] {
        for status in [
            ToolCallStatus::InProgress,
            ToolCallStatus::Completed,
            ToolCallStatus::Failed,
        ] {
            for verbosity in [
                ToolOutputVerbosity::Compact,
                ToolOutputVerbosity::Normal,
                ToolOutputVerbosity::Verbose,
            ] {
                let tool = streaming_tool_with_content(
                    streaming_tool(
                        "footer-1",
                        name,
                        ToolKind::Execute,
                        status,
                        vec![("command", json!("pwd")), ("timeout", json!(60000))],
                    ),
                    vec![text_tool_content("command output")],
                );
                let mut buf = new_buf(100, 20);
                render_streaming_tool_use(
                    &tool,
                    Rect::new(0, 0, 100, 20),
                    &mut buf,
                    &RenderTheme::plain(),
                    verbosity,
                    TranscriptRenderExtras {
                        auto_mode_allowed_tool_ids: &allowed,
                        ..TranscriptRenderExtras::empty()
                    },
                );
                let snap = all_text(&buf);
                let output = snap.find("command output").expect(&snap);
                let timeout = snap.find("Timeout: 60s").expect(&snap);
                let allowance = snap.find("Allowed by auto mode classifier").expect(&snap);
                assert!(
                    output < timeout && timeout < allowance,
                    "{name} {status:?} {verbosity:?}: {snap}"
                );
                assert!(
                    snap.trim_end().ends_with("Allowed by auto mode classifier"),
                    "{snap}"
                );
            }
        }
    }
}

#[test]
fn tool_footer_only_shows_configured_numeric_timeouts() {
    for (timeout, expected) in [
        (None, None),
        (Some(Value::Null), None),
        (Some(json!(-1)), None),
        (Some(json!("60000")), None),
        (Some(json!(25)), Some("Timeout: 25ms")),
        (Some(json!(1500)), Some("Timeout: 1.5s")),
        (Some(json!(60000)), Some("Timeout: 60s")),
    ] {
        let mut input = vec![("command", json!("pwd"))];
        if let Some(timeout) = timeout {
            input.push(("timeout", timeout));
        }
        let tool = streaming_tool_with_content(
            streaming_tool(
                "footer-1",
                "Bash",
                ToolKind::Execute,
                ToolCallStatus::Completed,
                input,
            ),
            vec![text_tool_content("command output")],
        );
        let snap = auto_mode_snapshot(tool, &[], 6);
        if let Some(expected) = expected {
            assert!(snap.contains(expected), "{snap}");
            assert!(snap.find("command output") < snap.find(expected), "{snap}");
        } else {
            assert!(!snap.contains("Timeout:"), "{snap}");
        }
        assert!(!snap.contains("Allowed by"), "{snap}");
    }
}

#[test]
fn tool_footer_never_displaces_output_in_a_short_viewport() {
    let tool = streaming_tool_with_content(
        streaming_tool(
            "footer-1",
            "Bash",
            ToolKind::Execute,
            ToolCallStatus::Completed,
            vec![("command", json!("pwd")), ("timeout", json!(60000))],
        ),
        vec![text_tool_content("command output")],
    );
    let allowed = HashMap::from([(
        "footer-1".to_string(),
        rebon_types::AutoModeAllowSource::Classifier,
    )]);
    for height in [2, 3, 4] {
        let mut buf = new_buf(100, height);
        let consumed = render_streaming_tool_use(
            &tool,
            Rect::new(0, 0, 100, height),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
            TranscriptRenderExtras {
                auto_mode_allowed_tool_ids: &allowed,
                ..TranscriptRenderExtras::empty()
            },
        );
        let snap = all_text(&buf);
        assert!(snap.contains("command output"), "{snap}");
        assert_eq!(snap.contains("Timeout: 60s"), height >= 3, "{snap}");
        assert_eq!(snap.contains("Allowed by"), height >= 4, "{snap}");
        assert_eq!(consumed, height);
    }
}

#[test]
fn tool_footer_allowance_follows_edit_and_workflow_bodies() {
    let edit = streaming_tool(
        "edit-1",
        "Edit",
        ToolKind::Edit,
        ToolCallStatus::Failed,
        vec![("file_path", json!("src/main.rs"))],
    );
    for (tool, id, body) in [
        (edit, "edit-1", "Edit failed"),
        (workflow_test_tool(), "workflow-1", "design"),
    ] {
        let snap = auto_mode_snapshot(
            tool,
            &[(id, rebon_types::AutoModeAllowSource::Classifier)],
            40,
        );
        let output = snap.find(body).expect(&snap);
        let allowance = snap.find("Allowed by auto mode classifier").expect(&snap);
        assert!(output < allowance, "{snap}");
        assert!(
            snap.trim_end().ends_with("Allowed by auto mode classifier"),
            "{snap}"
        );
    }
}

#[test]
fn tool_footer_survives_empty_output() {
    let tool = streaming_tool(
        "empty-1",
        "Bash",
        ToolKind::Execute,
        ToolCallStatus::InProgress,
        vec![("command", json!("pwd")), ("timeout", json!(60000))],
    );
    let snap = auto_mode_snapshot(
        tool,
        &[("empty-1", rebon_types::AutoModeAllowSource::Classifier)],
        4,
    );
    let timeout = snap.find("Timeout: 60s").expect(&snap);
    let allowance = snap.find("Allowed by auto mode classifier").expect(&snap);
    assert!(timeout < allowance, "{snap}");
    let workflow = streaming_tool(
        "empty-1",
        "Workflow",
        ToolKind::Other,
        ToolCallStatus::InProgress,
        vec![],
    );
    let snap = auto_mode_snapshot(
        workflow,
        &[("empty-1", rebon_types::AutoModeAllowSource::Classifier)],
        4,
    );
    assert!(snap.contains("Allowed by auto mode classifier"), "{snap}");
}

#[test]
fn tool_footer_fits_inside_live_workflow_row_budget() {
    let allowed = HashMap::from([(
        "workflow-1".to_string(),
        rebon_types::AutoModeAllowSource::Classifier,
    )]);
    for width in [48, 100] {
        let mut buf = new_buf(width, 30);
        let consumed = render_streaming_tool_use(
            &workflow_test_tool(),
            Rect::new(0, 0, width, 30),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
            TranscriptRenderExtras {
                auto_mode_allowed_tool_ids: &allowed,
                inline_live_workflow_card_max_rows: Some(8),
                ..TranscriptRenderExtras::empty()
            },
        );
        let snap = all_text(&buf);
        assert!(consumed <= 8, "{consumed}: {snap}");
        assert!(
            snap.trim_end().ends_with("Allowed by auto mode classifier"),
            "{snap}"
        );
    }
}

#[test]
fn sleep_tool_card_shows_seconds_summary_without_raw_body() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool_with_content(
            streaming_tool(
                "sleep-1",
                "Sleep",
                ToolKind::Other,
                ToolCallStatus::Completed,
                vec![("duration_ms", json!(3000))],
            ),
            vec![text_tool_content("sleeping 3000ms\nslept 3001ms")],
        ),
        vec![("durationMs", json!(3001)), ("completed", json!(true))],
    ));
    let mut buf = new_buf(80, 4);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 80, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..4)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("● Sleep (3s)"), "{snap:?}");
    assert!(!snap.contains("duration_ms"), "{snap:?}");
    assert!(!snap.contains("durationMs"), "{snap:?}");
    assert!(!snap.contains("completed"), "{snap:?}");
    assert!(!snap.contains("sleeping 3000ms"), "{snap:?}");
    assert!(!snap.contains("slept 3001ms"), "{snap:?}");
    assert!(!snap.contains("Ctrl+O"), "{snap:?}");
}

#[test]
fn web_search_tool_card_shows_only_quoted_query_and_answer() {
    let mut tool = streaming_tool_with_raw_output(
        streaming_tool(
            "search-1",
            "WebSearch",
            ToolKind::Search,
            ToolCallStatus::Completed,
            vec![("query", json!("Windows Terminal synchronized output"))],
        ),
        vec![
            ("query", json!("Windows Terminal synchronized output")),
            ("answer", json!("Search answer")),
            (
                "results",
                json!([{
                    "title": "Leaking result title",
                    "url": "https://example.com",
                    "snippet": "Leaking result snippet"
                }]),
            ),
            ("serverToolUses", json!([{"name": "web_search"}])),
        ],
    );
    tool.title = Some("Windows Terminal synchronized output".into());
    tool.content = Some(vec![text_tool_content("Leaking content description")]);

    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(tool);
    for verbosity in [
        ToolOutputVerbosity::Compact,
        ToolOutputVerbosity::Normal,
        ToolOutputVerbosity::Verbose,
    ] {
        let mut buf = new_buf(80, 4);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 80, 4),
            &mut buf,
            &RenderTheme::plain(),
            verbosity,
        );
        let snap = (0..4)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            snap.contains("● WebSearch (\"Windows Terminal synchronized output\")"),
            "{verbosity:?}: {snap:?}"
        );
        assert!(snap.contains("Search answer"), "{verbosity:?}: {snap:?}");
        for hidden in [
            "Leaking content description",
            "Leaking result title",
            "Leaking result snippet",
            "serverToolUses",
        ] {
            assert!(!snap.contains(hidden), "{verbosity:?}: {snap:?}");
        }
    }
}

#[test]
fn deferred_web_search_tool_card_shows_only_quoted_query_and_answer() {
    let mut tool = streaming_tool_with_raw_output(
        streaming_tool(
            "search-2",
            "InvokeDeferredTool",
            ToolKind::Other,
            ToolCallStatus::Completed,
            vec![
                ("tool_name", json!("WebSearch")),
                (
                    "arguments",
                    json!({ "query": "Windows Terminal synchronized output" }),
                ),
            ],
        ),
        vec![
            ("query", json!("Windows Terminal synchronized output")),
            ("answer", json!("Search answer")),
            (
                "results",
                json!([{
                    "title": "Leaking result title",
                    "url": "https://example.com",
                    "snippet": "Leaking result snippet"
                }]),
            ),
            ("serverToolUses", json!([{"name": "web_search"}])),
        ],
    );
    tool.title = Some("Windows Terminal synchronized output".into());
    tool.content = Some(vec![text_tool_content("Leaking content description")]);

    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(tool);
    for verbosity in [
        ToolOutputVerbosity::Compact,
        ToolOutputVerbosity::Normal,
        ToolOutputVerbosity::Verbose,
    ] {
        let mut buf = new_buf(80, 4);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 80, 4),
            &mut buf,
            &RenderTheme::plain(),
            verbosity,
        );
        let snap = (0..4)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            snap.contains("● WebSearch (\"Windows Terminal synchronized output\")"),
            "{verbosity:?}: {snap:?}"
        );
        assert!(
            !snap.contains("InvokeDeferredTool"),
            "{verbosity:?}: {snap:?}"
        );
        assert!(snap.contains("Search answer"), "{verbosity:?}: {snap:?}");
        for hidden in [
            "Leaking content description",
            "Leaking result title",
            "Leaking result snippet",
            "serverToolUses",
        ] {
            assert!(!snap.contains(hidden), "{verbosity:?}: {snap:?}");
        }
    }
}

#[test]
fn failed_web_search_tool_card_only_shows_answer() {
    let mut tool = streaming_tool_with_raw_output(
        streaming_tool_with_content(
            streaming_tool(
                "search-failed",
                "WebSearch",
                ToolKind::Search,
                ToolCallStatus::Failed,
                vec![("query", json!("secret query"))],
            ),
            vec![text_tool_content("Leaking content description")],
        ),
        vec![
            ("query", json!("secret query")),
            ("answer", json!("Search answer")),
            ("results", json!([{"title": "Leaking result title"}])),
        ],
    );
    tool.title = Some("Leaking title".into());

    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(tool);
    let mut buf = new_buf(80, 4);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 80, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    let snap = (0..4)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("WebSearch (\"secret query\")"), "{snap:?}");
    assert!(snap.contains("Search answer"), "{snap:?}");
    for hidden in [
        "Leaking title",
        "Leaking content description",
        "Leaking result title",
    ] {
        assert!(!snap.contains(hidden), "{snap:?}");
    }
}

#[test]
fn web_search_without_answer_hides_other_output() {
    let mut tool = streaming_tool_with_raw_output(
        streaming_tool_with_content(
            streaming_tool(
                "search-no-answer",
                "WebSearch",
                ToolKind::Search,
                ToolCallStatus::Completed,
                vec![("query", json!("secret query"))],
            ),
            vec![text_tool_content("Leaking content description")],
        ),
        vec![
            ("query", json!("secret query")),
            ("results", json!([{"title": "Leaking result title"}])),
        ],
    );
    tool.title = Some("Leaking title".into());

    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(tool);
    for verbosity in [
        ToolOutputVerbosity::Compact,
        ToolOutputVerbosity::Normal,
        ToolOutputVerbosity::Verbose,
    ] {
        let mut buf = new_buf(80, 4);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 80, 4),
            &mut buf,
            &RenderTheme::plain(),
            verbosity,
        );
        let snap = (0..4)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            snap.contains("WebSearch (\"secret query\")"),
            "{verbosity:?}: {snap:?}"
        );
        for hidden in [
            "Leaking title",
            "Leaking content description",
            "Leaking result title",
        ] {
            assert!(!snap.contains(hidden), "{verbosity:?}: {snap:?}");
        }
    }
}

#[test]
fn failed_workflow_card_shows_user_interruption() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_content(
        streaming_tool(
            "workflow-1",
            "Workflow",
            ToolKind::Other,
            ToolCallStatus::Failed,
            vec![("script", json!("workflow()"))],
        ),
        vec![ToolCallContent::Content(rebon_types::RegularContent {
            content: ContentBlock::Text(rebon_types::TextContent {
                text: "User intercepted workflow".into(),
                annotations: None,
            }),
        })],
    ));
    let mut buf = new_buf(80, 4);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 80, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Normal,
    );
    let snap = (0..4)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("● Workflow"), "{snap:?}");
    assert!(snap.contains("⎿ User intercepted workflow"), "{snap:?}");
}

#[test]
fn workflow_card_header_and_table_do_not_leak_raw_fields() {
    let snap = render_workflow_tool_snapshot(ToolOutputVerbosity::Normal, 14);

    assert!(snap.contains("Workflow: markdown-previewer"), "{snap:?}");
    assert!(
        snap.contains("run wf_test · running · 2 phases · 2 agents"),
        "{snap:?}"
    );
    // Two-column table: phase on the left, agents on the right.
    assert!(snap.contains("│ Agents"), "{snap:?}");
    assert!(snap.contains("─┼─"), "{snap:?}");
    assert!(snap.contains("running design"), "{snap:?}");
    assert!(snap.contains("ui-design"), "{snap:?}");
    assert!(snap.contains("1.2s"), "{snap:?}");
    assert!(snap.contains("1 tool"), "{snap:?}");
    assert!(snap.contains("1.2k tokens"), "{snap:?}");
    assert!(snap.contains("running review"), "{snap:?}");
    assert!(snap.contains("failed security-review"), "{snap:?}");
    assert!(snap.contains("error: 1 finding"), "{snap:?}");
    assert!(snap.contains("Log: narrator note"), "{snap:?}");
    assert!(!snap.contains("Phase: design"), "{snap:?}");
    assert!(!snap.contains("script="), "{snap:?}");
    assert!(!snap.contains("workflowProgress"), "{snap:?}");
    assert!(!snap.contains("raw_output"), "{snap:?}");
}

/// The table groups every agent under its phase row: the first agent
/// shares the phase row, later agents continue with an empty phase
/// cell, and an agent-less phase still gets a placeholder row.
#[test]
fn workflow_table_carries_phase_once_and_marks_empty_phases() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "workflow-table",
            "Workflow",
            ToolKind::Other,
            ToolCallStatus::InProgress,
            vec![("script", json!("workflow()"))],
        ),
        vec![
            ("status", json!("running")),
            (
                "workflowProgress",
                json!({
                    "runId": "wf_table",
                    "workflowName": "table-layout",
                    "entries": [
                        { "sequence": 1, "entry": { "type": "phase", "title": "scan", "state": "start" } },
                        { "sequence": 2, "entry": { "type": "agent", "index": 1, "state": "completed", "phaseTitle": "scan", "label": "alpha", "tokens": 0, "toolCalls": 0 } },
                        { "sequence": 3, "entry": { "type": "agent", "index": 2, "state": "start", "phaseTitle": "scan", "label": "beta", "tokens": 0, "toolCalls": 0 } },
                        { "sequence": 4, "entry": { "type": "phase", "title": "verify", "state": "start" } }
                    ]
                }),
            ),
        ],
    ));
    let mut buf = new_buf(120, 12);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 12),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Normal,
    );
    let snap = (0..12)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");
    let rows: Vec<&str> = snap.lines().collect();

    let first = rows
        .iter()
        .find(|row| row.contains("alpha"))
        .expect("first agent row");
    assert!(first.contains("running scan"), "{snap:?}");
    let second = rows
        .iter()
        .find(|row| row.contains("beta"))
        .expect("second agent row");
    assert!(!second.contains("scan"), "{snap:?}");
    assert!(second.trim_start().starts_with('│'), "{snap:?}");
    let empty = rows
        .iter()
        .find(|row| row.contains("verify"))
        .expect("empty phase row");
    assert!(empty.contains('—'), "{snap:?}");
}

#[test]
fn workflow_table_styles_done_and_agent_labels() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "workflow-styles",
            "Workflow",
            ToolKind::Other,
            ToolCallStatus::InProgress,
            vec![("script", json!("workflow()"))],
        ),
        vec![
            ("status", json!("running")),
            (
                "workflowProgress",
                json!({
                    "runId": "wf_styles",
                    "workflowName": "workflow-styles",
                    "entries": [
                        { "sequence": 1, "entry": { "type": "phase", "title": "Implement", "state": "start" } },
                        { "sequence": 2, "entry": { "type": "agent", "index": 1, "state": "completed", "phaseTitle": "Implement", "label": "implement:workflow-styles", "tokens": 42, "toolCalls": 1, "durationMs": 1200 } }
                    ]
                }),
            ),
        ],
    ));
    let mut buf = new_buf(120, 8);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 8),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Normal,
    );

    let row = (0..8)
        .find(|&y| row_text(&buf, y).contains("implement:workflow-styles"))
        .expect("agent row");
    let text = row_text(&buf, row);
    let done_x = text.find("done").expect("done label") as u16;
    let agent_x = text.find("implement:workflow-styles").expect("agent label") as u16;
    let suffix_x = text.find("1.2s").expect("metadata suffix") as u16;

    assert_eq!(buf[(done_x, row)].style().fg, Some(Color::Green));
    assert!(
        buf[(agent_x, row)]
            .style()
            .add_modifier
            .contains(Modifier::BOLD),
        "agent label should be bold: {text:?}"
    );
    assert!(
        !buf[(suffix_x, row)]
            .style()
            .add_modifier
            .contains(Modifier::BOLD),
        "metadata suffix should not be bold: {text:?}"
    );
}

#[test]
fn workflow_compact_card_shows_all_steps_without_expand_hint() {
    let snap = render_workflow_tool_snapshot(ToolOutputVerbosity::Compact, 10);

    assert!(snap.contains("Workflow: markdown-previewer"), "{snap:?}");
    assert!(
        snap.contains("run wf_test · running · 2 phases · 2 agents"),
        "{snap:?}"
    );
    assert!(snap.contains("│ Agents"), "{snap:?}");
    assert!(snap.contains("running design"), "{snap:?}");
    assert!(snap.contains("running review"), "{snap:?}");
    assert!(snap.contains("failed security-review"), "{snap:?}");
    assert!(!snap.contains("Ctrl+O to expand"), "{snap:?}");
}

fn tall_workflow_tool(status: ToolCallStatus) -> StreamingToolUse {
    let entries: Vec<Value> = std::iter::once(
        json!({ "sequence": 0, "entry": { "type": "phase", "title": "scan", "state": "start" } }),
    )
    .chain((1..=8u64).map(|i| {
        json!({
            "sequence": i,
            "entry": {
                "type": "agent",
                "index": i,
                "state": if i <= 6 { "completed" } else { "start" },
                "phaseTitle": "scan",
                "label": format!("agent-{i}"),
                "tokens": 100 * i,
                "toolCalls": 1
            }
        })
    }))
    .chain(std::iter::once(
        json!({ "sequence": 9, "entry": { "type": "log", "message": "latest note" } }),
    ))
    .collect();
    streaming_tool_with_raw_output(
        streaming_tool(
            "workflow-tall",
            "Workflow",
            ToolKind::Other,
            status,
            vec![("script", json!("workflow()"))],
        ),
        vec![
            (
                "status",
                json!(if status == ToolCallStatus::InProgress {
                    "running"
                } else {
                    "completed"
                }),
            ),
            (
                "workflowProgress",
                json!({
                    "runId": "wf_tall",
                    "workflowName": "tall-run",
                    "entries": entries
                }),
            ),
        ],
    )
}

fn render_workflow_overlay_with_budget(
    status: ToolCallStatus,
    budget: Option<u16>,
) -> (String, usize) {
    let mut s = AppState::new();
    s.overlay
        .upsert_streaming_tool_use(tall_workflow_tool(status));
    let mut buf = new_buf(120, 30);
    let mut cache = TranscriptMeasureCache::new();
    let result = render_transcript_cached_with_running_hints(
        &s,
        Rect::new(0, 0, 120, 30),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Normal,
        0,
        None,
        &mut cache,
        false,
        TranscriptRenderExtras {
            inline_live_workflow_card_max_rows: budget,
            ..TranscriptRenderExtras::empty()
        },
    );
    (all_text(&buf), result.total_lines)
}

/// The inline live region is bottom-anchored and an in-progress block cannot
/// drain to scrollback, so a live workflow card taller than its row budget
/// must elide its middle: header + run summary stay, the freshest rows stay,
/// and the card never exceeds the budget.
#[test]
fn in_progress_workflow_card_elides_middle_to_stay_within_live_row_budget() {
    let (snap, total_lines) =
        render_workflow_overlay_with_budget(ToolCallStatus::InProgress, Some(10));

    assert!(total_lines <= 10, "card must fit budget: {total_lines}");
    assert!(snap.contains("Workflow: tall-run"), "{snap:?}");
    assert!(
        snap.contains("run wf_tall · running · 1 phases · 8 agents"),
        "{snap:?}"
    );
    assert!(snap.contains("rows hidden"), "{snap:?}");
    assert!(
        !snap.contains("agent-1 "),
        "earliest rows are elided: {snap:?}"
    );
    assert!(
        snap.contains("agent-8"),
        "freshest agent row stays: {snap:?}"
    );
    assert!(snap.contains("Log: latest note"), "{snap:?}");
}

/// Terminal cards (the ones that drain to scrollback) are never elided, even
/// when the live budget is set — the budget only bounds in-progress cards.
#[test]
fn completed_workflow_card_ignores_live_row_budget() {
    let (snap, total_lines) =
        render_workflow_overlay_with_budget(ToolCallStatus::Completed, Some(10));

    assert!(total_lines > 10, "full card renders: {total_lines}");
    assert!(!snap.contains("rows hidden"), "{snap:?}");
    assert!(snap.contains("agent-1"), "{snap:?}");
    assert!(snap.contains("agent-8"), "{snap:?}");
}

/// Without a budget (screen mode, scrollback inserts) the live card renders
/// in full.
#[test]
fn in_progress_workflow_card_without_budget_renders_full() {
    let (snap, total_lines) = render_workflow_overlay_with_budget(ToolCallStatus::InProgress, None);

    assert!(total_lines > 10, "full card renders: {total_lines}");
    assert!(!snap.contains("rows hidden"), "{snap:?}");
    assert!(snap.contains("agent-1"), "{snap:?}");
}

#[test]
fn deferred_workflow_card_shows_all_steps_without_expand_hint() {
    let mut tool = workflow_test_tool();
    tool.tool_name = "InvokeDeferredTool".into();
    tool.raw_input = Some(HashMap::from([
        ("tool_name".into(), json!("Workflow")),
        (
            "arguments".into(),
            json!({ "name": "markdown-previewer", "script": "workflow()" }),
        ),
    ]));

    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(tool);
    let mut buf = new_buf(120, 10);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 10),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..10)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("Workflow: markdown-previewer"), "{snap:?}");
    assert!(
        snap.contains("run wf_test · running · 2 phases · 2 agents"),
        "{snap:?}"
    );
    assert!(snap.contains("│ Agents"), "{snap:?}");
    assert!(snap.contains("running design"), "{snap:?}");
    assert!(snap.contains("running review"), "{snap:?}");
    assert!(snap.contains("failed security-review"), "{snap:?}");
    assert!(!snap.contains("Ctrl+O to expand"), "{snap:?}");
}

#[test]
fn workflow_verbose_card_includes_artifact_summary() {
    let mut tool = workflow_test_tool();
    tool.raw_output
        .as_mut()
        .unwrap()
        .insert("transcriptDir".into(), json!("/tmp/wf"));
    tool.raw_output
        .as_mut()
        .unwrap()
        .insert("scriptPath".into(), json!("/tmp/wf.js"));
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(tool);
    let mut buf = new_buf(120, 18);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 18),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    let snap = (0..18)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("Run id: wf_test"), "{snap:?}");
    assert!(snap.contains("- Read"), "{snap:?}");
    assert!(snap.contains("src/ui.rs"), "{snap:?}");
}

/// A completed workflow whose result predates the `workflowProgress` field
/// (or never emitted progress entries) must still render a body synthesized
/// from the terminal schema rather than collapsing to a bare `● Workflow`.
#[test]
fn workflow_card_falls_back_to_terminal_schema_without_progress_entries() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "workflow-legacy",
            "Workflow",
            ToolKind::Other,
            ToolCallStatus::Completed,
            vec![("name", json!("tauri-tab-titlebar-implementation"))],
        ),
        vec![
            ("status", json!("completed")),
            ("runId", json!("wf_legacy")),
            ("summary", json!("Implement the tab titlebar")),
            (
                "phases",
                json!([{ "title": "research" }, { "title": "synthesis" }]),
            ),
            ("result", json!({ "agentCount": 3, "logs": [] })),
        ],
    ));
    let mut buf = new_buf(120, 8);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 8),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Normal,
    );
    let snap = (0..8)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("● Workflow"), "{snap:?}");
    assert!(
        snap.contains("run wf_legacy · completed · 2 phases · 3 agents"),
        "{snap:?}"
    );
    assert!(snap.contains("research"), "{snap:?}");
    assert!(snap.contains("synthesis"), "{snap:?}");
    assert!(snap.contains("Implement the tab titlebar"), "{snap:?}");
    assert!(!snap.contains("workflowProgress"), "{snap:?}");
}

#[test]
fn render_streaming_overlay_paints_tool_use_cards() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(StreamingToolUse {
        call_id: "tool-1".into(),
        tool_name: "Read".into(),
        kind: ToolKind::Read,
        status: ToolCallStatus::Completed,
        title: Some("Read Cargo.toml".into()),
        content: Some(vec![ToolCallContent::Content(
            rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "done".into(),
                    annotations: None,
                }),
            },
        )]),
        locations: Some(vec![ToolCallLocation {
            path: "Cargo.toml".into(),
            line: Some(1),
        }]),
        raw_input: Some(HashMap::from([("file_path".into(), json!("Cargo.toml"))])),
        raw_output: Some(HashMap::from([("bytes".into(), json!(123))])),
    });
    let mut buf = new_buf(60, 6);
    let used = render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 60, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    assert!(used > 0);
    let snap = (0..6)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(snap.contains("● Read (Cargo.toml)"), "{snap:?}");
    assert!(snap.contains("done"), "{snap:?}");
    assert!(snap.contains("Cargo.toml:1"), "{snap:?}");
    assert!(snap.contains("bytes=123"), "{snap:?}");
}

#[test]
fn tool_card_bolds_tool_name_and_dims_summary() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "bash-1",
        "Bash",
        ToolKind::Execute,
        ToolCallStatus::InProgress,
        vec![("command", json!("cargo test"))],
    ));

    let detail_color = Color::Rgb(96, 96, 96);
    let theme = RenderTheme {
        assistant_prefix: Style::new().fg(detail_color),
        ..RenderTheme::plain()
    };
    let mut buf = new_buf(80, 3);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 80, 3),
        &mut buf,
        &theme,
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..3)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("⠋ Bash (cargo test)"), "{snap:?}");
    let tool_x = (0..buf.area().width)
        .find(|&x| buf[(x, 0)].symbol() == "B")
        .expect("Bash tool name");
    let summary_x = (0..buf.area().width)
        .find(|&x| buf[(x, 0)].symbol() == "(")
        .expect("tool summary");
    assert_text_modifier(&buf, 0, "Bash", Modifier::BOLD, true);
    assert_text_modifier(&buf, 0, "(", Modifier::BOLD, false);
    assert_ne!(buf[(tool_x, 0)].style().fg, Some(detail_color));
    assert_eq!(buf[(summary_x, 0)].style().fg, Some(detail_color));
    assert_eq!(buf[(summary_x + 1, 0)].style().fg, Some(detail_color));
}

#[test]
fn workflow_card_bolds_tool_name_and_dims_label() {
    let detail_color = Color::Rgb(96, 96, 96);
    let theme = RenderTheme {
        assistant_prefix: Style::new().fg(detail_color),
        ..RenderTheme::plain()
    };
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(workflow_test_tool());
    let mut buf = new_buf(120, 14);

    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 14),
        &mut buf,
        &theme,
        ToolOutputVerbosity::Normal,
    );

    let tool_x = (0..buf.area().width)
        .find(|&x| buf[(x, 0)].symbol() == "W")
        .expect("Workflow tool name");
    let detail_x = (0..buf.area().width)
        .find(|&x| buf[(x, 0)].symbol() == ":")
        .expect("Workflow label");
    assert!(buf[(tool_x, 0)]
        .style()
        .add_modifier
        .contains(Modifier::BOLD));
    assert_ne!(buf[(tool_x, 0)].style().fg, Some(detail_color));
    assert_eq!(buf[(detail_x, 0)].style().fg, Some(detail_color));
    assert_eq!(buf[(detail_x + 2, 0)].style().fg, Some(detail_color));
    assert!(!buf[(detail_x, 0)]
        .style()
        .add_modifier
        .contains(Modifier::BOLD));
}

#[test]
fn in_progress_tool_without_output_renders_header_only() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "powershell-1",
        "PowerShell",
        ToolKind::Execute,
        ToolCallStatus::InProgress,
        vec![("command", json!("Get-ChildItem Env:"))],
    ));

    let mut buf = new_buf(80, 4);
    let theme = RenderTheme {
        frame_time_ms: 80,
        ..RenderTheme::plain()
    };
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 80, 4),
        &mut buf,
        &theme,
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..4)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        snap.contains("⠙ PowerShell (Get-ChildItem Env:)"),
        "{snap:?}"
    );
    assert!(!snap.contains("⎿"), "{snap:?}");
    assert!(!snap.contains("Loading PowerShell"), "{snap:?}");
}

#[test]
fn in_progress_tool_shows_body_after_output_arrives() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_content(
        streaming_tool(
            "powershell-1",
            "PowerShell",
            ToolKind::Execute,
            ToolCallStatus::InProgress,
            vec![("command", json!("Get-ChildItem Env:"))],
        ),
        vec![text_tool_content("PATH=C:\\Windows")],
    ));

    let mut buf = new_buf(80, 4);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 80, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..4)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("⎿ PATH=C:\\Windows"), "{snap:?}");
}

#[test]
fn completed_skill_loaded_renders_single_header_line() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_content(
        streaming_tool(
            "skill-1",
            "Skill",
            ToolKind::Other,
            ToolCallStatus::Completed,
            vec![("skill", json!("commit"))],
        ),
        vec![text_tool_content(
            "Loaded skill /commit\nDescription: Generate a commit\nExpanded instructions:\nBase directory: /tmp",
        )],
    ));

    let mut buf = new_buf(100, 5);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 5),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..5)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("● Skill (commit) loaded"), "{snap:?}");
    assert!(!snap.contains("⎿"), "{snap:?}");
    assert!(!snap.contains("Loaded skill /commit"), "{snap:?}");
    assert!(!snap.contains("Description:"), "{snap:?}");
    assert!(!snap.contains("Expanded instructions:"), "{snap:?}");
}

#[test]
fn agent_spawn_shows_subtype_label_and_background_hint() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "agent-1",
        "Agent",
        ToolKind::Other,
        ToolCallStatus::InProgress,
        vec![
            ("subagent_type", json!("Explore")),
            ("description", json!("compare permission modes")),
            ("prompt", json!("compare permission modes")),
        ],
    ));

    let mut buf = new_buf(120, 8);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 8),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..8)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        snap.contains("Explore: compare permission modes"),
        "{snap:?}"
    );
    assert!(
        !snap.contains("Agent: compare permission modes"),
        "{snap:?}"
    );
    assert!(snap.contains("Ctrl+B to run in background"), "{snap:?}");
    assert!(!snap.contains("⎿ Ctrl+B"), "{snap:?}");
    assert!(!snap.contains("Ctrl+O to expand"), "{snap:?}");
}

#[test]
fn agent_spawn_does_not_repeat_subtype_from_fallback_title() {
    let mut overlay = StreamingOverlay::new();
    let mut tool = streaming_tool(
        "agent-1",
        "Agent",
        ToolKind::Other,
        ToolCallStatus::InProgress,
        vec![
            ("subagent_type", json!("Explore")),
            ("prompt", json!("inspect renderer")),
        ],
    );
    tool.title = Some("Explore: inspect renderer".into());
    overlay.upsert_streaming_tool_use(tool);

    let mut buf = new_buf(100, 3);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 3),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..3)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("Explore: inspect renderer"), "{snap:?}");
    assert!(!snap.contains("Explore: Explore:"), "{snap:?}");
}

#[test]
fn named_agent_spawn_strips_distinct_subtype_from_fallback_title() {
    let mut overlay = StreamingOverlay::new();
    let mut tool = streaming_tool(
        "agent-1",
        "Agent",
        ToolKind::Other,
        ToolCallStatus::InProgress,
        vec![
            ("name", json!("reader-audit")),
            ("subagent_type", json!("Explore")),
            ("prompt", json!("inspect renderer")),
        ],
    );
    tool.title = Some("Explore: inspect renderer".into());
    overlay.upsert_streaming_tool_use(tool);

    let mut buf = new_buf(100, 3);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 3),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..3)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("reader-audit: inspect renderer"), "{snap:?}");
    assert!(!snap.contains("reader-audit: Explore:"), "{snap:?}");
}

#[test]
fn foreground_agent_spawn_prefers_instance_name_over_subtype() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "agent-1",
        "Agent",
        ToolKind::Other,
        ToolCallStatus::InProgress,
        vec![
            ("name", json!("merchant-tower-explore")),
            ("subagent_type", json!("Explore")),
            ("description", json!("梳理商人售塔链路")),
            ("prompt", json!("梳理商人售塔链路")),
        ],
    ));

    let mut buf = new_buf(120, 4);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..4)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("merchant-tower-explore:"), "{snap:?}");
    assert!(!snap.contains("Explore:"), "{snap:?}");
}

#[test]
fn foreground_agent_spawn_prefers_metadata_display_name_over_name() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "agent-1",
        "Agent",
        ToolKind::Other,
        ToolCallStatus::InProgress,
        vec![
            ("name", json!("input-name")),
            ("metadata", json!({"display_name": "metadata-name"})),
            ("subagent_type", json!("Explore")),
            ("description", json!("inspect renderer")),
        ],
    ));

    let mut buf = new_buf(100, 3);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 3),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..3)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("metadata-name: inspect renderer"), "{snap:?}");
    assert!(!snap.contains("input-name: inspect renderer"), "{snap:?}");
}

#[test]
fn agent_spawn_prefers_output_display_name_over_input_names() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "agent-1",
            "Agent",
            ToolKind::Other,
            ToolCallStatus::Completed,
            vec![
                ("name", json!("input-name")),
                ("metadata", json!({"display_name": "metadata-name"})),
                ("subagent_type", json!("Explore")),
                ("description", json!("inspect renderer")),
            ],
        ),
        vec![
            ("status", json!("completed")),
            ("display_name", json!("output-name")),
            ("final_text", json!("done")),
        ],
    ));

    let mut buf = new_buf(100, 5);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 5),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..5)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("output-name: inspect renderer"), "{snap:?}");
    assert!(
        !snap.contains("metadata-name: inspect renderer"),
        "{snap:?}"
    );
}

#[test]
fn agent_spawn_shows_child_activity_lines() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_content(
        streaming_tool(
            "agent-1",
            "Agent",
            ToolKind::Other,
            ToolCallStatus::InProgress,
            vec![
                ("subagent_type", json!("Explore")),
                ("description", json!("ACP code")),
                ("prompt", json!("ACP code")),
            ],
        ),
        vec![
            ToolCallContent::Content(rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "src/first.rs".into(),
                    annotations: None,
                }),
            }),
            ToolCallContent::Content(rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "src/second.rs".into(),
                    annotations: None,
                }),
            }),
            ToolCallContent::Content(rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "src/third.rs".into(),
                    annotations: None,
                }),
            }),
            ToolCallContent::Content(rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "src/fourth.rs".into(),
                    annotations: None,
                }),
            }),
            ToolCallContent::Content(rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "src/latest.rs".into(),
                    annotations: None,
                }),
            }),
        ],
    ));

    let mut buf = new_buf(140, 8);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 140, 8),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..8)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("Explore: ACP code"), "{snap:?}");
    assert!(snap.contains("latest.rs"), "{snap:?}");
    assert!(snap.contains("fourth.rs"), "{snap:?}");
    assert!(!snap.contains("first.rs"), "{snap:?}");
    assert!(snap.contains("… +1 older lines"), "{snap:?}");
    assert!(!snap.contains("Ctrl+O to expand"), "{snap:?}");
    assert!(snap.contains("Ctrl+B to run in background"), "{snap:?}");
}

#[test]
fn in_progress_agent_ctrl_b_hint_is_not_body_output_gutter() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "agent-1",
            "Agent",
            ToolKind::Other,
            ToolCallStatus::InProgress,
            vec![
                ("subagent_type", json!("Explore")),
                ("description", json!("ACP code")),
                ("prompt", json!("ACP code")),
            ],
        ),
        vec![(
            "activity_lines",
            json!(["/workspace/crates/rebon-cli/src/main.rs"]),
        )],
    ));

    let mut buf = new_buf(100, 6);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let rows = semantic_rows(&buf);

    assert!(
        rows.iter()
            .any(|row| row.contains("/workspace/crates/rebon-cli/src/main.rs")),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("Ctrl+B to run in background")),
        "{rows:?}"
    );
    assert!(!rows.iter().any(|row| row.contains("⎿ Ctrl+B")), "{rows:?}");
}

#[test]
fn completed_agent_compact_shows_usage_summary_and_expands_to_final_text() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_agent_tool_with_history(streaming_tool(
            "agent-1",
            "Agent",
            ToolKind::Other,
            ToolCallStatus::Completed,
            vec![
                ("subagent_type", json!("Explore")),
                ("description", json!("ACP code")),
                ("prompt", json!("ACP code")),
            ],
        )),
        vec![
            ("status", json!("completed")),
            (
                "final_text",
                json!("found session/load chain\nsummary details"),
            ),
            (
                "usage",
                json!({
                    "input_tokens": 88135,
                    "cache_read_input_tokens": 65024,
                    "output_tokens": 3945
                }),
            ),
        ],
    ));

    let mut compact_buf = new_buf(140, 6);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 140, 6),
        &mut compact_buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let compact = (0..6)
        .map(|y| row_text(&compact_buf, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(compact.contains("Explore: ACP code"), "{compact:?}");
    assert!(!compact.contains("Explore: ACP code ["), "{compact:?}");
    assert!(
        compact
            .contains("Read 2 files, Input 88135 tokens, Cached 65024 tokens, Output 3945 tokens"),
        "{compact:?}"
    );
    assert!(!compact.contains("found session/load chain"), "{compact:?}");
    assert!(!compact.contains("Ctrl+O to expand"), "{compact:?}");
    assert!(!compact.contains("agent_id"), "{compact:?}");

    let mut expanded_buf = new_buf(140, 8);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 140, 8),
        &mut expanded_buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Normal,
    );
    let expanded = (0..8)
        .map(|y| row_text(&expanded_buf, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        expanded.contains("found session/load chain"),
        "{expanded:?}"
    );
    assert!(expanded.contains("summary details"), "{expanded:?}");
    assert!(
        !expanded.contains("Explore: Explore ACP code"),
        "{expanded:?}"
    );
}

#[test]
fn completed_agent_header_shows_input_cached_and_cumulative_output_tokens() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "agent-usage",
            "Agent",
            ToolKind::Other,
            ToolCallStatus::Completed,
            vec![
                ("subagent_type", json!("Plan")),
                (
                    "description",
                    json!("Design background-agent failure notification fix"),
                ),
            ],
        ),
        vec![(
            "usage",
            json!({
                "input_tokens": 58443,
                "cache_read_input_tokens": 43210,
                "prompt_cache_hit_tokens": 40000,
                "output_tokens": 987,
                "cumulative_output_tokens": 1234
            }),
        )],
    ));

    let mut buf = new_buf(180, 6);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 180, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let rendered = (0..6)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        rendered.contains(
            "Plan: Design background-agent failure notification fix [Input 58443 tokens, Cached 43210 tokens, Output 1234 tokens]"
        ),
        "{rendered:?}"
    );
    assert!(!rendered.contains("Output 987 tokens]"), "{rendered:?}");
}

#[test]
fn completed_explore_shows_camel_case_usage_in_body_without_header_duplication() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "agent-camel-usage",
            "Agent",
            ToolKind::Other,
            ToolCallStatus::Completed,
            vec![
                ("subagent_type", json!("Explore")),
                ("description", json!("inspect usage")),
            ],
        ),
        vec![(
            "usage",
            json!({
                "inputTokens": 100,
                "promptCacheHitTokens": 80,
                "cumulativeOutputTokens": 0,
                "outputTokens": 20
            }),
        )],
    ));

    let mut buf = new_buf(120, 6);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let rendered = (0..6)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("Explore: inspect usage"), "{rendered:?}");
    assert!(
        !rendered.contains("Explore: inspect usage ["),
        "{rendered:?}"
    );
    assert!(
        rendered.contains("Read 0 files, Input 100 tokens, Cached 80 tokens, Output 20 tokens"),
        "{rendered:?}"
    );
}

#[test]
fn agent_usage_header_is_hidden_before_completion() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "agent-usage",
            "Agent",
            ToolKind::Other,
            ToolCallStatus::InProgress,
            vec![
                ("subagent_type", json!("Explore")),
                ("description", json!("inspect usage")),
            ],
        ),
        vec![(
            "usage",
            json!({
                "inputTokens": 100,
                "promptCacheHitTokens": 80,
                "outputTokens": 20
            }),
        )],
    ));

    let mut buf = new_buf(120, 6);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let rendered = (0..6)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("Explore: inspect usage"), "{rendered:?}");
    assert!(!rendered.contains("[Input 100 tokens"), "{rendered:?}");
}

#[test]
fn completed_agent_verbose_shows_full_input_output_and_history() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "agent-1",
            "Agent",
            ToolKind::Other,
            ToolCallStatus::Completed,
            vec![
                ("subagent_type", json!("Explore")),
                ("description", json!("ACP code")),
                (
                    "prompt",
                    json!("Find ACP session/load chain\nReport details"),
                ),
            ],
        ),
        vec![
            ("status", json!("completed")),
            ("agent_id", json!("agent-123")),
            (
                "final_text",
                json!("found session/load chain\nsummary details"),
            ),
            (
                "sub_agent_tool_calls",
                json!([
                    { "tool_use_id": "read-1", "name": "Read", "input": { "file_path": "crates/rebon-acp/src/server.rs" }, "ok": true },
                    { "tool_use_id": "grep-1", "name": "Grep", "input": { "pattern": "session/load", "path": "crates/rebon-acp" }, "ok": true }
                ]),
            ),
            (
                "usage",
                json!({ "input_tokens": 88135, "output_tokens": 3945 }),
            ),
        ],
    ));

    let mut buf = new_buf(160, 40);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 160, 40),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    let rendered = all_text(&buf);

    assert!(rendered.contains("Explore(ACP code)"), "{rendered:?}");
    assert!(rendered.contains("Prompt:"), "{rendered:?}");
    assert!(
        rendered.contains("Find ACP session/load chain"),
        "{rendered:?}"
    );
    assert!(rendered.contains("Report details"), "{rendered:?}");
    assert!(
        rendered.contains("Read(crates/rebon-acp/src/server.rs)"),
        "{rendered:?}"
    );
    assert!(
        rendered.contains("Grep(session/load, crates/rebon-acp)"),
        "{rendered:?}"
    );
    assert!(rendered.contains("Response:"), "{rendered:?}");
    assert!(
        rendered.contains("found session/load chain"),
        "{rendered:?}"
    );
    assert!(rendered.contains("summary details"), "{rendered:?}");
    assert!(
        rendered.contains("Done (2 tools used · 92.1k tokens)"),
        "{rendered:?}"
    );
    assert!(
        !rendered.contains("Description:")
            && !rendered.contains("Model:")
            && !rendered.contains("Usage:"),
        "{rendered:?}"
    );
    assert!(
        !rendered.contains("Agent id:") && !rendered.contains("Status:"),
        "{rendered:?}"
    );
    assert!(!rendered.contains("Sub-agent tool calls:"), "{rendered:?}");
    assert!(!rendered.contains("input_tokens"), "{rendered:?}");
}

#[test]
fn completed_agent_summary_counts_camel_case_history_without_legacy_fields() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "agent-1",
            "Agent",
            ToolKind::Other,
            ToolCallStatus::Completed,
            vec![
                ("subagent_type", json!("Explore")),
                ("description", json!("camel history")),
                ("prompt", json!("camel history")),
            ],
        ),
        vec![
            ("status", json!("completed")),
            (
                "subAgentToolCalls",
                json!([
                    { "tool_use_id": "read-1", "name": "Read", "ok": true },
                    { "tool_use_id": "file-read-1", "name": "FileReadTool", "ok": true },
                    { "tool_use_id": "grep-1", "name": "Grep", "ok": true },
                    { "tool_use_id": "read-failed", "name": "Read", "ok": false }
                ]),
            ),
            ("usage", json!({ "input_tokens": 123, "output_tokens": 45 })),
        ],
    ));

    let mut buf = new_buf(120, 6);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let rendered = (0..6)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        rendered.contains("Read 2 files, Input 123 tokens, Cached 0 tokens, Output 45 tokens"),
        "{rendered:?}"
    );
    assert!(
        !rendered.contains("Read 0 files, Input 123 tokens, Cached 0 tokens, Output 45 tokens"),
        "{rendered:?}"
    );
}

#[test]
fn completed_agent_summary_uses_history_over_zero_raw_read_count() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_agent_tool_with_history(streaming_tool(
            "agent-1",
            "Agent",
            ToolKind::Other,
            ToolCallStatus::Completed,
            vec![
                ("subagent_type", json!("Explore")),
                ("description", json!("history wins")),
                ("prompt", json!("history wins")),
            ],
        )),
        vec![
            ("status", json!("completed")),
            ("read_file_count", json!(0)),
            ("readFileCount", json!(0)),
            ("usage", json!({ "input_tokens": 10, "output_tokens": 20 })),
        ],
    ));

    let mut buf = new_buf(120, 6);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let rendered = (0..6)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        rendered.contains("Read 2 files, Input 10 tokens, Cached 0 tokens, Output 20 tokens"),
        "{rendered:?}"
    );
    assert!(
        !rendered.contains("Read 0 files, Input 10 tokens, Cached 0 tokens, Output 20 tokens"),
        "{rendered:?}"
    );
}

#[test]
fn completed_agent_summary_falls_back_to_legacy_raw_read_count() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "agent-1",
            "Agent",
            ToolKind::Other,
            ToolCallStatus::Completed,
            vec![
                ("subagent_type", json!("Explore")),
                ("description", json!("legacy raw")),
                ("prompt", json!("legacy raw")),
            ],
        ),
        vec![
            ("status", json!("completed")),
            ("readFileCount", json!(4)),
            ("usage", json!({ "input_tokens": 11, "output_tokens": 22 })),
        ],
    ));

    let mut buf = new_buf(120, 6);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let rendered = (0..6)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        rendered.contains("Read 4 files, Input 11 tokens, Cached 0 tokens, Output 22 tokens"),
        "{rendered:?}"
    );
}

#[test]
fn plan_agent_final_output_is_never_truncated() {
    let mut overlay = StreamingOverlay::new();
    let final_text = (1..=12)
        .map(|idx| format!("plan line {idx}"))
        .collect::<Vec<_>>()
        .join("\n");
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "agent-1",
            "Agent",
            ToolKind::Other,
            ToolCallStatus::Completed,
            vec![
                ("subagent_type", json!("Plan")),
                ("description", json!("draft plan")),
                ("prompt", json!("draft plan")),
            ],
        ),
        vec![
            ("status", json!("completed")),
            ("agent_type", json!("Plan")),
            ("final_text", json!(final_text)),
            ("usage", json!({ "input_tokens": 1, "output_tokens": 2 })),
        ],
    ));

    let mut buf = new_buf(140, 20);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 140, 20),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let rendered = (0..20)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("plan line 1"), "{rendered:?}");
    assert!(rendered.contains("plan line 12"), "{rendered:?}");
    assert!(!rendered.contains("… +"), "{rendered:?}");
    assert!(
        !rendered.contains("Read 0 files, Input 1 tokens, Cached 0 tokens, Output 2 tokens"),
        "{rendered:?}"
    );
}

#[test]
fn collapsed_run_renders_aggregated_summary_with_ctrl_o_hint() {
    let mut overlay = StreamingOverlay::new();
    // Two Greps + one Read + one LS = `Searching for 2 patterns,
    // reading 1 file, listing 1 directory…`
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t1",
        "Grep",
        ToolKind::Search,
        ToolCallStatus::Completed,
        vec![("pattern", json!("foo"))],
    ));
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t2",
        "Grep",
        ToolKind::Search,
        ToolCallStatus::Completed,
        vec![("pattern", json!("bar"))],
    ));
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t3",
        "Read",
        ToolKind::Read,
        ToolCallStatus::Completed,
        vec![("file_path", json!("src/lib.rs"))],
    ));
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t4",
        "LS",
        ToolKind::Read,
        ToolCallStatus::InProgress,
        vec![("path", json!("src/"))],
    ));

    let mut buf = new_buf(120, 10);
    let used = render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 10),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    assert!(used > 0);
    let snap = (0..10)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    // The aggregated summary collapses the per-tool cards.
    assert!(
        snap.contains("Searching for 2 patterns"),
        "missing search summary: {snap:?}"
    );
    assert!(
        snap.contains("reading 1 file"),
        "missing read summary: {snap:?}"
    );
    assert!(
        snap.contains("listing 1 directory"),
        "missing list summary: {snap:?}"
    );
    // Active-group ellipsis + inline expand hint.
    assert!(
        snap.contains("…"),
        "missing active-group ellipsis: {snap:?}"
    );
    assert!(
        snap.contains("(Ctrl+O to expand)"),
        "missing inline expand hint: {snap:?}"
    );
    // Latest hint line appears below the summary with `⎿` gutter.
    assert!(snap.contains("⎿"), "missing hint gutter: {snap:?}");
    // Per-tool card headers should NOT appear — the run is collapsed.
    assert!(
        !snap.contains("● Grep"),
        "per-tool Grep card leaked through aggregation: {snap:?}"
    );
    assert!(
        !snap.contains("● Read"),
        "per-tool Read card leaked through aggregation: {snap:?}"
    );
    // The trailing `Ctrl+O to expand` hint should be suppressed when
    // every tool segment was collapsed, so only the inline summary marker remains.
    assert_eq!(
        snap.matches("Ctrl+O to expand").count(),
        1,
        "trailing hint duplicated inline expand marker: {snap:?}"
    );
}

#[test]
fn collapsed_inline_hint_suppresses_trailing_single_tool_hint() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t1",
        "Grep",
        ToolKind::Search,
        ToolCallStatus::Completed,
        vec![("pattern", json!("foo"))],
    ));
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t2",
        "Read",
        ToolKind::Read,
        ToolCallStatus::Completed,
        vec![("file_path", json!("src/lib.rs"))],
    ));
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t3",
        "Bash",
        ToolKind::Execute,
        ToolCallStatus::Completed,
        vec![("command", json!("cargo test"))],
    ));

    let mut buf = new_buf(120, 12);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 12),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..12)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        snap.contains("Searched for 1 pattern, read 1 file"),
        "collapsed summary missing: {snap:?}"
    );
    assert!(snap.contains("Bash"), "single tool missing: {snap:?}");
    assert_eq!(
        snap.matches("Ctrl+O to expand").count(),
        1,
        "collapsed inline hint and trailing tool hint must not stack: {snap:?}"
    );
}

#[test]
fn single_collapsible_tool_still_renders_per_tool_card() {
    // Regression guard for render_streaming_overlay_paints_tool_use_cards:
    // a lone collapsible tool use must not trigger aggregation.
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t1",
        "Read",
        ToolKind::Read,
        ToolCallStatus::Completed,
        vec![("file_path", json!("Cargo.toml"))],
    ));
    let mut buf = new_buf(60, 6);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 60, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..6)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        snap.contains("● Read"),
        "per-tool card must survive single-tool path: {snap:?}"
    );
    assert!(
        !snap.contains("(Ctrl+O to expand)"),
        "summary inline hint leaked into single-tool path: {snap:?}"
    );
}

#[test]
fn single_collapsible_tool_with_following_thinking_handles_compact_staleness() {
    // Regression: the collapse scanner used to speculatively absorb
    // Thinking after a lone collapsible tool, fail to form a >=2-tool
    // run, then advance past the absorbed Thinking and drop it from
    // the expanded path.
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t1",
        "Read",
        ToolKind::Read,
        ToolCallStatus::Completed,
        vec![("file_path", json!("Cargo.toml"))],
    ));
    overlay.append_streaming_thinking("single-tool reasoning");
    overlay.end_streaming_thinking();
    overlay.append_streaming_text("done");

    for verbosity in [ToolOutputVerbosity::Normal, ToolOutputVerbosity::Compact] {
        let mut buf = new_buf(80, 12);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 80, 12),
            &mut buf,
            &RenderTheme::plain(),
            verbosity,
        );
        let snap = normalized_visible_rows(&buf).join("\n");
        assert!(
            snap.contains("● Read"),
            "single-tool card missing in {verbosity:?}: {snap:?}"
        );
        assert!(
            snap.contains("single-tool reasoning"),
            "thinking after failed collapse missing in {verbosity:?}: {snap:?}"
        );
        if matches!(verbosity, ToolOutputVerbosity::Compact) {
            assert!(
                snap.contains("Ctrl+O to expand"),
                "compact thinking title should show expand hint in {verbosity:?}: {snap:?}"
            );
        } else {
            assert!(
                !snap.contains("Ctrl+O to expand"),
                "normal thinking body should not show expand hint in {verbosity:?}: {snap:?}"
            );
        }
        assert!(
            snap.contains("done"),
            "following text missing in {verbosity:?}: {snap:?}"
        );
        assert!(
            !snap.contains("reading 1 file"),
            "single-tool path must not render collapsed summary in {verbosity:?}: {snap:?}"
        );
    }
}

#[test]
fn expanded_collapsed_run_preserves_pending_and_completed_tool_details() {
    // Verbose is the Ctrl+O-expanded path for collapsed streaming runs.
    // Assert a lifecycle-mixed collapsed run replays both tools and their
    // details rather than only the completed/executed tool.
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(StreamingToolUse {
        call_id: "pending-read".into(),
        tool_name: "Read".into(),
        kind: ToolKind::Read,
        status: ToolCallStatus::Pending,
        title: Some("Waiting to read Cargo.toml".into()),
        content: Some(vec![ToolCallContent::Content(
            rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "pending detail".into(),
                    annotations: None,
                }),
            },
        )]),
        locations: None,
        raw_input: Some(HashMap::from([("file_path".into(), json!("Cargo.toml"))])),
        raw_output: None,
    });
    overlay.upsert_streaming_tool_use(StreamingToolUse {
        call_id: "completed-grep".into(),
        tool_name: "Grep".into(),
        kind: ToolKind::Search,
        status: ToolCallStatus::Completed,
        title: Some("Grep found matches".into()),
        content: Some(vec![ToolCallContent::Content(
            rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "completed detail".into(),
                    annotations: None,
                }),
            },
        )]),
        locations: Some(vec![ToolCallLocation {
            path: "src/lib.rs".into(),
            line: Some(7),
        }]),
        raw_input: Some(HashMap::from([("pattern".into(), json!("needle"))])),
        raw_output: Some(HashMap::from([("matches".into(), json!(1))])),
    });

    let mut buf = new_buf(100, 18);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 18),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Verbose,
    );
    let snap = normalized_visible_rows(&buf).join("\n");
    assert!(snap.contains("⠋ Read (Cargo.toml)"), "{snap:?}");
    assert!(snap.contains("Waiting to read Cargo.toml"), "{snap:?}");
    assert!(snap.contains("pending detail"), "{snap:?}");
    assert!(snap.contains("● Grep (needle)"), "{snap:?}");
    assert!(snap.contains("Grep found matches"), "{snap:?}");
    assert!(snap.contains("completed detail"), "{snap:?}");
    assert!(snap.contains("src/lib.rs:7"), "{snap:?}");
    assert!(snap.contains("matches=1"), "{snap:?}");
}

#[test]
fn text_between_tool_uses_splits_the_collapse_run() {
    // A text block between two tool uses must NOT merge them into
    // a single collapsed summary — text always breaks the run.
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t1",
        "Grep",
        ToolKind::Search,
        ToolCallStatus::Completed,
        vec![("pattern", json!("foo"))],
    ));
    overlay.append_streaming_text("thinking between tools...");
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t2",
        "Grep",
        ToolKind::Search,
        ToolCallStatus::Completed,
        vec![("pattern", json!("bar"))],
    ));
    let mut buf = new_buf(80, 10);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 80, 10),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..10)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");
    // No aggregated "Searched for 2 patterns" — each Grep renders
    // individually because text sits between them.
    assert!(
        !snap.contains("Searching for 2 patterns") && !snap.contains("Searched for 2 patterns"),
        "text between tools should prevent aggregation: {snap:?}"
    );
}

#[test]
fn streaming_visible_tool_splits_adjacent_thinking_groups() {
    let mut overlay = StreamingOverlay::new();
    overlay.append_streaming_thinking("first reasoning");
    overlay.end_streaming_thinking();
    overlay.upsert_streaming_tool_use(streaming_tool_with_content(
        streaming_tool(
            "t1",
            "Bash",
            ToolKind::Execute,
            ToolCallStatus::Completed,
            vec![("command", json!("printf output"))],
        ),
        vec![text_tool_content("one\ntwo\nthree\nfour\nfive\nsix")],
    ));
    overlay.append_streaming_thinking("second reasoning");
    overlay.end_streaming_thinking();

    let mut buf = new_buf(100, 16);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 16),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = all_text(&buf);

    assert!(!snap.contains("Reasoning (2 steps)"), "{snap:?}");
    assert!(snap.contains("· first reasoning"), "{snap:?}");
    assert!(snap.contains("· second reasoning"), "{snap:?}");
    assert!(snap.contains("● Bash (printf output)"), "{snap:?}");
}

#[test]
fn thinking_between_streaming_tool_uses_preserves_collapse_run() {
    // Reasoning-model streaming shape: `[Thinking, Glob, Thinking,
    // Glob, Thinking, Glob]` — a fresh `Thinking` block lands
    // between every tool_use because `ThinkingEnd` fires after each
    // tool call. The tool run still collapses: leading thinking stays
    // standalone, while later thinking remains owned by the collapsed run.
    let mut overlay = StreamingOverlay::new();
    overlay.append_streaming_thinking("I'm considering using Glob to match paths like *.tsx.");
    overlay.end_streaming_thinking();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t1",
        "Glob",
        ToolKind::Search,
        ToolCallStatus::Completed,
        vec![("pattern", json!("**/*.tsx"))],
    ));
    overlay.append_streaming_thinking(
        "Now I need to broaden the search to the imageStore helper as well.",
    );
    overlay.end_streaming_thinking();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t2",
        "Glob",
        ToolKind::Search,
        ToolCallStatus::Completed,
        vec![("pattern", json!("**/*.tsx"))],
    ));
    overlay.append_streaming_thinking("One more sweep under rebon to cover the Rust tree.");
    overlay.end_streaming_thinking();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t3",
        "Glob",
        ToolKind::Search,
        ToolCallStatus::InProgress,
        vec![("pattern", json!("rebon/**"))],
    ));

    let mut buf = new_buf(140, 16);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 140, 16),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..16)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    // All three Globs fold into one summary.
    assert!(
        snap.contains("Searching for 3 patterns"),
        "thinking between tool_uses broke the collapse run: {snap:?}"
    );
    // Thinking before the run stays standalone; thinking absorbed after
    // the first tool remains owned by the collapsed group.
    assert!(!snap.contains("Reasoning ("), "{snap:?}");
    assert!(
        snap.contains("· I'm considering using Glob to match paths like *.tsx."),
        "leading standalone thinking should remain visible: {snap:?}"
    );
    assert!(
        !snap.contains("Now I need to broaden the search")
            && !snap.contains("One more sweep under rebon"),
        "thinking absorbed by the collapsed run leaked into compact output: {snap:?}"
    );
    assert_eq!(
        snap.matches("Ctrl+O to expand").count(),
        2,
        "the standalone thinking and collapsed summary should each remain expandable: {snap:?}"
    );
    assert!(
        snap.contains("rebon/**"),
        "latest collapsed hint should remain visible: {snap:?}"
    );
    // Per-tool cards must not survive the collapse.
    assert!(
        !snap.contains("● Glob") && !snap.contains("● Search ("),
        "per-tool Glob card leaked despite successful collapse: {snap:?}"
    );
}

#[test]
fn thinking_after_streaming_collapsed_group_is_absorbed_immediately() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t1",
        "Read",
        ToolKind::Read,
        ToolCallStatus::Completed,
        vec![("file_path", json!("src/a.rs"))],
    ));
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t2",
        "Read",
        ToolKind::Read,
        ToolCallStatus::Completed,
        vec![("file_path", json!("src/b.rs"))],
    ));
    overlay.append_streaming_thinking("post-read reasoning should stay hidden");
    overlay.end_streaming_thinking();

    let mut buf = new_buf(100, 12);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 12),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = normalized_visible_rows(&buf).join("\n");

    assert!(
        snap.contains("Read 2 files") || snap.contains("read 2 files"),
        "collapsed read group missing: {snap:?}"
    );
    assert!(
        snap.contains("src/b.rs"),
        "latest read hint should remain visible: {snap:?}"
    );
    assert!(
        !snap.contains("post-read reasoning"),
        "thinking after an open collapsed group should be hidden immediately: {snap:?}"
    );
}

#[test]
fn cancel_commits_streaming_tool_uses_along_with_text() {
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(&mut s, Action::SetStreamingText("partial".into()));
    reducer(
        &mut s,
        Action::StartToolUse {
            call_id: "tool-1".into(),
            tool_name: "Read".into(),
            kind: ToolKind::Read,
            initial_status: ToolCallStatus::Pending,
            initial_title: Some("Read Cargo.toml".into()),
            raw_input: Some(HashMap::from([("file_path".into(), json!("Cargo.toml"))])),
            content: None,
            locations: None,
            raw_output: None,
        },
    );
    reducer(
        &mut s,
        Action::Cancel {
            commit_uuid: "a-cancel".into(),
            commit_timestamp: "t".into(),
        },
    );
    let mut buf = new_buf(60, 10);
    render_transcript(
        &s,
        Rect::new(0, 0, 60, 10),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Verbose,
        0,
        None,
    );
    let snap = all_text(&buf);
    assert!(
        snap.contains("partial"),
        "committed partial must be visible: {snap:?}"
    );
    // Tool uses are now committed along with text on cancel,
    // preserving all visible streaming content.
    assert!(
        snap.contains("Read"),
        "streaming tool use must be committed: {snap:?}"
    );
}

#[test]
fn update_tool_use_transitions_status_and_patches_optional_fields() {
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::StartToolUse {
            call_id: "tool-1".into(),
            tool_name: "Bash".into(),
            kind: ToolKind::Execute,
            initial_status: ToolCallStatus::Pending,
            initial_title: Some("Bash ls".into()),
            raw_input: Some(HashMap::from([("command".into(), json!("ls"))])),
            content: None,
            locations: None,
            raw_output: None,
        },
    );
    reducer(
        &mut s,
        Action::UpdateToolUse {
            call_id: "tool-1".into(),
            status: Some(ToolCallStatus::Completed),
            title: Some("Bash ls - done".into()),
            content: Some(vec![ToolCallContent::Content(
                rebon_types::RegularContent {
                    content: ContentBlock::Text(rebon_types::TextContent {
                        text: "ok".into(),
                        annotations: None,
                    }),
                },
            )]),
            locations: Some(vec![ToolCallLocation {
                path: "Cargo.toml".into(),
                line: Some(1),
            }]),
            raw_output: Some(HashMap::from([("bytes".into(), json!(2))])),
        },
    );
    let tool = s.overlay.find_tool_use("tool-1").unwrap();
    assert_eq!(tool.status, ToolCallStatus::Completed);
    assert_eq!(tool.title.as_deref(), Some("Bash ls - done"));
    assert_eq!(tool.raw_input.as_ref().unwrap()["command"], json!("ls"));
    assert_eq!(tool.raw_output.as_ref().unwrap()["bytes"], json!(2));
    assert_eq!(tool.locations.as_ref().unwrap()[0].path, "Cargo.toml");
}

/// Cancel-commit shows up in the rendered transcript as a
/// normal assistant row, with the assistant gutter `●`, not the
/// streaming gutter `⋯`.
#[test]
fn cancel_commit_rendered_transcript_matches_committed_shape() {
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(&mut s, Action::Commit(user("u1", "prompt")));
    reducer(&mut s, Action::SetStreamingText("part".into()));
    reducer(
        &mut s,
        Action::Cancel {
            commit_uuid: "a-cancel".into(),
            commit_timestamp: "t".into(),
        },
    );
    let mut buf = new_buf(40, 10);
    render_transcript(
        &s,
        Rect::new(0, 0, 40, 10),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Verbose,
        0,
        None,
    );
    let snap = all_text(&buf);
    assert!(snap.contains("prompt"), "expected user content: {snap:?}");
    assert!(
        snap.contains("part"),
        "committed partial must be visible: {snap:?}"
    );
    assert!(
        !snap.contains("⋯ "),
        "streaming overlay must be empty after cancel: {snap:?}"
    );
}

// ------------------------------------------------------------------
// ToolOutputVerbosity gating tests
// ------------------------------------------------------------------

#[test]
fn compact_mode_tool_result_still_shows_id_and_body() {
    let msg = Message::User(UserMessage {
        uuid: "u1".into(),
        timestamp: "t".into(),
        message: UserMessageInner {
            role: UserRole::User,
            content: vec![UserContentBlock::ToolResult(UserToolResultBlock {
                tool_use_id: "toolu_42".into(),
                content: ToolResultContent::Text("result body line".into()),
                is_error: Some(false),
            })],
        },
        is_compact_summary: None,
        is_meta: None,
        is_visible_in_transcript_only: None,
        image_paste_ids: None,
        plan_content: None,
    });
    let mut buf = new_buf(40, 8);
    let used = render_message(
        &msg,
        Rect::new(0, 0, 40, 8),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    assert!(used > 0);
    let snap = all_text(&buf);
    assert!(snap.contains("toolu_42"), "expected tool_use_id: {snap:?}");
    // In the rebon-message-tui path, style_condensed only affects
    // file edits, not tool results: the body stays visible even in
    // compact mode.
    assert!(
        snap.contains("result body line"),
        "compact mode still renders the tool-result body: {snap:?}"
    );
}

#[test]
fn normal_mode_shows_tool_result_body() {
    let msg = Message::User(UserMessage {
        uuid: "u1".into(),
        timestamp: "t".into(),
        message: UserMessageInner {
            role: UserRole::User,
            content: vec![UserContentBlock::ToolResult(UserToolResultBlock {
                tool_use_id: "toolu_42".into(),
                content: ToolResultContent::Text("visible body".into()),
                is_error: Some(false),
            })],
        },
        is_compact_summary: None,
        is_meta: None,
        is_visible_in_transcript_only: None,
        image_paste_ids: None,
        plan_content: None,
    });
    let mut buf = new_buf(40, 8);
    let used = render_message(
        &msg,
        Rect::new(0, 0, 40, 8),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Normal,
    );
    assert!(used >= 2);
    let snap = all_text(&buf);
    assert!(snap.contains("toolu_42"), "expected tool_use_id: {snap:?}");
    assert!(snap.contains("visible body"), "{snap:?}");
}

#[test]
fn normal_mode_streaming_overlay_shows_header_and_title() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(StreamingToolUse {
        call_id: "tool-1".into(),
        tool_name: "Read".into(),
        kind: ToolKind::Read,
        status: ToolCallStatus::Completed,
        title: Some("Read the lockfile".into()),
        content: Some(vec![ToolCallContent::Content(
            rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "should be hidden".into(),
                    annotations: None,
                }),
            },
        )]),
        locations: Some(vec![ToolCallLocation {
            path: "Cargo.lock".into(),
            line: Some(1),
        }]),
        raw_input: Some(HashMap::from([("file_path".into(), json!("Cargo.toml"))])),
        raw_output: Some(HashMap::from([("bytes".into(), json!(99))])),
    });
    let mut buf = new_buf(60, 6);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 60, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Normal,
    );
    let snap = (0..6)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");
    // Header is shown.
    assert!(snap.contains("Read (Cargo.toml)"), "{snap:?}");
    // Title is shown (differs from raw_input compact form).
    assert!(snap.contains("Read the lockfile"), "{snap:?}");
    // Content preview is retained in Normal mode so expanding from
    // Compact never removes body/detail evidence. Locations and
    // raw_output remain Verbose-only.
    assert!(snap.contains("should be hidden"), "{snap:?}");
    assert!(!snap.contains("Cargo.lock:1"), "{snap:?}");
    assert!(!snap.contains("bytes=99"), "{snap:?}");
}

#[test]
fn normal_mode_streaming_overlay_expands_collapsed_group() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(StreamingToolUse {
        call_id: "tool-1".into(),
        tool_name: "Read".into(),
        kind: ToolKind::Read,
        status: ToolCallStatus::Completed,
        title: None,
        content: None,
        locations: None,
        raw_input: Some(HashMap::from([("file_path".into(), json!("src/a.rs"))])),
        raw_output: None,
    });
    overlay.append_streaming_thinking("checking whether src/b.rs has the companion code");
    overlay.end_streaming_thinking();
    overlay.upsert_streaming_tool_use(StreamingToolUse {
        call_id: "tool-2".into(),
        tool_name: "Read".into(),
        kind: ToolKind::Read,
        status: ToolCallStatus::Completed,
        title: None,
        content: None,
        locations: None,
        raw_input: Some(HashMap::from([("file_path".into(), json!("src/b.rs"))])),
        raw_output: None,
    });

    let mut compact_buf = new_buf(80, 6);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 80, 6),
        &mut compact_buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let compact_snap = (0..6)
        .map(|y| row_text(&compact_buf, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        compact_snap.contains("Read 2 files") || compact_snap.contains("read 2 files"),
        "{compact_snap:?}"
    );
    assert!(
        compact_snap.contains("Ctrl+O to expand"),
        "{compact_snap:?}"
    );
    assert!(
        !compact_snap.contains("companion code"),
        "compact collapsed group should absorb intermediate thinking: {compact_snap:?}"
    );

    let mut normal_buf = new_buf(80, 12);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 80, 12),
        &mut normal_buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Normal,
    );
    let normal_snap = (0..12)
        .map(|y| row_text(&normal_buf, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !normal_snap.contains("Read 2 files") && !normal_snap.contains("read 2 files"),
        "normal mode still showed collapsed summary: {normal_snap:?}"
    );
    let read_a_pos = normal_snap
        .find("Read (src/a.rs)")
        .expect("first read missing");
    let thinking_pos = normal_snap
        .find("checking whether src/b.rs has the companion code")
        .expect("thinking body missing");
    let read_b_pos = normal_snap
        .find("Read (src/b.rs)")
        .expect("second read missing");
    assert!(
        read_a_pos < thinking_pos && thinking_pos < read_b_pos,
        "expanded collapsed group should replay tool/thinking/tool order: {normal_snap:?}"
    );
    assert!(
        normal_snap.contains("checking whether src/b.rs has the companion code"),
        "thinking body missing in expanded collapsed group: {normal_snap:?}"
    );
    assert!(!normal_snap.contains("Ctrl+O to expand"), "{normal_snap:?}");
}

#[test]
fn normal_mode_expands_lifecycle_mixed_collapsed_streaming_group() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(StreamingToolUse {
        call_id: "pending-read".into(),
        tool_name: "Read".into(),
        kind: ToolKind::Read,
        status: ToolCallStatus::Pending,
        title: Some("Waiting to read Cargo.toml".into()),
        content: Some(vec![ToolCallContent::Content(
            rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "pending detail".into(),
                    annotations: None,
                }),
            },
        )]),
        locations: Some(vec![ToolCallLocation {
            path: "Cargo.toml".into(),
            line: Some(1),
        }]),
        raw_input: Some(HashMap::from([("file_path".into(), json!("Cargo.toml"))])),
        raw_output: Some(HashMap::from([("bytes".into(), json!(12))])),
    });
    overlay.upsert_streaming_tool_use(StreamingToolUse {
        call_id: "completed-grep".into(),
        tool_name: "Grep".into(),
        kind: ToolKind::Search,
        status: ToolCallStatus::Completed,
        title: Some("Grep found matches".into()),
        content: Some(vec![ToolCallContent::Content(
            rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "completed detail".into(),
                    annotations: None,
                }),
            },
        )]),
        locations: Some(vec![ToolCallLocation {
            path: "src/lib.rs".into(),
            line: Some(7),
        }]),
        raw_input: Some(HashMap::from([("pattern".into(), json!("needle"))])),
        raw_output: Some(HashMap::from([("matches".into(), json!(1))])),
    });

    let mut compact_buf = new_buf(100, 6);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 6),
        &mut compact_buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let compact_snap = normalized_visible_rows(&compact_buf).join("\n");
    assert!(
        compact_snap.contains("Ctrl+O to expand"),
        "compact collapsed group should show expansion hint: {compact_snap:?}"
    );
    assert!(
        compact_snap.contains("Cargo.toml") || compact_snap.contains("needle"),
        "compact collapsed group should show summary or hint context: {compact_snap:?}"
    );

    let mut normal_buf = new_buf(100, 14);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 14),
        &mut normal_buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Normal,
    );
    let normal_snap = normalized_visible_rows(&normal_buf).join("\n");
    assert!(
        !normal_snap.contains("Ctrl+O to expand"),
        "normal mode still showed collapsed expansion hint: {normal_snap:?}"
    );
    assert!(
        !normal_snap.contains("reading 1 file") && !normal_snap.contains("searching 1 pattern"),
        "normal mode still showed collapsed summary: {normal_snap:?}"
    );

    assert!(
        normal_snap.contains("⠋ Read (Cargo.toml)"),
        "{normal_snap:?}"
    );
    assert!(
        normal_snap.contains("Waiting to read Cargo.toml"),
        "{normal_snap:?}"
    );
    assert!(normal_snap.contains("pending detail"), "{normal_snap:?}");
    assert!(normal_snap.contains("● Grep (needle)"), "{normal_snap:?}");
    assert!(
        normal_snap.contains("Grep found matches"),
        "{normal_snap:?}"
    );
    assert!(normal_snap.contains("completed detail"), "{normal_snap:?}");
    assert!(!normal_snap.contains("Cargo.toml:1"), "{normal_snap:?}");
    assert!(!normal_snap.contains("bytes=12"), "{normal_snap:?}");
    assert!(!normal_snap.contains("src/lib.rs:7"), "{normal_snap:?}");
    assert!(!normal_snap.contains("matches=1"), "{normal_snap:?}");
}

#[test]
fn streaming_overlay_renders_interleaved_text_and_tools() {
    let mut overlay = StreamingOverlay::new();
    overlay.append_streaming_text("I'll read the file");
    overlay.upsert_streaming_tool_use(StreamingToolUse {
        call_id: "t1".into(),
        tool_name: "Read".into(),
        kind: ToolKind::Read,
        status: ToolCallStatus::Completed,
        title: None,
        content: None,
        locations: None,
        raw_input: Some(HashMap::from([("file_path".into(), json!("Cargo.toml"))])),
        raw_output: None,
    });
    overlay.append_streaming_text("The file looks good");

    let mut buf = new_buf(60, 6);
    let used = render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 60, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    assert!(used >= 3);
    let snap: Vec<String> = (0..6).map(|y| row_text(&buf, y)).collect();
    // Text appears first, then tool, then more text — chronological order.
    let text1_pos = snap.iter().position(|l| l.contains("I'll read the file"));
    let tool_pos = snap.iter().position(|l| l.contains("Read (Cargo.toml)"));
    let text2_pos = snap.iter().position(|l| l.contains("The file looks good"));
    assert!(text1_pos.is_some(), "first text segment missing");
    assert!(tool_pos.is_some(), "tool card missing");
    assert!(text2_pos.is_some(), "second text segment missing");
    assert!(text1_pos < tool_pos, "text1 must be before tool");
    assert!(tool_pos < text2_pos, "tool must be before text2");
}

// ------------------------------------------------------------------
// Committed transcript grouping tests
// ------------------------------------------------------------------

/// Build an assistant message whose content is *only* tool_use
/// blocks — the shape that participates in committed collapse.
fn assistant_tool_uses(uuid: &str, tools: Vec<(&str, &str, serde_json::Value)>) -> Message {
    assistant_tool_uses_with_status(uuid, tools, None)
}

fn assistant_tool_uses_with_status(
    uuid: &str,
    tools: Vec<(&str, &str, serde_json::Value)>,
    status: Option<ToolCallStatus>,
) -> Message {
    let content = tools
        .into_iter()
        .map(|(id, name, input)| {
            AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                id: id.into(),
                name: name.into(),
                input,
                tool_call_content: None,
                raw_output: None,
                title: None,
                locations: None,
                status,
            })
        })
        .collect();
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

/// Build a user message whose content is *only* tool_result
/// blocks — the user-side companion to `assistant_tool_uses`.
fn user_tool_results(uuid: &str, results: Vec<(&str, &str, Option<bool>)>) -> Message {
    let content = results
        .into_iter()
        .map(|(tool_use_id, body, is_error)| {
            UserContentBlock::ToolResult(UserToolResultBlock {
                tool_use_id: tool_use_id.into(),
                content: ToolResultContent::Text(body.into()),
                is_error,
            })
        })
        .collect();
    Message::User(UserMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: UserMessageInner {
            role: UserRole::User,
            content,
        },
        is_compact_summary: None,
        is_meta: None,
        is_visible_in_transcript_only: None,
        image_paste_ids: None,
        plan_content: None,
    })
}

fn committed_agent_tool(uuid: &str, call_id: &str) -> Message {
    Message::Assistant(AssistantMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: vec![AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                id: call_id.into(),
                name: "Agent".into(),
                input: json!({
                    "subagent_type": "Explore",
                    "description": "inspect render path",
                    "prompt": "inspect render path"
                }),
                tool_call_content: Some(vec![ToolCallContent::Content(
                    rebon_types::RegularContent {
                        content: ContentBlock::Text(rebon_types::TextContent {
                            text: "completed agent output".into(),
                            annotations: None,
                        }),
                    },
                )]),
                raw_output: Some(json!({
                    "final_text": "completed agent output",
                    "tool_call_count": 1,
                    "usage": { "input_tokens": 12, "output_tokens": 34 }
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

fn assistant_async_agent_launch_with_description(
    uuid: &str,
    call_id: &str,
    task_id: &str,
    description: &str,
) -> Message {
    Message::Assistant(AssistantMessage {
        uuid: uuid.into(),
        timestamp: "t".into(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content: vec![AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                id: call_id.into(),
                name: "Agent".into(),
                input: json!({
                    "subagent_type": "Explore",
                    "description": description,
                    "prompt": description
                }),
                tool_call_content: None,
                raw_output: Some(json!({
                    "status": "async_launched",
                    "task_id": task_id,
                    "agent_id": task_id,
                    "description": description,
                    "tool_call_count": 0
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

fn assistant_async_agent_launch(uuid: &str, call_id: &str) -> Message {
    assistant_async_agent_launch_with_description(
        uuid,
        call_id,
        "agent-task-1",
        "Research external file inputs",
    )
}

fn assistant_async_agent_launch_batch(tools: Vec<(&str, &str, &str, &str, &str)>) -> Message {
    let content = tools
        .into_iter()
        .map(
            |(call_id, task_id, description, display_name, agent_type)| {
                AssistantContentBlock::ToolUse(AssistantToolUseBlock {
                    id: call_id.into(),
                    name: "Agent".into(),
                    input: json!({
                        "subagent_type": agent_type,
                        "description": description,
                        "name": display_name,
                        "prompt": description
                    }),
                    tool_call_content: None,
                    raw_output: Some(json!({
                        "status": "async_launched",
                        "task_id": task_id,
                        "agent_id": task_id,
                        "agent_type": agent_type,
                        "display_name": display_name,
                        "description": description,
                        "tool_call_count": 0
                    })),
                    title: None,
                    locations: None,
                    status: Some(ToolCallStatus::Completed),
                })
            },
        )
        .collect();
    Message::Assistant(AssistantMessage {
        uuid: "a-agent-batch".into(),
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

fn with_thinking_blocks(mut message: Message, thinking: &[&str]) -> Message {
    let Message::Assistant(assistant) = &mut message else {
        unreachable!("agent fixture is always an assistant message");
    };
    for (idx, text) in thinking.iter().enumerate() {
        assistant.message.content.insert(
            idx,
            AssistantContentBlock::Thinking(AssistantThinkingBlock {
                thinking: (*text).to_string(),
                signature: None,
            }),
        );
    }
    message
}

#[test]
fn screen_committed_async_agent_launches_group_live_activity() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_async_agent_launch_batch(vec![
            (
                "toolu_agent_1",
                "agent-task-1",
                "Review rebon-tui render module",
                "review-tui-render",
                "Explore",
            ),
            (
                "toolu_agent_2",
                "agent-task-2",
                "Review rebon-cli runner render module",
                "review-cli-render",
                "Explore",
            ),
            (
                "toolu_agent_3",
                "agent-task-3",
                "Review rebon-core query module",
                "review-engine-query",
                "Explore",
            ),
        ])),
    );

    let start_time_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        - 34_000;
    let mut live = HashMap::new();
    live.insert(
        "toolu_agent_1".to_string(),
        LiveAgentToolActivity {
            text: Some("Search: ^use rebon_".to_string()),
            status: LiveAgentToolStatus::Running,
            title: Some("Review rebon-tui render module".to_string()),
            display_name: Some("review-tui-render".to_string()),
            start_time_ms: Some(start_time_ms),
            end_time_ms: None,
            tool_use_count: Some(42),
            token_count: Some(124_100),
            terminal_result: None,
        },
    );
    live.insert(
        "toolu_agent_2".to_string(),
        LiveAgentToolActivity {
            text: Some("Searching for 9 patterns, reading 1 file…".to_string()),
            status: LiveAgentToolStatus::Running,
            title: Some("Review rebon-cli runner render module".to_string()),
            display_name: Some("review-cli-render".to_string()),
            start_time_ms: Some(start_time_ms),
            end_time_ms: None,
            tool_use_count: Some(48),
            token_count: Some(102_600),
            terminal_result: None,
        },
    );
    live.insert(
        "toolu_agent_3".to_string(),
        LiveAgentToolActivity {
            text: None,
            status: LiveAgentToolStatus::Completed,
            title: Some("Review rebon-core query module".to_string()),
            display_name: Some("review-engine-query".to_string()),
            start_time_ms: Some(start_time_ms),
            end_time_ms: None,
            tool_use_count: Some(30),
            token_count: Some(163_200),
            terminal_result: Some(HashMap::from([("duration_ms".to_string(), json!(34_000))])),
        },
    );

    let detail_color = Color::Rgb(96, 96, 96);
    let theme = RenderTheme {
        assistant_prefix: Style::new().fg(detail_color),
        ..RenderTheme::plain()
    };
    let mut buf = new_buf(160, 12);
    let mut cache = TranscriptMeasureCache::new();
    render_transcript_cached_with_running_hints(
        &s,
        Rect::new(0, 0, 160, 12),
        &mut buf,
        &theme,
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
    let header = (0..buf.area().height)
        .map(|y| row_text(&buf, y))
        .find(|row| row.contains("3 background agents launched"))
        .expect("agent group header should render");

    assert!(header.starts_with('●'), "{header:?}");
    assert!(
        header.contains("3 background agents launched (↓ to manage)"),
        "{header:?}"
    );
    assert!(snap.contains("@review-tui-render (Explore)"), "{snap:?}");
    assert!(snap.contains("@review-cli-render (Explore)"), "{snap:?}");
    let detail_y = (0..buf.area().height)
        .find(|&y| row_text(&buf, y).contains("@review-tui-render"))
        .expect("agent detail row");
    let tool_x = (0..buf.area().width)
        .find(|&x| {
            buf[(x, detail_y)]
                .symbol()
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
        })
        .expect("agent tool name");
    let summary_x = (0..buf.area().width)
        .find(|&x| buf[(x, detail_y)].symbol() == "(")
        .expect("agent detail summary");
    assert!(buf[(tool_x, detail_y)]
        .style()
        .add_modifier
        .contains(Modifier::BOLD));
    assert_ne!(buf[(tool_x, detail_y)].style().fg, Some(detail_color));
    assert_eq!(buf[(summary_x, detail_y)].style().fg, Some(detail_color));
    assert!(!buf[(summary_x, detail_y)]
        .style()
        .add_modifier
        .contains(Modifier::BOLD));
    assert!(!snap.contains("42 tool uses"), "{snap:?}");
    assert!(snap.contains("↓ 124.1k tokens"), "{snap:?}");
    let completed_row = (0..buf.area().height)
        .map(|y| row_text(&buf, y))
        .find(|row| row.contains("@review-engine-query"))
        .expect("completed agent detail row");
    assert!(
        completed_row.trim_end().ends_with("34s · ↓ 163.2k tokens"),
        "{completed_row:?}"
    );
    assert!(snap.contains("Search: ^use rebon_"), "{snap:?}");
    assert!(snap.contains("Searching for 9 patterns"), "{snap:?}");
    assert!(snap.contains("Done"), "{snap:?}");
    assert!(!snap.contains("async_launched"), "{snap:?}");
}

#[test]
fn inline_scrollback_agent_launches_use_static_status_without_launched_rows() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_async_agent_launch_batch(vec![
            (
                "toolu_agent_1",
                "agent-task-1",
                "Review rebon-tui render module",
                "explore-tui",
                "Explore",
            ),
            (
                "toolu_agent_2",
                "agent-task-2",
                "Review rebon-cli runner render module",
                "explore-cli",
                "Explore",
            ),
        ])),
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
        TranscriptRenderExtras::empty(),
    );
    let screen_snap = all_text(&buf);
    let screen_header = (0..buf.area().height)
        .map(|y| row_text(&buf, y))
        .find(|row| row.contains("2 background agents launched"))
        .expect("screen agent group header should render");

    assert!(screen_header.starts_with('●'), "{screen_header:?}");
    assert!(screen_snap.contains("Launched"), "{screen_snap:?}");
    assert!(screen_snap.contains("⎿"), "{screen_snap:?}");

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
            static_agent_group_status: true,
            ..TranscriptRenderExtras::empty()
        },
    );
    let snap = all_text(&buf);
    let header = (0..buf.area().height)
        .map(|y| row_text(&buf, y))
        .find(|row| row.contains("2 background agents launched"))
        .expect("agent group header should render");

    assert!(header.starts_with('●'), "{header:?}");
    assert!(snap.contains("@explore-tui (Explore)"), "{snap:?}");
    assert!(snap.contains("@explore-cli (Explore)"), "{snap:?}");
    assert!(!snap.contains("Launched"), "{snap:?}");
    assert!(!snap.contains("⎿"), "{snap:?}");
}

#[test]
fn screen_clipped_agent_group_header_stays_static_across_frames() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_async_agent_launch_batch(vec![
            (
                "toolu_agent_1",
                "agent-task-1",
                "Review rebon-tui render module",
                "explore-tui",
                "Explore",
            ),
            (
                "toolu_agent_2",
                "agent-task-2",
                "Review rebon-cli runner render module",
                "explore-cli",
                "Explore",
            ),
        ])),
    );

    let area = Rect::new(0, 0, 120, 4);
    let extras = TranscriptRenderExtras {
        leading_segment_margin: true,
        ..TranscriptRenderExtras::empty()
    };
    let mut buf = new_buf(120, 4);
    let mut cache = TranscriptMeasureCache::new();
    render_transcript_cached_with_running_hints(
        &s,
        area,
        &mut buf,
        &RenderTheme::plain(),
        1,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        extras,
    );
    let first_header = (0..buf.area().height)
        .map(|y| row_text(&buf, y))
        .find(|row| row.contains("2 background agents launched"))
        .expect("first screen agent group header should render");
    assert_eq!(cache.clipped_segment_hits, 0);

    let next_theme = RenderTheme {
        frame_time_ms: 80,
        ..RenderTheme::plain()
    };
    render_transcript_cached_with_running_hints(
        &s,
        area,
        &mut buf,
        &next_theme,
        1,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        extras,
    );
    let next_header = (0..buf.area().height)
        .map(|y| row_text(&buf, y))
        .find(|row| row.contains("2 background agents launched"))
        .expect("next screen agent group header should render");

    assert!(first_header.starts_with('●'), "{first_header:?}");
    assert!(next_header.starts_with('●'), "{next_header:?}");
    assert_eq!(cache.clipped_segment_hits, 1);
}

#[test]
fn clipped_agent_group_invalidates_only_for_matching_live_activity() {
    use crate::state::{reducer, Action};

    let mut state = AppState::new();
    reducer(
        &mut state,
        Action::Commit(assistant_async_agent_launch_batch(vec![
            (
                "toolu_agent_1",
                "agent-task-1",
                "Review rebon-tui render module",
                "explore-tui",
                "Explore",
            ),
            (
                "toolu_agent_2",
                "agent-task-2",
                "Review rebon-cli runner render module",
                "explore-cli",
                "Explore",
            ),
        ])),
    );
    let mut live = HashMap::from([(
        "toolu_agent_1".to_string(),
        LiveAgentToolActivity {
            text: Some("reading source".into()),
            status: LiveAgentToolStatus::Running,
            title: Some("Review rebon-tui render module".into()),
            display_name: Some("explore-tui".into()),
            start_time_ms: None,
            end_time_ms: None,
            tool_use_count: None,
            token_count: None,
            terminal_result: None,
        },
    )]);
    let area = Rect::new(0, 0, 64, 4);
    let mut buf = new_buf(64, 4);
    let mut cache = TranscriptMeasureCache::new();
    let render = |live: &HashMap<String, LiveAgentToolActivity>,
                  revision,
                  buf: &mut Buffer,
                  cache: &mut TranscriptMeasureCache| {
        render_transcript_cached_with_running_hints(
            &state,
            area,
            buf,
            &RenderTheme::plain(),
            1,
            ToolOutputVerbosity::Compact,
            0,
            None,
            cache,
            false,
            TranscriptRenderExtras {
                live_agent_tool_activity: live,
                live_activity_revision: revision,
                leading_segment_margin: true,
                ..TranscriptRenderExtras::empty()
            },
        )
    };

    let first = render(&live, 1, &mut buf, &mut cache);
    assert_eq!(cache.layout_full_builds, 1);
    assert_eq!(cache.clipped_segment_hits, 0);

    render(&live, 1, &mut buf, &mut cache);
    assert_eq!(cache.layout_cache_hits, 1);
    assert_eq!(cache.clipped_segment_hits, 1);

    live.get_mut("toolu_agent_1").unwrap().text =
        Some("reading a much longer activity message that wraps across terminal rows".into());
    let updated = render(&live, 2, &mut buf, &mut cache);
    let snap = all_text(&buf);

    assert!(snap.contains("reading a much longer activity"), "{snap:?}");
    assert!(updated.total_lines > first.total_lines);
    assert_eq!(cache.layout_activity_updates, 1);
    assert_eq!(cache.layout_full_builds, 1);
    assert_eq!(cache.clipped_segment_hits, 1);
}

#[test]
fn expanded_agent_group_renders_its_thinking_as_reasoning() {
    let mut state = AppState::new();
    reducer(
        &mut state,
        Action::Commit(with_thinking_blocks(
            assistant_async_agent_launch_batch(vec![
                (
                    "toolu_agent_1",
                    "agent-task-1",
                    "Review rebon-tui render module",
                    "explore-tui",
                    "Explore",
                ),
                (
                    "toolu_agent_2",
                    "agent-task-2",
                    "Review rebon-cli runner render module",
                    "explore-cli",
                    "Explore",
                ),
            ]),
            &[
                "first agent reasoning\nfirst agent detail",
                "second agent reasoning\nsecond agent detail",
            ],
        )),
    );

    let mut buf = new_buf(120, 20);
    render_transcript(
        &state,
        Rect::new(0, 0, 120, 20),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Normal,
        0,
        None,
    );
    let snap = all_text(&buf);

    assert!(snap.contains("Reasoning (2 steps)"), "{snap:?}");
    assert!(snap.contains("├ first agent reasoning"), "{snap:?}");
    assert!(snap.contains("└ second agent reasoning"), "{snap:?}");
    assert_eq!(snap.matches("first agent detail").count(), 1, "{snap:?}");
    assert_eq!(snap.matches("second agent detail").count(), 1, "{snap:?}");
    assert!(snap.contains("explore-tui"), "{snap:?}");
    assert!(snap.contains("explore-cli"), "{snap:?}");
}

#[test]
fn clipped_running_agent_elapsed_inside_thinking_group_uses_wall_clock() {
    let mut state = AppState::new();
    reducer(
        &mut state,
        Action::Commit(with_thinking_blocks(
            assistant_async_agent_launch_batch(vec![
                (
                    "toolu_agent_1",
                    "agent-task-1",
                    "Review rebon-tui render module",
                    "explore-tui",
                    "Explore",
                ),
                (
                    "toolu_agent_2",
                    "agent-task-2",
                    "Review rebon-cli runner render module",
                    "explore-cli",
                    "Explore",
                ),
            ]),
            &["first reasoning", "second reasoning"],
        )),
    );

    let start_time_ms = 1_000_000;
    let live = HashMap::from([(
        "toolu_agent_1".to_string(),
        LiveAgentToolActivity {
            text: None,
            status: LiveAgentToolStatus::Running,
            title: Some("Review rebon-tui render module".to_string()),
            display_name: Some("explore-tui".to_string()),
            start_time_ms: Some(start_time_ms),
            end_time_ms: None,
            tool_use_count: None,
            token_count: Some(30_600),
            terminal_result: None,
        },
    )]);
    let extras = TranscriptRenderExtras {
        live_agent_tool_activity: &live,
        live_activity_revision: 1,
        leading_segment_margin: true,
        ..TranscriptRenderExtras::empty()
    };
    let area = Rect::new(0, 0, 120, 4);
    let mut buf = new_buf(120, 4);
    let mut cache = TranscriptMeasureCache::new();
    let render = |render_time_ms, buf: &mut Buffer, cache: &mut TranscriptMeasureCache| {
        render_transcript_cached_with_running_hints_at(
            &state,
            area,
            buf,
            &RenderTheme::plain(),
            usize::MAX,
            ToolOutputVerbosity::Compact,
            0,
            None,
            cache,
            false,
            extras,
            render_time_ms,
        );
    };

    render(1_034_000, &mut buf, &mut cache);
    assert_eq!(cache.clipped_segment_hits, 0);
    let first = all_text(&buf);
    assert!(first.contains("34s · ↓ 30.6k tokens"), "{first:?}");

    render(1_034_080, &mut buf, &mut cache);
    assert_eq!(cache.clipped_segment_hits, 1);

    render(1_035_000, &mut buf, &mut cache);
    assert_eq!(cache.clipped_segment_hits, 1);
    assert!(all_text(&buf).contains("35s · ↓ 30.6k tokens"));
}

#[test]
fn clipped_running_agent_elapsed_uses_wall_clock_when_frame_clock_stops() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_async_agent_launch_batch(vec![
            (
                "toolu_agent_1",
                "agent-task-1",
                "Review rebon-tui render module",
                "explore-tui",
                "Explore",
            ),
            (
                "toolu_agent_2",
                "agent-task-2",
                "Review rebon-cli runner render module",
                "explore-cli",
                "Explore",
            ),
        ])),
    );

    let start_time_ms = 1_000_000;
    let live = HashMap::from([(
        "toolu_agent_1".to_string(),
        LiveAgentToolActivity {
            text: None,
            status: LiveAgentToolStatus::Running,
            title: Some("Review rebon-tui render module".to_string()),
            display_name: Some("explore-tui".to_string()),
            start_time_ms: Some(start_time_ms),
            end_time_ms: None,
            tool_use_count: None,
            token_count: Some(30_600),
            terminal_result: None,
        },
    )]);
    let extras = TranscriptRenderExtras {
        live_agent_tool_activity: &live,
        live_activity_revision: 1,
        leading_segment_margin: true,
        ..TranscriptRenderExtras::empty()
    };
    let area = Rect::new(0, 0, 120, 4);
    let mut buf = new_buf(120, 4);
    let mut cache = TranscriptMeasureCache::new();
    let render =
        |frame_time_ms, render_time_ms, buf: &mut Buffer, cache: &mut TranscriptMeasureCache| {
            let theme = RenderTheme {
                frame_time_ms,
                ..RenderTheme::plain()
            };
            render_transcript_cached_with_running_hints_at(
                &s,
                area,
                buf,
                &theme,
                1,
                ToolOutputVerbosity::Compact,
                0,
                None,
                cache,
                false,
                extras,
                render_time_ms,
            );
        };

    render(800, 1_034_000, &mut buf, &mut cache);
    assert_eq!(cache.clipped_segment_hits, 0);
    let first = all_text(&buf);
    assert!(first.contains("34s · ↓ 30.6k tokens"), "{first:?}");

    render(0, 1_034_080, &mut buf, &mut cache);
    assert_eq!(cache.clipped_segment_hits, 1);

    render(0, 1_035_000, &mut buf, &mut cache);
    assert_eq!(cache.clipped_segment_hits, 1);
    let next_second = all_text(&buf);
    assert!(
        next_second.contains("35s · ↓ 30.6k tokens"),
        "{next_second:?}"
    );

    render(0, 1_035_080, &mut buf, &mut cache);
    assert_eq!(cache.clipped_segment_hits, 2);
}

#[test]
fn clipped_hidden_agent_metrics_do_not_invalidate_cache_each_second() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_async_agent_launch_batch(vec![
            (
                "toolu_agent_1",
                "agent-task-1",
                "Review rebon-tui render module",
                "explore-tui",
                "Explore",
            ),
            (
                "toolu_agent_2",
                "agent-task-2",
                "Review rebon-cli runner render module",
                "explore-cli",
                "Explore",
            ),
        ])),
    );

    let live = HashMap::from([(
        "toolu_agent_1".to_string(),
        LiveAgentToolActivity {
            text: None,
            status: LiveAgentToolStatus::Running,
            title: Some("Review rebon-tui render module".to_string()),
            display_name: Some("explore-tui".to_string()),
            start_time_ms: Some(1_000_000),
            end_time_ms: None,
            tool_use_count: None,
            token_count: Some(30_600),
            terminal_result: None,
        },
    )]);
    let extras = TranscriptRenderExtras {
        live_agent_tool_activity: &live,
        live_activity_revision: 1,
        leading_segment_margin: true,
        ..TranscriptRenderExtras::empty()
    };
    let area = Rect::new(0, 0, 24, 4);
    let mut buf = new_buf(24, 4);
    let mut cache = TranscriptMeasureCache::new();

    render_transcript_cached_with_running_hints_at(
        &s,
        area,
        &mut buf,
        &RenderTheme::plain(),
        1,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        extras,
        1_034_000,
    );
    assert_eq!(cache.clipped_segment_hits, 0);
    let first = all_text(&buf);
    assert!(!first.contains("30.6k tokens"), "{first:?}");

    render_transcript_cached_with_running_hints_at(
        &s,
        area,
        &mut buf,
        &RenderTheme::plain(),
        1,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        extras,
        1_035_000,
    );
    assert_eq!(cache.clipped_segment_hits, 1);
}

#[test]
fn committed_async_agent_group_verbose_expands_full_agent_details() {
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_async_agent_launch_batch(vec![
            (
                "toolu_agent_1",
                "agent-task-1",
                "Review rebon-tui render module",
                "review-tui-render",
                "Explore",
            ),
            (
                "toolu_agent_2",
                "agent-task-2",
                "Review rebon-cli runner render module",
                "review-cli-render",
                "Explore",
            ),
        ])),
    );

    let mut buf = new_buf(180, 32);
    let mut cache = TranscriptMeasureCache::new();
    render_transcript_cached_with_running_hints(
        &s,
        Rect::new(0, 0, 180, 32),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Verbose,
        0,
        None,
        &mut cache,
        false,
        TranscriptRenderExtras::empty(),
    );
    let snap = all_text(&buf);

    assert!(!snap.contains("2 background agents launched"), "{snap:?}");
    assert!(snap.contains("review-tui-render"), "{snap:?}");
    assert!(snap.contains("review-cli-render"), "{snap:?}");
    assert!(
        snap.contains("review-tui-render(Review rebon-tui render module)"),
        "{snap:?}"
    );
    assert!(
        snap.contains("Prompt:") && snap.contains("Review rebon-tui render module"),
        "{snap:?}"
    );
    assert!(snap.contains("Launched"), "{snap:?}");
    assert!(
        snap.contains("review-cli-render(Review rebon-cli runner render module)"),
        "{snap:?}"
    );
    assert!(
        snap.contains("Review rebon-cli runner render module"),
        "{snap:?}"
    );
    assert!(!snap.contains("Agent id:"), "{snap:?}");
    assert!(!snap.contains("Status: async_launched"), "{snap:?}");
}

#[test]
fn committed_agent_after_hidden_thinking_has_single_gap_and_selectable_output() {
    use crate::selection::SelectionState;
    use crate::state::{reducer, Action};

    let mut state = AppState::new();
    reducer(
        &mut state,
        Action::Commit(user("u-agent", "summarize this")),
    );
    reducer(
        &mut state,
        Action::Commit(assistant_thinking_only(
            "a-thinking",
            "considering the best agent",
        )),
    );
    reducer(
        &mut state,
        Action::Commit(committed_agent_tool("a-agent", "toolu_agent")),
    );

    let width = 100u16;
    let height = 10u16;
    let area = Rect::new(0, 0, width, height);
    let mut buf = new_buf(width, height);
    let mut cache = TranscriptMeasureCache::new();
    render_transcript_cached_with_running_hints(
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
        TranscriptRenderExtras::empty(),
    );

    let rows = all_rows(&buf);
    let user_row = row_y_containing(&rows, "summarize this");
    let agent_row = row_y_containing(&rows, "Explore: inspect render path");
    assert_eq!(
        agent_row,
        user_row + 2,
        "hidden thinking left more than the normal one-row transcript gap: {rows:?}"
    );
    assert!(
        rows[user_row + 1].trim().is_empty(),
        "normal inter-message gap missing: {rows:?}"
    );
    assert!(
        !all_text(&buf).contains("considering the best agent"),
        "hidden thinking leaked into committed transcript: {rows:?}"
    );

    let mut selection = SelectionState::new();
    selection.start(0, agent_row as u16);
    selection.update(width.saturating_sub(1), agent_row as u16);
    let selected = selection.get_selected_text(&buf, area);
    assert!(
        selected.contains("Explore: inspect render path"),
        "committed agent row must be present in selectable buffer; selected={selected:?}, rows={rows:?}"
    );
}

#[test]
fn committed_agent_after_hidden_task_residue_keeps_agent_and_reply_visible() {
    use crate::selection::SelectionState;
    use crate::state::{reducer, Action};

    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(user("u-start", "coordinate the renderer investigation")),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_thinking_only(
            "a-task-residue",
            "hidden TaskCreate residue before launching Agent",
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_async_agent_launch("a-agent", "toolu_agent")),
    );
    reducer(
        &mut s,
        Action::Commit(user(
            "u-task-note",
            "<task-notification>\n<summary>Agent created: Research external file inputs</summary>\n<status>running</status>\n</task-notification>",
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_text(
            "a-reply",
            "Coordinator reply after Agent launch is still visible.",
        )),
    );

    let width = 120u16;
    let height = 18u16;
    let area = Rect::new(0, 0, width, height);
    let mut buf = new_buf(width, height);
    let mut cache = TranscriptMeasureCache::new();
    let result = render_transcript_cached_with_running_hints(
        &s,
        area,
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

    let rows = all_rows(&buf);
    let snap = all_text(&buf);
    assert!(
        snap.contains("Research external file inputs"),
        "async Agent card/description was swallowed by hidden thinking collapse: {rows:?}"
    );
    assert!(
        snap.contains("Coordinator reply after Agent launch is still visible."),
        "assistant reply after async Agent launch disappeared: {rows:?}"
    );
    assert!(
        snap.contains("Agent created: Research external file inputs"),
        "coordinator-style task notification boundary should still render: {rows:?}"
    );
    assert!(
        !snap.contains("hidden TaskCreate residue before launching Agent"),
        "hidden thinking-only Task tool residue leaked instead of being collapsed: {rows:?}"
    );

    let agent_row = row_y_containing(&rows, "Research external file inputs");
    let reply_row = row_y_containing(
        &rows,
        "Coordinator reply after Agent launch is still visible.",
    );
    assert!(
        agent_row < reply_row,
        "non-thinking Agent row should be emitted before later assistant text, not skipped: {rows:?}"
    );

    let mut agent_selection = SelectionState::new();
    agent_selection.start(0, agent_row as u16);
    agent_selection.update(width.saturating_sub(1), agent_row as u16);
    let selected_agent = agent_selection.get_selected_text(&buf, area);
    assert!(
        selected_agent.contains("Research external file inputs"),
        "visible Agent row must be selectable; selected={selected_agent:?}, rows={rows:?}"
    );

    let mut reply_selection = SelectionState::new();
    reply_selection.start(0, reply_row as u16);
    reply_selection.update(width.saturating_sub(1), reply_row as u16);
    let selected_reply = reply_selection.get_selected_text(&buf, area);
    assert!(
        selected_reply.contains("Coordinator reply after Agent launch is still visible."),
        "visible assistant reply must be selectable; selected={selected_reply:?}, rows={rows:?}"
    );
    assert!(
        result.total_lines <= height as usize,
        "fixture should fit without scroll hiding rows; result={result:?}, rows={rows:?}"
    );
}

#[test]
fn committed_collapsed_group_uses_past_tense_and_omits_per_tool_cards() {
    // Two Grep tool_uses + one Read tool_use, each with a matching
    // tool_result. The run must collapse into a single past-tense
    // summary — "Searched for 2 patterns, read 1 file" — with the
    // per-tool cards suppressed.
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
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a2",
            vec![("toolu_3", "Read", json!({ "file_path": "src/lib.rs" }))],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u2",
            vec![("toolu_3", "contents of src/lib.rs", None)],
        )),
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
        snap.contains("Searched for 2 patterns"),
        "missing past-tense search summary: {snap:?}"
    );
    assert!(
        snap.contains("read 1 file"),
        "missing past-tense read summary: {snap:?}"
    );
    // Active-group ellipsis is suppressed for committed groups.
    assert!(
        !snap.contains("Searching for"),
        "present-tense leaked into committed summary: {snap:?}"
    );
    // The per-tool card headers must be suppressed — the whole
    // point of the committed collapse is to replace them.
    assert!(
        !snap.contains("Grep(pattern=foo)"),
        "Grep per-tool card leaked: {snap:?}"
    );
    assert!(
        !snap.contains("Read(file_path=src/lib.rs)"),
        "Read per-tool card leaked: {snap:?}"
    );
    // The tool_use_id should not show up either — tool_result
    // rows collapse into the same summary.
    assert!(
        !snap.contains("toolu_1"),
        "tool_result card leaked: {snap:?}"
    );
}

#[test]
fn streaming_collapsed_hint_lines_only_render_for_latest_segment() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t1",
        "Grep",
        ToolKind::Search,
        ToolCallStatus::Completed,
        vec![("pattern", json!("old-pattern"))],
    ));
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t2",
        "Read",
        ToolKind::Read,
        ToolCallStatus::Completed,
        vec![("file_path", json!("old.rs"))],
    ));
    overlay.append_streaming_text("assistant text splits groups");
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t3",
        "Grep",
        ToolKind::Search,
        ToolCallStatus::Completed,
        vec![("pattern", json!("new-pattern"))],
    ));
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t4",
        "Read",
        ToolKind::Read,
        ToolCallStatus::Completed,
        vec![("file_path", json!("new.rs"))],
    ));

    let mut buf = new_buf(120, 16);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 16),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..16)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        snap.contains("Searched for 1 pattern, read 1 file"),
        "both collapsed headers should remain visible: {snap:?}"
    );
    assert!(
        snap.contains("assistant text splits groups"),
        "text divider should render between groups: {snap:?}"
    );
    assert!(
        snap.contains("new.rs"),
        "latest collapsed segment should render its under-header hint: {snap:?}"
    );
    assert!(
        !snap.contains("old.rs"),
        "earlier collapsed segment hint should be replaced: {snap:?}"
    );
    assert_eq!(
        snap.matches("⎿").count(),
        1,
        "only the latest collapsed segment should have a hint gutter: {snap:?}"
    );
}

#[test]
fn streaming_collapsed_hint_lines_render_when_latest_visible_segment_is_collapsed() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t1",
        "Grep",
        ToolKind::Search,
        ToolCallStatus::Completed,
        vec![("pattern", json!("old-pattern"))],
    ));
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t2",
        "Read",
        ToolKind::Read,
        ToolCallStatus::Completed,
        vec![("file_path", json!("old.rs"))],
    ));
    overlay.append_streaming_text("assistant text splits groups");
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t3",
        "Grep",
        ToolKind::Search,
        ToolCallStatus::Completed,
        vec![("pattern", json!("new-pattern"))],
    ));
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t4",
        "Read",
        ToolKind::Read,
        ToolCallStatus::Completed,
        vec![("file_path", json!("new.rs"))],
    ));

    let mut buf = new_buf(120, 16);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 120, 16),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..16)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        snap.contains("new.rs"),
        "latest collapsed segment should render its under-header hint: {snap:?}"
    );
    assert!(
        !snap.contains("old.rs"),
        "earlier collapsed segment hint should be replaced: {snap:?}"
    );
    assert_eq!(
        snap.matches("⎿").count(),
        1,
        "only the latest visible collapsed segment should have a hint gutter: {snap:?}"
    );
}

#[test]
fn streaming_collapsed_hint_lines_persist_after_later_assistant_text() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t1",
        "Grep",
        ToolKind::Search,
        ToolCallStatus::Completed,
        vec![("pattern", json!("pattern"))],
    ));
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t2",
        "Read",
        ToolKind::Read,
        ToolCallStatus::Completed,
        vec![("file_path", json!("crates/rebon-tui/src/streaming.rs"))],
    ));
    overlay.upsert_streaming_tool_use(streaming_tool(
        "t3",
        "Read",
        ToolKind::Read,
        ToolCallStatus::Completed,
        vec![("file_path", json!("crates/rebon-tui/src/render.rs"))],
    ));
    overlay.append_streaming_text("Here is the answer after the read/search run.");

    let mut buf = new_buf(140, 12);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 140, 12),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..12)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        snap.contains("Searched for 1 pattern, read 2 files"),
        "collapsed header should remain visible: {snap:?}"
    );
    assert!(
        snap.contains("Here is the answer after the read/search run."),
        "later assistant text should render: {snap:?}"
    );
    assert!(
        snap.contains("crates/rebon-tui/src/render.rs"),
        "latest collapsed hint should persist after later assistant text: {snap:?}"
    );
    assert!(
        !snap.contains("crates/rebon-tui/src/streaming.rs"),
        "only the latest read hint should render: {snap:?}"
    );
    assert!(
        snap.contains("⎿"),
        "collapsed group followed by assistant text should keep the hint gutter: {snap:?}"
    );
}

/// Pins the bug fix for "trailing tool snap": when a single
/// completed tool sits at the tail of the streaming overlay (with
/// a previously-committed tool from an earlier turn already in
/// the transcript), `FlushSealedPrefix` must NOT promote the
/// trailing tool — otherwise it lands in the committed transcript
/// where `build_transcript_segments` would suddenly merge it with
/// the prior tool into a Collapsed group, producing the visual
/// snap-into-group flicker the user reported. The Read card must
/// stay rendered as itself across the flush attempt.
#[test]
fn flush_does_not_snap_trailing_tool_into_prior_committed_group() {
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(&mut s, Action::Commit(user("u0", "please inspect files")));
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a1",
            vec![("toolu_1", "Grep", json!({ "pattern": "old-pattern" }))],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u1",
            vec![("toolu_1", "matches for old-pattern", None)],
        )),
    );

    let mut frame_one = new_buf(120, 12);
    let mut cache = TranscriptMeasureCache::new();
    reducer(
        &mut s,
        Action::StartToolUse {
            call_id: "toolu_2".into(),
            tool_name: "Read".into(),
            kind: ToolKind::Read,
            initial_status: ToolCallStatus::Completed,
            initial_title: None,
            raw_input: Some(HashMap::from([(
                "file_path".into(),
                json!("crates/rebon-tui/src/render.rs"),
            )])),
            content: None,
            locations: None,
            raw_output: None,
        },
    );
    render_transcript_cached_with_running_hints(
        &s,
        Rect::new(0, 0, 120, 12),
        &mut frame_one,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        true,
        TranscriptRenderExtras::empty(),
    );
    let pre_flush_snap = all_text(&frame_one);
    assert!(
        pre_flush_snap.contains("Read"),
        "new single Read card should render: {pre_flush_snap:?}"
    );
    assert!(
        pre_flush_snap.contains("crates/rebon-tui/src/render.rs"),
        "single Read card should show its path: {pre_flush_snap:?}"
    );

    let transcript_len_before = s.transcript.len();
    let overlay_tools_before = s.overlay.tool_use_count();
    reducer(
        &mut s,
        Action::FlushSealedPrefix {
            commit_timestamp: "t".into(),
            policy: SealedPrefixFlushPolicy::HoldBackTrailingToolCluster,
        },
    );
    assert_eq!(
        s.transcript.len(),
        transcript_len_before,
        "trailing tool must NOT flush — would otherwise snap into prior collapsed group"
    );
    assert_eq!(
        s.overlay.tool_use_count(),
        overlay_tools_before,
        "Read must remain in the streaming overlay across flush attempts"
    );

    let mut frame_two = new_buf(120, 12);
    render_transcript_cached_with_running_hints(
        &s,
        Rect::new(0, 0, 120, 12),
        &mut frame_two,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        true,
        TranscriptRenderExtras::empty(),
    );
    let post_flush_snap = all_text(&frame_two);

    assert!(
        !post_flush_snap.contains("Searched for 1 pattern, read 1 file"),
        "committed Grep and trailing overlay Read must not merge into a Collapsed group: {post_flush_snap:?}"
    );
    assert!(
        post_flush_snap.contains("crates/rebon-tui/src/render.rs"),
        "Read card path should still be visible: {post_flush_snap:?}"
    );
}

#[test]
fn committed_collapsed_group_forces_terminal_status_for_progressive_scrollback() {
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses_with_status(
            "a1",
            vec![
                ("toolu_1", "Grep", json!({ "pattern": "old-pattern" })),
                ("toolu_2", "Read", json!({ "file_path": "old.rs" })),
            ],
            Some(ToolCallStatus::InProgress),
        )),
    );

    let mut buf = new_buf(120, 8);
    render_transcript(
        &s,
        Rect::new(0, 0, 120, 8),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
    );
    let snap = all_rows(&buf).join("\n");

    assert!(
        snap.contains("Searched for 1 pattern, read 1 file"),
        "progressively committed tool summaries must be immutable/past-tense in scrollback: {snap:?}"
    );
    assert!(
        !snap.contains("Searching for") && !snap.contains("reading 1 file…"),
        "present-tense running summary leaked into committed scrollback: {snap:?}"
    );
}

#[test]
fn committed_historical_collapsed_group_suppresses_hint_lines_after_later_assistant() {
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a1",
            vec![
                ("toolu_1", "Grep", json!({ "pattern": "old-pattern" })),
                ("toolu_2", "Read", json!({ "file_path": "old.rs" })),
            ],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u1",
            vec![
                ("toolu_1", "matches for old-pattern", None),
                ("toolu_2", "contents of old.rs", None),
            ],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(assistant_text("a2", "later assistant text")),
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
        snap.contains("Searched for 1 pattern, read 1 file"),
        "collapsed historical header should remain visible: {snap:?}"
    );
    assert!(
        snap.contains("later assistant text"),
        "later assistant message should render: {snap:?}"
    );
    assert!(
        !snap.contains("old.rs"),
        "historical collapsed under-header hint should be suppressed: {snap:?}"
    );
    assert!(
        !snap.contains("⎿"),
        "historical committed collapsed group should not render hint gutter: {snap:?}"
    );
}

#[test]
fn running_turn_keeps_collapsed_thinking_out_of_reasoning_groups() {
    let mut state = AppState::new();
    for message in [
        assistant_thinking_only("a-think-1", "first reasoning"),
        assistant_tool_uses(
            "a-read-1",
            vec![("toolu-read-1", "Read", json!({"file_path": "src/first.rs"}))],
        ),
        user_tool_results("u-read-1", vec![("toolu-read-1", "ok", None)]),
        assistant_thinking_only("a-think-2", "second reasoning"),
        assistant_tool_uses(
            "a-read-2",
            vec![(
                "toolu-read-2",
                "Read",
                json!({"file_path": "src/latest.rs"}),
            )],
        ),
        user_tool_results("u-read-2", vec![("toolu-read-2", "ok", None)]),
    ] {
        reducer(&mut state, Action::Commit(message));
    }

    let mut cache = TranscriptMeasureCache::new();
    let mut buf = new_buf(120, 12);
    render_transcript_cached_with_running_hints(
        &state,
        Rect::new(0, 0, 120, 12),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        true,
        TranscriptRenderExtras::empty(),
    );
    let snap = all_text(&buf);

    assert!(!snap.contains("Reasoning ("), "{snap:?}");
    assert!(!snap.contains("first reasoning"), "{snap:?}");
    assert!(!snap.contains("second reasoning"), "{snap:?}");
    assert!(snap.contains("src/latest.rs"), "{snap:?}");
    assert_eq!(snap.matches('⎿').count(), 1, "{snap:?}");
}

#[test]
fn running_turn_shows_latest_committed_collapsed_hint_lines() {
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(&mut s, Action::Commit(user("u0", "please inspect files")));
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a1",
            vec![
                ("toolu_1", "Grep", json!({ "pattern": "old-pattern" })),
                ("toolu_2", "Read", json!({ "file_path": "old.rs" })),
            ],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u1",
            vec![
                ("toolu_1", "matches for old-pattern", None),
                ("toolu_2", "contents of old.rs", None),
            ],
        )),
    );

    let mut buf = new_buf(120, 12);
    let mut cache = TranscriptMeasureCache::new();
    render_transcript_cached_with_running_hints(
        &s,
        Rect::new(0, 0, 120, 12),
        &mut buf,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        true,
        TranscriptRenderExtras::empty(),
    );
    let snap = all_text(&buf);

    assert!(
        snap.contains("Searched for 1 pattern, read 1 file"),
        "collapsed running-turn header should remain visible: {snap:?}"
    );
    assert!(
        snap.contains("old.rs"),
        "latest committed collapsed group should show hint while turn is running: {snap:?}"
    );
    assert!(
        snap.contains("⎿"),
        "running committed collapsed group should render hint gutter: {snap:?}"
    );
}

#[test]
fn running_turn_hint_cache_switches_off_when_idle() {
    use crate::state::{reducer, Action};
    let mut s = AppState::new();
    reducer(&mut s, Action::Commit(user("u0", "please inspect files")));
    reducer(
        &mut s,
        Action::Commit(assistant_tool_uses(
            "a1",
            vec![
                ("toolu_1", "Grep", json!({ "pattern": "old-pattern" })),
                ("toolu_2", "Read", json!({ "file_path": "old.rs" })),
            ],
        )),
    );
    reducer(
        &mut s,
        Action::Commit(user_tool_results(
            "u1",
            vec![
                ("toolu_1", "matches for old-pattern", None),
                ("toolu_2", "contents of old.rs", None),
            ],
        )),
    );

    let mut cache = TranscriptMeasureCache::new();
    let mut with_hint = new_buf(120, 12);
    render_transcript_cached_with_running_hints(
        &s,
        Rect::new(0, 0, 120, 12),
        &mut with_hint,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        true,
        TranscriptRenderExtras::empty(),
    );
    assert!(all_text(&with_hint).contains("old.rs"));

    let mut idle = new_buf(120, 12);
    render_transcript_cached_with_running_hints(
        &s,
        Rect::new(0, 0, 120, 12),
        &mut idle,
        &RenderTheme::plain(),
        0,
        ToolOutputVerbosity::Compact,
        0,
        None,
        &mut cache,
        false,
        TranscriptRenderExtras::empty(),
    );
    let snap = all_text(&idle);

    assert!(
        !snap.contains("old.rs"),
        "cached hinted height must not leak after the turn stops: {snap:?}"
    );
    assert!(
        !snap.contains("⎿"),
        "cached hinted render must not leak hint gutter after the turn stops: {snap:?}"
    );
}

#[test]
fn background_shell_bash_card_shows_launch_confirmation() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "background-bash-1",
            "Bash",
            ToolKind::Execute,
            ToolCallStatus::Completed,
            vec![
                ("command", json!("cargo test --workspace")),
                ("run_in_background", json!(true)),
            ],
        ),
        vec![
            ("shellId", json!("sh_0543b24f461cf722bf74f079ccad026e")),
            ("tool", json!("Bash")),
            ("command", json!("cargo test --workspace")),
            ("status", json!("running")),
            ("startedAtMs", json!(42)),
            ("completedAtMs", Value::Null),
            ("timeoutMs", Value::Null),
            ("exitCode", Value::Null),
        ],
    ));

    let mut buf = new_buf(100, 4);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = all_text(&buf);

    assert!(snap.contains("Bash (cargo test --workspace)"), "{snap:?}");
    assert!(snap.contains("⎿ Background shell launched"), "{snap:?}");
    assert!(
        !snap.contains("sh_0543b24f461cf722bf74f079ccad026e"),
        "{snap:?}"
    );
    assert!(!snap.contains("startedAtMs"), "{snap:?}");
}

#[test]
fn shell_output_card_shows_shell_id_header_and_output_body() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "shell-output-1",
            "ShellOutput",
            ToolKind::Execute,
            ToolCallStatus::Completed,
            vec![
                ("shellId", json!("sh_0543b24f461cf722bf74f079ccad026e")),
                ("wait", json!(true)),
                ("timeout", json!(30000)),
            ],
        ),
        vec![
            ("shellId", json!("sh_0543b24f461cf722bf74f079ccad026e")),
            ("command", json!("cargo build --release")),
            ("status", json!("exited")),
            ("completed", json!(true)),
            ("exitCode", json!(0)),
            ("nextCursor", json!(2)),
            ("output", json!("build finished\n")),
        ],
    ));

    let mut buf = new_buf(100, 6);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 100, 6),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..6)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    // Header shows the command that started the shell (echoed back in the
    // result), not the opaque shellId or an input-field dump.
    assert!(
        snap.contains("ShellOutput (cargo build --release)"),
        "{snap:?}"
    );
    assert!(
        !snap.contains("sh_0543b24f461cf722bf74f079ccad026e"),
        "{snap:?}"
    );
    assert!(!snap.contains("timeout=30000"), "{snap:?}");
    // Body is the status line followed by real output text — never a
    // key=value dump of the raw result map.
    assert!(snap.contains("exited (code 0)"), "{snap:?}");
    assert!(snap.contains("build finished"), "{snap:?}");
    assert!(!snap.contains("nextCursor="), "{snap:?}");
    assert!(!snap.contains("status=exited"), "{snap:?}");
}

#[test]
fn shell_output_poll_without_new_output_renders_single_status_line() {
    let mut overlay = StreamingOverlay::new();
    overlay.upsert_streaming_tool_use(streaming_tool_with_raw_output(
        streaming_tool(
            "shell-output-2",
            "ShellOutput",
            ToolKind::Execute,
            ToolCallStatus::Completed,
            vec![("shellId", json!("sh_1"))],
        ),
        vec![
            ("shellId", json!("sh_1")),
            ("status", json!("running")),
            ("completed", json!(false)),
            ("nextCursor", json!(4)),
            ("waitTimedOut", json!(true)),
        ],
    ));

    let mut buf = new_buf(80, 4);
    render_streaming_overlay(
        &overlay,
        Rect::new(0, 0, 80, 4),
        &mut buf,
        &RenderTheme::plain(),
        ToolOutputVerbosity::Compact,
    );
    let snap = (0..4)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(snap.contains("ShellOutput (sh_1)"), "{snap:?}");
    assert!(snap.contains("running · no new output"), "{snap:?}");
    assert!(!snap.contains("waitTimedOut"), "{snap:?}");
}

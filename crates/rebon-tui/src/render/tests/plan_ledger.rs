use super::super::*;
use super::common::{all_text, new_buf};
use crate::message::{
    AssistantContentBlock, AssistantMessage, AssistantMessageInner, AssistantRole,
    AssistantTextBlock, AssistantToolUseBlock, Message,
};
use crate::StreamingToolUse;
use serde_json::{json, Value};
use std::collections::HashMap;

fn requirements() -> Value {
    json!([
        { "id": "R1", "title": "支持中文需求自动换行", "source": "question", "roundAdded": 1 },
        { "id": "R2", "title": "Keep compact headers readable", "source": "question", "roundAdded": 1 },
        { "id": "R3", "title": "Render committed transcripts", "source": "question", "roundAdded": 1 },
        { "id": "R4", "title": "Render streaming cards", "source": "question", "roundAdded": 1 },
        { "id": "R5", "title": "Hide raw manifest data", "source": "question", "roundAdded": 1 },
        { "id": "R6", "title": "Preserve failure text", "source": "question", "roundAdded": 1 }
    ])
}

fn result(sealed: bool) -> Value {
    json!({
        "requirements": requirements(),
        "manifest": { "sourcePath": "secret-manifest", "hash": "manifest-hash" },
        "round": 3,
        "interviewRevision": 2,
        "understandingSeal": sealed.then(|| json!({ "hash": "seal-hash" }))
    })
}

fn object_map(value: Value) -> HashMap<String, Value> {
    value
        .as_object()
        .expect("object")
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn text_content(text: &str) -> ToolCallContent {
    ToolCallContent::Content(rebon_types::RegularContent {
        content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: text.to_string(),
            annotations: None,
        }),
    })
}

fn streaming_plan_ledger(
    operation: &str,
    items: Option<Value>,
    status: ToolCallStatus,
    output: Option<Value>,
) -> StreamingToolUse {
    let mut input = json!({ "operation": operation });
    if let Some(items) = items {
        input["items"] = items;
    }
    StreamingToolUse {
        call_id: format!("plan-ledger-{operation}"),
        tool_name: PLAN_LEDGER_TOOL_NAME.to_string(),
        kind: ToolKind::Other,
        status,
        title: Some("raw title must not render".to_string()),
        content: None,
        locations: None,
        raw_input: Some(object_map(input)),
        raw_output: output.map(object_map),
    }
}

fn deferred_streaming_plan_ledger(mut tool: StreamingToolUse) -> StreamingToolUse {
    let arguments = tool
        .raw_input
        .take()
        .map(|input| Value::Object(input.into_iter().collect()))
        .unwrap_or_else(|| json!({}));
    tool.tool_name = "InvokeDeferredTool".to_string();
    tool.raw_input = Some(object_map(json!({
        "tool_name": PLAN_LEDGER_TOOL_NAME,
        "arguments": arguments
    })));
    tool
}

fn render_streaming_plan_ledger(
    tool: StreamingToolUse,
    verbosity: ToolOutputVerbosity,
    width: u16,
) -> String {
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
    all_text(&buf)
}

fn committed_plan_ledger(
    operation: &str,
    items: Option<Value>,
    status: ToolCallStatus,
    output: Option<Value>,
    compound: bool,
) -> Message {
    let mut input = json!({ "operation": operation });
    if let Some(items) = items {
        input["items"] = items;
    }
    let tool = AssistantContentBlock::ToolUse(AssistantToolUseBlock {
        id: format!("plan-ledger-{operation}"),
        name: PLAN_LEDGER_TOOL_NAME.to_string(),
        input,
        tool_call_content: None,
        raw_output: output,
        title: Some("raw title must not render".to_string()),
        locations: None,
        status: Some(status),
    });
    let content = if compound {
        vec![
            AssistantContentBlock::Text(AssistantTextBlock {
                text: "Ledger update follows".to_string(),
            }),
            tool,
        ]
    } else {
        vec![tool]
    };
    Message::Assistant(AssistantMessage {
        uuid: format!("assistant-{operation}"),
        timestamp: "t".to_string(),
        message: AssistantMessageInner {
            role: AssistantRole::Assistant,
            content,
        },
        is_api_error_message: None,
        advisor_model: None,
        is_stream_continuation: None,
    })
}

fn defer_committed_plan_ledger(mut message: Message) -> Message {
    let Message::Assistant(assistant) = &mut message else {
        panic!("assistant message");
    };
    for block in &mut assistant.message.content {
        if let AssistantContentBlock::ToolUse(tool) = block {
            let arguments = std::mem::replace(&mut tool.input, json!({}));
            tool.name = "InvokeDeferredTool".to_string();
            tool.input = json!({
                "tool_name": PLAN_LEDGER_TOOL_NAME,
                "arguments": arguments
            });
        }
    }
    message
}

fn set_committed_plan_ledger_content(mut message: Message, text: &str) -> Message {
    let Message::Assistant(assistant) = &mut message else {
        panic!("assistant message");
    };
    for block in &mut assistant.message.content {
        if let AssistantContentBlock::ToolUse(tool) = block {
            tool.tool_call_content = Some(vec![text_content(text)]);
            tool.raw_output = None;
        }
    }
    message
}

fn render_committed_plan_ledger(
    message: Message,
    verbosity: ToolOutputVerbosity,
    width: u16,
) -> String {
    let mut buf = new_buf(width, 30);
    render_message(
        &message,
        Rect::new(0, 0, width, 30),
        &mut buf,
        &RenderTheme::plain(),
        verbosity,
    );
    all_text(&buf)
}

fn assert_raw_plan_ledger_fields_hidden(rendered: &str) {
    for hidden in [
        "secret-manifest",
        "manifest-hash",
        "seal-hash",
        "roundAdded",
        "source=",
        "requirements=",
        "items=",
        "raw title must not render",
    ] {
        assert!(
            !rendered.contains(hidden),
            "{hidden:?} leaked: {rendered:?}"
        );
    }
}

fn without_whitespace(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

#[test]
fn plan_ledger_streaming_headers_cover_all_operations() {
    let cases = [
        ("set_requirements", Some(requirements()), false, 6),
        ("add", Some(requirements()), false, 6),
        ("list", None, false, 6),
        ("seal_understanding", None, true, 6),
    ];

    for (operation, items, sealed, count) in cases {
        let rendered = render_streaming_plan_ledger(
            streaming_plan_ledger(
                operation,
                items,
                ToolCallStatus::Completed,
                Some(result(sealed)),
            ),
            ToolOutputVerbosity::Compact,
            100,
        );
        assert!(
            rendered.contains(&format!("PlanLedger ({operation} · {count} items)")),
            "{operation}: {rendered:?}"
        );
        assert!(!rendered.contains("items=["), "{operation}: {rendered:?}");
        assert_raw_plan_ledger_fields_hidden(&rendered);
    }
}

#[test]
fn deferred_plan_ledger_streaming_uses_structured_result() {
    let rendered = render_streaming_plan_ledger(
        deferred_streaming_plan_ledger(streaming_plan_ledger(
            "list",
            None,
            ToolCallStatus::Completed,
            Some(result(false)),
        )),
        ToolOutputVerbosity::Verbose,
        100,
    );

    assert!(
        rendered.contains("PlanLedger (list · 6 items)"),
        "{rendered:?}"
    );
    assert!(
        rendered.contains("R6  Preserve failure text"),
        "{rendered:?}"
    );
    assert!(
        rendered.contains("listed · round 3 · interview revision 2"),
        "{rendered:?}"
    );
    assert_raw_plan_ledger_fields_hidden(&rendered);
}

#[test]
fn deferred_plan_ledger_committed_failures_never_render_raw_json() {
    for compound in [false, true] {
        let message = committed_plan_ledger(
            "add",
            Some(requirements()),
            ToolCallStatus::Failed,
            None,
            compound,
        );
        let message = set_committed_plan_ledger_content(
            defer_committed_plan_ledger(message),
            r#"{"error":{"message":"invalid deferred requirement","hash":"secret-hash"},"requirements":[{"id":"R1"}],"manifest":"secret-manifest"}"#,
        );
        let rendered = render_committed_plan_ledger(message, ToolOutputVerbosity::Verbose, 100);

        assert!(
            rendered.contains("invalid deferred requirement"),
            "{rendered:?}"
        );
        for hidden in [
            "updated",
            "listed",
            "sealed",
            "secret-hash",
            "secret-manifest",
            "requirements",
            "{\"error\"",
        ] {
            assert!(
                !rendered.contains(hidden),
                "compound={compound}, {hidden:?} leaked: {rendered:?}"
            );
        }
    }
}

#[test]
fn committed_plan_ledger_recovers_structured_legacy_result_text() {
    let result_text = result(false).to_string();
    for deferred in [false, true] {
        let message = committed_plan_ledger("list", None, ToolCallStatus::Completed, None, true);
        let message = if deferred {
            defer_committed_plan_ledger(message)
        } else {
            message
        };
        let rendered = render_committed_plan_ledger(
            set_committed_plan_ledger_content(message, &result_text),
            ToolOutputVerbosity::Verbose,
            100,
        );

        assert!(
            rendered.contains("PlanLedger (list · 6 items)"),
            "deferred={deferred}: {rendered:?}"
        );
        assert!(
            rendered.contains("listed · round 3 · interview revision 2"),
            "deferred={deferred}: {rendered:?}"
        );
        assert!(
            rendered.contains("R6  Preserve failure text"),
            "{rendered:?}"
        );
        assert_raw_plan_ledger_fields_hidden(&rendered);
    }
}

#[test]
fn plan_ledger_compact_and_normal_cap_requirements_while_verbose_shows_all() {
    for verbosity in [ToolOutputVerbosity::Compact, ToolOutputVerbosity::Normal] {
        let rendered = render_streaming_plan_ledger(
            streaming_plan_ledger(
                "set_requirements",
                Some(requirements()),
                ToolCallStatus::Completed,
                Some(result(false)),
            ),
            verbosity,
            100,
        );
        for id in ["R1", "R2", "R3", "R4"] {
            assert!(rendered.contains(id), "{verbosity:?}: {rendered:?}");
        }
        assert!(!rendered.contains("R5  "), "{verbosity:?}: {rendered:?}");
        assert!(!rendered.contains("R6  "), "{verbosity:?}: {rendered:?}");
        assert!(
            rendered.contains("… +2 requirements"),
            "{verbosity:?}: {rendered:?}"
        );
        assert!(
            rendered.contains("updated · round 3 · interview revision 2"),
            "{verbosity:?}: {rendered:?}"
        );
        assert_raw_plan_ledger_fields_hidden(&rendered);
    }

    let rendered = render_streaming_plan_ledger(
        streaming_plan_ledger(
            "set_requirements",
            Some(requirements()),
            ToolCallStatus::Completed,
            Some(result(false)),
        ),
        ToolOutputVerbosity::Verbose,
        100,
    );
    for id in ["R1", "R2", "R3", "R4", "R5", "R6"] {
        assert!(rendered.contains(id), "{rendered:?}");
    }
    assert!(!rendered.contains("… +"), "{rendered:?}");
    assert_raw_plan_ledger_fields_hidden(&rendered);
}

#[test]
fn plan_ledger_list_renders_result_requirements_and_listed_status() {
    for verbosity in [
        ToolOutputVerbosity::Compact,
        ToolOutputVerbosity::Normal,
        ToolOutputVerbosity::Verbose,
    ] {
        let rendered = render_streaming_plan_ledger(
            streaming_plan_ledger("list", None, ToolCallStatus::Completed, Some(result(false))),
            verbosity,
            100,
        );
        assert!(
            without_whitespace(&rendered).contains("R1支持中文需求自动换行"),
            "{rendered:?}"
        );
        assert!(
            rendered.contains("listed · round 3 · interview revision 2"),
            "{rendered:?}"
        );
        assert_raw_plan_ledger_fields_hidden(&rendered);
    }
}

#[test]
fn plan_ledger_seal_only_renders_sealed_status() {
    let rendered = render_streaming_plan_ledger(
        streaming_plan_ledger(
            "seal_understanding",
            None,
            ToolCallStatus::Completed,
            Some(result(true)),
        ),
        ToolOutputVerbosity::Verbose,
        100,
    );

    assert!(
        rendered.contains("sealed · round 3 · interview revision 2"),
        "{rendered:?}"
    );
    assert!(!rendered.contains("R1  "), "{rendered:?}");
    assert!(!rendered.contains("支持中文需求"), "{rendered:?}");
    assert_raw_plan_ledger_fields_hidden(&rendered);
}

#[test]
fn plan_ledger_failed_card_preserves_error_without_success_or_raw_json() {
    let mut tool = streaming_plan_ledger(
        "add",
        Some(requirements()),
        ToolCallStatus::Failed,
        Some(json!({
            "error": { "message": "requirement id R1 already exists", "hash": "error-hash" },
            "requirements": requirements(),
            "round": 3,
            "interviewRevision": 2
        })),
    );
    tool.content = Some(vec![text_content(
        r#"{"error":{"message":"requirement id R1 already exists","hash":"content-hash"},"requirements":[{"id":"R1"}]}"#,
    )]);

    let rendered = render_streaming_plan_ledger(tool, ToolOutputVerbosity::Verbose, 100);
    assert!(
        rendered.contains("requirement id R1 already exists"),
        "{rendered:?}"
    );
    for hidden in [
        "updated",
        "listed",
        "sealed",
        "content-hash",
        "error-hash",
        "requirements",
        "interviewRevision",
        "{\"error\"",
    ] {
        assert!(
            !rendered.contains(hidden),
            "{hidden:?} leaked: {rendered:?}"
        );
    }
}

#[test]
fn plan_ledger_committed_card_uses_structured_rendering() {
    let rendered = render_committed_plan_ledger(
        committed_plan_ledger(
            "add",
            Some(requirements()),
            ToolCallStatus::Completed,
            Some(result(false)),
            false,
        ),
        ToolOutputVerbosity::Compact,
        100,
    );

    assert!(
        rendered.contains("PlanLedger (add · 6 items)"),
        "{rendered:?}"
    );
    assert!(
        without_whitespace(&rendered).contains("R1支持中文需求自动换行"),
        "{rendered:?}"
    );
    assert!(rendered.contains("… +2 requirements"), "{rendered:?}");
    assert!(
        rendered.contains("updated · round 3 · interview revision 2"),
        "{rendered:?}"
    );
    assert_raw_plan_ledger_fields_hidden(&rendered);
}

#[test]
fn plan_ledger_compound_message_uses_structured_projection_path() {
    let rendered = render_committed_plan_ledger(
        committed_plan_ledger(
            "list",
            None,
            ToolCallStatus::Completed,
            Some(result(false)),
            true,
        ),
        ToolOutputVerbosity::Normal,
        100,
    );

    assert!(rendered.contains("Ledger update follows"), "{rendered:?}");
    assert!(
        rendered.contains("PlanLedger (list · 6 items)"),
        "{rendered:?}"
    );
    assert!(
        without_whitespace(&rendered).contains("R1支持中文需求自动换行"),
        "{rendered:?}"
    );
    assert!(rendered.contains("… +2 requirements"), "{rendered:?}");
    assert!(
        rendered.contains("listed · round 3 · interview revision 2"),
        "{rendered:?}"
    );
    assert_raw_plan_ledger_fields_hidden(&rendered);
}

#[test]
fn plan_ledger_compound_failure_preserves_error_without_raw_json() {
    let rendered = render_committed_plan_ledger(
        committed_plan_ledger(
            "add",
            Some(requirements()),
            ToolCallStatus::Failed,
            Some(json!({
                "error": { "message": "requirement id R1 already exists", "hash": "compound-hash" },
                "requirements": requirements(),
                "round": 3,
                "interviewRevision": 2
            })),
            true,
        ),
        ToolOutputVerbosity::Verbose,
        100,
    );

    assert!(
        rendered.contains("requirement id R1 already exists"),
        "{rendered:?}"
    );
    for hidden in [
        "updated",
        "listed",
        "sealed",
        "compound-hash",
        "requirements",
        "interviewRevision",
        "{\"error\"",
    ] {
        assert!(
            !rendered.contains(hidden),
            "{hidden:?} leaked: {rendered:?}"
        );
    }
}

#[test]
fn plan_ledger_narrow_unicode_card_wraps_without_raw_json() {
    let rendered = render_streaming_plan_ledger(
        streaming_plan_ledger(
            "set_requirements",
            Some(requirements()),
            ToolCallStatus::Completed,
            Some(result(false)),
        ),
        ToolOutputVerbosity::Compact,
        18,
    );

    let visible = without_whitespace(&rendered);
    assert!(visible.contains("R1"), "{rendered:?}");
    assert!(visible.contains("支持中文"), "{rendered:?}");
    assert!(visible.contains("自动换行"), "{rendered:?}");
    assert!(rendered.contains("… +2"), "{rendered:?}");
    assert_raw_plan_ledger_fields_hidden(&rendered);
}

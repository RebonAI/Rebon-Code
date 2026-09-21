use super::*;

#[test]
fn acp_blocks_to_api_content_blocks_preserves_images() {
    let blocks = vec![
        AcpContentBlock::Text(TextContent {
            text: "describe this".into(),
            annotations: None,
        }),
        AcpContentBlock::Image(rebon_types::ImageContent {
            mime_type: "image/png".into(),
            data: "AAAA".into(),
            uri: None,
            annotations: None,
        }),
    ];

    let api_blocks = acp_blocks_to_api_content_blocks(&blocks);

    assert!(matches!(api_blocks[0], ApiContentBlock::Text(_)));
    match &api_blocks[1] {
        ApiContentBlock::Image(image) => {
            assert_eq!(image.source.media_type, "image/png");
            assert_eq!(image.source.data, "AAAA");
        }
        other => panic!("expected image block, got {other:?}"),
    }
}

#[test]
fn read_image_result_becomes_nested_tool_result_image_block() {
    let result = json!({
        "type": "image",
        "file": {
            "filePath": "C:/tmp/pixel.png",
            "type": "image/png",
            "base64": "AAAA"
        }
    });

    let content = read_tool_result_for_model(&result, true);

    match content {
        ToolResultContent::Blocks(blocks) => {
            assert_eq!(blocks.len(), 1);
            match &blocks[0] {
                ToolResultContentBlock::Image(image) => {
                    assert_eq!(image.source.media_type, "image/png");
                    assert_eq!(image.source.data, "AAAA");
                }
                other => panic!("expected image block, got {other:?}"),
            }
        }
        other => panic!("expected block content, got {other:?}"),
    }
}

#[test]
fn read_pdf_result_becomes_document_block_with_summary() {
    let result = json!({
        "type": "pdf",
        "file": {
            "filePath": "C:/tmp/doc.pdf",
            "base64": "JVBERi0=",
            "originalSize": 1024
        }
    });

    let content = read_tool_result_for_model(&result, true);

    match content {
        ToolResultContent::Blocks(blocks) => {
            assert_eq!(blocks.len(), 2);
            match &blocks[0] {
                ToolResultContentBlock::Text(text) => {
                    assert!(text.text.contains("PDF file read: C:/tmp/doc.pdf"));
                }
                other => panic!("expected text block, got {other:?}"),
            }
            match &blocks[1] {
                ToolResultContentBlock::Document(document) => {
                    assert_eq!(document.source.media_type, "application/pdf");
                    assert_eq!(document.source.data, "JVBERi0=");
                }
                other => panic!("expected document block, got {other:?}"),
            }
        }
        other => panic!("expected block content, got {other:?}"),
    }
}

#[test]
fn read_pdf_parts_result_becomes_summary_plus_image_blocks() {
    let result = json!({
        "type": "parts",
        "file": {
            "filePath": "C:/tmp/doc.pdf",
            "originalSize": 2048,
            "count": 2,
            "outputDir": "C:/tmp/out",
            "pages": [
                {"filePath": "C:/tmp/out/page-1.jpg", "type": "image/jpeg", "base64": "AAAA"},
                {"filePath": "C:/tmp/out/page-2.jpg", "type": "image/jpeg", "base64": "BBBB"}
            ]
        }
    });

    let content = read_tool_result_for_model(&result, true);

    match content {
        ToolResultContent::Blocks(blocks) => {
            assert_eq!(blocks.len(), 3);
            assert!(matches!(blocks[0], ToolResultContentBlock::Text(_)));
            assert!(matches!(blocks[1], ToolResultContentBlock::Image(_)));
            assert!(matches!(blocks[2], ToolResultContentBlock::Image(_)));
        }
        other => panic!("expected block content, got {other:?}"),
    }
}

#[test]
fn transcript_replay_restores_image_blocks() {
    let entries = vec![rebon_session::TranscriptEntry {
        entry_type: "user".into(),
        uuid: "u1".into(),
        parent_uuid: None,
        timestamp: None,
        raw: json!({
            "message": {
                "role": "user",
                "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                ]
            }
        }),
    }];

    let messages = transcript_to_api_messages(&entries);

    assert_eq!(messages.len(), 1);
    assert!(matches!(messages[0].content[0], ApiContentBlock::Text(_)));
    assert!(matches!(messages[0].content[1], ApiContentBlock::Image(_)));
}

#[test]
fn queued_attachment_model_message_uses_model_only_text_and_visible_text() {
    let message = ApiMessage {
            role: Role::User,
            content: vec![
                ApiContentBlock::Text(TextBlock {
                    text: "@project/module\\".into(),
                }),
                ApiContentBlock::Image(ImageBlock::base64("image/png", "AAAA")),
                ApiContentBlock::Text(TextBlock {
                    text: "<rebon-queued-user-input uuid=\"queued-1\" imagePasteIds=\"7,8\" />"
                        .into(),
                }),
                ApiContentBlock::Text(TextBlock {
                    text: "<rebon-queued-user-model-text>\n@project/module\\\n\nDirectory listing for project\\module:\nActs\\".into(),
                }),
            ],
        };

    assert_eq!(
        local_user_uuid_from_attachment(&message).as_deref(),
        Some("queued-1")
    );
    assert_eq!(local_image_paste_ids_from_attachment(&message), vec![7, 8]);

    let visible_message = visible_message_for_attachment(&message);
    assert_eq!(visible_message.content.len(), 2);
    match &visible_message.content[0] {
        ApiContentBlock::Text(text) => {
            assert_eq!(text.text, "@project/module\\");
            assert!(!text.text.contains("Directory listing"));
        }
        other => panic!("expected visible text, got {other:?}"),
    }

    let model_message = model_message_for_attachment(&message);

    assert_eq!(model_message.content.len(), 2);
    match &model_message.content[0] {
        ApiContentBlock::Text(text) => {
            assert!(text.text.starts_with("<system-reminder>"));
            assert!(text.text.contains("Directory listing for project\\module"));
            assert!(!text.text.contains("rebon-queued-user-input"));
        }
        other => panic!("expected wrapped model text, got {other:?}"),
    }
    match &model_message.content[1] {
        ApiContentBlock::Image(image) => {
            assert_eq!(image.source.media_type, "image/png");
            assert_eq!(image.source.data, "AAAA");
        }
        other => panic!("expected image block, got {other:?}"),
    }
}

#[test]
fn visible_runtime_attachment_splits_transcript_and_model_text() {
    let message = visible_runtime_attachment_message(
        "u-teammate-1",
        "<teammate-message teammate_id=\"alice\">{\"type\":\"idle_notification\",\"idleReason\":\"available\"}</teammate-message>"
            .to_string(),
        "<system-reminder>teammate completion for the model</system-reminder>".to_string(),
    );

    assert_eq!(
        local_user_uuid_from_attachment(&message).as_deref(),
        Some("u-teammate-1")
    );
    let visible = visible_message_for_attachment(&message);
    assert_eq!(visible.content.len(), 1);
    assert!(visible.content[0]
        .as_text()
        .is_some_and(|text| text.contains("idle_notification")));
    assert!(!visible.content[0]
        .as_text()
        .is_some_and(|text| text.contains("system-reminder")));

    let model = model_message_for_attachment(&message);
    assert_eq!(model.content.len(), 1);
    assert_eq!(
        model.content[0].as_text(),
        Some("<system-reminder>teammate completion for the model</system-reminder>")
    );
}

#[test]
fn transcript_replay_uses_model_content_for_queued_command() {
    let entries = vec![rebon_session::TranscriptEntry {
        entry_type: "user".into(),
        uuid: "queued-1".into(),
        parent_uuid: None,
        timestamp: None,
        raw: json!({
            "queuedCommand": true,
            "imagePasteIds": [7],
            "message": {
                "role": "user",
                "content": [
                    {"type": "text", "text": "@project/module\\"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                ]
            },
            "modelContent": [
                {"type": "text", "text": "@project/module\\\n\nDirectory listing for project\\module:\nActs\\"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
            ]
        }),
    }];

    let messages = transcript_to_api_messages(&entries);

    assert_eq!(messages.len(), 1);
    match &messages[0].content[0] {
        ApiContentBlock::Text(text) => {
            assert!(text.text.starts_with("<system-reminder>"));
            assert!(text.text.contains("Directory listing for project\\module"));
            assert!(!text.text.contains("rebon-queued-user-input"));
        }
        other => panic!("expected wrapped text, got {other:?}"),
    }
    match &messages[0].content[1] {
        ApiContentBlock::Image(image) => {
            assert_eq!(image.source.media_type, "image/png");
            assert_eq!(image.source.data, "AAAA");
        }
        other => panic!("expected image block, got {other:?}"),
    }
}

#[test]
fn transcript_replay_restores_document_blocks() {
    let entries = vec![
        rebon_session::TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: "a1".into(),
            parent_uuid: None,
            timestamp: None,
            raw: json!({
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "toolu_pdf", "name": "Read", "input": {"file_path": "doc.pdf"}}
                    ]
                }
            }),
        },
        rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u1".into(),
            parent_uuid: None,
            timestamp: None,
            raw: json!({
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_pdf",
                            "content": [
                                {"type": "text", "text": "PDF file read"},
                                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0="}}
                            ]
                        }
                    ]
                }
            }),
        },
    ];

    let messages = transcript_to_api_messages(&entries);

    assert_eq!(messages.len(), 2);
    match &messages[1].content[0] {
        ApiContentBlock::ToolResult(result) => match &result.content {
            ToolResultContent::Blocks(blocks) => {
                assert!(matches!(blocks[0], ToolResultContentBlock::Text(_)));
                assert!(matches!(blocks[1], ToolResultContentBlock::Document(_)));
            }
            other => panic!("expected block content, got {other:?}"),
        },
        other => panic!("expected tool result, got {other:?}"),
    }
}

#[test]
fn transcript_tool_result_block_omits_binary_blocks() {
    let block = ApiContentBlock::ToolResult(ToolResultBlock {
        tool_use_id: "toolu_pdf".into(),
        content: ToolResultContent::blocks(vec![
            ToolResultContentBlock::Text(TextBlock {
                text: "PDF file read".into(),
            }),
            ToolResultContentBlock::Document(DocumentBlock::base64("application/pdf", "JVBERi0=")),
            ToolResultContentBlock::Image(ImageBlock::base64("image/jpeg", "AAAA")),
        ]),
        is_error: false,
    });

    let trimmed = tool_result_block_for_transcript(&block);

    match &trimmed {
        ApiContentBlock::ToolResult(result) => match &result.content {
            ToolResultContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert!(matches!(blocks[0], ToolResultContentBlock::Text(_)));
            }
            other => panic!("expected block content, got {other:?}"),
        },
        other => panic!("expected tool result, got {other:?}"),
    }
    let serialized = serde_json::to_string(&trimmed).unwrap();
    assert!(!serialized.contains("JVBERi0="));
    assert!(!serialized.contains("AAAA"));
}

#[test]
fn transcript_tool_result_payload_omits_pdf_base64() {
    let block = ApiContentBlock::ToolResult(ToolResultBlock {
        tool_use_id: "toolu_pdf".into(),
        content: ToolResultContent::blocks(vec![
            ToolResultContentBlock::Text(TextBlock {
                text: "PDF file read".into(),
            }),
            ToolResultContentBlock::Document(DocumentBlock::base64("application/pdf", "JVBERi0=")),
        ]),
        is_error: false,
    });
    let transcript_blocks = vec![tool_result_block_for_transcript(&block)];
    let payload = build_tool_result_entry_payload(
        serde_json::to_value(&transcript_blocks).unwrap(),
        &[(
            "toolu_pdf".into(),
            json!({
                "type": "pdf",
                "file": {"filePath": "doc.pdf", "originalSize": 5}
            }),
        )],
    );

    let serialized = serde_json::to_string(&payload).unwrap();
    assert!(!serialized.contains("JVBERi0="));
    assert!(serialized.contains("PDF file read"));
    assert!(serialized.contains("originalSize"));
}

/// The auto-mode annotation is for the UI only, so it has to live beside
/// `message` and leave the model's view of the turn byte-identical.
#[test]
fn auto_mode_allowed_sidecar_stays_out_of_the_model_visible_message() {
    let block = ApiContentBlock::ToolResult(ToolResultBlock {
        tool_use_id: "toolu_auto".into(),
        content: ToolResultContent::text("ok"),
        is_error: false,
    });
    let transcript_blocks = vec![tool_result_block_for_transcript(&block)];
    let content_value = serde_json::to_value(&transcript_blocks).unwrap();

    let plain = build_tool_result_entry_payload(content_value.clone(), &[]);
    let annotated = build_tool_result_entry_payload_with_sidecars(
        content_value,
        &[],
        &[],
        &[(
            "toolu_auto".to_string(),
            rebon_types::AutoModeAllowSource::Classifier,
        )],
    );

    assert_eq!(
        annotated["message"], plain["message"],
        "the sidecar must not touch what the model reads back"
    );
    assert_eq!(
        annotated["autoModeAllowed"],
        json!([{ "id": "toolu_auto", "source": "classifier" }]),
        "the sidecar records which part of the gate allowed the call"
    );

    let empty = build_tool_result_entry_payload_with_sidecars(
        serde_json::to_value(&transcript_blocks).unwrap(),
        &[],
        &[],
        &[],
    );
    assert!(
        empty.get("autoModeAllowed").is_none(),
        "a turn with no auto-mode allows writes no sidecar at all"
    );
}

#[test]
fn ask_user_question_transcript_answer_includes_questions_and_annotations() {
    let value = json!({
        "questions": [
            {
                "question": "Which UI should we build?",
                "header": "UI",
                "multiSelect": false,
                "options": [
                    {"label": "Cards", "description": "Card layout"},
                    {"label": "Table", "description": "Table layout"}
                ]
            },
            {
                "question": "Which database should we use?",
                "header": "DB",
                "options": [
                    {"label": "Postgres", "description": "SQL"},
                    {"label": "SQLite", "description": "Embedded"}
                ]
            }
        ],
        "answers": {
            "Which database should we use?": "Postgres",
            "Which UI should we build?": "Cards"
        },
        "annotations": {
            "Which UI should we build?": {
                "preview": "<div>Cards mockup</div>",
                "notes": "Prefer dense layout"
            }
        }
    });

    let text = format_ask_user_question_answer_for_transcript(&value).unwrap();

    assert_eq!(
        text,
        "Answered questions:\n- Which UI should we build?\n  Answer: Cards\n  Selected preview:\n    <div>Cards mockup</div>\n  Notes: Prefer dense layout\n- Which database should we use?\n  Answer: Postgres"
    );
}

#[test]
fn transcript_to_api_messages_skips_transcript_only_ask_user_answers() {
    let entries = vec![
        make_user_entry("u1", "start"),
        rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: "u-ask".into(),
            parent_uuid: Some("u1".into()),
            timestamp: None,
            raw: json!({
                "type": "user",
                "uuid": "u-ask",
                "isVisibleInTranscriptOnly": true,
                "message": {
                    "role": "user",
                    "content": "Answered questions:\n- Which database should we use?\n  Answer: Postgres"
                }
            }),
        },
    ];

    let messages = transcript_to_api_messages(&entries);

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content[0].as_text(), Some("start"));
}

#[test]
fn transcript_to_api_messages_preserves_meta_internal_handoff_for_model() {
    let entries = vec![rebon_session::TranscriptEntry {
        entry_type: "user".into(),
        uuid: "u-internal-sess-1-123-0".into(),
        parent_uuid: None,
        timestamp: None,
        raw: json!({
            "type": "user",
            "uuid": "u-internal-sess-1-123-0",
            "isMeta": true,
            "message": {
                "role": "user",
                "content": "yes, continue with CEO mode"
            }
        }),
    }];

    let messages = transcript_to_api_messages(&entries);

    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].content[0].as_text(),
        Some("yes, continue with CEO mode")
    );
}

#[test]
fn execution_policy_filter_intersects_with_session_filter() {
    let session = ToolFilter::allow_only(["Read"]);
    let policy = ExecutionPolicy::ultraplan(rebon_types::UltraplanContext::planning_turn(
        "run-1",
        "plan_mode_active",
        rebon_types::PolicyMode::Observe,
    ));

    let combined = combine_filters(Some(&session), Some(&policy)).expect("combined filter");

    assert!(combined.allows("Read", &[]));
    assert!(!combined.allows("Glob", &[]));
    assert!(!combined.allows("Write", &[]));
}

#[test]
fn execution_policy_filter_allows_planning_essentials_without_session_filter() {
    let policy = ExecutionPolicy::ultraplan(rebon_types::UltraplanContext::planning_turn(
        "run-1",
        "plan_mode_active",
        rebon_types::PolicyMode::Observe,
    ));

    let filter = combine_filters(None, Some(&policy)).expect("policy filter");

    assert!(filter.allows("Agent", &[]));
    assert!(filter.allows("AskUserQuestion", &[]));
    assert!(filter.allows("ExitPlanMode", &[]));
    assert!(!filter.allows("ToolSearch", &["ToolSearchTool"]));
    assert!(!filter.allows(
        rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
        &["InvokeDeferredTool"]
    ));
    assert!(!filter.allows("Bash", &["BashTool"]));
    assert!(!filter.allows("PowerShell", &["PowerShellTool"]));
}

#[test]
fn ultraplan_policy_promotes_planning_controls_without_deferred_gateway() {
    let engine = crate::engine_with_every_builtin_tool();
    let policy = ExecutionPolicy::ultraplan(rebon_types::UltraplanContext::planning_turn(
        "run-1",
        "plan_mode_active",
        rebon_types::PolicyMode::Observe,
    ));
    let filter = combine_filters(None, Some(&policy)).expect("policy filter");

    let eager_names: Vec<_> =
        filtered_eager_tools_from_engine_for_policy(&engine, &filter, Some(&policy))
            .into_iter()
            .map(|tool| tool.name)
            .collect();
    assert!(eager_names.contains(&"Agent".to_string()));
    assert!(eager_names.contains(&"AskUserQuestion".to_string()));
    assert!(eager_names.contains(&"ExitPlanMode".to_string()));
    assert!(!eager_names.contains(&rebon_tool::TOOL_SEARCH_TOOL_NAME.to_string()));
    assert!(!eager_names.contains(&rebon_tool::INVOKE_DEFERRED_TOOL_NAME.to_string()));

    let deferred_names = engine.filtered_deferred_tool_names_for_policy(&filter, Some(&policy));
    assert!(!deferred_names.contains(&"Agent".to_string()));
    assert!(!deferred_names.contains(&"AskUserQuestion".to_string()));
    assert!(!deferred_names.contains(&"ExitPlanMode".to_string()));

    let index = engine.build_filtered_tool_search_index_for_policy(&filter, Some(&policy));
    let indexed = index.names();
    assert!(!indexed.contains(&"Agent"));
    assert!(!indexed.contains(&"AskUserQuestion"));
    assert!(!indexed.contains(&"ExitPlanMode"));
    assert!(indexed.is_empty());
}

// WebSearch is exposed on every provider route: with a provider-native
// delegate it uses server-side search, without one the tool itself
// falls back to Rebon's local search backend.
#[test]
fn web_search_stays_exposed_without_codex_oauth() {
    let engine = crate::engine_with_every_builtin_tool();
    assert!(engine
        .deferred_tool_names()
        .contains(&WEB_SEARCH_TOOL_NAME.to_string()));

    let deferred =
        deferred_tool_names_for_prompt(&engine, &engine.deferred_tool_names(), None, None, &[]);
    assert!(deferred.contains(&WEB_SEARCH_TOOL_NAME.to_string()));

    let index = engine.build_tool_search_index();
    assert!(index.names().contains(&WEB_SEARCH_TOOL_NAME));
}

#[test]
fn deferred_tool_names_for_prompt_preserves_no_policy_names() {
    let base_names = vec!["PlanOnly".to_string(), "DeniedDeferred".to_string()];
    let engine = build_engine_with_tools(vec![
        Arc::new(RecordingTool::deferred("PlanOnly", Value::Null)) as Arc<dyn Tool>,
        Arc::new(RecordingTool::deferred("DeniedDeferred", Value::Null)) as Arc<dyn Tool>,
    ]);

    let names = deferred_tool_names_for_prompt(&engine, &base_names, None, None, &[]);

    assert_eq!(names, base_names);
}

#[test]
fn deferred_tool_names_for_prompt_applies_effective_filter() {
    let engine = build_engine_with_tools(vec![
        Arc::new(RecordingTool::deferred("AskUserQuestion", Value::Null)) as Arc<dyn Tool>,
        Arc::new(RecordingTool::deferred("DeniedDeferred", Value::Null)) as Arc<dyn Tool>,
        Arc::new(RecordingTool::deferred("TeamCreate", Value::Null)) as Arc<dyn Tool>,
    ]);
    let policy = ExecutionPolicy::ultraplan(rebon_types::UltraplanContext::planning_turn(
        "run-1",
        "plan_mode_active",
        rebon_types::PolicyMode::Observe,
    ));
    let filter = combine_filters(None, Some(&policy)).expect("policy filter");

    let names = deferred_tool_names_for_prompt(
        &engine,
        &engine.deferred_tool_names(),
        Some(&filter),
        Some(&policy),
        &[],
    );

    assert!(!names.contains(&"AskUserQuestion".to_string()));
    assert!(!names.contains(&"DeniedDeferred".to_string()));
    assert!(!names.contains(&"TeamCreate".to_string()));
}

#[test]
fn build_tool_call_title_uses_specialized_agent_type() {
    let input = serde_json::Map::from_iter([
        ("subagent_type".to_string(), json!("Explore")),
        ("description".to_string(), json!("trace slash enter")),
        ("prompt".to_string(), json!("long prompt")),
    ]);

    assert_eq!(
        build_tool_call_title("Agent", &input),
        "Explore: trace slash enter"
    );
}

#[test]
fn build_tool_call_title_keeps_general_purpose_agent_label() {
    let input = serde_json::Map::from_iter([
        ("subagent_type".to_string(), json!("general-purpose")),
        ("description".to_string(), json!("inspect parser")),
    ]);

    assert_eq!(
        build_tool_call_title("Agent", &input),
        "Agent: inspect parser"
    );
}

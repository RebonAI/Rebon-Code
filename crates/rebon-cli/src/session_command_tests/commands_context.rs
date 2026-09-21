//! Terminal-level tests for `rebon_session_runtime::commands::context`.
//!
//! They live here rather than beside the code because each of them
//! builds an `AppState`, calls `crate::session_shell::session_command_inputs_from_app`, or
//! drives the TUI reducer — all three are the binary's, so a crate
//! that must not know what a terminal is cannot host them.

use crate::session::commands::context::*;

use crate::tui::app::AppState;
use crate::tui::runner::test_support::make_test_tui_session;

#[test]
fn context_command_empty_transcript() {
    let app = AppState::default();
    let session = make_test_tui_session();
    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("Context Usage"));
    assert!(output.contains("test-model"));
    assert!(output.contains("test"));
    assert!(output.contains("Prompt Sources (estimated)"));
    assert!(output.contains("system prompt:"));
    assert!(!output.contains("system prompt:   ~0 (not captured)"));
    assert!(output.contains("built-in tools:"));
    assert!(output.contains("Messages (0)"));
    assert!(output.contains("user: 0"));
    assert!(output.contains("assistant: 0"));
    assert!(!output.contains("Token Breakdown"));
    assert!(!output.contains("Top Tools"));
}

#[test]
fn context_command_shows_usage_source() {
    let app = AppState::default();
    let session = make_test_tui_session();
    session.model.prune_level.report_estimated_usage(123_456);

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(
        output.contains("(estimated)"),
        "expected usage source in /context output: {output}"
    );
}

#[test]
fn context_command_limits_transcript_scan() {
    let mut app = AppState::default();
    let session = make_test_tui_session();
    for i in 0..2005 {
        push_test_user_message(&mut app, &format!("u{i}"), "hello");
    }

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("Messages (2005, scanned latest 2000; skipped 5)"));
    assert!(output.contains("user: 2000"));
}

#[test]
fn context_command_counts_messages() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    push_test_user_message(&mut app, "u1", "Hello world");
    push_test_assistant_message(&mut app, "a1", "Hi there!");
    push_test_user_message(&mut app, "u2", "What's up?");
    push_test_assistant_message(&mut app, "a2", "Not much.");

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("Messages (4)"));
    assert!(output.contains("user: 2"));
    assert!(output.contains("assistant: 2"));
}

#[test]
fn context_command_counts_system_messages() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    push_test_user_message(&mut app, "u1", "hello");
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::System(
            rebon_render::transcript_row::SystemMessage {
                uuid: "s1".into(),
                timestamp: "2026-04-14T00:00:00.000Z".into(),
                subtype: "local_command".into(),
                content: Some("some system output".into()),
                level: None,
                is_meta: None,
            },
        )),
    );

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("Messages (2)"));
    assert!(output.contains("system: 1"));
}

#[test]
fn context_command_counts_tool_use_blocks() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    push_test_user_message(&mut app, "u1", "read file.txt");
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::Assistant(
            rebon_render::transcript_row::AssistantMessage {
                uuid: "a1".into(),
                timestamp: "2026-04-14T00:00:00.001Z".into(),
                message: rebon_render::transcript_row::AssistantMessageInner {
                    role: rebon_render::transcript_row::AssistantRole::Assistant,
                    content: vec![
                        rebon_render::transcript_row::AssistantContentBlock::Text(
                            rebon_render::transcript_row::AssistantTextBlock {
                                text: "I'll read that file.".into(),
                            },
                        ),
                        rebon_render::transcript_row::AssistantContentBlock::ToolUse(
                            rebon_render::transcript_row::AssistantToolUseBlock {
                                id: "tu-1".into(),
                                name: "Read".into(),
                                input: serde_json::json!({"file_path": "/tmp/file.txt"}),
                                tool_call_content: None,
                                raw_output: None,
                                title: None,
                                locations: None,
                                status: None,
                            },
                        ),
                    ],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            },
        )),
    );

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("tool_use: 1"));
    assert!(output.contains("text: 2"));
    assert!(output.contains("Top Tools"));
    assert!(output.contains("Read"));
}

#[test]
fn context_command_counts_tool_results() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::Assistant(
            rebon_render::transcript_row::AssistantMessage {
                uuid: "a1".into(),
                timestamp: "2026-04-14T00:00:00.001Z".into(),
                message: rebon_render::transcript_row::AssistantMessageInner {
                    role: rebon_render::transcript_row::AssistantRole::Assistant,
                    content: vec![
                        rebon_render::transcript_row::AssistantContentBlock::ToolUse(
                            rebon_render::transcript_row::AssistantToolUseBlock {
                                id: "tu-1".into(),
                                name: "Bash".into(),
                                input: serde_json::json!({"command": "ls"}),
                                tool_call_content: None,
                                raw_output: None,
                                title: None,
                                locations: None,
                                status: None,
                            },
                        ),
                    ],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            },
        )),
    );
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::User(
            rebon_render::transcript_row::UserMessage {
                uuid: "u1".into(),
                timestamp: "2026-04-14T00:00:00.002Z".into(),
                message: rebon_render::transcript_row::UserMessageInner {
                    role: rebon_render::transcript_row::UserRole::User,
                    content: vec![rebon_render::transcript_row::UserContentBlock::ToolResult(
                        rebon_render::transcript_row::UserToolResultBlock {
                            tool_use_id: "tu-1".into(),
                            content: rebon_render::transcript_row::ToolResultContent::Text(
                                "file1.txt\nfile2.txt\nfile3.txt".into(),
                            ),
                            is_error: None,
                        },
                    )],
                },
                is_compact_summary: None,
                is_meta: None,
                is_visible_in_transcript_only: None,
                image_paste_ids: None,
                plan_content: None,
            },
        )),
    );

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("tool_result: 1"));
    assert!(output.contains("tool_use: 1"));
    assert!(output.contains("tool results:"));
    assert!(output.contains("Bash: 1x"));
}

#[test]
fn context_command_counts_thinking_blocks() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::Assistant(
            rebon_render::transcript_row::AssistantMessage {
                uuid: "a1".into(),
                timestamp: "2026-04-14T00:00:00.001Z".into(),
                message: rebon_render::transcript_row::AssistantMessageInner {
                    role: rebon_render::transcript_row::AssistantRole::Assistant,
                    content: vec![
                        rebon_render::transcript_row::AssistantContentBlock::Thinking(
                            rebon_render::transcript_row::AssistantThinkingBlock {
                                thinking:
                                    "Let me think about this carefully and consider all angles..."
                                        .into(),
                                signature: None,
                            },
                        ),
                        rebon_render::transcript_row::AssistantContentBlock::Text(
                            rebon_render::transcript_row::AssistantTextBlock {
                                text: "Here's my answer.".into(),
                            },
                        ),
                    ],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            },
        )),
    );

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("thinking: 1"));
    assert!(output.contains("thinking:"));
}

#[test]
fn context_command_shows_warning_near_limit() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    session.model.prune_level.budget.report_usage(990_000);
    push_test_user_message(&mut app, "u1", "hello");

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("Near context window limit"));
    assert!(output.contains("99%"));
}

#[test]
fn context_command_shows_suggestion_when_high_usage() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    session.model.prune_level.budget.report_usage(850_000);
    push_test_user_message(&mut app, "u1", "hello");

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("Suggestions"));
    assert!(output.contains("Context is"));
    assert!(output.contains("/compact"));
}

#[test]
fn context_command_no_suggestions_when_low_usage() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    session.model.prune_level.budget.report_usage(100_000);
    push_test_user_message(&mut app, "u1", "hello");
    push_test_assistant_message(&mut app, "a1", "world");

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(!output.contains("Suggestions"));
}

#[test]
fn context_command_shows_prune_stats_when_active() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    session
        .model
        .prune_level
        .stats
        .requests_processed
        .store(5, std::sync::atomic::Ordering::Relaxed);
    session
        .model
        .prune_level
        .stats
        .tool_results_cleared
        .store(3, std::sync::atomic::Ordering::Relaxed);
    session
        .model
        .prune_level
        .stats
        .duplicates_removed
        .store(1, std::sync::atomic::Ordering::Relaxed);

    push_test_user_message(&mut app, "u1", "hello");

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("Prune stats:"));
    assert!(output.contains("3 tool results cleared"));
    assert!(output.contains("1 duplicates removed"));
}

#[test]
fn context_command_no_prune_stats_when_zero_requests() {
    let app = AppState::default();
    let session = make_test_tui_session();

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(!output.contains("Prune stats:"));
}

#[test]
fn context_command_shows_model_and_provider() {
    let app = AppState::default();
    let session = make_test_tui_session();

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("test-model"));
    assert!(output.contains("test"));
}

#[test]
fn context_command_shows_auto_compact_status() {
    let app = AppState::default();
    let session = make_test_tui_session();

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("Auto-compact: standby"));
    assert!(output.contains("Prune level: off"));
}

#[test]
fn context_command_shows_grid() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    session.model.prune_level.budget.report_usage(500_000);
    push_test_user_message(&mut app, "u1", "hello");

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("50%"));
    assert!(output.contains("50%"));
    assert!(output.contains("50%"));
}

#[test]
fn context_command_top_tools_sorted_by_tokens() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::Assistant(
            rebon_render::transcript_row::AssistantMessage {
                uuid: "a1".into(),
                timestamp: "2026-04-14T00:00:00.001Z".into(),
                message: rebon_render::transcript_row::AssistantMessageInner {
                    role: rebon_render::transcript_row::AssistantRole::Assistant,
                    content: vec![
                        rebon_render::transcript_row::AssistantContentBlock::ToolUse(
                            rebon_render::transcript_row::AssistantToolUseBlock {
                                id: "tu-1".into(),
                                name: "Read".into(),
                                input: serde_json::json!({"file_path": "/a"}),
                                tool_call_content: None,
                                raw_output: None,
                                title: None,
                                locations: None,
                                status: None,
                            },
                        ),
                    ],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            },
        )),
    );
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::Assistant(
            rebon_render::transcript_row::AssistantMessage {
                uuid: "a2".into(),
                timestamp: "2026-04-14T00:00:00.002Z".into(),
                message: rebon_render::transcript_row::AssistantMessageInner {
                    role: rebon_render::transcript_row::AssistantRole::Assistant,
                    content: vec![
                        rebon_render::transcript_row::AssistantContentBlock::ToolUse(
                            rebon_render::transcript_row::AssistantToolUseBlock {
                                id: "tu-2".into(),
                                name: "Bash".into(),
                                input: serde_json::json!({"command": "cargo build --release --all-features 2>&1 | head -100"}),
                                tool_call_content: None,
                                raw_output: None,
                                title: None,
                                locations: None,
                                status: None,
                            },
                        ),
                    ],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            },
        )),
    );

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("Top Tools"));
    assert!(output.contains("Read: 1x"));
    assert!(output.contains("Bash: 1x"));
    let bash_pos = output.find("Bash:").unwrap();
    let read_pos = output.find("Read: 1x").unwrap();
    assert!(
        bash_pos < read_pos,
        "Bash should appear before Read (more tokens)"
    );
}

#[test]
fn context_command_top_tools_truncated_to_five() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    for (i, name) in ["Alpha", "Beta", "Gamma", "Delta", "Epsilon", "Zeta", "Eta"]
        .iter()
        .enumerate()
    {
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::Commit(rebon_render::transcript_row::Message::Assistant(
                rebon_render::transcript_row::AssistantMessage {
                    uuid: format!("a{i}"),
                    timestamp: format!("2026-04-14T00:00:00.{i:03}Z"),
                    message: rebon_render::transcript_row::AssistantMessageInner {
                        role: rebon_render::transcript_row::AssistantRole::Assistant,
                        content: vec![
                            rebon_render::transcript_row::AssistantContentBlock::ToolUse(
                                rebon_render::transcript_row::AssistantToolUseBlock {
                                    id: format!("tu-{i}"),
                                    name: name.to_string(),
                                    input: serde_json::json!({"x": "y".repeat(i * 10 + 10)}),
                                    tool_call_content: None,
                                    raw_output: None,
                                    title: None,
                                    locations: None,
                                    status: None,
                                },
                            ),
                        ],
                    },
                    is_api_error_message: None,
                    advisor_model: None,
                    is_stream_continuation: None,
                },
            )),
        );
    }

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    let top_tools_section = output.split("Top Tools").nth(1).unwrap();
    let section_end = top_tools_section
        .find("\n\n")
        .unwrap_or(top_tools_section.len());
    let section = &top_tools_section[..section_end];
    let tool_lines: Vec<_> = section
        .lines()
        .filter(|l| l.contains(": ") && l.contains("x  ~"))
        .collect();
    assert_eq!(
        tool_lines.len(),
        5,
        "Should only show top 5 tools, got: {tool_lines:?}"
    );
}

#[test]
fn context_command_tool_result_matched_to_tool_name() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::Assistant(
            rebon_render::transcript_row::AssistantMessage {
                uuid: "a1".into(),
                timestamp: "2026-04-14T00:00:00.001Z".into(),
                message: rebon_render::transcript_row::AssistantMessageInner {
                    role: rebon_render::transcript_row::AssistantRole::Assistant,
                    content: vec![
                        rebon_render::transcript_row::AssistantContentBlock::ToolUse(
                            rebon_render::transcript_row::AssistantToolUseBlock {
                                id: "tu-grep".into(),
                                name: "Grep".into(),
                                input: serde_json::json!({"pattern": "foo"}),
                                tool_call_content: None,
                                raw_output: None,
                                title: None,
                                locations: None,
                                status: None,
                            },
                        ),
                    ],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            },
        )),
    );
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::User(
            rebon_render::transcript_row::UserMessage {
                uuid: "u1".into(),
                timestamp: "2026-04-14T00:00:00.002Z".into(),
                message: rebon_render::transcript_row::UserMessageInner {
                    role: rebon_render::transcript_row::UserRole::User,
                    content: vec![rebon_render::transcript_row::UserContentBlock::ToolResult(
                        rebon_render::transcript_row::UserToolResultBlock {
                            tool_use_id: "tu-grep".into(),
                            content: rebon_render::transcript_row::ToolResultContent::Text(
                                "match1\nmatch2\nmatch3".into(),
                            ),
                            is_error: None,
                        },
                    )],
                },
                is_compact_summary: None,
                is_meta: None,
                is_visible_in_transcript_only: None,
                image_paste_ids: None,
                plan_content: None,
            },
        )),
    );

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("Grep: 1x"));
}

#[test]
fn context_command_suggestion_tool_results_heavy() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    push_test_user_message(&mut app, "u0", "go");

    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::Assistant(
            rebon_render::transcript_row::AssistantMessage {
                uuid: "a1".into(),
                timestamp: "2026-04-14T00:00:00.001Z".into(),
                message: rebon_render::transcript_row::AssistantMessageInner {
                    role: rebon_render::transcript_row::AssistantRole::Assistant,
                    content: vec![
                        rebon_render::transcript_row::AssistantContentBlock::ToolUse(
                            rebon_render::transcript_row::AssistantToolUseBlock {
                                id: "tu-1".into(),
                                name: "Read".into(),
                                input: serde_json::json!({"file_path": "/x"}),
                                tool_call_content: None,
                                raw_output: None,
                                title: None,
                                locations: None,
                                status: None,
                            },
                        ),
                    ],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            },
        )),
    );
    let large_result = "x".repeat(4000);
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::User(
            rebon_render::transcript_row::UserMessage {
                uuid: "u1".into(),
                timestamp: "2026-04-14T00:00:00.002Z".into(),
                message: rebon_render::transcript_row::UserMessageInner {
                    role: rebon_render::transcript_row::UserRole::User,
                    content: vec![rebon_render::transcript_row::UserContentBlock::ToolResult(
                        rebon_render::transcript_row::UserToolResultBlock {
                            tool_use_id: "tu-1".into(),
                            content: rebon_render::transcript_row::ToolResultContent::Text(
                                large_result,
                            ),
                            is_error: None,
                        },
                    )],
                },
                is_compact_summary: None,
                is_meta: None,
                is_visible_in_transcript_only: None,
                image_paste_ids: None,
                plan_content: None,
            },
        )),
    );

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("Suggestions"));
    assert!(output.contains("/prune sweep"));
}

#[test]
fn context_command_shows_category_legend_from_plan() {
    let mut app = AppState::default();
    let session = make_test_tui_session();

    push_test_user_message(&mut app, "u1", "hello world hello world");
    push_test_assistant_message(&mut app, "a1", "hi there!");

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("Context Categories"), "output: {output}");
    assert!(output.contains("Messages:"), "output: {output}");
    assert!(output.contains("Free space:"), "output: {output}");
    assert!(output.contains("Autocompact buffer:"), "output: {output}");
}

#[test]
fn context_command_category_legend_omits_empty_buckets() {
    let app = AppState::default();
    let session = make_test_tui_session();

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("Context Categories"));
    assert!(output.contains("Free space:"));
    assert!(output.contains("Autocompact buffer:"));
    assert!(!output.contains("Messages:"));
    assert!(!output.contains("Tool calls:"));
    assert!(!output.contains("Tool results:"));
    assert!(!output.contains("Thinking:"));
}

#[test]
fn context_command_counts_only_toolsearch_loaded_deferred_tools() {
    let mut app = AppState::default();
    let mut session = make_test_tui_session();
    session.engine_half.engine = crate::tui::runner::test_support::builtin_test_engine();

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);
    assert!(
        !output.contains("  deferred tools:"),
        "unsearched deferred schemas should not count as loaded: {output}"
    );

    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::Assistant(
            rebon_render::transcript_row::AssistantMessage {
                uuid: "a-toolsearch".into(),
                timestamp: "2026-04-14T00:00:00.001Z".into(),
                message: rebon_render::transcript_row::AssistantMessageInner {
                    role: rebon_render::transcript_row::AssistantRole::Assistant,
                    content: vec![
                        rebon_render::transcript_row::AssistantContentBlock::ToolUse(
                            rebon_render::transcript_row::AssistantToolUseBlock {
                                id: "tu-toolsearch".into(),
                                name: rebon_tool::TOOL_SEARCH_TOOL_NAME.into(),
                                input: serde_json::json!({"query": "select:SaveMemory"}),
                                tool_call_content: None,
                                raw_output: None,
                                title: None,
                                locations: None,
                                status: None,
                            },
                        ),
                    ],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            },
        )),
    );
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::User(
            rebon_render::transcript_row::UserMessage {
                uuid: "u-toolsearch".into(),
                timestamp: "2026-04-14T00:00:00.002Z".into(),
                message: rebon_render::transcript_row::UserMessageInner {
                    role: rebon_render::transcript_row::UserRole::User,
                    content: vec![rebon_render::transcript_row::UserContentBlock::ToolResult(
                        rebon_render::transcript_row::UserToolResultBlock {
                            tool_use_id: "tu-toolsearch".into(),
                            content: rebon_render::transcript_row::ToolResultContent::Text(
                                serde_json::json!({
                                    "result": "<function>{}</function>",
                                    "matched_tools": ["SaveMemory"],
                                    "total_deferred_tools": 10
                                })
                                .to_string(),
                            ),
                            is_error: None,
                        },
                    )],
                },
                is_compact_summary: None,
                is_meta: None,
                is_visible_in_transcript_only: None,
                image_paste_ids: None,
                plan_content: None,
            },
        )),
    );

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);
    assert!(output.contains("  deferred tools:"), "output: {output}");
    assert!(output.contains("SaveMemory"), "output: {output}");
}

#[test]
fn context_command_omits_registered_but_uninvoked_skills() {
    let app = AppState::default();
    let mut session = make_test_tui_session();
    let registry = std::sync::Arc::new(rebon_plugin_skill::SkillRegistry::new());
    registry.register(rebon_plugin_skill::Skill {
        id: "project-context-skill".into(),
        title: "Project Context Skill".into(),
        description: "Registered but not invoked.".into(),
        prompt_template: "Use project context for this test.".into(),
        suggested_tools: Vec::new(),
        source: rebon_plugin_skill::SkillSource::Project,
        argument_hint: None,
        argument_names: Vec::new(),
        skill_root: None,
        user_invocable: true,
        disable_model_invocation: false,
        required_tools: Vec::new(),
    });
    session.engine_half.skill_registry = registry;

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("  skills:          ~0"), "output: {output}");
    assert!(!output.contains("\nSkills\n"), "output: {output}");
    assert!(
        !output.contains("project-context-skill"),
        "registered but uninvoked skill should not appear as loaded: {output}"
    );
}

#[test]
fn context_command_renders_invoked_skills_from_transcript() {
    let mut app = AppState::default();
    let mut session = make_test_tui_session();
    let registry = std::sync::Arc::new(rebon_plugin_skill::SkillRegistry::new());
    registry.register(rebon_plugin_skill::Skill {
        id: "project-context-skill".into(),
        title: "Project Context Skill".into(),
        description: "Registered and invoked.".into(),
        prompt_template: "Use project context for this test.".into(),
        suggested_tools: Vec::new(),
        source: rebon_plugin_skill::SkillSource::Project,
        argument_hint: None,
        argument_names: Vec::new(),
        skill_root: None,
        user_invocable: true,
        disable_model_invocation: false,
        required_tools: Vec::new(),
    });
    session.engine_half.skill_registry = registry;
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::Assistant(
            rebon_render::transcript_row::AssistantMessage {
                uuid: "a-skill".into(),
                timestamp: "2026-04-14T00:00:00.001Z".into(),
                message: rebon_render::transcript_row::AssistantMessageInner {
                    role: rebon_render::transcript_row::AssistantRole::Assistant,
                    content: vec![
                        rebon_render::transcript_row::AssistantContentBlock::ToolUse(
                            rebon_render::transcript_row::AssistantToolUseBlock {
                                id: "tu-skill".into(),
                                name: rebon_tools_core::SKILL_TOOL_NAME.into(),
                                input: serde_json::json!({"skill": "project-context-skill"}),
                                tool_call_content: None,
                                raw_output: None,
                                title: None,
                                locations: None,
                                status: None,
                            },
                        ),
                    ],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            },
        )),
    );
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::User(
            rebon_render::transcript_row::UserMessage {
                uuid: "u-skill".into(),
                timestamp: "2026-04-14T00:00:00.002Z".into(),
                message: rebon_render::transcript_row::UserMessageInner {
                    role: rebon_render::transcript_row::UserRole::User,
                    content: vec![rebon_render::transcript_row::UserContentBlock::ToolResult(
                        rebon_render::transcript_row::UserToolResultBlock {
                            tool_use_id: "tu-skill".into(),
                            content: rebon_render::transcript_row::ToolResultContent::Text(
                                serde_json::json!({
                                    "skill": "project-context-skill",
                                    "title": "Project Context Skill",
                                    "description": "Registered and invoked.",
                                    "prompt": "Use project context for this test.",
                                    "suggested_tools": []
                                })
                                .to_string(),
                            ),
                            is_error: None,
                        },
                    )],
                },
                is_compact_summary: None,
                is_meta: None,
                is_visible_in_transcript_only: None,
                image_paste_ids: None,
                plan_content: None,
            },
        )),
    );

    let inputs = crate::session_shell::session_command_inputs_from_app(&app, session.ui_mode);
    let output = execute_context_command(&inputs, &session);

    assert!(output.contains("Skills"), "output: {output}");
    assert!(
        output.contains("[Project]"),
        "expected Project source header from invoked Skill result: {output}"
    );
    assert!(
        output.contains("project-context-skill"),
        "expected invoked skill id in /context output: {output}"
    );
    assert!(
        !output.contains("[Built-in]"),
        "no built-in skills should appear unless invoked: {output}"
    );
}

#[test]
fn find_tool_name_for_result_returns_unknown_when_not_found() {
    let rows: Vec<rebon_render::transcript_row::Message> = vec![];
    assert_eq!(find_tool_name_for_result(&rows, "nonexistent"), "<unknown>");
}

#[test]
fn find_tool_name_for_result_matches_id() {
    let rows = vec![rebon_render::transcript_row::Message::Assistant(
        rebon_render::transcript_row::AssistantMessage {
            uuid: "a1".into(),
            timestamp: "t".into(),
            message: rebon_render::transcript_row::AssistantMessageInner {
                role: rebon_render::transcript_row::AssistantRole::Assistant,
                content: vec![
                    rebon_render::transcript_row::AssistantContentBlock::ToolUse(
                        rebon_render::transcript_row::AssistantToolUseBlock {
                            id: "tu-42".into(),
                            name: "Edit".into(),
                            input: serde_json::json!({}),
                            tool_call_content: None,
                            raw_output: None,
                            title: None,
                            locations: None,
                            status: None,
                        },
                    ),
                ],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        },
    )];
    assert_eq!(find_tool_name_for_result(&rows, "tu-42"), "Edit");
    assert_eq!(find_tool_name_for_result(&rows, "tu-99"), "<unknown>");
}

#[test]
fn parse_compact_allows_optional_instructions() {
    assert_eq!(
        parse_compact_command("/compact"),
        Some(CompactCommand { instructions: None })
    );
    assert_eq!(
        parse_compact_command("/compact keep exact test output"),
        Some(CompactCommand {
            instructions: Some("keep exact test output".into())
        })
    );
    assert_eq!(
        parse_compact_command("/compact: preserve file reads"),
        Some(CompactCommand {
            instructions: Some("preserve file reads".into())
        })
    );
}

#[test]
fn parse_compact_rejects_adjacent_command_names() {
    assert!(parse_compact_command("/compactnow").is_none());
}

fn push_test_user_message(app: &mut AppState, uuid: &str, text: &str) {
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::User(
            rebon_render::transcript_row::UserMessage {
                uuid: uuid.to_string(),
                timestamp: format!("2026-04-14T00:00:00.{}Z", uuid.trim_start_matches('u')),
                message: rebon_render::transcript_row::UserMessageInner {
                    role: rebon_render::transcript_row::UserRole::User,
                    content: vec![rebon_render::transcript_row::UserContentBlock::Text(
                        rebon_render::transcript_row::UserTextBlock {
                            text: text.to_string(),
                        },
                    )],
                },
                is_compact_summary: None,
                is_meta: None,
                is_visible_in_transcript_only: None,
                image_paste_ids: None,
                plan_content: None,
            },
        )),
    );
}

fn push_test_assistant_message(app: &mut AppState, uuid: &str, text: &str) {
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_render::transcript_row::Message::Assistant(
            rebon_render::transcript_row::AssistantMessage {
                uuid: uuid.to_string(),
                timestamp: format!("2026-04-14T00:00:00.{}Z", uuid.trim_start_matches('a')),
                message: rebon_render::transcript_row::AssistantMessageInner {
                    role: rebon_render::transcript_row::AssistantRole::Assistant,
                    content: vec![rebon_render::transcript_row::AssistantContentBlock::Text(
                        rebon_render::transcript_row::AssistantTextBlock {
                            text: text.to_string(),
                        },
                    )],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            },
        )),
    );
}

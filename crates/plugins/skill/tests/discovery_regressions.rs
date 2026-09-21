//! Real-execution regressions for progressive skill discovery.
//!
//! These ran in `rebon-core`'s query tests while the subscriber was one of
//! the engine's builtin turn hooks. They moved here with it: the subscriber is
//! this plugin's now, so the seam under test is the plugin registering on the
//! `turn-hooks` seat and reading its state out of the turn's extension bag.
//!
//! Every one of them drives a real `run_query` against a mock model stream, so
//! what they pin is the behaviour the model and the registry actually see, not
//! a call into the hook.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rebon_api::{
    Message as ApiMessage, MockModelClient, ModelClient, ModelError, PruneLevel, PruneLevelHandle,
    SessionHandle, StopReason, StreamEvent,
};
use rebon_core::query::{
    run_query, tools_from_engine, AttachmentPollRequest, AttachmentPoller, CancelToken, QueryEvent,
    QueryParams,
};
use rebon_plugin_skill::{SkillContext, SkillRegistry, SkillState};
use rebon_tool::{Tool, ToolContext};
use serde_json::{json, Value};

use support::{
    build_engine_with, build_engine_with_tools, drain, load_skill_plugin, text_turn,
    text_turn_with_stop, tool_turn, two_tool_turn, write_progressive_test_skill, RecordingTool,
};

/// The turn's state, in the one bag both readers see.
fn skill_params(
    params: QueryParams,
    registry: &Arc<SkillRegistry>,
    state: Arc<std::sync::Mutex<SkillState>>,
) -> QueryParams {
    let mut params = params;
    params
        .extensions
        .insert(SkillContext::new(registry.clone(), state));
    params
}

fn state_for(session: &str, cwd: &std::path::Path) -> Arc<std::sync::Mutex<SkillState>> {
    Arc::new(std::sync::Mutex::new(SkillState::empty(
        session,
        cwd.to_string_lossy(),
    )))
}

/// Discovery only happens where this plugin put a subscriber. A turn whose
/// hook seat is not the kernel's — a bare `QueryParams`, which builds its own
/// — runs the same tools and registers nothing, which is what makes the
/// plugin switch mean something.
#[tokio::test]
async fn discovery_is_owned_by_the_turn_hook_seat() {
    let temp = tempfile::tempdir().unwrap();
    let source = write_progressive_test_skill(
        &temp.path().join("project"),
        "late-review",
        "Review files discovered late",
    );

    let engine = build_engine_with(Arc::new(RecordingTool::new("Read", json!({"ok": true}))));
    let client = MockModelClient::new();
    let input = json!({"file_path": source.to_string_lossy()}).to_string();
    client.push_turn(tool_turn("msg_discover", "Read", "toolu_discover", &input));
    client.push_turn(text_turn("msg_done", "done"));

    let registry = Arc::new(SkillRegistry::new());
    let params = skill_params(
        QueryParams::new("mock", vec![ApiMessage::user_text("read the file")])
            .with_tools(tools_from_engine(&engine))
            .with_max_iterations(2),
        &registry,
        state_for("sess-progressive-owner", temp.path()),
    );

    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(client)),
        params,
        ToolContext::new().with_cwd(temp.path().to_string_lossy().into_owned()),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    assert!(
        registry.get("late-review").is_none(),
        "discovery must disappear with its turn-hook subscriber"
    );
}

/// Every spelling of every file tool triggers discovery, and a tool that
/// failed still counts: the round completed, and the path was still touched.
#[tokio::test]
async fn discovery_preserves_aliases_and_failure_semantics() {
    let plugin = load_skill_plugin(std::env::temp_dir());
    let file_tool_names: Vec<_> = rebon_tools_core::BUILTIN_TOOL_FACTS
        .iter()
        .filter(|facts| {
            facts.file_target_field.is_some() || facts.kind == rebon_tools_core::ToolKind::Search
        })
        .flat_map(|facts| std::iter::once(facts.name).chain(facts.aliases.iter().copied()))
        .collect();

    for (index, tool_name) in file_tool_names.into_iter().enumerate() {
        let temp = tempfile::tempdir().unwrap();
        let source =
            write_progressive_test_skill(&temp.path().join("project"), "late-review", tool_name);
        let tool = if index % 2 == 0 {
            RecordingTool::new(tool_name, json!({"ok": true}))
        } else {
            RecordingTool::failing(tool_name)
        };
        let engine = build_engine_with(Arc::new(tool));
        let client = MockModelClient::new();
        let path_field = rebon_tools_core::file_target_field_for_name(tool_name).unwrap_or("path");
        let mut input = serde_json::Map::new();
        input.insert(
            path_field.to_string(),
            Value::String(source.to_string_lossy().into_owned()),
        );
        client.push_turn(tool_turn(
            "msg_discover_alias",
            tool_name,
            "toolu_discover_alias",
            &Value::Object(input).to_string(),
        ));
        client.push_turn(text_turn("msg_done_alias", "done"));

        let registry = Arc::new(SkillRegistry::new());
        let params = skill_params(
            QueryParams::new("mock", vec![ApiMessage::user_text("touch the file")])
                .with_tools(tools_from_engine(&engine))
                .with_turn_hook_seat(plugin.hook_seat.clone())
                .with_max_iterations(2),
            &registry,
            state_for(&format!("sess-progressive-{index}"), temp.path()),
        );

        let mut rx = run_query(
            engine,
            SessionHandle::new(Arc::new(client)),
            params,
            ToolContext::new().with_cwd(temp.path().to_string_lossy().into_owned()),
            CancelToken::new(),
        );
        let events = drain(&mut rx).await;

        assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
        assert_eq!(
            registry
                .get("late-review")
                .unwrap_or_else(|| panic!("{tool_name} did not trigger discovery"))
                .description,
            tool_name,
        );
    }
}

/// A tool nobody classifies as a file tool, and a file tool with no path,
/// both leave the index alone.
#[tokio::test]
async fn discovery_ignores_unknown_and_empty_tool_inputs() {
    let plugin = load_skill_plugin(std::env::temp_dir());
    let temp = tempfile::tempdir().unwrap();
    let _source = write_progressive_test_skill(
        &temp.path().join("project"),
        "must-stay-hidden",
        "not touched",
    );
    let other = Arc::new(RecordingTool::new("Other", json!({"ok": true}))) as Arc<dyn Tool>;
    let read = Arc::new(RecordingTool::new("Read", json!({"ok": true}))) as Arc<dyn Tool>;
    let engine = build_engine_with_tools(vec![other, read]);
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_unknown",
        "Other",
        "toolu_unknown",
        "{\"file_path\":\"ignored.rs\"}",
    ));
    client.push_turn(tool_turn("msg_empty", "Read", "toolu_empty", "{}"));
    client.push_turn(text_turn("msg_done_unknown", "done"));

    let registry = Arc::new(SkillRegistry::new());
    let params = skill_params(
        QueryParams::new("mock", vec![ApiMessage::user_text("do unrelated work")])
            .with_tools(tools_from_engine(&engine))
            .with_turn_hook_seat(plugin.hook_seat.clone())
            .with_max_iterations(3),
        &registry,
        state_for("sess-progressive-unknown", temp.path()),
    );

    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(client)),
        params,
        ToolContext::new().with_cwd(temp.path().to_string_lossy().into_owned()),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    assert!(registry.is_empty());
}

/// Two tools in one round name the same skill id under different roots. The
/// later one in provider order wins, and a second identical round adds
/// nothing.
#[tokio::test]
async fn discovery_uses_parallel_tool_order_and_dedupes_later_rounds() {
    let plugin = load_skill_plugin(std::env::temp_dir());
    let temp = tempfile::tempdir().unwrap();
    let first_source = write_progressive_test_skill(
        &temp.path().join("project-a"),
        "collision",
        "first provider-order path",
    );
    let second_source = write_progressive_test_skill(
        &temp.path().join("project-b"),
        "collision",
        "second provider-order path",
    );
    let first_input = json!({"file_path": first_source.to_string_lossy()}).to_string();
    let second_input = json!({"file_path": second_source.to_string_lossy()}).to_string();
    let engine = build_engine_with(Arc::new(RecordingTool::new("Read", json!({"ok": true}))));
    let client = MockModelClient::new();
    client.push_turn(two_tool_turn(
        "msg_parallel_first",
        ("Read", "toolu_first_a", &first_input),
        ("Read", "toolu_first_b", &second_input),
    ));
    client.push_turn(two_tool_turn(
        "msg_parallel_repeat",
        ("Read", "toolu_repeat_a", &first_input),
        ("Read", "toolu_repeat_b", &second_input),
    ));
    client.push_turn(text_turn("msg_done_parallel", "done"));

    let registry = Arc::new(SkillRegistry::new());
    let params = skill_params(
        QueryParams::new("mock", vec![ApiMessage::user_text("read both")])
            .with_tools(tools_from_engine(&engine))
            .with_turn_hook_seat(plugin.hook_seat.clone())
            .with_max_iterations(3),
        &registry,
        state_for("sess-progressive-parallel", temp.path()),
    );

    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(client)),
        params,
        ToolContext::new().with_cwd(temp.path().to_string_lossy().into_owned()),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    assert_eq!(registry.ids(), ["collision"]);
    assert_eq!(
        registry.get("collision").unwrap().description,
        "second provider-order path"
    );
}

/// A truncated response and a transient error both replay before any tool
/// runs; the discovery that follows the eventual tool round still lands.
#[tokio::test(start_paused = true)]
async fn discovery_waits_through_continuation_and_transient_retry() {
    let plugin = load_skill_plugin(std::env::temp_dir());
    let temp = tempfile::tempdir().unwrap();
    let source = write_progressive_test_skill(
        &temp.path().join("project"),
        "after-replay",
        "loaded after replay",
    );
    let engine = build_engine_with(Arc::new(RecordingTool::new("Read", json!({"ok": true}))));
    let client = MockModelClient::new();
    client.push_turn(text_turn_with_stop(
        "msg_partial_discovery",
        "partial",
        StopReason::MaxTokens,
    ));
    client.push_error(ModelError::transient("retry before tools"));
    let input = json!({"file_path": source.to_string_lossy()}).to_string();
    client.push_turn(tool_turn(
        "msg_after_replay",
        "Read",
        "toolu_after_replay",
        &input,
    ));
    client.push_turn(text_turn("msg_done_replay", "done"));
    let captured_client = client.clone();

    let registry = Arc::new(SkillRegistry::new());
    let params = skill_params(
        QueryParams::new("mock", vec![ApiMessage::user_text("continue then read")])
            .with_tools(tools_from_engine(&engine))
            .with_turn_hook_seat(plugin.hook_seat.clone())
            .with_max_iterations(4),
        &registry,
        state_for("sess-progressive-replay", temp.path()),
    );

    let mut rx = run_query(
        engine,
        SessionHandle::new(Arc::new(client)),
        params,
        ToolContext::new().with_cwd(temp.path().to_string_lossy().into_owned()),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    assert_eq!(registry.ids(), ["after-replay"]);
    assert_eq!(captured_client.captured_requests().len(), 4);
}

/// A round that completed before a hard error still counts; a cancelled one
/// never does, because cancellation means the round did not complete.
#[tokio::test]
async fn discovery_keeps_completed_round_before_hard_error_but_skips_cancel() {
    let plugin = load_skill_plugin(std::env::temp_dir());
    let completed_temp = tempfile::tempdir().unwrap();
    let completed_source = write_progressive_test_skill(
        &completed_temp.path().join("project"),
        "before-error",
        "registered before hard error",
    );
    let completed_engine =
        build_engine_with(Arc::new(RecordingTool::new("Read", json!({"ok": true}))));
    let completed_client = MockModelClient::new();
    let completed_input = json!({"file_path": completed_source.to_string_lossy()}).to_string();
    completed_client.push_turn(tool_turn(
        "msg_before_error",
        "Read",
        "toolu_before_error",
        &completed_input,
    ));
    completed_client.push_error(ModelError::other("hard failure after tool round"));
    let completed_registry = Arc::new(SkillRegistry::new());
    let completed_params = skill_params(
        QueryParams::new("mock", vec![ApiMessage::user_text("read then fail")])
            .with_tools(tools_from_engine(&completed_engine))
            .with_turn_hook_seat(plugin.hook_seat.clone())
            .with_max_iterations(2),
        &completed_registry,
        state_for("sess-progressive-hard-error", completed_temp.path()),
    );
    let mut completed_rx = run_query(
        completed_engine,
        SessionHandle::new(Arc::new(completed_client)),
        completed_params,
        ToolContext::new().with_cwd(completed_temp.path().to_string_lossy().into_owned()),
        CancelToken::new(),
    );
    let completed_events = drain(&mut completed_rx).await;
    assert!(matches!(
        completed_events.last(),
        Some(QueryEvent::Error(_))
    ));
    assert!(completed_registry.get("before-error").is_some());

    let cancelled_temp = tempfile::tempdir().unwrap();
    let cancelled_source = write_progressive_test_skill(
        &cancelled_temp.path().join("project"),
        "must-not-load",
        "cancelled",
    );
    let cancelled_engine =
        build_engine_with(Arc::new(RecordingTool::new("Read", json!({"ok": true}))));
    let cancelled_client = MockModelClient::new();
    let cancelled_input = json!({"file_path": cancelled_source.to_string_lossy()}).to_string();
    cancelled_client.push_turn(tool_turn(
        "msg_cancelled",
        "Read",
        "toolu_cancelled",
        &cancelled_input,
    ));
    let cancelled_registry = Arc::new(SkillRegistry::new());
    let cancelled_params = skill_params(
        QueryParams::new("mock", vec![ApiMessage::user_text("cancel")])
            .with_tools(tools_from_engine(&cancelled_engine))
            .with_turn_hook_seat(plugin.hook_seat.clone()),
        &cancelled_registry,
        state_for("sess-progressive-cancel", cancelled_temp.path()),
    );
    let cancel = CancelToken::new();
    cancel.cancel();
    let mut cancelled_rx = run_query(
        cancelled_engine,
        SessionHandle::new(Arc::new(cancelled_client)),
        cancelled_params,
        ToolContext::new().with_cwd(cancelled_temp.path().to_string_lossy().into_owned()),
        cancel,
    );
    let cancelled_events = drain(&mut cancelled_rx).await;
    assert!(matches!(
        cancelled_events.last(),
        Some(QueryEvent::Cancelled)
    ));
    assert!(cancelled_registry.is_empty());
}

struct OneShotProgressiveContextReset(AtomicBool);

impl AttachmentPoller for OneShotProgressiveContextReset {
    fn poll(&self, _request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        Vec::new()
    }

    fn take_context_reset(&self) -> Option<Vec<ApiMessage>> {
        self.0.swap(false, Ordering::SeqCst).then(|| {
            vec![ApiMessage::user_text(
                "Implement the following plan:\n\nkeep the discovered skill",
            )]
        })
    }
}

/// State with no registry is a no-op rather than a failure, and a context
/// reset mid-turn does not lose what discovery already registered.
#[tokio::test]
async fn discovery_without_a_registry_is_a_noop_and_a_context_reset_keeps_it() {
    let plugin = load_skill_plugin(std::env::temp_dir());
    let temp = tempfile::tempdir().unwrap();
    let source = write_progressive_test_skill(
        &temp.path().join("project"),
        "across-reset",
        "registered before reset",
    );
    let input = json!({"file_path": source.to_string_lossy()}).to_string();
    let state = state_for("sess-progressive-reset", temp.path());

    let no_registry_engine =
        build_engine_with(Arc::new(RecordingTool::new("Read", json!({"ok": true}))));
    let no_registry_client = MockModelClient::new();
    no_registry_client.push_turn(tool_turn(
        "msg_no_registry",
        "Read",
        "toolu_no_registry",
        &input,
    ));
    no_registry_client.push_turn(text_turn("msg_no_registry_done", "done"));
    let mut no_registry_params =
        QueryParams::new("mock", vec![ApiMessage::user_text("first attempt")])
            .with_tools(tools_from_engine(&no_registry_engine))
            .with_turn_hook_seat(plugin.hook_seat.clone())
            .with_max_iterations(2);
    no_registry_params.extensions.insert(SkillContext {
        registry: None,
        discovery: Some(state.clone()),
    });
    let mut no_registry_rx = run_query(
        no_registry_engine,
        SessionHandle::new(Arc::new(no_registry_client)),
        no_registry_params,
        ToolContext::new().with_cwd(temp.path().to_string_lossy().into_owned()),
        CancelToken::new(),
    );
    assert!(matches!(
        drain(&mut no_registry_rx).await.last(),
        Some(QueryEvent::Done { .. })
    ));

    let registry = Arc::new(SkillRegistry::new());
    let reset_engine = build_engine_with(Arc::new(RecordingTool::new("Read", json!({"ok": true}))));
    let reset_client = MockModelClient::new();
    reset_client.push_turn(tool_turn(
        "msg_before_reset",
        "Read",
        "toolu_before_reset",
        &input,
    ));
    let reset_params = skill_params(
        QueryParams::new("mock", vec![ApiMessage::user_text("retry")])
            .with_tools(tools_from_engine(&reset_engine))
            .with_turn_hook_seat(plugin.hook_seat.clone())
            .with_attachment_poller(
                Arc::new(OneShotProgressiveContextReset(AtomicBool::new(true))),
                "session-reset",
                "turn-reset",
            )
            .with_max_iterations(2),
        &registry,
        state,
    );
    let mut reset_rx = run_query(
        reset_engine,
        SessionHandle::new(Arc::new(reset_client)),
        reset_params,
        ToolContext::new().with_cwd(temp.path().to_string_lossy().into_owned()),
        CancelToken::new(),
    );
    let reset_events = drain(&mut reset_rx).await;

    assert!(reset_events
        .iter()
        .any(|event| matches!(event, QueryEvent::ContextReset { .. })));
    assert!(matches!(
        reset_events.last(),
        Some(QueryEvent::ContextReset { .. })
    ));
    assert!(registry.get("across-reset").is_some());
}

/// Mid-turn auto-compaction rewrites the turn's history; the skill discovered
/// before it must survive, because the registry is not history.
#[tokio::test]
async fn discovery_survives_mid_turn_auto_compaction() {
    let plugin = load_skill_plugin(std::env::temp_dir());
    let temp = tempfile::tempdir().unwrap();
    let source = write_progressive_test_skill(
        &temp.path().join("project"),
        "before-compaction",
        "registered before compaction",
    );
    let engine = build_engine_with(Arc::new(RecordingTool::new("Read", json!({}))));
    let mock = MockModelClient::new();
    let input = json!({"file_path": source.to_string_lossy()}).to_string();
    let mut first_turn = tool_turn("msg_tool", "Read", "toolu_1", &input);
    let StreamEvent::MessageStart { usage, .. } = &mut first_turn[0] else {
        panic!("tool turn must start with MessageStart");
    };
    usage.input_tokens = 96_000;
    mock.push_turn(first_turn);
    mock.push_turn(text_turn("msg_done", "done"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());
    let handle = PruneLevelHandle::with_context_window(PruneLevel::Conservative, 100_000);
    let registry = Arc::new(SkillRegistry::new());
    let params = skill_params(
        QueryParams {
            max_tokens: 4_000,
            max_iterations: 2,
            prune_level: Some(handle),
            ..QueryParams::new("mock", vec![ApiMessage::user_text("inspect")])
        }
        .with_turn_hook_seat(plugin.hook_seat.clone()),
        &registry,
        state_for("sess-progressive-compaction", temp.path()),
    );

    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd(temp.path().to_string_lossy().into_owned()),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert!(events
        .iter()
        .any(|event| matches!(event, QueryEvent::CompactingStarted { .. })));
    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
    assert_eq!(mock.captured_requests().len(), 2);
    assert!(registry.get("before-compaction").is_some());
}

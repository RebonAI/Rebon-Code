//! A replay over the auto-compact threshold is summarised before its turn
//! is sent and the summary kept as the session's baseline, so a compaction
//! outlasts the turn it ran in — in this process and in the next one a
//! background worker starts for the following turn.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Summarises to one marked message plus the last one, and counts calls.
struct CountingSummariser {
    calls: Arc<AtomicUsize>,
    fails: bool,
}

#[async_trait::async_trait]
impl CompactProvider for CountingSummariser {
    async fn compact(
        &self,
        messages: &[ApiMessage],
        _system: Option<&str>,
        _protected_turns: usize,
        _custom_instructions: Option<&str>,
        _summary_options: &rebon_api::CompactSummaryOptions,
    ) -> rebon_api::ModelResult<rebon_api::CompactResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fails {
            return Err(rebon_api::ModelError::Permanent("summariser down".into()));
        }
        let mut kept = vec![ApiMessage::user_text(format!(
            "{COMPACT_SUMMARY_MARKER}\nthe earlier work, summarised\n"
        ))];
        kept.extend(messages.last().cloned());
        Ok(rebon_api::CompactResult { messages: kept })
    }
}

fn prompt(session_id: &str, cwd: &str, text: &str) -> PromptRequest {
    PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session_id.into(),
        cwd: cwd.into(),
        prompt: vec![AcpContentBlock::Text(TextContent {
            text: text.into(),
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        update_publisher: None,
        permission_publisher: None,
        cancel: rebon_agent_core::PromptCancel::new(),
        thinking_budget: None,
        max_tokens: None,
        reasoning_effort_ordinal: None,
        additional_working_directories: Vec::new(),
        coordinator_mode: None,
        coordinator_report_paths: Vec::new(),
        user_message_uuid: None,
        background_agent_system: None,
        background_agent_tool_filter: None,
        execution_policy: None,
        replay_requests: Vec::new(),
        skill_invocations: Vec::new(),
    }
}

/// A first turn of two tool rounds and an answer: six messages to replay,
/// more than a compaction keeps verbatim. `answer` is the closing reply.
fn push_working_turn(mock: &MockModelClient, answer: Vec<StreamEvent>) {
    mock.push_turn(tool_turn(
        "msg_t1",
        "Read",
        "toolu_1",
        "{\"path\":\"a.rs\"}",
    ));
    mock.push_turn(tool_turn(
        "msg_t2",
        "Read",
        "toolu_2",
        "{\"path\":\"b.rs\"}",
    ));
    mock.push_turn(answer);
}

fn request_text(request: &rebon_api::CreateMessageRequest) -> String {
    serde_json::to_string(&request.messages).unwrap()
}

struct World {
    _dir: tempfile::TempDir,
    projects_root: std::path::PathBuf,
    cwd: String,
    session_id: String,
    mock: MockModelClient,
    calls: Arc<AtomicUsize>,
    fails: bool,
    window: u32,
}

impl World {
    fn new(tag: &str, window: u32, fails: bool) -> (Self, Arc<rebon_session_state::ServerState>) {
        let dir = temp_projects_root(tag);
        let projects_root = dir.path().to_path_buf();
        let cwd = projects_root.to_string_lossy().to_string();
        let state = Arc::new(rebon_session_state::ServerState::new());
        let session = state.create_session(cwd.clone(), Vec::new());
        let world = Self {
            _dir: dir,
            projects_root,
            cwd,
            session_id: session.id,
            mock: MockModelClient::new(),
            calls: Arc::new(AtomicUsize::new(0)),
            fails,
            window,
        };
        (world, state)
    }

    /// An executor as a worker builds one: its own prune budget, its own
    /// replay store, over `state`.
    fn executor(&self, state: Arc<rebon_session_state::ServerState>) -> EngineQueryExecutor {
        let tool = Arc::new(RecordingTool::new("Read", json!({"contents": "body"})));
        let client: Arc<dyn ModelClient> = Arc::new(self.mock.clone());
        EngineQueryExecutor::new(
            build_engine_with(tool as Arc<dyn Tool>),
            client,
            &self.projects_root,
            "mock-model",
        )
        .with_server_state(state)
        .with_prune_level(PruneLevelHandle::with_context_window(
            PruneLevel::Conservative,
            self.window,
        ))
        .with_compact_provider(Arc::new(CountingSummariser {
            calls: self.calls.clone(),
            fails: self.fails,
        }))
    }

    /// A fresh process: a new state that loads the session from disk.
    fn reloaded(&self) -> Arc<rebon_session_state::ServerState> {
        let state = Arc::new(rebon_session_state::ServerState::new());
        state.mark_initialized().unwrap();
        state
            .load_session(
                &self.projects_root,
                &self.session_id,
                &self.cwd,
                None,
                Vec::new(),
            )
            .expect("load_session failed");
        state
    }

    fn baseline_file(&self) -> std::path::PathBuf {
        rebon_session::project_dir_path(&self.projects_root, &self.cwd)
            .join(format!("{}.compact.json", self.session_id))
    }

    fn threshold(&self) -> u32 {
        PruneLevelHandle::with_context_window(PruneLevel::Conservative, self.window)
            .budget
            .auto_compact_threshold()
    }
}

#[tokio::test]
async fn an_oversized_replay_is_summarised_once_and_the_summary_outlasts_the_process() {
    let (world, state) = World::new("replay_compaction_kept", 80_000, false);
    // Long enough by the local estimate alone.
    let big = "y".repeat(world.threshold() as usize * 5);
    push_working_turn(&world.mock, text_turn("msg_1", "done"));
    world.mock.push_turn(text_turn("msg_2", "second answer"));
    world.mock.push_turn(text_turn("msg_3", "third answer"));

    let executor = world.executor(state.clone());
    executor
        .execute(prompt(&world.session_id, &world.cwd, &big))
        .await
        .unwrap();
    let after_first = world.calls.load(Ordering::SeqCst);

    executor
        .execute(prompt(&world.session_id, &world.cwd, "second question"))
        .await
        .unwrap();
    assert_eq!(world.calls.load(Ordering::SeqCst), after_first + 1);
    assert!(
        world.baseline_file().exists(),
        "the summary is kept on disk"
    );
    let requests = world.mock.captured_requests();
    let second = request_text(requests.last().unwrap());
    assert!(second.contains(COMPACT_SUMMARY_MARKER), "{second}");
    assert!(
        !second.contains(&big[..64]),
        "the summarised text is not replayed"
    );
    assert!(second.contains("second question"));

    // The next turn, in a worker that starts from disk, replays the summary
    // and does not summarise again.
    let executor = world.executor(world.reloaded());
    executor
        .execute(prompt(&world.session_id, &world.cwd, "third question"))
        .await
        .unwrap();
    assert_eq!(world.calls.load(Ordering::SeqCst), after_first + 1);
    let third = request_text(world.mock.captured_requests().last().unwrap());
    assert!(third.contains(COMPACT_SUMMARY_MARKER), "{third}");
    assert!(third.contains("second answer") && third.contains("third question"));
    assert!(!third.contains(&big[..64]));
}

/// The estimate reads this replay short; the provider measured it — an
/// Anthropic cache read included — over the threshold.
#[tokio::test]
async fn a_replay_the_provider_measured_over_the_threshold_is_summarised() {
    let (world, state) = World::new("replay_compaction_measured", 200_000, false);
    let measured = world.threshold() + 10_000;
    let mut answer = text_turn("msg_1", "done");
    answer[0] = StreamEvent::MessageStart {
        message_id: "msg_1".into(),
        model: "mock".into(),
        usage: Usage {
            input_tokens: 40,
            cache_read_input_tokens: measured,
            ..Default::default()
        },
    };
    push_working_turn(&world.mock, answer);
    world.mock.push_turn(text_turn("msg_2", "second answer"));
    world.mock.push_turn(text_turn("msg_3", "third answer"));

    let executor = world.executor(state.clone());
    executor
        .execute(prompt(
            &world.session_id,
            &world.cwd,
            "short first question",
        ))
        .await
        .unwrap();
    assert_eq!(world.calls.load(Ordering::SeqCst), 0);

    executor
        .execute(prompt(&world.session_id, &world.cwd, "second question"))
        .await
        .unwrap();
    assert_eq!(world.calls.load(Ordering::SeqCst), 1);
    assert!(world.baseline_file().exists());
    let second = request_text(world.mock.captured_requests().last().unwrap());
    assert!(second.contains(COMPACT_SUMMARY_MARKER), "{second}");

    // What was measured before the summary does not count against it.
    let executor = world.executor(world.reloaded());
    executor
        .execute(prompt(&world.session_id, &world.cwd, "third question"))
        .await
        .unwrap();
    assert_eq!(world.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_short_replay_is_left_alone() {
    let (world, state) = World::new("replay_compaction_short", 200_000, false);
    push_working_turn(&world.mock, text_turn("msg_1", "done"));
    world.mock.push_turn(text_turn("msg_2", "second answer"));
    let executor = world.executor(state);
    executor
        .execute(prompt(&world.session_id, &world.cwd, "first"))
        .await
        .unwrap();
    executor
        .execute(prompt(&world.session_id, &world.cwd, "second"))
        .await
        .unwrap();
    assert_eq!(world.calls.load(Ordering::SeqCst), 0);
    assert!(!world.baseline_file().exists());
}

/// No summary to be had: the replay is truncated and the truncation kept,
/// as `/compact` keeps one, and the turn still goes out.
#[tokio::test]
async fn a_failed_summary_still_shortens_the_replay_and_the_turn_runs() {
    let (world, state) = World::new("replay_compaction_failed", 80_000, true);
    let big = "z".repeat(world.threshold() as usize * 5);
    // Long enough in messages too that truncation has something to drop.
    for round in 3..8 {
        let id = format!("toolu_{round}");
        world
            .mock
            .push_turn(tool_turn("msg_tx", "Read", &id, "{\"path\":\"c.rs\"}"));
    }
    push_working_turn(&world.mock, text_turn("msg_1", "done"));
    world.mock.push_turn(text_turn("msg_2", "second answer"));
    let executor = world.executor(state);
    executor
        .execute(prompt(&world.session_id, &world.cwd, &big))
        .await
        .unwrap();
    let before = world.calls.load(Ordering::SeqCst);
    let outcome = executor
        .execute(prompt(&world.session_id, &world.cwd, "second question"))
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, AcpStopReason::EndTurn);
    assert!(
        world.calls.load(Ordering::SeqCst) > before,
        "the summariser was asked"
    );
    assert!(world.baseline_file().exists(), "the truncation is kept too");
    let second = request_text(world.mock.captured_requests().last().unwrap());
    assert!(!second.contains(COMPACT_SUMMARY_MARKER));
    // Truncation keeps a bounded share of the earliest prompts, not all.
    assert!(
        second.len() < big.len() / 2,
        "{} of {}",
        second.len(),
        big.len()
    );
    assert!(second.contains("second question"));
}

fn assistant_row(uuid: &str, usage: serde_json::Value) -> rebon_session::TranscriptEntry {
    rebon_session::TranscriptEntry {
        entry_type: "assistant".into(),
        uuid: uuid.into(),
        parent_uuid: None,
        timestamp: None,
        raw: json!({"type": "assistant", "message": {"role": "assistant", "usage": usage}}),
    }
}

fn user_row(uuid: &str) -> rebon_session::TranscriptEntry {
    rebon_session::TranscriptEntry {
        entry_type: "user".into(),
        uuid: uuid.into(),
        parent_uuid: None,
        timestamp: None,
        raw: json!({"type": "user", "message": {"role": "user", "content": "hi"}}),
    }
}

#[test]
fn the_measured_context_is_the_longest_request_after_the_summary() {
    let raw = vec![
        user_row("u1"),
        assistant_row("a1", json!({"input_tokens": 900_000, "output_tokens": 1})),
        user_row("u2"),
        assistant_row(
            "a2",
            json!({"input_tokens": 10, "cache_read_input_tokens": 150_000, "output_tokens": 90}),
        ),
        assistant_row("a3", json!({"input_tokens": 20_000, "output_tokens": 5})),
        assistant_row("a4", json!({})),
    ];
    use super::super::replay_window::measured_context_tokens;
    // Without a summary, every reply counts.
    assert_eq!(measured_context_tokens(&raw, None), Some(900_001));
    // After one anchored on u2, only what came later.
    assert_eq!(measured_context_tokens(&raw, Some("u2")), Some(150_100));
    // Nothing after the anchor reported usage.
    assert_eq!(measured_context_tokens(&raw, Some("a4")), None);
    assert_eq!(measured_context_tokens(&raw[..1], None), None);
}

//! One response's tool calls run as ordered stages: concurrency-safe runs in
//! parallel, everything else alone and in provider order, and a failed step
//! strands the mutations planned after it.

use super::*;
use crate::query::tool_schedule::call_is_concurrency_safe;

fn n_tool_turn(id: &str, calls: &[(&str, &str, Value)]) -> Vec<StreamEvent> {
    let mut events = vec![message_start(id)];
    for (index, (tool_name, tool_id, input)) in calls.iter().enumerate() {
        events.push(StreamEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::ToolUse {
                id: (*tool_id).into(),
                name: (*tool_name).into(),
            },
        });
        events.push(StreamEvent::ContentBlockDelta {
            index,
            delta: ContentBlockDelta::InputJsonDelta {
                partial_json: input.to_string(),
            },
        });
        events.push(StreamEvent::ContentBlockStop { index });
    }
    events.extend([
        StreamEvent::MessageDelta {
            delta: MessageDeltaFields {
                stop_reason: Some(StopReason::ToolUse),
                usage: Usage {
                    output_tokens: 12,
                    ..Default::default()
                },
            },
        },
        StreamEvent::MessageStop,
    ]);
    events
}

fn file_tools_engine() -> Arc<Engine> {
    build_engine_with_tools(vec![
        Arc::new(rebon_tool::ReadTool),
        Arc::new(rebon_tool::WriteTool),
        Arc::new(rebon_tool::EditTool),
        Arc::new(rebon_tool::BashTool::default()),
    ])
}

/// Run one batch, then a closing text turn; returns the events and the
/// requests the model saw.
async fn run_batch(
    engine: Arc<Engine>,
    cwd: &Path,
    calls: &[(&str, &str, Value)],
) -> (Vec<QueryEvent>, Vec<rebon_api::CreateMessageRequest>) {
    run_batch_in(
        engine,
        ToolContext::new().with_cwd(cwd.to_string_lossy().to_string()),
        calls,
    )
    .await
}

async fn run_batch_in(
    engine: Arc<Engine>,
    context: ToolContext,
    calls: &[(&str, &str, Value)],
) -> (Vec<QueryEvent>, Vec<rebon_api::CreateMessageRequest>) {
    let mock = MockModelClient::new();
    mock.push_turn(n_tool_turn("msg_batch", calls));
    mock.push_turn(text_turn("msg_done", "done"));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("do the steps")])
        .with_tools(tools_from_engine(&engine));
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        context,
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;
    (events, mock.captured_requests())
}

fn result_text(requests: &[rebon_api::CreateMessageRequest], tool_id: &str) -> (String, bool) {
    let (_, result) = replayed_tool_call(&requests[1].messages, tool_id);
    (result.content.to_plain_text(), result.is_error)
}

/// The `tool-start` / `tool-result` events in arrival order, with the id.
fn dispatch_order(events: &[QueryEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            QueryEvent::ToolDispatchStart { tool_use_id, .. } => {
                Some(format!("start:{tool_use_id}"))
            }
            QueryEvent::ToolDispatchResult { tool_use_id, .. } => {
                Some(format!("result:{tool_use_id}"))
            }
            _ => None,
        })
        .collect()
}

/// A model's habitual Read — one `file_path`, relative to the working
/// directory — gets through dispatch's schema check and reads the file,
/// instead of costing a round on "`targets` is missing".
#[tokio::test]
async fn a_conventional_relative_read_goes_through_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.py"), "print('hi')\n").unwrap();
    let (_, requests) = run_batch(
        file_tools_engine(),
        dir.path(),
        &[
            ("Read", "toolu_plain", json!({"file_path": "x.py"})),
            (
                "Read",
                "toolu_alias",
                json!({"path": "./x.py", "offset": 1, "limit": 5}),
            ),
        ],
    )
    .await;

    for id in ["toolu_plain", "toolu_alias"] {
        let (text, is_error) = result_text(&requests, id);
        assert!(!is_error, "{id}: {text}");
        assert!(text.contains("print('hi')"), "{id}: {text}");
    }
}

#[tokio::test]
async fn write_then_bash_read_in_one_batch_sees_the_new_content() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("fresh.txt");
    let (_, requests) = run_batch(
        file_tools_engine(),
        dir.path(),
        &[
            (
                "Write",
                "toolu_write",
                json!({"file_path": file, "content": "written-this-batch\n"}),
            ),
            (
                "Bash",
                "toolu_cat",
                json!({"command": format!("cat {}", file.display())}),
            ),
        ],
    )
    .await;

    assert_eq!(requests.len(), 2, "one batch, one follow-up request");
    let (write, write_error) = result_text(&requests, "toolu_write");
    assert!(!write_error, "{write}");
    let (cat, cat_error) = result_text(&requests, "toolu_cat");
    assert!(!cat_error, "{cat}");
    assert!(cat.contains("written-this-batch"), "{cat}");
}

#[tokio::test]
async fn two_edits_to_one_file_in_one_batch_both_apply_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("lib.rs");
    std::fs::write(&file, "alpha\nbeta\n").unwrap();
    let (_, requests) = run_batch(
        file_tools_engine(),
        dir.path(),
        &[
            (
                "Edit",
                "toolu_first",
                json!({"file_path": file, "old_string": "alpha", "new_string": "one"}),
            ),
            // Anchored on the first edit's output: only correct if it ran first.
            (
                "Edit",
                "toolu_second",
                json!({"file_path": file, "old_string": "one\nbeta", "new_string": "one\ntwo"}),
            ),
        ],
    )
    .await;

    assert_eq!(requests.len(), 2);
    for id in ["toolu_first", "toolu_second"] {
        let (text, is_error) = result_text(&requests, id);
        assert!(!is_error, "{id}: {text}");
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\n");
}

#[tokio::test]
async fn edit_after_an_outside_modification_applies_when_the_anchor_is_unique() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    std::fs::write(&file, "alpha\nbeta\n").unwrap();
    let (_, requests) = run_batch(
        file_tools_engine(),
        dir.path(),
        &[
            ("Read", "toolu_read", json!({"targets": [{"path": file}]})),
            // Changes the file behind the Read's back, as a formatter would.
            (
                "Bash",
                "toolu_append",
                json!({"command": format!("printf 'tail\\n' >> {}", file.display())}),
            ),
            (
                "Edit",
                "toolu_edit",
                json!({"file_path": file, "old_string": "alpha", "new_string": "ALPHA"}),
            ),
        ],
    )
    .await;

    assert_eq!(requests.len(), 2);
    let (edit, is_error) = result_text(&requests, "toolu_edit");
    assert!(!is_error, "{edit}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "ALPHA\nbeta\ntail\n"
    );
}

#[tokio::test]
async fn edit_not_found_returns_the_closest_region_of_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("main.rs");
    let body: String = (1..=12)
        .map(|n| {
            if n == 7 {
                "    let total = compute(a, b);\n".to_string()
            } else {
                format!("// filler line {n}\n")
            }
        })
        .collect();
    std::fs::write(&file, &body).unwrap();
    let (_, requests) = run_batch(
        file_tools_engine(),
        dir.path(),
        &[(
            "Edit",
            "toolu_edit",
            json!({
                "file_path": file,
                "old_string": "let total = compute(a,b);",
                "new_string": "let total = compute(b, a);"
            }),
        )],
    )
    .await;

    assert_eq!(requests.len(), 2, "the snippet arrives with the error");
    let (text, is_error) = result_text(&requests, "toolu_edit");
    assert!(is_error, "{text}");
    assert!(text.contains("let total = compute(a, b);"), "{text}");
    assert!(text.contains('7'), "names the line: {text}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), body, "untouched");
}

/// Each response gets its own record of changed files: an insert by line
/// number after an Edit of the same file in that response is refused, not
/// applied at a line the Edit moved.
#[tokio::test]
async fn a_positional_edit_after_a_change_in_the_same_response_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("list.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
    let engine = build_engine_with_tools(vec![
        Arc::new(rebon_tool::EditTool),
        Arc::new(rebon_tool::StrReplaceEditorTool),
    ]);
    let (_, requests) = run_batch(
        engine,
        dir.path(),
        &[
            (
                "Edit",
                "toolu_edit",
                json!({"file_path": file, "old_string": "one\n", "new_string": "zero\none\n"}),
            ),
            (
                "str_replace_editor",
                "toolu_insert",
                json!({"command": "insert", "path": file, "insert_line": 2, "new_str": "between"}),
            ),
        ],
    )
    .await;

    let (edit, is_error) = result_text(&requests, "toolu_edit");
    assert!(!is_error, "{edit}");
    let (insert, is_error) = result_text(&requests, "toolu_insert");
    assert!(is_error, "{insert}");
    assert!(insert.contains("earlier call in this response"), "{insert}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "zero\none\ntwo\nthree\n"
    );
}

#[tokio::test]
async fn a_failed_mutation_skips_the_later_mutations_of_its_batch() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    let c = dir.path().join("c.txt");
    let (events, requests) = run_batch(
        file_tools_engine(),
        dir.path(),
        &[
            (
                "Bash",
                "toolu_ok",
                json!({"command": format!("echo one > {}", a.display())}),
            ),
            ("Bash", "toolu_fail", json!({"command": "exit 3"})),
            (
                "Write",
                "toolu_write",
                json!({"file_path": b, "content": "never"}),
            ),
            (
                "Bash",
                "toolu_read_only",
                json!({"command": "echo still-runs"}),
            ),
            (
                "Bash",
                "toolu_later",
                json!({"command": format!("echo two > {}", c.display())}),
            ),
        ],
    )
    .await;

    assert_eq!(requests.len(), 2);
    assert!(a.exists(), "the step before the failure ran");
    assert!(!b.exists() && !c.exists(), "the steps after it did not");
    let skipped = "skipped: call #2 (Bash) failed earlier in this batch; re-issue after fixing it";
    for id in ["toolu_write", "toolu_later"] {
        let (text, is_error) = result_text(&requests, id);
        assert!(is_error, "{id}");
        assert_eq!(text, skipped, "{id}");
    }
    let (read_only, is_error) = result_text(&requests, "toolu_read_only");
    assert!(!is_error, "{read_only}");
    assert!(read_only.contains("still-runs"), "{read_only}");
    // A skipped call still gets a card: a start and its result.
    let order = dispatch_order(&events);
    assert!(order.contains(&"start:toolu_write".to_string()));
    assert!(order.contains(&"result:toolu_write".to_string()));
}

/// A command that outlived its timeout keeps running in the background. It
/// has not finished, so the mutation planned after it is skipped with that
/// reason; a read-only call still runs.
#[cfg(unix)]
#[tokio::test]
async fn a_command_still_running_after_its_timeout_skips_the_later_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let after = dir.path().join("after.txt");
    let registry = Arc::new(rebon_tool::ShellProcessRegistry::new());
    let context = ToolContext::new()
        .with_cwd(dir.path().to_string_lossy().to_string())
        .with_session_id("schedule-handoff")
        .with_shell_process_registry(Arc::clone(&registry));
    let (_, requests) = run_batch_in(
        file_tools_engine(),
        context,
        &[
            (
                "Bash",
                "toolu_slow",
                json!({"command": "sleep 3", "timeout": 200}),
            ),
            (
                "Write",
                "toolu_write",
                json!({"file_path": after, "content": "never"}),
            ),
            (
                "Bash",
                "toolu_read_only",
                json!({"command": "echo still-runs"}),
            ),
        ],
    )
    .await;

    assert_eq!(requests.len(), 2);
    let (slow, is_error) = result_text(&requests, "toolu_slow");
    assert!(!is_error, "{slow}");
    assert!(slow.contains("\"backgrounded\":true"), "{slow}");
    let (write, is_error) = result_text(&requests, "toolu_write");
    assert!(is_error, "{write}");
    assert_eq!(
        write,
        "skipped: call #1 (Bash) is still running in the background after its timeout; \
         re-issue once it has finished"
    );
    assert!(!after.exists());
    let (read_only, is_error) = result_text(&requests, "toolu_read_only");
    assert!(!is_error && read_only.contains("still-runs"), "{read_only}");
}

#[tokio::test]
async fn read_only_bash_calls_share_a_stage_and_a_mutating_one_stands_alone() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    let (events, requests) = run_batch(
        file_tools_engine(),
        dir.path(),
        &[
            ("Bash", "toolu_a", json!({"command": "echo a"})),
            ("Bash", "toolu_b", json!({"command": "pwd ; echo b"})),
            (
                "Bash",
                "toolu_c",
                json!({"command": format!("echo c > {}", out.display())}),
            ),
            ("Bash", "toolu_d", json!({"command": "echo d"})),
        ],
    )
    .await;
    assert_eq!(requests.len(), 2);

    let order = dispatch_order(&events);
    // Both read-only calls start before either finishes...
    let position = |entry: &str| order.iter().position(|seen| seen == entry).unwrap();
    assert!(
        position("start:toolu_b") < position("result:toolu_a"),
        "{order:?}"
    );
    assert!(
        position("start:toolu_a") < position("result:toolu_b"),
        "{order:?}"
    );
    // ...and the redirecting one waits for both, and holds back the last.
    assert!(
        position("result:toolu_a") < position("start:toolu_c"),
        "{order:?}"
    );
    assert!(
        position("result:toolu_b") < position("start:toolu_c"),
        "{order:?}"
    );
    assert!(
        position("result:toolu_c") < position("start:toolu_d"),
        "{order:?}"
    );
}

/// A concurrency-safe tool that only returns once `parties` calls are
/// inside it at the same time: it times out unless they really overlap.
struct RendezvousTool {
    barrier: Arc<tokio::sync::Barrier>,
}

#[async_trait]
impl Tool for RendezvousTool {
    fn id(&self) -> ToolId {
        ToolId::new("Rendezvous")
    }

    fn description(&self) -> &str {
        "waits for its siblings"
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({ "type": "object" })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        false
    }

    async fn call(&self, _input: Value, _context: &ToolContext) -> ToolResult<Value> {
        tokio::time::timeout(std::time::Duration::from_secs(10), self.barrier.wait())
            .await
            .map_err(|_| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!("siblings never arrived"),
            })?;
        Ok(json!({"met": true}))
    }
}

#[tokio::test]
async fn concurrency_safe_calls_really_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let engine = build_engine_with(Arc::new(RendezvousTool {
        barrier: Arc::new(tokio::sync::Barrier::new(3)),
    }));
    let (_, requests) = run_batch(
        engine,
        dir.path(),
        &[
            ("Rendezvous", "toolu_1", json!({})),
            ("Rendezvous", "toolu_2", json!({})),
            ("Rendezvous", "toolu_3", json!({})),
        ],
    )
    .await;
    assert_eq!(requests.len(), 2);
    for id in ["toolu_1", "toolu_2", "toolu_3"] {
        let (text, is_error) = result_text(&requests, id);
        assert!(!is_error, "{id}: {text}");
    }
}

#[tokio::test]
async fn cancelling_a_barrier_never_starts_the_calls_queued_behind_it() {
    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let recording = Arc::new(RecordingTool::new("Recorder", json!({"ok": true})));
    let engine = build_engine_with_tools(vec![
        Arc::new(HangingTool {
            started: started.clone(),
            dropped: dropped.clone(),
        }),
        recording.clone(),
    ]);
    let mock = MockModelClient::new();
    mock.push_turn(n_tool_turn(
        "msg_batch",
        &[
            ("Hanging", "toolu_hang", json!({})),
            ("Recorder", "toolu_queued", json!({})),
        ],
    ));
    let client: Arc<dyn ModelClient> = Arc::new(mock.clone());
    let cancel = CancelToken::new();
    let params = QueryParams::new("mock", vec![ApiMessage::user_text("go")])
        .with_tools(tools_from_engine(&engine));
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        cancel.clone(),
    );
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        if matches!(&event, QueryEvent::ToolDispatchStart { tool_use_id, .. } if tool_use_id == "toolu_hang")
        {
            while !started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            cancel.cancel();
        }
        let terminal = event.is_terminal();
        events.push(event);
        if terminal {
            break;
        }
    }

    assert!(events
        .iter()
        .any(|event| matches!(event, QueryEvent::Cancelled)));
    assert_eq!(recording.call_count(), 0);
    let order = dispatch_order(&events);
    assert!(
        !order.contains(&"start:toolu_queued".to_string()),
        "{order:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        QueryEvent::ToolDispatchResult { tool_use_id, outcome: Err(err), .. }
            if tool_use_id == "toolu_queued" && err == "Interrupted by user"
    )));
    assert_eq!(mock.captured_requests().len(), 1);
}

#[test]
fn concurrency_classification_follows_each_tool() {
    let engine = file_tools_engine();
    let context = ToolContext::new();
    let tools = engine.tool_resolver_for_context(&context);
    let safe = |name: &str, input: Value, writers: bool| {
        call_is_concurrency_safe(tools.as_ref(), &context, name, &input, writers)
    };

    assert!(safe("Read", json!({"targets": []}), false));
    assert!(!safe("Write", json!({}), false));
    assert!(!safe("Edit", json!({}), false));
    assert!(safe("Bash", json!({"command": "git status ; ls"}), false));
    assert!(!safe("Bash", json!({"command": "mkdir x"}), false));
    assert!(!safe("Bash", json!({"command": "cd x && ls"}), false));
    assert!(!safe("Bash", json!({"command": "ls > listing"}), false));
    assert!(!safe("Bash", json!({"command": "cargo test"}), false));
    assert!(safe(
        rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
        json!({"tool_name": "Bash", "arguments": {"command": "ls"}}),
        false
    ));
    assert!(!safe(
        rebon_tool::INVOKE_DEFERRED_TOOL_NAME,
        json!({"tool_name": "Write", "arguments": {}}),
        false
    ));
    assert!(!safe("NoSuchTool", json!({}), false));

    let agent = rebon_tool::AGENT_TOOL_NAME;
    assert!(safe(
        agent,
        json!({"prompt": "look", "subagent_type": "Explore"}),
        false
    ));
    assert!(!safe(agent, json!({"prompt": "implement it"}), false));
    assert!(safe(agent, json!({"prompt": "implement it"}), true));
    assert!(safe(
        agent,
        json!({"prompt": "implement it", "run_in_background": true}),
        false
    ));
    assert!(safe(
        agent,
        json!({"prompt": "implement it", "isolation": "worktree"}),
        false
    ));
}

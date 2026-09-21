use super::*;

// ── Task reconciliation gate ───────────────────────────────────

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn task_reconciliation_gate_injects_once_for_pending_task() {
    let _env = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _config_dir = EnvVarGuard::set("REBON_CONFIG_DIR", temp.path().to_str().unwrap());
    let _tasks_enabled = EnvVarGuard::set("REBON_ENABLE_TASKS", "1");
    let task_list_id = "query-task-reconciliation-pending";

    let engine = build_engine_with(Arc::new(rebon_plugin_tasks::TaskCreateTool));
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_create",
        "TaskCreate",
        "toolu_create",
        r#"{"subject":"Implement gate","description":"Finish the implementation"}"#,
    ));
    client.push_turn(text_turn("msg_first_done", "implementation finished"));
    client.push_turn(text_turn(
        "msg_blocked",
        "verification is blocked by the environment",
    ));
    let captured_client = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("implement it")])
        .with_tools(tools_from_engine(&engine));
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new()
            .with_cwd("/tmp/repo")
            .with_task_list_id(task_list_id),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    let requests = captured_client.captured_requests();
    assert_eq!(requests.len(), 3);
    let final_request_text = text_message_texts(&requests[2].messages).join("\n");
    assert!(final_request_text.contains("Before finishing, reconcile the tasks touched"));
    assert!(final_request_text.contains("- #1: pending"));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, QueryEvent::IterationComplete { .. }))
            .count(),
        3
    );
    match events.last().expect("stream ends with Done") {
        QueryEvent::Done { final_message, .. } => {
            assert_eq!(
                final_message.text(),
                "verification is blocked by the environment"
            );
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn task_reconciliation_gate_skips_tasks_completed_in_the_same_turn() {
    let _env = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _config_dir = EnvVarGuard::set("REBON_CONFIG_DIR", temp.path().to_str().unwrap());
    let _tasks_enabled = EnvVarGuard::set("REBON_ENABLE_TASKS", "1");
    let task_list_id = "query-task-reconciliation-completed";

    let engine = build_engine_with_tools(vec![
        Arc::new(rebon_plugin_tasks::TaskCreateTool) as Arc<dyn Tool>,
        Arc::new(rebon_plugin_tasks::TaskUpdateTool) as Arc<dyn Tool>,
    ]);
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_create",
        "TaskCreate",
        "toolu_create",
        r#"{"subject":"Implement gate","description":"Finish the implementation"}"#,
    ));
    client.push_turn(tool_turn(
        "msg_complete",
        "TaskUpdate",
        "toolu_complete",
        r#"{"taskId":"1","status":"completed"}"#,
    ));
    client.push_turn(text_turn("msg_done", "done"));
    let captured_client = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("implement it")])
        .with_tools(tools_from_engine(&engine));
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new()
            .with_cwd("/tmp/repo")
            .with_task_list_id(task_list_id),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert_eq!(captured_client.captured_requests().len(), 3);
    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
}

#[tokio::test]
async fn task_reconciliation_gate_ignores_failed_task_writes() {
    let engine = build_engine_with(Arc::new(RecordingTool::failing("TaskCreate")));
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_create",
        "TaskCreate",
        "toolu_create",
        r#"{"subject":"Implement gate","description":"Finish the implementation"}"#,
    ));
    client.push_turn(text_turn("msg_done", "could not create the task"));
    let captured_client = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("implement it")])
        .with_tools(tools_from_engine(&engine));
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    assert_eq!(captured_client.captured_requests().len(), 2);
    assert!(matches!(events.last(), Some(QueryEvent::Done { .. })));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn task_reconciliation_gate_at_iteration_limit_allows_update_and_final_response() {
    let _env = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _config_dir = EnvVarGuard::set("REBON_CONFIG_DIR", temp.path().to_str().unwrap());
    let _tasks_enabled = EnvVarGuard::set("REBON_ENABLE_TASKS", "1");
    let task_list_id = "query-task-reconciliation-terminal-update";

    let engine = build_engine_with_tools(vec![
        Arc::new(rebon_plugin_tasks::TaskCreateTool) as Arc<dyn Tool>,
        Arc::new(rebon_plugin_tasks::TaskUpdateTool) as Arc<dyn Tool>,
    ]);
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_create",
        "TaskCreate",
        "toolu_create",
        r#"{"subject":"Implement gate","description":"Finish the implementation"}"#,
    ));
    client.push_turn(text_turn("msg_first_done", "implementation finished"));
    client.push_turn(tool_turn(
        "msg_complete",
        "TaskUpdate",
        "toolu_complete",
        r#"{"taskId":"1","status":"completed"}"#,
    ));
    client.push_turn(text_turn("msg_final", "task reconciled"));
    let captured_client = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("implement it")])
        .with_tools(tools_from_engine(&engine))
        .with_max_iterations(2);
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new()
            .with_cwd("/tmp/repo")
            .with_task_list_id(task_list_id),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    let requests = captured_client.captured_requests();
    assert_eq!(requests.len(), 4);
    assert!(text_message_texts(&requests[2].messages)
        .join("\n")
        .contains("Before finishing, reconcile the tasks touched"));
    assert!(!events
        .iter()
        .any(|event| matches!(event, QueryEvent::IterationLimitReached { .. })));
    assert!(matches!(
        events.last(),
        Some(QueryEvent::Done { final_message, .. }) if final_message.text() == "task reconciled"
    ));
    // The scripted client replays its turns whether or not the tools behind
    // them succeeded, so a TaskCreate that never wrote anything used to
    // surface here as `Option::unwrap()` on `None` — a panic that names
    // neither the tool nor the directory it was looking in.
    let stored = rebon_tool::tasks::get_task(task_list_id, "1")
        .expect("task store is readable")
        .unwrap_or_else(|| {
            panic!(
                "TaskCreate left no task in {}",
                rebon_tool::tasks::tasks_dir(task_list_id).display()
            )
        });
    assert_eq!(stored.status, rebon_tool::tasks::TaskListStatus::Completed);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn task_reconciliation_gate_after_terminal_attachment_reaches_the_model() {
    let _env = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _config_dir = EnvVarGuard::set("REBON_CONFIG_DIR", temp.path().to_str().unwrap());
    let _tasks_enabled = EnvVarGuard::set("REBON_ENABLE_TASKS", "1");
    let task_list_id = "query-task-reconciliation-terminal-attachment";

    let engine = build_engine_with(Arc::new(rebon_plugin_tasks::TaskCreateTool));
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_create",
        "TaskCreate",
        "toolu_create",
        r#"{"subject":"Implement gate","description":"Finish the implementation"}"#,
    ));
    client.push_turn(text_turn("msg_first_done", "implementation finished"));
    client.push_turn(text_turn("msg_attachment", "attachment handled"));
    client.push_turn(text_turn(
        "msg_blocked",
        "verification remains blocked after the attachment",
    ));
    let captured_client = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);
    let poller = Arc::new(EagerPoller::new(vec![
        Vec::new(),
        vec![ApiMessage::user_text("terminal attachment")],
    ]));

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("implement it")])
        .with_tools(tools_from_engine(&engine))
        .with_max_iterations(2)
        .with_attachment_poller(
            poller.clone() as Arc<dyn crate::query::AttachmentPoller>,
            "session-poller",
            "turn-poller",
        );
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new()
            .with_cwd("/tmp/repo")
            .with_task_list_id(task_list_id),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    let requests = captured_client.captured_requests();
    assert_eq!(requests.len(), 4);
    assert!(text_message_texts(&requests[2].messages)
        .join("\n")
        .contains("terminal attachment"));
    assert!(text_message_texts(&requests[3].messages)
        .join("\n")
        .contains("Before finishing, reconcile the tasks touched"));
    assert_eq!(poller.calls.load(Ordering::SeqCst), 2);
    assert!(!events
        .iter()
        .any(|event| matches!(event, QueryEvent::IterationLimitReached { .. })));
    assert!(matches!(
        events.last(),
        Some(QueryEvent::Done { final_message, .. })
            if final_message.text() == "verification remains blocked after the attachment"
    ));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn task_reconciliation_and_query_hook_have_separate_followups() {
    let _env = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _config_dir = EnvVarGuard::set("REBON_CONFIG_DIR", temp.path().to_str().unwrap());
    let _tasks_enabled = EnvVarGuard::set("REBON_ENABLE_TASKS", "1");
    let task_list_id = "query-task-reconciliation-validator";

    let hook_calls = Arc::new(AtomicUsize::new(0));

    let engine = build_engine_with_tools(vec![
        Arc::new(rebon_plugin_tasks::TaskCreateTool) as Arc<dyn Tool>,
        Arc::new(rebon_plugin_tasks::TaskUpdateTool) as Arc<dyn Tool>,
    ]);
    let client = MockModelClient::new();
    client.push_turn(tool_turn(
        "msg_create",
        "TaskCreate",
        "toolu_create",
        r#"{"subject":"Implement gate","description":"Finish the implementation"}"#,
    ));
    client.push_turn(text_turn("msg_first_done", "implementation finished"));
    client.push_turn(text_turn("msg_after_reminder", "blocker explained"));
    client.push_turn(tool_turn(
        "msg_complete",
        "TaskUpdate",
        "toolu_complete",
        r#"{"taskId":"1","status":"completed"}"#,
    ));
    client.push_turn(text_turn("msg_final", "hook satisfied"));
    let captured_client = client.clone();
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("implement it")])
        .with_tools(tools_from_engine(&engine))
        .with_max_iterations(2)
        .with_turn_hook(
            "tests/terminal-coercion",
            crate::turn_hook::Order::LAST,
            test_turn_end_hook({
                let hook_calls = hook_calls.clone();
                move |event, context| {
                    if hook_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        continue_terminal_turn(
                            event,
                            context,
                            ApiMessage::user_text("hook coercion"),
                            None,
                        );
                    }
                }
            }),
        );
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new()
            .with_cwd("/tmp/repo")
            .with_task_list_id(task_list_id),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    let requests = captured_client.captured_requests();
    assert_eq!(requests.len(), 5);
    assert!(text_message_texts(&requests[2].messages)
        .join("\n")
        .contains("Before finishing, reconcile the tasks touched"));
    assert!(text_message_texts(&requests[3].messages)
        .join("\n")
        .contains("hook coercion"));
    assert_eq!(hook_calls.load(Ordering::SeqCst), 2);
    assert!(!events
        .iter()
        .any(|event| matches!(event, QueryEvent::IterationLimitReached { .. })));
    assert!(matches!(
        events.last(),
        Some(QueryEvent::Done { final_message, .. }) if final_message.text() == "hook satisfied"
    ));
}

#[test]
fn task_turn_tracker_reports_only_unfinished_successful_writes_once() {
    let _env = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _config_dir = EnvVarGuard::set("REBON_CONFIG_DIR", temp.path().to_str().unwrap());
    let task_list_id = "task-turn-tracker-statuses";

    let pending_id = rebon_tool::tasks::create_task(
        task_list_id,
        rebon_tool::tasks::NewTask {
            subject: "pending".into(),
            status: rebon_tool::tasks::TaskListStatus::Pending,
            ..Default::default()
        },
    )
    .unwrap();
    let in_progress_id = rebon_tool::tasks::create_task(
        task_list_id,
        rebon_tool::tasks::NewTask {
            subject: "in progress".into(),
            status: rebon_tool::tasks::TaskListStatus::InProgress,
            ..Default::default()
        },
    )
    .unwrap();
    let completed_id = rebon_tool::tasks::create_task(
        task_list_id,
        rebon_tool::tasks::NewTask {
            subject: "completed".into(),
            status: rebon_tool::tasks::TaskListStatus::Completed,
            ..Default::default()
        },
    )
    .unwrap();
    let deleted_id = rebon_tool::tasks::create_task(
        task_list_id,
        rebon_tool::tasks::NewTask {
            subject: "deleted".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(rebon_tool::tasks::delete_task(task_list_id, &deleted_id).unwrap());

    let runtime = task_reconciliation_runtime();
    record_task_results(
        &runtime,
        &[
            ("TaskCreate", json!({"task": {"id": pending_id}})),
            ("TaskUpdate", json!({"success": true, "taskId": pending_id})),
            (
                "TaskUpdate",
                json!({"success": true, "taskId": in_progress_id}),
            ),
            (
                "TaskUpdate",
                json!({"success": true, "taskId": completed_id}),
            ),
            ("TaskUpdate", json!({"success": true, "taskId": deleted_id})),
            ("TaskUpdate", json!({"success": false, "taskId": "missing"})),
            ("TaskList", json!({"tasks": [{"id": pending_id}]})),
            ("TaskGet", json!({"task": {"id": pending_id}})),
        ],
    );

    let history = drive_task_reconciliation(&runtime, task_list_id);
    let reminder = history.last().expect("pending task reminder");
    let text = text_message_texts(std::slice::from_ref(reminder)).join("\n");
    assert!(text.contains("- #1: pending"));
    assert!(text.contains("- #2: in_progress"));
    assert!(!text.contains("#3"));
    assert!(!text.contains("#4"));
    // The turn keeps its assistant response, and the reminder never repeats.
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role, Role::Assistant);
    assert!(drive_task_reconciliation(&runtime, task_list_id).is_empty());
}

#[test]
fn task_turn_tracker_uses_the_resolved_task_list_only() {
    let _env = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _config_dir = EnvVarGuard::set("REBON_CONFIG_DIR", temp.path().to_str().unwrap());

    let pending_id = rebon_tool::tasks::create_task(
        "task-list-a",
        rebon_tool::tasks::NewTask {
            subject: "pending".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let completed_id = rebon_tool::tasks::create_task(
        "task-list-b",
        rebon_tool::tasks::NewTask {
            subject: "completed".into(),
            status: rebon_tool::tasks::TaskListStatus::Completed,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(pending_id, completed_id);

    let runtime = task_reconciliation_runtime();
    record_task_results(
        &runtime,
        &[("TaskCreate", json!({"task": {"id": pending_id}}))],
    );

    let history = drive_task_reconciliation(&runtime, "task-list-a");
    let reminder = history.last().expect("list-a reminder");
    let text = text_message_texts(std::slice::from_ref(reminder)).join("\n");
    assert!(text.contains("- #1: pending"));
}

// ── query-local terminal hook ──────────────────────────────────

#[tokio::test]
async fn query_local_terminal_hook_injects_coercion_and_continues_loop() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tool = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    // Two text turns: first triggers the hook, second answers its coercion.
    let client = MockModelClient::new();
    client.push_turn(text_turn("msg_1", "first reply"));
    client.push_turn(text_turn("msg_2", "coercion reply"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("do the task")])
        .with_turn_hook(
            "tests/one-shot-coercion",
            crate::turn_hook::Order::LAST,
            test_turn_end_hook(move |event, context| {
                if call_count_clone.fetch_add(1, Ordering::Relaxed) == 0 {
                    continue_terminal_turn(
                        event,
                        context,
                        ApiMessage::user_text("[coercion] please try again"),
                        None,
                    );
                }
            }),
        );
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    // The hook ran on both terminal responses; only the first continued.
    assert_eq!(call_count.load(Ordering::Relaxed), 2);

    // Two IterationComplete events (one per model response).
    let iteration_count = events
        .iter()
        .filter(|e| matches!(e, QueryEvent::IterationComplete { .. }))
        .count();
    assert_eq!(iteration_count, 2);

    // Final message is from the second turn.
    let done = events.last().expect("stream ends with Done");
    match done {
        QueryEvent::Done { final_message, .. } => {
            assert_eq!(final_message.text(), "coercion reply");
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn query_local_terminal_hook_skips_tool_use_responses() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tool = Arc::new(RecordingTool::new("Read", json!({"ok": 1})));
    let tool_ref: Arc<dyn Tool> = tool.clone();
    let engine = build_engine_with(tool_ref);

    // One tool-use turn followed by a text turn.
    let client = MockModelClient::new();
    client.push_turn(tool_turn("msg_1", "Read", "toolu_1", "{\"path\":\"a.rs\"}"));
    client.push_turn(text_turn("msg_2", "done"));
    let client: Arc<dyn ModelClient> = Arc::new(client);

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let params = QueryParams::new("mock", vec![ApiMessage::user_text("do it")])
        .with_tools(tools_from_engine(&engine))
        .with_turn_hook(
            "tests/count-terminal-responses",
            crate::turn_hook::Order::LAST,
            test_turn_end_hook(move |_event, _context| {
                call_count_clone.fetch_add(1, Ordering::Relaxed);
            }),
        );
    let mut rx = run_query(
        engine,
        SessionHandle::new(client),
        params,
        ToolContext::new().with_cwd("/tmp/repo"),
        CancelToken::new(),
    );
    let events = drain(&mut rx).await;

    // The hook only runs once — on the second (text) turn.
    // The first (tool-use) turn does not hit the terminal phase.
    assert_eq!(call_count.load(Ordering::Relaxed), 1);

    let done = events.last().expect("stream ends with Done");
    match done {
        QueryEvent::Done { final_message, .. } => {
            assert_eq!(final_message.text(), "done");
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

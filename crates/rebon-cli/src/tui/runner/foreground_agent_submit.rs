use tokio::runtime::Handle;

use crate::tui::app::AppState;
use crate::tui::dispatch::take_submit_payload;
use crate::tui::wiring::TuiEngineSession;

use super::inject_system_message;
use super::layout_and_scroll::repin_transcript_to_bottom;
use super::live_agent_view::{
    is_read_only_agent_task, release_terminal_foreground_agent, switch_to_live_agent,
    sync_foreground_agent_view,
};
use super::local_agent_continuation::{
    interrupt_and_continue_local_agent_task, parse_agent_interrupt_redirect,
};

pub(super) fn submit_to_foregrounded_agent(
    app: &mut AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    task_id: &str,
    raw_text: &str,
) -> bool {
    if app.foregrounded_task_id.as_deref() == Some(task_id)
        && release_terminal_foreground_agent(app, session.engine_half.tasks.as_ref())
    {
        return false;
    }

    if is_read_only_agent_task(app, task_id) {
        inject_system_message(
            app,
            "error",
            "This external agent is view-only here. Stop it with Ctrl+C, or switch to Main to continue.",
        );
        app.follow_transcript_tail = true;
        return true;
    }

    let Some(submit) = take_submit_payload(app, raw_text) else {
        return false;
    };
    let message = submit.prompt_text().to_string();
    if message.trim().is_empty() {
        inject_system_message(app, "error", "Background agent messages must include text.");
        app.follow_transcript_tail = true;
        return true;
    }
    if !app.is_remote_agent_task(task_id) {
        if let Some(new_instruction) = parse_agent_interrupt_redirect(&message) {
            if new_instruction.trim().is_empty() {
                inject_system_message(
                    app,
                    "error",
                    "`/interrupt` requires a new instruction for the replacement agent.",
                );
                app.follow_transcript_tail = true;
                return true;
            }
            return match interrupt_and_continue_local_agent_task(
                session,
                handle,
                task_id,
                &new_instruction,
                false,
            ) {
                Ok(new_task_id) => {
                    let mut detached_prompt = None;
                    switch_to_live_agent(app, &mut detached_prompt, &new_task_id);
                    repin_transcript_to_bottom(app);
                    true
                }
                Err(err) => {
                    inject_system_message(app, "error", &err);
                    app.follow_transcript_tail = true;
                    false
                }
            };
        }
    }
    submit_message_to_foregrounded_agent(app, session, handle, task_id, message)
}

pub(super) fn send_message_to_agent_task(
    app: &AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    task_id: &str,
    message: String,
) -> Result<(), String> {
    let tasks = session.engine_half.tasks.as_ref();
    let kind = tasks
        .snapshot(&rebon_plugin_tasks::runtime::TaskId::new(task_id))
        .map(|snapshot| snapshot.kind);
    match kind {
        Some(rebon_plugin_tasks::runtime::TaskKind::LocalAgent) => {
            rebon_plugin_tasks::runtime::send_message_to_local_agent_task(tasks, task_id, message)
                .map_err(|error| error.display_message)
        }
        Some(rebon_plugin_tasks::runtime::TaskKind::InProcessTeammate) => {
            if let Some(manager) = session.engine_half.team_manager.as_ref() {
                return handle
                    .block_on(manager.send_message_to_task(task_id, message))
                    .map_err(|error| error.display_message);
            }
            let ok = rebon_plugin_tasks::runtime::inject_user_message_to_teammate(
                tasks,
                &rebon_plugin_tasks::runtime::TaskId::new(task_id),
                message,
            );
            if ok {
                Ok(())
            } else {
                Err(format!(
                    "failed to queue message for teammate task `{task_id}`"
                ))
            }
        }
        Some(other) => Err(format!(
            "Agent \"{task_id}\" is a {} task and cannot receive messages from the agent footer",
            other.as_str()
        )),
        None if app.remote_background_tasks.contains_key(task_id) => {
            let job_id = session
                .remote_background_attachment
                .as_ref()
                .map(|remote| remote.job_id.as_str())
                .ok_or_else(|| {
                    format!(
                        "Agent \"{task_id}\" is mirrored from another host, but that host connection is unavailable."
                    )
                })?;
            crate::background::reply_to_background_task(job_id, task_id.to_string(), message)
                .map_err(|error| error.to_string())
        }
        None => Err(format!("Agent \"{task_id}\" has no active task.")),
    }
}

fn submit_message_to_foregrounded_agent(
    app: &mut AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    task_id: &str,
    message: String,
) -> bool {
    let result = send_message_to_agent_task(app, session, handle, task_id, message);

    match result {
        Ok(()) => {
            sync_foreground_agent_view(app, session.engine_half.tasks.as_ref());
            repin_transcript_to_bottom(app);
            true
        }
        Err(err) => {
            inject_system_message(app, "error", &err);
            app.follow_transcript_tail = true;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::runtime::{Builder, Handle, Runtime};

    use crate::tui::app::AppState;

    use super::super::test_support::make_test_tui_session;
    use super::{send_message_to_agent_task, submit_to_foregrounded_agent};

    fn make_immediate_handle() -> (Runtime, Handle) {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let handle = runtime.handle().clone();
        (runtime, handle)
    }

    fn local_agent_snapshot(
        id: &str,
        status: rebon_plugin_tasks::runtime::TaskStatus,
        backgrounded: bool,
    ) -> rebon_plugin_tasks::runtime::TaskSnapshot {
        use rebon_plugin_tasks::runtime::{
            LocalAgentData, TaskData, TaskId, TaskKind, TaskSnapshot,
        };

        TaskSnapshot {
            id: TaskId::new(id),
            kind: TaskKind::LocalAgent,
            status,
            title: "worker".into(),
            last_progress: None,
            error: None,
            result: None,
            is_backgrounded: backgrounded,
            notified: false,
            start_time_ms: 0,
            end_time_ms: None,
            metadata: serde_json::json!({}),
            data: TaskData::LocalAgent(LocalAgentData {
                prompt: "do work".into(),
                agent_type: "general-purpose".into(),
                model: None,
                system: None,
                allowed_tools: None,
                token_count: 0,
                tool_use_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
                pending_messages: Vec::new(),
                retrieved: false,
            }),
        }
    }

    fn insert_local_agent_task(
        reg: &rebon_plugin_tasks::runtime::TaskRegistry,
        id: &str,
        status: rebon_plugin_tasks::runtime::TaskStatus,
        backgrounded: bool,
    ) -> rebon_types::PromptCancel {
        let cancel = rebon_types::PromptCancel::new();
        let snapshot = local_agent_snapshot(id, status, backgrounded);
        reg.insert(
            rebon_plugin_tasks::runtime::TaskId::new(id),
            snapshot,
            cancel.clone(),
        );
        cancel
    }

    fn insert_idle_teammate_task(reg: &rebon_plugin_tasks::runtime::TaskRegistry, id: &str) {
        use rebon_plugin_tasks::runtime::{
            mark_in_process_teammate_idle, register_in_process_teammate_task,
            InProcessTeammateTaskSpec, TaskId, TeammateIdentity,
        };

        let id = TaskId::new(id);
        register_in_process_teammate_task(
            reg,
            InProcessTeammateTaskSpec {
                id: id.clone(),
                identity: TeammateIdentity {
                    agent_id: "researcher@default".into(),
                    agent_name: "researcher".into(),
                    team_name: "default".into(),
                    color: None,
                    plan_mode_required: false,
                    parent_session_id: "session-test".into(),
                },
                prompt: "inspect the codebase".into(),
                model: None,
                model_profile: None,
                permission_mode: "default".into(),
                agent_type: Some("Explore".into()),
                description: Some("Repository explorer".into()),
            },
        );
        assert!(mark_in_process_teammate_idle(
            reg,
            &id,
            Some("ready".into())
        ));
    }

    #[test]
    fn remote_agent_message_requires_its_owner_connection() {
        let (_runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let session = make_test_tui_session();
        app.remote_background_tasks.insert(
            "agent-remote".into(),
            rebon_session_host::BackgroundTaskSnapshot {
                task: rebon_session_host::BackgroundTaskDescriptor {
                    task_id: "agent-remote".into(),
                    title: "Inspect remote session".into(),
                    kind: "local_agent".into(),
                    status: "running".into(),
                    is_backgrounded: true,
                    start_time_ms: 1,
                    end_time_ms: None,
                    last_progress: None,
                    error: None,
                    prompt: None,
                    parent_tool_call_id: None,
                    agent_id: Some("agent-remote".into()),
                    agent_name: Some("Explore".into()),
                    agent_type: Some("Explore".into()),
                    model: None,
                    token_count: None,
                    tool_use_count: None,
                    result: None,
                },
                updated_at_ms: 2,
                log_preview: Vec::new(),
                transcript: Vec::new(),
            },
        );
        let error = send_message_to_agent_task(
            &app,
            &session,
            &handle,
            "agent-remote",
            "please continue".into(),
        )
        .unwrap_err();

        assert!(error.contains("host connection is unavailable"));
    }

    #[test]
    fn direct_agent_message_uses_the_exact_session_registry() {
        let (runtime, handle) = make_immediate_handle();
        let app = AppState::new();
        let session = make_test_tui_session();
        insert_local_agent_task(
            session.engine_half.tasks.as_ref(),
            "agent-session",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );

        send_message_to_agent_task(
            &app,
            &session,
            &handle,
            "agent-session",
            "from foreground".into(),
        )
        .expect("session task accepts the message");

        let snapshot = session
            .engine_half
            .tasks
            .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-session"))
            .expect("session task");
        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = snapshot.data else {
            panic!("expected local agent");
        };
        assert_eq!(data.pending_messages, vec!["from foreground"]);
        assert!(app.tasks.snapshots().is_empty());
        runtime.shutdown_background();
    }

    #[test]
    fn submit_to_foregrounded_agent_queues_message_without_main_prompt() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        let reg = std::sync::Arc::new(reg);
        app.tasks = reg.clone();
        session.engine_half.tasks = reg.clone();
        app.main_agent_view = Some(crate::tui::app::StoredTranscriptView::default());
        app.foregrounded_task_id = Some("agent-1".into());
        app.input = "please continue".into();
        app.cursor_offset = app.input.len();

        assert!(submit_to_foregrounded_agent(
            &mut app,
            &session,
            &handle,
            "agent-1",
            "please continue"
        ));

        let snapshot = reg
            .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-1"))
            .expect("agent task");
        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &snapshot.data else {
            panic!("expected local agent");
        };
        assert_eq!(data.pending_messages, vec!["please continue".to_string()]);
        assert!(app.input.is_empty());
        assert_eq!(app.cursor_offset, 0);
        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-1"));
        assert!(matches!(
            app.rebon_tui.transcript.rows().iter().find(|row| matches!(
                row,
                rebon_tui::Message::User(user)
                    if user.message.content.iter().any(|block| matches!(
                        block,
                        rebon_tui::UserContentBlock::Text(text) if text.text == "please continue"
                    ))
            )),
            Some(_)
        ));
        runtime.shutdown_background();
    }

    #[test]
    fn submit_to_idle_teammate_queues_follow_up_without_releasing_agent_view() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let reg = rebon_plugin_tasks::runtime::TaskRegistry::new();
        insert_idle_teammate_task(&reg, "teammate-1");
        let reg = std::sync::Arc::new(reg);
        app.tasks = reg.clone();
        session.engine_half.tasks = reg.clone();
        app.main_agent_view = Some(crate::tui::app::StoredTranscriptView::default());
        app.foregrounded_task_id = Some("teammate-1".into());
        app.input = "inspect the parser next".into();
        app.cursor_offset = app.input.len();

        assert!(submit_to_foregrounded_agent(
            &mut app,
            &session,
            &handle,
            "teammate-1",
            "inspect the parser next"
        ));

        let snapshot = reg
            .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("teammate-1"))
            .expect("teammate task");
        let rebon_plugin_tasks::runtime::TaskData::InProcessTeammate(data) = &snapshot.data else {
            panic!("expected teammate");
        };
        assert_eq!(data.pending_user_messages.len(), 1);
        assert_eq!(
            data.pending_user_messages[0].message,
            "inspect the parser next"
        );
        assert!(app.input.is_empty());
        assert_eq!(app.foregrounded_task_id.as_deref(), Some("teammate-1"));
        runtime.shutdown_background();
    }

    #[test]
    fn submit_to_foregrounded_agent_interrupts_and_continues_with_context() {
        let (runtime, handle) = make_immediate_handle();
        let mut app = AppState::new();
        let session = make_test_tui_session();
        let reg = session.engine_half.tasks.clone();
        let cancel = insert_local_agent_task(
            &reg,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            false,
        );
        reg.update(
            &rebon_plugin_tasks::runtime::TaskId::new("agent-1"),
            |snapshot| {
                snapshot.title = "Explore installer".into();
                snapshot.metadata = serde_json::json!({ "agent_type": "explorer" });
                let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &mut snapshot.data
                else {
                    panic!("expected local agent");
                };
                data.prompt = "Find daemon install behavior".into();
                data.agent_type = "explorer".into();
                data.model = Some("model-x".into());
                data.system = Some("system-x".into());
                data.transcript.push(
                    rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry::Assistant {
                        text: "I found the postinstall script.".into(),
                    },
                );
            },
        );
        app.tasks = reg.clone();
        app.main_agent_view = Some(crate::tui::app::StoredTranscriptView::default());
        app.foregrounded_task_id = Some("agent-1".into());
        app.input = "/interrupt only inspect daemon registration".into();
        app.cursor_offset = app.input.len();

        assert!(submit_to_foregrounded_agent(
            &mut app,
            &session,
            &handle,
            "agent-1",
            "/interrupt only inspect daemon registration"
        ));

        assert!(cancel.is_cancelled());
        assert_eq!(
            reg.snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-1"))
                .expect("original task")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Killed
        );
        let new_snapshot = reg
            .snapshots()
            .into_iter()
            .find(|snapshot| snapshot.id.as_str() != "agent-1")
            .expect("replacement task");
        assert_eq!(
            app.foregrounded_task_id.as_deref(),
            Some(new_snapshot.id.as_str())
        );
        assert_eq!(
            new_snapshot.status,
            rebon_plugin_tasks::runtime::TaskStatus::Running
        );
        assert_eq!(
            new_snapshot
                .metadata
                .get("continuation_of")
                .and_then(serde_json::Value::as_str),
            Some("agent-1")
        );
        let rebon_plugin_tasks::runtime::TaskData::LocalAgent(data) = &new_snapshot.data else {
            panic!("expected local agent");
        };
        assert_eq!(data.agent_type, "explorer");
        assert_eq!(data.model.as_deref(), Some("model-x"));
        assert!(data
            .system
            .as_deref()
            .is_some_and(|system| system.starts_with("system-x")));
        assert!(data.allowed_tools.is_none());
        assert!(data.prompt.contains("Original task id: agent-1"));
        assert!(data.prompt.contains("Find daemon install behavior"));
        assert!(data.prompt.contains("I found the postinstall script."));
        assert!(data
            .prompt
            .contains("New user instruction:\nonly inspect daemon registration"));
        runtime.shutdown_background();
    }
}

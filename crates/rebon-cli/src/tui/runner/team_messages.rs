//! Team mailbox message classification, auto-approval of plan
//! requests, and the XML envelope used to inject teammate replies
//! into the user's transcript.

use crate::tui::app::AppState;

pub(super) const TEAM_LEAD_NAME: &str = "team-lead";

/// Classified team message types used by inbox polling.
#[derive(Debug)]
pub(super) enum TeamMessageType {
    PlanApprovalRequest,
    ShutdownApproved,
    TeammateTerminated,
    IdleNotification,
    Regular,
}

pub(super) fn classify_team_message(text: &str) -> TeamMessageType {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return TeamMessageType::Regular;
    };
    match value.get("type").and_then(|v| v.as_str()) {
        Some("plan_approval_request") => TeamMessageType::PlanApprovalRequest,
        Some("shutdown_approved") => TeamMessageType::ShutdownApproved,
        Some("teammate_terminated") => TeamMessageType::TeammateTerminated,
        Some("idle_notification") => TeamMessageType::IdleNotification,
        _ => TeamMessageType::Regular,
    }
}

pub(super) fn maybe_auto_approve_plan_request(
    team_name: &str,
    message: &rebon_tool::TeamMailboxMessage,
) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.text) else {
        return;
    };
    let Some(kind) = value.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    if kind != "plan_approval_request" {
        return;
    }
    let Some(request_id) = value.get("requestId").and_then(|v| v.as_str()) else {
        return;
    };
    let response = serde_json::json!({
        "type": "plan_approval_response",
        "requestId": request_id,
        "approved": true,
        "timestamp": rebon_types::wall_clock_ms_u128().to_string(),
        "permissionMode": "default",
    });
    let _ = rebon_tool::write_mailbox_message(
        team_name,
        &message.from,
        rebon_tool::TeamMailboxMessage {
            from: TEAM_LEAD_NAME.to_string(),
            text: response.to_string(),
            timestamp: rebon_types::wall_clock_ms_u128().to_string(),
            read: false,
            color: None,
            summary: Some("plan approved".into()),
        },
    );
}

pub(super) fn format_teammate_message(
    from: &str,
    text: &str,
    summary: Option<&str>,
    color: Option<&str>,
) -> String {
    let mut attrs = format!(" teammate_id=\"{}\"", escape_xml_attr(from));
    if let Some(summary) = summary {
        attrs.push_str(&format!(" summary=\"{}\"", escape_xml_attr(summary)));
    }
    if let Some(color) = color {
        attrs.push_str(&format!(" color=\"{}\"", escape_xml_attr(color)));
    }
    format!(
        "<teammate-message{attrs}>{}</teammate-message>",
        escape_xml_text(text)
    )
}

pub(super) fn drain_team_mailbox(app: &mut AppState, session_id: &str) {
    let mut team_names = Vec::new();
    // In-process teammate tasks are the runtime authority for mailbox ownership.
    // Filter by parent session so an idle TUI never consumes another session's
    // messages; terminal snapshots stay included so notifications written just
    // before a teammate shut down are still delivered.
    for snapshot in app.task_snapshots() {
        if let rebon_plugin_tasks::runtime::TaskData::InProcessTeammate(data) = &snapshot.data {
            if data.identity.parent_session_id == session_id
                && !team_names.contains(&data.identity.team_name)
            {
                team_names.push(data.identity.team_name.clone());
            }
        }
    }
    if team_names.is_empty() {
        return;
    }
    let unread_by_team = team_names
        .into_iter()
        .filter_map(|team_name| {
            let unread = drain_unread_team_mailbox(&team_name);
            (!unread.is_empty()).then_some((team_name, unread))
        })
        .collect::<Vec<_>>();
    if unread_by_team.is_empty() {
        return;
    }
    super::live_agent_view::with_main_agent_view(app, move |app| {
        for (team_name, unread) in unread_by_team {
            project_team_mailbox_messages(app, &team_name, unread);
        }
    });
}

fn drain_unread_team_mailbox(team_name: &str) -> Vec<rebon_tool::TeamMailboxMessage> {
    match rebon_tool::drain_unread_mailbox(team_name, TEAM_LEAD_NAME) {
        Ok(messages) => messages,
        Err(err) => {
            tracing::warn!(
                error = %err,
                team_name = %team_name,
                "rebon-cli: failed to drain team mailbox"
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
fn drain_team_mailbox_for_team(app: &mut AppState, team_name: &str) {
    let unread = drain_unread_team_mailbox(team_name);
    project_team_mailbox_messages(app, team_name, unread);
}

fn project_team_mailbox_messages(
    app: &mut AppState,
    team_name: &str,
    unread: Vec<rebon_tool::TeamMailboxMessage>,
) {
    for message in unread {
        // Classify structured messages before committing to transcript.
        let msg_type = classify_team_message(&message.text);

        match msg_type {
            TeamMessageType::PlanApprovalRequest => {
                maybe_auto_approve_plan_request(team_name, &message);
                // Still commit to transcript so the model sees the request.
            }
            TeamMessageType::ShutdownApproved | TeamMessageType::TeammateTerminated => {
                // Internal lifecycle signals  - project_user_teammate_messages
                // filters these out, so committing them would produce an
                // empty user row (bare `❯` with no content). Skip.
                tracing::debug!(
                    msg_type = ?msg_type,
                    from = %message.from,
                    "rebon-cli: skipping internal teammate lifecycle message"
                );
                continue;
            }
            TeamMessageType::IdleNotification | TeamMessageType::Regular => {
                // Pass through to transcript + model prompt.
            }
        }

        let xml = format_teammate_message(
            &message.from,
            &message.text,
            message.summary.as_deref(),
            message.color.as_deref(),
        );
        // Queue the teammate XML so spawn_prompt_turn injects it into
        // the model's next prompt. Without this, the model never sees
        // teammate messages  - they only appear in the visual transcript.
        app.pending_teammate_prompts.push(xml.clone());
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::Commit(rebon_tui::Message::User(rebon_tui::UserMessage {
                uuid: format!(
                    "u-teammate-{}-{}",
                    message.from,
                    rebon_types::wall_clock_ms_u128()
                ),
                timestamp: message.timestamp,
                message: rebon_tui::UserMessageInner {
                    role: rebon_tui::UserRole::User,
                    content: vec![rebon_tui::UserContentBlock::Text(
                        rebon_tui::UserTextBlock { text: xml },
                    )],
                },
                is_compact_summary: None,
                is_meta: None,
                is_visible_in_transcript_only: None,
                image_paste_ids: None,
                plan_content: None,
            })),
        );
    }
}

fn escape_xml_attr(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_team_mailbox_commits_unread_messages_to_transcript() {
        let _guard = crate::test_env::lock_env();
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let team_name = format!("runner-mailbox-{}-{nonce}", std::process::id());
        rebon_tool::write_mailbox_message(
            &team_name,
            TEAM_LEAD_NAME,
            rebon_tool::TeamMailboxMessage {
                from: "alice".into(),
                text: "finished task 1".into(),
                timestamp: "2026-04-11T00:00:00.000Z".into(),
                read: false,
                color: Some("red".into()),
                summary: Some("task complete".into()),
            },
        )
        .unwrap();

        let mut app = AppState::new();
        drain_team_mailbox_for_team(&mut app, &team_name);

        assert_eq!(app.rebon_tui.transcript.len(), 1);
        match &app.rebon_tui.transcript.rows()[0] {
            rebon_tui::Message::User(user) => match &user.message.content[0] {
                rebon_tui::UserContentBlock::Text(text) => {
                    assert!(text.text.contains("<teammate-message"));
                    assert!(text.text.contains("alice"));
                    assert!(text.text.contains("finished task 1"));
                }
                other => panic!("expected text block, got {other:?}"),
            },
            other => panic!("expected user row, got {other:?}"),
        }

        let mailbox = rebon_tool::read_mailbox(&team_name, TEAM_LEAD_NAME).unwrap();
        assert_eq!(mailbox.len(), 1);
        assert!(mailbox[0].read);
        let _ = std::fs::remove_dir_all(rebon_tool::team_files::team_dir(&team_name));
    }

    #[test]
    fn foreground_agent_mailbox_delivery_commits_only_to_the_saved_main_view() {
        let _guard = crate::test_env::lock_env();
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let team_name = format!("runner-foreground-mailbox-{}-{nonce}", std::process::id());
        let session_id = format!("session-{team_name}");
        rebon_tool::write_mailbox_message(
            &team_name,
            TEAM_LEAD_NAME,
            rebon_tool::TeamMailboxMessage {
                from: "alice".into(),
                text: "main-only update".into(),
                timestamp: "2026-07-20T00:00:00.000Z".into(),
                read: false,
                color: None,
                summary: None,
            },
        )
        .unwrap();

        let mut app = AppState::new();
        let registry = rebon_plugin_tasks::runtime::TaskRegistry::new();
        super::super::test_support::insert_local_agent_task(
            &registry,
            "agent-1",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        rebon_plugin_tasks::runtime::register_in_process_teammate_task(
            &registry,
            rebon_plugin_tasks::runtime::InProcessTeammateTaskSpec {
                id: rebon_plugin_tasks::runtime::TaskId::new("tm-main-mailbox"),
                identity: rebon_plugin_tasks::runtime::TeammateIdentity {
                    agent_id: format!("alice@{team_name}"),
                    agent_name: "alice".into(),
                    team_name: team_name.clone(),
                    color: None,
                    plan_mode_required: false,
                    parent_session_id: session_id.clone(),
                },
                prompt: "work".into(),
                model: None,
                model_profile: None,
                permission_mode: "default".into(),
                agent_type: None,
                description: None,
            },
        );
        app.tasks = std::sync::Arc::new(registry);
        let mut active_prompt = None;
        assert!(super::super::live_agent_view::switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-1",
        ));
        let child_rows = app.rebon_tui.transcript.rows().to_vec();

        drain_team_mailbox(&mut app, &session_id);

        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-1"));
        assert_eq!(app.rebon_tui.transcript.rows(), child_rows.as_slice());
        let main = app.main_agent_view.as_ref().expect("saved main view");
        assert_eq!(main.tui.transcript.len(), 1);
        let rebon_tui::Message::User(user) = &main.tui.transcript.rows()[0] else {
            panic!("expected teammate user row in main view");
        };
        let rebon_tui::UserContentBlock::Text(text) = &user.message.content[0] else {
            panic!("expected teammate text block");
        };
        assert!(text.text.contains("main-only update"));
        assert_eq!(app.pending_teammate_prompts.len(), 1);

        let _ = std::fs::remove_dir_all(rebon_tool::team_files::team_dir(&team_name));
    }

    #[test]
    fn empty_team_mailbox_returns_before_entering_main_view_scope() {
        let _guard = crate::test_env::lock_env();
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let team_name = format!("runner-empty-mailbox-{}-{nonce}", std::process::id());

        let mut app = AppState::new();
        app.rebon_tui.overlay.set_streaming_text("foreground child");
        let child_overlay_blocks = app.rebon_tui.overlay.blocks.len();
        app.foregrounded_task_id = Some("agent-1".into());
        app.main_agent_view = None;

        drain_team_mailbox(&mut app, "empty-session");

        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-1"));
        assert!(app.main_agent_view.is_none());
        assert_eq!(app.rebon_tui.overlay.blocks.len(), child_overlay_blocks);
        let _ = std::fs::remove_dir_all(rebon_tool::team_files::team_dir(&team_name));
    }

    #[test]
    fn drain_team_mailbox_covers_registry_teams_without_env_binding() {
        let _guard = crate::test_env::lock_env();
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let team_name = format!("runner-registry-{}-{nonce}", std::process::id());
        rebon_tool::write_mailbox_message(
            &team_name,
            TEAM_LEAD_NAME,
            rebon_tool::TeamMailboxMessage {
                from: "alice".into(),
                text: serde_json::json!({
                    "type": "idle_notification",
                    "from": "alice",
                    "idleReason": "available",
                    "summary": "finished the fix",
                })
                .to_string(),
                timestamp: "2026-07-16T00:00:00.000Z".into(),
                read: false,
                color: None,
                summary: Some("alice idle".into()),
            },
        )
        .unwrap();

        let mut app = AppState::new();
        rebon_plugin_tasks::runtime::register_in_process_teammate_task(
            &app.tasks,
            rebon_plugin_tasks::runtime::InProcessTeammateTaskSpec {
                id: rebon_plugin_tasks::runtime::TaskId::new("tm-alice"),
                identity: rebon_plugin_tasks::runtime::TeammateIdentity {
                    agent_id: format!("alice@{team_name}"),
                    agent_name: "alice".into(),
                    team_name: team_name.clone(),
                    color: None,
                    plan_mode_required: false,
                    parent_session_id: "leader".into(),
                },
                prompt: "work".into(),
                model: None,
                model_profile: None,
                permission_mode: "default".into(),
                agent_type: None,
                description: None,
            },
        );

        drain_team_mailbox(&mut app, "leader");

        assert_eq!(app.rebon_tui.transcript.len(), 1);
        assert_eq!(app.pending_teammate_prompts.len(), 1);
        assert!(app.pending_teammate_prompts[0].contains("idle_notification"));
        let _ = std::fs::remove_dir_all(rebon_tool::team_files::team_dir(&team_name));
    }

    #[test]
    fn drain_team_mailbox_isolates_other_sessions() {
        let _guard = crate::test_env::lock_env();
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let session_a = format!("mailbox-session-a-{}-{nonce}", std::process::id());
        let session_b = format!("mailbox-session-b-{}-{nonce}", std::process::id());
        let team_a = format!("mailbox-team-a-{}-{nonce}", std::process::id());
        let team_b = format!("mailbox-team-b-{}-{nonce}", std::process::id());
        for (team_name, from, text) in [
            (&team_a, "alice", "message for session a"),
            (&team_b, "bob", "message for session b"),
        ] {
            rebon_tool::write_mailbox_message(
                team_name,
                TEAM_LEAD_NAME,
                rebon_tool::TeamMailboxMessage {
                    from: from.into(),
                    text: text.into(),
                    timestamp: "1".into(),
                    read: false,
                    color: None,
                    summary: None,
                },
            )
            .unwrap();
        }

        let mut app = AppState::new();
        for (id, team_name, agent_name, parent_session_id) in [
            ("tm-session-a", &team_a, "alice", &session_a),
            ("tm-session-b", &team_b, "bob", &session_b),
        ] {
            rebon_plugin_tasks::runtime::register_in_process_teammate_task(
                &app.tasks,
                rebon_plugin_tasks::runtime::InProcessTeammateTaskSpec {
                    id: rebon_plugin_tasks::runtime::TaskId::new(id),
                    identity: rebon_plugin_tasks::runtime::TeammateIdentity {
                        agent_id: format!("{agent_name}@{team_name}"),
                        agent_name: agent_name.into(),
                        team_name: team_name.to_string(),
                        color: None,
                        plan_mode_required: false,
                        parent_session_id: parent_session_id.to_string(),
                    },
                    prompt: "work".into(),
                    model: None,
                    model_profile: None,
                    permission_mode: "default".into(),
                    agent_type: None,
                    description: None,
                },
            );
        }

        drain_team_mailbox(&mut app, &session_a);

        assert_eq!(app.pending_teammate_prompts.len(), 1);
        assert!(app.pending_teammate_prompts[0].contains("message for session a"));
        assert!(!app.pending_teammate_prompts[0].contains("message for session b"));
        assert!(rebon_tool::read_mailbox(&team_a, TEAM_LEAD_NAME).unwrap()[0].read);
        assert!(!rebon_tool::read_mailbox(&team_b, TEAM_LEAD_NAME).unwrap()[0].read);

        let _ = std::fs::remove_dir_all(rebon_tool::team_files::team_dir(&team_a));
        let _ = std::fs::remove_dir_all(rebon_tool::team_files::team_dir(&team_b));
    }
}

fn escape_xml_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

//! One task snapshot builder, shared by this plugin's tests and the
//! terminal's.
//!
//! Every surface test needs a `TaskSnapshot` of some kind, and each one
//! that built its own drifted from the runtime's actual shape the moment a
//! field was added. This is that builder, once, behind the `test-support`
//! feature so it never reaches a release binary.

use crate::runtime::{
    BashTaskKind, DreamData, DreamPhase, InProcessTeammateData, LocalAgentData, LocalShellData,
    LocalWorkflowData, MonitorData, MonitorMcpData, MonitorSourceKind, RemoteAgentData,
    RemoteTaskType, TaskData, TaskId, TaskKind, TaskSnapshot, TaskStatus, TeammateIdentity,
};

/// A snapshot of `kind` with defaults for every field a test did not name.
pub fn task_snapshot(id: &str, kind: TaskKind, status: TaskStatus, title: &str) -> TaskSnapshot {
    // Build the per-kind `TaskData` shape from sensible defaults
    // so test callers only have to pass the 4 fields they care
    // about.
    let data = match kind {
        TaskKind::LocalShell => TaskData::LocalShell(LocalShellData {
            command: title.into(),
            exit_code: None,
            interrupted: false,
            display_kind: BashTaskKind::Bash,
            agent_id: None,
        }),
        TaskKind::LocalAgent => TaskData::LocalAgent(LocalAgentData {
            prompt: title.into(),
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
        TaskKind::RemoteAgent => TaskData::RemoteAgent(Box::new(RemoteAgentData {
            remote_task_type: RemoteTaskType::RemoteAgent,
            session_id: format!("sess-{id}"),
            command: title.into(),
            title: title.into(),
            poll_started_at_ms: 0,
            is_remote_review: false,
            is_ultraplan: false,
            is_long_running: false,
            ultraplan_phase: None,
            review_progress: None,
        })),
        TaskKind::InProcessTeammate => {
            TaskData::InProcessTeammate(Box::new(InProcessTeammateData {
                identity: TeammateIdentity {
                    agent_id: format!("{id}@team"),
                    agent_name: id.into(),
                    team_name: "team".into(),
                    color: None,
                    plan_mode_required: false,
                    parent_session_id: "leader".into(),
                },
                prompt: title.into(),
                model: None,
                model_profile: None,
                permission_mode: "default".into(),
                awaiting_plan_approval: false,
                is_idle: false,
                shutdown_requested: false,
                pending_user_messages: Vec::new(),
                tool_use_count: 0,
                token_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
            }))
        }
        TaskKind::LocalWorkflow => TaskData::LocalWorkflow(LocalWorkflowData {
            run_id: "wf_test".into(),
            workflow_name: title.into(),
            summary: None,
            agent_count: 1,
            progress_entries: Vec::new(),
            token_count: 0,
            tool_use_count: 0,
            output_path: None,
            script_path: None,
            args: None,
        }),
        TaskKind::Monitor => TaskData::Monitor(MonitorData {
            description: title.into(),
            source: MonitorSourceKind::Command,
            redacted_target: "command".into(),
            event_count: 0,
            suppressed_count: 0,
            end_reason: None,
        }),
        TaskKind::MonitorMcp => TaskData::MonitorMcp(MonitorMcpData {
            server_name: "mcp".into(),
            description: title.into(),
        }),
        TaskKind::Dream => TaskData::Dream(DreamData {
            phase: DreamPhase::Starting,
            sessions_reviewing: 1,
            files_touched: Vec::new(),
            turns: Vec::new(),
            prior_mtime: 0,
        }),
    };
    TaskSnapshot {
        id: TaskId::new(id),
        kind,
        status,
        title: title.into(),
        last_progress: None,
        error: None,
        result: None,
        is_backgrounded: false,
        notified: false,
        start_time_ms: 0,
        end_time_ms: None,
        metadata: serde_json::json!({}),
        data,
    }
}

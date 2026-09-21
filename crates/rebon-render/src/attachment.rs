//! Attachment row projections: one renderable shape per attachment type,
//! covering file and directory references, memories, skills, hooks, task
//! status, teammate mailbox traffic and MCP resources.

use serde_json::Value;

use crate::diagnostics::{
    project_diagnostics_display, DiagnosticFileDisplay, DiagnosticsProjection,
};
use crate::mailbox::{
    get_shutdown_message_summary, get_task_assignment_summary, ShutdownRejectedMessage,
    ShutdownRequestMessage, TaskAssignmentMessage,
};
use crate::plan_approval::{
    format_teammate_message_content, get_idle_notification_summary, project_plan_approval_request,
    project_plan_approval_response, IdleNotificationMessage, PlanApprovalRenderable,
    PlanApprovalRequestMessage, PlanApprovalResponseMessage,
};
use crate::teammate_messages::{project_teammate_message_content, TeammateMessageContentDisplay};

/// Top-level attachment projector input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentMessageInput {
    /// Whether extra vertical margin surrounds the row.
    pub add_margin: bool,
    /// Verbose render mode.
    pub verbose: bool,
    /// Whether the transcript screen is active.
    pub is_transcript_mode: bool,
    /// Selected-message background, when one is in use.
    pub background: Option<String>,
    /// Platform-specific dot glyph.
    pub dot_glyph: String,
    /// Path separator: `\` on Windows, `/` on POSIX. Appended after a
    /// directory listing line.
    pub path_separator: String,
    /// Attachment payload.
    pub attachment: AttachmentInput,
}

/// Supported attachment payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentInput {
    /// `directory`.
    Directory {
        /// Precomputed display path.
        display_path: String,
    },
    /// `file` / `already_read_file`.
    File(AttachmentFileDisplay),
    /// `compact_file_reference`.
    CompactFileReference {
        /// Display path.
        display_path: String,
    },
    /// `pdf_reference`.
    PdfReference {
        /// Display path.
        display_path: String,
        /// Page count.
        page_count: usize,
    },
    /// `selected_lines_in_ide`.
    SelectedLinesInIde {
        /// Display path.
        display_path: String,
        /// Start line of the selection.
        line_start: usize,
        /// End line.
        line_end: usize,
        /// IDE display name.
        ide_name: String,
    },
    /// `nested_memory`.
    NestedMemory {
        /// Display path.
        display_path: String,
    },
    /// `relevant_memories`.
    RelevantMemories {
        /// Memory entries.
        memories: Vec<AttachmentRelevantMemoryEntry>,
    },
    /// `dynamic_skill`.
    DynamicSkill {
        /// Display path.
        display_path: String,
        /// Loaded skill names.
        skill_names: Vec<String>,
    },
    /// `skill_listing`.
    SkillListing {
        /// Skill count.
        skill_count: usize,
        /// Whether this is the initial attachment.
        is_initial: bool,
    },
    /// `skill_discovery`.
    SkillDiscovery {
        /// Whether the feature gate is enabled.
        feature_enabled: bool,
        /// Whether internal-only hint copy should render.
        internal_build: bool,
        /// Demo env disables the hint.
        is_demo_env: bool,
        /// Discovered skills.
        skills: Vec<AttachmentSkillDiscoveryEntry>,
    },
    /// `agent_listing_delta`.
    AgentListingDelta {
        /// Whether this is the initial attachment.
        is_initial: bool,
        /// Added agent types.
        added_types: Vec<String>,
    },
    /// `queued_command`.
    QueuedCommand {
        /// Extracted prompt text.
        prompt_text: String,
        /// Optional pasted image ids.
        image_paste_ids: Vec<usize>,
    },
    /// `plan_file_reference`.
    PlanFileReference {
        /// Preformatted plan file path.
        display_path: String,
    },
    /// `invoked_skills`.
    InvokedSkills {
        /// Restored skill names.
        skill_names: Vec<String>,
    },
    /// `diagnostics`.
    Diagnostics {
        /// Diagnostic files.
        files: Vec<DiagnosticFileDisplay>,
        /// CWD for path shortening.
        cwd: String,
    },
    /// `mcp_resource`.
    McpResource {
        /// Resource name.
        name: String,
        /// Server name.
        server: String,
    },
    /// `command_permissions`.
    CommandPermissions,
    /// `async_hook_response`.
    AsyncHookResponse {
        /// Hook event.
        hook_event: String,
    },
    /// `hook_blocking_error`.
    HookBlockingError {
        /// Hook event.
        hook_event: String,
        /// Hook name.
        hook_name: String,
        /// stderr text.
        stderr: String,
    },
    /// `hook_non_blocking_error`.
    HookNonBlockingError {
        /// Hook event.
        hook_event: String,
        /// Hook name.
        hook_name: String,
    },
    /// `hook_error_during_execution`.
    HookErrorDuringExecution {
        /// Hook event.
        hook_event: String,
        /// Hook name.
        hook_name: String,
    },
    /// `hook_success`.
    HookSuccess,
    /// `hook_stopped_continuation`.
    HookStoppedContinuation {
        /// Hook event.
        hook_event: String,
        /// Hook name.
        hook_name: String,
        /// Message text.
        message: String,
    },
    /// `hook_system_message`.
    HookSystemMessage {
        /// Hook name.
        hook_name: String,
        /// Content.
        content: String,
    },
    /// `hook_permission_decision`.
    HookPermissionDecision {
        /// `allow` / `deny`.
        decision: String,
        /// Hook event.
        hook_event: String,
    },
    /// `task_status`.
    TaskStatus {
        /// Task type.
        task_type: String,
        /// Status.
        status: String,
        /// Description.
        description: String,
        /// Optional teammate identity for `in_process_teammate`.
        teammate: Option<(String, Option<String>)>,
        /// Whether swarm mode is enabled.
        agent_swarms_enabled: bool,
    },
    /// `teammate_shutdown_batch`.
    TeammateShutdownBatch {
        /// Count.
        count: usize,
    },
    /// `teammate_mailbox`.
    TeammateMailbox {
        /// Whether swarm mode is enabled.
        agent_swarms_enabled: bool,
        /// Messages in the mailbox.
        messages: Vec<AttachmentMailboxMessage>,
    },
    /// Anything intentionally hidden from rendering.
    Hidden,
}

/// File display input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentFileDisplay {
    /// Display path.
    pub display_path: String,
    /// File render kind.
    pub kind: AttachmentFileKind,
}

/// File kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentFileKind {
    /// Notebook file.
    Notebook {
        /// Cell count.
        cell_count: usize,
    },
    /// Unchanged file.
    Unchanged,
    /// Text file.
    Text {
        /// Number of lines.
        num_lines: usize,
        /// Whether the count was truncated.
        truncated: bool,
    },
    /// Binary/other file.
    Binary {
        /// Preformatted size string.
        formatted_size: String,
    },
}

/// Relevant memory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRelevantMemoryEntry {
    /// File path.
    pub path: String,
    /// Content text.
    pub content: String,
}

/// Skill discovery entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSkillDiscoveryEntry {
    /// Skill name.
    pub name: String,
    /// Optional short id.
    pub short_id: Option<String>,
}

/// Teammate mailbox message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentMailboxMessage {
    /// Sender name.
    pub from: String,
    /// Raw content.
    pub text: String,
    /// Optional color.
    pub color: Option<String>,
    /// Optional summary.
    pub summary: Option<String>,
}

/// Attachment projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentProjection {
    /// Nothing should render.
    Hidden,
    /// One or more generic line rows.
    Lines(Vec<AttachmentLineDisplay>),
    /// Relevant memories block.
    RelevantMemories(AttachmentRelevantMemoriesDisplay),
    /// Queued command block.
    QueuedCommand(QueuedCommandDisplay),
    /// Diagnostics block.
    Diagnostics(DiagnosticsProjection),
    /// Task status block.
    TaskStatus(AttachmentTaskStatusDisplay),
    /// Teammate mailbox block.
    TeammateMailbox(AttachmentTeammateMailboxDisplay),
}

/// One generic line row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentLineDisplay {
    /// Text.
    pub text: String,
    /// Optional tone.
    pub tone: Option<AttachmentTone>,
    /// Whether the line is dimmed.
    pub dim: bool,
    /// Selected-message background.
    pub background: Option<String>,
}

/// Line color tone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentTone {
    /// Error tone.
    Error,
    /// Warning tone.
    Warning,
}

/// Relevant memories display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRelevantMemoriesDisplay {
    /// Margin top.
    pub margin_top: u8,
    /// Background.
    pub background: Option<String>,
    /// Memory count.
    pub count: usize,
    /// Word for the count (`memory` / `memories`).
    pub count_word: &'static str,
    /// Show the expand hint.
    pub show_expand_hint: bool,
    /// Whether detailed entries should render.
    pub show_entries: bool,
    /// Left gutter width, in cells.
    pub gutter_width: u8,
    /// Left padding for the transcript-mode body block, in cells.
    pub transcript_padding_left: u8,
    /// Detailed entries.
    pub entries: Vec<AttachmentRelevantMemoryRow>,
}

/// One relevant-memory entry projected for the render layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRelevantMemoryRow {
    /// Full path, used as the click target.
    pub path: String,
    /// Display basename.
    pub basename: String,
    /// Optional ANSI body shown in transcript mode.
    pub transcript_body: Option<String>,
}

/// Queued command display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedCommandDisplay {
    /// Prompt text.
    pub prompt_text: String,
    /// Image ids.
    pub image_paste_ids: Vec<usize>,
    /// Margin top.
    pub margin_top: u8,
}

/// Task status display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentTaskStatusDisplay {
    /// Generic task line.
    Generic {
        /// Background.
        background: Option<String>,
        /// Dot glyph.
        dot_glyph: String,
        /// Description.
        description: String,
        /// Status copy.
        status_text: String,
    },
    /// In-process teammate line.
    Teammate {
        /// Background.
        background: Option<String>,
        /// Dot glyph.
        dot_glyph: String,
        /// Agent name.
        agent_name: String,
        /// Optional agent color.
        agent_color: Option<String>,
        /// Status copy.
        status_text: String,
    },
}

/// Teammate mailbox display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentTeammateMailboxDisplay {
    /// Visible items.
    pub items: Vec<AttachmentTeammateMailboxItemDisplay>,
}

/// One teammate mailbox item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentTeammateMailboxItemDisplay {
    /// Special task-assignment row.
    TaskAssignment {
        /// Dot glyph.
        dot_glyph: String,
        /// Task id.
        task_id: String,
        /// Subject.
        subject: Option<String>,
        /// Sender.
        from: String,
    },
    /// Plan approval request/response.
    PlanApproval(PlanApprovalRenderable),
    /// Plain teammate content row.
    Plain(TeammateMessageContentDisplay),
}

/// Projects one attachment payload: generic line rows for most types, and
/// dedicated displays for relevant memories, queued commands, diagnostics,
/// task status and teammate mailbox traffic.
pub fn project_attachment_message(input: &AttachmentMessageInput) -> AttachmentProjection {
    match &input.attachment {
        AttachmentInput::Directory { display_path } => AttachmentProjection::Lines(vec![line(
            format!("Listed directory {display_path}{}", input.path_separator),
            None,
            true,
            input.background.clone(),
        )]),
        AttachmentInput::File(file) => AttachmentProjection::Lines(vec![line(
            match &file.kind {
                AttachmentFileKind::Notebook { cell_count } => {
                    format!("Read {} ({} cells)", file.display_path, cell_count)
                }
                AttachmentFileKind::Unchanged => {
                    format!("Read {} (unchanged)", file.display_path)
                }
                AttachmentFileKind::Text {
                    num_lines,
                    truncated,
                } => format!(
                    "Read {} ({}{} lines)",
                    file.display_path,
                    num_lines,
                    if *truncated { "+" } else { "" }
                ),
                AttachmentFileKind::Binary { formatted_size } => {
                    format!("Read {} ({formatted_size})", file.display_path)
                }
            },
            None,
            true,
            input.background.clone(),
        )]),
        AttachmentInput::CompactFileReference { display_path } => {
            AttachmentProjection::Lines(vec![line(
                format!("Referenced file {display_path}"),
                None,
                true,
                input.background.clone(),
            )])
        }
        AttachmentInput::PdfReference {
            display_path,
            page_count,
        } => AttachmentProjection::Lines(vec![line(
            format!("Referenced PDF {display_path} ({page_count} pages)"),
            None,
            true,
            input.background.clone(),
        )]),
        AttachmentInput::SelectedLinesInIde {
            display_path,
            line_start,
            line_end,
            ide_name,
        } => AttachmentProjection::Lines(vec![line(
            format!(
                "\u{29C9} Selected {} lines from {} in {}",
                line_end - line_start + 1,
                display_path,
                ide_name
            ),
            None,
            true,
            input.background.clone(),
        )]),
        AttachmentInput::NestedMemory { display_path } => AttachmentProjection::Lines(vec![line(
            format!("Loaded {display_path}"),
            None,
            true,
            input.background.clone(),
        )]),
        AttachmentInput::RelevantMemories { memories } => {
            AttachmentProjection::RelevantMemories(AttachmentRelevantMemoriesDisplay {
                margin_top: u8::from(input.add_margin),
                background: input.background.clone(),
                count: memories.len(),
                count_word: if memories.len() == 1 {
                    "memory"
                } else {
                    "memories"
                },
                show_expand_hint: !input.is_transcript_mode,
                show_entries: input.verbose || input.is_transcript_mode,
                gutter_width: 2,
                transcript_padding_left: 5,
                entries: memories
                    .iter()
                    .map(|memory| AttachmentRelevantMemoryRow {
                        path: memory.path.clone(),
                        basename: file_name_from_path(&memory.path),
                        transcript_body: input.is_transcript_mode.then(|| memory.content.clone()),
                    })
                    .collect(),
            })
        }
        AttachmentInput::DynamicSkill {
            display_path,
            skill_names,
        } => AttachmentProjection::Lines(vec![line(
            format!(
                "Loaded {} {} from {}",
                skill_names.len(),
                plural(skill_names.len(), "skill", "skills"),
                display_path
            ),
            None,
            true,
            input.background.clone(),
        )]),
        AttachmentInput::SkillListing {
            skill_count,
            is_initial,
        } => project_skill_listing(input, skill_count, is_initial),
        AttachmentInput::SkillDiscovery {
            feature_enabled,
            internal_build,
            is_demo_env,
            skills,
        } => project_skill_discovery(input, feature_enabled, internal_build, is_demo_env, skills),
        AttachmentInput::AgentListingDelta {
            is_initial,
            added_types,
        } => project_agent_listing_delta(input, is_initial, added_types),
        AttachmentInput::QueuedCommand {
            prompt_text,
            image_paste_ids,
        } => AttachmentProjection::QueuedCommand(QueuedCommandDisplay {
            prompt_text: prompt_text.clone(),
            image_paste_ids: image_paste_ids.clone(),
            margin_top: u8::from(input.add_margin),
        }),
        AttachmentInput::PlanFileReference { display_path } => {
            AttachmentProjection::Lines(vec![line(
                format!("Plan file referenced ({display_path})"),
                None,
                true,
                input.background.clone(),
            )])
        }
        AttachmentInput::InvokedSkills { skill_names } => {
            if skill_names.is_empty() {
                AttachmentProjection::Hidden
            } else {
                AttachmentProjection::Lines(vec![line(
                    format!("Skills restored ({})", skill_names.join(", ")),
                    None,
                    true,
                    input.background.clone(),
                )])
            }
        }
        AttachmentInput::Diagnostics { files, cwd } => {
            project_diagnostics_display(files, input.verbose, cwd)
                .map(AttachmentProjection::Diagnostics)
                .unwrap_or(AttachmentProjection::Hidden)
        }
        AttachmentInput::McpResource { name, server } => AttachmentProjection::Lines(vec![line(
            format!("Read MCP resource {name} from {server}"),
            None,
            true,
            input.background.clone(),
        )]),
        AttachmentInput::CommandPermissions
        | AttachmentInput::HookSuccess
        | AttachmentInput::Hidden => AttachmentProjection::Hidden,
        AttachmentInput::AsyncHookResponse { hook_event } => {
            project_async_hook_response(input, hook_event)
        }
        AttachmentInput::HookBlockingError {
            hook_event,
            hook_name,
            stderr,
        } => project_hook_blocking_error(input, hook_event, hook_name, stderr),
        AttachmentInput::HookNonBlockingError {
            hook_event,
            hook_name,
        } => project_hook_non_blocking_error(input, hook_event, hook_name),
        AttachmentInput::HookErrorDuringExecution {
            hook_event,
            hook_name,
        } => project_hook_error_during_execution(input, hook_event, hook_name),
        AttachmentInput::HookStoppedContinuation {
            hook_event,
            hook_name,
            message,
        } => project_hook_stopped_continuation(input, hook_event, hook_name, message),
        AttachmentInput::HookSystemMessage { hook_name, content } => {
            project_hook_system_message(input, hook_name, content)
        }
        AttachmentInput::HookPermissionDecision {
            decision,
            hook_event,
        } => project_hook_permission_decision(input, decision, hook_event),
        AttachmentInput::TaskStatus {
            task_type,
            status,
            description,
            teammate,
            agent_swarms_enabled,
        } => AttachmentProjection::TaskStatus(project_task_status(
            task_type,
            status,
            description,
            teammate.as_ref(),
            *agent_swarms_enabled,
            input.background.clone(),
            input.dot_glyph.clone(),
        )),
        AttachmentInput::TeammateShutdownBatch { count } => {
            AttachmentProjection::Lines(vec![AttachmentLineDisplay {
                text: format!(
                    "{} {} shut down gracefully",
                    count,
                    plural(*count, "teammate", "teammates")
                ),
                tone: None,
                dim: true,
                background: input.background.clone(),
            }])
        }
        AttachmentInput::TeammateMailbox {
            agent_swarms_enabled,
            messages,
        } => {
            if !*agent_swarms_enabled {
                return AttachmentProjection::Hidden;
            }
            let items = messages
                .iter()
                .filter(|message| !is_shutdown_approved_json(&message.text))
                .filter(|message| !is_hidden_mailbox_json(&message.text))
                .filter_map(|message| {
                    project_mailbox_item(message, input.is_transcript_mode, &input.dot_glyph)
                })
                .collect::<Vec<_>>();
            if items.is_empty() {
                AttachmentProjection::Hidden
            } else {
                AttachmentProjection::TeammateMailbox(AttachmentTeammateMailboxDisplay { items })
            }
        }
    }
}

fn project_task_status(
    task_type: &str,
    status: &str,
    description: &str,
    teammate: Option<&(String, Option<String>)>,
    agent_swarms_enabled: bool,
    background: Option<String>,
    dot_glyph: String,
) -> AttachmentTaskStatusDisplay {
    if agent_swarms_enabled && task_type == "in_process_teammate" {
        if let Some((agent_name, agent_color)) = teammate {
            return AttachmentTaskStatusDisplay::Teammate {
                background,
                dot_glyph,
                agent_name: agent_name.clone(),
                agent_color: agent_color.clone(),
                status_text: if status == "completed" {
                    "shut down gracefully".into()
                } else {
                    status.to_string()
                },
            };
        }
    }

    AttachmentTaskStatusDisplay::Generic {
        background,
        dot_glyph,
        description: description.to_string(),
        status_text: match status {
            "completed" => "completed in background".into(),
            "killed" => "stopped".into(),
            "running" => "still running in background".into(),
            _ => status.to_string(),
        },
    }
}

fn project_mailbox_item(
    message: &AttachmentMailboxMessage,
    is_transcript_mode: bool,
    dot_glyph: &str,
) -> Option<AttachmentTeammateMailboxItemDisplay> {
    let parsed = serde_json::from_str::<Value>(&message.text).ok();

    if let Some(parsed) = &parsed {
        if parsed.get("type").and_then(Value::as_str) == Some("task_assignment") {
            return Some(AttachmentTeammateMailboxItemDisplay::TaskAssignment {
                dot_glyph: dot_glyph.to_string(),
                task_id: parsed
                    .get("taskId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                subject: parsed
                    .get("subject")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                from: parsed
                    .get("assignedBy")
                    .and_then(Value::as_str)
                    .unwrap_or(message.from.as_str())
                    .to_string(),
            });
        }
    }

    if let Some(renderable) = maybe_plan_approval_renderable(&message.text, &message.from) {
        return Some(AttachmentTeammateMailboxItemDisplay::PlanApproval(
            renderable,
        ));
    }

    let content = format_structured_teammate_text(&message.text);
    Some(AttachmentTeammateMailboxItemDisplay::Plain(
        project_teammate_message_content(
            &message.from,
            message.color.as_deref(),
            &content,
            message.summary.as_deref(),
            is_transcript_mode,
        ),
    ))
}

fn maybe_plan_approval_renderable(
    content: &str,
    sender_name: &str,
) -> Option<PlanApprovalRenderable> {
    let parsed = serde_json::from_str::<Value>(content).ok()?;
    match parsed.get("type")?.as_str()? {
        "plan_approval_request" => Some(PlanApprovalRenderable::Request(
            project_plan_approval_request(&PlanApprovalRequestMessage {
                from: parsed.get("from")?.as_str()?.to_string(),
                plan_content: parsed
                    .get("planContent")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                plan_file_path: parsed
                    .get("planFilePath")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
        )),
        "plan_approval_response" => Some(PlanApprovalRenderable::Response(
            project_plan_approval_response(
                &PlanApprovalResponseMessage {
                    approved: parsed
                        .get("approved")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    feedback: parsed
                        .get("feedback")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                },
                sender_name,
            ),
        )),
        _ => None,
    }
}

fn format_structured_teammate_text(content: &str) -> String {
    let parsed = serde_json::from_str::<Value>(content).ok();
    let shutdown_summary = parsed.as_ref().and_then(parse_shutdown_summary);
    let idle_summary = parsed.as_ref().and_then(parse_idle_summary);
    let task_assignment_summary = parsed.as_ref().and_then(parse_task_assignment_summary);
    let terminated_message = parsed.as_ref().and_then(|parsed| {
        (parsed.get("type").and_then(Value::as_str) == Some("teammate_terminated"))
            .then(|| parsed.get("message").and_then(Value::as_str))
            .flatten()
    });
    format_teammate_message_content(
        content,
        None,
        None,
        shutdown_summary,
        idle_summary,
        task_assignment_summary,
        terminated_message,
    )
}

fn parse_shutdown_summary(parsed: &Value) -> Option<String> {
    match parsed.get("type")?.as_str()? {
        "shutdown_request" => get_shutdown_message_summary(
            Some(&ShutdownRequestMessage {
                from: parsed.get("from")?.as_str()?.to_string(),
                reason: parsed
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
            }),
            None,
            None,
        ),
        "shutdown_rejected" => get_shutdown_message_summary(
            None,
            None,
            Some(&ShutdownRejectedMessage {
                from: parsed.get("from")?.as_str()?.to_string(),
                reason: parsed
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
        ),
        "shutdown_approved" => {
            get_shutdown_message_summary(None, parsed.get("from").and_then(Value::as_str), None)
        }
        _ => None,
    }
}

fn parse_idle_summary(parsed: &Value) -> Option<String> {
    if parsed.get("type").and_then(Value::as_str) != Some("idle_notification") {
        return None;
    }
    Some(get_idle_notification_summary(&IdleNotificationMessage {
        completed_task_id: parsed
            .get("completedTaskId")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        completed_status: parsed
            .get("completedStatus")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        summary: parsed
            .get("summary")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    }))
}

fn parse_task_assignment_summary(parsed: &Value) -> Option<String> {
    if parsed.get("type").and_then(Value::as_str) != Some("task_assignment") {
        return None;
    }
    Some(get_task_assignment_summary(&TaskAssignmentMessage {
        task_id: parsed.get("taskId")?.as_str()?.to_string(),
        assigned_by: parsed.get("assignedBy")?.as_str()?.to_string(),
        subject: parsed.get("subject")?.as_str()?.to_string(),
        description: parsed
            .get("description")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    }))
}

fn is_hidden_mailbox_json(content: &str) -> bool {
    let parsed = serde_json::from_str::<Value>(content).ok();
    matches!(
        parsed
            .as_ref()
            .and_then(|parsed| parsed.get("type"))
            .and_then(Value::as_str),
        Some("idle_notification" | "teammate_terminated")
    )
}

fn is_shutdown_approved_json(content: &str) -> bool {
    let parsed = serde_json::from_str::<Value>(content).ok();
    parsed
        .as_ref()
        .and_then(|parsed| parsed.get("type"))
        .and_then(Value::as_str)
        == Some("shutdown_approved")
}

fn is_stop_hook(hook_event: &str) -> bool {
    matches!(hook_event, "Stop" | "SubagentStop")
}

fn line(
    text: String,
    tone: Option<AttachmentTone>,
    dim: bool,
    background: Option<String>,
) -> AttachmentLineDisplay {
    AttachmentLineDisplay {
        text,
        tone,
        dim,
        background,
    }
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 {
        singular
    } else {
        plural
    }
}

fn file_name_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{DiagnosticEntry, DiagnosticSeverity};

    fn input(attachment: AttachmentInput) -> AttachmentMessageInput {
        AttachmentMessageInput {
            add_margin: true,
            verbose: false,
            is_transcript_mode: false,
            background: Some("messageActionsBackground".into()),
            dot_glyph: "\u{25cf}".into(),
            path_separator: "/".into(),
            attachment,
        }
    }

    #[test]
    fn file_and_reference_lines_render_as_expected() {
        let projection =
            project_attachment_message(&input(AttachmentInput::File(AttachmentFileDisplay {
                display_path: "src/lib.rs".into(),
                kind: AttachmentFileKind::Text {
                    num_lines: 42,
                    truncated: true,
                },
            })));
        assert_eq!(
            projection,
            AttachmentProjection::Lines(vec![AttachmentLineDisplay {
                text: "Read src/lib.rs (42+ lines)".into(),
                tone: None,
                dim: true,
                background: Some("messageActionsBackground".into()),
            }])
        );

        let projection = project_attachment_message(&input(AttachmentInput::PdfReference {
            display_path: "doc.pdf".into(),
            page_count: 3,
        }));
        assert_eq!(
            projection,
            AttachmentProjection::Lines(vec![AttachmentLineDisplay {
                text: "Referenced PDF doc.pdf (3 pages)".into(),
                tone: None,
                dim: true,
                background: Some("messageActionsBackground".into()),
            }])
        );

        assert_eq!(
            project_attachment_message(&input(AttachmentInput::Directory {
                display_path: "src".into(),
            })),
            AttachmentProjection::Lines(vec![AttachmentLineDisplay {
                text: "Listed directory src/".into(),
                tone: None,
                dim: true,
                background: Some("messageActionsBackground".into()),
            }])
        );

        let mut windows_input = input(AttachmentInput::Directory {
            display_path: "C:\\repo\\src".into(),
        });
        windows_input.path_separator = "\\".into();
        let AttachmentProjection::Lines(lines) = project_attachment_message(&windows_input) else {
            panic!("expected lines");
        };
        assert_eq!(lines[0].text, "Listed directory C:\\repo\\src\\");
        assert_eq!(
            project_attachment_message(&input(AttachmentInput::CompactFileReference {
                display_path: "README.md".into(),
            })),
            AttachmentProjection::Lines(vec![AttachmentLineDisplay {
                text: "Referenced file README.md".into(),
                tone: None,
                dim: true,
                background: Some("messageActionsBackground".into()),
            }])
        );
        assert_eq!(
            project_attachment_message(&input(AttachmentInput::SelectedLinesInIde {
                display_path: "src/lib.rs".into(),
                line_start: 10,
                line_end: 14,
                ide_name: "VS Code".into(),
            })),
            AttachmentProjection::Lines(vec![AttachmentLineDisplay {
                text: "\u{29C9} Selected 5 lines from src/lib.rs in VS Code".into(),
                tone: None,
                dim: true,
                background: Some("messageActionsBackground".into()),
            }])
        );
        assert_eq!(
            project_attachment_message(&input(AttachmentInput::NestedMemory {
                display_path: ".rebon/memory.md".into(),
            })),
            AttachmentProjection::Lines(vec![AttachmentLineDisplay {
                text: "Loaded .rebon/memory.md".into(),
                tone: None,
                dim: true,
                background: Some("messageActionsBackground".into()),
            }])
        );
    }

    #[test]
    fn relevant_memories_gate_entries_and_content_by_mode() {
        let projection = project_attachment_message(&input(AttachmentInput::RelevantMemories {
            memories: vec![AttachmentRelevantMemoryEntry {
                path: "/tmp/memory.md".into(),
                content: "body".into(),
            }],
        }));
        let AttachmentProjection::RelevantMemories(display) = projection else {
            panic!("expected relevant memories");
        };
        assert_eq!(display.margin_top, 1);
        assert_eq!(display.gutter_width, 2);
        assert_eq!(display.transcript_padding_left, 5);
        assert_eq!(display.count_word, "memory");
        assert!(display.show_expand_hint);
        assert!(!display.show_entries);

        let mut transcript = input(AttachmentInput::RelevantMemories {
            memories: vec![
                AttachmentRelevantMemoryEntry {
                    path: "/tmp/memory.md".into(),
                    content: "body".into(),
                },
                AttachmentRelevantMemoryEntry {
                    path: "/tmp/other.md".into(),
                    content: "more".into(),
                },
            ],
        });
        transcript.is_transcript_mode = true;
        let AttachmentProjection::RelevantMemories(display) =
            project_attachment_message(&transcript)
        else {
            panic!("expected relevant memories");
        };
        assert!(!display.show_expand_hint);
        assert!(display.show_entries);
        assert_eq!(display.count_word, "memories");
        assert_eq!(
            display.entries,
            vec![
                AttachmentRelevantMemoryRow {
                    path: "/tmp/memory.md".into(),
                    basename: "memory.md".into(),
                    transcript_body: Some("body".into()),
                },
                AttachmentRelevantMemoryRow {
                    path: "/tmp/other.md".into(),
                    basename: "other.md".into(),
                    transcript_body: Some("more".into()),
                },
            ]
        );
    }

    #[test]
    fn skill_discovery_and_skill_listing_follow_feature_and_initial_gates() {
        assert_eq!(
            project_attachment_message(&input(AttachmentInput::SkillDiscovery {
                feature_enabled: false,
                internal_build: true,
                is_demo_env: false,
                skills: vec![AttachmentSkillDiscoveryEntry {
                    name: "verify".into(),
                    short_id: Some("abc".into()),
                }],
            })),
            AttachmentProjection::Hidden
        );

        let projection = project_attachment_message(&input(AttachmentInput::SkillDiscovery {
            feature_enabled: true,
            internal_build: true,
            is_demo_env: false,
            skills: vec![AttachmentSkillDiscoveryEntry {
                name: "verify".into(),
                short_id: Some("abc".into()),
            }],
        }));
        let AttachmentProjection::Lines(lines) = projection else {
            panic!("expected lines");
        };
        assert!(lines[0].text.contains("1 relevant skill: verify [abc]"));
        assert!(lines[0].text.contains(" \u{00B7} /skill-feedback abc"));
        assert!(!lines[0].text.contains('\u{8DEF}'));

        assert_eq!(
            project_attachment_message(&input(AttachmentInput::SkillListing {
                skill_count: 3,
                is_initial: true,
            })),
            AttachmentProjection::Hidden
        );
    }

    #[test]
    fn queued_command_and_invoked_skills_project_cleanly() {
        let projection = project_attachment_message(&input(AttachmentInput::QueuedCommand {
            prompt_text: "hello".into(),
            image_paste_ids: vec![1, 2],
        }));
        assert_eq!(
            projection,
            AttachmentProjection::QueuedCommand(QueuedCommandDisplay {
                prompt_text: "hello".into(),
                image_paste_ids: vec![1, 2],
                margin_top: 1,
            })
        );

        let projection = project_attachment_message(&input(AttachmentInput::InvokedSkills {
            skill_names: vec!["verify".into(), "review".into()],
        }));
        let AttachmentProjection::Lines(lines) = projection else {
            panic!("expected lines");
        };
        assert_eq!(lines[0].text, "Skills restored (verify, review)");

        let projection = project_attachment_message(&input(AttachmentInput::PlanFileReference {
            display_path: "plans/current.md".into(),
        }));
        let AttachmentProjection::Lines(lines) = projection else {
            panic!("expected lines");
        };
        assert_eq!(lines[0].text, "Plan file referenced (plans/current.md)");

        let projection = project_attachment_message(&input(AttachmentInput::McpResource {
            name: "schema".into(),
            server: "github".into(),
        }));
        let AttachmentProjection::Lines(lines) = projection else {
            panic!("expected lines");
        };
        assert_eq!(lines[0].text, "Read MCP resource schema from github");
    }

    #[test]
    fn diagnostics_and_async_hook_rules_follow_gates() {
        let projection = project_attachment_message(&input(AttachmentInput::Diagnostics {
            files: vec![DiagnosticFileDisplay {
                uri: "file:///repo/a.ts".into(),
                diagnostics: vec![DiagnosticEntry {
                    severity: DiagnosticSeverity::Error,
                    line: 0,
                    character: 0,
                    message: "bad".into(),
                    code: None,
                    source: None,
                }],
            }],
            cwd: "/repo".into(),
        }));
        assert!(matches!(projection, AttachmentProjection::Diagnostics(_)));

        assert_eq!(
            project_attachment_message(&input(AttachmentInput::AsyncHookResponse {
                hook_event: "SessionStart".into(),
            })),
            AttachmentProjection::Hidden
        );

        let mut verbose_input = input(AttachmentInput::AsyncHookResponse {
            hook_event: "SessionStart".into(),
        });
        verbose_input.verbose = true;
        let AttachmentProjection::Lines(lines) = project_attachment_message(&verbose_input) else {
            panic!("expected lines");
        };
        assert_eq!(lines[0].text, "Async hook SessionStart completed");
    }

    #[test]
    fn hook_error_branches_hide_stop_hooks_and_surface_messages() {
        assert_eq!(
            project_attachment_message(&input(AttachmentInput::HookBlockingError {
                hook_event: "Stop".into(),
                hook_name: "lint".into(),
                stderr: "boom".into(),
            })),
            AttachmentProjection::Hidden
        );

        let projection = project_attachment_message(&input(AttachmentInput::HookBlockingError {
            hook_event: "PreToolUse".into(),
            hook_name: "lint".into(),
            stderr: "boom".into(),
        }));
        let AttachmentProjection::Lines(lines) = projection else {
            panic!("expected lines");
        };
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].tone, Some(AttachmentTone::Error));
        assert_eq!(lines[1].text, "boom");
    }

    #[test]
    fn task_status_switches_between_generic_and_teammate_modes() {
        let projection = project_attachment_message(&input(AttachmentInput::TaskStatus {
            task_type: "local_agent".into(),
            status: "running".into(),
            description: "index repo".into(),
            teammate: None,
            agent_swarms_enabled: false,
        }));
        assert_eq!(
            projection,
            AttachmentProjection::TaskStatus(AttachmentTaskStatusDisplay::Generic {
                background: Some("messageActionsBackground".into()),
                dot_glyph: "\u{25cf}".into(),
                description: "index repo".into(),
                status_text: "still running in background".into(),
            })
        );

        let projection = project_attachment_message(&input(AttachmentInput::TaskStatus {
            task_type: "in_process_teammate".into(),
            status: "completed".into(),
            description: "ignored".into(),
            teammate: Some(("alice".into(), Some("red".into()))),
            agent_swarms_enabled: true,
        }));
        assert_eq!(
            projection,
            AttachmentProjection::TaskStatus(AttachmentTaskStatusDisplay::Teammate {
                background: Some("messageActionsBackground".into()),
                dot_glyph: "\u{25cf}".into(),
                agent_name: "alice".into(),
                agent_color: Some("red".into()),
                status_text: "shut down gracefully".into(),
            })
        );
    }

    #[test]
    fn teammate_mailbox_filters_hidden_messages_and_routes_structured_items() {
        let projection = project_attachment_message(&input(AttachmentInput::TeammateMailbox {
            agent_swarms_enabled: true,
            messages: vec![
                AttachmentMailboxMessage {
                    from: "alice".into(),
                    text: "{\"type\":\"shutdown_approved\",\"from\":\"alice\"}".into(),
                    color: None,
                    summary: None,
                },
                AttachmentMailboxMessage {
                    from: "alice".into(),
                    text: "{\"type\":\"task_assignment\",\"taskId\":\"7\",\"subject\":\"Fix bug\",\"assignedBy\":\"bob\"}".into(),
                    color: None,
                    summary: None,
                },
                AttachmentMailboxMessage {
                    from: "alice".into(),
                    text: "{\"type\":\"plan_approval_request\",\"from\":\"alice\",\"planContent\":\"x\",\"planFilePath\":\"/tmp/p\"}".into(),
                    color: None,
                    summary: None,
                },
                AttachmentMailboxMessage {
                    from: "alice".into(),
                    text: "{\"type\":\"shutdown_rejected\",\"from\":\"alice\",\"reason\":\"busy\"}".into(),
                    color: Some("green".into()),
                    summary: Some("s".into()),
                },
            ],
        }));

        let AttachmentProjection::TeammateMailbox(display) = projection else {
            panic!("expected mailbox");
        };
        assert_eq!(display.items.len(), 3);
        assert!(matches!(
            display.items[0],
            AttachmentTeammateMailboxItemDisplay::TaskAssignment { .. }
        ));
        assert!(matches!(
            display.items[1],
            AttachmentTeammateMailboxItemDisplay::PlanApproval(_)
        ));
        let AttachmentTeammateMailboxItemDisplay::Plain(plain) = &display.items[2] else {
            panic!("expected plain");
        };
        assert_eq!(plain.content, "[Shutdown Rejected] alice: busy");
    }

    #[test]
    fn teammate_mailbox_hidden_when_swarms_disabled_or_messages_filtered_out() {
        assert_eq!(
            project_attachment_message(&input(AttachmentInput::TeammateMailbox {
                agent_swarms_enabled: false,
                messages: vec![],
            })),
            AttachmentProjection::Hidden
        );

        assert_eq!(
            project_attachment_message(&input(AttachmentInput::TeammateMailbox {
                agent_swarms_enabled: true,
                messages: vec![AttachmentMailboxMessage {
                    from: "alice".into(),
                    text: "{\"type\":\"idle_notification\"}".into(),
                    color: None,
                    summary: None,
                }],
            })),
            AttachmentProjection::Hidden
        );
    }

    #[test]
    fn dynamic_skill_and_agent_listing_delta_follow_counts() {
        let projection = project_attachment_message(&input(AttachmentInput::DynamicSkill {
            display_path: ".rebon/skills".into(),
            skill_names: vec!["verify".into(), "review".into()],
        }));
        let AttachmentProjection::Lines(lines) = projection else {
            panic!("expected lines");
        };
        assert_eq!(lines[0].text, "Loaded 2 skills from .rebon/skills");

        let projection = project_attachment_message(&input(AttachmentInput::AgentListingDelta {
            is_initial: false,
            added_types: vec!["planner".into(), "reviewer".into()],
        }));
        let AttachmentProjection::Lines(lines) = projection else {
            panic!("expected lines");
        };
        assert_eq!(lines[0].text, "2 agent types available");
    }
}

/// The skill-listing attachment. The initial listing is not shown: it is the
/// state the session starts in, not a change worth a line.
fn project_skill_listing(
    input: &AttachmentMessageInput,
    skill_count: &usize,
    is_initial: &bool,
) -> AttachmentProjection {
    if *is_initial {
        AttachmentProjection::Hidden
    } else {
        AttachmentProjection::Lines(vec![line(
            format!(
                "{skill_count} {} available",
                plural(*skill_count, "skill", "skills")
            ),
            None,
            true,
            input.background.clone(),
        )])
    }
}

/// The skill-discovery attachment, shown only where discovery is on.
fn project_skill_discovery(
    input: &AttachmentMessageInput,
    feature_enabled: &bool,
    internal_build: &bool,
    is_demo_env: &bool,
    skills: &[AttachmentSkillDiscoveryEntry],
) -> AttachmentProjection {
    if !*feature_enabled || skills.is_empty() {
        AttachmentProjection::Hidden
    } else {
        let names = skills
            .iter()
            .map(|skill| match &skill.short_id {
                Some(short_id) => format!("{} [{}]", skill.name, short_id),
                None => skill.name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let hint = if *internal_build && !*is_demo_env {
            skills
                .first()
                .and_then(|skill| skill.short_id.as_ref())
                .map(|short_id| {
                    format!(" /skill-feedback {short_id} 1=wrong 2=noisy 3=good [comment]")
                })
        } else {
            None
        };
        AttachmentProjection::Lines(vec![line(
            format!(
                "{} relevant {}: {}{}",
                skills.len(),
                plural(skills.len(), "skill", "skills"),
                names,
                hint.map(|hint| format!(" \u{00B7}{hint}"))
                    .unwrap_or_default()
            ),
            None,
            true,
            input.background.clone(),
        )])
    }
}

/// Agents that appeared since the last listing. Like the skill listing, the
/// initial one is the starting state rather than a change.
fn project_agent_listing_delta(
    input: &AttachmentMessageInput,
    is_initial: &bool,
    added_types: &[String],
) -> AttachmentProjection {
    if *is_initial || added_types.is_empty() {
        AttachmentProjection::Hidden
    } else {
        let count = added_types.len();
        AttachmentProjection::Lines(vec![line(
            format!("{count} agent {} available", plural(count, "type", "types")),
            None,
            true,
            input.background.clone(),
        )])
    }
}

/// An async hook that finished. Hidden outside verbose and transcript modes,
/// because the turn it belongs to has usually moved on.
fn project_async_hook_response(
    input: &AttachmentMessageInput,
    hook_event: &str,
) -> AttachmentProjection {
    if !input.verbose && (hook_event == "SessionStart" || !input.is_transcript_mode) {
        AttachmentProjection::Hidden
    } else {
        AttachmentProjection::Lines(vec![line(
            format!("Async hook {hook_event} completed"),
            None,
            true,
            input.background.clone(),
        )])
    }
}

/// A hook that refused the action. Stop hooks are hidden: their refusal is
/// already reported as the cancel request's failure.
fn project_hook_blocking_error(
    input: &AttachmentMessageInput,
    hook_event: &str,
    hook_name: &str,
    stderr: &str,
) -> AttachmentProjection {
    if is_stop_hook(hook_event) {
        AttachmentProjection::Hidden
    } else {
        let mut lines = vec![line(
            format!("{hook_name} hook returned blocking error"),
            Some(AttachmentTone::Error),
            true,
            input.background.clone(),
        )];
        let stderr = stderr.trim();
        if !stderr.is_empty() {
            lines.push(line(
                stderr.to_string(),
                Some(AttachmentTone::Error),
                true,
                input.background.clone(),
            ));
        }
        AttachmentProjection::Lines(lines)
    }
}

/// A hook that failed without refusing the action.
fn project_hook_non_blocking_error(
    input: &AttachmentMessageInput,
    hook_event: &str,
    hook_name: &str,
) -> AttachmentProjection {
    if is_stop_hook(hook_event) {
        AttachmentProjection::Hidden
    } else {
        AttachmentProjection::Lines(vec![line(
            format!("{hook_name} hook error"),
            Some(AttachmentTone::Error),
            true,
            input.background.clone(),
        )])
    }
}

/// A hook that failed while running.
fn project_hook_error_during_execution(
    input: &AttachmentMessageInput,
    hook_event: &str,
    hook_name: &str,
) -> AttachmentProjection {
    if is_stop_hook(hook_event) {
        AttachmentProjection::Hidden
    } else {
        AttachmentProjection::Lines(vec![line(
            format!("{hook_name} hook warning"),
            None,
            true,
            input.background.clone(),
        )])
    }
}

/// A hook that stopped the agent from continuing.
fn project_hook_stopped_continuation(
    input: &AttachmentMessageInput,
    hook_event: &str,
    hook_name: &str,
    message: &str,
) -> AttachmentProjection {
    if is_stop_hook(hook_event) {
        AttachmentProjection::Hidden
    } else {
        AttachmentProjection::Lines(vec![line(
            format!("{hook_name} hook stopped continuation: {message}"),
            Some(AttachmentTone::Warning),
            true,
            input.background.clone(),
        )])
    }
}

/// A message a hook asked to show the user.
fn project_hook_system_message(
    input: &AttachmentMessageInput,
    hook_name: &str,
    content: &str,
) -> AttachmentProjection {
    AttachmentProjection::Lines(vec![line(
        format!("{hook_name} says: {content}"),
        None,
        true,
        input.background.clone(),
    )])
}

/// A permission decision a hook made on the agent's behalf.
fn project_hook_permission_decision(
    input: &AttachmentMessageInput,
    decision: &str,
    hook_event: &str,
) -> AttachmentProjection {
    let action = if decision == "allow" {
        "Allowed"
    } else {
        "Denied"
    };
    AttachmentProjection::Lines(vec![line(
        format!("{action} by {hook_event} hook"),
        None,
        true,
        input.background.clone(),
    )])
}

//! Tool-call grouping helpers.
//!
//! Groups related tool-use and tool-result messages for transcript display.
use std::collections::{HashMap, HashSet};

/// An attachment type string tested against the null-rendering list.
pub type NullRenderingAttachmentType = &'static str;

/// Attachment types that are always invisible, so grouping can skip them.
pub const NULL_RENDERING_TYPES: &[NullRenderingAttachmentType] = &[
    "hook_success",
    "hook_additional_context",
    "hook_cancelled",
    "command_permissions",
    "agent_mention",
    "budget_usd",
    "critical_system_reminder",
    "edited_image_file",
    "edited_text_file",
    "opened_file_in_ide",
    "output_style",
    "plan_mode",
    "plan_mode_exit",
    "plan_mode_reentry",
    "structured_output",
    "team_context",
    "todo_reminder",
    "context_efficiency",
    "deferred_tools_delta",
    "mcp_instructions_delta",
    "companion_intro",
    "token_usage",
    "ultrathink_effort",
    "max_turns_reached",
    "task_reminder",
    "auto_mode",
    "auto_mode_exit",
    "output_token_usage",
    "pen_mode_enter",
    "pen_mode_exit",
    "verify_plan_reminder",
    "current_session_memory",
    "compaction_reminder",
    "date_change",
];

/// Minimal attachment shape for null-rendering checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentLike {
    /// Attachment `type`.
    pub kind: String,
}

/// One grouped-tool-use input entry after the outer lookups are erased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupedToolUseInput {
    /// Tool name.
    pub tool_name: String,
    /// Tool use id.
    pub tool_use_id: String,
    /// Progress messages, already filtered by the caller.
    pub progress_messages: Vec<String>,
}

/// One grouped-tool-use output row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupedToolUseData {
    /// Tool use id.
    pub tool_use_id: String,
    /// Resolved state.
    pub is_resolved: bool,
    /// Error state.
    pub is_error: bool,
    /// In-progress state.
    pub is_in_progress: bool,
    /// Progress messages for this id.
    pub progress_messages: Vec<String>,
    /// Optional serialized result payload.
    pub result_payload: Option<String>,
}

/// Consumer-facing render request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupedToolUseRenderRequest {
    /// Tool name.
    pub tool_name: String,
    /// Whether animation should run.
    pub should_animate: bool,
    /// Per-tool-use entries.
    pub entries: Vec<GroupedToolUseData>,
}

/// True when the attachment is always invisible.
pub fn is_null_rendering_attachment(attachment: &AttachmentLike) -> bool {
    NULL_RENDERING_TYPES.contains(&attachment.kind.as_str())
}

/// Build the per-tool-use flags, payloads and animation flag for one tool
/// name. Animation is reported only when `should_animate` is set AND at
/// least one entry is still in progress.
pub fn build_grouped_tool_use_data(
    tool_name: &str,
    entries: &[GroupedToolUseInput],
    resolved_tool_use_ids: &HashSet<String>,
    errored_tool_use_ids: &HashSet<String>,
    in_progress_tool_use_ids: &HashSet<String>,
    result_payloads: &HashMap<String, String>,
    should_animate: bool,
) -> GroupedToolUseRenderRequest {
    let built_entries = entries
        .iter()
        .map(|entry| GroupedToolUseData {
            tool_use_id: entry.tool_use_id.clone(),
            is_resolved: resolved_tool_use_ids.contains(&entry.tool_use_id),
            is_error: errored_tool_use_ids.contains(&entry.tool_use_id),
            is_in_progress: in_progress_tool_use_ids.contains(&entry.tool_use_id),
            progress_messages: entry.progress_messages.clone(),
            result_payload: result_payloads.get(&entry.tool_use_id).cloned(),
        })
        .collect::<Vec<_>>();
    let any_in_progress = built_entries.iter().any(|d| d.is_in_progress);
    GroupedToolUseRenderRequest {
        tool_name: tool_name.to_string(),
        should_animate: should_animate && any_in_progress,
        entries: built_entries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_rendering_attachment_kinds_are_expected() {
        assert!(is_null_rendering_attachment(&AttachmentLike {
            kind: "hook_success".into()
        }));
        assert!(!is_null_rendering_attachment(&AttachmentLike {
            kind: "image".into()
        }));
    }

    #[test]
    fn grouped_tool_use_builds_entry_flags_and_animation() {
        let entries = vec![GroupedToolUseInput {
            tool_name: "Agent".into(),
            tool_use_id: "u1".into(),
            progress_messages: vec!["p".into()],
        }];
        let resolved = HashSet::from(["u1".to_string()]);
        let errors = HashSet::new();
        let in_progress = HashSet::from(["u1".to_string()]);
        let result_payloads = HashMap::from([("u1".to_string(), "done".to_string())]);
        let req = build_grouped_tool_use_data(
            "Agent",
            &entries,
            &resolved,
            &errors,
            &in_progress,
            &result_payloads,
            true,
        );
        assert!(req.should_animate);
        assert_eq!(req.entries[0].result_payload.as_deref(), Some("done"));
        assert!(req.entries[0].is_resolved);
    }
}

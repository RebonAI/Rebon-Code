//! `HookEventMetadata` table — pure per-event summary/description/
//! matcher-metadata lookup.
//!
//! ## Table contents
//!
//! [`build_hook_event_metadata`] returns 28 entries — one per
//! `HookEvent` variant — each with a short `summary`, a long
//! `description` (long-form text a UI may show), and an
//! optional [`MatcherMetadata`] naming the hook-input field a matcher
//! pattern is tested against plus the canonical value list for that
//! field:
//!
//! * `tool_name` — `PreToolUse`, `PostToolUse`, `PostToolUseFailure`,
//!   `PermissionDenied`, `PermissionRequest`. Values are the caller's
//!   tool names.
//! * `notification_type` — `Notification`. Values `permission_prompt`,
//!   `idle_prompt`, `auth_success`, `elicitation_dialog`,
//!   `elicitation_complete`, `elicitation_response`.
//! * `source` — `SessionStart` (`startup`, `resume`, `clear`,
//!   `compact`) and `ConfigChange` (`user_settings`,
//!   `project_settings`, `local_settings`, `policy_settings`,
//!   `skills`).
//! * `error` — `StopFailure`. Values `rate_limit`,
//!   `authentication_failed`, `billing_error`, `invalid_request`,
//!   `server_error`, `max_output_tokens`, `unknown`.
//! * `agent_type` — `SubagentStart`, `SubagentStop`. Values are the
//!   caller's agent types.
//! * `trigger` — `PreCompact` and `PostCompact` (`manual`, `auto`),
//!   plus `Setup` (`init`, `maintenance`).
//! * `reason` — `SessionEnd`. Values `clear`, `logout`,
//!   `prompt_input_exit`, `other`.
//! * `mcp_server_name` — `Elicitation`, `ElicitationResult`. Values are
//!   the caller's elicitation servers.
//! * `load_reason` — `InstructionsLoaded`. Values `session_start`,
//!   `nested_traversal`, `path_glob_match`, `include`, `compact`.
//! * `phase` — `Onboarding`. Values `opened`, `advanced`, `closed`,
//!   `completed`.
//!
//! Three load-bearing details:
//!
//! 1. **Per-event matcher metadata is optional.** Nine events have
//!    no matcher metadata at all (`UserPromptSubmit`, `Stop`,
//!    `TeammateIdle`, `TaskCreated`, `TaskCompleted`, `WorktreeCreate`,
//!    `WorktreeRemove`, `CwdChanged`, `FileChanged`), modeled as
//!    `Option<MatcherMetadata>`.
//! 2. **`tool_names` is injected.** Five events take their value list
//!    directly from the `tool_names` input (`PreToolUse`,
//!    `PostToolUse`, `PostToolUseFailure`, `PermissionDenied`,
//!    `PermissionRequest`). This crate takes a `MetadataInputs`
//!    struct so the test path can pin the table without spinning up
//!    an MCP-tool registry.
//! 3. **The `agent_types` and `elicitation_servers` value lists are
//!    also caller-injected.** They start out empty and are filled at
//!    the call site (`SubagentStart`, `SubagentStop`, `Elicitation`,
//!    `ElicitationResult`) as optional fields on `MetadataInputs` so
//!    the consumer can pin them.
//!
//! ## What is NOT modeled
//!
//! * Memoisation of the metadata table. It is computed fresh per
//!   call; consumers can layer a cache on top if they want one.

use std::collections::HashMap;

use crate::event::HookEvent;

/// Describes the field of the hook input that the matcher pattern is
/// applied against, plus the canonical value list (if known) for
/// hint UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatcherMetadata {
    pub field_to_match: String,
    pub values: Vec<String>,
}

/// The per-event summary line, the long description shown as the
/// `dialog` subtitle, and the optional matcher metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookEventMetadata {
    pub summary: String,
    pub description: String,
    pub matcher_metadata: Option<MatcherMetadata>,
}

/// Map from event to its metadata, as built by
/// [`build_hook_event_metadata`].
pub type EventMetadataMap = HashMap<HookEvent, HookEventMetadata>;

/// Inputs for [`build_hook_event_metadata`]:
///
/// * `tool_names` — used by `PreToolUse`, `PostToolUse`,
///   `PostToolUseFailure`, `PermissionDenied`, and
///   `PermissionRequest`.
/// * `agent_types` — starts empty, filled at the call site for
///   `SubagentStart` / `SubagentStop`.
/// * `elicitation_servers` — starts empty, filled at the call site
///   for `Elicitation` / `ElicitationResult`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataInputs {
    pub tool_names: Vec<String>,
    pub agent_types: Vec<String>,
    pub elicitation_servers: Vec<String>,
}

const NOTIFICATION_VALUES: &[&str] = &[
    "permission_prompt",
    "idle_prompt",
    "auth_success",
    "elicitation_dialog",
    "elicitation_complete",
    "elicitation_response",
];
const SESSION_START_VALUES: &[&str] = &["startup", "resume", "clear", "compact"];
const STOP_FAILURE_VALUES: &[&str] = &[
    "rate_limit",
    "authentication_failed",
    "billing_error",
    "invalid_request",
    "server_error",
    "max_output_tokens",
    "unknown",
];
const TRIGGER_VALUES: &[&str] = &["manual", "auto"];
const SESSION_END_VALUES: &[&str] = &["clear", "logout", "prompt_input_exit", "other"];
const SETUP_VALUES: &[&str] = &["init", "maintenance"];
const CONFIG_CHANGE_VALUES: &[&str] = &[
    "user_settings",
    "project_settings",
    "local_settings",
    "policy_settings",
    "skills",
];
const INSTRUCTIONS_LOADED_VALUES: &[&str] = &[
    "session_start",
    "nested_traversal",
    "path_glob_match",
    "include",
    "compact",
];
const ONBOARDING_VALUES: &[&str] = &["opened", "advanced", "closed", "completed"];

fn vec_of_str(slice: &[&str]) -> Vec<String> {
    slice.iter().map(|s| (*s).to_string()).collect()
}

fn meta(summary: &str, description: &str, matcher: Option<MatcherMetadata>) -> HookEventMetadata {
    HookEventMetadata {
        summary: summary.into(),
        description: description.into(),
        matcher_metadata: matcher,
    }
}

fn tool_name_matcher(inputs: &MetadataInputs) -> MatcherMetadata {
    MatcherMetadata {
        field_to_match: "tool_name".into(),
        values: inputs.tool_names.clone(),
    }
}

/// Builds the per-event metadata map from the given inputs. Returns
/// a fresh map per call (no memoisation — see crate-level note).
pub fn build_hook_event_metadata(inputs: &MetadataInputs) -> EventMetadataMap {
    let mut m = EventMetadataMap::new();

    m.insert(
        HookEvent::PreToolUse,
        meta(
            "Before tool execution",
            "Input to command is JSON of tool call arguments.\nExit code 0 - stdout/stderr not shown\nExit code 2 - show stderr to model and block tool call\nOther exit codes - show stderr to user only but continue with tool call",
            Some(tool_name_matcher(inputs)),
        ),
    );
    m.insert(
        HookEvent::PostToolUse,
        meta(
            "After tool execution",
            "Input to command is JSON with fields \"inputs\" (tool call arguments) and \"response\" (tool call response).\nExit code 0 - stdout shown in transcript mode (Ctrl+O)\nExit code 2 - show stderr to model immediately\nOther exit codes - show stderr to user only",
            Some(tool_name_matcher(inputs)),
        ),
    );
    m.insert(
        HookEvent::PostToolUseFailure,
        meta(
            "After tool execution fails",
            "Input to command is JSON with tool_name, tool_input, tool_use_id, error, error_type, is_interrupt, and is_timeout.\nExit code 0 - stdout shown in transcript mode (Ctrl+O)\nExit code 2 - show stderr to model immediately\nOther exit codes - show stderr to user only",
            Some(tool_name_matcher(inputs)),
        ),
    );
    m.insert(
        HookEvent::PermissionDenied,
        meta(
            "After auto mode classifier denies a tool call",
            "Input to command is JSON with tool_name, tool_input, tool_use_id, and reason.\nReturn {\"hookSpecificOutput\":{\"hookEventName\":\"PermissionDenied\",\"retry\":true}} to tell the model it may retry.\nExit code 0 - stdout shown in transcript mode (Ctrl+O)\nOther exit codes - show stderr to user only",
            Some(tool_name_matcher(inputs)),
        ),
    );
    m.insert(
        HookEvent::Notification,
        meta(
            "When notifications are sent",
            "Input to command is JSON with notification message and type.\nExit code 0 - stdout/stderr not shown\nOther exit codes - show stderr to user only",
            Some(MatcherMetadata {
                field_to_match: "notification_type".into(),
                values: vec_of_str(NOTIFICATION_VALUES),
            }),
        ),
    );
    m.insert(
        HookEvent::UserPromptSubmit,
        meta(
            "When the user submits a prompt",
            "Input to command is JSON with original user prompt text.\nExit code 0 - stdout shown to Rebon\nExit code 2 - block processing, erase original prompt, and show stderr to user only\nOther exit codes - show stderr to user only",
            None,
        ),
    );
    m.insert(
        HookEvent::SessionStart,
        meta(
            "When a new session is started",
            "Input to command is JSON with session start source.\nExit code 0 - stdout shown to Rebon\nBlocking errors are ignored\nOther exit codes - show stderr to user only",
            Some(MatcherMetadata {
                field_to_match: "source".into(),
                values: vec_of_str(SESSION_START_VALUES),
            }),
        ),
    );
    m.insert(
        HookEvent::Stop,
        meta(
            "Right before Rebon concludes its response",
            "Exit code 0 - stdout/stderr not shown\nExit code 2 - show stderr to model and continue conversation\nOther exit codes - show stderr to user only",
            None,
        ),
    );
    m.insert(
        HookEvent::StopFailure,
        meta(
            "When the turn ends due to an API error",
            "Fires instead of Stop when an API error (rate limit, auth failure, etc.) ended the turn. Fire-and-forget — hook output and exit codes are ignored.",
            Some(MatcherMetadata {
                field_to_match: "error".into(),
                values: vec_of_str(STOP_FAILURE_VALUES),
            }),
        ),
    );
    m.insert(
        HookEvent::SubagentStart,
        meta(
            "When a subagent (Agent tool call) is started",
            "Input to command is JSON with agent_id and agent_type.\nExit code 0 - stdout shown to subagent\nBlocking errors are ignored\nOther exit codes - show stderr to user only",
            Some(MatcherMetadata {
                field_to_match: "agent_type".into(),
                values: inputs.agent_types.clone(),
            }),
        ),
    );
    m.insert(
        HookEvent::SubagentStop,
        meta(
            "Right before a subagent (Agent tool call) concludes its response",
            "Input to command is JSON with agent_id, agent_type, and agent_transcript_path.\nExit code 0 - stdout/stderr not shown\nExit code 2 - show stderr to subagent and continue having it run\nOther exit codes - show stderr to user only",
            Some(MatcherMetadata {
                field_to_match: "agent_type".into(),
                values: inputs.agent_types.clone(),
            }),
        ),
    );
    m.insert(
        HookEvent::PreCompact,
        meta(
            "Before conversation compaction",
            "Input to command is JSON with compaction details.\nExit code 0 - stdout appended as custom compact instructions\nExit code 2 - block compaction\nOther exit codes - show stderr to user only but continue with compaction",
            Some(MatcherMetadata {
                field_to_match: "trigger".into(),
                values: vec_of_str(TRIGGER_VALUES),
            }),
        ),
    );
    m.insert(
        HookEvent::PostCompact,
        meta(
            "After conversation compaction",
            "Input to command is JSON with compaction details and the summary.\nExit code 0 - stdout shown to user\nOther exit codes - show stderr to user only",
            Some(MatcherMetadata {
                field_to_match: "trigger".into(),
                values: vec_of_str(TRIGGER_VALUES),
            }),
        ),
    );
    m.insert(
        HookEvent::SessionEnd,
        meta(
            "When a session is ending",
            "Input to command is JSON with session end reason.\nExit code 0 - command completes successfully\nOther exit codes - show stderr to user only",
            Some(MatcherMetadata {
                field_to_match: "reason".into(),
                values: vec_of_str(SESSION_END_VALUES),
            }),
        ),
    );
    m.insert(
        HookEvent::PermissionRequest,
        meta(
            "When a permission dialog is displayed",
            "Input to command is JSON with tool_name, tool_input, and tool_use_id.\nOutput JSON with hookSpecificOutput containing decision to allow or deny.\nExit code 0 - use hook decision if provided\nOther exit codes - show stderr to user only",
            Some(tool_name_matcher(inputs)),
        ),
    );
    m.insert(
        HookEvent::Setup,
        meta(
            "Repo setup hooks for init and maintenance",
            "Input to command is JSON with trigger (init or maintenance).\nExit code 0 - stdout shown to Rebon\nBlocking errors are ignored\nOther exit codes - show stderr to user only",
            Some(MatcherMetadata {
                field_to_match: "trigger".into(),
                values: vec_of_str(SETUP_VALUES),
            }),
        ),
    );
    m.insert(
        HookEvent::TeammateIdle,
        meta(
            "When a teammate is about to go idle",
            "Input to command is JSON with teammate_name and team_name.\nExit code 0 - stdout/stderr not shown\nExit code 2 - show stderr to teammate and prevent idle (teammate continues working)\nOther exit codes - show stderr to user only",
            None,
        ),
    );
    m.insert(
        HookEvent::TaskCreated,
        meta(
            "When a task is being created",
            "Input to command is JSON with task_id, task_subject, task_description, teammate_name, and team_name.\nExit code 0 - stdout/stderr not shown\nExit code 2 - show stderr to model and prevent task creation\nOther exit codes - show stderr to user only",
            None,
        ),
    );
    m.insert(
        HookEvent::TaskCompleted,
        meta(
            "When a task is being marked as completed",
            "Input to command is JSON with task_id, task_subject, task_description, teammate_name, and team_name.\nExit code 0 - stdout/stderr not shown\nExit code 2 - show stderr to model and prevent task completion\nOther exit codes - show stderr to user only",
            None,
        ),
    );
    m.insert(
        HookEvent::Elicitation,
        meta(
            "When an MCP server requests user input (elicitation)",
            "Input to command is JSON with mcp_server_name, message, and requested_schema.\nOutput JSON with hookSpecificOutput containing action (accept/decline/cancel) and optional content.\nExit code 0 - use hook response if provided\nExit code 2 - deny the elicitation\nOther exit codes - show stderr to user only",
            Some(MatcherMetadata {
                field_to_match: "mcp_server_name".into(),
                values: inputs.elicitation_servers.clone(),
            }),
        ),
    );
    m.insert(
        HookEvent::ElicitationResult,
        meta(
            "After a user responds to an MCP elicitation",
            "Input to command is JSON with mcp_server_name, action, content, mode, and elicitation_id.\nOutput JSON with hookSpecificOutput containing optional action and content to override the response.\nExit code 0 - use hook response if provided\nExit code 2 - block the response (action becomes decline)\nOther exit codes - show stderr to user only",
            Some(MatcherMetadata {
                field_to_match: "mcp_server_name".into(),
                values: inputs.elicitation_servers.clone(),
            }),
        ),
    );
    m.insert(
        HookEvent::ConfigChange,
        meta(
            "When configuration files change during a session",
            "Input to command is JSON with source (user_settings, project_settings, local_settings, policy_settings, skills) and file_path.\nExit code 0 - allow the change\nExit code 2 - block the change from being applied to the session\nOther exit codes - show stderr to user only",
            Some(MatcherMetadata {
                field_to_match: "source".into(),
                values: vec_of_str(CONFIG_CHANGE_VALUES),
            }),
        ),
    );
    m.insert(
        HookEvent::InstructionsLoaded,
        meta(
            "When an instruction file (REBON.md or rule) is loaded",
            "Input to command is JSON with file_path, memory_type (User, Project, Local, Managed), load_reason (session_start, nested_traversal, path_glob_match, include, compact), globs (optional — the paths: frontmatter patterns that matched), trigger_file_path (optional — the file Rebon touched that caused the load), and parent_file_path (optional — the file that @-included this one).\nExit code 0 - command completes successfully\nOther exit codes - show stderr to user only\nThis hook is observability-only and does not support blocking.",
            Some(MatcherMetadata {
                field_to_match: "load_reason".into(),
                values: vec_of_str(INSTRUCTIONS_LOADED_VALUES),
            }),
        ),
    );
    m.insert(
        HookEvent::WorktreeCreate,
        meta(
            "Create an isolated worktree for VCS-agnostic isolation",
            "Input to command is JSON with name (suggested worktree slug).\nStdout should contain the absolute path to the created worktree directory.\nExit code 0 - worktree created successfully\nOther exit codes - worktree creation failed",
            None,
        ),
    );
    m.insert(
        HookEvent::WorktreeRemove,
        meta(
            "Remove a previously created worktree",
            "Input to command is JSON with worktree_path (absolute path to worktree).\nExit code 0 - worktree removed successfully\nOther exit codes - show stderr to user only",
            None,
        ),
    );
    m.insert(
        HookEvent::CwdChanged,
        meta(
            "After the working directory changes",
            "Input to command is JSON with old_cwd and new_cwd.\nREBON_ENV_FILE is set — write bash exports there to apply env to subsequent BashTool commands.\nHook output can include hookSpecificOutput.watchPaths (array of absolute paths) to register with the FileChanged watcher.\nExit code 0 - command completes successfully\nOther exit codes - show stderr to user only",
            None,
        ),
    );
    m.insert(
        HookEvent::FileChanged,
        meta(
            "When a watched file changes",
            "Input to command is JSON with file_path and event (change, add, unlink).\nREBON_ENV_FILE is set — write bash exports there to apply env to subsequent BashTool commands.\nThe matcher field specifies filenames to watch in the current directory (e.g. \".envrc|.env\").\nHook output can include hookSpecificOutput.watchPaths (array of absolute paths) to dynamically update the watch list.\nExit code 0 - command completes successfully\nOther exit codes - show stderr to user only",
            None,
        ),
    );
    m.insert(
        HookEvent::Onboarding,
        meta(
            "When onboarding lifecycle events occur",
            "Observes onboarding dialog lifecycle phases. Input to command is JSON with phase, source, and optional step transition/outcome metadata. Hook effects are ignored so onboarding cannot be blocked or changed.",
            Some(MatcherMetadata {
                field_to_match: "phase".into(),
                values: vec_of_str(ONBOARDING_VALUES),
            }),
        ),
    );

    m
}

/// Returns the optional matcher metadata for an event from a
/// previously-built map.
pub fn matcher_metadata_for_event(
    metadata: &EventMetadataMap,
    event: HookEvent,
) -> Option<&MatcherMetadata> {
    metadata.get(&event)?.matcher_metadata.as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> EventMetadataMap {
        build_hook_event_metadata(&MetadataInputs {
            tool_names: vec!["Bash".into(), "Read".into(), "Write".into()],
            agent_types: vec!["test".into()],
            elicitation_servers: vec!["server-a".into()],
        })
    }

    #[test]
    fn covers_all_28_events() {
        let m = fixture();
        // Every HookEvent variant must have an entry.
        assert_eq!(m.len(), 28);
    }

    #[test]
    fn pre_tool_use_has_tool_name_matcher() {
        let m = fixture();
        let md = matcher_metadata_for_event(&m, HookEvent::PreToolUse).unwrap();
        assert_eq!(md.field_to_match, "tool_name");
        assert_eq!(md.values, vec!["Bash", "Read", "Write"]);
    }

    #[test]
    fn post_tool_use_has_tool_name_matcher() {
        let m = fixture();
        let md = matcher_metadata_for_event(&m, HookEvent::PostToolUse).unwrap();
        assert_eq!(md.field_to_match, "tool_name");
    }

    #[test]
    fn permission_request_has_tool_name_matcher() {
        let m = fixture();
        let md = matcher_metadata_for_event(&m, HookEvent::PermissionRequest).unwrap();
        assert_eq!(md.field_to_match, "tool_name");
    }

    #[test]
    fn permission_denied_has_tool_name_matcher() {
        let m = fixture();
        let md = matcher_metadata_for_event(&m, HookEvent::PermissionDenied).unwrap();
        assert_eq!(md.field_to_match, "tool_name");
    }

    #[test]
    fn post_tool_use_failure_has_tool_name_matcher() {
        let m = fixture();
        let md = matcher_metadata_for_event(&m, HookEvent::PostToolUseFailure).unwrap();
        assert_eq!(md.field_to_match, "tool_name");
    }

    #[test]
    fn user_prompt_submit_has_no_matcher() {
        let m = fixture();
        assert!(matcher_metadata_for_event(&m, HookEvent::UserPromptSubmit).is_none());
    }

    #[test]
    fn stop_has_no_matcher() {
        let m = fixture();
        assert!(matcher_metadata_for_event(&m, HookEvent::Stop).is_none());
    }

    #[test]
    fn teammate_idle_has_no_matcher() {
        let m = fixture();
        assert!(matcher_metadata_for_event(&m, HookEvent::TeammateIdle).is_none());
    }

    #[test]
    fn task_created_has_no_matcher() {
        let m = fixture();
        assert!(matcher_metadata_for_event(&m, HookEvent::TaskCreated).is_none());
    }

    #[test]
    fn task_completed_has_no_matcher() {
        let m = fixture();
        assert!(matcher_metadata_for_event(&m, HookEvent::TaskCompleted).is_none());
    }

    #[test]
    fn worktree_create_has_no_matcher() {
        let m = fixture();
        assert!(matcher_metadata_for_event(&m, HookEvent::WorktreeCreate).is_none());
    }

    #[test]
    fn worktree_remove_has_no_matcher() {
        let m = fixture();
        assert!(matcher_metadata_for_event(&m, HookEvent::WorktreeRemove).is_none());
    }

    #[test]
    fn cwd_changed_has_no_matcher() {
        let m = fixture();
        assert!(matcher_metadata_for_event(&m, HookEvent::CwdChanged).is_none());
    }

    #[test]
    fn file_changed_has_no_matcher() {
        let m = fixture();
        assert!(matcher_metadata_for_event(&m, HookEvent::FileChanged).is_none());
    }

    #[test]
    fn notification_value_list_pinned() {
        let m = fixture();
        let md = matcher_metadata_for_event(&m, HookEvent::Notification).unwrap();
        assert_eq!(md.field_to_match, "notification_type");
        assert_eq!(
            md.values,
            vec![
                "permission_prompt",
                "idle_prompt",
                "auth_success",
                "elicitation_dialog",
                "elicitation_complete",
                "elicitation_response",
            ]
        );
    }

    #[test]
    fn session_start_value_list_pinned() {
        let m = fixture();
        let md = matcher_metadata_for_event(&m, HookEvent::SessionStart).unwrap();
        assert_eq!(md.field_to_match, "source");
        assert_eq!(md.values, vec!["startup", "resume", "clear", "compact"]);
    }

    #[test]
    fn stop_failure_value_list_pinned() {
        let m = fixture();
        let md = matcher_metadata_for_event(&m, HookEvent::StopFailure).unwrap();
        assert_eq!(md.field_to_match, "error");
        assert_eq!(md.values.len(), 7);
        assert_eq!(md.values[0], "rate_limit");
        assert_eq!(md.values[6], "unknown");
    }

    #[test]
    fn pre_and_post_compact_share_trigger_values() {
        let m = fixture();
        let pre = matcher_metadata_for_event(&m, HookEvent::PreCompact).unwrap();
        let post = matcher_metadata_for_event(&m, HookEvent::PostCompact).unwrap();
        assert_eq!(pre.field_to_match, "trigger");
        assert_eq!(post.field_to_match, "trigger");
        assert_eq!(pre.values, vec!["manual", "auto"]);
        assert_eq!(post.values, pre.values);
    }

    #[test]
    fn session_end_value_list_pinned() {
        let m = fixture();
        let md = matcher_metadata_for_event(&m, HookEvent::SessionEnd).unwrap();
        assert_eq!(md.field_to_match, "reason");
        assert_eq!(
            md.values,
            vec!["clear", "logout", "prompt_input_exit", "other"]
        );
    }

    #[test]
    fn setup_value_list_pinned() {
        let m = fixture();
        let md = matcher_metadata_for_event(&m, HookEvent::Setup).unwrap();
        assert_eq!(md.field_to_match, "trigger");
        assert_eq!(md.values, vec!["init", "maintenance"]);
    }

    #[test]
    fn onboarding_value_list_pinned() {
        let m = fixture();
        let md = matcher_metadata_for_event(&m, HookEvent::Onboarding).unwrap();
        assert_eq!(md.field_to_match, "phase");
        assert_eq!(md.values, vec!["opened", "advanced", "closed", "completed"]);
    }

    #[test]
    fn config_change_value_list_pinned() {
        let m = fixture();
        let md = matcher_metadata_for_event(&m, HookEvent::ConfigChange).unwrap();
        assert_eq!(md.field_to_match, "source");
        assert_eq!(md.values.len(), 5);
        assert_eq!(md.values[0], "user_settings");
        assert_eq!(md.values[4], "skills");
    }

    #[test]
    fn instructions_loaded_value_list_pinned() {
        let m = fixture();
        let md = matcher_metadata_for_event(&m, HookEvent::InstructionsLoaded).unwrap();
        assert_eq!(md.field_to_match, "load_reason");
        assert_eq!(md.values.len(), 5);
    }

    #[test]
    fn subagent_start_uses_injected_agent_types() {
        let m = build_hook_event_metadata(&MetadataInputs {
            tool_names: vec![],
            agent_types: vec!["alpha".into(), "beta".into()],
            elicitation_servers: vec![],
        });
        let md = matcher_metadata_for_event(&m, HookEvent::SubagentStart).unwrap();
        assert_eq!(md.field_to_match, "agent_type");
        assert_eq!(md.values, vec!["alpha", "beta"]);
    }

    #[test]
    fn subagent_stop_uses_injected_agent_types() {
        let m = build_hook_event_metadata(&MetadataInputs {
            tool_names: vec![],
            agent_types: vec!["alpha".into()],
            elicitation_servers: vec![],
        });
        let md = matcher_metadata_for_event(&m, HookEvent::SubagentStop).unwrap();
        assert_eq!(md.field_to_match, "agent_type");
        assert_eq!(md.values, vec!["alpha"]);
    }

    #[test]
    fn elicitation_uses_injected_servers() {
        let m = build_hook_event_metadata(&MetadataInputs {
            tool_names: vec![],
            agent_types: vec![],
            elicitation_servers: vec!["s1".into(), "s2".into()],
        });
        let md = matcher_metadata_for_event(&m, HookEvent::Elicitation).unwrap();
        assert_eq!(md.field_to_match, "mcp_server_name");
        assert_eq!(md.values, vec!["s1", "s2"]);
    }

    #[test]
    fn elicitation_result_uses_injected_servers() {
        let m = build_hook_event_metadata(&MetadataInputs {
            tool_names: vec![],
            agent_types: vec![],
            elicitation_servers: vec!["s1".into()],
        });
        let md = matcher_metadata_for_event(&m, HookEvent::ElicitationResult).unwrap();
        assert_eq!(md.field_to_match, "mcp_server_name");
    }

    #[test]
    fn empty_tool_names_yield_empty_value_list() {
        let m = build_hook_event_metadata(&MetadataInputs::default());
        let md = matcher_metadata_for_event(&m, HookEvent::PreToolUse).unwrap();
        assert!(md.values.is_empty());
    }

    #[test]
    fn summary_strings_pinned_for_pre_tool_use() {
        let m = fixture();
        assert_eq!(
            m.get(&HookEvent::PreToolUse).unwrap().summary,
            "Before tool execution"
        );
    }

    #[test]
    fn summary_strings_pinned_for_session_end() {
        let m = fixture();
        assert_eq!(
            m.get(&HookEvent::SessionEnd).unwrap().summary,
            "When a session is ending"
        );
    }

    /// Exhaustive matcher-presence check across all 28 events.
    #[test]
    fn matcher_presence_table() {
        let m = fixture();
        let table: [(HookEvent, Option<&str>); 28] = [
            (HookEvent::PreToolUse, Some("tool_name")),
            (HookEvent::PostToolUse, Some("tool_name")),
            (HookEvent::PostToolUseFailure, Some("tool_name")),
            (HookEvent::Notification, Some("notification_type")),
            (HookEvent::UserPromptSubmit, None),
            (HookEvent::SessionStart, Some("source")),
            (HookEvent::SessionEnd, Some("reason")),
            (HookEvent::Stop, None),
            (HookEvent::StopFailure, Some("error")),
            (HookEvent::SubagentStart, Some("agent_type")),
            (HookEvent::SubagentStop, Some("agent_type")),
            (HookEvent::PreCompact, Some("trigger")),
            (HookEvent::PostCompact, Some("trigger")),
            (HookEvent::PermissionRequest, Some("tool_name")),
            (HookEvent::PermissionDenied, Some("tool_name")),
            (HookEvent::Setup, Some("trigger")),
            (HookEvent::TeammateIdle, None),
            (HookEvent::TaskCreated, None),
            (HookEvent::TaskCompleted, None),
            (HookEvent::Elicitation, Some("mcp_server_name")),
            (HookEvent::ElicitationResult, Some("mcp_server_name")),
            (HookEvent::ConfigChange, Some("source")),
            (HookEvent::WorktreeCreate, None),
            (HookEvent::WorktreeRemove, None),
            (HookEvent::InstructionsLoaded, Some("load_reason")),
            (HookEvent::CwdChanged, None),
            (HookEvent::FileChanged, None),
            (HookEvent::Onboarding, Some("phase")),
        ];
        for (event, expected_field) in table {
            let actual = matcher_metadata_for_event(&m, event).map(|md| md.field_to_match.as_str());
            assert_eq!(actual, expected_field, "event {event:?}");
        }
    }
}

//! Hook invocation input model.
//!
//! This is the single struct that stands between the host's runtime
//! (engine, TUI, ACP) and every kind of hook handler (command /
//! prompt / agent / http). A host builds a [`HookInvocationInput`]
//! once per event firing; the runtime feeds the same value to every
//! matching hook regardless of transport.
//!
//! ## Wire shape
//!
//! One JSON object per firing, serialised as stdin for command hooks
//! and as a request body for HTTP hooks. Its layout is the contract:
//!
//! * the per-event payload shapes
//! * the field layout written to the child's stdin
//! * the POST body layout
//!
//! The fields below are the intersection that every hook event sees,
//! plus the per-event payload in [`HookEventPayload`].
//!
//! ## One value, many transports
//!
//! ```ignore
//! let input = HookInvocationInput::new(ctx, HookEventPayload::PreToolUse { ... });
//! runtime.run_event(&input).await;
//! ```
//!
//! The executor decides how to render `input` — stdin JSON, HTTP body,
//! agent task input — from that one value. That is what this struct
//! exists to prevent: two transports seeing different bytes for the same
//! firing is a bug that stays invisible until a hook misbehaves.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::HookEvent;
use crate::output_protocol::{HookPermissionBehavior, JsonObject};

/// Common fields every hook event carries, serialised under these
/// top-level keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookInvocationInput {
    /// Absolute working directory of the session at firing time.
    /// Serialised as `cwd`.
    pub cwd: String,
    /// Absolute path to the transcript file for this session. Command
    /// and HTTP hooks read this to introspect message history.
    /// Serialised as `transcript_path`.
    pub transcript_path: String,
    /// Stable session identifier. Serialised as `session_id`.
    pub session_id: String,
    /// Canonical event name — present in both the envelope and the
    /// payload discriminator so JSON consumers can route on a single
    /// field. Serialised as `hook_event_name`.
    pub hook_event_name: HookEvent,
    /// Current permission mode (bypassPermissions / acceptEdits /
    /// default / plan). `None` when the host hasn't set one yet (e.g.
    /// before `SessionStart` settles). Serialised as `permission_mode`,
    /// omitted while `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<HookPermissionBehavior>,
    /// Per-subagent invocation identifier, when firing inside a worker
    /// agent spawned by the coordinator. `None` for main-thread
    /// events. Serialised as `agent_id`, omitted while `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Agent-type string (e.g. `"general-purpose"`, `"explorer"`).
    /// `None` on main-thread. Serialised as `agent_type`, omitted while
    /// `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// The discriminated payload for this specific event.
    pub payload: HookEventPayload,
}

impl HookInvocationInput {
    /// Build a new invocation with the event derived from the
    /// payload. Keeps the two discriminators in sync so callers can't
    /// pass an input whose envelope disagrees with its payload.
    pub fn new(ctx: HookInvocationContext, payload: HookEventPayload) -> Self {
        let hook_event_name = payload.event();
        Self {
            cwd: ctx.cwd,
            transcript_path: ctx.transcript_path,
            session_id: ctx.session_id,
            hook_event_name,
            permission_mode: ctx.permission_mode,
            agent_id: ctx.agent_id,
            agent_type: ctx.agent_type,
            payload,
        }
    }
}

/// Session-level fields the host holds once and reuses for every
/// firing. Exists only so [`HookInvocationInput::new`] takes a single
/// struct argument instead of a seven-field positional call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookInvocationContext {
    pub cwd: String,
    pub transcript_path: String,
    pub session_id: String,
    pub permission_mode: Option<HookPermissionBehavior>,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
}

/// Per-event payload. One variant per [`HookEvent`] — the variants
/// only differ in which of the event-specific fields they carry; the
/// common envelope fields live on [`HookInvocationInput`].
///
/// Two structuring rules:
///
/// 1. Every variant is named after a [`HookEvent`] byte-for-byte; the
///    `#[serde(tag = "hook_event_name")]` at the top lets host code
///    pattern-match on the same discriminator the JSON carries.
/// 2. When in doubt, model the field exactly the way the runtime
///    writes it out. That keeps the on-the-wire schema identical, so
///    existing Claude Code hook scripts work unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "hook_event_name")]
pub enum HookEventPayload {
    PreToolUse {
        tool_name: String,
        tool_input: Value,
        tool_use_id: String,
    },
    PostToolUse {
        tool_name: String,
        tool_input: Value,
        tool_response: Value,
        tool_use_id: String,
    },
    PostToolUseFailure {
        tool_name: String,
        tool_input: Value,
        tool_use_id: String,
        error: String,
    },
    Notification {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    UserPromptSubmit {
        prompt: String,
    },
    SessionStart {
        source: String,
        model: String,
    },
    SessionEnd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Stop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
    },
    StopFailure {
        error: String,
    },
    SubagentStart {
        agent_id: String,
        agent_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
    },
    SubagentStop {
        agent_id: String,
        agent_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
    },
    PreCompact {
        trigger: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
    },
    PostCompact {
        trigger: String,
    },
    PermissionRequest {
        tool_name: String,
        tool_input: JsonObject,
        tool_use_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    PermissionDenied {
        tool_name: String,
        tool_input: JsonObject,
        tool_use_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Setup,
    TeammateIdle {
        agent_id: String,
        agent_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    TaskCreated {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    TaskCompleted {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<String>,
    },
    Elicitation {
        server: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_schema: Option<Value>,
    },
    ElicitationResult {
        server: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<JsonObject>,
    },
    ConfigChange {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changed_keys: Vec<String>,
    },
    WorktreeCreate {
        requested_path: String,
    },
    WorktreeRemove {
        worktree_path: String,
    },
    InstructionsLoaded {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
    },
    CwdChanged {
        old_cwd: String,
        new_cwd: String,
    },
    FileChanged {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        change_type: Option<String>,
    },
    Onboarding {
        phase: String,
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous_step: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_step: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dialog_title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        theme_only: Option<bool>,
    },
}

impl HookEventPayload {
    /// The [`HookEvent`] this payload represents. Lets the runtime
    /// pick matchers and metadata from a single argument instead of
    /// threading the discriminator separately.
    pub const fn event(&self) -> HookEvent {
        match self {
            HookEventPayload::PreToolUse { .. } => HookEvent::PreToolUse,
            HookEventPayload::PostToolUse { .. } => HookEvent::PostToolUse,
            HookEventPayload::PostToolUseFailure { .. } => HookEvent::PostToolUseFailure,
            HookEventPayload::Notification { .. } => HookEvent::Notification,
            HookEventPayload::UserPromptSubmit { .. } => HookEvent::UserPromptSubmit,
            HookEventPayload::SessionStart { .. } => HookEvent::SessionStart,
            HookEventPayload::SessionEnd { .. } => HookEvent::SessionEnd,
            HookEventPayload::Stop { .. } => HookEvent::Stop,
            HookEventPayload::StopFailure { .. } => HookEvent::StopFailure,
            HookEventPayload::SubagentStart { .. } => HookEvent::SubagentStart,
            HookEventPayload::SubagentStop { .. } => HookEvent::SubagentStop,
            HookEventPayload::PreCompact { .. } => HookEvent::PreCompact,
            HookEventPayload::PostCompact { .. } => HookEvent::PostCompact,
            HookEventPayload::PermissionRequest { .. } => HookEvent::PermissionRequest,
            HookEventPayload::PermissionDenied { .. } => HookEvent::PermissionDenied,
            HookEventPayload::Setup => HookEvent::Setup,
            HookEventPayload::TeammateIdle { .. } => HookEvent::TeammateIdle,
            HookEventPayload::TaskCreated { .. } => HookEvent::TaskCreated,
            HookEventPayload::TaskCompleted { .. } => HookEvent::TaskCompleted,
            HookEventPayload::Elicitation { .. } => HookEvent::Elicitation,
            HookEventPayload::ElicitationResult { .. } => HookEvent::ElicitationResult,
            HookEventPayload::ConfigChange { .. } => HookEvent::ConfigChange,
            HookEventPayload::WorktreeCreate { .. } => HookEvent::WorktreeCreate,
            HookEventPayload::WorktreeRemove { .. } => HookEvent::WorktreeRemove,
            HookEventPayload::InstructionsLoaded { .. } => HookEvent::InstructionsLoaded,
            HookEventPayload::CwdChanged { .. } => HookEvent::CwdChanged,
            HookEventPayload::FileChanged { .. } => HookEvent::FileChanged,
            HookEventPayload::Onboarding { .. } => HookEvent::Onboarding,
        }
    }

    /// The tool name associated with this payload, when the event is
    /// scoped to a specific tool call. Used by the matcher filter to
    /// decide whether a hook with `matcher = "Bash"` fires.
    pub fn tool_name(&self) -> Option<&str> {
        match self {
            HookEventPayload::PreToolUse { tool_name, .. }
            | HookEventPayload::PostToolUse { tool_name, .. }
            | HookEventPayload::PostToolUseFailure { tool_name, .. }
            | HookEventPayload::PermissionRequest { tool_name, .. }
            | HookEventPayload::PermissionDenied { tool_name, .. } => Some(tool_name.as_str()),
            _ => None,
        }
    }

    /// The matcher value associated with this payload, when the event
    /// has matcher metadata. Tool-scoped events use `tool_name`; other
    /// matcher-backed events use their event-specific field.
    pub fn matcher_value(&self) -> Option<&str> {
        match self {
            HookEventPayload::PreToolUse { tool_name, .. }
            | HookEventPayload::PostToolUse { tool_name, .. }
            | HookEventPayload::PostToolUseFailure { tool_name, .. }
            | HookEventPayload::PermissionRequest { tool_name, .. }
            | HookEventPayload::PermissionDenied { tool_name, .. } => Some(tool_name.as_str()),
            HookEventPayload::SessionStart { source, .. } => Some(source.as_str()),
            HookEventPayload::SessionEnd { reason } => reason.as_deref(),
            HookEventPayload::StopFailure { error } => Some(error.as_str()),
            HookEventPayload::SubagentStart { agent_type, .. }
            | HookEventPayload::SubagentStop { agent_type, .. } => Some(agent_type.as_str()),
            HookEventPayload::PreCompact { trigger, .. }
            | HookEventPayload::PostCompact { trigger } => Some(trigger.as_str()),
            HookEventPayload::Elicitation { server, .. }
            | HookEventPayload::ElicitationResult { server, .. } => Some(server.as_str()),
            HookEventPayload::Onboarding { phase, .. } => Some(phase.as_str()),
            _ => None,
        }
    }

    /// The `tool_use_id` associated with this payload, when tool-
    /// scoped. Used to correlate hook results back to the originating
    /// tool call in the runtime.
    pub fn tool_use_id(&self) -> Option<&str> {
        match self {
            HookEventPayload::PreToolUse { tool_use_id, .. }
            | HookEventPayload::PostToolUse { tool_use_id, .. }
            | HookEventPayload::PostToolUseFailure { tool_use_id, .. }
            | HookEventPayload::PermissionRequest { tool_use_id, .. }
            | HookEventPayload::PermissionDenied { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> HookInvocationContext {
        HookInvocationContext {
            cwd: "/tmp/proj".into(),
            transcript_path: "/tmp/proj/.rebon/transcript.jsonl".into(),
            session_id: "abc-123".into(),
            permission_mode: Some(HookPermissionBehavior::Ask),
            agent_id: None,
            agent_type: None,
        }
    }

    #[test]
    fn new_syncs_envelope_and_payload_discriminators() {
        let input = HookInvocationInput::new(
            ctx(),
            HookEventPayload::UserPromptSubmit {
                prompt: "hi".into(),
            },
        );
        assert_eq!(input.hook_event_name, HookEvent::UserPromptSubmit);
        assert!(matches!(
            input.payload,
            HookEventPayload::UserPromptSubmit { .. }
        ));
    }

    #[test]
    fn envelope_threads_common_fields() {
        let input = HookInvocationInput::new(ctx(), HookEventPayload::Setup);
        assert_eq!(input.cwd, "/tmp/proj");
        assert_eq!(input.session_id, "abc-123");
        assert_eq!(input.permission_mode, Some(HookPermissionBehavior::Ask));
    }

    #[test]
    fn matcher_value_follows_event_specific_matcher_fields() {
        let cases = [
            (
                HookEventPayload::SessionStart {
                    source: "resume".into(),
                    model: "opus".into(),
                },
                Some("resume"),
            ),
            (
                HookEventPayload::SessionEnd {
                    reason: Some("clear".into()),
                },
                Some("clear"),
            ),
            (
                HookEventPayload::StopFailure {
                    error: "rate_limit".into(),
                },
                Some("rate_limit"),
            ),
            (
                HookEventPayload::SubagentStart {
                    agent_id: "a".into(),
                    agent_type: "verification".into(),
                    task: None,
                },
                Some("verification"),
            ),
            (
                HookEventPayload::PreCompact {
                    trigger: "auto".into(),
                    custom_instructions: None,
                },
                Some("auto"),
            ),
            (
                HookEventPayload::Elicitation {
                    server: "mcp-a".into(),
                    message: "fill this".into(),
                    requested_schema: None,
                },
                Some("mcp-a"),
            ),
            (
                HookEventPayload::Onboarding {
                    phase: "opened".into(),
                    source: "slash_onboarding".into(),
                    step: None,
                    previous_step: None,
                    next_step: None,
                    outcome: None,
                    dialog_title: None,
                    theme_only: None,
                },
                Some("opened"),
            ),
        ];

        for (payload, expected) in cases {
            assert_eq!(payload.matcher_value(), expected, "{payload:?}");
        }
    }

    #[test]
    fn matcher_value_none_for_matcherless_events() {
        assert_eq!(
            HookEventPayload::UserPromptSubmit {
                prompt: "hi".into()
            }
            .matcher_value(),
            None
        );
        assert_eq!(
            HookEventPayload::CwdChanged {
                old_cwd: "/a".into(),
                new_cwd: "/b".into()
            }
            .matcher_value(),
            None
        );
    }

    #[test]
    fn event_derives_every_variant() {
        // Spot-check the important tool-scoped variants — every
        // HookEvent variant is round-tripped by the test below.
        let p = HookEventPayload::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "ls"}),
            tool_use_id: "t1".into(),
        };
        assert_eq!(p.event(), HookEvent::PreToolUse);
        assert_eq!(p.tool_name(), Some("Bash"));
        assert_eq!(p.tool_use_id(), Some("t1"));
    }

    #[test]
    fn tool_name_none_for_non_tool_events() {
        let p = HookEventPayload::UserPromptSubmit {
            prompt: "hi".into(),
        };
        assert_eq!(p.tool_name(), None);
        assert_eq!(p.tool_use_id(), None);
    }

    #[test]
    fn serialized_shape_carries_hook_event_name_tag() {
        let payload = HookEventPayload::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "ls"}),
            tool_use_id: "t1".into(),
        };
        let wire = serde_json::to_value(&payload).unwrap();
        assert_eq!(wire["hook_event_name"], "PreToolUse");
        assert_eq!(wire["tool_name"], "Bash");
    }

    #[test]
    fn every_event_payload_roundtrips() {
        // Walk every HookEvent variant: build the smallest payload
        // that fits, roundtrip through JSON, and confirm the event
        // name threads through.
        for event in crate::event::HOOK_EVENTS {
            let payload = smallest_payload(event);
            let wire = serde_json::to_value(&payload).unwrap();
            assert_eq!(wire["hook_event_name"], event.name(), "{event:?}");
            let back: HookEventPayload = serde_json::from_value(wire).unwrap();
            assert_eq!(back.event(), event, "{event:?}");
        }
    }

    #[test]
    fn onboarding_payload_event_and_matcher_value() {
        let payload = HookEventPayload::Onboarding {
            phase: "opened".into(),
            source: "slash_onboarding".into(),
            step: None,
            previous_step: None,
            next_step: None,
            outcome: None,
            dialog_title: None,
            theme_only: None,
        };

        assert_eq!(payload.event(), HookEvent::Onboarding);
        assert_eq!(payload.matcher_value(), Some("opened"));
    }

    #[test]
    fn onboarding_minimal_json_skips_optional_fields_and_roundtrips() {
        let payload = HookEventPayload::Onboarding {
            phase: "opened".into(),
            source: "first_run".into(),
            step: None,
            previous_step: None,
            next_step: None,
            outcome: None,
            dialog_title: None,
            theme_only: None,
        };

        let wire = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            wire,
            json!({
                "hook_event_name": "Onboarding",
                "phase": "opened",
                "source": "first_run"
            })
        );
        let back: HookEventPayload = serde_json::from_value(wire).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn onboarding_optional_fields_serialize() {
        let payload = HookEventPayload::Onboarding {
            phase: "advanced".into(),
            source: "slash_theme".into(),
            step: Some("setup".into()),
            previous_step: Some("theme".into()),
            next_step: Some("setup".into()),
            outcome: Some("theme_selected".into()),
            dialog_title: Some("Customize Rebon".into()),
            theme_only: Some(true),
        };

        let wire = serde_json::to_value(&payload).unwrap();
        assert_eq!(wire["step"], "setup");
        assert_eq!(wire["previous_step"], "theme");
        assert_eq!(wire["next_step"], "setup");
        assert_eq!(wire["outcome"], "theme_selected");
        assert_eq!(wire["dialog_title"], "Customize Rebon");
        assert_eq!(wire["theme_only"], true);
        let back: HookEventPayload = serde_json::from_value(wire).unwrap();
        assert_eq!(back, payload);
    }

    fn smallest_payload(event: HookEvent) -> HookEventPayload {
        match event {
            HookEvent::PreToolUse => HookEventPayload::PreToolUse {
                tool_name: "T".into(),
                tool_input: json!({}),
                tool_use_id: "x".into(),
            },
            HookEvent::PostToolUse => HookEventPayload::PostToolUse {
                tool_name: "T".into(),
                tool_input: json!({}),
                tool_response: json!({}),
                tool_use_id: "x".into(),
            },
            HookEvent::PostToolUseFailure => HookEventPayload::PostToolUseFailure {
                tool_name: "T".into(),
                tool_input: json!({}),
                tool_use_id: "x".into(),
                error: "boom".into(),
            },
            HookEvent::Notification => HookEventPayload::Notification {
                message: "m".into(),
                title: None,
            },
            HookEvent::UserPromptSubmit => {
                HookEventPayload::UserPromptSubmit { prompt: "p".into() }
            }
            HookEvent::SessionStart => HookEventPayload::SessionStart {
                source: "startup".into(),
                model: "opus".into(),
            },
            HookEvent::SessionEnd => HookEventPayload::SessionEnd { reason: None },
            HookEvent::Stop => HookEventPayload::Stop { stop_reason: None },
            HookEvent::StopFailure => HookEventPayload::StopFailure { error: "x".into() },
            HookEvent::SubagentStart => HookEventPayload::SubagentStart {
                agent_id: "a".into(),
                agent_type: "t".into(),
                task: None,
            },
            HookEvent::SubagentStop => HookEventPayload::SubagentStop {
                agent_id: "a".into(),
                agent_type: "t".into(),
                stop_reason: None,
            },
            HookEvent::PreCompact => HookEventPayload::PreCompact {
                trigger: "manual".into(),
                custom_instructions: None,
            },
            HookEvent::PostCompact => HookEventPayload::PostCompact {
                trigger: "manual".into(),
            },
            HookEvent::PermissionRequest => HookEventPayload::PermissionRequest {
                tool_name: "T".into(),
                tool_input: Default::default(),
                tool_use_id: "x".into(),
                reason: None,
            },
            HookEvent::PermissionDenied => HookEventPayload::PermissionDenied {
                tool_name: "T".into(),
                tool_input: Default::default(),
                tool_use_id: "x".into(),
                message: None,
            },
            HookEvent::Setup => HookEventPayload::Setup,
            HookEvent::TeammateIdle => HookEventPayload::TeammateIdle {
                agent_id: "a".into(),
                agent_type: "t".into(),
                prompt: None,
            },
            HookEvent::TaskCreated => HookEventPayload::TaskCreated {
                task_id: "tid".into(),
                description: None,
            },
            HookEvent::TaskCompleted => HookEventPayload::TaskCompleted {
                task_id: "tid".into(),
                outcome: None,
            },
            HookEvent::Elicitation => HookEventPayload::Elicitation {
                server: "s".into(),
                message: "m".into(),
                requested_schema: None,
            },
            HookEvent::ElicitationResult => HookEventPayload::ElicitationResult {
                server: "s".into(),
                message: "m".into(),
                content: None,
            },
            HookEvent::ConfigChange => HookEventPayload::ConfigChange {
                changed_keys: vec![],
            },
            HookEvent::WorktreeCreate => HookEventPayload::WorktreeCreate {
                requested_path: "/tmp/wt".into(),
            },
            HookEvent::WorktreeRemove => HookEventPayload::WorktreeRemove {
                worktree_path: "/tmp/wt".into(),
            },
            HookEvent::InstructionsLoaded => HookEventPayload::InstructionsLoaded { paths: vec![] },
            HookEvent::CwdChanged => HookEventPayload::CwdChanged {
                old_cwd: "/a".into(),
                new_cwd: "/b".into(),
            },
            HookEvent::FileChanged => HookEventPayload::FileChanged {
                path: "/tmp/f".into(),
                change_type: None,
            },
            HookEvent::Onboarding => HookEventPayload::Onboarding {
                phase: "opened".into(),
                source: "slash_onboarding".into(),
                step: None,
                previous_step: None,
                next_step: None,
                outcome: None,
                dialog_title: None,
                theme_only: None,
            },
        }
    }
}

//! `IndividualHookConfig` shape and the small helpers
//! `event_supports_matcher` / `matcher_or_all`.
//!
//! ## Behaviour
//!
//! [`IndividualHookConfig`] is one hook record: the event it fires on,
//! its [`HookCommand`], an optional matcher, the [`HookSource`] it was
//! read from, and — for plugin hooks only — the plugin's name.
//!
//! [`event_supports_matcher`] is true iff the event carries matcher
//! metadata in the [`EventMetadataMap`], i.e. iff a matcher string can
//! ever mean anything for that event.
//!
//! Matchers have two string defaults, and they are deliberately not the
//! same string. The display label for an absent or empty matcher is
//! `"(all)"` ([`matcher_or_all`]); the grouping dictionary keys the same
//! matcher as the empty string ([`matcher_key`]).

use crate::event::HookEvent;
use crate::event_metadata::{matcher_metadata_for_event, EventMetadataMap};
use crate::hook_command::HookCommand;
use crate::hook_source::HookSource;

/// One hook record: what to run, on which event, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndividualHookConfig {
    pub event: HookEvent,
    pub config: HookCommand,
    /// `None` for events with no matcher
    /// metadata (e.g. `Stop`, `UserPromptSubmit`); empty string and
    /// `None` are treated as equivalent by the grouping pass.
    pub matcher: Option<String>,
    pub source: HookSource,
    /// Set only when `source == PluginHook`: the id of the plugin that
    /// registered the hook.
    pub plugin_name: Option<String>,
}

/// True iff the event has matcher metadata in the `HookEventMetadata`
/// table.
pub fn event_supports_matcher(event: HookEvent, metadata: &EventMetadataMap) -> bool {
    matcher_metadata_for_event(metadata, event).is_some()
}

/// The display label for a matcher: the matcher itself, or `"(all)"`.
///
/// `None` and `Some("")` both render as `"(all)"`.
pub fn matcher_or_all(matcher: Option<&str>) -> &str {
    match matcher {
        Some(m) if !m.is_empty() => m,
        _ => "(all)",
    }
}

/// The dictionary key form of a matcher: `None` becomes `""`. Used by
/// [`crate::grouping::group_hooks_by_event_and_matcher`].
pub fn matcher_key(matcher: Option<&str>) -> &str {
    matcher.unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_metadata::{build_hook_event_metadata, MetadataInputs};
    use crate::hook_command::{BashCommandHook, HookCommand};

    fn fixture_inputs() -> MetadataInputs {
        MetadataInputs {
            tool_names: vec!["Bash".into(), "Read".into()],
            agent_types: vec!["test-agent".into()],
            elicitation_servers: vec![],
        }
    }

    #[test]
    fn struct_round_trips_all_fields() {
        let h = IndividualHookConfig {
            event: HookEvent::PreToolUse,
            config: HookCommand::Command(BashCommandHook {
                command: "ls".into(),
                r#if: None,
                shell: None,
                timeout: None,
                status_message: None,
                once: None,
                r#async: None,
                async_rewake: None,
            }),
            matcher: Some("Bash".into()),
            source: HookSource::ProjectSettings,
            plugin_name: None,
        };
        assert_eq!(h.event, HookEvent::PreToolUse);
        assert_eq!(h.matcher.as_deref(), Some("Bash"));
        assert_eq!(h.source, HookSource::ProjectSettings);
        assert_eq!(h.plugin_name, None);
        assert_eq!(h.config.type_str(), "command");
    }

    #[test]
    fn matcher_or_all_some_value() {
        assert_eq!(matcher_or_all(Some("Bash")), "Bash");
    }

    #[test]
    fn matcher_or_all_none_returns_all_label() {
        assert_eq!(matcher_or_all(None), "(all)");
    }

    #[test]
    fn matcher_or_all_empty_string_returns_all_label() {
        assert_eq!(matcher_or_all(Some("")), "(all)");
    }

    #[test]
    fn matcher_key_some_value() {
        assert_eq!(matcher_key(Some("Bash")), "Bash");
    }

    #[test]
    fn matcher_key_none_returns_empty_string() {
        // The dict-key form preserves the empty-string default the
        // grouping pass uses.
        assert_eq!(matcher_key(None), "");
    }

    #[test]
    fn matcher_key_empty_string_passes_through() {
        assert_eq!(matcher_key(Some("")), "");
    }

    #[test]
    fn event_supports_matcher_true_for_pre_tool_use() {
        let meta = build_hook_event_metadata(&fixture_inputs());
        // PreToolUse has matcher metadata in the event metadata table.
        assert!(event_supports_matcher(HookEvent::PreToolUse, &meta));
    }

    #[test]
    fn event_supports_matcher_false_for_user_prompt_submit() {
        let meta = build_hook_event_metadata(&fixture_inputs());
        // UserPromptSubmit has NO matcher metadata.
        assert!(!event_supports_matcher(HookEvent::UserPromptSubmit, &meta));
    }

    #[test]
    fn event_supports_matcher_false_for_stop() {
        let meta = build_hook_event_metadata(&fixture_inputs());
        assert!(!event_supports_matcher(HookEvent::Stop, &meta));
    }

    #[test]
    fn event_supports_matcher_true_for_session_start() {
        let meta = build_hook_event_metadata(&fixture_inputs());
        assert!(event_supports_matcher(HookEvent::SessionStart, &meta));
    }

    #[test]
    fn event_supports_matcher_false_for_teammate_idle() {
        let meta = build_hook_event_metadata(&fixture_inputs());
        assert!(!event_supports_matcher(HookEvent::TeammateIdle, &meta));
    }

    #[test]
    fn event_supports_matcher_false_for_worktree_create() {
        let meta = build_hook_event_metadata(&fixture_inputs());
        // WorktreeCreate has no matcher metadata in the event metadata table.
        assert!(!event_supports_matcher(HookEvent::WorktreeCreate, &meta));
    }
}

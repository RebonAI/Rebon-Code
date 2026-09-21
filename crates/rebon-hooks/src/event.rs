//! `HookEvent` enum and the `HOOK_EVENTS` canonical list.
//!
//! ## Canonical event list
//!
//! ```text
//! PreToolUse
//! PostToolUse
//! PostToolUseFailure
//! Notification
//! UserPromptSubmit
//! SessionStart
//! SessionEnd
//! Stop
//! StopFailure
//! SubagentStart
//! SubagentStop
//! PreCompact
//! PostCompact
//! PermissionRequest
//! PermissionDenied
//! Setup
//! TeammateIdle
//! TaskCreated
//! TaskCompleted
//! Elicitation
//! ElicitationResult
//! ConfigChange
//! WorktreeCreate
//! WorktreeRemove
//! InstructionsLoaded
//! CwdChanged
//! FileChanged
//! Onboarding
//! ```
//!
//! Order must be preserved exactly: it is the order the event-metadata
//! table and the event/matcher grouping both rely on when they iterate
//! events.

/// One of the canonical hook events, in the fixed order the
/// event-metadata table and [`HookEvent::all`] both rely on.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    Notification,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
    Stop,
    StopFailure,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    PermissionRequest,
    PermissionDenied,
    Setup,
    TeammateIdle,
    TaskCreated,
    TaskCompleted,
    Elicitation,
    ElicitationResult,
    ConfigChange,
    WorktreeCreate,
    WorktreeRemove,
    InstructionsLoaded,
    CwdChanged,
    FileChanged,
    Onboarding,
}

/// Canonical ordered list of every hook event.
pub const HOOK_EVENTS: [HookEvent; 28] = [
    HookEvent::PreToolUse,
    HookEvent::PostToolUse,
    HookEvent::PostToolUseFailure,
    HookEvent::Notification,
    HookEvent::UserPromptSubmit,
    HookEvent::SessionStart,
    HookEvent::SessionEnd,
    HookEvent::Stop,
    HookEvent::StopFailure,
    HookEvent::SubagentStart,
    HookEvent::SubagentStop,
    HookEvent::PreCompact,
    HookEvent::PostCompact,
    HookEvent::PermissionRequest,
    HookEvent::PermissionDenied,
    HookEvent::Setup,
    HookEvent::TeammateIdle,
    HookEvent::TaskCreated,
    HookEvent::TaskCompleted,
    HookEvent::Elicitation,
    HookEvent::ElicitationResult,
    HookEvent::ConfigChange,
    HookEvent::WorktreeCreate,
    HookEvent::WorktreeRemove,
    HookEvent::InstructionsLoaded,
    HookEvent::CwdChanged,
    HookEvent::FileChanged,
    HookEvent::Onboarding,
];

impl HookEvent {
    /// Return the canonical string identifier (`"PreToolUse"` etc.).
    /// Used for serializing to settings.json and for the `name` keys
    /// in the grouped-by-event-and-matcher record.
    pub const fn name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::Notification => "Notification",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::Stop => "Stop",
            HookEvent::StopFailure => "StopFailure",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
            HookEvent::PermissionRequest => "PermissionRequest",
            HookEvent::PermissionDenied => "PermissionDenied",
            HookEvent::Setup => "Setup",
            HookEvent::TeammateIdle => "TeammateIdle",
            HookEvent::TaskCreated => "TaskCreated",
            HookEvent::TaskCompleted => "TaskCompleted",
            HookEvent::Elicitation => "Elicitation",
            HookEvent::ElicitationResult => "ElicitationResult",
            HookEvent::ConfigChange => "ConfigChange",
            HookEvent::WorktreeCreate => "WorktreeCreate",
            HookEvent::WorktreeRemove => "WorktreeRemove",
            HookEvent::InstructionsLoaded => "InstructionsLoaded",
            HookEvent::CwdChanged => "CwdChanged",
            HookEvent::FileChanged => "FileChanged",
            HookEvent::Onboarding => "Onboarding",
        }
    }

    /// Iterator over [`HOOK_EVENTS`].
    pub fn all() -> impl Iterator<Item = HookEvent> {
        HOOK_EVENTS.iter().copied()
    }
}

/// Parse a string into a [`HookEvent`] — exact match, case-sensitive,
/// no fuzzing.
///
/// Returns `None` for unknown identifiers; callers can map that to
/// whatever error type they want.
pub fn parse_hook_event(s: &str) -> Option<HookEvent> {
    HOOK_EVENTS.iter().copied().find(|e| e.name() == s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_event_count_is_28() {
        // 27 canonical events plus the onboarding lifecycle extension.
        assert_eq!(HOOK_EVENTS.len(), 28);
    }

    #[test]
    fn hook_event_order_is_expected() {
        // Spot-check the first and last so the table tests below
        // can rely on the ordering.
        assert_eq!(HOOK_EVENTS[0], HookEvent::PreToolUse);
        assert_eq!(HOOK_EVENTS[26], HookEvent::FileChanged);
        assert_eq!(HOOK_EVENTS[27], HookEvent::Onboarding);
    }

    #[test]
    fn name_round_trips() {
        for event in HOOK_EVENTS {
            assert_eq!(parse_hook_event(event.name()), Some(event));
        }
    }

    #[test]
    fn parse_unknown_returns_none() {
        assert_eq!(parse_hook_event("PreToolUseExtra"), None);
        assert_eq!(parse_hook_event(""), None);
    }

    #[test]
    fn parse_is_case_sensitive() {
        assert_eq!(parse_hook_event("pretooluse"), None);
        assert_eq!(parse_hook_event("PRETOOLUSE"), None);
    }

    #[test]
    fn all_yields_canonical_order() {
        let collected: Vec<HookEvent> = HookEvent::all().collect();
        assert_eq!(collected, HOOK_EVENTS.to_vec());
    }

    #[test]
    fn serde_uses_exact_pascal_case_names() {
        let json = serde_json::to_string(&HookEvent::PermissionRequest).unwrap();
        assert_eq!(json, "\"PermissionRequest\"");
        let event: HookEvent = serde_json::from_str("\"WorktreeCreate\"").unwrap();
        assert_eq!(event, HookEvent::WorktreeCreate);
    }

    #[test]
    fn serde_rejects_unknown_or_wrong_case_names() {
        assert!(serde_json::from_str::<HookEvent>("\"permissionrequest\"").is_err());
        assert!(serde_json::from_str::<HookEvent>("\"Unknown\"").is_err());
    }

    /// table — every event from `HOOK_EVENTS` must
    /// round-trip name → enum → name.
    #[test]
    fn event_name_table_round_trips() {
        let table: [(&str, HookEvent); 28] = [
            ("PreToolUse", HookEvent::PreToolUse),
            ("PostToolUse", HookEvent::PostToolUse),
            ("PostToolUseFailure", HookEvent::PostToolUseFailure),
            ("Notification", HookEvent::Notification),
            ("UserPromptSubmit", HookEvent::UserPromptSubmit),
            ("SessionStart", HookEvent::SessionStart),
            ("SessionEnd", HookEvent::SessionEnd),
            ("Stop", HookEvent::Stop),
            ("StopFailure", HookEvent::StopFailure),
            ("SubagentStart", HookEvent::SubagentStart),
            ("SubagentStop", HookEvent::SubagentStop),
            ("PreCompact", HookEvent::PreCompact),
            ("PostCompact", HookEvent::PostCompact),
            ("PermissionRequest", HookEvent::PermissionRequest),
            ("PermissionDenied", HookEvent::PermissionDenied),
            ("Setup", HookEvent::Setup),
            ("TeammateIdle", HookEvent::TeammateIdle),
            ("TaskCreated", HookEvent::TaskCreated),
            ("TaskCompleted", HookEvent::TaskCompleted),
            ("Elicitation", HookEvent::Elicitation),
            ("ElicitationResult", HookEvent::ElicitationResult),
            ("ConfigChange", HookEvent::ConfigChange),
            ("WorktreeCreate", HookEvent::WorktreeCreate),
            ("WorktreeRemove", HookEvent::WorktreeRemove),
            ("InstructionsLoaded", HookEvent::InstructionsLoaded),
            ("CwdChanged", HookEvent::CwdChanged),
            ("FileChanged", HookEvent::FileChanged),
            ("Onboarding", HookEvent::Onboarding),
        ];
        for (name, event) in table {
            assert_eq!(parse_hook_event(name), Some(event), "parse `{name}`");
            assert_eq!(event.name(), name, "name() of {event:?}");
        }
    }
}

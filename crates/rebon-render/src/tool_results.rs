//! Tool-result message projections.
//!
//! Normalizes tool results into renderable rows, preserving status,
//! truncation, and display metadata for the transcript.

/// Marker for a cancelled turn. Written into the transcript as a meta user
/// entry when a turn is cancelled, so every surface that reconstructs a
/// conversation has to recognize it rather than read it as something the
/// user typed.
pub const INTERRUPT_MESSAGE: &str = "[Request interrupted by user]";
/// Marker for a turn cancelled while a tool use was pending.
pub const INTERRUPT_MESSAGE_FOR_TOOL_USE: &str = "[Request interrupted by user for tool use]";
/// Tool result sent to the model when the user cancels a tool call.
pub const CANCEL_MESSAGE: &str = "The user doesn't want to take this action right now. STOP what you are doing and wait for the user to tell you how to proceed.";
/// Tool result sent to the model when the user rejects a tool use.
pub const REJECT_MESSAGE: &str = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.";
/// Prefix of the tool result sent when the user rejects a tool use and says
/// why; the reason follows it.
pub const REJECT_MESSAGE_WITH_REASON_PREFIX: &str = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). To tell you how to proceed, the user said:\n";
/// Prefix a rejected-plan message carries.
pub const PLAN_REJECTION_PREFIX: &str = "The agent proposed a plan that was rejected by the user. The user chose to stay in plan mode rather than proceed with implementation.\n\nRejected plan:\n";

/// The projection's outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserToolResultProjection {
    /// No matching tool-use block was found.
    MissingToolUse,
    /// User canceled the tool call.
    Canceled,
    /// Plan rejection message.
    RejectedPlan {
        /// Rejected plan content.
        plan: String,
    },
    /// Tool use rejected by the user.
    RejectedToolUse,
    /// Error branch.
    Error(UserToolErrorProjection),
    /// Success branch.
    Success(UserToolSuccessProjection),
}

/// Error-branch projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserToolErrorProjection {
    /// Interrupted by user.
    Interrupted,
    /// Plan rejection in error channel.
    RejectedPlan {
        /// Rejected plan content.
        plan: String,
    },
    /// Rejected tool use with reason.
    RejectedToolUse,
    /// Denied by classifier.
    ClassifierDenied,
    /// Fallback error renderer.
    Fallback {
        /// Raw result text.
        result: String,
        /// Verbose flag.
        verbose: bool,
    },
    /// Custom tool error renderer.
    Custom {
        /// Raw result text.
        result: String,
        /// Verbose flag.
        verbose: bool,
        /// Transcript mode flag.
        is_transcript_mode: bool,
    },
}

/// Reject-branch projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserToolRejectProjection {
    /// Fallback reject renderer.
    Fallback,
    /// Custom tool reject renderer.
    Custom {
        /// Optional summarized input.
        input_summary: Option<String>,
        /// Verbose flag.
        verbose: bool,
        /// Transcript mode flag.
        is_transcript_mode: bool,
    },
}

/// Success-branch projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserToolSuccessProjection {
    /// Tool use id.
    pub tool_use_id: String,
    /// Verbose flag.
    pub verbose: bool,
    /// Transcript mode flag.
    pub is_transcript_mode: bool,
    /// Width seam.
    pub width: String,
    /// Optional classifier rule text.
    pub classifier_rule: Option<String>,
    /// Optional yolo/classifier reason.
    pub yolo_reason: Option<String>,
    /// Whether the result should render as assistant text.
    pub renders_as_assistant_text: bool,
}

/// Input seam for the dispatcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserToolResultInput {
    /// Tool use id.
    pub tool_use_id: String,
    /// Raw tool result content.
    pub content: String,
    /// Error flag from `param.is_error`.
    pub is_error: bool,
    /// Whether the referenced tool use exists.
    pub tool_exists: bool,
    /// Whether the tool exposes a custom reject renderer.
    pub tool_has_custom_reject_renderer: bool,
    /// Whether the tool exposes a custom error renderer.
    pub tool_has_custom_error_renderer: bool,
    /// Whether the success branch should render as assistant text.
    pub renders_as_assistant_text: bool,
    /// Optional summarized input.
    pub input_summary: Option<String>,
    /// Verbose flag.
    pub verbose: bool,
    /// Transcript mode flag.
    pub is_transcript_mode: bool,
    /// Width seam.
    pub width: String,
    /// Optional classifier rule.
    pub classifier_rule: Option<String>,
    /// Optional yolo reason.
    pub yolo_reason: Option<String>,
    /// Whether the content is a classifier denial.
    pub classifier_denial: bool,
}

/// Pick the branch a user tool-result row renders: missing tool use,
/// cancelled, rejected, error, or success.
pub fn project_user_tool_result(input: &UserToolResultInput) -> UserToolResultProjection {
    if !input.tool_exists {
        return UserToolResultProjection::MissingToolUse;
    }
    if input.content.starts_with(CANCEL_MESSAGE) {
        return UserToolResultProjection::Canceled;
    }
    if (input.content.starts_with(REJECT_MESSAGE)
        || input.content == INTERRUPT_MESSAGE_FOR_TOOL_USE)
        && !input.is_error
    {
        return UserToolResultProjection::RejectedToolUse;
    }
    if input.is_error {
        return UserToolResultProjection::Error(project_user_tool_error(input));
    }
    UserToolResultProjection::Success(UserToolSuccessProjection {
        tool_use_id: input.tool_use_id.clone(),
        verbose: input.verbose,
        is_transcript_mode: input.is_transcript_mode,
        width: input.width.clone(),
        classifier_rule: input.classifier_rule.clone(),
        yolo_reason: input.yolo_reason.clone(),
        renders_as_assistant_text: input.renders_as_assistant_text,
    })
}

/// Pick the branch an errored tool result renders.
pub fn project_user_tool_error(input: &UserToolResultInput) -> UserToolErrorProjection {
    if input.content.contains(INTERRUPT_MESSAGE_FOR_TOOL_USE) {
        return UserToolErrorProjection::Interrupted;
    }
    if let Some(plan) = input.content.strip_prefix(PLAN_REJECTION_PREFIX) {
        return UserToolErrorProjection::RejectedPlan {
            plan: plan.to_string(),
        };
    }
    if input.content.starts_with(REJECT_MESSAGE_WITH_REASON_PREFIX) {
        return UserToolErrorProjection::RejectedToolUse;
    }
    if input.classifier_denial {
        return UserToolErrorProjection::ClassifierDenied;
    }
    if input.tool_has_custom_error_renderer {
        return UserToolErrorProjection::Custom {
            result: input.content.clone(),
            verbose: input.verbose,
            is_transcript_mode: input.is_transcript_mode,
        };
    }
    UserToolErrorProjection::Fallback {
        result: input.content.clone(),
        verbose: input.verbose,
    }
}

/// Pick the branch a rejected tool use renders: the tool's own reject
/// renderer when it has one, otherwise the fallback.
pub fn project_user_tool_reject(input: &UserToolResultInput) -> UserToolRejectProjection {
    if input.tool_has_custom_reject_renderer {
        UserToolRejectProjection::Custom {
            input_summary: input.input_summary.clone(),
            verbose: input.verbose,
            is_transcript_mode: input.is_transcript_mode,
        }
    } else {
        UserToolRejectProjection::Fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> UserToolResultInput {
        UserToolResultInput {
            tool_use_id: "u1".into(),
            content: "ok".into(),
            is_error: false,
            tool_exists: true,
            tool_has_custom_reject_renderer: false,
            tool_has_custom_error_renderer: false,
            renders_as_assistant_text: false,
            input_summary: Some("path=/tmp/x".into()),
            verbose: false,
            is_transcript_mode: false,
            width: "100".into(),
            classifier_rule: None,
            yolo_reason: None,
            classifier_denial: false,
        }
    }

    #[test]
    fn top_level_dispatch_handles_missing_cancel_reject_and_success() {
        let mut i = input();
        i.tool_exists = false;
        assert_eq!(
            project_user_tool_result(&i),
            UserToolResultProjection::MissingToolUse
        );

        let mut i = input();
        i.content = CANCEL_MESSAGE.into();
        assert_eq!(
            project_user_tool_result(&i),
            UserToolResultProjection::Canceled
        );

        let mut i = input();
        i.content = REJECT_MESSAGE.into();
        assert_eq!(
            project_user_tool_result(&i),
            UserToolResultProjection::RejectedToolUse
        );

        let i = input();
        assert!(matches!(
            project_user_tool_result(&i),
            UserToolResultProjection::Success(_)
        ));
    }

    #[test]
    fn error_projection_prioritizes_interrupt_plan_reject_classifier() {
        let mut i = input();
        i.is_error = true;
        i.content = INTERRUPT_MESSAGE_FOR_TOOL_USE.into();
        assert_eq!(
            project_user_tool_error(&i),
            UserToolErrorProjection::Interrupted
        );

        let mut i = input();
        i.is_error = true;
        i.content = format!("{PLAN_REJECTION_PREFIX}plan");
        assert_eq!(
            project_user_tool_error(&i),
            UserToolErrorProjection::RejectedPlan {
                plan: "plan".into()
            }
        );

        let mut i = input();
        i.is_error = true;
        i.content = REJECT_MESSAGE_WITH_REASON_PREFIX.into();
        assert_eq!(
            project_user_tool_error(&i),
            UserToolErrorProjection::RejectedToolUse
        );

        let mut i = input();
        i.is_error = true;
        i.classifier_denial = true;
        assert_eq!(
            project_user_tool_error(&i),
            UserToolErrorProjection::ClassifierDenied
        );
    }

    #[test]
    fn error_projection_uses_custom_or_fallback_renderer() {
        let mut i = input();
        i.is_error = true;
        i.tool_has_custom_error_renderer = true;
        assert!(matches!(
            project_user_tool_error(&i),
            UserToolErrorProjection::Custom { .. }
        ));

        let mut i = input();
        i.is_error = true;
        assert!(matches!(
            project_user_tool_error(&i),
            UserToolErrorProjection::Fallback { .. }
        ));
    }

    #[test]
    fn reject_projection_uses_custom_or_fallback_renderer() {
        let mut i = input();
        i.tool_has_custom_reject_renderer = true;
        assert!(matches!(
            project_user_tool_reject(&i),
            UserToolRejectProjection::Custom { .. }
        ));

        let i = input();
        assert_eq!(
            project_user_tool_reject(&i),
            UserToolRejectProjection::Fallback
        );
    }

    #[test]
    fn success_projection_threads_classifier_and_width_fields() {
        let mut i = input();
        i.classifier_rule = Some("rule".into());
        i.yolo_reason = Some("classifier".into());
        i.renders_as_assistant_text = true;
        let UserToolResultProjection::Success(s) = project_user_tool_result(&i) else {
            panic!("expected success");
        };
        assert_eq!(s.classifier_rule.as_deref(), Some("rule"));
        assert_eq!(s.yolo_reason.as_deref(), Some("classifier"));
        assert!(s.renders_as_assistant_text);
        assert_eq!(s.width, "100");
    }
}

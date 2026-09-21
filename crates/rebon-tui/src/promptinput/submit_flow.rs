//! The submit-path decisions.
//!
//! The async side effects (sending the direct member message, the leader and
//! agent submit calls, `AppState` mutation, notification emission) remain
//! caller owned. This module only resolves the deterministic decision tree
//! around
//! prompt-suggestion acceptance, speculation short-circuiting, direct-message
//! parsing, submit blocking, and leader-vs-agent routing.

/// Minimal prompt-suggestion state read by the submit flow.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptSuggestionState {
    /// Suggested text when present.
    pub text: Option<String>,
    /// Non-zero once the suggestion has actually been shown to the user.
    pub shown_at: u64,
}

/// Parsed `@agent message` direct-message shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectMemberMessage {
    /// Recipient agent name after the leading `@`.
    pub recipient_name: String,
    /// Trimmed message text.
    pub message: String,
}

/// Submission destination after all blocking/precheck logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitRoute {
    /// Normal leader submission.
    Leader,
    /// Route to the active teammate / local agent.
    ActiveAgent,
}

/// Reason a submit attempt is blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitBlockReason {
    /// A visible footer pill still owns Enter.
    FooterSelectionVisible,
    /// Agent selection mode owns Enter.
    SelectingAgentMode,
    /// No text and no images attached.
    EmptyWithoutImages,
    /// Non-directory suggestions are still open.
    SuggestionsVisible,
}

/// Inputs for the first submit-preparation phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitPreparationInput {
    /// Raw input as typed.
    pub raw_input: String,
    /// Whether a still-visible footer selection exists.
    pub footer_selection_visible: bool,
    /// Whether the view selection mode is `selecting-agent`.
    pub selecting_agent_mode: bool,
    /// Whether any pasted content is an image.
    pub has_images: bool,
    /// Current prompt-suggestion state.
    pub prompt_suggestion_state: PromptSuggestionState,
    /// Whether we are currently viewing a teammate task.
    pub viewing_agent_task_id_present: bool,
    /// Whether speculative submission is active.
    pub speculation_active: bool,
    /// Whether direct member messaging is enabled.
    pub agent_swarms_enabled: bool,
}

/// Result of the preparation phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitPreparation {
    /// Submit is blocked before any side effects should run.
    Blocked(SubmitBlockReason),
    /// Speculation should be accepted immediately, bypassing the normal query.
    AcceptSpeculation {
        /// Text to submit instead of the raw input.
        submit_text: String,
    },
    /// Normal prepared submit state that the caller can continue with.
    Prepared(PreparedSubmit),
}

/// Prepared submit state after trim/suggestion/direct-message precheck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSubmit {
    /// Final text after trimming and optional suggestion acceptance.
    pub submit_text: String,
    /// Whether image attachments are present.
    pub has_images: bool,
    /// Whether the prompt suggestion existed at all.
    pub prompt_suggestion_text_present: bool,
    /// Whether that suggestion had already been shown.
    pub prompt_suggestion_was_shown: bool,
    /// Whether this submit accepted the prompt suggestion.
    pub prompt_suggestion_accepted: bool,
    /// Parsed direct message candidate, if any.
    pub direct_message: Option<DirectMemberMessage>,
}

/// Inputs for the second-phase submit finalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizeSubmitInput {
    /// Prepared state from `prepare_submit`.
    pub prepared: PreparedSubmit,
    /// Whether a slash command is currently being submitted.
    pub is_submitting_slash_command: bool,
    /// Number of currently visible typeahead suggestions.
    pub suggestion_count: usize,
    /// True when every suggestion is a directory suggestion.
    pub has_only_directory_suggestions: bool,
    /// Final target route after checking the active agent.
    pub route: SubmitRoute,
}

/// Finalized submit action after all blocking logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedSubmit {
    /// Leader or active-agent destination.
    pub route: SubmitRoute,
    /// Final text to submit.
    pub submit_text: String,
    /// Whether the prompt-suggestion outcome should be logged.
    pub should_log_prompt_suggestion_outcome: bool,
    /// Whether the stash hint should be cleared.
    pub should_clear_stash_hint: bool,
    /// Whether this path accepted the prompt suggestion.
    pub prompt_suggestion_accepted: bool,
}

/// Result of the finalize step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeSubmitResult {
    /// Submit is blocked.
    Blocked(SubmitBlockReason),
    /// Submit should proceed.
    Submit(FinalizedSubmit),
}

/// Parse a leading `@agent message` into its two parts.
pub fn parse_direct_member_message(input: &str) -> Option<DirectMemberMessage> {
    let rest = input.strip_prefix('@')?;
    let split_at = rest.find(char::is_whitespace)?;
    let recipient_name = &rest[..split_at];
    if recipient_name.is_empty()
        || !recipient_name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    let message = rest[split_at..].trim();
    if message.is_empty() {
        return None;
    }
    Some(DirectMemberMessage {
        recipient_name: recipient_name.to_string(),
        message: message.to_string(),
    })
}

/// Resolve everything up to and including direct-message parsing.
pub fn prepare_submit(input: &SubmitPreparationInput) -> SubmitPreparation {
    let mut submit_text = input.raw_input.trim_end().to_string();

    if input.footer_selection_visible {
        return SubmitPreparation::Blocked(SubmitBlockReason::FooterSelectionVisible);
    }
    if input.selecting_agent_mode {
        return SubmitPreparation::Blocked(SubmitBlockReason::SelectingAgentMode);
    }

    let suggestion_text = input.prompt_suggestion_state.text.clone();
    let suggestion_text_present = suggestion_text.is_some();
    let suggestion_was_shown = input.prompt_suggestion_state.shown_at > 0;
    let input_matches_suggestion = submit_text.trim().is_empty()
        || suggestion_text
            .as_deref()
            .is_some_and(|suggestion| submit_text == suggestion);
    let mut prompt_suggestion_accepted = false;

    if !input.has_images && !input.viewing_agent_task_id_present {
        if let Some(suggestion_text) = suggestion_text {
            if input_matches_suggestion {
                if input.speculation_active {
                    return SubmitPreparation::AcceptSpeculation {
                        submit_text: suggestion_text,
                    };
                }
                if suggestion_was_shown {
                    prompt_suggestion_accepted = true;
                    submit_text = suggestion_text;
                }
            }
        }
    }

    let direct_message = input
        .agent_swarms_enabled
        .then(|| parse_direct_member_message(&submit_text))
        .flatten();

    SubmitPreparation::Prepared(PreparedSubmit {
        submit_text,
        has_images: input.has_images,
        prompt_suggestion_text_present: suggestion_text_present,
        prompt_suggestion_was_shown: suggestion_was_shown,
        prompt_suggestion_accepted,
        direct_message,
    })
}

/// Resolve the blocking checks after direct-message parsing.
pub fn finalize_submit(input: &FinalizeSubmitInput) -> FinalizeSubmitResult {
    if input.prepared.submit_text.trim().is_empty() && !input.prepared.has_images {
        return FinalizeSubmitResult::Blocked(SubmitBlockReason::EmptyWithoutImages);
    }

    if input.suggestion_count > 0
        && !input.is_submitting_slash_command
        && !input.has_only_directory_suggestions
    {
        return FinalizeSubmitResult::Blocked(SubmitBlockReason::SuggestionsVisible);
    }

    FinalizeSubmitResult::Submit(FinalizedSubmit {
        route: input.route,
        submit_text: input.prepared.submit_text.clone(),
        should_log_prompt_suggestion_outcome: input.prepared.prompt_suggestion_text_present
            && input.prepared.prompt_suggestion_was_shown,
        should_clear_stash_hint: true,
        prompt_suggestion_accepted: input.prepared.prompt_suggestion_accepted,
    })
}

/// Whether the prompt suggestion should be shown at all.
pub fn should_show_prompt_suggestion(
    mode: &str,
    suggestion_count: usize,
    prompt_suggestion_present: bool,
    viewing_agent_task_id_present: bool,
) -> bool {
    mode == "prompt"
        && suggestion_count == 0
        && prompt_suggestion_present
        && !viewing_agent_task_id_present
}

/// Whether the prompt suggestion's timing suppression should reset.
pub fn should_reset_prompt_suggestion_for_timing(
    prompt_suggestion_text_present: bool,
    prompt_suggestion_renderable: bool,
    shown_at: u64,
    viewing_agent_task_id_present: bool,
) -> bool {
    prompt_suggestion_text_present
        && !prompt_suggestion_renderable
        && shown_at == 0
        && !viewing_agent_task_id_present
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_preparation() -> SubmitPreparationInput {
        SubmitPreparationInput {
            raw_input: String::from("hello"),
            footer_selection_visible: false,
            selecting_agent_mode: false,
            has_images: false,
            prompt_suggestion_state: PromptSuggestionState::default(),
            viewing_agent_task_id_present: false,
            speculation_active: false,
            agent_swarms_enabled: false,
        }
    }

    #[test]
    fn parse_direct_member_message_rules() {
        assert_eq!(
            parse_direct_member_message("@alice hi there"),
            Some(DirectMemberMessage {
                recipient_name: String::from("alice"),
                message: String::from("hi there"),
            })
        );
        assert_eq!(parse_direct_member_message("@alice   "), None);
        assert_eq!(parse_direct_member_message("alice hi"), None);
        assert_eq!(parse_direct_member_message("@ali.ce hi"), None);
    }

    #[test]
    fn prepare_submit_blocks_visible_footer_and_selection_mode() {
        let mut footer = base_preparation();
        footer.footer_selection_visible = true;
        assert_eq!(
            prepare_submit(&footer),
            SubmitPreparation::Blocked(SubmitBlockReason::FooterSelectionVisible)
        );

        let mut selecting = base_preparation();
        selecting.selecting_agent_mode = true;
        assert_eq!(
            prepare_submit(&selecting),
            SubmitPreparation::Blocked(SubmitBlockReason::SelectingAgentMode)
        );
    }

    #[test]
    fn prepare_submit_accepts_speculation_before_normal_submit() {
        let mut input = base_preparation();
        input.raw_input = String::from("");
        input.prompt_suggestion_state = PromptSuggestionState {
            text: Some(String::from("suggest this")),
            shown_at: 123,
        };
        input.speculation_active = true;

        assert_eq!(
            prepare_submit(&input),
            SubmitPreparation::AcceptSpeculation {
                submit_text: String::from("suggest this"),
            }
        );
    }

    #[test]
    fn prepare_submit_accepts_shown_prompt_suggestion_for_normal_path() {
        let mut input = base_preparation();
        input.raw_input = String::from("   ");
        input.prompt_suggestion_state = PromptSuggestionState {
            text: Some(String::from("suggest this")),
            shown_at: 123,
        };

        let prepared = match prepare_submit(&input) {
            SubmitPreparation::Prepared(prepared) => prepared,
            other => panic!("expected prepared submit, got {other:?}"),
        };
        assert_eq!(prepared.submit_text, "suggest this");
        assert!(prepared.prompt_suggestion_accepted);
    }

    #[test]
    fn prepare_submit_does_not_autoaccept_with_images_or_teammate_view() {
        let mut with_images = base_preparation();
        with_images.raw_input = String::from("");
        with_images.has_images = true;
        with_images.prompt_suggestion_state = PromptSuggestionState {
            text: Some(String::from("suggest this")),
            shown_at: 123,
        };
        let prepared = match prepare_submit(&with_images) {
            SubmitPreparation::Prepared(prepared) => prepared,
            other => panic!("expected prepared submit, got {other:?}"),
        };
        assert_eq!(prepared.submit_text, "");
        assert!(!prepared.prompt_suggestion_accepted);

        let mut teammate_view = base_preparation();
        teammate_view.raw_input = String::from("");
        teammate_view.viewing_agent_task_id_present = true;
        teammate_view.prompt_suggestion_state = PromptSuggestionState {
            text: Some(String::from("suggest this")),
            shown_at: 123,
        };
        let prepared = match prepare_submit(&teammate_view) {
            SubmitPreparation::Prepared(prepared) => prepared,
            other => panic!("expected prepared submit, got {other:?}"),
        };
        assert_eq!(prepared.submit_text, "");
        assert!(!prepared.prompt_suggestion_accepted);
    }

    #[test]
    fn prepare_submit_trims_end_and_parses_direct_message_when_enabled() {
        let mut input = base_preparation();
        input.raw_input = String::from("@alice hi there   ");
        input.agent_swarms_enabled = true;

        let prepared = match prepare_submit(&input) {
            SubmitPreparation::Prepared(prepared) => prepared,
            other => panic!("expected prepared submit, got {other:?}"),
        };
        assert_eq!(prepared.submit_text, "@alice hi there");
        assert_eq!(
            prepared.direct_message,
            Some(DirectMemberMessage {
                recipient_name: String::from("alice"),
                message: String::from("hi there"),
            })
        );
    }

    #[test]
    fn finalize_submit_blocks_empty_without_images() {
        let result = finalize_submit(&FinalizeSubmitInput {
            prepared: PreparedSubmit {
                submit_text: String::new(),
                has_images: false,
                prompt_suggestion_text_present: false,
                prompt_suggestion_was_shown: false,
                prompt_suggestion_accepted: false,
                direct_message: None,
            },
            is_submitting_slash_command: false,
            suggestion_count: 0,
            has_only_directory_suggestions: false,
            route: SubmitRoute::Leader,
        });
        assert_eq!(
            result,
            FinalizeSubmitResult::Blocked(SubmitBlockReason::EmptyWithoutImages)
        );
    }

    #[test]
    fn finalize_submit_allows_image_only_submission() {
        let result = finalize_submit(&FinalizeSubmitInput {
            prepared: PreparedSubmit {
                submit_text: String::new(),
                has_images: true,
                prompt_suggestion_text_present: false,
                prompt_suggestion_was_shown: false,
                prompt_suggestion_accepted: false,
                direct_message: None,
            },
            is_submitting_slash_command: false,
            suggestion_count: 0,
            has_only_directory_suggestions: false,
            route: SubmitRoute::Leader,
        });
        assert!(matches!(result, FinalizeSubmitResult::Submit(_)));
    }

    #[test]
    fn finalize_submit_blocks_non_directory_suggestions_but_allows_directory_and_slash_submit() {
        let prepared = PreparedSubmit {
            submit_text: String::from("hello"),
            has_images: false,
            prompt_suggestion_text_present: false,
            prompt_suggestion_was_shown: false,
            prompt_suggestion_accepted: false,
            direct_message: None,
        };

        assert_eq!(
            finalize_submit(&FinalizeSubmitInput {
                prepared: prepared.clone(),
                is_submitting_slash_command: false,
                suggestion_count: 2,
                has_only_directory_suggestions: false,
                route: SubmitRoute::Leader,
            }),
            FinalizeSubmitResult::Blocked(SubmitBlockReason::SuggestionsVisible)
        );

        assert!(matches!(
            finalize_submit(&FinalizeSubmitInput {
                prepared: prepared.clone(),
                is_submitting_slash_command: false,
                suggestion_count: 2,
                has_only_directory_suggestions: true,
                route: SubmitRoute::Leader,
            }),
            FinalizeSubmitResult::Submit(_)
        ));

        assert!(matches!(
            finalize_submit(&FinalizeSubmitInput {
                prepared,
                is_submitting_slash_command: true,
                suggestion_count: 2,
                has_only_directory_suggestions: false,
                route: SubmitRoute::Leader,
            }),
            FinalizeSubmitResult::Submit(_)
        ));
    }

    #[test]
    fn finalize_submit_routes_to_leader_or_agent_and_logs_prompt_outcome_when_shown() {
        let prepared = PreparedSubmit {
            submit_text: String::from("hello"),
            has_images: false,
            prompt_suggestion_text_present: true,
            prompt_suggestion_was_shown: true,
            prompt_suggestion_accepted: true,
            direct_message: None,
        };

        let leader = finalize_submit(&FinalizeSubmitInput {
            prepared: prepared.clone(),
            is_submitting_slash_command: false,
            suggestion_count: 0,
            has_only_directory_suggestions: false,
            route: SubmitRoute::Leader,
        });
        let agent = finalize_submit(&FinalizeSubmitInput {
            prepared,
            is_submitting_slash_command: false,
            suggestion_count: 0,
            has_only_directory_suggestions: false,
            route: SubmitRoute::ActiveAgent,
        });

        let FinalizeSubmitResult::Submit(leader) = leader else {
            panic!("expected leader submit");
        };
        assert_eq!(leader.route, SubmitRoute::Leader);
        assert!(leader.should_log_prompt_suggestion_outcome);
        assert!(leader.should_clear_stash_hint);
        assert!(leader.prompt_suggestion_accepted);

        let FinalizeSubmitResult::Submit(agent) = agent else {
            panic!("expected agent submit");
        };
        assert_eq!(agent.route, SubmitRoute::ActiveAgent);
    }

    #[test]
    fn prompt_suggestion_visibility_and_timing_reset_gates() {
        assert!(should_show_prompt_suggestion("prompt", 0, true, false));
        assert!(!should_show_prompt_suggestion("bash", 0, true, false));
        assert!(!should_show_prompt_suggestion("prompt", 1, true, false));
        assert!(!should_show_prompt_suggestion("prompt", 0, true, true));

        assert!(should_reset_prompt_suggestion_for_timing(
            true, false, 0, false
        ));
        assert!(!should_reset_prompt_suggestion_for_timing(
            true, true, 0, false
        ));
        assert!(!should_reset_prompt_suggestion_for_timing(
            true, false, 1, false
        ));
        assert!(!should_reset_prompt_suggestion_for_timing(
            true, false, 0, true
        ));
    }
}

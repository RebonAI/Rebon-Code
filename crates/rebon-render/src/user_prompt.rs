//! Projection for the user prompt row: the brief-layout gate, head+tail
//! truncation of long prompts, and the background choice.
//!
//! The locale-aware timestamp shown by the highlighted-text child
//! ([`project_highlighted_thinking_text`]) stays injected as preformatted
//! text.

use crate::thinking::{
    project_highlighted_thinking_text, HighlightedThinkingTextInput,
    HighlightedThinkingTextProjection,
};

/// Hard cap on displayed prompt text.
pub const MAX_DISPLAY_CHARS: usize = 10_000;

/// Number of leading chars preserved after truncation.
pub const TRUNCATE_HEAD_CHARS: usize = 2_500;

/// Number of trailing chars preserved after truncation.
pub const TRUNCATE_TAIL_CHARS: usize = 2_500;

/// Background choices for the user prompt row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserPromptBackground {
    /// The `messageActionsBackground` theme color, used while selected.
    MessageActionsBackground,
    /// The `userMessageBackground` theme color.
    UserMessageBackground,
}

/// Input for [`project_user_prompt_message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPromptMessageInput {
    /// Whether the standard top margin is added above the row.
    pub add_margin: bool,
    /// Raw prompt text.
    pub text: String,
    /// Whether the transcript view is showing.
    pub is_transcript_mode: bool,
    /// Preformatted timestamp label used in brief mode.
    pub timestamp_label: Option<String>,
    /// Whether this build offers assistant mode and its brief layout at
    /// all. When `false` the brief layout is never selected.
    pub assistant_mode_enabled: bool,
    /// Whether assistant mode is active, which selects the brief layout
    /// without a user opt-in.
    pub assistant_mode_active: bool,
    /// Whether the user opted in to the brief layout. Only counts together
    /// with `brief_env_enabled` or `brief_flag_enabled`.
    pub user_message_opt_in: bool,
    /// Brief-mode environment gate, resolved by the caller.
    pub brief_env_enabled: bool,
    /// Brief-mode feature flag, resolved by the caller.
    pub brief_flag_enabled: bool,
    /// Whether the app state has the brief-only view on.
    pub is_brief_only: bool,
    /// Whether an agent task's transcript is being viewed.
    pub viewing_agent_task: bool,
    /// Message-actions selection state.
    pub is_selected: bool,
    /// Queued-message context passed through to the child.
    pub is_queued: bool,
    /// Runtime ultrathink gate passed through to the child.
    pub ultrathink_enabled: bool,
}

/// Pure display projection for the user prompt row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPromptMessageDisplay {
    /// Top margin on the outer box.
    pub margin_top: u8,
    /// Background color, if any.
    pub background: Option<UserPromptBackground>,
    /// Right padding on the outer box.
    pub padding_right: u8,
    /// Whether the brief layout was selected.
    pub use_brief_layout: bool,
    /// Projection of the highlighted prompt text.
    pub highlighted_text: HighlightedThinkingTextProjection,
}

/// Whether the prompt row uses the brief layout: the build offers it,
/// assistant mode or a gated user opt-in selects it, the brief-only view is
/// on, and neither the transcript nor an agent task is being viewed.
pub fn should_use_brief_layout(input: &UserPromptMessageInput) -> bool {
    if !input.assistant_mode_enabled {
        return false;
    }

    let brief_opt_in =
        input.user_message_opt_in && (input.brief_env_enabled || input.brief_flag_enabled);

    (input.assistant_mode_active || brief_opt_in)
        && input.is_brief_only
        && !input.is_transcript_mode
        && !input.viewing_agent_task
}

/// Truncate a prompt longer than [`MAX_DISPLAY_CHARS`] to its head and
/// tail, with a hidden-line count between them.
pub fn truncate_user_prompt_text(text: &str) -> String {
    if text.chars().count() <= MAX_DISPLAY_CHARS {
        return text.to_string();
    }

    let head = prefix_chars(text, TRUNCATE_HEAD_CHARS);
    let tail = suffix_chars(text, TRUNCATE_TAIL_CHARS);
    let hidden_lines = count_char_from_char_index(text, '\n', TRUNCATE_HEAD_CHARS)
        .saturating_sub(tail.chars().filter(|ch| *ch == '\n').count());

    format!("{head}\n\u{2026} +{hidden_lines} lines \u{2026}\n{tail}")
}

/// Project the user prompt row; `None` for empty text.
pub fn project_user_prompt_message(
    input: &UserPromptMessageInput,
) -> Option<UserPromptMessageDisplay> {
    if input.text.is_empty() {
        return None;
    }

    let use_brief_layout = should_use_brief_layout(input);
    let display_text = truncate_user_prompt_text(&input.text);

    Some(UserPromptMessageDisplay {
        margin_top: u8::from(input.add_margin),
        background: if input.is_selected {
            Some(UserPromptBackground::MessageActionsBackground)
        } else if use_brief_layout {
            None
        } else {
            Some(UserPromptBackground::UserMessageBackground)
        },
        padding_right: if use_brief_layout { 0 } else { 1 },
        use_brief_layout,
        highlighted_text: project_highlighted_thinking_text(&HighlightedThinkingTextInput {
            text: display_text,
            use_brief_layout,
            formatted_timestamp: if use_brief_layout {
                input.timestamp_label.clone()
            } else {
                None
            },
            is_queued: input.is_queued,
            is_selected: input.is_selected,
            ultrathink_enabled: input.ultrathink_enabled,
        }),
    })
}

fn prefix_chars(text: &str, char_count: usize) -> &str {
    let end = byte_index_at_char(text, char_count);
    &text[..end]
}

fn suffix_chars(text: &str, char_count: usize) -> &str {
    let total = text.chars().count();
    let start = byte_index_at_char(text, total.saturating_sub(char_count));
    &text[start..]
}

fn count_char_from_char_index(text: &str, needle: char, start_char: usize) -> usize {
    let start = byte_index_at_char(text, start_char);
    text[start..].chars().filter(|ch| *ch == needle).count()
}

fn byte_index_at_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thinking::{BriefThinkingDisplay, HighlightedThinkingTextProjection};

    fn input(text: &str) -> UserPromptMessageInput {
        UserPromptMessageInput {
            add_margin: true,
            text: text.to_string(),
            is_transcript_mode: false,
            timestamp_label: Some("1:30 PM".into()),
            assistant_mode_enabled: true,
            assistant_mode_active: false,
            user_message_opt_in: true,
            brief_env_enabled: false,
            brief_flag_enabled: true,
            is_brief_only: true,
            viewing_agent_task: false,
            is_selected: false,
            is_queued: false,
            ultrathink_enabled: true,
        }
    }

    #[test]
    fn brief_layout_requires_build_gate_and_runtime_conditions() {
        let mut value = input("prompt");
        assert!(should_use_brief_layout(&value));

        value.assistant_mode_enabled = false;
        assert!(!should_use_brief_layout(&value));

        let mut transcript = input("prompt");
        transcript.is_transcript_mode = true;
        assert!(!should_use_brief_layout(&transcript));

        let mut viewing_task = input("prompt");
        viewing_task.viewing_agent_task = true;
        assert!(!should_use_brief_layout(&viewing_task));
    }

    #[test]
    fn truncation_keeps_head_tail_and_hidden_line_count() {
        let text = format!(
            "{}\n{}\n{}",
            "a".repeat(TRUNCATE_HEAD_CHARS),
            "middle\n".repeat(1000),
            "z".repeat(TRUNCATE_TAIL_CHARS)
        );

        let truncated = truncate_user_prompt_text(&text);

        assert!(truncated.starts_with(&"a".repeat(TRUNCATE_HEAD_CHARS)));
        assert!(truncated.ends_with(&"z".repeat(TRUNCATE_TAIL_CHARS)));
        assert!(truncated.contains("+1002 lines"));
    }

    #[test]
    fn short_text_is_not_truncated() {
        assert_eq!(truncate_user_prompt_text("hello"), "hello");
    }

    #[test]
    fn projection_returns_none_for_missing_text() {
        assert_eq!(project_user_prompt_message(&input("")), None);
    }

    #[test]
    fn projection_uses_brief_layout_background_rules() {
        let projection = project_user_prompt_message(&input("prompt")).unwrap();
        assert_eq!(projection.margin_top, 1);
        assert_eq!(projection.background, None);
        assert_eq!(projection.padding_right, 0);
        assert!(projection.use_brief_layout);

        let HighlightedThinkingTextProjection::Brief(BriefThinkingDisplay { label, .. }) =
            projection.highlighted_text
        else {
            panic!("expected brief child projection");
        };
        assert_eq!(label, "You");
    }

    #[test]
    fn selection_forces_message_actions_background_even_in_brief_mode() {
        let mut value = input("prompt");
        value.is_selected = true;
        let projection = project_user_prompt_message(&value).unwrap();
        assert_eq!(
            projection.background,
            Some(UserPromptBackground::MessageActionsBackground)
        );
    }
}

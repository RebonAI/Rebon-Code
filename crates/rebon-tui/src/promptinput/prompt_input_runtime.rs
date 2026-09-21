//! Composes the prompt-surface, suggestion, placeholder, and text-input-view
//! helpers into one derived-state cluster: the displayed value, whether the
//! suggestion is shown, the placeholder, the chip positions, whether the
//! cursor sits on a chip, the combined highlights, and the final text-input
//! view booleans and strings.

use crate::promptinput::prompt_surface::{
    build_prompt_highlights, extract_all_ref_positions, is_cursor_at_image_chip,
    snap_cursor_out_of_image_chip, PromptHighlightInput, PromptSurfaceHighlight, TextRange,
    ThemeHighlightRange,
};
use crate::promptinput::submit_flow::{
    should_reset_prompt_suggestion_for_timing, should_show_prompt_suggestion, PromptSuggestionState,
};
use crate::promptinput::text_input_view::{
    build_text_input_view_state, TextInputViewInput, TextInputViewState,
};

/// Inputs for the prompt-surface and text-input derived state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptInputRuntimeInput {
    /// Current prompt mode.
    pub mode: String,
    /// Current raw input.
    pub input: String,
    /// Optional history-match display string.
    pub history_match_display: Option<String>,
    /// Whether history search is active.
    pub is_searching_history: bool,
    /// Whether a modal overlay is active.
    pub is_modal_overlay_active: bool,
    /// Whether a footer pill is selected.
    pub footer_item_selected: bool,
    /// Current cursor offset.
    pub cursor_offset: usize,
    /// Visible typeahead suggestion count.
    pub suggestion_count: usize,
    /// Default placeholder string.
    pub default_placeholder: Option<String>,
    /// Renderable prompt suggestion text, when there is one.
    pub prompt_suggestion: Option<String>,
    /// AppState-backed prompt suggestion state.
    pub prompt_suggestion_state: PromptSuggestionState,
    /// Whether a teammate transcript is currently being viewed.
    pub viewing_agent_task_id_present: bool,
    /// Whether undo is available.
    pub can_undo: bool,
    /// Highlight-composition inputs from trigger finders.
    pub history_failed_match: bool,
    /// Length of the history search query.
    pub history_query_length: usize,
    /// `btw` trigger ranges.
    pub btw_triggers: Vec<TextRange>,
    /// Slash command trigger ranges.
    pub slash_command_triggers: Vec<TextRange>,
    /// Token budget trigger ranges.
    pub token_budget_triggers: Vec<TextRange>,
    /// Slack channel trigger ranges.
    pub slack_channel_triggers: Vec<TextRange>,
    /// Mention highlight ranges with colors.
    pub member_mention_highlights: Vec<ThemeHighlightRange>,
    /// Voice interim range.
    pub voice_interim_range: Option<TextRange>,
    /// Ultrathink trigger ranges.
    pub think_triggers: Vec<TextRange>,
    /// Ultraplan trigger ranges.
    pub ultraplan_triggers: Vec<TextRange>,
    /// Ultrareview trigger ranges.
    pub ultrareview_triggers: Vec<TextRange>,
    /// Whether ultrathink rainbow highlighting is enabled.
    pub ultrathink_enabled: bool,
    /// Whether ultraplan highlighting is enabled.
    pub ultraplan_enabled: bool,
}

/// Derived prompt-input state after composing the helper modules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptInputRuntimeState {
    /// Final displayed value.
    pub displayed_value: String,
    /// Whether prompt suggestion should be shown in the placeholder slot.
    pub show_prompt_suggestion: bool,
    /// Whether the suggestion should be marked as shown.
    pub should_mark_prompt_suggestion_shown: bool,
    /// Whether the timing suppression branch should reset AppState prompt suggestion.
    pub should_reset_prompt_suggestion_for_timing: bool,
    /// Parsed chip positions over the displayed value.
    pub image_ref_positions: Vec<TextRange>,
    /// Whether the cursor is exactly at an image chip start.
    pub cursor_at_image_chip: bool,
    /// Optional snapped cursor offset when currently inside an image chip.
    pub snapped_cursor_offset: Option<usize>,
    /// Final combined text highlights.
    pub highlights: Vec<PromptSurfaceHighlight>,
    /// Final text-input view booleans and strings.
    pub text_input_view: TextInputViewState,
}

/// Derive the full prompt-input state: the displayed value, the prompt
/// suggestion, the chip state, the combined highlights, and the text-input
/// view.
pub fn derive_prompt_input_runtime_state(
    input: &PromptInputRuntimeInput,
    get_rainbow_color: impl Fn(usize, usize, bool) -> String,
) -> PromptInputRuntimeState {
    let text_input_view = build_text_input_view_state(&TextInputViewInput {
        input: input.input.clone(),
        history_match_display: input.history_match_display.clone(),
        is_searching_history: input.is_searching_history,
        is_modal_overlay_active: input.is_modal_overlay_active,
        footer_item_selected: input.footer_item_selected,
        suggestion_count: input.suggestion_count,
        cursor_at_image_chip: false,
        default_placeholder: input.default_placeholder.clone(),
        show_prompt_suggestion: false,
        prompt_suggestion: input.prompt_suggestion.clone(),
        can_undo: input.can_undo,
    });
    let displayed_value = text_input_view.displayed_value.clone();

    let show_prompt_suggestion = should_show_prompt_suggestion(
        &input.mode,
        input.suggestion_count,
        input.prompt_suggestion.is_some(),
        input.viewing_agent_task_id_present,
    );
    let should_reset_prompt_suggestion_for_timing = should_reset_prompt_suggestion_for_timing(
        input.prompt_suggestion_state.text.is_some(),
        input.prompt_suggestion.is_some(),
        input.prompt_suggestion_state.shown_at,
        input.viewing_agent_task_id_present,
    );

    // Use all chip types (pasted text, image, truncated text) for
    // cursor snap and highlighting so every chip behaves atomically.
    let image_ref_positions = extract_all_ref_positions(&displayed_value);
    let cursor_at_image_chip = is_cursor_at_image_chip(&image_ref_positions, input.cursor_offset);
    let snapped_cursor_offset =
        snap_cursor_out_of_image_chip(&image_ref_positions, input.cursor_offset);

    let highlights = build_prompt_highlights(
        &PromptHighlightInput {
            cursor_offset: input.cursor_offset,
            is_searching_history: input.is_searching_history,
            history_match_present: input.history_match_display.is_some(),
            history_failed_match: input.history_failed_match,
            history_query_length: input.history_query_length,
            image_ref_positions: image_ref_positions.clone(),
            btw_triggers: input.btw_triggers.clone(),
            slash_command_triggers: input.slash_command_triggers.clone(),
            token_budget_triggers: input.token_budget_triggers.clone(),
            slack_channel_triggers: input.slack_channel_triggers.clone(),
            member_mention_highlights: input.member_mention_highlights.clone(),
            voice_interim_range: input.voice_interim_range,
            think_triggers: input.think_triggers.clone(),
            ultraplan_triggers: input.ultraplan_triggers.clone(),
            ultrareview_triggers: input.ultrareview_triggers.clone(),
            ultrathink_enabled: input.ultrathink_enabled,
            ultraplan_enabled: input.ultraplan_enabled,
        },
        get_rainbow_color,
    );

    let text_input_view = build_text_input_view_state(&TextInputViewInput {
        input: input.input.clone(),
        history_match_display: input.history_match_display.clone(),
        is_searching_history: input.is_searching_history,
        is_modal_overlay_active: input.is_modal_overlay_active,
        footer_item_selected: input.footer_item_selected,
        suggestion_count: input.suggestion_count,
        cursor_at_image_chip,
        default_placeholder: input.default_placeholder.clone(),
        show_prompt_suggestion,
        prompt_suggestion: input.prompt_suggestion.clone(),
        can_undo: input.can_undo,
    });

    PromptInputRuntimeState {
        displayed_value,
        show_prompt_suggestion,
        should_mark_prompt_suggestion_shown: show_prompt_suggestion,
        should_reset_prompt_suggestion_for_timing,
        image_ref_positions,
        cursor_at_image_chip,
        snapped_cursor_offset,
        highlights,
        text_input_view,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rainbow(index: usize, _len: usize, shimmer: bool) -> String {
        if shimmer {
            format!("shine-{index}")
        } else {
            format!("base-{index}")
        }
    }

    fn base() -> PromptInputRuntimeInput {
        PromptInputRuntimeInput {
            mode: String::from("prompt"),
            input: String::from("hello [Image #3]"),
            history_match_display: None,
            is_searching_history: false,
            is_modal_overlay_active: false,
            footer_item_selected: false,
            cursor_offset: 6,
            suggestion_count: 0,
            default_placeholder: Some(String::from("default")),
            prompt_suggestion: Some(String::from("suggested")),
            prompt_suggestion_state: PromptSuggestionState {
                text: Some(String::from("suggested")),
                shown_at: 0,
            },
            viewing_agent_task_id_present: false,
            can_undo: true,
            history_failed_match: false,
            history_query_length: 0,
            btw_triggers: vec![],
            slash_command_triggers: vec![],
            token_budget_triggers: vec![],
            slack_channel_triggers: vec![],
            member_mention_highlights: vec![],
            voice_interim_range: None,
            think_triggers: vec![],
            ultraplan_triggers: vec![],
            ultrareview_triggers: vec![],
            ultrathink_enabled: false,
            ultraplan_enabled: false,
        }
    }

    #[test]
    fn runtime_state_composes_prompt_suggestion_and_text_input_view() {
        let state = derive_prompt_input_runtime_state(&base(), rainbow);
        assert_eq!(state.displayed_value, "hello [Image #3]");
        assert!(state.show_prompt_suggestion);
        assert!(state.should_mark_prompt_suggestion_shown);
        assert_eq!(
            state.text_input_view.placeholder.as_deref(),
            Some("suggested")
        );
        assert!(state.text_input_view.undo_enabled);
    }

    #[test]
    fn runtime_state_resets_timing_when_state_text_exists_but_renderable_suggestion_does_not() {
        let mut input = base();
        input.prompt_suggestion = None;
        let state = derive_prompt_input_runtime_state(&input, rainbow);
        assert!(state.should_reset_prompt_suggestion_for_timing);
        assert!(!state.show_prompt_suggestion);
        assert_eq!(
            state.text_input_view.placeholder.as_deref(),
            Some("default")
        );
    }

    #[test]
    fn runtime_state_extracts_image_chip_and_snaps_cursor_when_inside() {
        let mut input = base();
        input.cursor_offset = 9;
        let state = derive_prompt_input_runtime_state(&input, rainbow);
        assert_eq!(
            state.image_ref_positions,
            vec![TextRange { start: 6, end: 16 }]
        );
        assert!(!state.cursor_at_image_chip);
        assert_eq!(state.snapped_cursor_offset, Some(6));
    }

    #[test]
    fn runtime_state_marks_cursor_on_chip_and_hides_cursor_in_view_state() {
        let mut input = base();
        input.cursor_offset = 6;
        let state = derive_prompt_input_runtime_state(&input, rainbow);
        assert!(state.cursor_at_image_chip);
        assert!(!state.text_input_view.show_cursor);
    }

    #[test]
    fn runtime_state_builds_history_and_rainbow_highlights() {
        let mut input = base();
        input.history_match_display = Some(String::from("/plan"));
        input.is_searching_history = true;
        input.history_query_length = 2;
        input.think_triggers = vec![TextRange { start: 0, end: 2 }];
        input.ultrathink_enabled = true;
        let state = derive_prompt_input_runtime_state(&input, rainbow);
        assert_eq!(state.displayed_value, "/plan");
        assert!(state
            .highlights
            .iter()
            .any(|highlight| highlight.priority == 20));
        assert!(state
            .highlights
            .iter()
            .any(|highlight| highlight.shimmer_color.is_some()));
    }

    #[test]
    fn runtime_state_snaps_cursor_inside_pasted_text_chip() {
        let mut input = base();
        input.input = String::from("ask [Pasted text #1 +3 lines]");
        input.cursor_offset = 10; // inside the chip
        let state = derive_prompt_input_runtime_state(&input, rainbow);
        // Should snap to chip.start (4) since 10 is in the left half of 4..29
        assert_eq!(state.snapped_cursor_offset, Some(4));
    }

    #[test]
    fn runtime_state_marks_cursor_at_pasted_text_chip_start() {
        let mut input = base();
        input.input = String::from("ask [Pasted text #1 +3 lines]");
        input.cursor_offset = 4; // at chip start
        let state = derive_prompt_input_runtime_state(&input, rainbow);
        assert!(state.cursor_at_image_chip);
        assert!(!state.text_input_view.show_cursor);
    }

    #[test]
    fn runtime_state_inverts_pasted_text_chip_when_selected() {
        let mut input = base();
        input.input = String::from("[Pasted text #1]");
        input.cursor_offset = 0; // at chip start
        let state = derive_prompt_input_runtime_state(&input, rainbow);
        assert!(state
            .highlights
            .iter()
            .any(|h| h.inverse && h.start == 0 && h.end == 16));
    }
}

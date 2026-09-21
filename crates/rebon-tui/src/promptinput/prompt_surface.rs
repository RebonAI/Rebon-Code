//! Prompt-surface helpers.
//!
//! This module holds the "prompt surface" helpers around pasted-image chips
//! and highlight composition. Trigger detection remains caller-owned; this
//! module only combines already-computed trigger ranges into the final
//! highlight list.

/// Minimal parsed reference match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptReferenceMatch {
    /// Parsed numeric id.
    pub id: u32,
    /// Full matched placeholder text.
    pub matched_text: String,
    /// Start index of the match in the input string.
    pub index: usize,
}

/// Half-open text range (`start..end`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    /// Inclusive start offset.
    pub start: usize,
    /// Exclusive end offset.
    pub end: usize,
}

/// Range tagged with a resolved theme color.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeHighlightRange {
    /// Inclusive start offset.
    pub start: usize,
    /// Exclusive end offset.
    pub end: usize,
    /// Theme color key.
    pub theme_color: String,
}

/// One final prompt highlight row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSurfaceHighlight {
    /// Inclusive start offset.
    pub start: usize,
    /// Exclusive end offset.
    pub end: usize,
    /// Optional base theme color.
    pub color: Option<String>,
    /// Optional shimmer color for rainbow keywords.
    pub shimmer_color: Option<String>,
    /// Render the range dimmed in addition to `color`.
    pub dim_color: bool,
    /// Render the range inverted.
    pub inverse: bool,
    /// Higher wins when highlights overlap.
    pub priority: u8,
}

/// Minimal history content block used by prompt-surface helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceMessageContentBlock {
    /// Text block that may contain prompt references.
    Text(String),
    /// Non-text blocks are dropped; only text is surfaced.
    Other,
}

/// Minimal history message shape used by the prompt-surface helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfacePromptHistoryMessage {
    /// Only user messages matter for prompt references.
    pub is_user: bool,
    /// Optional image ids already extracted by the caller.
    pub image_paste_ids: Vec<u32>,
    /// Content blocks from the stored message body.
    pub content_blocks: Vec<SurfaceMessageContentBlock>,
}

/// Inputs required to compose the prompt-surface highlight list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptHighlightInput {
    /// Current cursor offset.
    pub cursor_offset: usize,
    /// Whether the history search is open.
    pub is_searching_history: bool,
    /// Whether a history match exists.
    pub history_match_present: bool,
    /// Whether the history search found no match for the query.
    pub history_failed_match: bool,
    /// Length of the current history query.
    pub history_query_length: usize,
    /// Pre-parsed image chip positions.
    pub image_ref_positions: Vec<TextRange>,
    /// Ranges of `btw` keyword triggers.
    pub btw_triggers: Vec<TextRange>,
    /// Ranges of slash-command triggers.
    pub slash_command_triggers: Vec<TextRange>,
    /// Ranges of token-budget triggers.
    pub token_budget_triggers: Vec<TextRange>,
    /// Ranges of Slack channel mentions.
    pub slack_channel_triggers: Vec<TextRange>,
    /// Member-mention ranges with their resolved theme colors.
    pub member_mention_highlights: Vec<ThemeHighlightRange>,
    /// Range of the interim voice transcription, when one is showing.
    pub voice_interim_range: Option<TextRange>,
    /// Ranges of `think` keyword triggers.
    pub think_triggers: Vec<TextRange>,
    /// Ranges of `ultraplan` keyword triggers.
    pub ultraplan_triggers: Vec<TextRange>,
    /// Ranges of `ultrareview` keyword triggers.
    pub ultrareview_triggers: Vec<TextRange>,
    /// Whether ultrathink is enabled for this session.
    pub ultrathink_enabled: bool,
    /// Whether the ultraplan feature flag is on.
    pub ultraplan_enabled: bool,
}

/// Scans the input for `[Pasted text #N]`, `[Image #N]`, and
/// `[...Truncated text #N]` references.
pub fn parse_references(input: &str) -> Vec<PromptReferenceMatch> {
    let prefixes = ["[Pasted text #", "[Image #", "[...Truncated text #"];
    let bytes = input.as_bytes();
    let mut matches = Vec::new();

    for prefix in prefixes {
        for (index, matched_prefix) in input.match_indices(prefix) {
            let mut cursor = index + matched_prefix.len();
            let number_start = cursor;
            while cursor < input.len() && bytes[cursor].is_ascii_digit() {
                cursor += 1;
            }
            if cursor == number_start {
                continue;
            }
            let Some(close_bracket_offset) = input[cursor..].find(']') else {
                continue;
            };
            let end = cursor + close_bracket_offset + 1;
            if let Ok(id) = input[number_start..cursor].parse::<u32>() {
                if id > 0 {
                    matches.push(PromptReferenceMatch {
                        id,
                        matched_text: input[index..end].to_string(),
                        index,
                    });
                }
            }
        }
    }

    matches.sort_by_key(|matched| matched.index);
    matches
}

/// Positions of every `[Image #N]` chip in `input`.
pub fn extract_image_ref_positions(input: &str) -> Vec<TextRange> {
    parse_references(input)
        .into_iter()
        .filter(|matched| matched.matched_text.starts_with("[Image"))
        .map(|matched| TextRange {
            start: matched.index,
            end: matched.index + matched.matched_text.len(),
        })
        .collect()
}

/// Return positions for ALL chip reference types: `[Pasted text #N]`,
/// `[Image #N]`, `[...Truncated text #N]`. Used for cursor snap and
/// inverse highlight so every chip type behaves atomically.
pub fn extract_all_ref_positions(input: &str) -> Vec<TextRange> {
    parse_references(input)
        .into_iter()
        .map(|matched| TextRange {
            start: matched.index,
            end: matched.index + matched.matched_text.len(),
        })
        .collect()
}

/// True when `cursor_offset` sits exactly at the start of an image chip.
pub fn is_cursor_at_image_chip(image_ref_positions: &[TextRange], cursor_offset: usize) -> bool {
    image_ref_positions
        .iter()
        .any(|range| range.start == cursor_offset)
}

/// Snap a cursor sitting strictly inside an image chip to the nearer edge.
pub fn snap_cursor_out_of_image_chip(
    image_ref_positions: &[TextRange],
    cursor_offset: usize,
) -> Option<usize> {
    let inside = image_ref_positions
        .iter()
        .find(|range| cursor_offset > range.start && cursor_offset < range.end)?;
    let midpoint = (inside.start + inside.end) as f64 / 2.0;
    Some(if (cursor_offset as f64) < midpoint {
        inside.start
    } else {
        inside.end
    })
}

/// Compose the final highlight list from the pre-computed trigger ranges.
pub fn build_prompt_highlights(
    input: &PromptHighlightInput,
    get_rainbow_color: impl Fn(usize, usize, bool) -> String,
) -> Vec<PromptSurfaceHighlight> {
    let mut highlights = Vec::new();

    for range in &input.image_ref_positions {
        if input.cursor_offset == range.start {
            highlights.push(PromptSurfaceHighlight {
                start: range.start,
                end: range.end,
                color: None,
                shimmer_color: None,
                dim_color: false,
                inverse: true,
                priority: 8,
            });
        }
    }

    if input.is_searching_history && input.history_match_present && !input.history_failed_match {
        highlights.push(PromptSurfaceHighlight {
            start: input.cursor_offset,
            end: input.cursor_offset + input.history_query_length,
            color: Some(String::from("warning")),
            shimmer_color: None,
            dim_color: false,
            inverse: false,
            priority: 20,
        });
    }

    push_colored_ranges(&mut highlights, &input.btw_triggers, "warning", 15, false);
    push_colored_ranges(
        &mut highlights,
        &input.slash_command_triggers,
        "suggestion",
        5,
        false,
    );
    push_colored_ranges(
        &mut highlights,
        &input.token_budget_triggers,
        "suggestion",
        5,
        false,
    );
    push_colored_ranges(
        &mut highlights,
        &input.slack_channel_triggers,
        "suggestion",
        5,
        false,
    );

    for range in &input.member_mention_highlights {
        highlights.push(PromptSurfaceHighlight {
            start: range.start,
            end: range.end,
            color: Some(range.theme_color.clone()),
            shimmer_color: None,
            dim_color: false,
            inverse: false,
            priority: 5,
        });
    }

    if let Some(range) = input.voice_interim_range {
        highlights.push(PromptSurfaceHighlight {
            start: range.start,
            end: range.end,
            color: None,
            shimmer_color: None,
            dim_color: true,
            inverse: false,
            priority: 1,
        });
    }

    if input.ultrathink_enabled {
        push_rainbow_ranges(&mut highlights, &input.think_triggers, &get_rainbow_color);
    }
    if input.ultraplan_enabled {
        push_rainbow_ranges(
            &mut highlights,
            &input.ultraplan_triggers,
            &get_rainbow_color,
        );
    }
    push_rainbow_ranges(
        &mut highlights,
        &input.ultrareview_triggers,
        &get_rainbow_color,
    );

    highlights
}

fn push_colored_ranges(
    highlights: &mut Vec<PromptSurfaceHighlight>,
    ranges: &[TextRange],
    color: &str,
    priority: u8,
    dim_color: bool,
) {
    for range in ranges {
        highlights.push(PromptSurfaceHighlight {
            start: range.start,
            end: range.end,
            color: Some(color.to_string()),
            shimmer_color: None,
            dim_color,
            inverse: false,
            priority,
        });
    }
}

fn push_rainbow_ranges(
    highlights: &mut Vec<PromptSurfaceHighlight>,
    ranges: &[TextRange],
    get_rainbow_color: impl Fn(usize, usize, bool) -> String,
) {
    for range in ranges {
        let len = range.end.saturating_sub(range.start);
        for offset in 0..len {
            highlights.push(PromptSurfaceHighlight {
                start: range.start + offset,
                end: range.start + offset + 1,
                color: Some(get_rainbow_color(offset, len, false)),
                shimmer_color: Some(get_rainbow_color(offset, len, true)),
                dim_color: false,
                inverse: false,
                priority: 10,
            });
        }
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

    #[test]
    fn parse_references_matches_history_formats() {
        let matches = parse_references(
            "x [Pasted text #2] y [Image #7] z [...Truncated text #9 +3 lines...]",
        );
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].id, 2);
        assert_eq!(matches[1].matched_text, "[Image #7]");
        assert_eq!(matches[2].id, 9);
    }

    #[test]
    fn extract_image_ref_positions_only_keeps_image_refs() {
        let positions = extract_image_ref_positions("[Pasted text #2] [Image #7]");
        assert_eq!(positions, vec![TextRange { start: 17, end: 27 }]);
    }

    #[test]
    fn extract_all_ref_positions_includes_every_chip_type() {
        let positions = extract_all_ref_positions(
            "[Pasted text #2] [Image #7] [...Truncated text #9 +3 lines...]",
        );
        assert_eq!(positions.len(), 3);
        assert_eq!(positions[0], TextRange { start: 0, end: 16 });
        assert_eq!(positions[1], TextRange { start: 17, end: 27 });
        assert_eq!(positions[2], TextRange { start: 28, end: 62 });
    }

    #[test]
    fn cursor_chip_helpers_follow_selection_rules() {
        let positions = vec![TextRange { start: 10, end: 19 }];
        assert!(is_cursor_at_image_chip(&positions, 10));
        assert_eq!(snap_cursor_out_of_image_chip(&positions, 12), Some(10));
        assert_eq!(snap_cursor_out_of_image_chip(&positions, 18), Some(19));
        assert_eq!(snap_cursor_out_of_image_chip(&positions, 19), None);
    }

    #[test]
    fn build_prompt_highlights_inverts_selected_image_chip_and_history_match() {
        let highlights = build_prompt_highlights(
            &PromptHighlightInput {
                cursor_offset: 4,
                is_searching_history: true,
                history_match_present: true,
                history_failed_match: false,
                history_query_length: 3,
                image_ref_positions: vec![TextRange { start: 4, end: 13 }],
                ..PromptHighlightInput::default()
            },
            rainbow,
        );
        assert_eq!(highlights.len(), 2);
        assert!(highlights[0].inverse);
        assert_eq!(highlights[1].color.as_deref(), Some("warning"));
        assert_eq!(highlights[1].priority, 20);
    }

    #[test]
    fn build_prompt_highlights_adds_colored_ranges_and_voice_dim() {
        let highlights = build_prompt_highlights(
            &PromptHighlightInput {
                btw_triggers: vec![TextRange { start: 1, end: 4 }],
                slash_command_triggers: vec![TextRange { start: 5, end: 8 }],
                token_budget_triggers: vec![TextRange { start: 8, end: 11 }],
                slack_channel_triggers: vec![TextRange { start: 11, end: 14 }],
                member_mention_highlights: vec![ThemeHighlightRange {
                    start: 14,
                    end: 19,
                    theme_color: String::from("cyan"),
                }],
                voice_interim_range: Some(TextRange { start: 20, end: 24 }),
                ..PromptHighlightInput::default()
            },
            rainbow,
        );
        assert_eq!(highlights.len(), 6);
        assert_eq!(highlights[0].color.as_deref(), Some("warning"));
        assert_eq!(highlights[4].color.as_deref(), Some("cyan"));
        assert!(highlights[5].dim_color);
    }

    #[test]
    fn build_prompt_highlights_expands_rainbow_triggers_per_character() {
        let highlights = build_prompt_highlights(
            &PromptHighlightInput {
                ultrathink_enabled: true,
                ultraplan_enabled: true,
                think_triggers: vec![TextRange { start: 0, end: 2 }],
                ultraplan_triggers: vec![TextRange { start: 2, end: 4 }],
                ultrareview_triggers: vec![TextRange { start: 4, end: 5 }],
                ..PromptHighlightInput::default()
            },
            rainbow,
        );
        assert_eq!(highlights.len(), 5);
        assert_eq!(highlights[0].color.as_deref(), Some("base-0"));
        assert_eq!(highlights[0].shimmer_color.as_deref(), Some("shine-0"));
        assert_eq!(highlights[4].start, 4);
        assert_eq!(highlights[4].priority, 10);
    }
}

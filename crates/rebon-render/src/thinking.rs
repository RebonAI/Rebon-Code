//! Thinking-message projections for transcript rendering.
//!
//! Builds the visible rows and metadata used when assistant thinking
//! content is shown, hidden, or collapsed.

/// Pointer glyph that prefixes inline thinking content (U+276F).
pub const POINTER_GLYPH: &str = "\u{276f}";

/// Label for the brief layout ("You").
pub const BRIEF_LABEL: &str = "You";

/// Static compact thinking label.
pub const THINKING_LABEL: &str = "\u{2234} Thinking";

/// Expanded thinking label with the trailing ellipsis.
pub const THINKING_LABEL_WITH_ELLIPSIS: &str = "\u{2234} Thinking\u{2026}";

/// Theme color keys touched by the thinking-related renderers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingThemeColor {
    /// `"suggestion"`
    Suggestion,
    /// `"subtle"`
    Subtle,
    /// `"briefLabelYou"`
    BriefLabelYou,
    /// `"text"`
    Text,
    /// `"rainbow_red"`
    RainbowRed,
    /// `"rainbow_orange"`
    RainbowOrange,
    /// `"rainbow_yellow"`
    RainbowYellow,
    /// `"rainbow_green"`
    RainbowGreen,
    /// `"rainbow_blue"`
    RainbowBlue,
    /// `"rainbow_indigo"`
    RainbowIndigo,
    /// `"rainbow_violet"`
    RainbowViolet,
    /// `"rainbow_red_shimmer"`
    RainbowRedShimmer,
    /// `"rainbow_orange_shimmer"`
    RainbowOrangeShimmer,
    /// `"rainbow_yellow_shimmer"`
    RainbowYellowShimmer,
    /// `"rainbow_green_shimmer"`
    RainbowGreenShimmer,
    /// `"rainbow_blue_shimmer"`
    RainbowBlueShimmer,
    /// `"rainbow_indigo_shimmer"`
    RainbowIndigoShimmer,
    /// `"rainbow_violet_shimmer"`
    RainbowVioletShimmer,
}

impl ThinkingThemeColor {
    /// The theme key string for this color.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Suggestion => "suggestion",
            Self::Subtle => "subtle",
            Self::BriefLabelYou => "briefLabelYou",
            Self::Text => "text",
            Self::RainbowRed => "rainbow_red",
            Self::RainbowOrange => "rainbow_orange",
            Self::RainbowYellow => "rainbow_yellow",
            Self::RainbowGreen => "rainbow_green",
            Self::RainbowBlue => "rainbow_blue",
            Self::RainbowIndigo => "rainbow_indigo",
            Self::RainbowViolet => "rainbow_violet",
            Self::RainbowRedShimmer => "rainbow_red_shimmer",
            Self::RainbowOrangeShimmer => "rainbow_orange_shimmer",
            Self::RainbowYellowShimmer => "rainbow_yellow_shimmer",
            Self::RainbowGreenShimmer => "rainbow_green_shimmer",
            Self::RainbowBlueShimmer => "rainbow_blue_shimmer",
            Self::RainbowIndigoShimmer => "rainbow_indigo_shimmer",
            Self::RainbowVioletShimmer => "rainbow_violet_shimmer",
        }
    }
}

/// One trigger word found inside a thought, with its byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingTriggerPosition {
    /// The matched text exactly as written, case preserved.
    pub word: String,
    /// Start byte index.
    pub start: usize,
    /// End byte index.
    pub end: usize,
}

/// Input for highlighting trigger words inside one thought.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightedThinkingTextInput {
    /// Raw text to render.
    pub text: String,
    /// Whether the parent selected the brief layout branch.
    pub use_brief_layout: bool,
    /// Already formatted timestamp label, if any.
    pub formatted_timestamp: Option<String>,
    /// Whether the queued-message context is active.
    pub is_queued: bool,
    /// Whether the surrounding message-actions context is selected.
    pub is_selected: bool,
    /// Runtime gate for the rainbow ultrathink highlighting.
    pub ultrathink_enabled: bool,
}

/// What a renderer draws for one highlighted thought.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HighlightedThinkingTextProjection {
    /// Brief/chat-style layout.
    Brief(BriefThinkingDisplay),
    /// Inline pointer + text layout.
    Inline(InlineThinkingDisplay),
}

/// Brief-layout display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefThinkingDisplay {
    /// Label text, always `"You"`.
    pub label: &'static str,
    /// Label color.
    pub label_color: ThinkingThemeColor,
    /// Optional formatted timestamp.
    pub timestamp: Option<String>,
    /// Message body text.
    pub text: String,
    /// Body color.
    pub text_color: ThinkingThemeColor,
    /// Left padding the caller applies, 2 columns.
    pub padding_left: u8,
}

/// Inline pointer display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlinePointerDisplay {
    /// The pointer glyph, `POINTER_GLYPH` (U+276F).
    pub glyph: &'static str,
    /// Pointer color.
    pub color: ThinkingThemeColor,
}

/// One inline segment in the non-brief layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingTextSegment {
    /// Segment text.
    pub text: String,
    /// Theme color for the segment.
    pub color: ThinkingThemeColor,
}

/// Inline layout display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineThinkingDisplay {
    /// Pointer prefix.
    pub pointer: InlinePointerDisplay,
    /// Inline segments after the pointer.
    pub segments: Vec<ThinkingTextSegment>,
}

/// Input for projecting one assistant thinking block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssistantThinkingMessageInput {
    /// The raw thinking text.
    pub thinking: String,
    /// Whether the standard top margin is added above the block.
    pub add_margin: bool,
    /// Whether the transcript view is active.
    pub is_transcript_mode: bool,
    /// `verbose`.
    pub verbose: bool,
    /// Whether the block is hidden in transcript mode.
    pub hide_in_transcript: bool,
    /// Caller asks transcript compact mode to render the truncated thinking
    /// preview directly, without the `Thinking` heading used by expanded views.
    pub compact_preview: bool,
    /// Caller signals the `thinking` body is a truncated preview (e.g.
    /// only the first non-empty line) and the Expanded display should
    /// surface a `Ctrl+O to expand` affordance so the user knows the
    /// full content is reachable via the verbosity toggle.
    pub show_expand_hint: bool,
}

/// What a renderer draws for one assistant thinking block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantThinkingMessageProjection {
    /// Hidden because the block is empty or suppressed.
    Hidden,
    /// Compact thinking row.
    Collapsed(AssistantThinkingCollapsedDisplay),
    /// Full markdown branch.
    Expanded(AssistantThinkingExpandedDisplay),
}

/// Compact thinking title row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantThinkingCollapsedDisplay {
    /// Top margin, taken from the add-margin flag.
    pub margin_top: u8,
    /// First non-empty line of the thinking content, rendered as markdown.
    pub markdown: String,
    /// Whether compact mode shows the expand hint.
    pub show_expand_hint: bool,
}

/// Full expanded thinking layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantThinkingExpandedDisplay {
    /// Top margin, taken from the add-margin flag.
    pub margin_top: u8,
    /// Heading label, `THINKING_LABEL`.
    pub label: &'static str,
    /// Markdown body text.
    pub markdown: String,
    /// Left padding on the markdown body, 2 columns.
    pub padding_left: u8,
    /// Gap above the body, 1 line.
    pub gap: u8,
    /// Width handed to the layout, always `"100%"`.
    pub width: &'static str,
    /// Whether to render the heading row before the markdown body.
    pub show_label: bool,
    /// Render a `Ctrl+O to expand` affordance beneath the body when the
    /// `markdown` is a truncated preview of a longer thought. Always
    /// `false` in true verbose / transcript mode because there is no
    /// further content to reveal.
    pub show_expand_hint: bool,
}

/// Find every trigger word in `text`, in order, with byte ranges.
pub fn find_thinking_trigger_positions(text: &str) -> Vec<ThinkingTriggerPosition> {
    const KEYWORD: &[u8] = b"ultrathink";

    let bytes = text.as_bytes();
    let mut positions = Vec::new();
    let mut cursor = 0usize;

    while cursor + KEYWORD.len() <= bytes.len() {
        let candidate = &bytes[cursor..cursor + KEYWORD.len()];
        let matches = candidate.eq_ignore_ascii_case(KEYWORD);
        let left_ok = cursor == 0 || !is_word_byte(bytes[cursor - 1]);
        let right_ok =
            cursor + KEYWORD.len() == bytes.len() || !is_word_byte(bytes[cursor + KEYWORD.len()]);

        if matches && left_ok && right_ok {
            positions.push(ThinkingTriggerPosition {
                word: text[cursor..cursor + KEYWORD.len()].to_string(),
                start: cursor,
                end: cursor + KEYWORD.len(),
            });
            cursor += KEYWORD.len();
            continue;
        }

        cursor += 1;
    }

    positions
}

/// Rainbow color for one character position, or its shimmer variant when
/// `shimmer` is set.
pub fn get_rainbow_color(char_index: usize, shimmer: bool) -> ThinkingThemeColor {
    const RAINBOW: [ThinkingThemeColor; 7] = [
        ThinkingThemeColor::RainbowRed,
        ThinkingThemeColor::RainbowOrange,
        ThinkingThemeColor::RainbowYellow,
        ThinkingThemeColor::RainbowGreen,
        ThinkingThemeColor::RainbowBlue,
        ThinkingThemeColor::RainbowIndigo,
        ThinkingThemeColor::RainbowViolet,
    ];
    const RAINBOW_SHIMMER: [ThinkingThemeColor; 7] = [
        ThinkingThemeColor::RainbowRedShimmer,
        ThinkingThemeColor::RainbowOrangeShimmer,
        ThinkingThemeColor::RainbowYellowShimmer,
        ThinkingThemeColor::RainbowGreenShimmer,
        ThinkingThemeColor::RainbowBlueShimmer,
        ThinkingThemeColor::RainbowIndigoShimmer,
        ThinkingThemeColor::RainbowVioletShimmer,
    ];

    let palette = if shimmer { &RAINBOW_SHIMMER } else { &RAINBOW };
    palette[char_index % palette.len()]
}

/// Project one highlighted thought into its display form.
pub fn project_highlighted_thinking_text(
    input: &HighlightedThinkingTextInput,
) -> HighlightedThinkingTextProjection {
    if input.use_brief_layout {
        return HighlightedThinkingTextProjection::Brief(BriefThinkingDisplay {
            label: BRIEF_LABEL,
            label_color: if input.is_queued {
                ThinkingThemeColor::Subtle
            } else {
                ThinkingThemeColor::BriefLabelYou
            },
            timestamp: input
                .formatted_timestamp
                .clone()
                .filter(|timestamp| !timestamp.is_empty()),
            text: input.text.clone(),
            text_color: if input.is_queued {
                ThinkingThemeColor::Subtle
            } else {
                ThinkingThemeColor::Text
            },
            padding_left: 2,
        });
    }

    let pointer = InlinePointerDisplay {
        glyph: POINTER_GLYPH,
        color: if input.is_selected {
            ThinkingThemeColor::Suggestion
        } else {
            ThinkingThemeColor::Subtle
        },
    };

    let triggers = if input.ultrathink_enabled {
        find_thinking_trigger_positions(&input.text)
    } else {
        Vec::new()
    };

    if triggers.is_empty() {
        return HighlightedThinkingTextProjection::Inline(InlineThinkingDisplay {
            pointer,
            segments: vec![ThinkingTextSegment {
                text: input.text.clone(),
                color: ThinkingThemeColor::Text,
            }],
        });
    }

    let mut segments = Vec::new();
    let mut cursor = 0usize;

    for trigger in triggers {
        if trigger.start > cursor {
            segments.push(ThinkingTextSegment {
                text: input.text[cursor..trigger.start].to_string(),
                color: ThinkingThemeColor::Text,
            });
        }

        for (offset, ch) in input.text[trigger.start..trigger.end].chars().enumerate() {
            segments.push(ThinkingTextSegment {
                text: ch.to_string(),
                color: get_rainbow_color(offset, false),
            });
        }

        cursor = trigger.end;
    }

    if cursor < input.text.len() {
        segments.push(ThinkingTextSegment {
            text: input.text[cursor..].to_string(),
            color: ThinkingThemeColor::Text,
        });
    }

    HighlightedThinkingTextProjection::Inline(InlineThinkingDisplay { pointer, segments })
}

/// Project one assistant thinking block into its display form.
pub fn project_assistant_thinking_message(
    input: &AssistantThinkingMessageInput,
) -> AssistantThinkingMessageProjection {
    let thinking = input.thinking.trim();
    if thinking.is_empty() || input.hide_in_transcript {
        return AssistantThinkingMessageProjection::Hidden;
    }

    let margin_top = u8::from(input.add_margin);
    if input.compact_preview {
        let markdown = first_non_empty_line(thinking)
            .unwrap_or(thinking)
            .to_string();
        return AssistantThinkingMessageProjection::Collapsed(AssistantThinkingCollapsedDisplay {
            margin_top,
            markdown,
            show_expand_hint: input.show_expand_hint,
        });
    }

    AssistantThinkingMessageProjection::Expanded(AssistantThinkingExpandedDisplay {
        margin_top,
        label: THINKING_LABEL,
        markdown: thinking.to_string(),
        padding_left: 2,
        gap: 1,
        width: "100%",
        show_label: false,
        show_expand_hint: false,
    })
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_case_insensitive_ultrathink_boundaries() {
        let positions =
            find_thinking_trigger_positions("ultrathink x ULTRATHINK ultrathinky not-ultrathink");
        assert_eq!(positions.len(), 3);
        assert_eq!(positions[0].word, "ultrathink");
        assert_eq!(positions[1].word, "ULTRATHINK");
        assert_eq!(positions[2].word, "ultrathink");
    }

    #[test]
    fn rainbow_colors_cycle_and_shimmer() {
        assert_eq!(get_rainbow_color(0, false), ThinkingThemeColor::RainbowRed);
        assert_eq!(get_rainbow_color(7, false), ThinkingThemeColor::RainbowRed);
        assert_eq!(
            get_rainbow_color(1, true),
            ThinkingThemeColor::RainbowOrangeShimmer
        );
    }

    #[test]
    fn brief_projection_uses_queued_colors_and_timestamp() {
        let projection = project_highlighted_thinking_text(&HighlightedThinkingTextInput {
            text: "prompt".into(),
            use_brief_layout: true,
            formatted_timestamp: Some("1:30 PM".into()),
            is_queued: true,
            is_selected: false,
            ultrathink_enabled: true,
        });

        let HighlightedThinkingTextProjection::Brief(display) = projection else {
            panic!("expected brief projection");
        };

        assert_eq!(display.label, BRIEF_LABEL);
        assert_eq!(display.label_color, ThinkingThemeColor::Subtle);
        assert_eq!(display.timestamp.as_deref(), Some("1:30 PM"));
        assert_eq!(display.text_color, ThinkingThemeColor::Subtle);
        assert_eq!(display.padding_left, 2);
    }

    #[test]
    fn inline_projection_keeps_plain_text_when_no_trigger() {
        let projection = project_highlighted_thinking_text(&HighlightedThinkingTextInput {
            text: "hello".into(),
            use_brief_layout: false,
            formatted_timestamp: None,
            is_queued: false,
            is_selected: true,
            ultrathink_enabled: false,
        });

        let HighlightedThinkingTextProjection::Inline(display) = projection else {
            panic!("expected inline projection");
        };

        assert_eq!(display.pointer.glyph, POINTER_GLYPH);
        assert_eq!(display.pointer.color, ThinkingThemeColor::Suggestion);
        assert_eq!(display.segments.len(), 1);
        assert_eq!(display.segments[0].text, "hello");
        assert_eq!(display.segments[0].color, ThinkingThemeColor::Text);
    }

    #[test]
    fn inline_projection_splits_plain_and_rainbow_segments() {
        let projection = project_highlighted_thinking_text(&HighlightedThinkingTextInput {
            text: "pre ultrathink post".into(),
            use_brief_layout: false,
            formatted_timestamp: None,
            is_queued: false,
            is_selected: false,
            ultrathink_enabled: true,
        });

        let HighlightedThinkingTextProjection::Inline(display) = projection else {
            panic!("expected inline projection");
        };

        assert_eq!(display.pointer.color, ThinkingThemeColor::Subtle);
        assert_eq!(display.segments.first().unwrap().text, "pre ");
        assert_eq!(display.segments.last().unwrap().text, " post");
        assert_eq!(display.segments[1].text, "u");
        assert_eq!(display.segments[1].color, ThinkingThemeColor::RainbowRed);
        assert_eq!(display.segments[3].text, "t");
    }

    #[test]
    fn assistant_thinking_hides_empty_and_transcript_suppressed_blocks() {
        assert_eq!(
            project_assistant_thinking_message(&AssistantThinkingMessageInput {
                thinking: String::new(),
                ..Default::default()
            }),
            AssistantThinkingMessageProjection::Hidden
        );
        assert_eq!(
            project_assistant_thinking_message(&AssistantThinkingMessageInput {
                thinking: "secret".into(),
                hide_in_transcript: true,
                ..Default::default()
            }),
            AssistantThinkingMessageProjection::Hidden
        );
    }

    #[test]
    fn assistant_thinking_compact_preview_uses_first_line_with_expand_hint() {
        let projection = project_assistant_thinking_message(&AssistantThinkingMessageInput {
            thinking: "\n**Title line**\nfull reasoning".into(),
            add_margin: true,
            compact_preview: true,
            show_expand_hint: true,
            ..Default::default()
        });

        assert_eq!(
            projection,
            AssistantThinkingMessageProjection::Collapsed(AssistantThinkingCollapsedDisplay {
                margin_top: 1,
                markdown: "**Title line**".into(),
                show_expand_hint: true,
            })
        );
    }

    #[test]
    fn assistant_thinking_expands_full_markdown_when_not_compact_preview() {
        let projection = project_assistant_thinking_message(&AssistantThinkingMessageInput {
            thinking: "step 1\nstep 2".into(),
            is_transcript_mode: true,
            verbose: true,
            show_expand_hint: true,
            ..Default::default()
        });

        assert_eq!(
            projection,
            AssistantThinkingMessageProjection::Expanded(AssistantThinkingExpandedDisplay {
                margin_top: 0,
                label: THINKING_LABEL,
                markdown: "step 1\nstep 2".into(),
                padding_left: 2,
                gap: 1,
                width: "100%",
                show_label: false,
                show_expand_hint: false,
            })
        );
    }
}

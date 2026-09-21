//! Footer suggestion windowing and row layout.
//!
//! The consumer draws the suggestions; this module returns the
//! visible-window choice and the per-row styled text segments
//! as plain data.

use rebon_width::{terminal_char_width, truncate_to_ellipsis, WidthStr};

/// Maximum number of items shown in the overlay.
pub const OVERLAY_MAX_ITEMS: usize = 5;

const ELLIPSIS: &str = "\u{2026}";
const EM_DASH_WITH_SPACES: &str = " \u{2014} ";

/// Minimal suggestion item the projection reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestionItem {
    /// Stable item id (`file-*`, `agent-*`, etc.).
    pub id: String,
    /// Main display label.
    pub display_text: String,
    /// Optional tag rendered as `[tag] ` in non-unified rows.
    pub tag: Option<String>,
    /// Optional description text.
    pub description: Option<String>,
    /// Optional explicit theme color key.
    pub color: Option<String>,
}

/// Semantic role of one rendered suggestion text span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionSegmentRole {
    /// Main display text / line content.
    Primary,
    /// Optional `[tag] ` segment.
    Tag,
    /// Optional trailing description segment.
    Description,
}

/// One styled text span in the rendered row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledSegment {
    /// Semantic role of the span.
    pub role: SuggestionSegmentRole,
    /// Plain text content.
    pub text: String,
    /// Optional theme color key.
    pub color: Option<String>,
    /// Whether the span is dimmed.
    pub dim: bool,
}

/// Plain-data version of one visible suggestion row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSuggestionRow {
    /// Source index inside the full suggestion list.
    pub source_index: usize,
    /// Whether this row is selected.
    pub is_selected: bool,
    /// Styled row segments in display order.
    pub segments: Vec<StyledSegment>,
}

/// The visible suggestion window plus the computed width budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSuggestionList {
    /// Window start index (inclusive).
    pub start_index: usize,
    /// Window end index (exclusive).
    pub end_index: usize,
    /// Computed width budget for one row's content.
    pub max_column_width: usize,
    /// Visible rendered rows.
    pub rows: Vec<RenderedSuggestionRow>,
}

/// Project the footer suggestions into visible rows plus the width budget.
pub fn render_footer_suggestions(
    suggestions: &[SuggestionItem],
    selected_suggestion: usize,
    max_column_width_override: Option<usize>,
    overlay: bool,
    terminal_rows: usize,
    terminal_columns: usize,
) -> Option<RenderedSuggestionList> {
    if suggestions.is_empty() {
        return None;
    }

    let selected = selected_suggestion.min(suggestions.len() - 1);
    let max_column_width = max_column_width_override.unwrap_or_else(|| {
        suggestions
            .iter()
            .map(|item| string_width(&item.display_text))
            .max()
            .unwrap_or(0)
            + 5
    });
    let max_visible_items = if overlay {
        OVERLAY_MAX_ITEMS
    } else {
        6.min(1.max(terminal_rows.saturating_sub(3)))
    };
    let start_index = selected
        .saturating_sub(max_visible_items / 2)
        .min(suggestions.len().saturating_sub(max_visible_items));
    let end_index = (start_index + max_visible_items).min(suggestions.len());
    let rows = suggestions[start_index..end_index]
        .iter()
        .enumerate()
        .map(|(offset, item)| {
            let source_index = start_index + offset;
            render_suggestion_row(
                item,
                source_index,
                terminal_columns,
                max_column_width,
                source_index == selected,
            )
        })
        .collect();

    Some(RenderedSuggestionList {
        start_index,
        end_index,
        max_column_width,
        rows,
    })
}

fn render_suggestion_row(
    item: &SuggestionItem,
    source_index: usize,
    terminal_columns: usize,
    max_column_width: usize,
    is_selected: bool,
) -> RenderedSuggestionRow {
    if is_unified_suggestion(&item.id) {
        let line = render_unified_line(item, terminal_columns);
        return RenderedSuggestionRow {
            source_index,
            is_selected,
            segments: vec![StyledSegment {
                role: SuggestionSegmentRole::Primary,
                text: line,
                color: if is_selected {
                    Some(String::from("suggestion"))
                } else {
                    None
                },
                dim: !is_selected,
            }],
        };
    }

    let max_name_width = (terminal_columns as f64 * 0.4).floor() as usize;
    let display_text_width = max_column_width.min(max_name_width);
    let mut display_text = item.display_text.clone();
    if string_width(&display_text) > display_text_width.saturating_sub(2) {
        display_text = truncate_to_ellipsis(&display_text, display_text_width.saturating_sub(2));
    }
    let padded_display_text = format!(
        "{}{}",
        display_text,
        " ".repeat(display_text_width.saturating_sub(string_width(&display_text)))
    );
    let tag_text = item
        .tag
        .as_ref()
        .map(|tag| format!("[{tag}] "))
        .unwrap_or_default();
    let tag_width = string_width(&tag_text);
    let description_width = terminal_columns.saturating_sub(display_text_width + tag_width + 4);
    let truncated_description = item
        .description
        .as_deref()
        .map(normalize_whitespace)
        .map(|text| truncate_to_ellipsis(&text, description_width))
        .unwrap_or_default();

    let mut segments = vec![StyledSegment {
        role: SuggestionSegmentRole::Primary,
        text: padded_display_text,
        color: item
            .color
            .clone()
            .or_else(|| is_selected.then(|| String::from("suggestion"))),
        dim: !is_selected,
    }];
    if !tag_text.is_empty() {
        segments.push(StyledSegment {
            role: SuggestionSegmentRole::Tag,
            text: tag_text,
            color: None,
            dim: true,
        });
    }
    segments.push(StyledSegment {
        role: SuggestionSegmentRole::Description,
        text: truncated_description,
        color: is_selected.then(|| String::from("suggestion")),
        dim: !is_selected,
    });

    RenderedSuggestionRow {
        source_index,
        is_selected,
        segments,
    }
}

fn render_unified_line(item: &SuggestionItem, terminal_columns: usize) -> String {
    let icon = get_icon(&item.id);
    let is_file = item.id.starts_with("file-");
    let is_mcp_resource = item.id.starts_with("mcp-resource-");
    let separator_width = if item.description.is_some() { 3 } else { 0 };

    let display_text = if is_file {
        let desc_reserve = item
            .description
            .as_deref()
            .map(string_width)
            .map(|width| width.min(20))
            .unwrap_or(0);
        let max_path_length =
            terminal_columns.saturating_sub(2 + 4 + separator_width + desc_reserve);
        truncate_path_middle(&item.display_text, max_path_length)
    } else if is_mcp_resource {
        truncate_to_ellipsis(&item.display_text, 30)
    } else {
        item.display_text.clone()
    };

    if let Some(description) = item.description.as_deref() {
        let available_width =
            terminal_columns.saturating_sub(2 + string_width(&display_text) + separator_width + 4);
        let truncated_description =
            truncate_to_ellipsis(&normalize_whitespace(description), available_width);
        format!("{icon} {display_text}{EM_DASH_WITH_SPACES}{truncated_description}")
    } else {
        format!("{icon} {display_text}")
    }
}

fn get_icon(item_id: &str) -> &'static str {
    if item_id.starts_with("file-") {
        "+"
    } else if item_id.starts_with("mcp-resource-") {
        "○"
    } else if item_id.starts_with("agent-") {
        "*"
    } else {
        "+"
    }
}

fn is_unified_suggestion(item_id: &str) -> bool {
    item_id.starts_with("file-")
        || item_id.starts_with("mcp-resource-")
        || item_id.starts_with("agent-")
}

fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn string_width(text: &str) -> usize {
    WidthStr::width(text)
}

fn truncate_path_middle(path: &str, max_length: usize) -> String {
    if string_width(path) <= max_length {
        return path.to_string();
    }
    if max_length == 0 {
        return ELLIPSIS.to_string();
    }
    if max_length < 5 {
        return truncate_to_ellipsis(path, max_length);
    }

    let last_slash = path.rfind('/');
    let (directory, filename) = match last_slash {
        Some(idx) => (&path[..idx], &path[idx..]),
        None => ("", path),
    };
    let filename_width = string_width(filename);
    if filename_width >= max_length.saturating_sub(1) {
        return truncate_start_to_width(path, max_length);
    }

    let available_for_dir = max_length.saturating_sub(1 + filename_width);
    if available_for_dir == 0 {
        return truncate_start_to_width(filename, max_length);
    }
    let truncated_dir = truncate_to_width_no_ellipsis(directory, available_for_dir);
    format!("{truncated_dir}{ELLIPSIS}{filename}")
}

fn truncate_start_to_width(text: &str, max_width: usize) -> String {
    if string_width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return ELLIPSIS.to_string();
    }

    let chars: Vec<char> = text.chars().collect();
    let mut width = 0;
    let mut start_idx = chars.len();
    for (idx, ch) in chars.iter().enumerate().rev() {
        let ch_width = terminal_char_width(*ch);
        if width + ch_width > max_width - 1 {
            break;
        }
        width += ch_width;
        start_idx = idx;
    }

    format!(
        "{ELLIPSIS}{}",
        chars[start_idx..].iter().collect::<String>()
    )
}

fn truncate_to_width_no_ellipsis(text: &str, max_width: usize) -> String {
    if string_width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }

    let mut width = 0;
    let mut result = String::new();
    for ch in text.chars() {
        let ch_width = terminal_char_width(ch);
        if width + ch_width > max_width {
            break;
        }
        width += ch_width;
        result.push(ch);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suggestion(id: &str, display_text: &str) -> SuggestionItem {
        SuggestionItem {
            id: id.into(),
            display_text: display_text.into(),
            tag: None,
            description: None,
            color: None,
        }
    }

    #[test]
    fn empty_suggestions_return_none() {
        assert_eq!(render_footer_suggestions(&[], 0, None, false, 10, 80), None);
    }

    #[test]
    fn overlay_window_uses_fixed_max_items_and_centers_selection() {
        let suggestions = (0..10)
            .map(|idx| suggestion(&format!("command-{idx}"), &format!("cmd-{idx}")))
            .collect::<Vec<_>>();
        let rendered = render_footer_suggestions(&suggestions, 7, None, true, 4, 80).unwrap();
        assert_eq!(rendered.start_index, 5);
        assert_eq!(rendered.end_index, 10);
        assert_eq!(rendered.rows.len(), 5);
        assert_eq!(rendered.rows[2].source_index, 7);
    }

    #[test]
    fn inline_window_uses_terminal_rows_budget() {
        let suggestions = (0..10)
            .map(|idx| suggestion(&format!("command-{idx}"), &format!("cmd-{idx}")))
            .collect::<Vec<_>>();
        let rendered = render_footer_suggestions(&suggestions, 1, None, false, 5, 80).unwrap();
        assert_eq!(rendered.rows.len(), 2);
        assert_eq!(rendered.start_index, 0);
        assert_eq!(rendered.end_index, 2);
    }

    #[test]
    fn file_suggestions_use_middle_truncation_and_description_budget() {
        let mut item = suggestion("file-src", "AppHeader.tsx");
        item.description = Some("a long description that should be compacted".into());
        let rendered = render_footer_suggestions(&[item], 0, None, false, 10, 48).unwrap();
        let row = &rendered.rows[0];
        assert_eq!(row.segments.len(), 1);
        let text = &row.segments[0].text;
        assert!(text.starts_with("+ "));
        assert!(text.contains(ELLIPSIS));
        assert!(text.contains("AppHeader.tsx"));
        assert!(text.contains('\u{2014}'));
    }

    #[test]
    fn mcp_resource_suggestions_truncate_display_text_to_thirty_columns() {
        let mut item = suggestion("mcp-resource-1", "abcdefghijklmnopqrstuvwxyz0123456789");
        item.description = Some("desc".into());
        let rendered = render_footer_suggestions(&[item], 0, None, false, 10, 80).unwrap();
        let text = &rendered.rows[0].segments[0].text;
        assert!(text.contains("abcdefghijklmnopqrstuvwxyz012\u{2026}"));
    }

    #[test]
    fn non_unified_rows_keep_primary_tag_and_description_segments() {
        let mut item = suggestion("command-help", "/very-long-command-name");
        item.tag = Some("chat".into());
        item.description = Some("multi\nline   description".into());
        item.color = Some("custom".into());
        let rendered = render_footer_suggestions(&[item], 0, Some(18), false, 10, 64).unwrap();
        let row = &rendered.rows[0];
        assert_eq!(
            row.segments
                .iter()
                .map(|segment| segment.role)
                .collect::<Vec<_>>(),
            vec![
                SuggestionSegmentRole::Primary,
                SuggestionSegmentRole::Tag,
                SuggestionSegmentRole::Description
            ]
        );
        assert_eq!(row.segments[0].color.as_deref(), Some("custom"));
        assert_eq!(row.segments[1].text, "[chat] ");
        assert_eq!(row.segments[2].text, "multi line description");
    }
}

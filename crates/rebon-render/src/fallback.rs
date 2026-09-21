//! Fallback render projections for a tool use that was rejected or failed
//! and whose tool has no renderer of its own.

/// Line cap for the fallback error row.
pub const MAX_RENDERED_LINES: usize = 10;
/// Static height of the fallback rejected-tool-use row.
pub const FALLBACK_TOOL_USE_REJECTED_HEIGHT: usize = 1;

/// Minimal tool-result content surface read by the fallback error row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackToolResultContent {
    /// String tool result.
    Text(String),
    /// Any non-string tool result payload.
    NonText,
}

/// Pure display projection for the fallback tool-error row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackToolUseErrorProjection {
    /// Final error text after cleanup and truncation.
    pub error_text: String,
    /// Number of hidden lines beyond the 10-line cap.
    pub hidden_line_count: usize,
    /// Footer hint shown only when hidden lines exist.
    pub footer: Option<String>,
}

/// Clean up a failed tool result for display: unwrap `<tool_use_error>`,
/// strip sandbox-violation and `<error>` tags, prefix `Error: ` unless the
/// text already starts with `Error: ` or `Cancelled: ` (a non-verbose input
/// validation error becomes `Invalid tool parameters`), and cap the text at
/// [`MAX_RENDERED_LINES`] unless verbose.
pub fn project_fallback_tool_use_error(
    result: &FallbackToolResultContent,
    verbose: bool,
    transcript_shortcut: &str,
) -> FallbackToolUseErrorProjection {
    let transcript_shortcut =
        rebon_design_system::format_shortcut_for_current_platform(transcript_shortcut);
    let error = match result {
        FallbackToolResultContent::NonText => "Tool execution failed".to_owned(),
        FallbackToolResultContent::Text(result) => {
            let extracted_error =
                extract_tag(result, "tool_use_error").unwrap_or_else(|| result.clone());
            let without_sandbox = remove_sandbox_violation_tags(&extracted_error);
            let without_error_tags = without_sandbox
                .replace("<error>", "")
                .replace("</error>", "");
            let trimmed = without_error_tags.trim().to_owned();
            if !verbose && trimmed.contains("InputValidationError: ") {
                "Invalid tool parameters".to_owned()
            } else if trimmed.starts_with("Error: ") || trimmed.starts_with("Cancelled: ") {
                trimmed
            } else {
                format!("Error: {trimmed}")
            }
        }
    };

    let line_count = error.chars().filter(|ch| *ch == '\n').count() + 1;
    let hidden_line_count = line_count.saturating_sub(MAX_RENDERED_LINES);
    let rendered = if verbose {
        error
    } else {
        error
            .lines()
            .take(MAX_RENDERED_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let error_text = strip_underline_ansi(&rendered);
    let footer = (!verbose && hidden_line_count > 0).then(|| {
        format!(
            "... +{} {} ({} to see all)",
            hidden_line_count,
            if hidden_line_count == 1 {
                "line"
            } else {
                "lines"
            },
            transcript_shortcut
        )
    });

    FallbackToolUseErrorProjection {
        error_text,
        hidden_line_count,
        footer,
    }
}

fn extract_tag(html: &str, tag_name: &str) -> Option<String> {
    let open = format!("<{tag_name}>");
    let close = format!("</{tag_name}>");
    let start = html.find(&open)?;
    let after_open = start + open.len();
    let end = html[after_open..].find(&close)? + after_open;
    Some(html[after_open..end].to_owned())
}

fn remove_sandbox_violation_tags(text: &str) -> String {
    let mut output = text.to_owned();
    let open = "<sandbox_violations>";
    let close = "</sandbox_violations>";
    while let Some(start) = output.find(open) {
        let Some(relative_end) = output[start + open.len()..].find(close) else {
            break;
        };
        let end = start + open.len() + relative_end + close.len();
        output.replace_range(start..end, "");
    }
    output
}

fn strip_underline_ansi(content: &str) -> String {
    let mut output = String::with_capacity(content.len());
    let mut index = 0usize;

    while index < content.len() {
        let bytes = content.as_bytes();
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            let start = index;
            index += 2;
            while index < content.len() && bytes[index] != b'm' {
                index += 1;
            }
            if index >= content.len() {
                output.push_str(&content[start..]);
                break;
            }

            let params = &content[start + 2..index];
            let is_underline = params.split(';').any(|part| part == "4");
            if !is_underline {
                output.push_str(&content[start..=index]);
            }
            index += 1;
            continue;
        }

        let ch = content[index..]
            .chars()
            .next()
            .expect("valid UTF-8 slice while stripping ANSI");
        output.push(ch);
        index += ch.len_utf8();
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_text_result_falls_back_to_generic_message() {
        let projection =
            project_fallback_tool_use_error(&FallbackToolResultContent::NonText, false, "ctrl+o");
        assert_eq!(projection.error_text, "Tool execution failed");
        assert_eq!(projection.hidden_line_count, 0);
        assert_eq!(projection.footer, None);
    }

    #[test]
    fn fallback_error_extracts_tool_use_error_and_strips_sandbox_and_error_tags() {
        let projection = project_fallback_tool_use_error(
            &FallbackToolResultContent::Text(
                "<tool_use_error><sandbox_violations>deny</sandbox_violations><error>boom</error></tool_use_error>".into(),
            ),
            false,
            "ctrl+o",
        );
        assert_eq!(projection.error_text, "Error: boom");
    }

    #[test]
    fn input_validation_error_collapses_to_short_message_when_not_verbose() {
        let projection = project_fallback_tool_use_error(
            &FallbackToolResultContent::Text("InputValidationError: bad field".into()),
            false,
            "ctrl+o",
        );
        assert_eq!(projection.error_text, "Invalid tool parameters");
    }

    #[test]
    fn existing_error_and_cancelled_prefixes_are_preserved() {
        let cancelled = project_fallback_tool_use_error(
            &FallbackToolResultContent::Text("Cancelled: stop".into()),
            true,
            "ctrl+o",
        );
        assert_eq!(cancelled.error_text, "Cancelled: stop");

        let error = project_fallback_tool_use_error(
            &FallbackToolResultContent::Text("Error: boom".into()),
            true,
            "ctrl+o",
        );
        assert_eq!(error.error_text, "Error: boom");
    }

    #[test]
    fn fallback_error_truncates_after_ten_lines_and_formats_footer() {
        let content = (1..=12)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let projection = project_fallback_tool_use_error(
            &FallbackToolResultContent::Text(content),
            false,
            "ctrl+o",
        );
        assert_eq!(projection.hidden_line_count, 2);
        assert_eq!(
            projection.footer,
            Some("... +2 lines (Ctrl+O to see all)".into())
        );
        assert_eq!(projection.error_text.lines().count(), 10);
    }

    #[test]
    fn underline_ansi_sequences_are_removed_but_other_text_is_preserved() {
        let projection = project_fallback_tool_use_error(
            &FallbackToolResultContent::Text("\u{001b}[4mboom\u{001b}[0m".into()),
            true,
            "ctrl+o",
        );
        assert_eq!(projection.error_text, "Error: boom\u{001b}[0m");
    }
}

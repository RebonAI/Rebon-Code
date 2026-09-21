//! Prompt-input frame helpers.
//!
//! This module captures the pure helper tail for the prompt input:
//! border-color choice, initial paste-id scanning, and optional border text.

/// Minimal message shape used by [`get_initial_paste_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptHistoryMessage {
    /// Only user messages participate in the scan.
    pub is_user: bool,
    /// Existing image placeholder ids from the message payload.
    pub image_paste_ids: Vec<u32>,
    /// Text blocks that may contain `[Image #N]` / `[Pasted text #N]` refs.
    pub text_blocks: Vec<String>,
}

/// Border text position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderTextPosition {
    /// Top border text.
    Top,
}

/// Border text alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderTextAlign {
    /// Right aligned.
    End,
}

/// Text drawn into the prompt frame's border.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BorderText {
    /// Display content including surrounding spaces.
    pub content: String,
    /// Border edge.
    pub position: BorderTextPosition,
    /// Alignment along the edge.
    pub align: BorderTextAlign,
    /// Offset from the aligned edge.
    pub offset: usize,
}

/// Prompt border color: `bashBorder` in bash mode, `promptBorder` for an
/// in-process teammate, otherwise the teammate's theme color when it
/// resolves, falling back to `promptBorder`.
pub fn resolve_prompt_border_color(
    mode: &str,
    is_in_process_teammate: bool,
    teammate_color_name: Option<&str>,
    resolve_theme_color: impl Fn(&str) -> Option<String>,
) -> String {
    if mode == "bash" {
        return String::from("bashBorder");
    }
    if is_in_process_teammate {
        return String::from("promptBorder");
    }
    if let Some(color_name) = teammate_color_name {
        if let Some(theme_color) = resolve_theme_color(color_name) {
            return theme_color;
        }
    }
    String::from("promptBorder")
}

/// One past the highest image or pasted-text reference id found in user
/// messages.
pub fn get_initial_paste_id(messages: &[PromptHistoryMessage]) -> u32 {
    let mut max_id = 0;
    for message in messages {
        if !message.is_user {
            continue;
        }
        for &id in &message.image_paste_ids {
            max_id = max_id.max(id);
        }
        for text in &message.text_blocks {
            for id in parse_reference_ids(text) {
                max_id = max_id.max(id);
            }
        }
    }
    max_id + 1
}

/// Top-right border text with the fast-mode icon (and the `/fast` hint while
/// it shows); `None` when the icon is hidden.
pub fn build_border_text(
    show_fast_icon: bool,
    show_fast_icon_hint: bool,
    fast_mode_cooldown: bool,
    get_fast_icon_string: impl Fn(bool, bool) -> String,
    dim_fast_label: impl Fn(&str) -> String,
) -> Option<BorderText> {
    if !show_fast_icon {
        return None;
    }

    let fast_segment = if show_fast_icon_hint {
        format!(
            "{} {}",
            get_fast_icon_string(true, fast_mode_cooldown),
            dim_fast_label("/fast")
        )
    } else {
        get_fast_icon_string(true, fast_mode_cooldown)
    };

    Some(BorderText {
        content: format!(" {fast_segment} "),
        position: BorderTextPosition::Top,
        align: BorderTextAlign::End,
        offset: 0,
    })
}

fn parse_reference_ids(input: &str) -> Vec<u32> {
    const PREFIXES: [&str; 3] = ["[Pasted text #", "[Image #", "[...Truncated text #"];
    let mut ids = Vec::new();
    let mut start = 0;

    while start < input.len() {
        let slice = &input[start..];
        let Some(relative_bracket) = slice.find('[') else {
            break;
        };
        let bracket_index = start + relative_bracket;
        let candidate = &input[bracket_index..];

        let Some(prefix_len) = PREFIXES
            .iter()
            .find_map(|prefix| candidate.starts_with(prefix).then_some(prefix.len()))
        else {
            start = bracket_index + 1;
            continue;
        };

        let bytes = candidate.as_bytes();
        let mut digits_end = prefix_len;
        while bytes.get(digits_end).is_some_and(u8::is_ascii_digit) {
            digits_end += 1;
        }
        if digits_end == prefix_len {
            start = bracket_index + prefix_len;
            continue;
        }

        let Some(close_index) = candidate[digits_end..].find(']') else {
            start = bracket_index + digits_end;
            continue;
        };
        let suffix = &candidate[digits_end..digits_end + close_index];
        if !is_valid_reference_suffix(suffix) {
            start = bracket_index + digits_end;
            continue;
        }

        if let Ok(id) = candidate[prefix_len..digits_end].parse::<u32>() {
            if id > 0 {
                ids.push(id);
            }
        }

        start = bracket_index + digits_end + close_index + 1;
    }

    ids.sort_unstable();
    ids
}

fn is_valid_reference_suffix(suffix: &str) -> bool {
    if suffix.is_empty() {
        return true;
    }

    let Some(rest) = suffix.strip_prefix(" +") else {
        return false;
    };
    let digits_len = rest
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits_len == 0 {
        return false;
    }

    let rest = &rest[digits_len..];
    let Some(rest) = rest.strip_prefix(" lines") else {
        return false;
    };

    rest.chars().all(|ch| ch == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_theme_color(color_name: &str) -> Option<String> {
        match color_name {
            "cyan" => Some(String::from("cyan_FOR_SUBAGENTS_ONLY")),
            _ => None,
        }
    }

    #[test]
    fn border_color_prioritizes_bash_and_then_in_process_guard() {
        assert_eq!(
            resolve_prompt_border_color("bash", false, Some("cyan"), resolve_theme_color),
            "bashBorder"
        );
        assert_eq!(
            resolve_prompt_border_color("prompt", true, Some("cyan"), resolve_theme_color),
            "promptBorder"
        );
    }

    #[test]
    fn border_color_uses_resolved_teammate_color_or_default() {
        assert_eq!(
            resolve_prompt_border_color("prompt", false, Some("cyan"), resolve_theme_color),
            "cyan_FOR_SUBAGENTS_ONLY"
        );
        assert_eq!(
            resolve_prompt_border_color("prompt", false, Some("unknown"), resolve_theme_color),
            "promptBorder"
        );
    }

    #[test]
    fn initial_paste_id_scans_user_images_and_text_references() {
        let messages = vec![
            PromptHistoryMessage {
                is_user: true,
                image_paste_ids: vec![2, 8],
                text_blocks: vec![
                    String::from("hello [Pasted text #9 +2 lines] there"),
                    String::from("[Image #11] [Image #3]"),
                ],
            },
            PromptHistoryMessage {
                is_user: false,
                image_paste_ids: vec![42],
                text_blocks: vec![String::from("[Image #99]")],
            },
        ];
        assert_eq!(get_initial_paste_id(&messages), 12);
    }

    #[test]
    fn initial_paste_id_understands_truncated_text_refs() {
        let messages = vec![PromptHistoryMessage {
            is_user: true,
            image_paste_ids: vec![],
            text_blocks: vec![String::from("[...Truncated text #7 +1 lines...]")],
        }];
        assert_eq!(get_initial_paste_id(&messages), 8);
    }

    #[test]
    fn initial_paste_id_ignores_zero_and_malformed_refs() {
        let messages = vec![PromptHistoryMessage {
            is_user: true,
            image_paste_ids: vec![],
            text_blocks: vec![
                String::from("[Image #0]"),
                String::from("[Image #12oops]"),
                String::from("[Pasted text #3]"),
            ],
        }];
        assert_eq!(get_initial_paste_id(&messages), 4);
    }

    #[test]
    fn build_border_text_omits_when_fast_icon_hidden() {
        assert_eq!(
            build_border_text(
                false,
                false,
                false,
                |_, _| String::from("FAST"),
                |text| { text.to_string() }
            ),
            None
        );
    }

    #[test]
    fn build_border_text_appends_dimmed_fast_hint_when_requested() {
        let border = build_border_text(
            true,
            true,
            false,
            |_, _| String::from("FAST"),
            |text| format!("<{text}>"),
        )
        .unwrap();
        assert_eq!(border.content, " FAST </fast> ");
        assert_eq!(border.position, BorderTextPosition::Top);
        assert_eq!(border.align, BorderTextAlign::End);
        assert_eq!(border.offset, 0);
    }
}

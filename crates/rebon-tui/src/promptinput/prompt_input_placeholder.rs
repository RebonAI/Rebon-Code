//! Placeholder text shown in the empty prompt input.

/// How many times the "edit queued messages" hint is shown before it retires.
pub const NUM_TIMES_QUEUE_HINT_SHOWN: u64 = 3;
/// Teammate names longer than this (in UTF-16 units) are truncated with `...`.
pub const MAX_TEAMMATE_NAME_LENGTH: usize = 20;

/// Everything the placeholder decision reads, resolved by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptPlaceholderInput {
    /// Current input value.
    pub input: String,
    /// Prompts submitted so far this session.
    pub submit_count: u64,
    /// Optional teammate name when viewing a teammate.
    pub viewing_agent_name: Option<String>,
    /// Whether any queued command can still be edited.
    pub has_editable_queued_commands: bool,
    /// How many times the queued-command hint has been shown (0 when unset).
    pub queued_command_up_hint_count: u64,
    /// Whether prompt suggestions are enabled.
    pub prompt_suggestion_enabled: bool,
    /// Whether proactive mode is active.
    pub proactive_active: bool,
    /// Context-sensitive placeholder tip selected by the TUI. The
    /// selector owns all time/rotation policy; promptinput only applies
    /// placeholder priority deterministically.
    pub contextual_tip: Option<String>,
    /// Cached example command shown before the first submit.
    pub example_command: Option<String>,
}

/// Pure placeholder resolver for the prompt input.
pub fn resolve_prompt_input_placeholder(input: &PromptPlaceholderInput) -> Option<String> {
    if !input.input.is_empty() {
        return None;
    }

    if let Some(agent_name) = &input.viewing_agent_name {
        let display_name = if utf16_len(agent_name) > MAX_TEAMMATE_NAME_LENGTH {
            format!(
                "{}...",
                truncate_utf16_units(agent_name, MAX_TEAMMATE_NAME_LENGTH - 3)
            )
        } else {
            agent_name.clone()
        };
        return Some(format!("Message @{display_name}\u{2026}"));
    }

    if input.has_editable_queued_commands
        && input.queued_command_up_hint_count < NUM_TIMES_QUEUE_HINT_SHOWN
    {
        return Some("Press up to edit queued messages".to_string());
    }

    if let Some(tip) = &input.contextual_tip {
        if !tip.is_empty() {
            return Some(tip.clone());
        }
    }

    if input.submit_count < 1 && input.prompt_suggestion_enabled && !input.proactive_active {
        return input.example_command.clone();
    }

    None
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn truncate_utf16_units(value: &str, max_units: usize) -> String {
    if max_units == 0 {
        return String::new();
    }

    let mut units = 0;
    let mut end = 0;
    for (idx, ch) in value.char_indices() {
        let next_units = units + ch.len_utf16();
        if next_units > max_units {
            break;
        }
        units = next_units;
        end = idx + ch.len_utf8();
    }
    value[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PromptPlaceholderInput {
        PromptPlaceholderInput {
            input: String::new(),
            submit_count: 0,
            viewing_agent_name: None,
            has_editable_queued_commands: false,
            queued_command_up_hint_count: 0,
            prompt_suggestion_enabled: false,
            proactive_active: false,
            contextual_tip: None,
            example_command: Some("/help".into()),
        }
    }

    #[test]
    fn non_empty_input_suppresses_placeholder() {
        let mut input = base();
        input.input = "x".into();
        assert_eq!(resolve_prompt_input_placeholder(&input), None);
    }

    #[test]
    fn teammate_placeholder_uses_ellipsis_and_truncation() {
        let mut input = base();
        input.viewing_agent_name = Some("averyveryverylongteammatename".into());
        assert_eq!(
            resolve_prompt_input_placeholder(&input),
            Some("Message @averyveryverylong...\u{2026}".to_string())
        );
    }

    #[test]
    fn queue_hint_shows_under_cap() {
        let mut input = base();
        input.has_editable_queued_commands = true;
        input.queued_command_up_hint_count = 2;
        assert_eq!(
            resolve_prompt_input_placeholder(&input),
            Some("Press up to edit queued messages".to_string())
        );
    }

    #[test]
    fn queue_hint_hidden_at_cap() {
        let mut input = base();
        input.has_editable_queued_commands = true;
        input.queued_command_up_hint_count = NUM_TIMES_QUEUE_HINT_SHOWN;
        assert_eq!(resolve_prompt_input_placeholder(&input), None);
    }

    #[test]
    fn contextual_tip_shows_after_queue_hint_and_before_example() {
        let mut input = base();
        input.contextual_tip = Some("Tip: press / for commands".into());
        input.prompt_suggestion_enabled = true;
        assert_eq!(
            resolve_prompt_input_placeholder(&input),
            Some("Tip: press / for commands".to_string())
        );

        input.has_editable_queued_commands = true;
        input.queued_command_up_hint_count = 0;
        assert_eq!(
            resolve_prompt_input_placeholder(&input),
            Some("Press up to edit queued messages".to_string())
        );
    }

    #[test]
    fn teammate_placeholder_wins_over_contextual_tip() {
        let mut input = base();
        input.viewing_agent_name = Some("teammate".into());
        input.contextual_tip = Some("Tip: hidden".into());
        assert_eq!(
            resolve_prompt_input_placeholder(&input),
            Some("Message @teammate\u{2026}".to_string())
        );
    }

    #[test]
    fn example_command_shows_only_before_first_submit_and_when_enabled() {
        let mut input = base();
        input.prompt_suggestion_enabled = true;
        assert_eq!(
            resolve_prompt_input_placeholder(&input),
            Some("/help".into())
        );

        input.submit_count = 1;
        assert_eq!(resolve_prompt_input_placeholder(&input), None);
    }

    #[test]
    fn proactive_mode_suppresses_example_command() {
        let mut input = base();
        input.prompt_suggestion_enabled = true;
        input.proactive_active = true;
        assert_eq!(resolve_prompt_input_placeholder(&input), None);
    }

    #[test]
    fn teammate_placeholder_truncation_is_utf16_safe_for_cjk() {
        let mut input = base();
        input.viewing_agent_name = Some("你好世界你好世界你好世界你好世界".into());
        let placeholder = resolve_prompt_input_placeholder(&input).unwrap();
        assert!(placeholder.starts_with("Message @"));
        assert!(placeholder.ends_with("\u{2026}"));
    }

    #[test]
    fn teammate_placeholder_truncation_keeps_scalar_boundaries_for_emoji() {
        let mut input = base();
        input.viewing_agent_name = Some("😀😀😀😀😀😀😀😀😀😀😀".into());
        let placeholder = resolve_prompt_input_placeholder(&input).unwrap();
        assert!(placeholder.starts_with("Message @"));
        assert!(placeholder.ends_with("\u{2026}"));
        assert!(!placeholder.contains('\u{fffd}'));
    }
}

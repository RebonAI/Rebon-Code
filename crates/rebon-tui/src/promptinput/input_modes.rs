//! Prompt input modes and the `!` bash-mode prefix.

/// Prompt input mode used by [`prepend_mode_character_to_input`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptInputMode {
    /// Normal prompt mode.
    Prompt,
    /// Bash mode, prefixed with `!`.
    Bash,
}

/// History mode inferred back from the input prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryMode {
    /// Normal prompt history.
    Prompt,
    /// Bash history.
    Bash,
}

/// Prefix the input with `!` in bash mode; prompt mode leaves it unchanged.
pub fn prepend_mode_character_to_input(input: &str, mode: PromptInputMode) -> String {
    match mode {
        PromptInputMode::Bash => format!("!{input}"),
        PromptInputMode::Prompt => input.to_string(),
    }
}

/// Infer the history mode from a leading `!`.
pub fn get_mode_from_input(input: &str) -> HistoryMode {
    if input.starts_with('!') {
        HistoryMode::Bash
    } else {
        HistoryMode::Prompt
    }
}

/// Strip the mode prefix, if any, from the input.
pub fn get_value_from_input(input: &str) -> String {
    match get_mode_from_input(input) {
        HistoryMode::Prompt => input.to_string(),
        HistoryMode::Bash => input.chars().skip(1).collect(),
    }
}

/// Whether the input is exactly the bash-mode character `!`.
pub fn is_input_mode_character(input: &str) -> bool {
    input == "!"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepend_mode_character_only_affects_bash() {
        assert_eq!(
            prepend_mode_character_to_input("ls", PromptInputMode::Bash),
            "!ls"
        );
        assert_eq!(
            prepend_mode_character_to_input("hello", PromptInputMode::Prompt),
            "hello"
        );
    }

    #[test]
    fn get_mode_from_input_detects_bash_prefix() {
        assert_eq!(get_mode_from_input("!ls"), HistoryMode::Bash);
        assert_eq!(get_mode_from_input("hello"), HistoryMode::Prompt);
    }

    #[test]
    fn get_value_from_input_strips_only_leading_bang() {
        assert_eq!(get_value_from_input("!ls"), "ls");
        assert_eq!(get_value_from_input("hello"), "hello");
        assert_eq!(get_value_from_input("!!"), "!");
    }

    #[test]
    fn is_input_mode_character_only_matches_single_bang() {
        assert!(is_input_mode_character("!"));
        assert!(!is_input_mode_character("!!"));
        assert!(!is_input_mode_character(""));
    }
}

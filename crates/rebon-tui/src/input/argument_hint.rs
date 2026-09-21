//! Slash-command argument-hint decision.
//!
//! The hint is shown only while the input contains a slash command with no
//! arguments yet. A trailing space immediately after the command still counts
//! as "no arguments"; trailing spaces after real arguments do not re-show the
//! hint.

/// Inputs for [`should_show_argument_hint`].
#[derive(Debug, Clone, Copy)]
pub struct ArgumentHintInput<'a> {
    /// The argument-hint text configured for the command.
    /// `None` or empty string means no hint configured.
    pub argument_hint: Option<&'a str>,
    /// Current input value.
    pub value: &'a str,
}

/// Compute whether the argument hint should be displayed.
///
/// Returns true iff all four conditions hold:
/// 1. `argument_hint` is `Some(s)` with `!s.is_empty()`.
/// 2. `value` is non-empty.
/// 3. `value.starts_with('/')`.
/// 4. The trimmed value contains no literal spaces.
pub fn should_show_argument_hint(input: &ArgumentHintInput<'_>) -> bool {
    // Outer gates first (short-circuit early).
    let argument_hint = match input.argument_hint {
        Some(s) if !s.is_empty() => s,
        _ => return false,
    };
    let _ = argument_hint; // silence unused — we only check non-emptiness
    if input.value.is_empty() {
        return false;
    }
    if !input.value.starts_with('/') {
        return false;
    }

    // No-arguments check: `value.trim()` contains no literal space.
    // A trailing space after the command still shows the hint because
    // trim removes it; trailing spaces after actual args stay hidden.
    let trimmed = input.value.trim();
    !trimmed.contains(' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(hint: Option<&'a str>, value: &'a str) -> ArgumentHintInput<'a> {
        ArgumentHintInput {
            argument_hint: hint,
            value,
        }
    }

    // ------- gates: argument_hint, value, leading slash -----------------

    #[test]
    fn no_hint_returns_false() {
        assert!(!should_show_argument_hint(&input(None, "/commit")));
    }

    #[test]
    fn empty_hint_returns_false() {
        assert!(!should_show_argument_hint(&input(Some(""), "/commit")));
    }

    #[test]
    fn empty_value_returns_false() {
        assert!(!should_show_argument_hint(&input(Some("<msg>"), "")));
    }

    #[test]
    fn non_slash_value_returns_false() {
        assert!(!should_show_argument_hint(&input(Some("<msg>"), "commit")));
    }

    #[test]
    fn single_slash_no_text_still_starts_with_slash() {
        // "/" alone — trimmed="/" has no space, starts with '/'
        // so the no-arguments check passes. All four gates pass.
        assert!(should_show_argument_hint(&input(Some("<msg>"), "/")));
    }

    // ------- no-arguments branches --------------------------------

    #[test]
    fn slash_command_without_args_shows_hint() {
        assert!(should_show_argument_hint(&input(Some("<msg>"), "/commit")));
    }

    #[test]
    fn slash_command_with_trailing_space_shows_hint() {
        assert!(should_show_argument_hint(&input(Some("<msg>"), "/commit ")));
    }

    #[test]
    fn slash_command_with_arg_hides_hint() {
        assert!(!should_show_argument_hint(&input(
            Some("<msg>"),
            "/commit foo"
        )));
    }

    #[test]
    fn slash_command_with_arg_and_trailing_space_hides_hint() {
        assert!(!should_show_argument_hint(&input(
            Some("<msg>"),
            "/commit foo "
        )));
    }

    #[test]
    fn slash_command_with_arg_and_multiple_trailing_spaces_hides_hint() {
        assert!(!should_show_argument_hint(&input(
            Some("<msg>"),
            "/commit foo  "
        )));
    }

    #[test]
    fn slash_command_with_leading_spaces_not_matched() {
        // " /commit" does NOT start with '/' — gate fails.
        assert!(!should_show_argument_hint(&input(
            Some("<msg>"),
            " /commit"
        )));
    }

    #[test]
    fn slash_command_with_leading_and_trailing_spaces() {
        // "/commit " with a leading space — " /commit " doesn't
        // start with '/'. Returns false.
        assert!(!should_show_argument_hint(&input(
            Some("<msg>"),
            " /commit "
        )));
    }

    #[test]
    fn slash_with_only_trailing_spaces_shows_hint() {
        assert!(should_show_argument_hint(&input(Some("<msg>"), "/   ")));
    }

    #[test]
    fn value_with_only_slash_and_space_shows_hint() {
        assert!(should_show_argument_hint(&input(Some("<msg>"), "/ ")));
    }

    // ------- edge cases --------------------------------------------------

    #[test]
    fn non_slash_with_trailing_space_returns_false() {
        // "commit " — doesn't start with '/'. Returns false
        // regardless of the no-arguments check.
        assert!(!should_show_argument_hint(&input(Some("<msg>"), "commit ")));
    }

    #[test]
    fn empty_trimmed_value_still_honours_leading_slash() {
        // Value is "/  " — trimmed is "/", starts with '/'. Hint shows.
        assert!(should_show_argument_hint(&input(Some("hint"), "/  ")));
    }

    #[test]
    fn value_of_just_spaces_returns_false() {
        // "   " — doesn't start with '/'. Returns false.
        assert!(!should_show_argument_hint(&input(Some("hint"), "   ")));
    }

    #[test]
    fn slash_command_with_tab_character_in_middle_shows_hint() {
        // The space check looks for a literal space, not any whitespace.
        // So "/commit\tfoo" has NO space, the no-arguments check passes.
        assert!(should_show_argument_hint(&input(
            Some("hint"),
            "/commit\tfoo"
        )));
    }
}

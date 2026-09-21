//! Boolean environment flags, read one way.
//!
//! Every `REBON_*` switch that means "on" or "off" parses through here, so a
//! user who writes `REBON_SIMPLE=Yes` in one place and `=1` in another gets the
//! same answer from both. The vocabulary is fixed:
//!
//! | value (trimmed, ASCII case-folded) | meaning       |
//! |------------------------------------|---------------|
//! | `1`, `true`, `yes`, `on`           | `Some(true)`  |
//! | `0`, `false`, `no`, `off`          | `Some(false)` |
//! | anything else, including empty     | `None`        |
//!
//! An empty or unrecognized value is *not* a way to spell "off": a shell that
//! exports `REBON_FOO=` by accident must not disable something the user never
//! asked to disable, and every caller that wants a default falls back on
//! `None` explicitly. The one gate that treats empty as off (the ripgrep
//! bundled-binary switch) does so on purpose and keeps its own parser.

/// Parse one flag value. See the module documentation for the vocabulary.
pub fn parse_env_flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// `true` when `value` spells "on".
pub fn is_env_truthy(value: &str) -> bool {
    parse_env_flag(value) == Some(true)
}

/// Read and parse the variable `name` from the process environment.
///
/// Unset and non-Unicode values both read as `None`.
pub fn env_flag(name: &str) -> Option<bool> {
    std::env::var(name).ok().as_deref().and_then(parse_env_flag)
}

/// `true` when the variable `name` is set and spells "on".
pub fn env_truthy(name: &str) -> bool {
    env_flag(name) == Some(true)
}

/// `true` when the variable `name` is set and spells "off".
pub fn env_defined_falsy(name: &str) -> bool {
    env_flag(name) == Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_on_spellings_parse_as_true() {
        for value in ["1", "true", "yes", "on"] {
            assert_eq!(parse_env_flag(value), Some(true), "{value:?}");
        }
    }

    #[test]
    fn the_four_off_spellings_parse_as_false() {
        for value in ["0", "false", "no", "off"] {
            assert_eq!(parse_env_flag(value), Some(false), "{value:?}");
        }
    }

    #[test]
    fn case_and_surrounding_whitespace_do_not_matter() {
        assert_eq!(parse_env_flag("  TRUE\n"), Some(true));
        assert_eq!(parse_env_flag("\tOfF "), Some(false));
        assert_eq!(parse_env_flag("Yes"), Some(true));
        assert_eq!(parse_env_flag("nO"), Some(false));
    }

    #[test]
    fn empty_whitespace_and_unknown_values_are_neither() {
        for value in [
            "", "   ", "\t", "maybe", "00", "2", "-1", "disabled", "falsey",
        ] {
            assert_eq!(parse_env_flag(value), None, "{value:?}");
        }
    }

    #[test]
    fn truthy_is_exactly_some_true() {
        assert!(is_env_truthy("on"));
        assert!(!is_env_truthy("off"));
        assert!(!is_env_truthy(""));
        assert!(!is_env_truthy("maybe"));
    }

    /// The process-environment readers share one lock: `std::env::set_var`
    /// is process-global, and these tests each set a variable of their own.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn env_flag_reads_the_process_environment() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        const NAME: &str = "REBON_TYPES_ENV_FLAG_TEST";
        std::env::remove_var(NAME);
        assert_eq!(env_flag(NAME), None);
        assert!(!env_truthy(NAME));
        assert!(!env_defined_falsy(NAME));

        std::env::set_var(NAME, " On ");
        assert_eq!(env_flag(NAME), Some(true));
        assert!(env_truthy(NAME));
        assert!(!env_defined_falsy(NAME));

        std::env::set_var(NAME, "0");
        assert_eq!(env_flag(NAME), Some(false));
        assert!(!env_truthy(NAME));
        assert!(env_defined_falsy(NAME));

        std::env::set_var(NAME, "");
        assert_eq!(env_flag(NAME), None, "an empty export is not a choice");
        assert!(!env_defined_falsy(NAME));
        std::env::remove_var(NAME);
    }
}

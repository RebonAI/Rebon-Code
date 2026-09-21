//! Whether the most recent shell output should render in full instead of
//! being truncated.
//!
//! The flag is a plain boolean carried by the caller, so this module only
//! models the minimal contract for it: a value type with explicit
//! enabled/disabled helpers.
/// Lightweight wrapper around the expand-shell-output flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExpandShellOutputContextValue(pub bool);

impl ExpandShellOutputContextValue {
    /// The flag itself: true when shell output renders fully.
    pub const fn enabled(self) -> bool {
        self.0
    }

    /// Builds an explicit value.
    pub const fn from_bool(enabled: bool) -> Self {
        Self(enabled)
    }
}

/// Read the flag out of a context value.
pub fn expand_shell_output_enabled(value: ExpandShellOutputContextValue) -> bool {
    value.enabled()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled() {
        assert!(!ExpandShellOutputContextValue::default().enabled());
    }

    #[test]
    fn explicit_enabled_is_true() {
        assert!(expand_shell_output_enabled(
            ExpandShellOutputContextValue::from_bool(true)
        ));
    }

    #[test]
    fn explicit_disabled_is_false() {
        assert!(!expand_shell_output_enabled(
            ExpandShellOutputContextValue::from_bool(false)
        ));
    }
}
